//! Wire types shared between the asteria-music server and its clients.

pub use asteria_framework_api::*;

/// Application identifier sent in the handshake.
pub const APP: &str = "asteria-music";
/// Version of this application's API.
pub const APP_API: u32 = parse_major(env!("CARGO_PKG_VERSION_MAJOR"));

/// The [`ClientInfo`] a client of this application sends.
pub fn client_info() -> ClientInfo {
    ClientInfo {
        app: APP.into(),
        app_api: APP_API,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn app_api_tracks_crate_major() {
        assert_eq!(super::APP_API, 2);
    }
}
