use std::path::PathBuf;

use asteria_framework::hash::{ContentHash, HashScope};
use asteria_framework::{RatingEntry, Sidecar, UserId};
use chrono::{TimeZone, Utc};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/xmp")
        .join(name)
}

fn load(name: &str) -> Sidecar {
    Sidecar::read(&fixture(name)).unwrap().unwrap()
}

fn text(sc: &Sidecar) -> String {
    String::from_utf8(sc.to_bytes()).unwrap()
}

fn reparse(sc: &Sidecar) -> Sidecar {
    Sidecar::parse(&sc.to_bytes()).unwrap()
}

#[test]
fn roundtrip_preserves_unknown_namespaces() {
    let mut sc = load("darktable_attr.xmp");
    sc.rating = Some(RatingEntry {
        value: 5,
        modified: None,
    });
    let out = text(&sc);
    assert_eq!(reparse(&sc).rating.unwrap().value, 5);
    for needle in [
        "xmlns:darktable=\"http://darktable.sf.net/\"",
        "xmlns:xmpMM=\"http://ns.adobe.com/xap/1.0/mm/\"",
        "darktable:xmp_version=\"5\"",
        "darktable:iop_order_version=\"2\"",
        "xmpMM:DerivedFrom=\"IMG_0001.CR2\"",
        "darktable:operation=\"exposure\"",
        "darktable:params=\"00000000\"",
        "<darktable:history>",
    ] {
        assert!(out.contains(needle), "missing {needle} in {out}");
    }
    assert_eq!(out.matches("Rating=").count(), 1);
}

#[test]
fn element_form_rating_is_rewritten_once() {
    let mut sc = load("element_form.xmp");
    sc.rating.as_mut().unwrap().value = 2;
    let out = text(&sc);
    assert_eq!(out.matches("xmp:Rating").count(), 1, "{out}");
    assert!(out.contains("xmp:Rating=\"2\""));
    assert_eq!(reparse(&sc).rating.unwrap().value, 2);
    assert_eq!(reparse(&sc).label.as_deref(), Some("Red"));
}

#[test]
fn xpacket_wrapper_is_preserved() {
    let sc = load("xpacket_wrapped.xmp");
    let out = text(&sc);
    assert!(out.starts_with("<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>"));
    assert!(out.trim_end().ends_with("<?xpacket end=\"w\"?>"));
    assert!(reparse(&sc).has_xpacket_wrapper());
    assert_eq!(reparse(&sc).rating.unwrap().value, 2);
}

#[test]
fn unwrapped_source_stays_unwrapped() {
    let out = text(&load("darktable_attr.xmp"));
    assert!(out.starts_with("<?xml"));
    assert!(!out.contains("xpacket"));
}

#[test]
fn new_sidecar_roundtrips_all_fields() {
    let mut sc = Sidecar::new();
    sc.rating = Some(RatingEntry {
        value: -1,
        modified: Some(Utc.with_ymd_and_hms(2026, 10, 22, 9, 0, 0).unwrap()),
    });
    let u1: UserId = "123abcd".parse().unwrap();
    sc.user_ratings.insert(
        u1,
        RatingEntry {
            value: 5,
            modified: Some(Utc.with_ymd_and_hms(2026, 10, 21, 20, 0, 0).unwrap()),
        },
    );
    sc.subjects
        .extend(["cat".to_string(), "京都 & <b>".to_string()]);
    sc.hierarchical_subjects.insert("animal|cat".into());
    sc.description = Some("A \"cat\" & more".into());
    sc.label = Some("Red".into());
    let hash: ContentHash = format!("blake3:{}", "ab".repeat(32)).parse().unwrap();
    sc.content_hash = Some((hash, HashScope::MediaPayload1));

    let out = text(&sc);
    assert!(out.contains("2026-10-22T09:00:00Z"), "{out}");
    assert!(out.contains("xml:lang=\"x-default\""));
    let back = reparse(&sc);
    assert_eq!(back.rating, sc.rating);
    assert_eq!(back.user_ratings, sc.user_ratings);
    assert_eq!(back.subjects, sc.subjects);
    assert_eq!(back.hierarchical_subjects, sc.hierarchical_subjects);
    assert_eq!(back.description, sc.description);
    assert_eq!(back.label, sc.label);
    assert_eq!(back.content_hash, sc.content_hash);
    assert!(!back.has_xpacket_wrapper());
}

#[test]
fn empty_values_are_not_written() {
    let mut sc = Sidecar::new();
    sc.label = Some(String::new());
    sc.description = Some(String::new());
    let out = text(&sc);
    assert!(
        !out.contains("Label") && !out.contains("description"),
        "{out}"
    );
    assert!(!out.contains("subject") && !out.contains("UserRatings"));
    assert!(Sidecar::parse(out.as_bytes()).is_ok());
}

#[test]
fn multi_description_managed_props_are_consolidated_unknown_kept() {
    let mut sc = load("multi_description.xmp");
    sc.rating.as_mut().unwrap().value = 3;
    sc.subjects.insert("dog".into());
    let out = text(&sc);
    assert_eq!(out.matches("Rating").count(), 1, "{out}");
    assert_eq!(out.matches("<rdf:Description").count(), 2);
    let back = reparse(&sc);
    assert_eq!(back.rating.unwrap().value, 3);
    assert_eq!(back.subjects, sc.subjects);
    assert_eq!(back.description.as_deref(), Some("A cat"));
    assert_eq!(back.hierarchical_subjects, sc.hierarchical_subjects);
    // the German translation is not asteria's to drop
    assert!(out.contains("Ein Kater"), "{out}");
}

#[test]
fn clearing_values_removes_them_from_file() {
    let mut sc = load("asteria_full.xmp");
    sc.rating = None;
    sc.user_ratings.clear();
    sc.content_hash = None;
    let out = text(&sc);
    for gone in ["Rating", "ContentHash", "HashScope", "UserRatings"] {
        assert!(!out.contains(gone), "{gone} still in {out}");
    }
    assert!(out.contains("xmlns:asteria="));
}

#[test]
fn foreign_content_is_kept_verbatim() {
    let src = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><!--keep--><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:ex="urn:ex" ex:a="1 &amp; 2"><ex:b><![CDATA[<raw>]]></ex:b><?pi data?></rdf:Description></rdf:RDF></x:xmpmeta>"#;
    let mut sc = Sidecar::parse(src.as_bytes()).unwrap();
    sc.label = Some("Blue".into());
    let out = text(&sc);
    for needle in [
        "<!--keep-->",
        "ex:a=\"1 &amp; 2\"",
        "<![CDATA[<raw>]]>",
        "<?pi data?>",
        "xmlns:ex=\"urn:ex\"",
    ] {
        assert!(out.contains(needle), "missing {needle} in {out}");
    }
    assert_eq!(reparse(&sc).label.as_deref(), Some("Blue"));
}

#[test]
fn new_property_reuses_existing_nondefault_prefix() {
    let mut sc = load("multi_description.xmp"); // xap prefix for xmp, dublin for dc
    sc.label = Some("Green".into());
    let out = text(&sc);
    assert!(out.contains("xap:Label=\"Green\""), "{out}");
}

#[test]
fn write_is_atomic_file_write() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.jpg.xmp");
    let mut sc = Sidecar::new();
    sc.label = Some("Red".into());
    sc.write(&p).unwrap();
    assert_eq!(
        Sidecar::read(&p).unwrap().unwrap().label.as_deref(),
        Some("Red")
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn rewriting_is_stable() {
    for name in [
        "multi_description.xmp",
        "asteria_full.xmp",
        "element_form.xmp",
    ] {
        let once = load(name).to_bytes();
        let twice = Sidecar::parse(&once).unwrap().to_bytes();
        assert_eq!(
            String::from_utf8(once).unwrap(),
            String::from_utf8(twice).unwrap(),
            "{name}"
        );
    }
}

#[test]
fn canonical_prefix_bound_to_other_uri_is_not_reused() {
    let src = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/" xmlns:dc="urn:other"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about=""/></rdf:RDF></x:xmpmeta>"#;
    let mut sc = Sidecar::parse(src.as_bytes()).unwrap();
    sc.subjects.insert("cat".into());
    let back = reparse(&sc);
    assert_eq!(back.subjects, sc.subjects);
}

const DE_ONLY: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="1"><dc:description><rdf:Alt><rdf:li xmlns:q="urn:q" xml:lang="de" q:extra="1"> Ein  Kater </rdf:li></rdf:Alt></dc:description></rdf:Description></rdf:RDF></x:xmpmeta>"#;

const DESC_PART: &str = r#"<dc:description><rdf:Alt><rdf:li xmlns:q="urn:q" xml:lang="de" q:extra="1"> Ein  Kater </rdf:li></rdf:Alt></dc:description>"#;

#[test]
fn unchanged_description_is_byte_identical_after_rating_change() {
    let mut sc = Sidecar::parse(DE_ONLY.as_bytes()).unwrap();
    sc.rating.as_mut().unwrap().value = 4;
    let out = text(&sc);
    assert!(out.contains(DESC_PART), "{out}");
    assert!(!out.contains("x-default"));
    assert!(out.contains("xmp:Rating=\"4\""));
}

#[test]
fn changed_description_keeps_other_language_items_whole() {
    let mut sc = Sidecar::parse(DE_ONLY.as_bytes()).unwrap();
    sc.description = Some("A cat".into());
    let out = text(&sc);
    assert!(
        out.contains(r#"<rdf:li xmlns:q="urn:q" xml:lang="de" q:extra="1"> Ein  Kater </rdf:li>"#),
        "{out}"
    );
    assert_eq!(out.matches("x-default").count(), 1);
    assert_eq!(reparse(&sc).description.as_deref(), Some("A cat"));
}

#[test]
fn unchanged_properties_stay_in_their_original_description() {
    let mut sc = load("multi_description.xmp");
    sc.label = Some("Red".into());
    let out = text(&sc);
    // subjects were not touched, so both original (differently prefixed) blocks are intact
    assert!(out.contains("<dublin:subject xmlns:dublin"), "{out}");
    assert_eq!(reparse(&sc).subjects, sc.subjects);
}

fn two_desc(second_attrs: &str, li: &str) -> String {
    format!(
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="1"/><rdf:Description rdf:about="" xmlns:dc="http://purl.org/dc/elements/1.1/" {second_attrs}><dc:description><rdf:Alt>{li}</rdf:Alt></dc:description></rdf:Description></rdf:RDF></x:xmpmeta>"#
    )
}

fn moved(src: &str) -> (String, Sidecar) {
    let mut sc = Sidecar::parse(src.as_bytes()).unwrap();
    sc.description = Some("A cat".into());
    let out = text(&sc);
    let back = Sidecar::parse(out.as_bytes()).unwrap_or_else(|e| panic!("{e}\n{out}"));
    assert_eq!(back.description.as_deref(), Some("A cat"));
    (out, sc)
}

fn find(out: &str, needle: &str) {
    assert!(out.contains(needle), "missing {needle} in {out}");
}

#[test]
fn moved_li_keeps_attribute_prefix_declared_elsewhere() {
    let src = two_desc(
        r#"xmlns:q="urn:q""#,
        r#"<rdf:li xml:lang="de" q:extra="1">Kater</rdf:li>"#,
    );
    let (out, _) = moved(&src);
    find(&out, "q:extra=\"1\"");
    find(&out, "xmlns:q=\"urn:q\"");
}

#[test]
fn moved_li_keeps_non_canonical_rdf_prefix() {
    let src = two_desc(
        r#"xmlns:R="http://www.w3.org/1999/02/22-rdf-syntax-ns#""#,
        r#"<R:li xml:lang="de">Kater</R:li>"#,
    );
    let (out, _) = moved(&src);
    find(&out, "<R:li ");
    find(&out, "Kater");
}

#[test]
fn moved_li_keeps_child_element_prefix() {
    let src = two_desc(
        r#"xmlns:q="urn:q""#,
        r#"<rdf:li xml:lang="de">Kater<q:note>n</q:note></rdf:li>"#,
    );
    let (out, _) = moved(&src);
    find(&out, "<q:note>n</q:note>");
    find(&out, "xmlns:q=\"urn:q\"");
}

#[test]
fn moved_li_keeps_default_namespace_of_unprefixed_children() {
    let src = two_desc(
        r#"xmlns="urn:d""#,
        r#"<rdf:li xml:lang="de">Kater<note/></rdf:li>"#,
    );
    let (out, _) = moved(&src);
    find(&out, "<note/>");
    find(&out, "xmlns=\"urn:d\"");
    // the namespace of <note/> is intact: re-read and re-write keeps the declaration
    let again = Sidecar::parse(out.as_bytes()).unwrap();
    find(&text(&again), "xmlns=\"urn:d\"");
}
