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
    let xpacket = bytes.starts_with(b"<?xpacket")
        || bytes
            .strip_prefix(b"\xEF\xBB\xBF")
            .is_some_and(|b| b.starts_with(b"<?xpacket"));
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
    let v: i8 = s
        .trim()
        .parse()
        .map_err(|_| format!("invalid rating {s:?}"))?;
    if (-1..=5).contains(&v) {
        Ok(v)
    } else {
        Err(format!("rating {v} out of range -1..=5"))
    }
}

fn parse_time(s: &str) -> R<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s.trim())
        .map(|t| t.with_timezone(&Utc))
        .map_err(|_| format!("invalid timestamp {s:?}"))
}

fn extract(root: &Element) -> R<Sidecar> {
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
        _ => return Err("asteria:ContentHash and asteria:HashScope must appear together".into()),
    };

    for d in &descs {
        for prop in d.child_elements() {
            if prop.is(ns::DC, "subject") {
                sc.subjects.extend(items(prop).map(Element::text));
            } else if prop.is(ns::LR, "hierarchicalSubject") {
                sc.hierarchical_subjects
                    .extend(items(prop).map(Element::text));
            } else if prop.is(ns::DC, "description") && sc.description.is_none() {
                let li: Vec<&Element> = items(prop).collect();
                let pick = li
                    .iter()
                    .find(|e| e.attr(ns::XML, "lang") == Some("x-default"))
                    .or_else(|| li.first());
                sc.description = pick.map(|e| e.text());
            } else if prop.is(ns::ASTERIA, "UserRatings") {
                for li in items(prop) {
                    let user = li
                        .attr(ns::ASTERIA, "User")
                        .ok_or("UserRatings item without asteria:User")?;
                    let user: UserId = user
                        .trim()
                        .parse()
                        .map_err(|_| format!("invalid user id {user:?}"))?;
                    let value = parse_rating(
                        li.attr(ns::ASTERIA, "Rating")
                            .ok_or("UserRatings item without asteria:Rating")?,
                    )?;
                    let modified = li
                        .attr(ns::ASTERIA, "Modified")
                        .map(parse_time)
                        .transpose()?;
                    sc.user_ratings
                        .entry(user)
                        .or_insert(RatingEntry { value, modified });
                }
            }
        }
    }
    Ok(sc)
}
