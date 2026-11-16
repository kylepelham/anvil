//! Editing JSON with comments (VS Code's and Zed's settings files) in place: the text outside what Anvil changes, the
//! user's entries and comments included, stays as it was.

use anyhow::{bail, Result};
use serde_json::Value;

// The indent of what Anvil writes, as VS Code writes its own files
const INDENT: &str = "    ";

// How deep objects and arrays may nest (as serde_json allows): deeper is refused, not a stack overflow
const MAX_DEPTH: usize = 128;

/// A value's place in the text, and for objects and arrays, what it holds.
#[derive(Debug)]
pub struct Node {
    pub start: usize,
    pub end: usize,
    pub kind: NodeKind,
}

#[derive(Debug)]
pub enum NodeKind {
    /// Members: the key, and the value.
    Object(Vec<(String, Node)>),
    /// Elements, each with the place of the comma after it (if any).
    Array(Vec<(Node, Option<usize>)>),
    String(String),
    Other,
}

struct Parser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    position: usize,
    depth: usize,
}

impl Node {
    /// The member `key` of an object: its last, when the key is there twice, as JSON parsers (VS Code's too) read it.
    pub fn member(&self, key: &str) -> Option<&Node> {
        match &self.kind {
            NodeKind::Object(members) => members.iter().rev().find(|(name, _)| name == key).map(|(_, value)| value),
            _ => None,
        }
    }

    /// The text of a string value.
    pub fn text(&self) -> Option<&str> {
        match &self.kind {
            NodeKind::String(text) => Some(text),
            _ => None,
        }
    }
}

/// Parses JSON with `//` and `/* */` comments and trailing commas, as VS Code reads its files.
pub fn parse(text: &str) -> Result<Node> {
    let mut parser = Parser {
        text,
        bytes: text.as_bytes(),
        position: 0,
        depth: 0,
    };

    let node = parser.value()?;
    parser.skip_trivia()?;
    if parser.position != parser.bytes.len() {
        bail!("unexpected text after the value at {}", parser.place());
    }

    Ok(node)
}

/// Replaces the elements of an array that `owned` picks with `entries`, where the first of them was (else at the end),
/// keeping the others and every comment. `path` names the array: members of the top object, or none for a top array.
/// A missing array is added to its object.
pub fn replace_entries(text: &str, path: &[&str], owned: &dyn Fn(&Node) -> bool, entries: &[Value]) -> Result<String> {
    let root = parse(text)?;

    // The array, or the object it goes into
    let mut node = &root;
    for (depth, key) in path.iter().enumerate() {
        match node.member(key) {
            Some(child) => node = child,
            None if matches!(node.kind, NodeKind::Object(_)) => {
                let value = Value::Array(entries.to_vec());
                let parents = &path[..depth];
                return set_member(text, parents, key, &value);
            }
            None => bail!("`{}` is not an object", path[..depth].join(".")),
        }
    }
    let NodeKind::Array(elements) = &node.kind else {
        match path.is_empty() {
            true => bail!("the file does not hold a list"),
            false => bail!("`{}` is not a list", path.join(".")),
        }
    };

    // The array's indent, and its elements'
    let array_indent = line_indent(text, node.start);
    let element_indent = match elements.first() {
        Some((first, _)) => line_indent(text, first.start),
        None => format!("{array_indent}{INDENT}"),
    };

    // Each kept element with what leads up to it; comments around a removed one stay
    let mut items: Vec<String> = Vec::new();
    let mut orphans = String::new();
    let mut insert_at = None;
    let mut previous_end = node.start + 1;
    for (element, comma) in elements {
        let leading = &text[previous_end..element.start];
        let trailing = &text[element.end..comma.unwrap_or(element.end)];
        previous_end = comma.map(|comma| comma + 1).unwrap_or(element.end);

        if owned(element) {
            insert_at.get_or_insert(items.len());
            for part in [leading, trailing] {
                if has_comment(part) {
                    orphans.push_str(part.trim_end());
                }
            }
            continue;
        }

        items.push(format!("{orphans}{leading}{}{trailing}", &text[element.start..element.end]));
        orphans.clear();
    }

    // Anvil's entries, then what followed the last element
    let new_items: Vec<String> = entries
        .iter()
        .map(|entry| format!("\n{element_indent}{}", indented(&pretty(entry), &element_indent)))
        .collect();
    let insert_at = insert_at.unwrap_or(items.len());
    items.splice(insert_at..insert_at, new_items);
    let mut tail = format!("{orphans}{}", &text[previous_end..node.end - 1]);
    if tail.trim().is_empty() {
        tail = format!("\n{array_indent}");
    }

    Ok(format!("{}[{}{tail}]{}", &text[..node.start], items.join(","), &text[node.end..]))
}

/// Sets `key` in the object at `path` (members of the top object) to `value`: replaced where it is, else added as its
/// last member. Objects on the path that are missing are added.
pub fn set_member(text: &str, path: &[&str], key: &str, value: &Value) -> Result<String> {
    let root = parse(text)?;

    // The deepest object of the path that exists; what is missing of the path wraps the value
    let mut node = &root;
    let mut depth = 0;
    while depth < path.len() {
        match node.member(path[depth]) {
            Some(child) if matches!(child.kind, NodeKind::Object(_)) => node = child,
            Some(_) => bail!("`{}` is not an object", path[..=depth].join(".")),
            None => break,
        }
        depth += 1;
    }
    let NodeKind::Object(members) = &node.kind else {
        bail!("the file does not hold an object");
    };
    let (key, value) = match path[depth..].split_first() {
        Some((first, rest)) => {
            let mut nested = serde_json::json!({ key: value });
            for name in rest.iter().rev() {
                nested = serde_json::json!({ *name: nested });
            }
            (*first, nested)
        }
        None => (key, value.clone()),
    };

    // Replace it in place (the last of a key that is there twice, the one that counts)
    let object_indent = line_indent(text, node.start);
    if let Some((_, existing)) = members.iter().rev().find(|(name, _)| name == key) {
        let indent = line_indent(text, existing.start);
        let replacement = indented(&pretty(&value), &indent);
        return Ok(format!("{}{replacement}{}", &text[..existing.start], &text[existing.end..]));
    }

    // Or add it after the last member, or as the only one
    let member_indent = match members.first() {
        Some((_, first)) if text[node.start..first.start].contains('\n') => line_indent(text, first.start),
        _ => format!("{object_indent}{INDENT}"),
    };
    let member = format!(
        "\n{member_indent}{}: {}",
        serde_json::to_string(key)?,
        indented(&pretty(&value), &member_indent)
    );
    let result = match members.last() {
        Some((_, last)) => {
            // After its comma when it has one (a trailing comma), else after a new one
            let after = &text[last.end..node.end - 1];
            match after.trim_start().starts_with(',') {
                true => {
                    let comma = last.end + after.find(',').unwrap();
                    format!("{}{member},{}", &text[..comma + 1], &text[comma + 1..])
                }
                false => format!("{},{member}{}", &text[..last.end], &text[last.end..]),
            }
        }
        None => {
            let inside = &text[node.start + 1..node.end - 1];
            let inside = if inside.trim().is_empty() { "" } else { inside.trim_end() };
            format!("{}{{{inside}{member}\n{object_indent}}}{}", &text[..node.start], &text[node.end..])
        }
    };

    Ok(result)
}

/// A value as Anvil writes it: pretty, four spaces.
pub fn pretty(value: &Value) -> String {
    let mut bytes = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(INDENT.as_bytes());
    let mut serializer = serde_json::Serializer::with_formatter(&mut bytes, formatter);
    serde::Serialize::serialize(value, &mut serializer).expect("JSON values always serialize");

    String::from_utf8(bytes).expect("serde_json writes UTF-8")
}

/// A whole new file holding `value`.
pub fn document(value: &Value) -> String {
    format!("{}\n", pretty(value))
}

/// The lines after the first moved right by `indent`, for a value written at that indent.
fn indented(text: &str, indent: &str) -> String {
    text.replace('\n', &format!("\n{indent}"))
}

/// The spaces and tabs that start the line `position` is on.
fn line_indent(text: &str, position: usize) -> String {
    let line_start = text[..position].rfind('\n').map(|newline| newline + 1).unwrap_or(0);
    text[line_start..]
        .chars()
        .take_while(|character| *character == ' ' || *character == '\t')
        .collect()
}

fn has_comment(text: &str) -> bool {
    text.contains("//") || text.contains("/*")
}

impl Parser<'_> {
    fn value(&mut self) -> Result<Node> {
        self.skip_trivia()?;
        let start = self.position;

        let kind = match self.bytes.get(start) {
            Some(b'{' | b'[') if self.depth == MAX_DEPTH => bail!("objects and lists nest deeper than {MAX_DEPTH} levels at {}", self.place()),
            Some(b'{') => self.nested(Self::object)?,
            Some(b'[') => self.nested(Self::array)?,
            Some(b'"') => NodeKind::String(self.string()?),
            Some(b'-' | b'0'..=b'9' | b't' | b'f' | b'n') => {
                while self
                    .bytes
                    .get(self.position)
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'+' | b'.'))
                {
                    self.position += 1;
                }
                let word = &self.text[start..self.position];
                if !matches!(word, "true" | "false" | "null") && word.parse::<f64>().is_err() {
                    bail!("`{word}` is not a JSON value at {}", self.place_of(start));
                }
                NodeKind::Other
            }
            Some(_) => bail!("expected a value at {}", self.place()),
            None => bail!("the file ends where a value should be"),
        };

        Ok(Node {
            start,
            end: self.position,
            kind,
        })
    }

    /// An object or array, one level deeper.
    fn nested(&mut self, parse: fn(&mut Self) -> Result<NodeKind>) -> Result<NodeKind> {
        self.depth += 1;
        let kind = parse(self);
        self.depth -= 1;
        kind
    }

    fn object(&mut self) -> Result<NodeKind> {
        self.position += 1;
        let mut members = Vec::new();

        loop {
            self.skip_trivia()?;
            match self.bytes.get(self.position) {
                Some(b'}') => {
                    self.position += 1;
                    return Ok(NodeKind::Object(members));
                }
                Some(b'"') => {}
                _ => bail!("expected a key or `}}` at {}", self.place()),
            }

            let key = self.string()?;
            self.skip_trivia()?;
            if self.bytes.get(self.position) != Some(&b':') {
                bail!("expected `:` after \"{key}\" at {}", self.place());
            }
            self.position += 1;
            let value = self.value()?;
            members.push((key, value));

            self.skip_trivia()?;
            match self.bytes.get(self.position) {
                Some(b',') => self.position += 1,
                Some(b'}') => {}
                _ => bail!("expected `,` or `}}` at {}", self.place()),
            }
        }
    }

    fn array(&mut self) -> Result<NodeKind> {
        self.position += 1;
        let mut elements = Vec::new();

        loop {
            self.skip_trivia()?;
            if self.bytes.get(self.position) == Some(&b']') {
                self.position += 1;
                return Ok(NodeKind::Array(elements));
            }

            let element = self.value()?;
            self.skip_trivia()?;
            match self.bytes.get(self.position) {
                Some(b',') => {
                    elements.push((element, Some(self.position)));
                    self.position += 1;
                }
                Some(b']') => elements.push((element, None)),
                _ => bail!("expected `,` or `]` at {}", self.place()),
            }
        }
    }

    fn string(&mut self) -> Result<String> {
        let start = self.position;
        self.position += 1;

        loop {
            match self.bytes.get(self.position) {
                Some(b'"') => break,
                Some(b'\\') => self.position += 2,
                Some(_) => self.position += 1,
                None => bail!("a string that starts at {} does not end", self.place_of(start)),
            }
        }
        self.position += 1;

        let literal = &self.text[start..self.position];
        serde_json::from_str(literal).map_err(|error| anyhow::anyhow!("bad string at {}: {error}", self.place_of(start)))
    }

    /// Whitespace and comments.
    fn skip_trivia(&mut self) -> Result<()> {
        loop {
            match (self.bytes.get(self.position), self.bytes.get(self.position + 1)) {
                (Some(byte), _) if byte.is_ascii_whitespace() => self.position += 1,
                (Some(b'/'), Some(b'/')) => {
                    while self.bytes.get(self.position).is_some_and(|byte| *byte != b'\n') {
                        self.position += 1;
                    }
                }
                (Some(b'/'), Some(b'*')) => {
                    let start = self.position;
                    match self.text[start + 2..].find("*/") {
                        Some(end) => self.position = start + 2 + end + 2,
                        None => bail!("a comment that starts at {} does not end", self.place_of(start)),
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn place(&self) -> String {
        self.place_of(self.position)
    }

    /// "line 3, column 7".
    fn place_of(&self, position: usize) -> String {
        let before = &self.text[..position.min(self.text.len())];
        let line = before.matches('\n').count() + 1;
        let column = before.len() - before.rfind('\n').map(|newline| newline + 1).unwrap_or(0) + 1;
        format!("line {line}, column {column}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ours(node: &Node) -> bool {
        node.member("label")
            .and_then(Node::text)
            .is_some_and(|label| label.starts_with("anvil: "))
    }

    #[test]
    fn parses_comments_and_trailing_commas() {
        let node = parse("// top\n{ /* a */ \"a\": [1, 2,], \"b\": \"x\\\"y\", }\n").unwrap();
        assert_eq!(node.member("b").and_then(Node::text), Some("x\"y"));
        assert!(matches!(&node.member("a").unwrap().kind, NodeKind::Array(elements) if elements.len() == 2));
        assert!(parse("{ \"a\": }").is_err());
        assert!(parse("{ \"a\": 1 } x").is_err());
        assert!(parse("{ /* open").is_err());
    }

    #[test]
    fn refuses_deep_nesting_instead_of_overflowing() {
        let deep = format!("{}{}", "[".repeat(3000), "]".repeat(3000));
        let error = parse(&deep).unwrap_err().to_string();
        assert!(error.contains("deeper than 128 levels"), "{error}");

        let allowed = format!("{}{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        assert!(parse(&allowed).is_ok());
    }

    #[test]
    fn a_key_given_twice_counts_as_its_last() {
        let text = "{ \"tasks\": [], \"tasks\": [ { \"label\": \"mine\" } ] }";
        let node = parse(text).unwrap();
        assert!(matches!(&node.member("tasks").unwrap().kind, NodeKind::Array(elements) if elements.len() == 1));

        // Anvil's entries go into the list VS Code reads, and a setting is replaced where it counts
        let merged = replace_entries(text, &["tasks"], &ours, &[json!({ "label": "anvil: build" })]).unwrap();
        assert!(merged.starts_with("{ \"tasks\": [], \"tasks\": [ { \"label\": \"mine\" },"));
        assert_eq!(
            set_member("{ \"a\": 1, \"a\": 2 }", &[], "a", &json!(3)).unwrap(),
            "{ \"a\": 1, \"a\": 3 }"
        );
    }

    #[test]
    fn replaces_only_owned_entries_and_keeps_comments() {
        let text = r#"{
    // My tasks
    "version": "2.0.0",
    "tasks": [
        // Mine, keep it
        { "label": "mine", "command": "make" },
        { "label": "anvil: old", "command": "anvil" },
        { "label": "anvil: older", "command": "anvil" },
        /* after */
        { "label": "also mine" }, // trailing note
    ]
}
"#;
        let entries = [json!({ "label": "anvil: build" })];
        let merged = replace_entries(text, &["tasks"], &ours, &entries).unwrap();

        assert!(merged.contains("// My tasks"));
        assert!(merged.contains("// Mine, keep it"));
        assert!(merged.contains("/* after */"));
        assert!(merged.contains("// trailing note"));
        assert!(merged.contains(r#"{ "label": "mine", "command": "make" }"#));
        assert!(!merged.contains("anvil: old"));
        assert!(merged.find("\"mine\"").unwrap() < merged.find("anvil: build").unwrap());
        assert!(merged.find("anvil: build").unwrap() < merged.find("also mine").unwrap());

        // The result parses, and merging again changes nothing
        let labels: Vec<String> = match parse(&merged).unwrap().member("tasks").unwrap().kind {
            NodeKind::Array(ref elements) => elements
                .iter()
                .map(|(element, _)| element.member("label").and_then(Node::text).unwrap().to_string())
                .collect(),
            _ => panic!(),
        };
        assert_eq!(labels, ["mine", "anvil: build", "also mine"]);
        assert_eq!(replace_entries(&merged, &["tasks"], &ours, &entries).unwrap(), merged);
    }

    #[test]
    fn adds_a_missing_list_and_fills_an_empty_one() {
        let added = replace_entries(
            "{\n    \"version\": \"0.2.0\"\n}\n",
            &["configurations"],
            &ours,
            &[json!({ "label": "anvil: a" })],
        )
        .unwrap();
        assert_eq!(
            added,
            "{\n    \"version\": \"0.2.0\",\n    \"configurations\": [\n        {\n            \"label\": \"anvil: a\"\n        }\n    ]\n}\n"
        );

        let filled = replace_entries("{ \"tasks\": [] }", &["tasks"], &ours, &[json!(1)]).unwrap();
        assert_eq!(filled, "{ \"tasks\": [\n    1\n] }");

        // A top-level array (Zed's files)
        let top = replace_entries("[\n  { \"label\": \"anvil: x\" }\n]\n", &[], &ours, &[json!(2)]).unwrap();
        assert_eq!(top, "[\n  2\n]\n");
    }

    #[test]
    fn sets_members_in_place_or_at_the_end() {
        let text = "{\n    // keep\n    \"a\": 1,\n    \"b\": { \"x\": 1 }\n}\n";
        assert_eq!(
            set_member(text, &[], "a", &json!(2)).unwrap(),
            "{\n    // keep\n    \"a\": 2,\n    \"b\": { \"x\": 1 }\n}\n"
        );
        assert_eq!(
            set_member(text, &["b"], "y", &json!(2)).unwrap(),
            "{\n    // keep\n    \"a\": 1,\n    \"b\": { \"x\": 1,\n        \"y\": 2 }\n}\n"
        );
        assert_eq!(
            set_member(text, &[], "c", &json!("z")).unwrap(),
            "{\n    // keep\n    \"a\": 1,\n    \"b\": { \"x\": 1 },\n    \"c\": \"z\"\n}\n"
        );
        assert_eq!(
            set_member("{}", &["n"], "k", &json!(1)).unwrap(),
            "{\n    \"n\": {\n        \"k\": 1\n    }\n}"
        );
        assert_eq!(set_member("{ \"a\": 1, }", &[], "b", &json!(2)).unwrap(), "{ \"a\": 1,\n    \"b\": 2, }");
    }
}
