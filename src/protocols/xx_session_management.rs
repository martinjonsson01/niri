use std::collections::HashMap;
use std::fs;
use std::iter::repeat_with;
use std::path::PathBuf;
use std::time::Duration;

use calloop::channel::Sender;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{LoopHandle, RegistrationToken};
use directories::ProjectDirs;
use niri_config::{FloatOrInt, FloatingPosition, PresetSize, RelativeTo};
use serde::{Deserialize, Serialize};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::XdgToplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::utils::Coordinate;
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
use crate::utils::with_toplevel_role;
use crate::window::{Mapped, Unmapped};

const VERSION: u32 = 1;

/// How long after the last toplevel update that the sessions should be saved.
const SESSION_SAVE_DELAY: Duration = Duration::from_secs(3);

/// How often to automatically update the state of session-tracked toplevels.
const AUTO_UPDATE_INTERVAL: Duration = Duration::from_secs(10 * 60); // from_mins is unstable

/// Used to globally identify a session.
pub type SessionId = String;

/// Used to identify an application.
pub type AppId = String;

/// A reference to a specific toplevel session.
#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq)]
pub struct ToplevelSessionRef {
    pub session_id: SessionId,
    pub toplevel_id: ToplevelId,
    app_id: Option<AppId>,
}

/// Session data for all applications.
///
/// A global object shared by all clients.
#[derive(Debug, Clone)]
pub struct SessionManagerState {
    /// Token for any currently running auto save timer.
    pub auto_save_timer: Option<RegistrationToken>,
    /// Used to send events whenever a toplevel's state is refreshed.
    toplevel_updated: Sender<ToplevelUpdated>,
    /// Maps session IDs to `xx_session_v1` data.
    sessions: HashMap<SessionId, SessionState>,
}

/// Emitted whenever a toplevel's session state is updated.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ToplevelUpdated {
    session_ref: ToplevelSessionRef,
}

pub struct SessionManagerGlobalData {
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

impl SessionManagerState {
    pub fn new<D, F>(display: &DisplayHandle, filter: F, event_loop: &LoopHandle<D>) -> Self
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

        let (toplevel_updated, on_toplevel_updated) = calloop::channel::channel();
        event_loop
            .insert_source(on_toplevel_updated, move |_, _, state| {
                state.save_sessions_after(SESSION_SAVE_DELAY)
            })
            .unwrap();

        let auto_update_timer = Timer::from_duration(AUTO_UPDATE_INTERVAL);
        event_loop
            .insert_source(auto_update_timer, |_, _, state| {
                debug!("auto-updating session state...");
                state.update_tracked_toplevels();
                // Reschedule to run again.
                TimeoutAction::ToDuration(AUTO_UPDATE_INTERVAL)
            })
            .unwrap();

        let loaded_sessions = Self::load_sessions();

        Self {
            auto_save_timer: None,
            toplevel_updated,
            sessions: loaded_sessions.unwrap_or_default(),
        }
    }

    /// Updates the session state of the given toplevel.
    pub fn update_toplevel(&mut self, mapped: &Mapped, layout: &Layout<Mapped>) {
        mapped.session_ref().and_then(|session_ref| {
            self.sessions
                .get_mut(&session_ref.session_id)
                .and_then(|session| session.sessions.get_mut(&session_ref.toplevel_id))
                .map(|session_state| session_state.update(mapped, layout))
                .map(|_| {
                    let event = ToplevelUpdated {
                        session_ref: session_ref.clone(),
                    };
                    if let Err(error) = self.toplevel_updated.send(event) {
                        error!("failed to send ToplevelUpdated event: {}", error);
                    }
                })
        });
    }

    /// Checks whether a session with the given ID exists.
    pub fn session_exists(&self, session_id: &SessionId) -> bool {
        self.sessions.contains_key(session_id)
    }

    /// Saves session data to persistent storage.
    pub fn save(&self) {
        let json = match serde_json::to_string(&self.sessions) {
            Ok(json) => json,
            Err(error) => {
                error!("failed to serialize sessions data: {}", error);
                return;
            }
        };
        let sessions_path = Self::resolve_session_data_path();
        match fs::write(sessions_path.clone(), json) {
            Ok(_) => {
                info!("saved sessions data to {:?}", sessions_path);
            }
            Err(error) => {
                error!("failed to save sessions data: {}", error);
            }
        }
    }

    /// Loads saved sessions from persistent storage.
    pub fn load_sessions() -> Option<HashMap<SessionId, SessionState>> {
        let sessions_path = Self::resolve_session_data_path();
        let sessions_json = match fs::read_to_string(sessions_path.clone()) {
            Ok(json) => json,
            Err(error) => {
                error!("failed to read sessions data file: {}", error);
                return None;
            }
        };
        match serde_json::from_str::<HashMap<SessionId, SessionState>>(&sessions_json) {
            Ok(state) => {
                info!("loaded sessions data from `{:?}`", sessions_path);
                error!("state `{:?}`", state);
                Some(state)
            }
            Err(error) => {
                warn!("failed to deserialize sessions data: {}", error);
                warn!("removing sessions data and starting over from scratch...");
                match fs::remove_file(sessions_path.clone()) {
                    Ok(_) => {}
                    Err(error) => {
                        error!("failed to delete `{:?}`: {}", sessions_path, error);
                    }
                };
                None
            }
        }
    }

    fn resolve_session_data_path() -> PathBuf {
        let system_path = Self::system_session_data_path();

        if let Some(path) = Self::default_session_data_path() {
            // Use default path if it exists.
            if path.exists() {
                return path;
            }

            // Otherwise, use system path if it exists.
            if system_path.exists() {
                return system_path;
            }

            // Prefer default path if none exist already.
            return path;
        }

        system_path
    }

    /// Default is `$XDG_CONFIG_HOME/niri/sessions.json`.
    fn default_session_data_path() -> Option<PathBuf> {
        let Some(dirs) = ProjectDirs::from("", "", "niri") else {
            warn!("error retrieving home directory");
            return None;
        };

        let mut path = dirs.config_dir().to_owned();
        path.push("sessions.json");
        Some(path)
    }

    fn system_session_data_path() -> PathBuf {
        PathBuf::from("/etc/niri/sessions.json")
    }

    fn generate_unique_session_id(state: &mut impl SessionManagementHandler) -> Option<String> {
        repeat_with(|| {
            repeat_with(fastrand::alphanumeric)
                .take(32)
                .collect::<String>()
        })
        // Keep generating new ones until a unique one is found...
        .find(|new_session_id| !state.session_exists(new_session_id))
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
                let Some(session_id) = maybe_session_id
                    // Unknown session IDs are treated as None.
                    .filter(|session_id| state.session_exists(session_id))
                    // A valid session ID was not provided, so generate a unique one.
                    .or_else(|| Self::generate_unique_session_id(state))
                else {
                    error!("cannot create new session: unable to generate unique session id");
                    return;
                };

                // TODO: implement session replacing.
                let client_id = client.id();
                if state.any_window_in_session(&client_id, &session_id) {
                    let error_message = format!(
                        "session `{}` already in use by client {:?}",
                        session_id, client_id
                    );
                    warn!("{}", error_message);
                    manager.post_error(xx_session_manager_v1::Error::InUse, error_message);
                    return;
                }

                let sessions = &mut state.session_management_state().sessions;

                let restoring = sessions.contains_key(&session_id);

                let new_session_state = SessionState::new(session_id.clone());
                let session_state = sessions
                    .entry(session_id.clone())
                    .or_insert(new_session_state);

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
    /// Checks whether a session with the given ID exists.
    fn session_exists(&self, session_id: &SessionId) -> bool;
    /// Get a reference to the [`XdgShellState`].
    fn xdg_shell_state(&self) -> &XdgShellState;
    /// Finds a window mapped to the given surface, if it exists.
    fn find_mapped_window(&mut self, surface: &WlSurface) -> Option<&Mapped>;
    /// Gets the currently unmapped windows.
    fn unmapped_windows(&mut self) -> &mut HashMap<WlSurface, Unmapped>;
    /// Removes a session from any mapped window associated with the surface.
    fn remove_session_from_mapped(&mut self, surface: ToplevelSessionRef);
    /// Checks if any of the client's windows are part of a given session.
    fn any_window_in_session(&self, client_id: &ClientId, session_id: &SessionId) -> bool;
    /// Schedule autosave of sessions to happen after a given delay.
    fn save_sessions_after(&mut self, delay: Duration);
    /// Updates the state of any session-tracked toplevels.
    fn update_tracked_toplevels(&mut self);
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
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionState {
    session_id: SessionId,
    /// Maps toplevel IDs ("names") to `xx_toplevel_session_v1` data.
    sessions: HashMap<ToplevelId, ToplevelSessionState>,
}

impl SessionState {
    fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            sessions: Default::default(),
        }
    }
}

/// A session which may store persistent state for a toplevel.
#[derive(Debug, Clone, PartialEq)]
pub struct ToplevelSession {
    surface: WlSurface,
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
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ToplevelSessionState {
    session_ref: ToplevelSessionRef,
    workspace: Option<ToplevelSessionWorkspace>,
    attributes: Option<WindowAttributes>,
}

impl ToplevelSessionState {
    fn new(session_ref: ToplevelSessionRef) -> Self {
        Self {
            session_ref,
            workspace: None,
            attributes: None,
        }
    }

    fn get_ref(&self) -> ToplevelSessionRef {
        self.session_ref.clone()
    }

    pub fn update(&mut self, mapped: &Mapped, layout: &Layout<Mapped>) {
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
                width: mapped.window.geometry().size.w,
                height: mapped.window.geometry().size.h,
                x: mapped.window.geometry().loc.x,
                y: mapped.window.geometry().loc.y,
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
            WindowAttributes::Floating { width, .. } => PresetSize::Fixed(*width),
        })
    }

    fn initial_height(&self) -> Option<PresetSize> {
        self.attributes.as_ref().map(|window| match window {
            WindowAttributes::Scrolling {
                height: WindowHeight::Fixed(height),
                ..
            } => PresetSize::Fixed(*height as i32),
            WindowAttributes::Scrolling { height: _, .. } => PresetSize::Proportion(1.0),
            WindowAttributes::Floating { height, .. } => PresetSize::Fixed(*height),
        })
    }

    fn initial_floating_position(&self) -> Option<FloatingPosition> {
        self.attributes.as_ref().and_then(|window| match window {
            WindowAttributes::Floating { x, y, .. } => Some(FloatingPosition {
                x: FloatOrInt(x.to_f64()),
                y: FloatOrInt(y.to_f64()),
                relative_to: RelativeTo::TopLeft,
            }),
            _ => None,
        })
    }
}

/// Identifies a workspace by name (if it has one) or an ID.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ToplevelSessionWorkspace {
    Named(String),
    Unnamed(WorkspaceId),
}

/// Describes where a toplevel window is located.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
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
        /// The window's width, in logical pixels.
        width: i32,
        /// The window's height, in logical pixels.
        height: i32,
        /// The window's horizontal coordinate, in logical pixels from the left side.
        x: i32,
        /// The window's vertical coordinate, in logical pixels from the top.
        y: i32,
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
                        .map(|(_, toplevel_state)| toplevel_state.get_ref())
                        .for_each(|session_ref| state.remove_session_from_mapped(session_ref))
                });
                debug!("removed session `{}`", data.session_id);
            }
            xx_session_v1::Request::AddToplevel {
                id,
                toplevel,
                name: toplevel_id,
            } => {
                add_toplevel(
                    false,
                    state,
                    session,
                    data,
                    data_init,
                    id,
                    &toplevel,
                    toplevel_id,
                );
            }
            xx_session_v1::Request::RestoreToplevel {
                id,
                toplevel,
                name: toplevel_id,
            } => {
                add_toplevel(
                    true,
                    state,
                    session,
                    &data,
                    data_init,
                    id,
                    &toplevel,
                    toplevel_id,
                );
            }
        }
    }
}

fn add_toplevel<D>(
    is_restoring: bool,
    state: &mut D,
    session: &XxSessionV1,
    data: &SessionState,
    data_init: &mut DataInit<D>,
    id: New<XxToplevelSessionV1>,
    toplevel: &XdgToplevel,
    toplevel_id: String,
) where
    D: Dispatch<XxSessionV1, SessionState>,
    D: Dispatch<XxToplevelSessionV1, ToplevelSessionState>,
    D: SessionManagementHandler,
    D: 'static,
{
    let op_name = if is_restoring { "restore" } else { "add" };
    let Some(surface) = state.xdg_shell_state().get_toplevel(&toplevel) else {
        error!(
            "Tried to {} a toplevel session with an invalid toplevel",
            op_name
        );
        return;
    };

    let Some(unmapped) = state.unmapped_windows().get_mut(surface.wl_surface()) else {
        error!("Unable to find unmapped window");
        return;
    };

    let app_id = unmapped
        .window
        .toplevel()
        .and_then(|toplevel| with_toplevel_role(toplevel, |role| role.app_id.clone()));

    let new_session_ref = ToplevelSessionRef {
        session_id: data.session_id.clone(),
        toplevel_id: toplevel_id.clone(),
        app_id: app_id.clone(),
    };
    if unmapped
        .session
        .as_ref()
        .is_some_and(|session| session.state.session_ref == new_session_ref)
    {
        let error_message = format!(
            "cannot {}: toplevel `{}` is already present in session",
            op_name, toplevel_id
        );
        warn!("{}", error_message);
        session.post_error(xx_session_v1::Error::NameInUse, error_message);
        return;
    }

    if let Some(mapped) = state.find_mapped_window(surface.wl_surface()) {
        let error_message = format!(
            "cannot {}: toplevel {:?} was already mapped when restored",
            op_name,
            mapped
                .window
                .toplevel()
                .expect("no x11 support")
                .xdg_toplevel()
                .id()
        );
        warn!("{}", error_message);
        session.post_error(xx_session_v1::Error::AlreadyMapped, error_message);
        return;
    }

    let sessions = &mut state.session_management_state().sessions;

    let new_toplevel_session_state = ToplevelSessionState::new(new_session_ref);

    // We may either create a new toplevel session state, or fetch an existing one.
    let toplevel_session_state = if is_restoring {
        let Some(session_state) = sessions.get(&data.session_id) else {
            error!("Unable to find session with id `{}`", data.session_id);
            return;
        };
        // First, try to get state from current session.
        let toplevel_session_state = session_state.sessions.get(&toplevel_id).cloned();
        // If it doesn't exist, see if there's an old session we can restore this toplevel from.
        let toplevel_session_state = toplevel_session_state.or_else(|| {
            sessions
                .values()
                .find(|old_session| {
                    old_session
                        .sessions
                        .get(&toplevel_id)
                        .is_some_and(|old_toplevel_session| {
                            // Ensure old session is from same app, to avoid toplevel ID conflicts.
                            old_toplevel_session.session_ref.app_id == app_id
                        })
                })
                .and_then(|old_session| old_session.sessions.get(&toplevel_id).cloned())
                .map(|old_toplevel_session| {
                    info!(
                        "couldn't find toplevel `{}` in current session `{}`, \
                        instead restoring from session `{}`...",
                        toplevel_id, data.session_id, old_toplevel_session.session_ref.session_id
                    );
                    // Replace the session reference to adopt it into the current session.
                    ToplevelSessionState {
                        session_ref: ToplevelSessionRef {
                            session_id: data.session_id.clone(),
                            ..old_toplevel_session.session_ref
                        },
                        ..old_toplevel_session
                    }
                })
        });
        // Otherwise, start with a blank slate.
        let toplevel_session_state =
            toplevel_session_state.unwrap_or_else(|| new_toplevel_session_state);
        toplevel_session_state
    } else {
        let Some(session_state) = sessions.get_mut(&data.session_id) else {
            error!("Unable to find session with id `{}`", data.session_id);
            return;
        };
        session_state
            .sessions
            .insert(toplevel_id, new_toplevel_session_state.clone());
        new_toplevel_session_state
    };

    let toplevel_resource = data_init.init(id, toplevel_session_state.clone());

    let Some(unmapped) = state.unmapped_windows().get_mut(surface.wl_surface()) else {
        error!("Unable to find unmapped window");
        return;
    };

    debug!(
        "{} toplevel with session state: {:?}",
        if is_restoring { "restoring" } else { "added" },
        toplevel_session_state
    );

    let toplevel_session = ToplevelSession {
        surface: surface.wl_surface().clone(),
        resource: toplevel_resource,
        state: toplevel_session_state.clone(),
        is_restoring,
    };

    unmapped.session = Some(toplevel_session);
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
