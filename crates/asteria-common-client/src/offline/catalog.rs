use std::collections::{BTreeMap, HashMap};

use asteria_common_api::ItemId;

use super::store::{Doc, MediaKey, PinTarget};
use crate::rpc::ClientError;
use crate::session::{Request, Response};

/// One RPC as the sync makes it.
pub trait Fetch {
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ClientError>;
}

/// What an app knows about its own answers. Runs on worker threads.
pub trait OfflineCatalog: Send + Sync + 'static {
    /// Every document `pin` needs, fetched through `rpc` (paging and following links as
    /// the app must). `Err(ClientError::NotFound)` means the server no longer has it.
    fn fetch(&self, pin: &PinTarget, rpc: &mut dyn Fetch) -> Result<Vec<Doc>, ClientError>;
    /// The media `doc` points to.
    fn media_of(&self, doc: &Doc) -> Vec<MediaKey>;
    /// The answer to `request` from the saved documents; `None` if it cannot be answered.
    fn answer(&self, request: &Request, docs: &Docs) -> Option<Result<Response, ClientError>>;
}

/// The saved answer copies of one library, as a catalog reads them.
#[derive(Debug, Default)]
pub struct Docs {
    /// kind -> id -> doc.
    by_kind: BTreeMap<String, BTreeMap<String, Doc>>,
    /// Every item a doc points to, with its content hash.
    hashes: HashMap<ItemId, String>,
}

impl Docs {
    pub fn new(docs: Vec<Doc>, catalog: &dyn OfflineCatalog) -> Docs {
        let mut out = Docs::default();
        for doc in docs {
            for key in catalog.media_of(&doc) {
                out.hashes.insert(key.item, key.file_hash);
            }
            out.by_kind
                .entry(doc.kind.clone())
                .or_default()
                .insert(doc.id.clone(), doc);
        }
        out
    }

    pub fn get(&self, kind: &str, id: &str) -> Option<&Doc> {
        self.by_kind.get(kind)?.get(id)
    }

    /// The docs of `kind`, sorted by id.
    pub fn of_kind<'a>(&'a self, kind: &'a str) -> impl Iterator<Item = &'a Doc> + 'a {
        self.by_kind.get(kind).into_iter().flat_map(|m| m.values())
    }

    /// The content hash of `item`, if a saved doc points to it.
    pub fn hash_of(&self, item: &ItemId) -> Option<&str> {
        self.hashes.get(item).map(String::as_str)
    }

    /// Every doc, by kind then id.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Doc> {
        self.by_kind.values().flat_map(|m| m.values())
    }
}
