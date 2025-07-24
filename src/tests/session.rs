use std::{env, io};

use insta::assert_snapshot;
use niri_config::WorkspaceReference;
use niri_ipc::SizeChange;
use smithay::reexports::wayland_server::Resource;
use tracing_subscriber::EnvFilter;
use wayland_backend::client::ObjectId;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::Proxy;

use super::*;
use crate::protocols::xx_session_management::SessionId;
use crate::tests::client::ClientId;
use crate::tests::raw::xx_session_management::v1::client::xx_session_manager_v1::Reason;
use crate::tests::raw::xx_session_management::v1::client::xx_session_v1::XxSessionV1;

const TEST_WINDOW_NAME: &str = "test-window-name";

fn set_up() -> (Fixture, ClientId, XxSessionV1, SessionId) {
    let directives = "niri=trace,smithay::backend::renderer::gles=error";
    let env_filter = EnvFilter::builder().parse_lossy(directives);
    let _ = tracing_subscriber::fmt()
        .compact()
        .with_writer(io::stderr)
        .with_env_filter(env_filter)
        .try_init();

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let session = f.client(id).get_session(Reason::Launch, None);
    f.double_roundtrip(id);
    let session_id = f.client(id).latest_session_id();
    (f, id, session, session_id)
}

fn init_window(f: &mut Fixture, id: ClientId, session: &XxSessionV1, restore: bool) -> WlSurface {
    let client = f.client(id);
    let window = client.create_window();
    let toplevel = window.xdg_toplevel.clone();
    let surface = window.surface.clone();
    let _toplevel_session = if restore {
        client.session_restore_toplevel(&session, &toplevel, TEST_WINDOW_NAME.to_owned());
    } else {
        client.session_add_toplevel(&session, &toplevel, TEST_WINDOW_NAME.to_owned());
    };
    let window = client.window(&surface);
    window.commit();
    f.roundtrip(id);

    // Clear recent configures.
    let _ = f.client(id).window(&surface).recent_configures();

    // Create window buffer.
    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(100, 100);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    // Commit in response to the Activated state change configure.
    f.client(id).window(&surface).ack_last_and_commit();
    f.double_roundtrip(id);

    surface
}

fn get_current_workspace_name(f: &mut Fixture, toplevel_id: ObjectId) -> &String {
    let workspace = f
        .niri()
        .layout
        .workspaces()
        .find_map(|(_, _, ws)| {
            ws.windows().find_map(|win| {
                win.window
                    .toplevel()
                    .filter(|toplevel| {
                        format!("{:?}", toplevel.xdg_toplevel().id())
                            == format!("{:?}", toplevel_id)
                    })
                    .map(|_| ws)
            })
        })
        .unwrap();
    workspace.name().unwrap()
}

fn is_window_floating(f: &mut Fixture, toplevel_id: ObjectId) -> bool {
    f.niri()
        .layout
        .windows()
        .find(|(_, mapped)| {
            format!(
                "{:?}",
                mapped.window.toplevel().unwrap().xdg_toplevel().id()
            ) == format!("{:?}", toplevel_id)
        })
        .map(|(_, mapped)| mapped.is_floating())
        .unwrap()
}

#[test]
fn session_remembers_column_width() {
    let (mut f, id, session, session_id) = set_up();

    // Create initial window.
    let surface = init_window(&mut f, id, &session, false);

    // Resize column.
    f.niri().layout.set_column_width(SizeChange::SetFixed(500));
    f.double_roundtrip(id);

    // This should resize to the new width of 500.
    assert_snapshot!(
        f.client(id).window(&surface).format_recent_configures(),
        @r"
        size: 936 × 1048, bounds: 1888 × 1048, states: [Activated]
        size: 500 × 1048, bounds: 1888 × 1048, states: [Activated]
        "
    );

    // Close window (this should save its state).
    f.client(id).close_window(&surface);
    f.double_roundtrip(id);

    // Create new client.
    let id = f.add_client();

    // Restore session.
    let session = f.client(id).get_session(Reason::Launch, Some(session_id));

    // Restore window.
    let surface = init_window(&mut f, id, &session, true);

    // This should make it the same width as before.
    assert_snapshot!(
        f.client(id).window(&surface).format_recent_configures(),
        @"size: 500 × 1048, bounds: 1888 × 1048, states: [Activated]"
    );
}

#[test]
fn session_remembers_workspace() {
    let (mut f, id, session, session_id) = set_up();

    // Set up workspaces.
    f.niri()
        .layout
        .set_workspace_name("first".to_owned(), Some(WorkspaceReference::Index(1)));
    f.niri()
        .layout
        .set_workspace_name("second".to_owned(), Some(WorkspaceReference::Index(2)));
    f.niri()
        .layout
        .set_workspace_name("third".to_owned(), Some(WorkspaceReference::Index(3)));

    // Create initial window.
    let surface = init_window(&mut f, id, &session, false);
    let toplevel_id = f.client(id).window(&surface).xdg_toplevel.id().clone();

    // Move to specific workspace.
    let (workspace_index, _) = f.niri().layout.find_workspace_by_name("third").unwrap();
    f.niri()
        .layout
        .move_column_to_workspace(workspace_index, true);

    // Ensure window is on correct workspace.
    let actual_workspace = get_current_workspace_name(&mut f, toplevel_id);
    assert_eq!(actual_workspace, "third");

    // Close window (this should save its state).
    f.client(id).close_window(&surface);
    f.double_roundtrip(id);

    // Move workspace focus so the new window doesn't spawn there by default.
    f.niri().layout.focus_window_or_workspace_up();

    // Create new client.
    let id = f.add_client();

    // Restore session.
    let session = f.client(id).get_session(Reason::Launch, Some(session_id));

    // Restore window.
    let surface = init_window(&mut f, id, &session, true);
    let toplevel_id = f.client(id).window(&surface).xdg_toplevel.id().clone();

    // Window should be on the same workspace as before.
    let actual_workspace = get_current_workspace_name(&mut f, toplevel_id);
    assert_eq!(actual_workspace, "third");
}

#[test]
fn session_remembers_floating_state() {
    let (mut f, id, session, session_id) = set_up();

    // Create initial window.
    let surface = init_window(&mut f, id, &session, false);
    let toplevel_id = f.client(id).window(&surface).xdg_toplevel.id().clone();

    // Ensure default window state is not floating.
    let is_floating = is_window_floating(&mut f, toplevel_id.clone());
    assert!(!is_floating);

    // Make window floating.
    f.niri().layout.toggle_window_floating(None);
    f.roundtrip(id);

    // Ensure window is floating.
    let is_floating = is_window_floating(&mut f, toplevel_id.clone());
    assert!(is_floating);

    // Close window (this should save its state).
    f.client(id).close_window(&surface);
    f.double_roundtrip(id);

    // Create new client.
    let id = f.add_client();

    // Restore session.
    let session = f.client(id).get_session(Reason::Launch, Some(session_id));

    // Restore window.
    let surface = init_window(&mut f, id, &session, true);
    f.double_roundtrip(id);
    let toplevel_id = f.client(id).window(&surface).xdg_toplevel.id().clone();

    // Window should still be floating.
    let is_floating = is_window_floating(&mut f, toplevel_id.clone());
    assert!(is_floating);
}

#[test]
fn session_restore_toplevel_sends_restored_event() {
    let (mut f, id, session, _) = set_up();

    // Restore window.
    let surface = init_window(&mut f, id, &session, true);

    // We should get a xx_toplevel_session_v1::restored event.
    let window = f.client(id).window(&surface);
    assert!(window.restored);
}

#[test]
fn session_add_toplevel_does_not_send_restored_event() {
    let (mut f, id, session, _) = set_up();

    // Restore window.
    let surface = init_window(&mut f, id, &session, false);

    // We should NOT get a xx_toplevel_session_v1::restored event.
    let window = f.client(id).window(&surface);
    assert!(!window.restored);
}
