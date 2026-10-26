//! Wire types for the client/server handshake shared by all asteria apps.

use std::fmt;

mod media;
mod rpc;
mod upload;

pub use media::*;
pub use rpc::*;
pub use upload::*;

use serde::{Deserialize, Serialize};

/// `CARGO_PKG_VERSION_MAJOR` as a number. Cargo guarantees it is decimal digits.
///
/// An application API crate derives its `APP_API` from its own major version with
/// `parse_major(env!("CARGO_PKG_VERSION_MAJOR"))`, so the two cannot drift.
pub const fn parse_major(s: &str) -> u32 {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut n = 0u32;
    while i < bytes.len() {
        n = n * 10 + (bytes[i] - b'0') as u32;
        i += 1;
    }
    n
}

/// What a client announces about itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub app: String,
    pub app_api: u32,
}

/// What a server announces about itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub app: String,
    pub app_api: u32,
    pub server_version: String,
    /// Identifies the server's current library state; changes when it is rebuilt.
    pub epoch: String,
}

/// Why a client and server cannot talk to each other.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HandshakeError {
    AppMismatch { client: String, server: String },
    ApiMismatch { client: u32, server: u32 },
}

impl fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AppMismatch { client, server } => {
                write!(f, "app mismatch: client is {client}, server is {server}")
            }
            Self::ApiMismatch { client, server } => {
                write!(
                    f,
                    "API version mismatch: client is {client}, server is {server}"
                )
            }
        }
    }
}

impl std::error::Error for HandshakeError {}

/// Checks the app first, then the API version.
pub fn check_handshake(client: &ClientInfo, server: &ServerInfo) -> Result<(), HandshakeError> {
    if client.app != server.app {
        return Err(HandshakeError::AppMismatch {
            client: client.app.clone(),
            server: server.app.clone(),
        });
    }
    if client.app_api != server.app_api {
        return Err(HandshakeError::ApiMismatch {
            client: client.app_api,
            server: server.app_api,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(app: &str, api: u32) -> ClientInfo {
        ClientInfo {
            app: app.into(),
            app_api: api,
        }
    }

    fn server(app: &str, api: u32) -> ServerInfo {
        ServerInfo {
            app: app.into(),
            app_api: api,
            server_version: "0.1.0".into(),
            epoch: "e1".into(),
        }
    }

    #[test]
    fn handshake_accepts_same_app_and_api() {
        assert_eq!(
            check_handshake(&client("asteria-photos", 1), &server("asteria-photos", 1)),
            Ok(())
        );
    }

    #[test]
    fn handshake_rejects_api_mismatch() {
        assert_eq!(
            check_handshake(&client("asteria-photos", 2), &server("asteria-photos", 1)),
            Err(HandshakeError::ApiMismatch {
                client: 2,
                server: 1
            })
        );
    }

    #[test]
    fn handshake_rejects_other_app() {
        assert_eq!(
            check_handshake(&client("asteria-music", 1), &server("asteria-photos", 1)),
            Err(HandshakeError::AppMismatch {
                client: "asteria-music".into(),
                server: "asteria-photos".into()
            })
        );
    }

    #[test]
    fn parse_major_parses_digits() {
        assert_eq!(parse_major("2"), 2);
        assert_eq!(parse_major("10"), 10);
    }

    #[test]
    fn server_info_wire_format() {
        let s = server("asteria-photos", 2);
        let json = r#"{"app":"asteria-photos","app_api":2,"server_version":"0.1.0","epoch":"e1"}"#;
        assert_eq!(serde_json::to_string(&s).unwrap(), json);
        assert_eq!(serde_json::from_str::<ServerInfo>(json).unwrap(), s);
    }

    #[test]
    fn server_info_json_roundtrip() {
        let s = server("asteria-photos", 1);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<ServerInfo>(&json).unwrap(), s);
    }
}
