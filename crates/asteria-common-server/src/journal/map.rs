//! Between asteria's types and corundum's journal: actors, origins, skips and errors.

use asteria_common_api::{RpcError, SkipReason, codes};
use asteria_common_core::UserId;
use asteria_common_core::journal::Actor as CoreActor;
use corundum_framework::journal::{Actor, JournalError, Origin, Skip};
use corundum_framework::keys::KeyId;

/// The journal's actor for `a`: the person, or the agent acting for them.
pub fn actor_of(a: CoreActor) -> Actor {
    match a {
        CoreActor::User(u) => Actor::person(u.to_string()),
        CoreActor::Agent { on_behalf_of } => Actor::agent(on_behalf_of.to_string()),
    }
}

/// The actor `a` stands for; `None` when its principal is not a user id.
pub fn asteria_actor(a: &Actor) -> Option<CoreActor> {
    let user: UserId = a.principal.parse().ok()?;
    Some(if a.agent {
        CoreActor::Agent { on_behalf_of: user }
    } else {
        CoreActor::User(user)
    })
}

/// What an API key's requests share: one undo stack per key.
pub fn origin_key(key: &KeyId) -> Origin {
    Origin::new(format!("key:{key}"))
}

/// What one MCP session's requests share.
pub fn origin_mcp(session: &str) -> Origin {
    Origin::new(format!("mcp:{session}"))
}

/// What the scan's own changes share.
pub fn origin_scan() -> Origin {
    Origin::new("scan")
}

pub fn skip_of(r: SkipReason) -> Skip {
    match r {
        SkipReason::Missing => Skip::NotFound,
        SkipReason::Changed | SkipReason::HashMismatch => Skip::PreconditionFailed,
        SkipReason::TargetExists => Skip::Exists,
        SkipReason::Symlink => Skip::InvalidPath,
        SkipReason::Io => Skip::Io("i/o error".into()),
    }
}

pub fn reason_of(s: &Skip) -> SkipReason {
    match s {
        Skip::NotFound => SkipReason::Missing,
        Skip::PreconditionFailed => SkipReason::Changed,
        Skip::Exists => SkipReason::TargetExists,
        Skip::InvalidPath => SkipReason::Symlink,
        _ => SkipReason::Io,
    }
}

/// The RPC error for a refused or failed journal call.
pub fn rpc_error(e: JournalError) -> RpcError {
    let code = match &e {
        JournalError::Expired(_) => codes::UNDO_EXPIRED,
        JournalError::Forbidden(_) => codes::FORBIDDEN,
        JournalError::AlreadyUndone(_) => codes::ALREADY_UNDONE,
        JournalError::NotFound(_) => codes::NOT_FOUND,
        JournalError::Busy => codes::BUSY,
        _ => {
            tracing::error!(error = %e, "journal internal error");
            return crate::rpc_error(codes::INTERNAL_ERROR, "internal error");
        }
    };
    crate::rpc_error(code, e.to_string())
}
