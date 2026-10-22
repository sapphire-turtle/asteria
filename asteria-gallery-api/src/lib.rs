//! Wire types shared between the asteria-gallery server and its clients.

pub use asteria_framework_api::*;

/// Application identifier sent in the handshake.
pub const APP: &str = "asteria-gallery";
/// Version of this application's API.
pub const APP_API: u32 = 1;

/// The [`ClientInfo`] a client of this application sends.
pub fn client_info() -> ClientInfo {
    ClientInfo {
        app: APP.into(),
        app_api: APP_API,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_info_uses_app_constants() {
        assert_eq!(
            client_info(),
            ClientInfo {
                app: "asteria-gallery".into(),
                app_api: 1
            }
        );
    }
}
