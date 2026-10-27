//! Reading M3U playlists: parsing the text and resolving each entry to a library item.
//!
//! Pure functions only; nothing here touches the filesystem.

#![cfg_attr(not(test), expect(dead_code, reason = "used by the music RPC methods"))]

use std::path::Path;

/// The most entries read from one playlist; the rest are dropped.
pub const MAX_ENTRIES: usize = 10_000;

/// One entry of a playlist, as written in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEntry {
    pub path: String,
    /// The title of the `#EXTINF` line before the entry.
    pub title_hint: Option<String>,
    /// The value of the `#ASTERIA-HASH` line before the entry.
    pub hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub entries: Vec<RawEntry>,
    /// More than [`MAX_ENTRIES`] entries were present and the rest were dropped.
    pub truncated: bool,
}

/// UTF-8 with lossy replacement; strips a BOM; CRLF and LF.
pub fn parse(bytes: &[u8]) -> Parsed {
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut entries = Vec::new();
    let mut title_hint = None;
    let mut hash = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("#EXTM3U") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            // `<seconds>,<title>`
            title_hint = rest
                .split_once(',')
                .map(|(_, title)| title.trim())
                .filter(|title| !title.is_empty())
                .map(str::to_owned);
        } else if let Some(rest) = line.strip_prefix("#ASTERIA-HASH:") {
            hash = Some(rest.trim().to_owned()).filter(|h| !h.is_empty());
        } else if line.starts_with('#') {
            continue;
        } else {
            if entries.len() == MAX_ENTRIES {
                return Parsed {
                    entries,
                    truncated: true,
                };
            }
            // The hint and the hash belong to this entry only.
            entries.push(RawEntry {
                path: line.to_owned(),
                title_hint: title_hint.take(),
                hash: hash.take(),
            });
        }
    }
    Parsed {
        entries,
        truncated: false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// The item at the written path.
    Found(String),
    /// The path is gone, but an item with the same media payload hash is elsewhere.
    Moved(String),
    Missing,
}

/// `playlist_rel`: the playlist's library-relative path. `exists(rel)`: an item at that path.
/// `by_hash(hash)`: item paths with that media-payload hash, sorted.
pub fn resolve(
    playlist_rel: &str,
    entry: &RawEntry,
    exists: impl Fn(&str) -> bool,
    by_hash: impl Fn(&str) -> Vec<String>,
) -> Resolved {
    // `resolve` has no library root, so an absolute path is not looked up by path here; the caller
    // rewrites those with `library_relative` first. They can still be found by hash.
    let absolute = entry.path.starts_with(['/', '\\']);
    if !absolute
        && let Some(rel) = library_relative(playlist_rel, &entry.path, Path::new(""))
        && exists(&rel)
    {
        return Resolved::Found(rel);
    }
    match entry.hash.as_deref() {
        Some(hash) => by_hash(hash)
            .into_iter()
            .next()
            .map_or(Resolved::Missing, Resolved::Moved),
        None => Resolved::Missing,
    }
}

/// Turns a path written in a playlist into a library-relative path, or `None` when it is a URL,
/// leaves the library, or names a hidden component (which includes `.asteria`).
///
/// Works on strings only: `\` is a separator, `.` and `..` are folded, nothing is canonicalized.
/// An absolute path is accepted only when it lies under `root`.
pub fn library_relative(playlist_rel: &str, raw: &str, root: &Path) -> Option<String> {
    let raw = raw.replace('\\', "/");
    if is_url(&raw) || has_drive(&raw) {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    let relative = if raw.starts_with('/') {
        let root = root.to_string_lossy().replace('\\', "/");
        let root = root.trim_end_matches('/');
        let rest = raw.strip_prefix(root)?;
        // `/lib` must not match `/library/x`.
        rest.strip_prefix('/')?
    } else {
        if let Some((dir, _)) = playlist_rel.rsplit_once('/') {
            fold(dir, &mut parts)?;
        }
        &raw
    };
    fold(relative, &mut parts)?;
    if parts.is_empty() || parts.iter().any(|p| p.is_empty() || p.starts_with('.')) {
        return None;
    }
    Some(parts.join("/"))
}

/// Pushes the components of `path` onto `parts`, folding `.` and `..`. `None` on an escape.
fn fold<'a>(path: &'a str, parts: &mut Vec<&'a str>) -> Option<()> {
    for part in path.split('/') {
        match part {
            "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(())
}

/// `scheme://...`
fn is_url(s: &str) -> bool {
    s.split_once("://").is_some_and(|(scheme, _)| {
        scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    })
}

/// `C:` at the start.
fn has_drive(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, hash: Option<&str>) -> RawEntry {
        RawEntry {
            path: path.to_owned(),
            title_hint: None,
            hash: hash.map(str::to_owned),
        }
    }

    #[test]
    fn parses_extinf_and_asteria_hash() {
        let text = "#EXTM3U\n#EXTINF:215,First Song\n#ASTERIA-HASH:blake3:aa\nA/1.flac\n\
                    # a comment\n\nA/2.flac\n#ASTERIA-HASH:blake3:bb\nA/3.flac\n";
        let parsed = parse(text.as_bytes());
        assert!(!parsed.truncated);
        assert_eq!(
            parsed.entries,
            vec![
                RawEntry {
                    path: "A/1.flac".into(),
                    title_hint: Some("First Song".into()),
                    hash: Some("blake3:aa".into()),
                },
                // The hint and hash of the first entry do not leak into the second.
                entry("A/2.flac", None),
                RawEntry {
                    path: "A/3.flac".into(),
                    title_hint: None,
                    hash: Some("blake3:bb".into()),
                },
            ]
        );
    }

    #[test]
    fn bom_crlf_and_latin1_bytes_are_tolerated() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"#EXTM3U\r\nA/caf\xE9.flac\r\nA/2.flac\r\n");
        let parsed = parse(&bytes);
        let paths: Vec<_> = parsed.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["A/caf\u{FFFD}.flac", "A/2.flac"]);
    }

    #[test]
    fn truncates_after_ten_thousand_entries() {
        let text: String = (0..=MAX_ENTRIES).map(|i| format!("A/{i}.flac\n")).collect();
        let parsed = parse(text.as_bytes());
        assert_eq!(parsed.entries.len(), MAX_ENTRIES);
        assert!(parsed.truncated);

        let exact: String = (0..MAX_ENTRIES).map(|i| format!("A/{i}.flac\n")).collect();
        let parsed = parse(exact.as_bytes());
        assert_eq!(parsed.entries.len(), MAX_ENTRIES);
        assert!(!parsed.truncated);
    }

    #[test]
    fn relative_paths_resolve_from_the_playlist_folder() {
        let root = Path::new("/lib");
        assert_eq!(
            library_relative("Lists/x.m3u8", "../A/1.flac", root).as_deref(),
            Some("A/1.flac")
        );
        assert_eq!(
            library_relative("Lists/x.m3u8", "./b.flac", root).as_deref(),
            Some("Lists/b.flac")
        );
        assert_eq!(
            library_relative("x.m3u8", "A/1.flac", root).as_deref(),
            Some("A/1.flac")
        );
    }

    #[test]
    fn backslashes_are_separators() {
        let root = Path::new("/lib");
        assert_eq!(
            library_relative("Lists/x.m3u8", "..\\A\\1.flac", root).as_deref(),
            Some("A/1.flac")
        );
    }

    #[test]
    fn escapes_and_hidden_paths_are_rejected() {
        let root = Path::new("/lib");
        for raw in [
            "../../etc/passwd",
            "/etc/passwd",
            "/library/a.flac",
            ".asteria/library.toml",
            ".hidden/a.flac",
            "A/.hidden.flac",
            "C:\\Music\\a.flac",
            "C:a.flac",
            "http://x/a.mp3",
            "A//1.flac",
            "",
        ] {
            assert_eq!(library_relative("Lists/x.m3u8", raw, root), None, "{raw}");
        }
        // One level up from the root-level playlist is already outside.
        assert_eq!(library_relative("x.m3u8", "../a.flac", root), None);
        assert_eq!(library_relative("x.m3u8", "/lib/../etc/a", root), None);
    }

    #[test]
    fn absolute_path_inside_root_is_accepted() {
        let root = Path::new("/lib");
        assert_eq!(
            library_relative("Lists/x.m3u8", "/lib/A/1.flac", root).as_deref(),
            Some("A/1.flac")
        );
        assert_eq!(
            library_relative("Lists/x.m3u8", "/lib/A/../B/1.flac", root).as_deref(),
            Some("B/1.flac")
        );
        assert_eq!(library_relative("x.m3u8", "/lib/.asteria/x", root), None);
    }

    #[test]
    fn missing_path_with_hash_is_moved() {
        let e = entry("A/gone.flac", Some("blake3:aa"));
        let got = resolve(
            "x.m3u8",
            &e,
            |_| false,
            |h| {
                assert_eq!(h, "blake3:aa");
                vec!["B/1.flac".into(), "C/1.flac".into()]
            },
        );
        assert_eq!(got, Resolved::Moved("B/1.flac".into()));
        // A hash with no match is missing; an existing path wins over the hash.
        assert_eq!(
            resolve("x.m3u8", &e, |_| false, |_| vec![]),
            Resolved::Missing
        );
        assert_eq!(
            resolve(
                "x.m3u8",
                &e,
                |p| p == "A/gone.flac",
                |_| vec!["B/1.flac".into()]
            ),
            Resolved::Found("A/gone.flac".into())
        );
    }

    #[test]
    fn missing_path_without_hash_is_missing() {
        let e = entry("A/gone.flac", None);
        assert_eq!(
            resolve("x.m3u8", &e, |_| false, |_| vec!["B/1.flac".into()]),
            Resolved::Missing
        );
        let url = entry("http://x/a.mp3", Some("blake3:aa"));
        assert_eq!(
            resolve("x.m3u8", &url, |_| true, |_| vec![]),
            Resolved::Missing
        );
    }
}
