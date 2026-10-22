use std::collections::btree_map::Entry;
use std::path::Path;

use chrono::{DateTime, Utc};

use crate::hash::{ContentHash, HashScope};
use crate::{Error, Result, UserId};

use super::model::{RatingEntry, Sidecar, XmpDoc};
use super::ns;
use super::xml::{self, Element};

type R<T> = std::result::Result<T, String>;

impl Sidecar {
    /// Parse sidecar bytes.
    pub fn parse(bytes: &[u8]) -> Result<Sidecar> {
        parse_inner(bytes).map_err(|reason| Error::MalformedSidecar { path: None, reason })
    }

    /// Read a sidecar file; `Ok(None)` if it does not exist.
    pub fn read(path: &Path) -> Result<Option<Sidecar>> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        parse_inner(&bytes)
            .map(Some)
            .map_err(|reason| Error::MalformedSidecar {
                path: Some(path.to_path_buf()),
                reason,
            })
    }
}

fn parse_inner(bytes: &[u8]) -> R<Sidecar> {
    let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let xpacket = body.trim_ascii_start().starts_with(b"<?xpacket");
    let root = xml::parse(bytes)?;
    let mut sc = extract(&root)?;
    sc.doc = Some(XmpDoc { root, xpacket });
    Ok(sc)
}

fn rdf_root(root: &Element) -> R<&Element> {
    if root.is(ns::RDF, "RDF") {
        return Ok(root);
    }
    root.child(ns::RDF, "RDF")
        .ok_or_else(|| "no rdf:RDF element".to_string())
}

/// Value of a simple property, from an attribute or a child element of the first
/// `rdf:Description` that has it.
fn simple(descs: &[&Element], uri: &str, local: &str) -> Option<String> {
    descs.iter().find_map(|d| {
        d.attr(uri, local)
            .map(|v| v.trim().to_string())
            .or_else(|| d.child(uri, local).map(|e| e.text()))
    })
}

fn container(prop: &Element) -> Option<&Element> {
    prop.child_elements()
        .find(|e| e.is(ns::RDF, "Bag") || e.is(ns::RDF, "Seq") || e.is(ns::RDF, "Alt"))
}

fn items(prop: &Element) -> impl Iterator<Item = &Element> {
    container(prop)
        .into_iter()
        .flat_map(|c| c.child_elements().filter(|e| e.is(ns::RDF, "li")))
}

fn parse_rating(s: &str) -> R<i8> {
    let bad = || format!("invalid rating {s:?}");
    let v: f64 = s.trim().parse().map_err(|_| bad())?;
    if v.fract() != 0.0 {
        return Err(bad());
    }
    if (-1.0..=5.0).contains(&v) {
        Ok(v as i8)
    } else {
        Err(format!("rating {s:?} out of range -1..=5"))
    }
}

/// XMP dates: `YYYY[-MM[-DD[Thh:mm[:ss[.s+]][TZD]]]]`. A missing TZD means UTC and missing
/// fields take their minimum.
fn parse_time(s: &str) -> R<DateTime<Utc>> {
    let bad = || format!("invalid timestamp {s:?}");
    let s = s.trim();
    let (date, time) = match s.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() > 3 {
        return Err(bad());
    }
    let (y, mo, d) = (
        parts[0],
        parts.get(1).copied().unwrap_or("01"),
        parts.get(2).copied().unwrap_or("01"),
    );
    let clock = match time {
        None => "00:00:00Z".to_string(),
        Some(t) => {
            let (body, tz) = if let Some(b) = t.strip_suffix('Z') {
                (b, "Z")
            } else if let Some(i) = t.rfind(['+', '-']) {
                t.split_at(i)
            } else {
                (t, "Z")
            };
            let f: Vec<&str> = body.split(':').collect();
            if !(2..=3).contains(&f.len()) {
                return Err(bad());
            }
            format!(
                "{}:{}:{}{tz}",
                f[0],
                f[1],
                f.get(2).copied().unwrap_or("00")
            )
        }
    };
    if y.len() != 4 || mo.len() != 2 || d.len() != 2 {
        return Err(bad());
    }
    DateTime::parse_from_rfc3339(&format!("{y}-{mo}-{d}T{clock}"))
        .map(|t| t.with_timezone(&Utc))
        .map_err(|_| bad())
}

/// A struct field of an `rdf:li`: attribute, child element, or inside a nested `rdf:Description`.
fn field(li: &Element, local: &str) -> Option<String> {
    simple(&[li], ns::ASTERIA, local).or_else(|| {
        let nested: Vec<&Element> = li
            .child_elements()
            .filter(|e| e.is(ns::RDF, "Description"))
            .collect();
        simple(&nested, ns::ASTERIA, local)
    })
}

/// Bag/Seq/Alt items, or a bare literal as a single item.
fn values(prop: &Element) -> Vec<String> {
    if container(prop).is_some() {
        items(prop).map(Element::text).collect()
    } else {
        let t = prop.text();
        if t.is_empty() { vec![] } else { vec![t] }
    }
}

pub(super) fn extract(root: &Element) -> R<Sidecar> {
    let rdf = rdf_root(root)?;
    let descs: Vec<&Element> = rdf
        .child_elements()
        .filter(|e| e.is(ns::RDF, "Description"))
        .collect();
    let mut sc = Sidecar::new();

    let modified = simple(&descs, ns::ASTERIA, "RatingModified")
        .map(|s| parse_time(&s))
        .transpose()?;
    if let Some(r) = simple(&descs, ns::XMP, "Rating") {
        sc.rating = Some(RatingEntry {
            value: parse_rating(&r)?,
            modified,
        });
    }
    sc.label = simple(&descs, ns::XMP, "Label");

    let hash = simple(&descs, ns::ASTERIA, "ContentHash");
    let scope = simple(&descs, ns::ASTERIA, "HashScope");
    sc.content_hash = match (hash, scope) {
        (Some(h), Some(s)) => Some((
            h.parse::<ContentHash>().map_err(|e| e.to_string())?,
            s.parse::<HashScope>().map_err(|e| e.to_string())?,
        )),
        (None, None) => None,
        // Half a hash is useless but recomputable, so treat it as absent.
        _ => None,
    };

    for d in &descs {
        for prop in d.child_elements() {
            if prop.is(ns::DC, "subject") {
                sc.subjects.extend(values(prop));
            } else if prop.is(ns::LR, "hierarchicalSubject") {
                sc.hierarchical_subjects.extend(values(prop));
            } else if prop.is(ns::DC, "description") && sc.description.is_none() {
                if container(prop).is_some() {
                    let li: Vec<&Element> = items(prop).collect();
                    let pick = li
                        .iter()
                        .find(|e| e.attr(ns::XML, "lang") == Some("x-default"))
                        .or_else(|| li.first());
                    sc.description = pick.map(|e| e.text());
                } else {
                    sc.description = values(prop).into_iter().next();
                }
            } else if prop.is(ns::ASTERIA, "UserRatings") {
                for li in items(prop) {
                    let user = field(li, "User").ok_or("UserRatings item without asteria:User")?;
                    let user: UserId = user
                        .parse()
                        .map_err(|_| format!("invalid user id {user:?}"))?;
                    let value = parse_rating(
                        &field(li, "Rating").ok_or("UserRatings item without asteria:Rating")?,
                    )?;
                    let modified = field(li, "Modified").map(|m| parse_time(&m)).transpose()?;
                    let entry = RatingEntry { value, modified };
                    // Latest modification wins; an entry with no time loses to one with a time.
                    match sc.user_ratings.entry(user) {
                        Entry::Vacant(v) => {
                            v.insert(entry);
                        }
                        Entry::Occupied(mut o) => {
                            if entry.modified > o.get().modified {
                                o.insert(entry);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(sc)
}
