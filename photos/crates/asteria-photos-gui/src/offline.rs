//! What the photos app saves for offline use, and how it answers from the copies.

use asteria_common_api::{FOLDER_LIST, FolderListParams, FolderListing, MediaGetParams, MediaItem};
use asteria_common_client::offline::{Doc, Docs, Fetch, MediaKey, OfflineCatalog, PinTarget};
use asteria_common_client::rpc::ClientError;
use asteria_common_client::session::{Request, Response};

/// A pin of one folder and everything below it.
pub const PIN_FOLDER: &str = "folder";

/// Copy kind: one folder (id = its library-relative path, `""` for the root), all
/// pages joined into one [`FolderListing`].
const DOC_FOLDER: &str = "folder";

const PRESET_THUMB: &str = "thumb";
const PRESET_IMAGE: &str = "offline-image";
const THUMB_ESTIMATE: u64 = 20_000;
const IMAGE_ESTIMATE: u64 = 130_000;
const PAGE_LIMIT: u32 = 200;
/// The JSON-RPC code of a missing target.
const RPC_NOT_FOUND: i32 = -32001;

/// The photos app's [`OfflineCatalog`]: folders.
pub struct PhotosCatalog;

fn is_not_found(e: &ClientError) -> bool {
    matches!(
        e,
        ClientError::NotFound
            | ClientError::Rpc {
                code: RPC_NOT_FOUND,
                ..
            }
    )
}

/// Every page of one folder, joined.
fn list_folder(rpc: &mut dyn Fetch, path: &str) -> Result<FolderListing, ClientError> {
    let mut all = FolderListing {
        folders: Vec::new(),
        items: Vec::new(),
        next_cursor: None,
    };
    let mut cursor: Option<String> = None;
    loop {
        let params = FolderListParams {
            path: path.to_owned(),
            cursor: cursor.take(),
            limit: Some(PAGE_LIMIT),
        };
        let params =
            serde_json::to_value(params).map_err(|e| ClientError::Protocol(e.to_string()))?;
        let value = rpc.call(FOLDER_LIST, params)?;
        let page: FolderListing = serde_json::from_value(value)
            .map_err(|e| ClientError::Protocol(format!("{FOLDER_LIST}: {e}")))?;
        all.folders.extend(page.folders);
        all.items.extend(page.items);
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(all),
        }
    }
}

fn child_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

fn page(folders: Vec<String>, items: Vec<MediaItem>) -> Response {
    Response::FolderList(FolderListing {
        folders,
        items,
        next_cursor: None,
    })
}

impl OfflineCatalog for PhotosCatalog {
    fn fetch(&self, pin: &PinTarget, rpc: &mut dyn Fetch) -> Result<Vec<Doc>, ClientError> {
        if pin.kind != PIN_FOLDER {
            return Err(ClientError::Protocol(format!(
                "unknown pin kind {}",
                pin.kind
            )));
        }
        let mut docs = Vec::new();
        let mut queue = std::collections::VecDeque::from([pin.id.clone()]);
        while let Some(path) = queue.pop_front() {
            let listing = match list_folder(rpc, &path) {
                Ok(listing) => listing,
                // A subfolder that vanished since its parent was listed is left out;
                // the pinned folder itself being gone fails the pin.
                Err(e) if is_not_found(&e) && path != pin.id => continue,
                Err(e) => return Err(e),
            };
            for name in &listing.folders {
                queue.push_back(child_path(&path, name));
            }
            let value =
                serde_json::to_value(&listing).map_err(|e| ClientError::Protocol(e.to_string()))?;
            docs.push(Doc {
                kind: DOC_FOLDER.into(),
                id: path,
                value,
            });
        }
        Ok(docs)
    }

    fn media_of(&self, doc: &Doc) -> Vec<MediaKey> {
        let mut out = Vec::new();
        if doc.kind != DOC_FOLDER {
            return out;
        }
        let Ok(listing) = serde_json::from_value::<FolderListing>(doc.value.clone()) else {
            return out;
        };
        for item in &listing.items {
            let Some(hash) = &item.file_hash else {
                continue;
            };
            // A video is saved as its still: the server renders `offline-image` so.
            for (preset, estimate) in [
                (PRESET_THUMB, THUMB_ESTIMATE),
                (PRESET_IMAGE, IMAGE_ESTIMATE),
            ] {
                out.push(MediaKey {
                    item: item.id.clone(),
                    file_hash: hash.clone(),
                    preset: preset.into(),
                    estimate,
                });
            }
        }
        out
    }

    fn answer(&self, request: &Request, docs: &Docs) -> Option<Result<Response, ClientError>> {
        match request {
            Request::FolderList(p) => folder_list(p, docs).map(Ok),
            Request::MediaGet(p) => media_get(p, docs).map(Ok),
            _ => None,
        }
    }
}

fn folder_list(p: &FolderListParams, docs: &Docs) -> Option<Response> {
    if let Some(doc) = docs.get(DOC_FOLDER, &p.path) {
        let listing: FolderListing = serde_json::from_value(doc.value.clone()).ok()?;
        // The copy is one page: a `cursor` asks for one that does not exist.
        return Some(if p.cursor.is_none() {
            page(listing.folders, listing.items)
        } else {
            page(Vec::new(), Vec::new())
        });
    }
    // Not saved itself, but above something saved: only the way down.
    let prefix = if p.path.is_empty() {
        String::new()
    } else {
        format!("{}/", p.path)
    };
    let below: std::collections::BTreeSet<String> = docs
        .of_kind(DOC_FOLDER)
        .filter_map(|d| d.id.strip_prefix(&prefix))
        .filter_map(|rest| rest.split('/').next())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    if below.is_empty() {
        return None;
    }
    let folders = if p.cursor.is_none() {
        below.into_iter().collect()
    } else {
        Vec::new()
    };
    Some(page(folders, Vec::new()))
}

fn media_get(p: &MediaGetParams, docs: &Docs) -> Option<Response> {
    docs.of_kind(DOC_FOLDER)
        .filter_map(|d| serde_json::from_value::<FolderListing>(d.value.clone()).ok())
        .find_map(|l| l.items.into_iter().find(|i| i.id == p.id))
        .map(Response::Media)
}

#[cfg(test)]
mod tests {
    use super::*;
    use asteria_common_api::{ItemId, MediaKind};
    use serde_json::Value;

    /// A `Fetch` that answers from recorded pages, keyed by path and cursor.
    struct Recorded(
        Vec<(
            &'static str,
            Option<&'static str>,
            Result<Value, ClientError>,
        )>,
    );

    impl Fetch for Recorded {
        fn call(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
            assert_eq!(method, FOLDER_LIST);
            assert_eq!(params["limit"], 200);
            let path = params["path"].as_str().unwrap();
            let cursor = params["cursor"].as_str();
            self.0
                .iter()
                .find(|(p, c, _)| *p == path && *c == cursor)
                .map(|(_, _, r)| r.clone())
                .unwrap_or(Err(ClientError::NotFound))
        }
    }

    fn item(id: &str, kind: MediaKind, hash: Option<&str>) -> MediaItem {
        MediaItem {
            id: ItemId(id.into()),
            path: format!("{id}.jpg"),
            kind,
            size: 1,
            modified: "2026-01-01T00:00:00Z".into(),
            rating: None,
            subjects: vec![],
            hierarchical_subjects: vec![],
            description: None,
            label: None,
            content_hash: None,
            file_hash: hash.map(Into::into),
            read_only_reason: None,
        }
    }

    fn listing(folders: &[&str], items: Vec<MediaItem>, next: Option<&str>) -> Value {
        serde_json::to_value(FolderListing {
            folders: folders.iter().map(|f| f.to_string()).collect(),
            items,
            next_cursor: next.map(Into::into),
        })
        .unwrap()
    }

    fn folder_doc(path: &str, folders: &[&str], items: Vec<MediaItem>) -> Doc {
        Doc {
            kind: "folder".into(),
            id: path.into(),
            value: listing(folders, items, None),
        }
    }

    fn pin(path: &str) -> PinTarget {
        PinTarget {
            kind: PIN_FOLDER.into(),
            id: path.into(),
            name: path.into(),
        }
    }

    fn ask(docs: &Docs, path: &str, cursor: Option<&str>) -> Option<FolderListing> {
        let request = Request::FolderList(FolderListParams {
            path: path.into(),
            cursor: cursor.map(Into::into),
            limit: Some(200),
        });
        match PhotosCatalog.answer(&request, docs) {
            Some(Ok(Response::FolderList(l))) => Some(l),
            Some(other) => panic!("unexpected {other:?}"),
            None => None,
        }
    }

    #[test]
    fn a_folder_pin_fetches_every_page_and_subfolder() {
        let mut rpc = Recorded(vec![
            (
                "2026",
                None,
                Ok(listing(
                    &["trip"],
                    vec![item("a", MediaKind::Image, Some("h"))],
                    Some("c1"),
                )),
            ),
            (
                "2026",
                Some("c1"),
                Ok(listing(
                    &[],
                    vec![item("b", MediaKind::Image, Some("h"))],
                    None,
                )),
            ),
            (
                "2026/trip",
                None,
                Ok(listing(&[], vec![item("c", MediaKind::Image, None)], None)),
            ),
        ]);
        let docs = PhotosCatalog.fetch(&pin("2026"), &mut rpc).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].id, "2026");
        assert_eq!(docs[1].id, "2026/trip");
        let l: FolderListing = serde_json::from_value(docs[0].value.clone()).unwrap();
        assert_eq!(l.folders, ["trip"]);
        assert_eq!(l.items.len(), 2);
        assert_eq!(l.next_cursor, None);
    }

    #[test]
    fn a_vanished_subfolder_is_skipped_but_a_vanished_pin_is_not() {
        let mut rpc = Recorded(vec![("", None, Ok(listing(&["gone"], vec![], None)))]);
        let docs = PhotosCatalog.fetch(&pin(""), &mut rpc).unwrap();
        assert_eq!(docs.len(), 1);
        let mut rpc = Recorded(vec![]);
        assert!(matches!(
            PhotosCatalog.fetch(&pin("x"), &mut rpc),
            Err(ClientError::NotFound)
        ));
        let mut rpc = Recorded(vec![
            ("", None, Ok(listing(&["bad"], vec![], None))),
            ("bad", None, Err(ClientError::Unauthorized)),
        ]);
        assert!(matches!(
            PhotosCatalog.fetch(&pin(""), &mut rpc),
            Err(ClientError::Unauthorized)
        ));
    }

    #[test]
    fn an_ancestor_lists_only_the_way_down() {
        let docs = Docs::new(
            vec![
                folder_doc("2026/trip", &[], vec![]),
                folder_doc("2026/home", &[], vec![]),
                folder_doc("2025/x", &[], vec![]),
            ],
            &PhotosCatalog,
        );
        let root = ask(&docs, "", None).unwrap();
        assert_eq!(root.folders, ["2025", "2026"]);
        assert!(root.items.is_empty());
        let year = ask(&docs, "2026", None).unwrap();
        assert_eq!(year.folders, ["home", "trip"]);
        assert!(year.items.is_empty());
        assert!(ask(&docs, "2024", None).is_none());
        assert!(ask(&docs, "202", None).is_none());
    }

    #[test]
    fn a_saved_folder_answers_its_pages() {
        let docs = Docs::new(
            vec![folder_doc(
                "2026",
                &["trip"],
                vec![item("a", MediaKind::Image, Some("h"))],
            )],
            &PhotosCatalog,
        );
        let whole = ask(&docs, "2026", None).unwrap();
        assert_eq!(whole.folders, ["trip"]);
        assert_eq!(whole.items.len(), 1);
        let next = ask(&docs, "2026", Some("c")).unwrap();
        assert!(next.folders.is_empty() && next.items.is_empty());
    }

    #[test]
    fn media_get_finds_a_saved_item() {
        let docs = Docs::new(
            vec![folder_doc(
                "2026",
                &[],
                vec![item("a", MediaKind::Image, Some("h"))],
            )],
            &PhotosCatalog,
        );
        let get = |id: &str| {
            PhotosCatalog.answer(
                &Request::MediaGet(MediaGetParams {
                    id: ItemId(id.into()),
                }),
                &docs,
            )
        };
        match get("a") {
            Some(Ok(Response::Media(m))) => assert_eq!(m.id.0, "a"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(get("z").is_none());
        assert!(PhotosCatalog.answer(&Request::LibraryInfo, &docs).is_none());
    }

    #[test]
    fn media_of_is_thumb_and_offline_image() {
        let doc = folder_doc(
            "2026",
            &[],
            vec![
                item("a", MediaKind::Image, Some("h1")),
                item("v", MediaKind::Video, Some("h2")),
                item("n", MediaKind::Image, None),
            ],
        );
        let keys = PhotosCatalog.media_of(&doc);
        assert_eq!(keys.len(), 4);
        let of = |item: &str, preset: &str| {
            keys.iter()
                .find(|k| k.item.0 == item && k.preset == preset)
                .map(|k| (k.file_hash.as_str(), k.estimate))
        };
        assert_eq!(of("a", "thumb"), Some(("h1", 20_000)));
        assert_eq!(of("a", "offline-image"), Some(("h1", 130_000)));
        assert_eq!(of("v", "offline-image"), Some(("h2", 130_000)));
        assert_eq!(of("n", "thumb"), None);
    }
}
