pub mod mutter_x11_interop {
    pub mod v1 {
        pub use self::generated::server;

        mod generated {
            pub mod server {
                #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
                #![allow(non_upper_case_globals, non_snake_case, unused_imports)]
                #![allow(missing_docs, clippy::all)]

                use smithay::reexports::wayland_server;
                use wayland_server::protocol::*;

                pub mod __interfaces {
                    use smithay::reexports::wayland_server;
                    use wayland_server::protocol::__interfaces::*;
                    wayland_scanner::generate_interfaces!("resources/mutter-x11-interop.xml");
                }
                use self::__interfaces::*;

                wayland_scanner::generate_server_code!("resources/mutter-x11-interop.xml");
            }
        }
    }
}

#[cfg(feature = "xx-session-management")]
pub mod xx_session_management {
    pub mod v1 {
        pub use self::generated::server;

        mod generated {
            pub mod server {
                #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
                #![allow(non_upper_case_globals, non_snake_case, unused_imports)]
                #![allow(missing_docs, clippy::all)]

                use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
                use smithay::reexports::wayland_server;

                pub mod __interfaces {
                    use smithay::reexports::wayland_protocols::xdg::shell::server::__interfaces::*;
                    wayland_scanner::generate_interfaces!("resources/xx-session-management-v1.xml");
                }
                use self::__interfaces::*;

                wayland_scanner::generate_server_code!("resources/xx-session-management-v1.xml");
            }
        }
    }
}
