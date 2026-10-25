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

const HEAD: &str = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:lr="http://ns.adobe.com/lightroom/1.0/" xmlns:asteria="https://sapphire-turtle.github.io/ns/asteria/1.0/">"#;

fn parse_desc(inner: &str) -> Result<Sidecar, Error> {
    Sidecar::parse(format!("{HEAD}<rdf:Description {inner}</rdf:Description></rdf:RDF>").as_bytes())
}

fn is_malformed<T>(r: Result<T, Error>) -> bool {
    matches!(r, Err(Error::MalformedSidecar { .. }))
}

#[test]
fn two_root_elements_is_error() {
    let one = std::fs::read(fixture("element_form.xmp")).unwrap();
    let mut both = one.clone();
    both.extend_from_slice(&one);
    assert!(is_malformed(Sidecar::parse(&both)));
}

#[test]
fn user_ratings_parse_type_resource_form() {
    let sc = load("usrratings_resource.xmp");
    let user: UserId = "123abcd".parse().unwrap();
    assert_eq!(sc.user_ratings[&user].value, 5);
    assert!(sc.user_ratings[&user].modified.is_some());
}

#[test]
fn user_ratings_nested_description_forms() {
    let sc = parse_desc(
        r#"><asteria:UserRatings><rdf:Bag>
        <rdf:li><rdf:Description asteria:User="123abcd" asteria:Rating="2"/></rdf:li>
        <rdf:li><rdf:Description><asteria:User>abcdefg</asteria:User><asteria:Rating>4</asteria:Rating></rdf:Description></rdf:li>
        </rdf:Bag></asteria:UserRatings>"#,
    )
    .unwrap();
    assert_eq!(sc.user_ratings.len(), 2);
    let a: UserId = "123abcd".parse().unwrap();
    assert_eq!(sc.user_ratings[&a].value, 2);
}

#[test]
fn user_ratings_item_missing_fields_is_error() {
    let r = parse_desc(
        r#"><asteria:UserRatings><rdf:Bag><rdf:li asteria:User="123abcd"/></rdf:Bag></asteria:UserRatings>"#,
    );
    assert!(is_malformed(r));
    let r = parse_desc(
        r#"><asteria:UserRatings><rdf:Bag><rdf:li asteria:Rating="3"/></rdf:Bag></asteria:UserRatings>"#,
    );
    assert!(is_malformed(r));
}

#[test]
fn integral_real_rating_accepted_fractional_rejected() {
    assert_eq!(
        parse_desc(r#"xmp:Rating="3.0">"#)
            .unwrap()
            .rating
            .unwrap()
            .value,
        3
    );
    assert!(is_malformed(parse_desc(r#"xmp:Rating="2.5">"#)));
}

#[test]
fn xpacket_after_leading_whitespace() {
    let mut b = b"\n  ".to_vec();
    b.extend(std::fs::read(fixture("xpacket_wrapped.xmp")).unwrap());
    assert!(Sidecar::parse(&b).unwrap().has_xpacket_wrapper());
}

#[test]
fn duplicate_user_latest_modified_wins() {
    let sc = parse_desc(
        r#"><asteria:UserRatings><rdf:Bag>
        <rdf:li asteria:User="123abcd" asteria:Rating="1"/>
        <rdf:li asteria:User="123abcd" asteria:Rating="2" asteria:Modified="2026-01-01T00:00:00Z"/>
        <rdf:li asteria:User="123abcd" asteria:Rating="3" asteria:Modified="2026-03-01T00:00:00Z"/>
        <rdf:li asteria:User="123abcd" asteria:Rating="4" asteria:Modified="2026-02-01T00:00:00Z"/>
        <rdf:li asteria:User="123abcd" asteria:Rating="5"/>
        </rdf:Bag></asteria:UserRatings>"#,
    )
    .unwrap();
    let a: UserId = "123abcd".parse().unwrap();
    assert_eq!(sc.user_ratings[&a].value, 3);
}

#[test]
fn half_a_hash_is_absent() {
    let sc = parse_desc(r#"asteria:HashScope="file/1">"#).unwrap();
    assert!(sc.content_hash.is_none());
    let h = format!("asteria:ContentHash=\"blake3:{}\">", "ab".repeat(32));
    assert!(parse_desc(&h).unwrap().content_hash.is_none());
}

#[test]
fn bare_literal_subjects_are_single_items() {
    let sc = parse_desc(
        r#"><dc:subject>cat</dc:subject><lr:hierarchicalSubject>animal|cat</lr:hierarchicalSubject>"#,
    )
    .unwrap();
    assert_eq!(sc.subjects.iter().collect::<Vec<_>>(), ["cat"]);
    assert!(sc.hierarchical_subjects.contains("animal|cat"));
}

#[test]
fn reduced_precision_dates() {
    let t = |s: &str| {
        parse_desc(&format!(r#"xmp:Rating="1" asteria:RatingModified="{s}">"#))
            .unwrap()
            .rating
            .unwrap()
            .modified
            .unwrap()
            .to_rfc3339()
    };
    assert_eq!(t("2026-10-22T09:00Z"), "2026-10-22T09:00:00+00:00");
    assert_eq!(t("2026-10-22"), "2026-10-22T00:00:00+00:00");
    assert_eq!(t("2026"), "2026-01-01T00:00:00+00:00");
    assert_eq!(t("2026-03"), "2026-03-01T00:00:00+00:00");
    assert_eq!(t("2026-10-22T09:00"), "2026-10-22T09:00:00+00:00");
    assert_eq!(
        t("2026-10-22T09:00:05.5+09:00"),
        "2026-10-22T00:00:05.500+00:00"
    );
    assert!(is_malformed(parse_desc(
        r#"xmp:Rating="1" asteria:RatingModified="yesterday">"#
    )));
}

#[test]
fn element_form_modified_and_hash() {
    let h = "ab".repeat(32);
    let sc = parse_desc(&format!(
        "><xmp:Rating>2</xmp:Rating><asteria:RatingModified>2026-10-22T09:00:00Z</asteria:RatingModified>\
         <asteria:ContentHash>blake3:{h}</asteria:ContentHash><asteria:HashScope>file/1</asteria:HashScope>"
    ))
    .unwrap();
    assert!(sc.rating.unwrap().modified.is_some());
    let (hash, scope) = sc.content_hash.unwrap();
    assert_eq!(hash.to_string(), format!("blake3:{h}"));
    assert_eq!(scope, HashScope::File1);
}

#[test]
fn element_form_values_on_their_own_lines_are_trimmed() {
    let xmp = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n  <rdf:Description xmlns:asteria=\"https://sapphire-turtle.github.io/ns/asteria/1.0/\">\n   <asteria:ContentHash>\n    blake3:abababababababababababababababababababababababababababababababab\n   </asteria:ContentHash>\n   <asteria:HashScope>\n    file/1\n   </asteria:HashScope>\n   <asteria:UserRatings><rdf:Bag><rdf:li>\n    <asteria:User>\n     123abcd\n    </asteria:User>\n    <asteria:Rating>\n     5\n    </asteria:Rating>\n   </rdf:li></rdf:Bag></asteria:UserRatings>\n  </rdf:Description>\n </rdf:RDF>\n</x:xmpmeta>";
    let sc = Sidecar::parse(xmp.as_bytes()).unwrap();
    let (_, scope) = sc.content_hash.unwrap();
    assert_eq!(scope, HashScope::File1);
    let u: UserId = "123abcd".parse().unwrap();
    assert_eq!(sc.user_ratings[&u].value, 5);
}

#[test]
fn unknown_hash_scope_is_absent_but_bad_hash_with_known_scope_is_malformed() {
    let good = "ab".repeat(32);
    let sc = parse_desc(&format!(
        r#"asteria:ContentHash="blake3:{good}" asteria:HashScope="media-payload/2">"#
    ))
    .unwrap();
    assert!(sc.content_hash.is_none());
    assert!(is_malformed(parse_desc(
        r#"asteria:ContentHash="blake3:zz" asteria:HashScope="media-payload/1">"#
    )));
}

/// A valid sidecar whose deepest element sits at `depth` (rdf:RDF is depth 1).
fn nested(depth: usize) -> String {
    let inner = depth - 2; // rdf:RDF and rdf:Description
    format!(
        r#"{HEAD}<rdf:Description xmlns:ex="urn:ex" xmp:Rating="2">{}{}</rdf:Description></rdf:RDF>"#,
        "<ex:a>".repeat(inner),
        "</ex:a>".repeat(inner)
    )
}

#[test]
fn element_depth_is_capped() {
    let ok = Sidecar::parse(nested(100).as_bytes()).unwrap();
    assert_eq!(ok.rating.unwrap().value, 2);
    match Sidecar::parse(nested(129).as_bytes()) {
        Err(Error::MalformedSidecar { reason, .. }) => {
            assert!(reason.contains("depth"), "{reason}")
        }
        Err(e) => panic!("unexpected error: {e:?}"),
        Ok(_) => panic!("depth 129 was accepted"),
    }
    // Far deeper input is rejected instead of overflowing the stack.
    assert!(is_malformed(Sidecar::parse(nested(60_000).as_bytes())));
}

#[test]
fn oversize_file_is_malformed() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("big.jpg.xmp");
    let mut body = nested(3).into_bytes();
    // Pad with trailing whitespace (still well-formed XML) to one byte over 16 MiB.
    body.resize(16 * 1024 * 1024 + 1, b' ');
    std::fs::write(&p, &body).unwrap();
    match Sidecar::read(&p) {
        Err(Error::MalformedSidecar {
            path: Some(_),
            reason,
        }) => {
            assert!(reason.contains("too large"), "{reason}")
        }
        Err(e) => panic!("unexpected error: {e:?}"),
        Ok(_) => panic!("oversize file was accepted"),
    }
    body.truncate(16 * 1024 * 1024);
    std::fs::write(&p, &body).unwrap();
    assert!(Sidecar::read(&p).unwrap().is_some());
}
