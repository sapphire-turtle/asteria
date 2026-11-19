//! The server's corundum journal: its context, the mappings and the wait helper.

mod common;

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use asteria_common_api::{SkipReason, codes};
use asteria_common_core::journal::Actor;
use asteria_common_core::{Library, UserId};
use asteria_common_server::Cache;
use asteria_common_server::journal::map::{
    actor_of, asteria_actor, origin_key, origin_mcp, origin_scan, reason_of, rpc_error, skip_of,
};
use asteria_common_server::journal::{JournalCtx, open_journal, wait_up_to};
use asteria_common_server::keys::KeyId;
use corundum_framework::journal::{
    Applied, BatchId, Journal, JournalConfig, JournalError, MemoryStore, OpKind, Operation, Origin,
    Planned, Registry, Skip,
};
use grain_id::GrainId;

#[test]
fn actors_round_trip() {
    let u = UserId(GrainId::random());
    for a in [Actor::User(u), Actor::Agent { on_behalf_of: u }] {
        assert_eq!(asteria_actor(&actor_of(a)), Some(a));
    }
    assert!(!actor_of(Actor::User(u)).agent);
    assert!(actor_of(Actor::Agent { on_behalf_of: u }).agent);
    assert_eq!(actor_of(Actor::User(u)).principal, u.to_string());
    let stranger = corundum_framework::journal::Actor::person("not a user id");
    assert_eq!(asteria_actor(&stranger), None);
}

#[test]
fn origins_are_named_by_source() {
    let key = KeyId::new("0190b3a8c1d27e4a8f5b2c6d9e0f1a2b").unwrap();
    assert_eq!(
        origin_key(&key).as_str(),
        "key:0190b3a8c1d27e4a8f5b2c6d9e0f1a2b"
    );
    assert_eq!(origin_mcp("s1").as_str(), "mcp:s1");
    assert_eq!(origin_scan().as_str(), "scan");
}

#[test]
fn skips_map_both_ways() {
    let back = [
        (SkipReason::Missing, SkipReason::Missing),
        (SkipReason::Changed, SkipReason::Changed),
        (SkipReason::HashMismatch, SkipReason::Changed),
        (SkipReason::TargetExists, SkipReason::TargetExists),
        (SkipReason::Symlink, SkipReason::Symlink),
        (SkipReason::Io, SkipReason::Io),
    ];
    for (reason, expected) in back {
        assert_eq!(reason_of(&skip_of(reason)), expected);
    }
    assert_eq!(skip_of(SkipReason::Missing), Skip::NotFound);
    assert_eq!(skip_of(SkipReason::HashMismatch), Skip::PreconditionFailed);
    assert_eq!(skip_of(SkipReason::Symlink), Skip::InvalidPath);
    assert!(matches!(skip_of(SkipReason::Io), Skip::Io(_)));
    assert_eq!(reason_of(&Skip::Denied), SkipReason::Io);
}

#[test]
fn expired_and_forbidden_have_their_codes() {
    let id = BatchId::random();
    assert_eq!(codes::UNDO_EXPIRED, -32010);
    assert_eq!(codes::FORBIDDEN, -32011);
    assert_eq!(
        rpc_error(JournalError::Expired(id)).code,
        codes::UNDO_EXPIRED
    );
    assert_eq!(
        rpc_error(JournalError::Forbidden(id)).code,
        codes::FORBIDDEN
    );
    assert_eq!(
        rpc_error(JournalError::AlreadyUndone(id)).code,
        codes::ALREADY_UNDONE
    );
    assert_eq!(rpc_error(JournalError::NotFound(id)).code, codes::NOT_FOUND);
    assert_eq!(rpc_error(JournalError::Busy).code, codes::BUSY);
    assert_eq!(rpc_error(JournalError::Stopped).code, codes::INTERNAL_ERROR);
}

struct Nap;
impl OpKind for Nap {
    const KIND: &'static str = "nap";
    type Plan = ();
    type Done = ();
}
impl Operation<()> for Nap {
    fn apply(&self, _: &(), _: &()) -> Result<Applied<()>, Skip> {
        std::thread::sleep(Duration::from_millis(300));
        Ok(Applied::new(()))
    }
    fn inverse(&self, _: &()) -> Option<()> {
        None
    }
}

#[tokio::test]
async fn wait_up_to_gives_none_on_time() {
    let mut reg = Registry::new();
    reg.register(Nap);
    let j = Journal::new(reg, (), MemoryStore::new(10), JournalConfig::default());
    let id = j
        .submit(
            vec![Planned::new::<Nap>(&())],
            &Origin::new("t"),
            &corundum_framework::journal::Actor::person("p"),
            None,
        )
        .unwrap();
    assert!(
        wait_up_to(&j, id, Duration::from_millis(50))
            .await
            .unwrap()
            .is_none()
    );
    let done = wait_up_to(&j, id, Duration::from_secs(1)).await.unwrap();
    assert!(done.unwrap().summary.complete);
}

#[test]
fn a_server_starts_over_an_old_journal() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    fs::create_dir(&root).unwrap();
    let state = tmp.path().join("state");
    let lib = Arc::new(
        Library::init(&root, "owner")
            .unwrap()
            .with_state_dir(&state),
    );
    let cache = Arc::new(Cache::open(&tmp.path().join("cache"), &lib.config().id).unwrap());
    let old = state.join("journal");
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join("2026-11.jsonl"), b"{}\n").unwrap();
    let ctx = JournalCtx {
        library: lib,
        cache,
        ext: Arc::new(common::Photos),
    };
    let _journal = open_journal(ctx, None).unwrap();
    assert!(state.join("journal-v1/2026-11.jsonl").exists());
    assert!(state.join("journal/journal.lock").exists());
}
