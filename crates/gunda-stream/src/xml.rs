//! A bounded XML reader for MPEG-DASH manifests.
//!
//! This deliberately implements only what an MPD needs: elements, attributes
//! (single or double quoted), text, CDATA, comments, the XML prolog, and
//! character references. It resolves no namespaces (prefixes are stripped and
//! only local names are matched), expands no DTD entities, and validates no
//! schema. A manifest that needs more than this is rejected rather than
//! partially understood.
//!
//! Depth and node count are capped so a hostile manifest cannot exhaust memory.

use crate::error::ManifestError;

/// Maximum element nesting accepted from a manifest.
const MAX_DEPTH: usize = 64;

/// Maximum element count accepted from a manifest.
const MAX_NODES: usize = 100_000;

/// One parsed element.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Node {
    name: String,
    attributes: Vec<(String, String)>,
    children: Vec<Node>,
    text: String,
}

impl Node {
    /// Returns the element's local name, without any namespace prefix.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns an attribute by local name.
    #[must_use]
    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// Returns an attribute parsed as an unsigned integer.
    pub fn attribute_u64(&self, name: &str) -> Result<Option<u64>, ManifestError> {
        self.attribute(name)
            .map(|value| {
                value
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| ManifestError::InvalidNumber)
            })
            .transpose()
    }

    /// Returns an attribute parsed as a signed integer.
    pub fn attribute_i64(&self, name: &str) -> Result<Option<i64>, ManifestError> {
        self.attribute(name)
            .map(|value| {
                value
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| ManifestError::InvalidNumber)
            })
            .transpose()
    }

    /// Returns an attribute parsed as a 32-bit unsigned integer.
    pub fn attribute_u32(&self, name: &str) -> Result<Option<u32>, ManifestError> {
        self.attribute(name)
            .map(|value| {
                value
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| ManifestError::InvalidNumber)
            })
            .transpose()
    }

    /// Iterates the direct children with the given local name.
    pub fn children_named<'a, 'n: 'a>(
        &'a self,
        name: &'n str,
    ) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |child| child.name == name)
    }

    /// Returns the first direct child with the given local name.
    #[must_use]
    pub fn child<'a>(&'a self, name: &str) -> Option<&'a Node> {
        self.children.iter().find(|child| child.name == name)
    }

    /// Returns the element's own text content, trimmed.
    #[must_use]
    pub fn text(&self) -> &str {
        self.text.trim()
    }
}

/// Parses a document and returns its root element.
pub fn parse_document(input: &str) -> Result<Node, ManifestError> {
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let mut cursor = 0_usize;

    let mut stack: Vec<Node> = Vec::new();
    let mut root: Option<Node> = None;
    let mut node_count = 0_usize;

    loop {
        let rest = &input[cursor..];

        if rest.is_empty() {
            break;
        }

        match rest.find('<') {
            Some(index) => {
                push_text(&mut stack, &rest[..index])?;
                cursor += index;
            }
            None => {
                push_text(&mut stack, rest)?;
                break;
            }
        }

        let rest = &input[cursor..];

        if let Some(after) = rest.strip_prefix("</") {
            let end = after.find('>').ok_or(ManifestError::Malformed)?;
            let name = after[..end].trim();
            let name = local_name(name);

            if name.is_empty() {
                return Err(ManifestError::Malformed);
            }

            let node = stack.pop().ok_or(ManifestError::Malformed)?;

            if node.name != name {
                return Err(ManifestError::Malformed);
            }

            cursor += 2 + end + 1;
            attach(&mut stack, &mut root, node)?;
            continue;
        }

        if let Some(after) = rest.strip_prefix("<!--") {
            let end = after.find("-->").ok_or(ManifestError::Malformed)?;
            cursor += 4 + end + 3;
            continue;
        }

        if let Some(after) = rest.strip_prefix("<![CDATA[") {
            let end = after.find("]]>").ok_or(ManifestError::Malformed)?;
            push_text(&mut stack, &after[..end])?;
            cursor += 9 + end + 3;
            continue;
        }

        if let Some(after) = rest.strip_prefix("<?") {
            let end = after.find("?>").ok_or(ManifestError::Malformed)?;
            cursor += 2 + end + 2;
            continue;
        }

        if rest.starts_with("<!") {
            let end = rest.find('>').ok_or(ManifestError::Malformed)?;
            cursor += end + 1;
            continue;
        }

        let end = find_tag_end(rest)?;
        let inner = rest[1..end].trim_end();
        let self_closing = inner.ends_with('/');
        let inner = inner.strip_suffix('/').unwrap_or(inner).trim_end();

        let (name, attributes) = parse_tag(inner)?;

        node_count += 1;
        if node_count > MAX_NODES {
            return Err(ManifestError::Malformed);
        }

        let node = Node {
            name,
            attributes,
            children: Vec::new(),
            text: String::new(),
        };

        cursor += end + 1;

        if self_closing {
            attach(&mut stack, &mut root, node)?;
        } else {
            if stack.len() >= MAX_DEPTH {
                return Err(ManifestError::Malformed);
            }

            stack.push(node);
        }
    }

    if !stack.is_empty() {
        return Err(ManifestError::Malformed);
    }

    root.ok_or(ManifestError::Malformed)
}

fn attach(stack: &mut [Node], root: &mut Option<Node>, node: Node) -> Result<(), ManifestError> {
    match stack.last_mut() {
        Some(parent) => parent.children.push(node),
        None => {
            if root.is_some() {
                // A second root element is not a document.
                return Err(ManifestError::Malformed);
            }

            *root = Some(node);
        }
    }

    Ok(())
}

fn push_text(stack: &mut [Node], text: &str) -> Result<(), ManifestError> {
    if text.trim().is_empty() {
        return Ok(());
    }

    if let Some(target) = stack.last_mut() {
        target.text.push_str(&decode_entities(text)?);
    } else if !text.trim().is_empty() {
        // Text outside the root element is only legal as whitespace.
        return Err(ManifestError::Malformed);
    }

    Ok(())
}

/// Finds the `>` ending a tag, ignoring `>` inside quoted attribute values.
fn find_tag_end(rest: &str) -> Result<usize, ManifestError> {
    let mut quote: Option<char> = None;

    for (index, character) in rest.char_indices() {
        match (quote, character) {
            (Some(open), character) if character == open => quote = None,
            (None, character @ ('"' | '\'')) => quote = Some(character),
            (None, '>') => return Ok(index),
            _ => {}
        }
    }

    Err(ManifestError::Malformed)
}

/// Parses an element's name and attributes.
fn parse_tag(inner: &str) -> Result<(String, Vec<(String, String)>), ManifestError> {
    let split = inner.find(char::is_whitespace).unwrap_or(inner.len());
    let name = inner[..split].trim();

    if name.is_empty() {
        return Err(ManifestError::Malformed);
    }

    let mut rest = inner[split..].trim_start();
    let mut attributes = Vec::new();

    while !rest.is_empty() {
        let Some((key, tail)) = rest.split_once('=') else {
            return Err(ManifestError::Malformed);
        };

        let key = key.trim();

        if key.is_empty() || key.contains(char::is_whitespace) {
            return Err(ManifestError::Malformed);
        }

        let key = local_name(key);

        let tail = tail.trim_start();

        let (value, next) = if let Some(quoted) = tail.strip_prefix('"') {
            let (value, tail) = quoted.split_once('"').ok_or(ManifestError::Malformed)?;
            (value.to_owned(), tail)
        } else if let Some(quoted) = tail.strip_prefix('\'') {
            let (value, tail) = quoted.split_once('\'').ok_or(ManifestError::Malformed)?;
            (value.to_owned(), tail)
        } else {
            let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
            (tail[..end].to_owned(), &tail[end..])
        };

        if attributes
            .iter()
            .any(|(existing, _): &(String, String)| existing == key)
        {
            return Err(ManifestError::Malformed);
        }

        attributes.push((key.to_owned(), decode_entities(&value)?));
        rest = next.trim_start();
    }

    Ok((local_name(name).to_owned(), attributes))
}

/// Strips a namespace prefix, if any.
fn local_name(name: &str) -> &str {
    match name.rfind(':') {
        Some(index) => &name[index + 1..],
        None => name,
    }
}

/// Expands the five predefined entities and numeric character references.
fn decode_entities(value: &str) -> Result<String, ManifestError> {
    if !value.contains('&') {
        return Ok(value.to_owned());
    }

    let mut decoded = String::with_capacity(value.len());
    let mut rest = value;

    while let Some(start) = rest.find('&') {
        decoded.push_str(&rest[..start]);
        let after = &rest[start + 1..];

        let Some(end) = after.find(';') else {
            // A bare `&` is tolerated; XML processors accept it in practice.
            decoded.push('&');
            rest = after;
            continue;
        };

        let entity = &after[..end];

        let replacement = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => decode_character_reference(entity)?,
        };

        match replacement {
            Some(character) => decoded.push(character),
            None => {
                decoded.push('&');
                decoded.push_str(entity);
                decoded.push(';');
            }
        }

        rest = &after[end + 1..];
    }

    decoded.push_str(rest);

    Ok(decoded)
}

fn decode_character_reference(entity: &str) -> Result<Option<char>, ManifestError> {
    let Some(digits) = entity.strip_prefix('#') else {
        return Ok(None);
    };

    let code = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).map_err(|_| ManifestError::Malformed)?,
        None => digits
            .parse::<u32>()
            .map_err(|_| ManifestError::Malformed)?,
    };

    Ok(char::from_u32(code))
}

#[cfg(test)]
mod tests {
    use super::{Node, parse_document};

    fn root(text: &str) -> Node {
        parse_document(text).expect("document must parse")
    }

    #[test]
    fn reads_nested_elements_and_attributes() {
        let node = root("<?xml version=\"1.0\"?><MPD type=\"static\"><Period id=\"0\"/></MPD>");

        assert_eq!(node.name(), "MPD");
        assert_eq!(node.attribute("type"), Some("static"));

        let period = node.child("Period").expect("period must exist");
        assert_eq!(period.attribute("id"), Some("0"));
    }

    #[test]
    fn local_names_ignore_namespace_prefixes() {
        let node = root("<mpd:MPD><mpd:Period cenc:pssh=\"x\"/></mpd:MPD>");

        assert_eq!(node.name(), "MPD");
        assert_eq!(
            node.child("Period")
                .and_then(|period| period.attribute("pssh")),
            Some("x")
        );
    }

    #[test]
    fn reads_element_text_and_ignores_surrounding_whitespace() {
        let node = root("<MPD><BaseURL>\n  https://cdn.example/v/\n</BaseURL></MPD>");

        assert_eq!(
            node.child("BaseURL").map(Node::text),
            Some("https://cdn.example/v/")
        );
    }

    #[test]
    fn decodes_entities_and_character_references() {
        let node = root("<MPD><BaseURL>a&amp;b&#47;c&#x2F;d</BaseURL></MPD>");

        assert_eq!(
            node.child("BaseURL").map(Node::text),
            Some("a&amp;b/c/d".replace("&amp;", "&").as_str())
        );
    }

    #[test]
    fn tolerates_comments_cdata_and_self_closing_tags() {
        let node = root(
            "<MPD><!-- a comment --><BaseURL><![CDATA[https://cdn/v/?a=1&b=2]]></BaseURL><X/></MPD>",
        );

        assert_eq!(
            node.child("BaseURL").map(Node::text),
            Some("https://cdn/v/?a=1&b=2")
        );
        assert!(node.child("X").is_some());
    }

    #[test]
    fn accepts_single_quoted_attributes() {
        let node = root("<MPD type='dynamic'/>");

        assert_eq!(node.attribute("type"), Some("dynamic"));
    }

    #[test]
    fn rejects_unbalanced_documents() {
        assert!(parse_document("<MPD><Period></MPD>").is_err());
        assert!(parse_document("<MPD>").is_err());
        assert!(parse_document("</MPD>").is_err());
        assert!(parse_document("<MPD/><Period/>").is_err());
    }

    #[test]
    fn rejects_malformed_attribute_lists() {
        assert!(parse_document("<MPD type=\"static").is_err());
        assert!(parse_document("<MPD =\"static\">").is_err());
        assert!(parse_document("<MPD type=\"a\" type=\"b\"/>").is_err());
    }

    #[test]
    fn rejects_deep_nesting() {
        let depth = super::MAX_DEPTH + 1;
        let text = format!("{}{}", "<A>".repeat(depth), "</A>".repeat(depth));

        assert!(parse_document(&text).is_err());
    }

    #[test]
    fn skips_the_doctype_and_xml_prolog() {
        let node = root("<?xml version='1.0' encoding='utf-8'?><!DOCTYPE MPD><MPD/>");

        assert_eq!(node.name(), "MPD");
    }
}
