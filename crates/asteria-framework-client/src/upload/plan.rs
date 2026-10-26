//! Turns dropped files and folders into upload groups (spec §5.3).

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use asteria_framework::MediaKind;
use asteria_framework::kind::classify;
use walkdir::WalkDir;

/// One file to upload: where it is, and where it goes under the server's `Inbox/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub local: PathBuf,
    /// `/`-separated, relative to `Inbox/`.
    pub inbox_path: String,
}

/// Files that belong together (a Live Photo pair, a RAW+JPEG, a sidecar) and land in one folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedGroup {
    pub files: Vec<PlannedFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanWarning {
    /// A `<stem>.xmp` next to several media files with that stem: not uploaded.
    AmbiguousSidecar {
        path: PathBuf,
    },
    Unreadable {
        path: PathBuf,
        reason: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DropPlan {
    pub groups: Vec<PlannedGroup>,
    /// Hidden, non-media and orphan sidecar files that were left out.
    pub excluded: usize,
    pub warnings: Vec<PlanWarning>,
}

/// Same place, same folder in the inbox, same name up to the first `.`.
type GroupKey = (PathBuf, String, String);

#[derive(Default)]
struct Members {
    media: Vec<PlannedFile>,
    sidecars: Vec<PlannedFile>,
}

fn is_hidden(name: &OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

fn is_xmp(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("xmp"))
}

fn name_of(path: &Path) -> Option<&str> {
    path.file_name()?.to_str()
}

fn stem_of(name: &str) -> &str {
    name.split('.').next().unwrap_or(name)
}

/// Walks what was dropped and groups it. Blocking: call it from a worker.
/// `accept` says which kinds of media to keep (sidecars always travel with their media).
pub fn plan_drop(dropped: &[PathBuf], accept: impl Fn(MediaKind) -> bool) -> DropPlan {
    let mut plan = DropPlan::default();
    let mut groups: BTreeMap<GroupKey, Members> = BTreeMap::new();

    let mut add = |plan: &mut DropPlan, local: &Path, inbox_path: String| {
        let (dir, name) = match inbox_path.rsplit_once('/') {
            Some((d, n)) => (d.to_owned(), n),
            None => (String::new(), inbox_path.as_str()),
        };
        let key = (
            local.parent().map(Path::to_path_buf).unwrap_or_default(),
            dir,
            stem_of(name).to_owned(),
        );
        let file = PlannedFile {
            local: local.to_owned(),
            inbox_path,
        };
        let members = groups.entry(key).or_default();
        if is_xmp(local) {
            members.sidecars.push(file);
        } else {
            match classify(local) {
                Ok(kind) if accept(kind) => members.media.push(file),
                Ok(_) => plan.excluded += 1,
                Err(e) => plan.warnings.push(PlanWarning::Unreadable {
                    path: local.to_owned(),
                    reason: e.to_string(),
                }),
            }
        }
    };

    for root in dropped {
        let Some(root_name) = root.file_name() else {
            plan.warnings.push(PlanWarning::Unreadable {
                path: root.clone(),
                reason: "no file name".into(),
            });
            continue;
        };
        if is_hidden(root_name) {
            plan.excluded += 1;
            continue;
        }
        let Some(root_name) = root_name.to_str() else {
            plan.warnings.push(PlanWarning::Unreadable {
                path: root.clone(),
                reason: "name is not valid UTF-8".into(),
            });
            continue;
        };
        let meta = match std::fs::symlink_metadata(root) {
            Ok(m) => m,
            Err(e) => {
                plan.warnings.push(PlanWarning::Unreadable {
                    path: root.clone(),
                    reason: e.to_string(),
                });
                continue;
            }
        };
        if meta.is_file() {
            add(&mut plan, root, root_name.to_owned());
        } else if meta.is_dir() {
            let mut walker = WalkDir::new(root)
                .follow_links(false)
                .sort_by_file_name()
                .into_iter();
            while let Some(entry) = walker.next() {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        plan.warnings.push(PlanWarning::Unreadable {
                            path: e
                                .path()
                                .map(Path::to_path_buf)
                                .unwrap_or_else(|| root.clone()),
                            reason: e.to_string(),
                        });
                        continue;
                    }
                };
                if entry.depth() > 0 && is_hidden(entry.file_name()) {
                    // A hidden directory is left out whole and counts once.
                    if entry.file_type().is_dir() {
                        walker.skip_current_dir();
                    }
                    plan.excluded += 1;
                    continue;
                }
                if !entry.file_type().is_file() {
                    continue;
                }
                let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
                let parts: Option<Vec<&str>> = rel.iter().map(OsStr::to_str).collect();
                match parts {
                    Some(parts) => {
                        let inbox_path = format!("{root_name}/{}", parts.join("/"));
                        add(&mut plan, entry.path(), inbox_path);
                    }
                    None => plan.warnings.push(PlanWarning::Unreadable {
                        path: entry.path().to_owned(),
                        reason: "name is not valid UTF-8".into(),
                    }),
                }
            }
        } else {
            // A dropped symlink or special file.
            plan.excluded += 1;
        }
    }

    for (key, members) in groups {
        let Members { media, sidecars } = members;
        let mut files = media.clone();
        for sidecar in sidecars {
            let name = name_of(&sidecar.local).unwrap_or_default();
            let base = name
                .get(..name.len().saturating_sub(".xmp".len()))
                .unwrap_or(name);
            let media_named = |n: &str| media.iter().any(|m| name_of(&m.local) == Some(n));
            if media_named(base) {
                // darktable style: `<media name>.xmp`.
                files.push(sidecar);
            } else if base == key.2 && media.len() == 1 {
                // Lightroom style: `<stem>.xmp`, named after its one media file.
                let media_name = name_of(&media[0].local).unwrap_or_default();
                let inbox_path = match sidecar.inbox_path.rsplit_once('/') {
                    Some((dir, _)) => format!("{dir}/{media_name}.xmp"),
                    None => format!("{media_name}.xmp"),
                };
                files.push(PlannedFile {
                    local: sidecar.local,
                    inbox_path,
                });
            } else if base == key.2 && media.len() > 1 {
                plan.warnings.push(PlanWarning::AmbiguousSidecar {
                    path: sidecar.local,
                });
            } else {
                plan.excluded += 1;
            }
        }
        if media.is_empty() {
            continue;
        }
        files.sort_by(|a, b| a.inbox_path.cmp(&b.inbox_path));
        plan.groups.push(PlannedGroup { files });
    }
    plan
}
