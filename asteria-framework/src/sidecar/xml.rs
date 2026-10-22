//! Minimal namespace-aware XML tree.
//!
//! Unlike `xmltree`, attributes keep their namespace URI and prefix, and each element keeps the
//! namespace declarations made on it, so a later serializer can write the document back without
//! losing information. Only parsing lives here for now.
// The prefix/declaration/comment fields exist for the serializer added in a later task.
#![allow(dead_code)]

use std::collections::BTreeMap;

use xml::reader::{EventReader, ParserConfig, XmlEvent};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Name {
    pub ns: Option<String>,
    pub prefix: Option<String>,
    pub local: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Attribute {
    pub name: Name,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Node {
    Element(Element),
    Text(String),
    CData(String),
    Comment(String),
    ProcessingInstruction { name: String, data: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Element {
    pub name: Name,
    /// `(prefix, uri)` declarations made on this element; the default namespace has prefix `""`.
    pub declarations: Vec<(String, String)>,
    pub attributes: Vec<Attribute>,
    pub children: Vec<Node>,
}

impl Element {
    pub fn is(&self, ns: &str, local: &str) -> bool {
        self.name.ns.as_deref() == Some(ns) && self.name.local == local
    }

    /// Value of the attribute with this namespace URI and local name.
    pub fn attr(&self, ns: &str, local: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|a| a.name.ns.as_deref() == Some(ns) && a.name.local == local)
            .map(|a| a.value.as_str())
    }

    pub fn child_elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|n| match n {
            Node::Element(e) => Some(e),
            _ => None,
        })
    }

    pub fn child(&self, ns: &str, local: &str) -> Option<&Element> {
        self.child_elements().find(|e| e.is(ns, local))
    }

    /// Concatenated text and CDATA children, trimmed.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for n in &self.children {
            if let Node::Text(t) | Node::CData(t) = n {
                out.push_str(t);
            }
        }
        out.trim().to_string()
    }
}

fn convert_name(n: &xml::name::OwnedName) -> Name {
    Name {
        ns: n.namespace.clone(),
        prefix: n.prefix.clone(),
        local: n.local_name.clone(),
    }
}

/// Parse a document into its root element. Root-level comments and processing instructions
/// (including `<?xpacket?>`) are not kept.
pub(crate) fn parse(bytes: &[u8]) -> Result<Element, String> {
    let config = ParserConfig::new()
        .ignore_comments(false)
        .whitespace_to_characters(true);
    let reader = EventReader::new_with_config(bytes, config);

    let mut stack: Vec<Element> = Vec::new();
    let mut ns_stack: Vec<BTreeMap<String, String>> = Vec::new();
    let mut root = None;

    for event in reader {
        match event.map_err(|e| e.to_string())? {
            XmlEvent::StartElement {
                name,
                attributes,
                namespace,
            } => {
                let empty = BTreeMap::new();
                let is_root = ns_stack.is_empty();
                let parent = ns_stack.last().unwrap_or(&empty);
                let declarations = namespace
                    .0
                    .iter()
                    .filter(|(p, u)| {
                        // The reader seeds the root scope with xml/xmlns and an empty default.
                        p.as_str() != "xml"
                            && p.as_str() != "xmlns"
                            && !(is_root && p.is_empty() && u.is_empty())
                            && parent.get(*p) != Some(*u)
                    })
                    .map(|(p, u)| (p.clone(), u.clone()))
                    .collect();
                ns_stack.push(namespace.0.clone());
                stack.push(Element {
                    name: convert_name(&name),
                    declarations,
                    attributes: attributes
                        .iter()
                        .map(|a| Attribute {
                            name: convert_name(&a.name),
                            value: a.value.clone(),
                        })
                        .collect(),
                    children: Vec::new(),
                });
            }
            XmlEvent::EndElement { .. } => {
                ns_stack.pop();
                let done = stack.pop().ok_or("unbalanced end tag")?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(Node::Element(done)),
                    None => root = Some(done),
                }
            }
            XmlEvent::Characters(t) | XmlEvent::Whitespace(t) => {
                if let Some(top) = stack.last_mut() {
                    top.children.push(Node::Text(t));
                }
            }
            XmlEvent::CData(t) => {
                if let Some(top) = stack.last_mut() {
                    top.children.push(Node::CData(t));
                }
            }
            XmlEvent::Comment(t) => {
                if let Some(top) = stack.last_mut() {
                    top.children.push(Node::Comment(t));
                }
            }
            XmlEvent::ProcessingInstruction { name, data } => {
                if let Some(top) = stack.last_mut() {
                    top.children
                        .push(Node::ProcessingInstruction { name, data });
                }
            }
            XmlEvent::StartDocument { .. } | XmlEvent::EndDocument => {}
        }
    }
    root.ok_or_else(|| "no root element".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_local_name_attributes_in_different_namespaces_both_survive() {
        let x = r#"<r xmlns:a="urn:a" xmlns:b="urn:b" a:Rating="1" b:Rating="2" xml:lang="x"/>"#;
        let e = parse(x.as_bytes()).unwrap();
        assert_eq!(e.attr("urn:a", "Rating"), Some("1"));
        assert_eq!(e.attr("urn:b", "Rating"), Some("2"));
        assert_eq!(e.attr("", "Rating"), None);
        assert_eq!(e.attr(super::super::ns::XML, "lang"), Some("x"));
        let a = e.attributes.iter().find(|a| a.value == "1").unwrap();
        assert_eq!(a.name.prefix.as_deref(), Some("a"));
        assert_eq!(
            e.declarations,
            [("a".into(), "urn:a".into()), ("b".into(), "urn:b".into())]
        );
    }

    #[test]
    fn keeps_child_order_and_node_kinds() {
        let x = "<r><!--c--><a/>t<![CDATA[d]]><?pi x?></r>";
        let e = parse(x.as_bytes()).unwrap();
        assert!(matches!(e.children[0], Node::Comment(_)));
        assert!(matches!(e.children[1], Node::Element(_)));
        assert!(matches!(e.children[2], Node::Text(_)));
        assert!(matches!(e.children[3], Node::CData(_)));
        assert!(matches!(e.children[4], Node::ProcessingInstruction { .. }));
    }

    #[test]
    fn unclosed_tag_is_error() {
        assert!(parse(b"<r><a></r>").is_err());
        assert!(parse(b"<r>").is_err());
        assert!(parse(b"").is_err());
    }
}
