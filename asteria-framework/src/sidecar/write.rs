//! Writing sidecars: only asteria's own properties are replaced; every other node is copied from
//! the parsed tree unchanged.

use std::path::Path;

use chrono::{DateTime, SecondsFormat, Utc};

use crate::fsutil::atomic_write;
use crate::{Error, Result};

use super::model::{RatingEntry, Sidecar};
use super::ns;
use super::read::extract;
use super::xml::{self, Attribute, Element, Name, Node};

/// Properties asteria owns, as `(namespace URI, local name)`.
const MANAGED: [(&str, &str); 9] = [
    (ns::XMP, "Rating"),
    (ns::XMP, "Label"),
    (ns::ASTERIA, "RatingModified"),
    (ns::ASTERIA, "UserRatings"),
    (ns::ASTERIA, "ContentHash"),
    (ns::ASTERIA, "HashScope"),
    (ns::DC, "subject"),
    (ns::DC, "description"),
    (ns::LR, "hierarchicalSubject"),
];

const INDENT: &str = "  ";

impl Sidecar {
    /// Serialize to XMP. Unknown content from the parsed source is kept; asteria's properties are
    /// written in canonical form into the first `rdf:Description`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let (mut root, xpacket) = match &self.doc {
            Some(d) => (d.root.clone(), d.xpacket),
            None => (skeleton(), false),
        };
        let orig = self.doc.as_ref().and_then(|d| extract(&d.root).ok());
        apply(&mut root, self, &dirty_props(self, orig.as_ref()));
        xml::serialize(&root, xpacket)
    }

    /// Atomically write the sidecar to `path`.
    ///
    /// The serialized bytes are parsed back first; if that fails, or the managed properties read
    /// back differ from `self`, nothing is written and an error is returned, so a serializer bug
    /// can never replace a valid sidecar with a corrupt one.
    pub fn write(&self, path: &Path) -> Result<()> {
        let bytes = self.to_bytes();
        verify_output(self, &bytes).map_err(Error::UnsafeSidecarWrite)?;
        atomic_write(path, &bytes)
    }
}

fn skeleton() -> Element {
    Element {
        name: Name {
            ns: Some("adobe:ns:meta/".into()),
            prefix: Some("x".into()),
            local: "xmpmeta".into(),
        },
        declarations: vec![("x".into(), "adobe:ns:meta/".into())],
        attributes: vec![],
        children: vec![],
    }
}

type Prop = (&'static str, &'static str);

/// The managed properties whose value differs from what the source document says. Everything
/// else is left exactly as it is in the tree.
fn dirty_props(sc: &Sidecar, orig: Option<&Sidecar>) -> Vec<Prop> {
    let Some(o) = orig else {
        return MANAGED.to_vec();
    };
    let mut d = Vec::new();
    let mut add = |changed: bool, props: &[Prop]| {
        if changed {
            d.extend_from_slice(props);
        }
    };
    let value = |s: &Sidecar| s.rating.as_ref().map(|r| r.value);
    let modified = |s: &Sidecar| s.rating.as_ref().and_then(|r| r.modified);
    add(value(sc) != value(o), &[(ns::XMP, "Rating")]);
    // A stray RatingModified attaches to whatever rating exists, so it is rewritten with Rating.
    add(
        value(sc) != value(o) || modified(sc) != modified(o),
        &[(ns::ASTERIA, "RatingModified")],
    );
    add(sc.label != o.label, &[(ns::XMP, "Label")]);
    add(sc.subjects != o.subjects, &[(ns::DC, "subject")]);
    add(
        sc.hierarchical_subjects != o.hierarchical_subjects,
        &[(ns::LR, "hierarchicalSubject")],
    );
    add(sc.description != o.description, &[(ns::DC, "description")]);
    add(
        sc.user_ratings != o.user_ratings,
        &[(ns::ASTERIA, "UserRatings")],
    );
    add(
        sc.content_hash != o.content_hash,
        &[(ns::ASTERIA, "ContentHash"), (ns::ASTERIA, "HashScope")],
    );
    d
}

/// Check that `bytes` parses and reads back as the managed properties of `sc`. Values are
/// compared the way the reader sees them (trimmed, empty ones absent).
fn verify_output(sc: &Sidecar, bytes: &[u8]) -> std::result::Result<(), String> {
    let back = Sidecar::parse(bytes).map_err(|e| format!("output does not parse: {e}"))?;
    let clean = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
    let set = |s: &std::collections::BTreeSet<String>| -> std::collections::BTreeSet<String> {
        s.iter().filter_map(|v| clean(v)).collect()
    };
    let mut bad = Vec::new();
    let mut check = |name: &str, same: bool| {
        if !same {
            bad.push(name.to_string());
        }
    };
    check("rating", sc.rating == back.rating);
    check("userRatings", sc.user_ratings == back.user_ratings);
    check("subject", set(&sc.subjects) == back.subjects);
    check(
        "hierarchicalSubject",
        set(&sc.hierarchical_subjects) == back.hierarchical_subjects,
    );
    check("label", sc.label.as_deref().and_then(clean) == back.label);
    check("contentHash", sc.content_hash == back.content_hash);
    // Other-language items may legitimately stand in for an absent description.
    if let Some(d) = sc.description.as_deref().and_then(clean) {
        check("description", Some(d) == back.description);
    }
    if clean_opt(&sc.description).is_none()
        && let Ok(root) = xml::parse(bytes)
    {
        check("description (stray x-default)", !has_x_default(&root));
    }
    if bad.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "output reads back differently for: {}",
            bad.join(", ")
        ))
    }
}

fn clean_opt(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

fn has_x_default(e: &Element) -> bool {
    (e.is(ns::DC, "description")
        && e.child_elements().any(|c| {
            c.child_elements()
                .any(|li| li.attr(ns::XML, "lang") == Some("x-default"))
        }))
        || e.child_elements().any(has_x_default)
}

fn is_dirty(dirty: &[Prop], ns: Option<&str>, local: &str) -> bool {
    ns.is_some_and(|ns| dirty.iter().any(|&(u, l)| u == ns && l == local))
}

fn has(dirty: &[Prop], ns: &str, local: &str) -> bool {
    is_dirty(dirty, Some(ns), local)
}

/// Namespace declarations in scope at the element being written.
struct Scope(Vec<(String, String)>);

impl Scope {
    fn resolve(&self, prefix: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|(p, _)| p == prefix)
            .map(|(_, u)| u.as_str())
    }

    /// A prefix bound to `uri` here, declaring one (in `new`, and in scope) if there is none.
    fn prefix(&mut self, uri: &str, canonical: &str, new: &mut Vec<(String, String)>) -> String {
        let found = self
            .0
            .iter()
            .rev()
            .find(|(p, u)| u == uri && !p.is_empty() && self.resolve(p) == Some(uri));
        if let Some((p, _)) = found {
            return p.clone();
        }
        let mut prefix = canonical.to_string();
        let mut n = 1;
        while self.resolve(&prefix).is_some() {
            prefix = format!("{canonical}{n}");
            n += 1;
        }
        self.0.push((prefix.clone(), uri.to_string()));
        new.push((prefix.clone(), uri.to_string()));
        prefix
    }
}

fn is_blank(n: &Node) -> bool {
    matches!(n, Node::Text(t) if t.trim().is_empty())
}

fn newline(depth: usize) -> Node {
    Node::Text(format!("\n{}", INDENT.repeat(depth)))
}

fn find_rdf(e: &mut Element) -> Option<&mut Element> {
    e.children.iter_mut().find_map(|n| match n {
        Node::Element(c) if c.is(ns::RDF, "RDF") => Some(c),
        _ => None,
    })
}

fn apply(root: &mut Element, sc: &Sidecar, dirty: &[Prop]) {
    let mut scope = Scope(root.declarations.clone());
    let rdf: &mut Element = if root.is(ns::RDF, "RDF") {
        root
    } else {
        if find_rdf(root).is_none() {
            let prefix = scope.prefix(ns::RDF, "rdf", &mut Vec::new());
            let mut e = new_element(ns::RDF, &prefix, "RDF");
            e.declarations.push((prefix, ns::RDF.to_string()));
            root.children.push(newline(1));
            root.children.push(Node::Element(e));
            root.children.push(newline(0));
        }
        let Some(rdf) = find_rdf(root) else { return };
        scope.0.extend(rdf.declarations.iter().cloned());
        rdf
    };

    // Managed properties may sit in any Description (attribute or element form); clear them all.
    let mut other_langs: Vec<Element> = Vec::new();
    for n in &mut rdf.children {
        if let Node::Element(d) = n
            && d.is(ns::RDF, "Description")
        {
            strip_managed(d, dirty, &mut other_langs);
        }
    }

    let idx = rdf
        .children
        .iter()
        .position(|n| matches!(n, Node::Element(d) if d.is(ns::RDF, "Description")));
    let idx = idx.unwrap_or_else(|| {
        let mut new = Vec::new();
        let prefix = scope.prefix(ns::RDF, "rdf", &mut new);
        let mut d = new_element(ns::RDF, &prefix, "Description");
        d.declarations = new;
        d.attributes.push(Attribute {
            name: qname(ns::RDF, &prefix, "about"),
            value: String::new(),
        });
        let at = rdf
            .children
            .iter()
            .rposition(|n| !is_blank(n))
            .map_or(0, |i| i + 1);
        rdf.children.insert(at, Node::Element(d));
        rdf.children.insert(at, newline(2));
        if at == 0 {
            rdf.children.push(newline(1));
        }
        at + 1
    });
    let Node::Element(desc) = &mut rdf.children[idx] else {
        return;
    };
    scope.0.extend(desc.declarations.iter().cloned());
    fill(desc, &mut scope, sc, dirty, other_langs);
}

fn qname(ns: &str, prefix: &str, local: &str) -> Name {
    Name {
        ns: Some(ns.to_string()),
        prefix: Some(prefix.to_string()),
        local: local.to_string(),
    }
}

fn new_element(ns: &str, prefix: &str, local: &str) -> Element {
    Element {
        name: qname(ns, prefix, local),
        declarations: vec![],
        attributes: vec![],
        children: vec![],
    }
}

/// Remove every dirty attribute and child element. Language alternatives of `dc:description`
/// other than `x-default` belong to the user, so they are returned (as they are) for reuse.
fn strip_managed(d: &mut Element, dirty: &[Prop], other_langs: &mut Vec<Element>) {
    d.attributes
        .retain(|a| !is_dirty(dirty, a.name.ns.as_deref(), &a.name.local));
    let mut kept: Vec<Node> = Vec::with_capacity(d.children.len());
    for n in std::mem::take(&mut d.children) {
        match n {
            Node::Element(e) if is_dirty(dirty, e.name.ns.as_deref(), &e.name.local) => {
                if e.is(ns::DC, "description") {
                    collect_other_langs(&e, other_langs);
                }
                // The indentation that led up to the removed element goes with it.
                if kept.last().is_some_and(is_blank) {
                    kept.pop();
                }
            }
            n => kept.push(n),
        }
    }
    d.children = kept;
}

fn collect_other_langs(prop: &Element, out: &mut Vec<Element>) {
    let Some(alt) = prop.child(ns::RDF, "Alt") else {
        return;
    };
    for li in alt.child_elements().filter(|e| e.is(ns::RDF, "li")) {
        if let Some(lang) = li.attr(ns::XML, "lang")
            && lang != "x-default"
            && !out.iter().any(|o| o.attr(ns::XML, "lang") == Some(lang))
        {
            out.push(li.clone());
        }
    }
}

fn fmt_time(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn fill(
    d: &mut Element,
    scope: &mut Scope,
    sc: &Sidecar,
    dirty: &[Prop],
    other_langs: Vec<Element>,
) {
    let mut decls = Vec::new();
    let mut xmp = None;
    let mut asteria = None;
    let mut dc = None;
    let mut lr = None;
    let mut rdf = None;
    // Lazily resolve each prefix so only the namespaces actually used get declared.
    macro_rules! pfx {
        ($slot:ident, $uri:expr, $canon:expr) => {{
            if $slot.is_none() {
                $slot = Some(scope.prefix($uri, $canon, &mut decls));
            }
            $slot.clone().unwrap_or_default()
        }};
    }
    let attr = |ns: &str, prefix: String, local: &str, value: String| Attribute {
        name: qname(ns, &prefix, local),
        value,
    };

    let mut attrs = Vec::new();
    let mut props: Vec<Element> = Vec::new();

    if let Some(r) = &sc.rating {
        if has(dirty, ns::XMP, "Rating") {
            let p = pfx!(xmp, ns::XMP, "xmp");
            attrs.push(attr(ns::XMP, p, "Rating", r.value.to_string()));
        }
        if let (Some(m), true) = (r.modified, has(dirty, ns::ASTERIA, "RatingModified")) {
            let p = pfx!(asteria, ns::ASTERIA, "asteria");
            attrs.push(attr(ns::ASTERIA, p, "RatingModified", fmt_time(m)));
        }
    }
    let label = sc.label.as_deref().filter(|_| has(dirty, ns::XMP, "Label"));
    if let Some(l) = label.filter(|l| !l.is_empty()) {
        let p = pfx!(xmp, ns::XMP, "xmp");
        attrs.push(attr(ns::XMP, p, "Label", l.to_string()));
    }
    let hash = sc.content_hash.as_ref();
    if let Some((h, s)) = hash.filter(|_| has(dirty, ns::ASTERIA, "ContentHash")) {
        let p = pfx!(asteria, ns::ASTERIA, "asteria");
        attrs.push(attr(ns::ASTERIA, p.clone(), "ContentHash", h.to_string()));
        attrs.push(attr(ns::ASTERIA, p, "HashScope", s.to_string()));
    }

    let subjects: Vec<&String> = sc
        .subjects
        .iter()
        .filter(|s| !s.is_empty() && has(dirty, ns::DC, "subject"))
        .collect();
    if !subjects.is_empty() {
        let (p, r) = (pfx!(dc, ns::DC, "dc"), pfx!(rdf, ns::RDF, "rdf"));
        props.push(literal_container(
            ns::DC,
            &p,
            "subject",
            &r,
            "Bag",
            subjects,
        ));
    }
    let hier: Vec<&String> = sc
        .hierarchical_subjects
        .iter()
        .filter(|s| !s.is_empty() && has(dirty, ns::LR, "hierarchicalSubject"))
        .collect();
    if !hier.is_empty() {
        let (p, r) = (pfx!(lr, ns::LR, "lr"), pfx!(rdf, ns::RDF, "rdf"));
        props.push(literal_container(
            ns::LR,
            &p,
            "hierarchicalSubject",
            &r,
            "Bag",
            hier,
        ));
    }

    let desc_dirty = has(dirty, ns::DC, "description");
    let desc = sc.description.as_deref().filter(|s| !s.is_empty());
    if desc_dirty && (desc.is_some() || !other_langs.is_empty()) {
        let (p, r) = (pfx!(dc, ns::DC, "dc"), pfx!(rdf, ns::RDF, "rdf"));
        let mut alt = new_element(ns::RDF, &r, "Alt");
        if let Some(t) = desc {
            let mut li = new_element(ns::RDF, &r, "li");
            li.attributes.push(Attribute {
                name: qname(ns::XML, "xml", "lang"),
                value: "x-default".to_string(),
            });
            li.children.push(Node::Text(t.to_string()));
            alt.children.push(Node::Element(li));
        }
        for mut li in other_langs {
            bind_prefixes(&mut li, scope);
            alt.children.push(Node::Element(li));
        }
        let mut prop = new_element(ns::DC, &p, "description");
        prop.children.push(Node::Element(alt));
        props.push(prop);
    }

    if !sc.user_ratings.is_empty() && has(dirty, ns::ASTERIA, "UserRatings") {
        let (a, r) = (
            pfx!(asteria, ns::ASTERIA, "asteria"),
            pfx!(rdf, ns::RDF, "rdf"),
        );
        let mut bag = new_element(ns::RDF, &r, "Bag");
        for (user, RatingEntry { value, modified }) in &sc.user_ratings {
            let mut li = new_element(ns::RDF, &r, "li");
            li.attributes
                .push(attr(ns::ASTERIA, a.clone(), "User", user.to_string()));
            li.attributes
                .push(attr(ns::ASTERIA, a.clone(), "Rating", value.to_string()));
            if let Some(m) = modified {
                li.attributes
                    .push(attr(ns::ASTERIA, a.clone(), "Modified", fmt_time(*m)));
            }
            bag.children.push(Node::Element(li));
        }
        let mut prop = new_element(ns::ASTERIA, &a, "UserRatings");
        prop.children.push(Node::Element(bag));
        props.push(prop);
    }

    d.declarations.extend(decls);
    d.attributes.extend(attrs);
    if props.is_empty() {
        return;
    }
    let mut new_nodes = Vec::new();
    for mut p in props {
        indent(&mut p, 3);
        new_nodes.push(newline(3));
        new_nodes.push(Node::Element(p));
    }
    match d.children.last() {
        Some(last) if is_blank(last) => {
            let at = d.children.len() - 1;
            d.children.splice(at..at, new_nodes);
        }
        _ => {
            d.children.extend(new_nodes);
            d.children.push(newline(2));
        }
    }
}

/// `<prop><rdf:Bag><rdf:li>v</rdf:li>...</rdf:Bag></prop>`
fn literal_container(
    ns_uri: &str,
    prefix: &str,
    local: &str,
    rdf: &str,
    kind: &str,
    values: Vec<&String>,
) -> Element {
    let mut c = new_element(ns::RDF, rdf, kind);
    for v in values {
        let mut li = new_element(ns::RDF, rdf, "li");
        li.children.push(Node::Text(v.clone()));
        c.children.push(Node::Element(li));
    }
    let mut prop = new_element(ns_uri, prefix, local);
    prop.children.push(Node::Element(c));
    prop
}

/// Put each element child of a freshly built element on its own line.
fn indent(e: &mut Element, depth: usize) {
    if !e.children.iter().all(|n| matches!(n, Node::Element(_))) || e.children.is_empty() {
        return;
    }
    let mut out = Vec::new();
    for n in std::mem::take(&mut e.children) {
        if let Node::Element(mut c) = n {
            indent(&mut c, depth + 1);
            out.push(newline(depth + 1));
            out.push(Node::Element(c));
        }
    }
    out.push(newline(depth));
    e.children = out;
}

/// An element moved under a different `rdf:Description` must still resolve every prefix (and the
/// default namespace) to the same URI. Declare, on `li`, whatever the target scope lacks, taking
/// the URI from the names themselves.
fn bind_prefixes(li: &mut Element, target: &Scope) {
    let mut needs: Vec<(String, String)> = Vec::new();
    let mut local: Vec<(String, String)> = Vec::new();
    collect_needs(li, target, &mut local, &mut needs);
    for n in needs {
        if !li.declarations.contains(&n) {
            li.declarations.push(n);
        }
    }
}

fn collect_needs(
    e: &Element,
    target: &Scope,
    local: &mut Vec<(String, String)>,
    needs: &mut Vec<(String, String)>,
) {
    let mark = local.len();
    local.extend(e.declarations.iter().cloned());
    let mut need = |prefix: &str, uri: &str| {
        let bound_locally = local.iter().any(|(p, _)| p == prefix);
        if !bound_locally
            && target.resolve(prefix) != Some(uri)
            && !needs.iter().any(|(p, u)| p == prefix && u == uri)
        {
            needs.push((prefix.to_string(), uri.to_string()));
        }
    };
    match (&e.name.prefix, &e.name.ns) {
        (Some(p), Some(u)) => need(p, u),
        (None, Some(u)) => need("", u),
        _ => {}
    }
    for a in &e.attributes {
        if let (Some(p), Some(u)) = (&a.name.prefix, &a.name.ns)
            && p != "xml"
        {
            need(p, u);
        }
    }
    for c in e.child_elements() {
        collect_needs(c, target, local, needs);
    }
    local.truncate(mark);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_accepts_matching_and_rejects_corrupt_or_different_output() {
        let mut sc = Sidecar::new();
        sc.label = Some("Red".into());
        sc.subjects.insert(" a ".into());
        let good = sc.to_bytes();
        assert!(verify_output(&sc, &good).is_ok());

        let mut other = sc.clone();
        other.label = Some("Blue".into());
        let err = verify_output(&other, &good).unwrap_err();
        assert!(err.contains("label"), "{err}");

        assert!(verify_output(&sc, &good[..good.len() / 2]).is_err());
        assert!(verify_output(&sc, b"<x:unbound/>").is_err());
    }

    #[test]
    fn verify_rejects_x_default_when_model_has_no_description() {
        let mut with = Sidecar::new();
        with.description = Some("A cat".into());
        let bytes = with.to_bytes();
        let without = Sidecar::new();
        let err = verify_output(&without, &bytes).unwrap_err();
        assert!(err.contains("x-default"), "{err}");
        assert!(verify_output(&with, &bytes).is_ok());
    }
}
