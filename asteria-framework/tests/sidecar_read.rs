use std::path::PathBuf;

use asteria_framework::hash::HashScope;
use asteria_framework::{Error, Sidecar, UserId};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/xmp")
        .join(name)
}

fn load(name: &str) -> Sidecar {
    Sidecar::read(&fixture(name)).unwrap().unwrap()
}

#[test]
fn reads_attribute_form_rating() {
    let sc = load("darktable_attr.xmp");
    let r = sc.rating.clone().unwrap();
    assert_eq!(r.value, 3);
    assert_eq!(r.modified, None);
    assert!(!sc.has_xpacket_wrapper());
}

#[test]
fn reads_element_form_rating() {
    let sc = load("element_form.xmp");
    assert_eq!(sc.rating.unwrap().value, 4);
    assert_eq!(sc.label.as_deref(), Some("Red"));
}

#[test]
fn reads_xpacket_wrapped() {
    let sc = load("xpacket_wrapped.xmp");
    assert!(sc.has_xpacket_wrapper());
    assert_eq!(sc.rating.unwrap().value, 2);
}

#[test]
fn reads_full_asteria_sidecar() {
    let sc = load("asteria_full.xmp");
    let r = sc.rating.unwrap();
    assert_eq!(r.value, 4);
    assert_eq!(
        r.modified.unwrap().to_rfc3339(),
        "2026-10-22T09:00:00+00:00"
    );
    assert_eq!(sc.user_ratings.len(), 1);
    let user: UserId = "123abcd".parse().unwrap();
    let u = &sc.user_ratings[&user];
    assert_eq!(u.value, 5);
    assert_eq!(
        u.modified.unwrap().to_rfc3339(),
        "2026-10-21T20:00:00+00:00"
    );
    let (hash, scope) = sc.content_hash.unwrap();
    assert_eq!(scope, HashScope::MediaPayload1);
    assert_eq!(hash.to_string(), format!("blake3:{}", "ab".repeat(32)));
}

#[test]
fn merges_descriptions_and_matches_by_uri_not_prefix() {
    let sc = load("multi_description.xmp");
    assert_eq!(sc.rating.unwrap().value, -1);
    let subjects: Vec<_> = sc.subjects.iter().map(String::as_str).collect();
    assert_eq!(subjects, ["beach", "cat", "sunset"]);
    assert_eq!(sc.description.as_deref(), Some("A cat"));
    assert!(sc.hierarchical_subjects.contains("animal|cat"));
}

#[test]
fn description_falls_back_to_first_alt_item() {
    let xml = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:dc="http://purl.org/dc/elements/1.1/">
      <rdf:Description><dc:description><rdf:Alt><rdf:li xml:lang="fr">Un chat</rdf:li></rdf:Alt></dc:description></rdf:Description></rdf:RDF>"#;
    let sc = Sidecar::parse(xml.as_bytes()).unwrap();
    assert_eq!(sc.description.as_deref(), Some("Un chat"));
}

#[test]
fn foreign_namespace_rating_is_ignored() {
    let xml = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:xmp="urn:other">
      <rdf:Description xmp:Rating="9"/></rdf:RDF>"#;
    assert!(Sidecar::parse(xml.as_bytes()).unwrap().rating.is_none());
}

#[test]
fn malformed_xml_is_error() {
    let err = Sidecar::read(&fixture("malformed.xmp")).unwrap_err();
    assert!(matches!(err, Error::MalformedSidecar { path: Some(_), .. }));
}

#[test]
fn out_of_range_rating_is_error() {
    let xml = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:xmp="http://ns.adobe.com/xap/1.0/">
      <rdf:Description xmp:Rating="9"/></rdf:RDF>"#;
    assert!(matches!(
        Sidecar::parse(xml.as_bytes()),
        Err(Error::MalformedSidecar { .. })
    ));
}

#[test]
fn bad_hash_is_error() {
    let xml = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:asteria="https://sapphire-turtle.github.io/ns/asteria/1.0/">
      <rdf:Description asteria:ContentHash="blake3:zz" asteria:HashScope="file/1"/></rdf:RDF>"#;
    assert!(matches!(
        Sidecar::parse(xml.as_bytes()),
        Err(Error::MalformedSidecar { .. })
    ));
}

#[test]
fn missing_file_reads_none() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        Sidecar::read(&dir.path().join("nope.jpg.xmp"))
            .unwrap()
            .is_none()
    );
}
