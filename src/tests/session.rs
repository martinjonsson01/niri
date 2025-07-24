use std::{env, io};

use insta::assert_snapshot;
use niri_ipc::SizeChange;
use tracing_subscriber::EnvFilter;
use wayland_client::protocol::wl_surface::WlSurface;

use super::*;
use crate::tests::client::ClientId;
use crate::tests::raw::xx_session_management::v1::client::xx_session_manager_v1::Reason;
use crate::tests::raw::xx_session_management::v1::client::xx_session_v1::XxSessionV1;

const TEST_WINDOW_NAME: &str = "test-window-name";

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

#[test]
fn session_remembers_column_width() {
    let directives = "niri=trace,smithay::backend::renderer::gles=error";
    let env_filter = EnvFilter::builder().parse_lossy(directives);
    tracing_subscriber::fmt()
        .compact()
        .with_writer(io::stderr)
        .with_env_filter(env_filter)
        .init();

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let session = f.client(id).get_session(Reason::Launch, None);

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

    // Restore window.
    let surface = init_window(&mut f, id, &session, true);

    // This should make it the same width as before.
    assert_snapshot!(
        f.client(id).window(&surface).format_recent_configures(),
        @"size: 500 × 1048, bounds: 1888 × 1048, states: [Activated]"
    );
}
