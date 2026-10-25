//! Connection failures in words the user can act on, shared by every screen that shows one.

use asteria_framework_api::HandshakeError;
use corundum_framework::egui;
use corundum_framework::gui::tr;

use crate::rpc::ClientError;

/// A connection failure in words the user can act on, in the installed language. The
/// error texts never contain the token. Failures whose details are technical (a malformed
/// answer, a server-side error, a local failure) get a general sentence; their details are in
/// [`error_detail`], for a hover or the log.
pub fn error_text(ctx: &egui::Context, e: &ClientError) -> String {
    match e {
        ClientError::Unreachable(detail) => tr!(
            ctx,
            "asteria-client-servers-error-unreachable",
            detail = detail.as_str()
        ),
        ClientError::Unauthorized => tr!(ctx, "asteria-client-servers-error-unauthorized"),
        ClientError::Handshake(HandshakeError::AppMismatch { .. }) => {
            tr!(ctx, "asteria-client-servers-error-app-mismatch")
        }
        ClientError::Handshake(HandshakeError::ApiMismatch { client, server }) => tr!(
            ctx,
            "asteria-client-servers-error-api-mismatch",
            client = client.to_string(),
            server = server.to_string()
        ),
        ClientError::Retry { after } => tr!(
            ctx,
            "asteria-client-servers-error-busy",
            seconds = after.as_secs()
        ),
        ClientError::Unprocessable => tr!(ctx, "asteria-client-servers-error-unprocessable"),
        ClientError::NotFound => tr!(ctx, "asteria-client-servers-error-not-found"),
        ClientError::LibraryChanged => tr!(ctx, "asteria-client-servers-error-library-changed"),
        ClientError::Rpc { .. } => tr!(ctx, "asteria-client-servers-error-server"),
        ClientError::Protocol(_) => tr!(ctx, "asteria-client-servers-error-protocol"),
        ClientError::Io(_) => tr!(ctx, "asteria-client-servers-error-local"),
    }
}

/// The technical details [`error_text`] leaves out (untranslated, as the server or the system
/// gave them), for a hover or the log; `None` when the sentence says everything.
pub fn error_detail(e: &ClientError) -> Option<String> {
    match e {
        ClientError::Rpc { .. } | ClientError::Protocol(_) | ClientError::Io(_) => {
            Some(e.to_string())
        }
        _ => None,
    }
}
