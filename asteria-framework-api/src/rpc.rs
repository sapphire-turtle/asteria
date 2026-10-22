//! JSON-RPC 2.0 envelope and method names.

use serde::{Deserialize, Serialize};

/// A JSON-RPC request. `P` is the method's parameter type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcRequest<P> {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    pub params: P,
}

/// A JSON-RPC response. `R` is the method's result type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcResponse<R> {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    pub id: u64,
    #[serde(flatten)]
    pub outcome: RpcOutcome<R>,
}

/// Either the `result` or the `error` member of a response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RpcOutcome<R> {
    #[serde(rename = "result")]
    Ok(R),
    #[serde(rename = "error")]
    Err(RpcError),
}

/// The `error` member of a failed response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

/// Error codes: the JSON-RPC standard ones and asteria's own.
pub mod codes {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
    pub const NOT_FOUND: i32 = -32001;
    pub const INVALID_PATH: i32 = -32002;
    pub const READ_ONLY: i32 = -32003;
    pub const BUSY: i32 = -32004;
    pub const API_MISMATCH: i32 = -32005;
}

pub const SERVER_INFO: &str = "server.info";
pub const LIBRARY_INFO: &str = "library.info";
pub const LIBRARY_RESCAN: &str = "library.rescan";
pub const FOLDER_LIST: &str = "folder.list";
pub const MEDIA_GET: &str = "media.get";
pub const MEDIA_UPDATE: &str = "media.update";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_response_ok_and_err_shapes() {
        let ok = RpcResponse {
            jsonrpc: "2.0".into(),
            id: 1,
            outcome: RpcOutcome::Ok(5),
        };
        let json = serde_json::to_string(&ok).unwrap();
        assert_eq!(json, r#"{"jsonrpc":"2.0","id":1,"result":5}"#);
        assert_eq!(serde_json::from_str::<RpcResponse<i32>>(&json).unwrap(), ok);

        let err = RpcResponse::<i32> {
            jsonrpc: "2.0".into(),
            id: 1,
            outcome: RpcOutcome::Err(RpcError {
                code: codes::NOT_FOUND,
                message: "x".into(),
            }),
        };
        let json = serde_json::to_string(&err).unwrap();
        assert_eq!(
            json,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32001,"message":"x"}}"#
        );
        assert_eq!(
            serde_json::from_str::<RpcResponse<i32>>(&json).unwrap(),
            err
        );
    }

    #[test]
    fn rpc_request_roundtrip() {
        let r = RpcRequest {
            jsonrpc: "2.0".into(),
            id: 7,
            method: MEDIA_GET.into(),
            params: vec![1u8],
        };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(
            serde_json::from_str::<RpcRequest<Vec<u8>>>(&json).unwrap(),
            r
        );
    }
}
