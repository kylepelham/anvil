//! `textDocument/completion`: table and key names where a key goes (from the schema, so only what the parser
//! takes), closed values (kind, warnings, toolchain, ...), condition words in `when` keys and `if` values, target
//! and dependency names where they are named, `{target:...}` and the other placeholders in texts, profiles, presets,
//! tests and vcpkg ports; snippets of whole tables where a line may start one.

use super::scan::{self, Cursor, Part, Place, TokenKind};
use super::schema_info::{self, FileKind};
use super::view::{self, View, WordKind};
use std::collections::HashMap;
use std::ops::Range;

// LSP CompletionItemKind values
pub const KIND_VARIABLE: u32 = 6;
pub const KIND_CLASS: u32 = 7;
pub const KIND_MODULE: u32 = 9;
pub const KIND_PROPERTY: u32 = 10;
pub const KIND_VALUE: u32 = 12;
pub const KIND_KEYWORD: u32 = 14;
pub const KIND_SNIPPET: u32 = 15;
pub const KIND_REFERENCE: u32 = 18;
pub const KIND_ENUM_MEMBER: u32 = 20;
pub const KIND_CONSTANT: u32 = 21;

// Ports offered at most at once (vcpkg has thousands)
const MAX_PORTS: usize = 200;

// Placeholders of texts, and those only build steps take
const PLACEHOLDERS: &[(&str, &str)] = &[
    ("root", "The package root."),
    ("out", "The profile's output folder (out/<profile>)."),
    ("gen", "The target's generated-files folder (on its include path)."),
    ("name", "The target's name (a matrix test's name in tests)."),
    ("profile", "The profile's name."),
    ("config", "The profile's name as CMake writes configurations (Debug, RelWithDebInfo)."),
    ("version", "[project] version."),
    ("vcpkg", "The vcpkg triplet's install folder."),
    ("cxx", "The C++ compiler's path."),
    ("cc", "The C compiler's path."),
    ("target:", "A target's file (a step waits for it): {target:app}, {target:pkg/tool}."),
    (
        "hash:",
        "16 hex digits that change when the listed files, `compiler` or `flags` do (in defines and flags).",
    ),
];
const STEP_PLACEHOLDERS: &[(&str, &str)] = &[
    ("input", "The file `for-each` runs the command for."),
    ("stem", "The input's name without its extension."),
    ("filename", "The input's file name."),
    ("inputs", "The step's inputs."),
    ("output", "The step's output."),
    ("outputs", "The step's outputs."),
    ("depfile", "The depfile the command writes (depfile = true)."),
];

/// A snippet in LSP snippet syntax (`${1:name}`, `${2|a,b|}`, `$0`; `\}` is a brace inside a placeholder).
struct Snippet {
    label: &'static str,
    detail: &'static str,
    body: &'static str,
    /// Offered in tests.toml too, which holds only [[test]] tables.
    tests: bool,
    /// An entry of [deps] or [dev-deps] rather than a table: offered only there.
    dependency: bool,
}

const SNIPPETS: &[Snippet] = &[
    Snippet {
        label: "[target.name]",
        detail: "A target: program, library, test...",
        body: "[target.${1:name}]\nkind = \"${2|exe,lib,shared,object,headers,test,custom|}\"\nsources = [\"${3:src/**/*.cpp}\"]\ndeps = [$4]\n$0",
        tests: false,
        dependency: false,
    },
    Snippet {
        label: "[target.name] (program)",
        detail: "A program with its own folder",
        body: "[target.${1:app}]\nkind = \"exe\"\ndir = \"${2:$1}\"\ndeps = [$3]\n$0",
        tests: false,
        dependency: false,
    },
    Snippet {
        label: "[target.name] (library)",
        detail: "A static library with public headers in include/",
        body: "[target.${1:core}]\nkind = \"lib\"\ndir = \"${2:$1}\"\n\n[target.$1.public]\ninclude = [\"include\"]\ndeps = [$3]\n$0",
        tests: false,
        dependency: false,
    },
    Snippet {
        label: "[deps]",
        detail: "Dependencies: vcpkg ports, git, path, pkg-config, system",
        body: "[deps]\n${1:fmt} = \"${2|vcpkg,system,pkg-config|}\"\n$0",
        tests: false,
        dependency: false,
    },
    Snippet {
        label: "git dependency",
        detail: "A dependency fetched with git",
        body: "${1:name} = { git = \"${2:https://github.com/owner/repo}\", tag = \"${3:v1.0.0}\" }\n$0",
        tests: false,
        dependency: true,
    },
    Snippet {
        label: "[dev-deps]",
        detail: "Dependencies of the tests only",
        body: "[dev-deps]\n${1:gtest} = \"vcpkg\"\n$0",
        tests: false,
        dependency: false,
    },
    Snippet {
        label: "[[test]]",
        detail: "A test that runs a command",
        body: "[[test]]\nname = \"${1:name}\"\ncommand = [\"{target:${2:app}}\"${3:, \"--arg\"}]\n$0",
        tests: true,
        dependency: false,
    },
    Snippet {
        label: "[[test]] with expect",
        detail: "A test judged by its output",
        body: "[[test]]\nname = \"${1:name}\"\ncommand = [\"{target:${2:app}}\"]\nexpect = [\"${3:regex}\"]\nexit-code = ${4:0}\n$0",
        tests: true,
        dependency: false,
    },
    Snippet {
        label: "[profile.name]",
        detail: "A build profile",
        body: "[profile.${1:shipping}]\ninherits = \"${2|release,debug|}\"\n${3:lto = true}\n$0",
        tests: false,
        dependency: false,
    },
    Snippet {
        label: "[options]",
        detail: "Build options (-o name=true|false)",
        body: "[options]\n${1:feature} = { default = ${2|true,false|}, description = \"${3:What it turns on}\" }\n$0",
        tests: false,
        dependency: false,
    },
    Snippet {
        label: "[[target.name.run]]",
        detail: "A build step that writes files",
        body: "[[target.${1:app}.run]]\nfor-each = \"${2:shaders/*.hlsl}\"\noutput = \"{gen}/${3:{stem\\}.h}\"\ncommand = \"${4:tool {input\\} -o {output\\}}\"\n$0",
        tests: false,
        dependency: false,
    },
];

#[derive(Debug, Clone)]
pub struct Item {
    pub label: String,
    pub kind: u32,
    pub detail: Option<String>,
    pub documentation: Option<String>,
    /// The span replaced, and the text written there.
    pub edit: (Range<usize>, String),
    /// The text is a snippet (crate::lsp::completion::plain_text turns it into plain text).
    pub snippet: bool,
}

pub fn complete(view: &View, offset: usize) -> Vec<Item> {
    let file = view.document();
    let cursor = scan::cursor(&file.text, offset);

    let mut items = match cursor.place {
        Place::Header => header_items(view, &cursor, offset),
        Place::Key => key_items(view, &cursor, offset),
        Place::Value => value_items(view, &cursor, offset),
        Place::Other => Vec::new(),
    };
    items.extend(snippet_items(view, &cursor, offset));

    items
}

/// A snippet as plain text, for clients without snippets: each placeholder's default, a choice's first option, the
/// tab stops left out.
pub fn plain_text(snippet: &str) -> String {
    expand_snippet(snippet, &mut HashMap::new())
}

/// `[` and `[[` lines: tables, then the names under them.
fn header_items(view: &View, cursor: &Cursor, offset: usize) -> Vec<Item> {
    let file = view.document();
    let replace = token_span(cursor, offset);

    if cursor.path.last() == Some(&Part::Key("when".into())) {
        return word_items(view, replace, true);
    }

    let mut items = Vec::new();
    for key in schema_info::keys_at(file.kind, &cursor.path) {
        let wanted = !cursor.array_header || ["test", "write", "run", "pre-build", "post-build", "file"].contains(&key.name.as_str());
        if wanted {
            items.push(item(
                &key.name,
                KIND_MODULE,
                None,
                Some(key.description),
                replace.clone(),
                quote_key(&key.name),
            ));
        }
    }

    // Names declared already: `[target.` offers the targets, for their when blocks and steps
    if schema_info::takes_any_key(file.kind, &cursor.path) {
        for name in file.scan.children(&cursor.path) {
            items.push(item(&name, KIND_CLASS, None, None, replace.clone(), quote_key(&name)));
        }
    }

    items
}

/// Where a key goes: the table's keys not written yet, condition words under `when`, ports under [deps].
fn key_items(view: &View, cursor: &Cursor, offset: usize) -> Vec<Item> {
    let file = view.document();
    let replace = token_span(cursor, offset);
    let typed = cursor.token.as_ref().map(|token| token.text.as_str()).unwrap_or_default();

    if cursor.path.last() == Some(&Part::Key("when".into())) {
        return word_items(view, replace, true);
    }

    // [deps] and [dev-deps]: vcpkg ports by the name typed so far
    if file.kind == FileKind::Manifest
        && cursor.path.len() == 1
        && matches!(&cursor.path[0], Part::Key(table) if table == "deps" || table == "dev-deps")
    {
        return port_items(view, typed, replace, |port| format!("{} = \"vcpkg\"", quote_key(port)));
    }

    let present = file.scan.children(&cursor.path);
    schema_info::keys_at(file.kind, &cursor.path)
        .into_iter()
        .filter(|key| key.name == typed || !present.contains(&key.name))
        .map(|key| {
            item(
                &key.name,
                KIND_PROPERTY,
                None,
                Some(key.description),
                replace.clone(),
                quote_key(&key.name),
            )
        })
        .collect()
}

/// Where a value goes: by what the key names.
fn value_items(view: &View, cursor: &Cursor, offset: usize) -> Vec<Item> {
    let file = view.document();
    let key = view::last_key(&cursor.path).unwrap_or_default();
    let string = cursor.token.as_ref().filter(|token| token.kind == TokenKind::String);

    // Inside a string: placeholders, then names by key
    if let Some(token) = string {
        // A position on a closing quote completes the content before it
        let offset = offset.clamp(token.content.start, token.content.end);
        let typed = &file.text[token.content.start..offset];
        if let Some(items) = placeholder_items(view, cursor, token.content.start, typed, offset) {
            return items;
        }

        let whole = token.content.clone();
        return match key {
            _ if view::names_dependency(&cursor.path) => name_items(view, dependency_names(view, &cursor.path), whole, false),
            "default-run" => name_items(view, executables(view), whole, false),
            "if" => {
                let start = offset - typed.len() + typed.rfind(|character: char| !is_word_character(character)).map_or(0, |index| index + 1);
                let end = offset
                    + file.text[offset..token.content.end]
                        .find(|character: char| !is_word_character(character))
                        .unwrap_or(token.content.end - offset);
                word_items(view, start..end, false)
            }
            "inherits" => {
                let own = match cursor.path.get(1) {
                    Some(Part::Key(name)) => name.clone(),
                    _ => String::new(),
                };
                let profiles: Vec<(String, String)> = view
                    .profiles()
                    .into_iter()
                    .filter(|profile| *profile != own)
                    .map(|profile| (profile, "profile".into()))
                    .collect();
                name_items(view, profiles, whole, false)
            }
            "preset" => {
                let presets = file
                    .scan
                    .children(&scan::keys(&["preset"]))
                    .into_iter()
                    .map(|name| (name, "preset".into()))
                    .collect();
                name_items(view, presets, whole, false)
            }
            "after" => {
                let tests = view.tests_of(view.package()).into_iter().map(|name| (name, "test".into())).collect();
                name_items(view, tests, whole, false)
            }
            "target" if is_dependency_entry(&cursor.path) => {
                let package = match &cursor.path[1] {
                    Part::Key(name) => view.dependency_package(file, name).unwrap_or_default(),
                    Part::Index(_) => String::new(),
                };
                let targets = view
                    .targets_of(&package)
                    .into_iter()
                    .map(|name| (name, format!("target of `{package}`")))
                    .collect();
                name_items(view, targets, whole, false)
            }
            "vcpkg" if is_dependency_entry(&cursor.path) => port_items(view, typed, whole, |port| port.to_string()),
            _ => schema_values(file.kind, &cursor.path, whole, true),
        };
    }

    // No quotes yet: whole values
    let replace = token_span(cursor, offset);
    if view::names_dependency(&cursor.path) && matches!(cursor.path.last(), Some(Part::Index(_))) {
        return name_items(view, dependency_names(view, &cursor.path), replace, true);
    }

    schema_values(file.kind, &cursor.path, replace, false)
}

/// `{` typed in a text: the placeholders; `{target:` the targets. None when the cursor is not in a placeholder.
fn placeholder_items(view: &View, cursor: &Cursor, content_start: usize, typed: &str, offset: usize) -> Option<Vec<Item>> {
    let open = typed.rfind('{')?;
    if typed[open..].contains('}') {
        return None;
    }

    let inside = &typed[open + 1..];
    let start = content_start + open + 1;
    let file = view.document();
    let closed = file.text[offset..].starts_with('}');

    // `{target:` names a target; another package's with its name in front
    if let Some(name) = inside.strip_prefix("target:") {
        let replace = start + "target:".len()..offset;
        let _ = name;
        let targets = qualified_targets(view);
        return Some(
            targets
                .into_iter()
                .map(|(name, detail)| {
                    let text = if closed { name.clone() } else { format!("{name}}}") };
                    item(&name, KIND_REFERENCE, Some(detail), None, replace.clone(), text)
                })
                .collect(),
        );
    }

    // In a build step, the step's own placeholders too
    let in_step = cursor
        .path
        .iter()
        .any(|part| matches!(part, Part::Key(key) if key == "run" || key == "pre-build" || key == "post-build"));
    let steps = if in_step { STEP_PLACEHOLDERS } else { &[] };
    let replace = start..offset;

    Some(
        PLACEHOLDERS
            .iter()
            .chain(steps)
            .map(|(name, description)| {
                let text = if name.ends_with(':') || closed {
                    name.to_string()
                } else {
                    format!("{name}}}")
                };
                item(name, KIND_CONSTANT, None, Some(description.to_string()), replace.clone(), text)
            })
            .collect(),
    )
}

/// The names a target's `deps` may hold: its package's other targets and [deps], member packages, other packages'
/// targets.
fn dependency_names(view: &View, path: &[Part]) -> Vec<(String, String)> {
    let package = view.package();
    let own = match path {
        [Part::Key(table), Part::Key(name), ..] if table == "target" => name.as_str(),
        _ => "",
    };
    let mut names: Vec<(String, String)> = Vec::new();

    for target in view.targets_of(package).into_iter().filter(|target| target != own) {
        names.push((target, "target".into()));
    }
    for dependency in view.dependencies_of(package) {
        names.push((dependency, "dependency".into()));
    }
    for (name, detail) in qualified_targets(view) {
        if name.contains('/') {
            names.push((name, detail));
        }
    }
    for other in view.packages() {
        if other != package && !names.iter().any(|(known, _)| *known == other) {
            names.push((other, "package".into()));
        }
    }

    names
}

/// Every target of the project: the document's package's by name, the others' as `package/target`.
fn qualified_targets(view: &View) -> Vec<(String, String)> {
    let own = view.package().to_string();
    let mut names: Vec<(String, String)> = Vec::new();

    for package in view.packages() {
        for target in view.targets_of(&package) {
            let kind = view
                .model
                .and_then(|model| model.targets.iter().find(|info| info.package == package && info.name == target))
                .map(|info| info.kind)
                .unwrap_or("target");
            let name = if package == own { target } else { format!("{package}/{target}") };
            if !names.iter().any(|(known, _)| *known == name) {
                names.push((name, kind.to_string()));
            }
        }
    }

    names
}

fn executables(view: &View) -> Vec<(String, String)> {
    let package = view.package();
    let model_targets = view.model.map(|model| model.targets.as_slice()).unwrap_or_default();

    view.targets_of(package)
        .into_iter()
        .filter(|name| {
            model_targets
                .iter()
                .find(|target| target.package == package && target.name == *name)
                .is_none_or(|target| target.kind == "exe")
        })
        .map(|name| (name, "program".into()))
        .collect()
}

fn name_items(_view: &View, names: Vec<(String, String)>, replace: Range<usize>, quoted: bool) -> Vec<Item> {
    names
        .into_iter()
        .map(|(name, detail)| {
            let text = if quoted { format!("{name:?}") } else { escape(&name) };
            item(&name, KIND_REFERENCE, Some(detail), None, replace.clone(), text)
        })
        .collect()
}

fn word_items(view: &View, replace: Range<usize>, as_key: bool) -> Vec<Item> {
    view.condition_words()
        .into_iter()
        .map(|(word, kind)| {
            let (detail, item_kind) = match kind {
                WordKind::Platform => ("platform", KIND_ENUM_MEMBER),
                WordKind::Compiler => ("compiler", KIND_ENUM_MEMBER),
                WordKind::Constant => ("constant", KIND_KEYWORD),
                WordKind::Profile => ("profile", KIND_CLASS),
                WordKind::Option => ("option", KIND_VARIABLE),
            };
            let text = if as_key { quote_key(&word) } else { word.clone() };
            item(&word, item_kind, Some(detail.into()), None, replace.clone(), text)
        })
        .collect()
}

fn port_items(view: &View, typed: &str, replace: Range<usize>, text: impl Fn(&str) -> String) -> Vec<Item> {
    if typed.is_empty() {
        return Vec::new();
    }

    view.ports
        .iter()
        .filter(|port| port.starts_with(typed))
        .take(MAX_PORTS)
        .map(|port| item(port, KIND_MODULE, Some("vcpkg port".into()), None, replace.clone(), text(port)))
        .collect()
}

/// The schema's values for a key; inside quotes only the texts, without their quotes.
fn schema_values(kind: FileKind, path: &[Part], replace: Range<usize>, in_string: bool) -> Vec<Item> {
    schema_info::values_at(kind, path)
        .into_iter()
        .filter_map(|(value, description)| {
            let text = match (in_string, value.strip_prefix('"').and_then(|rest| rest.strip_suffix('"'))) {
                (true, Some(inner)) => inner.to_string(),
                (true, None) => return None,
                (false, _) => value.clone(),
            };
            let label = text.trim_matches('"').to_string();
            Some(item(&label, KIND_VALUE, None, Some(description), replace.clone(), text))
        })
        .collect()
}

/// Snippets where a line may start a table: the line so far is empty, a bare word or a partial header. In tests.toml
/// only [[test]] ones; under [deps] and [dev-deps] the entry ones too.
fn snippet_items(view: &View, cursor: &Cursor, offset: usize) -> Vec<Item> {
    if !matches!(cursor.place, Place::Header | Place::Key) {
        return Vec::new();
    }

    let file = view.document();
    let line_start = file.text[..offset].rfind('\n').map_or(0, |index| index + 1);
    let typed = file.text[line_start..offset].trim_start();
    let partial = typed
        .trim_start_matches('[')
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "_.- ".contains(character));
    if !partial {
        return Vec::new();
    }

    let in_dependencies =
        cursor.place == Place::Key && cursor.path.len() == 1 && matches!(&cursor.path[0], Part::Key(table) if table == "deps" || table == "dev-deps");
    let replace = offset - typed.len()..offset;

    SNIPPETS
        .iter()
        .filter(|snippet| match file.kind {
            FileKind::Tests => snippet.tests,
            FileKind::Manifest => !snippet.dependency || in_dependencies,
        })
        .map(|snippet| Item {
            snippet: true,
            ..item(
                snippet.label,
                KIND_SNIPPET,
                Some(snippet.detail.into()),
                None,
                replace.clone(),
                snippet.body.into(),
            )
        })
        .collect()
}

/// Expands a snippet into plain text; `defaults` holds each tab stop's text, for the `$N` that repeat it.
fn expand_snippet(snippet: &str, defaults: &mut HashMap<String, String>) -> String {
    let mut text = String::new();
    let mut rest = snippet;

    while let Some(index) = rest.find(['$', '\\']) {
        text.push_str(&rest[..index]);
        let marker = &rest[index..];

        // An escaped character stands for itself
        if let Some(escaped) = marker.strip_prefix('\\') {
            let length = escaped.chars().next().map_or(0, char::len_utf8);
            text.push_str(&escaped[..length]);
            rest = &escaped[length..];
            continue;
        }

        // `$N`: the text of tab stop N so far
        let after = &marker[1..];
        let digits = after.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 {
            text.push_str(defaults.get(&after[..digits]).map(String::as_str).unwrap_or_default());
            rest = &after[digits..];
            continue;
        }

        // `${N:default}` and `${N|first,second|}`
        let body = after
            .strip_prefix('{')
            .and_then(|inner| closing_brace(inner).map(|end| (&inner[..end], &inner[end + 1..])));
        let Some((body, remainder)) = body else {
            text.push('$');
            rest = after;
            continue;
        };
        let digits = body.chars().take_while(char::is_ascii_digit).count();
        let number = body[..digits].to_string();
        let value = match &body[digits..] {
            choices if choices.starts_with('|') => choices.trim_matches('|').split(',').next().unwrap_or_default().to_string(),
            default if default.starts_with(':') => expand_snippet(&default[1..], defaults),
            _ => defaults.get(&number).cloned().unwrap_or_default(),
        };
        text.push_str(&value);
        defaults.entry(number).or_insert(value);
        rest = remainder;
    }
    text.push_str(rest);

    text
}

/// Where the `}` closing a `${` is, in the text after the `{`: nested `${...}` and escapes skipped.
fn closing_brace(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0;
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 1,
            b'$' if bytes.get(index + 1) == Some(&b'{') => {
                depth += 1;
                index += 1;
            }
            b'}' if depth == 0 => return Some(index),
            b'}' => depth -= 1,
            _ => {}
        }
        index += 1;
    }

    None
}

fn item(label: &str, kind: u32, detail: Option<String>, documentation: Option<String>, replace: Range<usize>, text: String) -> Item {
    Item {
        label: label.to_string(),
        kind,
        detail,
        documentation: documentation.filter(|text| !text.is_empty()),
        edit: (replace, text),
        snippet: false,
    }
}

/// The span the token at the cursor covers (what an item replaces), else an empty one at the cursor.
fn token_span(cursor: &Cursor, offset: usize) -> Range<usize> {
    match &cursor.token {
        Some(token) if token.kind == TokenKind::String => token.content.clone(),
        Some(token) => token.span.start..token.span.end.max(offset),
        None => offset..offset,
    }
}

/// `[deps.x]` or `[dev-deps.x]` keys: `target`, `vcpkg`.
fn is_dependency_entry(path: &[Part]) -> bool {
    path.len() == 3 && matches!(&path[0], Part::Key(table) if table == "deps" || table == "dev-deps")
}

/// A key as TOML writes it: bare when it can be, else quoted.
pub fn quote_key(key: &str) -> String {
    let bare = !key.is_empty() && key.chars().all(|character| character.is_ascii_alphanumeric() || "-_".contains(character));
    if bare {
        key.to_string()
    } else {
        format!("\"{}\"", escape(key))
    }
}

/// Text for inside a basic string.
pub fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

fn is_word_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || "_-.+".contains(character)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::locate::SourceFile;
    use std::path::PathBuf;

    fn labels(text: &str, ports: &[String]) -> Vec<String> {
        let offset = text.find('|').unwrap();
        let text = text.replace('|', "");
        let file = SourceFile::new(PathBuf::from("C:/p/anvil.toml"), text, "game".into());
        let view = View {
            files: vec![&file],
            model: None,
            ports,
        };
        complete(&view, offset)
            .into_iter()
            .filter(|item| !item.snippet)
            .map(|item| item.label)
            .collect()
    }

    const BASE: &str = "[project]\nname = \"game\"\n[deps]\nfmt = \"vcpkg\"\n[options]\nfast = true\n[profile.ship]\ninherits = \"release\"\n[target.core]\nkind = \"lib\"\n";

    #[test]
    fn contexts() {
        let at = |tail: &str| labels(&format!("{BASE}{tail}"), &["fmt".to_string(), "fmtlog".to_string(), "zlib".to_string()]);

        // Keys of a target, without those written; tables in headers
        let keys = at("[target.app]\nkind = \"exe\"\nde|");
        assert!(keys.contains(&"deps".to_string()) && keys.contains(&"defines".to_string()));
        assert!(!keys.contains(&"kind".to_string()));
        assert!(at("[ta|").contains(&"target".to_string()));
        assert!(at("[target.|").contains(&"core".to_string()));

        // Values: kinds, targets and dependencies, conditions, profiles
        assert!(at("[target.app]\nkind = \"|\"").contains(&"shared".to_string()));
        let deps = at("[target.app]\ndeps = [\"|\"]");
        assert!(deps.contains(&"core".to_string()) && deps.contains(&"fmt".to_string()));
        assert!(!at("[target.core]\ndeps = [\"|\"]").contains(&"core".to_string()));
        let words = at("[target.app]\nif = \"windows & f|\"");
        assert!(words.contains(&"fast".to_string()) && words.contains(&"ship".to_string()) && words.contains(&"msvc".to_string()));
        assert!(at("[target.app.when.|]").contains(&"linux".to_string()));
        assert!(at("[target.app]\nwhen.|").contains(&"fast".to_string()));
        assert!(at("[profile.fast2]\ninherits = \"|\"").contains(&"ship".to_string()));

        // Placeholders and targets in commands
        let placeholders = at("[[target.app.run]]\ncommand = [\"x\", \"{|\"]");
        assert!(placeholders.contains(&"target:".to_string()) && placeholders.contains(&"output".to_string()));
        assert!(at("[[test]]\nname = \"t\"\ncommand = \"{target:|\"").contains(&"core".to_string()));

        // vcpkg ports under [deps], by the name typed
        assert_eq!(at("[deps]\nfm|"), vec!["fmt", "fmtlog"]);
    }

    fn items(path: &str, text: &str) -> Vec<Item> {
        let offset = text.find('|').unwrap();
        let file = SourceFile::new(PathBuf::from(path), text.replace('|', ""), "game".into());
        let view = View {
            files: vec![&file],
            model: None,
            ports: &[],
        };
        complete(&view, offset)
    }

    fn snippet_labels(path: &str, text: &str) -> Vec<String> {
        items(path, text).into_iter().filter(|item| item.snippet).map(|item| item.label).collect()
    }

    #[test]
    fn snippets_where_a_table_may_start() {
        let manifest = "C:/p/anvil.toml";

        // An empty line, a word, a partial header: the table snippets, replacing what was typed
        let empty = snippet_labels(manifest, &format!("{BASE}\n|"));
        assert!(empty.contains(&"[target.name]".to_string()) && empty.contains(&"[[test]]".to_string()));
        assert!(!empty.contains(&"git dependency".to_string()));
        let header = items(manifest, &format!("{BASE}\n  [[te|"));
        let test = header.iter().find(|item| item.label == "[[test]]").unwrap();
        assert_eq!(test.kind, KIND_SNIPPET);
        assert_eq!(test.edit.0, BASE.len() + 3..BASE.len() + 7);
        assert!(test.edit.1.starts_with("[[test]]\nname = \"${1:name}\""));
        assert!(snippet_labels(manifest, &format!("{BASE}\npro|")).contains(&"[profile.name]".to_string()));

        // Under [deps], the entry snippets too; none in values or after a key
        assert!(snippet_labels(manifest, &format!("{BASE}[deps]\ngi|")).contains(&"git dependency".to_string()));
        assert!(snippet_labels(manifest, &format!("{BASE}[target.app]\nkind = \"|\"")).is_empty());
        assert!(snippet_labels(manifest, &format!("{BASE}[target.app]\nkind = |")).is_empty());

        // tests.toml: only [[test]] ones
        let tests = snippet_labels("C:/p/tests/tests.toml", "|");
        assert_eq!(tests, vec!["[[test]]", "[[test]] with expect"]);
    }

    #[test]
    fn snippets_as_plain_text() {
        assert_eq!(
            plain_text("[target.${1:app}]\ndir = \"${2:$1}\"\ndeps = [$3]\n$0"),
            "[target.app]\ndir = \"app\"\ndeps = []\n"
        );
        assert_eq!(plain_text("kind = \"${2|exe,lib|}\""), "kind = \"exe\"");
        assert_eq!(plain_text("output = \"{gen}/${3:{stem\\}.h}\""), "output = \"{gen}/{stem}.h\"");
        assert_eq!(
            plain_text("command = [\"{target:${2:app}}\"${3:, \"--arg\"}]"),
            "command = [\"{target:app}\", \"--arg\"]"
        );
        assert_eq!(plain_text("cost: $ 5"), "cost: $ 5");
    }

    #[test]
    fn snippets_are_valid_files() {
        // Each snippet, as written with its defaults, is what the parser takes
        for snippet in SNIPPETS {
            let text = plain_text(snippet.body);
            let parsed = match (snippet.tests, snippet.dependency) {
                (true, _) => crate::manifest::parse_tests(&text).map(|_| ()),
                (false, true) => crate::manifest::parse_str(&format!("[project]\nname = \"p\"\n[deps]\n{text}")).map(|_| ()),
                (false, false) => crate::manifest::parse_str(&format!("[project]\nname = \"p\"\n{text}")).map(|_| ()),
            };
            assert!(parsed.is_ok(), "{}: {:#}\n{text}", snippet.label, parsed.unwrap_err());
        }
    }
}
