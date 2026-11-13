//! What the JSON Schemas of crate::schema say about a key path: the keys a table takes, their descriptions, and the
//! values a key takes. The schemas follow the parser (their tests check it), so completion and hover offer exactly
//! what anvil.toml and tests.toml accept.

use super::scan::Part;
use serde_json::Value;
use std::sync::OnceLock;

/// A key a table takes, with its description.
pub struct KeyInfo {
    pub name: String,
    pub description: String,
}

/// Which file a schema is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Manifest,
    Tests,
}

pub fn schema(kind: FileKind) -> &'static Value {
    static MANIFEST: OnceLock<Value> = OnceLock::new();
    static TESTS: OnceLock<Value> = OnceLock::new();

    match kind {
        FileKind::Manifest => MANIFEST.get_or_init(crate::schema::manifest_schema),
        FileKind::Tests => TESTS.get_or_init(crate::schema::tests_schema),
    }
}

/// The schemas a path may be at (several where a value has alternatives: a dep's string or table).
pub fn nodes_at(kind: FileKind, path: &[Part]) -> Vec<&'static Value> {
    let root = schema(kind);
    let mut nodes = expand(root, root);

    for part in path {
        let mut next = Vec::new();
        for node in nodes {
            let child = match part {
                Part::Key(key) => match node.get("properties").and_then(|properties| properties.get(key)) {
                    Some(property) => Some(property),
                    None => node.get("additionalProperties").filter(|value| value.is_object()),
                },
                Part::Index(_) => node.get("items"),
            };
            if let Some(child) = child {
                next.extend(expand(child, root));
            }
        }
        nodes = next;
    }

    nodes
}

/// The keys the table at `path` takes; empty for a table of free names (targets, deps, conditions).
pub fn keys_at(kind: FileKind, path: &[Part]) -> Vec<KeyInfo> {
    let mut keys: Vec<KeyInfo> = Vec::new();

    for node in nodes_at(kind, path) {
        let Some(properties) = node.get("properties").and_then(Value::as_object) else {
            continue;
        };
        for (name, property) in properties {
            if keys.iter().any(|key| key.name == *name) {
                continue;
            }
            keys.push(KeyInfo {
                name: name.clone(),
                description: description(property, schema(kind)),
            });
        }
    }

    keys
}

/// Whether the table at `path` takes keys of any name (its children are names: targets, deps, conditions).
pub fn takes_any_key(kind: FileKind, path: &[Part]) -> bool {
    nodes_at(kind, path)
        .iter()
        .any(|node| node.get("additionalProperties").is_some_and(Value::is_object))
}

/// The description of the key or value at `path`.
pub fn description_at(kind: FileKind, path: &[Part]) -> Option<String> {
    let root = schema(kind);
    let mut node = root;

    // Walk with the first alternative that has the part, keeping each step's own description
    for part in path {
        let candidates = expand(node, root);
        let child = candidates.iter().find_map(|candidate| match part {
            Part::Key(key) => candidate
                .get("properties")
                .and_then(|properties| properties.get(key))
                .or_else(|| candidate.get("additionalProperties").filter(|value| value.is_object())),
            Part::Index(_) => candidate.get("items"),
        })?;
        node = child;
    }

    let text = description(node, root);
    (!text.is_empty()).then_some(text)
}

/// The values a key takes when they are a closed set (kind, warnings, toolchain, true/false), as TOML text.
pub fn values_at(kind: FileKind, path: &[Part]) -> Vec<(String, String)> {
    let root = schema(kind);
    let mut values: Vec<(String, String)> = Vec::new();
    let mut add = |text: String, description: String| {
        if !values.iter().any(|(known, _)| *known == text) {
            values.push((text, description));
        }
    };

    for node in nodes_at(kind, path) {
        let own = description(node, root);
        if let Some(choices) = node.get("enum").and_then(Value::as_array) {
            for choice in choices {
                add(choice.to_string(), own.clone());
            }
        }
        if node.get("type").and_then(Value::as_str) == Some("boolean") {
            add("true".into(), own.clone());
            add("false".into(), own.clone());
        }
    }

    values
}

/// A schema with its `$ref`, `allOf` and `anyOf` taken apart into the alternatives it allows.
fn expand<'a>(node: &'a Value, root: &'a Value) -> Vec<&'a Value> {
    if let Some(reference) = node.get("$ref").and_then(Value::as_str) {
        let name = reference.trim_start_matches("#/definitions/");
        return match root.get("definitions").and_then(|definitions| definitions.get(name)) {
            Some(target) => expand(target, root),
            None => Vec::new(),
        };
    }

    let mut alternatives = vec![node];
    for key in ["allOf", "anyOf", "oneOf"] {
        for alternative in node.get(key).and_then(Value::as_array).into_iter().flatten() {
            alternatives.extend(expand(alternative, root));
        }
    }

    alternatives
}

fn description(node: &Value, root: &Value) -> String {
    if let Some(text) = node.get("description").and_then(Value::as_str) {
        return text.to_string();
    }

    expand(node, root)
        .iter()
        .find_map(|alternative| alternative.get("description").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::scan::keys;

    #[test]
    fn keys_and_values_by_path() {
        let names = |path: &[Part]| -> Vec<String> { keys_at(FileKind::Manifest, path).into_iter().map(|key| key.name).collect() };

        assert!(names(&keys(&["target", "app"])).contains(&"kind".to_string()));
        assert!(names(&keys(&["target", "app", "when", "windows"])).contains(&"libs".to_string()));
        assert!(!names(&keys(&["target", "app", "when", "windows"])).contains(&"dir".to_string()));
        assert!(!names(&keys(&["defaults"])).contains(&"kind".to_string()));
        assert!(names(&keys(&["deps", "fmt"])).contains(&"features".to_string()));
        assert!(names(&keys(&["project"])).contains(&"default-run".to_string()));
        assert!(
            names(&[Part::Key("target".into()), Part::Key("a".into()), Part::Key("run".into()), Part::Index(0)]).contains(&"for-each".to_string())
        );
        assert!(takes_any_key(FileKind::Manifest, &keys(&["target"])));
        assert!(!takes_any_key(FileKind::Manifest, &keys(&["project"])));

        let values: Vec<String> = values_at(FileKind::Manifest, &keys(&["target", "app", "kind"]))
            .into_iter()
            .map(|(text, _)| text)
            .collect();
        assert!(values.contains(&"\"exe\"".to_string()) && values.contains(&"\"shared\"".to_string()));
        let flags: Vec<String> = values_at(FileKind::Manifest, &keys(&["target", "app", "werror"]))
            .into_iter()
            .map(|(text, _)| text)
            .collect();
        assert_eq!(flags, vec!["true", "false"]);

        let tests: Vec<String> = keys_at(FileKind::Tests, &[Part::Key("test".into()), Part::Index(3)])
            .into_iter()
            .map(|key| key.name)
            .collect();
        assert!(tests.contains(&"after".to_string()) && tests.contains(&"matrix".to_string()));

        assert!(description_at(FileKind::Manifest, &keys(&["target", "x", "needs"]))
            .unwrap()
            .contains("built before"));
    }
}
