//! Turns dropped files and folders into upload groups (spec §5.3).

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use asteria_framework::MediaKind;
use asteria_framework::kind::classify;
use asteria_framework::upload_path::validate_inbox_path;
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

/// Same place, same folder in the inbox, same name without its extension.
type GroupKey = (PathBuf, String, String);

/// A sidecar waiting to be matched with its media.
struct Loose {
    parent: PathBuf,
    /// Its folder in the inbox.
    dir: String,
    local: PathBuf,
    /// Its name without `.xmp`: a media name (darktable) or one without its extension (Lightroom).
    rest: String,
}

fn is_hidden(name: &OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

fn is_xmp(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("xmp"))
}

/// `name` without its last extension: `a.b.HEIC` is `a.b`, `README` stays `README`.
fn without_extension(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    }
}

fn name_of(path: &Path) -> Option<&str> {
    path.file_name()?.to_str()
}

/// `dir/name`, or `name` in the inbox itself.
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// Walks what was dropped and groups it. Blocking: call it from a worker.
/// `accept` says which kinds of media to keep (sidecars always travel with their media).
pub fn plan_drop(dropped: &[PathBuf], accept: impl Fn(MediaKind) -> bool) -> DropPlan {
    let mut plan = DropPlan::default();
    // The media of each group; sidecars join them once everything is walked.
    let mut groups: BTreeMap<GroupKey, Vec<PlannedFile>> = BTreeMap::new();
    let mut loose: Vec<Loose> = Vec::new();

    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut add = |plan: &mut DropPlan, local: &Path, inbox_path: String| {
        // The same file reached twice (dropped twice, or inside a dropped folder) is planned once.
        let identity = std::fs::canonicalize(local).unwrap_or_else(|_| local.to_owned());
        if !seen.insert(identity) {
            return;
        }
        if let Err(reason) = validate_inbox_path(&inbox_path) {
            plan.warnings.push(PlanWarning::Unreadable {
                path: local.to_owned(),
                reason,
            });
            return;
        }
        let (dir, name) = match inbox_path.rsplit_once('/') {
            Some((d, n)) => (d.to_owned(), n),
            None => (String::new(), inbox_path.as_str()),
        };
        let parent = local.parent().map(Path::to_path_buf).unwrap_or_default();
        if is_xmp(local) {
            let rest = name[..name.len() - ".xmp".len()].to_owned();
            loose.push(Loose {
                parent,
                dir,
                local: local.to_owned(),
                rest,
            });
        } else {
            let key = (parent, dir, without_extension(name).to_owned());
            let file = PlannedFile {
                local: local.to_owned(),
                inbox_path,
            };
            match classify(local) {
                Ok(kind) if accept(kind) => groups.entry(key).or_default().push(file),
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
                if entry.file_type().is_symlink() {
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

    // Every group starts with its media; sidecars are added below, sent as `<media name>.xmp`
    // (lowercase, the only spelling the server reads).
    let mut files: BTreeMap<GroupKey, Vec<PlannedFile>> = groups.clone();
    let mut matched = vec![false; loose.len()];
    // darktable style first: `<media name>.xmp`, which wins over a Lightroom one.
    for (i, l) in loose.iter().enumerate() {
        let key = (
            l.parent.clone(),
            l.dir.clone(),
            without_extension(&l.rest).to_owned(),
        );
        let named = |m: &PlannedFile| name_of(&m.local) == Some(l.rest.as_str());
        if groups
            .get(&key)
            .is_some_and(|media| media.iter().any(named))
        {
            matched[i] = true;
            add_sidecar(
                &mut files,
                &mut plan,
                key,
                l,
                join(&l.dir, &format!("{}.xmp", l.rest)),
            );
        }
    }
    // Lightroom style: `<name without extension>.xmp`, for its one media file.
    for (i, l) in loose.iter().enumerate() {
        if matched[i] {
            continue;
        }
        let key = (l.parent.clone(), l.dir.clone(), l.rest.clone());
        match groups.get(&key).map(Vec::as_slice) {
            Some([only]) => {
                let media_name = name_of(&only.local).unwrap_or_default();
                let inbox_path = join(&l.dir, &format!("{media_name}.xmp"));
                add_sidecar(&mut files, &mut plan, key, l, inbox_path);
            }
            Some([_, _, ..]) => plan.warnings.push(PlanWarning::AmbiguousSidecar {
                path: l.local.clone(),
            }),
            _ => plan.excluded += 1,
        }
    }
    for (_, mut group) in files {
        group.sort_by(|a, b| a.inbox_path.cmp(&b.inbox_path));
        plan.groups.push(PlannedGroup { files: group });
    }
    plan
}

/// Adds sidecar `l` to group `key` as `inbox_path`, unless another sidecar has that name
/// already (then it is left out).
fn add_sidecar(
    files: &mut BTreeMap<GroupKey, Vec<PlannedFile>>,
    plan: &mut DropPlan,
    key: GroupKey,
    l: &Loose,
    inbox_path: String,
) {
    let group = files.entry(key).or_default();
    if group.iter().any(|f| f.inbox_path == inbox_path) {
        plan.excluded += 1;
    } else {
        group.push(PlannedFile {
            local: l.local.clone(),
            inbox_path,
        });
    }
}
