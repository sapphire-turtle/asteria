//! Writing sidecars: only asteria's own properties are replaced; every other node is copied from
//! the parsed tree unchanged.

use std::path::Path;

use chrono::{DateTime, SecondsFormat, Utc};

use crate::Result;
use crate::fsutil::atomic_write;

use super::model::{RatingEntry, Sidecar};
use super::ns;
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
        apply(&mut root, self);
        xml::serialize(&root, xpacket)
    }

    /// Atomically write the sidecar to `path`.
    pub fn write(&self, path: &Path) -> Result<()> {
        atomic_write(path, &self.to_bytes())
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

fn is_managed(ns: Option<&str>, local: &str) -> bool {
    ns.is_some_and(|ns| MANAGED.iter().any(|&(u, l)| u == ns && l == local))
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

fn apply(root: &mut Element, sc: &Sidecar) {
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
    let mut other_langs = Vec::new();
    for n in &mut rdf.children {
        if let Node::Element(d) = n
            && d.is(ns::RDF, "Description")
        {
            strip_managed(d, &mut other_langs);
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
    fill(desc, &mut scope, sc, other_langs);
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

/// Remove every managed attribute and child element. Language alternatives of `dc:description`
/// other than `x-default` belong to the user, so they are returned as `(lang, text)`.
fn strip_managed(d: &mut Element, other_langs: &mut Vec<(String, String)>) {
    d.attributes
        .retain(|a| !is_managed(a.name.ns.as_deref(), &a.name.local));
    let mut kept: Vec<Node> = Vec::with_capacity(d.children.len());
    for n in std::mem::take(&mut d.children) {
        match n {
            Node::Element(e) if is_managed(e.name.ns.as_deref(), &e.name.local) => {
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

fn collect_other_langs(prop: &Element, out: &mut Vec<(String, String)>) {
    let Some(alt) = prop.child(ns::RDF, "Alt") else {
        return;
    };
    for li in alt.child_elements().filter(|e| e.is(ns::RDF, "li")) {
        if let Some(lang) = li.attr(ns::XML, "lang")
            && lang != "x-default"
        {
            out.push((lang.to_string(), li.raw_text()));
        }
    }
}

fn fmt_time(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn fill(d: &mut Element, scope: &mut Scope, sc: &Sidecar, other_langs: Vec<(String, String)>) {
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
        let p = pfx!(xmp, ns::XMP, "xmp");
        attrs.push(attr(ns::XMP, p, "Rating", r.value.to_string()));
        if let Some(m) = r.modified {
            let p = pfx!(asteria, ns::ASTERIA, "asteria");
            attrs.push(attr(ns::ASTERIA, p, "RatingModified", fmt_time(m)));
        }
    }
    if let Some(l) = sc.label.as_deref().filter(|l| !l.is_empty()) {
        let p = pfx!(xmp, ns::XMP, "xmp");
        attrs.push(attr(ns::XMP, p, "Label", l.to_string()));
    }
    if let Some((h, s)) = &sc.content_hash {
        let p = pfx!(asteria, ns::ASTERIA, "asteria");
        attrs.push(attr(ns::ASTERIA, p.clone(), "ContentHash", h.to_string()));
        attrs.push(attr(ns::ASTERIA, p, "HashScope", s.to_string()));
    }

    let subjects: Vec<&String> = sc.subjects.iter().filter(|s| !s.is_empty()).collect();
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
        .filter(|s| !s.is_empty())
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

    let desc = sc.description.as_deref().filter(|s| !s.is_empty());
    if desc.is_some() || !other_langs.is_empty() {
        let (p, r) = (pfx!(dc, ns::DC, "dc"), pfx!(rdf, ns::RDF, "rdf"));
        let lang = |l: &str, text: &str| {
            let mut li = new_element(ns::RDF, &r, "li");
            li.attributes.push(Attribute {
                name: qname(ns::XML, "xml", "lang"),
                value: l.to_string(),
            });
            li.children.push(Node::Text(text.to_string()));
            Node::Element(li)
        };
        let mut alt = new_element(ns::RDF, &r, "Alt");
        if let Some(t) = desc {
            alt.children.push(lang("x-default", t));
        }
        for (l, t) in &other_langs {
            alt.children.push(lang(l, t));
        }
        let mut prop = new_element(ns::DC, &p, "description");
        prop.children.push(Node::Element(alt));
        props.push(prop);
    }

    if !sc.user_ratings.is_empty() {
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
