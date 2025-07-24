use std::collections::HashMap;
use std::iter::repeat_with;

use niri_config::{FloatOrInt, FloatingPosition, PresetSize, RelativeTo};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::XdgToplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::utils::{Coordinate, Logical, Rectangle};
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::xdg::XdgShellState;
use wayland_backend::server::ClientId;
use xx_session_manager_v1::XxSessionManagerV1;
use xx_session_v1::XxSessionV1;
use xx_toplevel_session_v1::XxToplevelSessionV1;

use super::raw::xx_session_management::v1::server::{
    xx_session_manager_v1, xx_session_v1, xx_toplevel_session_v1,
};
use crate::layout::scrolling::{ColumnWidth, WindowHeight};
use crate::layout::workspace::WorkspaceId;
use crate::layout::Layout;
use crate::window::{Mapped, Unmapped};

const VERSION: u32 = 1;

/// Used to globally identify a session.
pub type SessionId = String;

/// A reference to a specific toplevel session.
#[derive(Debug, Clone)]
pub struct ToplevelSessionRef {
    session_id: SessionId,
    toplevel_id: ToplevelId,
}

/// Session data for all applications.
///
/// A global object shared by all clients.
pub struct SessionManagerState {
    /// Maps session IDs to `xx_session_v1` data.
    sessions: HashMap<SessionId, SessionState>,
}

impl SessionManagerState {
    /// Gets a mutable reference to a toplevel's session state, if it exists.
    pub fn get_toplevel_session_mut(
        &mut self,
        session_ref: &ToplevelSessionRef,
    ) -> Option<&mut ToplevelSession> {
        self.sessions
            .get_mut(&session_ref.session_id)
            .and_then(|session| session.sessions.get_mut(&session_ref.toplevel_id))
    }
}

pub struct SessionManagerGlobalData {
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

impl SessionManagerState {
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<XxSessionManagerV1, SessionManagerGlobalData>,
        D: Dispatch<XxSessionManagerV1, ()>,
        D: Dispatch<XxSessionV1, SessionState>,
        D: Dispatch<XxToplevelSessionV1, ToplevelSessionState>,
        D: SessionManagementHandler,
        D: 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        let global_data = SessionManagerGlobalData {
            filter: Box::new(filter),
        };
        display.create_global::<D, XxSessionManagerV1, _>(VERSION, global_data);

        Self {
            sessions: HashMap::new(),
        }
    }
}

impl<D> GlobalDispatch<XxSessionManagerV1, SessionManagerGlobalData, D> for SessionManagerState
where
    D: GlobalDispatch<XxSessionManagerV1, SessionManagerGlobalData>,
    D: Dispatch<XxSessionManagerV1, ()>,
    D: Dispatch<XxSessionV1, SessionState>,
    D: Dispatch<XxToplevelSessionV1, ToplevelSessionState>,
    D: SessionManagementHandler,
    D: 'static,
{
    fn bind(
        _state: &mut D,
        _display: &DisplayHandle,
        _client: &Client,
        manager: New<XxSessionManagerV1>,
        _manager_state: &SessionManagerGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(manager, ());
    }

    fn can_view(client: Client, global_data: &SessionManagerGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

impl<D> Dispatch<XxSessionManagerV1, (), D> for SessionManagerState
where
    D: GlobalDispatch<XxSessionManagerV1, SessionManagerGlobalData>,
    D: Dispatch<XxSessionManagerV1, ()>,
    D: Dispatch<XxSessionV1, SessionState>,
    D: Dispatch<XxToplevelSessionV1, ToplevelSessionState>,
    D: SessionManagementHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        client: &Client,
        manager: &XxSessionManagerV1,
        request: xx_session_manager_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            xx_session_manager_v1::Request::Destroy => return,
            xx_session_manager_v1::Request::GetSession {
                id,
                session: maybe_session_id,
                ..
            } => {
                let session_id = maybe_session_id.unwrap_or_else(|| {
                    // session_id was not provided, so generate one.
                    repeat_with(fastrand::alphanumeric).take(32).collect()
                });

                let sessions = &mut state.session_management_state().sessions;

                let restoring = sessions.contains_key(&session_id);

                let client_id = client.id();
                let new_session_state = SessionState::new(session_id.clone());
                let session_state = sessions
                    .entry(session_id.clone())
                    .or_insert(new_session_state);

                if session_state.owned_by_client == Some(client_id.clone()) {
                    manager.post_error(
                        xx_session_manager_v1::Error::InUse,
                        "session already in use",
                    );
                    return;
                }
                session_state.owned_by_client = Some(client_id);

                let session = data_init.init(id, session_state.clone());

                if restoring {
                    session.restored();
                    debug!("restored session `{}`", session_id);
                } else {
                    session.created(session_id.clone());
                    debug!("created session `{}`", session_id);
                }
            }
        }
    }
}

/// Handler trait for session-management.
pub trait SessionManagementHandler {
    /// Get a mutable reference to the session management state.
    fn session_management_state(&mut self) -> &mut SessionManagerState;
    /// Get a reference to the [`XdgShellState`].
    fn xdg_shell_state(&self) -> &XdgShellState;
    /// Finds a window mapped to the given surface, if it exists.
    fn find_mapped_window(&mut self, surface: &WlSurface) -> Option<&Mapped>;
    /// Gets the currently unmapped windows.
    fn unmapped_windows(&mut self) -> &mut HashMap<WlSurface, Unmapped>;
    /// Removes a session from any mapped window associated with the surface.
    fn remove_session_from_mapped(&mut self, surface: &WlSurface);
}

#[allow(missing_docs)]
#[macro_export]
macro_rules! delegate_session_management {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            crate::protocols::raw::xx_session_management::v1::server::xx_session_manager_v1::XxSessionManagerV1: $crate::protocols::xx_session_management::SessionManagerGlobalData
        ] => $crate::protocols::xx_session_management::SessionManagerState);

        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            crate::protocols::raw::xx_session_management::v1::server::xx_session_manager_v1::XxSessionManagerV1: ()
        ] => $crate::protocols::xx_session_management::SessionManagerState);

        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            crate::protocols::raw::xx_session_management::v1::server::xx_session_v1::XxSessionV1: $crate::protocols::xx_session_management::SessionState
        ] => $crate::protocols::xx_session_management::SessionManagerState);

        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            crate::protocols::raw::xx_session_management::v1::server::xx_toplevel_session_v1::XxToplevelSessionV1: $crate::protocols::xx_session_management::ToplevelSessionState
        ] => $crate::protocols::xx_session_management::SessionManagerState);
    };
}

/// Used to identify what window is being restored,
/// and may be used by clients to store window specific state within the session.
pub type ToplevelId = String;

/// The session data for an application's windows.
///
/// Corresponds to `xx_session_v1`.
#[derive(Debug, Clone)]
pub struct SessionState {
    session_id: SessionId,
    owned_by_client: Option<ClientId>,
    /// Maps toplevel IDs ("names") to `xx_toplevel_session_v1` data.
    sessions: HashMap<ToplevelId, ToplevelSession>,
}

impl SessionState {
    fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            owned_by_client: None,
            sessions: Default::default(),
        }
    }
}

/// A session which may store persistent state for a toplevel.
#[derive(Debug, Clone)]
pub struct ToplevelSession {
    resource: XxToplevelSessionV1,
    state: ToplevelSessionState,
    /// Whether the session was created using xx_session_v1::restore_toplevel
    /// or xx_session_v1::add_toplevel.
    is_restoring: bool,
}

impl ToplevelSession {
    pub fn get_ref(&self) -> ToplevelSessionRef {
        self.state.get_ref()
    }

    pub fn is_restoring(&self) -> bool {
        self.is_restoring
    }

    pub fn restored(&self, toplevel: &XdgToplevel) {
        debug!(
            "sending xx_toplevel_session_v1::restored for toplevel {:?}",
            toplevel.id()
        );
        self.resource.restored(toplevel)
    }

    pub fn update(&mut self, mapped: &Mapped, layout: &Layout<Mapped>) {
        self.state.update(mapped, layout);
    }

    pub fn initial_workspace(&self) -> Option<&ToplevelSessionWorkspace> {
        self.state.initial_workspace()
    }

    pub fn initial_column_index(&self) -> Option<usize> {
        self.state.initial_column_index()
    }

    pub fn was_full_width(&self) -> Option<bool> {
        self.state.was_full_width()
    }

    pub fn was_floating(&self) -> Option<bool> {
        self.state.was_floating()
    }

    pub fn initial_width(&self) -> Option<PresetSize> {
        self.state.initial_width()
    }

    pub fn initial_height(&self) -> Option<PresetSize> {
        self.state.initial_height()
    }

    pub fn initial_floating_position(&self) -> Option<FloatingPosition> {
        self.state.initial_floating_position()
    }
}

/// The session data for a single toplevel window.
///
/// Corresponds to `xx_toplevel_session_v1`.
#[derive(Debug, Clone)]
pub struct ToplevelSessionState {
    session_ref: ToplevelSessionRef,
    surface: WlSurface,
    workspace: Option<ToplevelSessionWorkspace>,
    attributes: Option<WindowAttributes>,
}

impl ToplevelSessionState {
    fn new(surface: WlSurface, session_id: SessionId, toplevel_id: ToplevelId) -> Self {
        Self {
            session_ref: ToplevelSessionRef {
                session_id,
                toplevel_id,
            },
            surface,
            workspace: None,
            attributes: None,
        }
    }

    fn get_ref(&self) -> ToplevelSessionRef {
        self.session_ref.clone()
    }

    fn update(&mut self, mapped: &Mapped, layout: &Layout<Mapped>) {
        let surface = mapped.window.wl_surface().expect("no x11 support");

        let Some(workspace) = layout.find_window_workspace(&surface) else {
            error!("unable to update toplevel session state: couldn't find window workspace");
            return;
        };

        self.workspace = Some(
            workspace
                .name()
                .cloned()
                .map(ToplevelSessionWorkspace::Named)
                .unwrap_or(ToplevelSessionWorkspace::Unnamed(workspace.id())),
        );

        self.attributes = Some(if mapped.is_floating() {
            WindowAttributes::Floating {
                geometry: mapped.window.geometry(),
            }
        } else {
            let Some(column_index) = workspace.get_column_index_of(&mapped.window) else {
                error!("unable to get column index of non-floating window");
                return;
            };

            let Some(height) = workspace.get_window_height(&mapped.window) else {
                error!("unable to get column height of non-floating window");
                return;
            };

            let Some(width) = workspace.get_window_column_width(&mapped.window) else {
                error!("unable to get column width of non-floating window");
                return;
            };

            let Some(is_full_width) = workspace.is_window_column_full_width(&mapped.window) else {
                error!("unable to find column width of non-floating window");
                return;
            };

            WindowAttributes::Scrolling {
                column_index,
                height,
                width,
                is_full_width,
            }
        });

        debug!(
            "updated top level session state for Mapped {:?} to {:?}",
            mapped.id(),
            &self
        );
    }

    fn initial_workspace(&self) -> Option<&ToplevelSessionWorkspace> {
        self.workspace.as_ref()
    }

    fn initial_column_index(&self) -> Option<usize> {
        self.attributes.as_ref().and_then(|window| match window {
            WindowAttributes::Scrolling { column_index, .. } => Some(*column_index),
            _ => None,
        })
    }

    fn was_full_width(&self) -> Option<bool> {
        self.attributes.as_ref().and_then(|window| match window {
            WindowAttributes::Scrolling { is_full_width, .. } => Some(*is_full_width),
            _ => None,
        })
    }

    fn was_floating(&self) -> Option<bool> {
        self.attributes
            .as_ref()
            .map(|window| matches!(window, WindowAttributes::Floating { .. }))
    }

    fn initial_width(&self) -> Option<PresetSize> {
        self.attributes.as_ref().map(|window| match window {
            WindowAttributes::Scrolling { width, .. } => (*width).into(),
            WindowAttributes::Floating { geometry, .. } => PresetSize::Fixed(geometry.size.w),
        })
    }

    fn initial_height(&self) -> Option<PresetSize> {
        self.attributes.as_ref().map(|window| match window {
            WindowAttributes::Scrolling {
                height: WindowHeight::Fixed(height),
                ..
            } => PresetSize::Fixed(*height as i32),
            WindowAttributes::Scrolling { height: _, .. } => PresetSize::Proportion(1.0),
            WindowAttributes::Floating { geometry, .. } => PresetSize::Fixed(geometry.size.h),
        })
    }

    fn initial_floating_position(&self) -> Option<FloatingPosition> {
        self.attributes.as_ref().and_then(|window| match window {
            WindowAttributes::Floating { geometry } => Some(FloatingPosition {
                x: FloatOrInt(geometry.loc.x.to_f64()),
                y: FloatOrInt(geometry.loc.y.to_f64()),
                relative_to: RelativeTo::TopLeft,
            }),
            _ => None,
        })
    }
}

/// Identifies a workspace by name (if it has one) or an ID.
#[derive(Debug, Clone)]
pub enum ToplevelSessionWorkspace {
    Named(String),
    Unnamed(WorkspaceId),
}

/// Describes where a toplevel window is located.
#[derive(Debug, Clone)]
pub enum WindowAttributes {
    /// The window is in a scrollable-tiling space.
    Scrolling {
        /// Where in the scrollable-tiling space the window is located.
        column_index: usize,

        /// How wide the window should be.
        width: ColumnWidth,

        /// How tall the window should be.
        height: WindowHeight,

        /// Whether the column is full-width.
        is_full_width: bool,
    },

    /// The window is in a floating space.
    Floating {
        /// Where the window is located and its size.
        geometry: Rectangle<i32, Logical>,
    },
}

impl<D> Dispatch<XxSessionV1, SessionState, D> for SessionManagerState
where
    D: Dispatch<XxSessionV1, SessionState>,
    D: Dispatch<XxToplevelSessionV1, ToplevelSessionState>,
    D: SessionManagementHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        session: &XxSessionV1,
        request: xx_session_v1::Request,
        data: &SessionState,
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            xx_session_v1::Request::Destroy => {
                debug!("destroyed session `{}`", data.session_id);
            }
            xx_session_v1::Request::Remove => {
                let sessions = &mut state.session_management_state().sessions;
                sessions.remove(&data.session_id).map(|session_state| {
                    session_state
                        .sessions
                        .iter()
                        .map(|(_, toplevel_state)| &toplevel_state.state.surface)
                        .for_each(|surface| state.remove_session_from_mapped(surface))
                });
                debug!("removed session `{}`", data.session_id);
            }
            xx_session_v1::Request::AddToplevel {
                id,
                toplevel,
                name: toplevel_id,
            } => {
                let Some(surface) = state.xdg_shell_state().get_toplevel(&toplevel) else {
                    error!("Tried to add a toplevel session with an invalid toplevel");
                    return;
                };

                if data.sessions.contains_key(&toplevel_id) {
                    session.post_error(
                        xx_session_v1::Error::NameInUse,
                        "toplevel name is already present in session",
                    );
                    return;
                }

                if let Some(mapped) = state.find_mapped_window(surface.wl_surface()) {
                    session.post_error(
                        xx_session_v1::Error::AlreadyMapped,
                        format!(
                            "toplevel {:?} was already mapped when restored",
                            mapped
                                .window
                                .toplevel()
                                .expect("no x11 support")
                                .xdg_toplevel()
                                .id()
                        ),
                    );
                    return;
                }

                let toplevel_session_state = ToplevelSessionState::new(
                    surface.wl_surface().clone(),
                    data.session_id.clone(),
                    toplevel_id.clone(),
                );
                let toplevel_resource = data_init.init(id, toplevel_session_state.clone());
                let toplevel_session = ToplevelSession {
                    resource: toplevel_resource,
                    state: toplevel_session_state,
                    is_restoring: false,
                };

                let Some(session_state) = state
                    .session_management_state()
                    .sessions
                    .get_mut(&data.session_id)
                else {
                    error!("Unable to find session with id `{}`", data.session_id);
                    return;
                };

                session_state
                    .sessions
                    .insert(toplevel_id, toplevel_session.clone());

                let Some(unmapped) = state.unmapped_windows().get_mut(surface.wl_surface()) else {
                    error!("Unable to find unmapped window");
                    return;
                };
                unmapped.session = Some(toplevel_session);

                debug!("added toplevel with session state: {:?}", unmapped.session);
            }
            xx_session_v1::Request::RestoreToplevel {
                id,
                toplevel,
                name: toplevel_id,
            } => {
                let Some(surface) = state.xdg_shell_state().get_toplevel(&toplevel) else {
                    error!("Tried to restore a toplevel session with an invalid toplevel");
                    return;
                };

                if data.sessions.contains_key(&toplevel_id) {
                    session.post_error(
                        xx_session_v1::Error::NameInUse,
                        "toplevel name is already present in session",
                    );
                    return;
                }

                if let Some(mapped) = state.find_mapped_window(surface.wl_surface()) {
                    session.post_error(
                        xx_session_v1::Error::AlreadyMapped,
                        format!(
                            "toplevel {:?} was already mapped when restored",
                            mapped
                                .window
                                .toplevel()
                                .expect("no x11 support")
                                .xdg_toplevel()
                                .id()
                        ),
                    );
                    return;
                }

                let toplevel_session_state = ToplevelSessionState::new(
                    surface.wl_surface().clone(),
                    data.session_id.clone(),
                    toplevel_id.clone(),
                );
                let toplevel_resource = data_init.init(id, toplevel_session_state.clone());
                let toplevel_session = ToplevelSession {
                    resource: toplevel_resource,
                    state: toplevel_session_state,
                    is_restoring: true,
                };

                let Some(session_state) = state
                    .session_management_state()
                    .sessions
                    .get_mut(&data.session_id)
                else {
                    error!("Unable to find session with id `{}`", data.session_id);
                    return;
                };

                let mut session = session_state
                    .sessions
                    .entry(toplevel_id)
                    .or_insert_with(|| toplevel_session.clone())
                    .clone();

                session.is_restoring = true;

                let Some(unmapped) = state.unmapped_windows().get_mut(surface.wl_surface()) else {
                    error!("Unable to find unmapped window");
                    return;
                };
                unmapped.session = Some(session);

                debug!(
                    "restoring toplevel with session state: {:?}",
                    unmapped.session
                );
            }
        }
    }
}

impl<D> Dispatch<XxToplevelSessionV1, ToplevelSessionState, D> for SessionManagerState
where
    D: Dispatch<XxToplevelSessionV1, ToplevelSessionState>,
    D: SessionManagementHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _toplevel_session: &XxToplevelSessionV1,
        request: xx_toplevel_session_v1::Request,
        data: &ToplevelSessionState,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            xx_toplevel_session_v1::Request::Destroy => {
                debug!("destroyed toplevel session `{:?}`", data.session_ref);
            }
            xx_toplevel_session_v1::Request::Remove => {
                state
                    .session_management_state()
                    .sessions
                    .get_mut(&data.session_ref.session_id)
                    .map(|session_state| {
                        session_state.sessions.remove(&data.session_ref.toplevel_id)
                    });
                debug!("removed toplevel session `{:?}`", data.session_ref);
            }
        }
    }
}
