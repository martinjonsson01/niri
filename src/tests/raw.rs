#[cfg(feature = "xx-session-management")]
pub mod xx_session_management {
    pub mod v1 {
        pub use self::generated::client;

        mod generated {
            pub mod client {
                #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
                #![allow(non_upper_case_globals, non_snake_case, unused_imports)]
                #![allow(missing_docs, clippy::all)]

                use smithay::reexports::wayland_protocols::xdg::shell::client::xdg_toplevel;
                use wayland_client;

                pub mod __interfaces {
                    use smithay::reexports::wayland_protocols::xdg::shell::server::__interfaces::*;
                    wayland_scanner::generate_interfaces!("resources/xx-session-management-v1.xml");
                }
                use self::__interfaces::*;

                wayland_scanner::generate_client_code!("resources/xx-session-management-v1.xml");
            }
        }
    }
}
