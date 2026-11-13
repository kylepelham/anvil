//! Go to definition, find references and rename for what manifests name: targets (also `package/target` and
//! `{target:...}` across packages), [deps] entries, options and profiles (also as condition words), presets and
//! tests (`after`). Paths of path dependencies and workspace members lead to their anvil.toml.

use super::scan::{Part, TokenKind};
use super::view::{self, View, WordKind};
use crate::util;
use std::ops::Range;
use std::path::{Path, PathBuf};

/// Something a manifest declares and names elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Symbol {
    Target {
        package: String,
        name: String,
    },
    Dependency {
        package: String,
        name: String,
    },
    Option(String),
    Profile(String),
    /// Presets belong to one manifest (by its index in the view).
    Preset {
        file: usize,
        name: String,
    },
    Test {
        package: String,
        name: String,
    },
}

/// Where a symbol's name is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    pub file: usize,
    pub span: Range<usize>,
    pub definition: bool,
}

/// What go to definition leads to.
pub enum Target {
    Span(usize, Range<usize>),
    File(PathBuf),
}

/// The symbol at a byte offset of the document, and the span of its name there.
pub fn symbol_at(view: &View, offset: usize) -> Option<(Symbol, Range<usize>)> {
    let file = view.document();
    let token = file.scan.token_at(offset)?;
    let package = view.package().to_string();
    let path = &token.path;

    // Conditions: an option's or profile's name
    if view::is_condition(token) {
        let (word, span) = view::word_at(&file.text, token, offset)?;
        return match view.word_kind(&word)? {
            WordKind::Option => Some((Symbol::Option(word), span)),
            WordKind::Profile => Some((Symbol::Profile(word), span)),
            _ => None,
        };
    }

    // Declarations: table keys
    if matches!(token.kind, TokenKind::Header | TokenKind::Key) {
        let name = token.text.clone();
        let span = token.content.clone();
        return match key_names(path).as_slice() {
            ["target", _] => Some((Symbol::Target { package, name }, span)),
            ["deps" | "dev-deps", _] => Some((Symbol::Dependency { package, name }, span)),
            ["options", _] => Some((Symbol::Option(name), span)),
            ["profile", _] => Some((Symbol::Profile(name), span)),
            ["preset", _] => Some((Symbol::Preset { file: 0, name }, span)),
            _ => None,
        };
    }
    if token.kind != TokenKind::String {
        return None;
    }

    // `{target:x}` anywhere in a text, from its brace to its brace
    for (name, span) in view::target_references(&token.text) {
        let span = shift(token.content.start, span);
        if span.start - "{target:".len() <= offset && offset <= span.end + 1 {
            return Some(target_symbol(view, &package, &name, span));
        }
    }

    let text = token.text.clone();
    let span = token.content.clone();
    let key = view::last_key(path).unwrap_or_default();
    match key {
        _ if view::names_dependency(path) || (key == "default-run" && path.len() == 2) => {
            if text.contains('/') {
                return Some(target_symbol(view, &package, &text, span));
            }
            if view.targets_of(&package).contains(&text) {
                return Some((Symbol::Target { package, name: text }, span));
            }
            if view.dependencies_of(&package).contains(&text) {
                return Some((Symbol::Dependency { package, name: text }, span));
            }
            view.packages().contains(&text).then(|| {
                (
                    Symbol::Target {
                        package: text.clone(),
                        name: text,
                    },
                    span,
                )
            })
        }
        "after" => {
            let (test_package, name) = match text.split_once('/') {
                Some((test_package, name)) if view.packages().iter().any(|known| known == test_package) => {
                    (test_package.to_string(), name.to_string())
                }
                _ => (package, text),
            };
            Some((Symbol::Test { package: test_package, name }, span))
        }
        "inherits" => Some((Symbol::Profile(text), span)),
        "preset" => Some((Symbol::Preset { file: 0, name: text }, span)),
        "name" if matches!(path.as_slice(), [Part::Key(test), Part::Index(_), _] if test == "test") => {
            Some((Symbol::Test { package, name: text }, span))
        }
        "target" if path.len() == 3 && matches!(&path[0], Part::Key(table) if table == "deps" || table == "dev-deps") => {
            let Part::Key(dependency) = &path[1] else { return None };
            let target_package = view.dependency_package(file, dependency)?;
            Some((
                Symbol::Target {
                    package: target_package,
                    name: text,
                },
                span,
            ))
        }
        _ => None,
    }
}

/// Where go to definition goes from a byte offset: the symbol's declaration, or the anvil.toml a path names.
pub fn definition(view: &View, offset: usize) -> Option<Target> {
    let file = view.document();

    // `path = "../lib"` of a dependency, a workspace member
    if let Some(token) = file.scan.token_at(offset).filter(|token| token.kind == TokenKind::String) {
        let names_folder = match key_names(&token.path).as_slice() {
            ["deps" | "dev-deps", _, "path"] => true,
            ["workspace", "members"] => !token.text.contains(['*', '?', '[', '{']),
            _ => false,
        };
        if names_folder {
            let folder = util::absolute(file.path.parent()?, Path::new(&token.text));
            let manifest = folder.join(crate::project::MANIFEST_NAME);
            return manifest.is_file().then_some(Target::File(manifest));
        }
    }

    let (symbol, span) = symbol_at(view, offset)?;
    let occurrences = occurrences(view, &symbol);
    let definition = occurrences.iter().find(|occurrence| occurrence.definition);

    // On a path dependency's own [deps] key: the manifest of the package it names
    if let Symbol::Dependency { name, .. } = &symbol {
        let on_definition = definition.is_some_and(|found| found.file == 0 && found.span == span);
        let manifest = view::dependency_folder(file, name).map(|folder| folder.join(crate::project::MANIFEST_NAME));
        if let Some(manifest) = manifest.filter(|manifest| on_definition && manifest.is_file()) {
            return Some(Target::File(manifest));
        }
    }

    if let Some(found) = definition {
        return Some(Target::Span(found.file, found.span.clone()));
    }

    // A target the layout made, a package: its manifest
    match &symbol {
        Symbol::Target { package, .. } => {
            let (index, _) = view.package_manifest(package)?;
            Some(Target::Span(index, 0..0))
        }
        Symbol::Profile(name) if name == "debug" || name == "release" => None,
        _ => None,
    }
}

/// Every place a symbol's name is written in the view's files.
pub fn occurrences(view: &View, symbol: &Symbol) -> Vec<Occurrence> {
    let mut found = Vec::new();

    for (index, file) in view.files.iter().enumerate() {
        let scan = &file.scan;
        let mut add = |span: Range<usize>, definition: bool| {
            let occurrence = Occurrence {
                file: index,
                span,
                definition,
            };
            if !found.contains(&occurrence) {
                found.push(occurrence);
            }
        };

        match symbol {
            Symbol::Target { package, name } => {
                let own = file.package == *package;
                let qualified = format!("{package}/{name}");
                for token in &scan.tokens {
                    match token.kind {
                        TokenKind::Header | TokenKind::Key if own && key_names(&token.path) == ["target", name.as_str()] => {
                            add(token.content.clone(), true)
                        }
                        TokenKind::String => {
                            let names_target = view::names_dependency(&token.path) || (key_names(&token.path) == ["project", "default-run"]);
                            if names_target && own && token.text == *name {
                                add(token.content.clone(), false);
                            }
                            if names_target && token.text == qualified {
                                add(shift(token.content.start, package.len() + 1..qualified.len()), false);
                            }
                            for (reference, span) in view::target_references(&token.text) {
                                let span = shift(token.content.start, span);
                                if own && reference == *name {
                                    add(span, false);
                                } else if reference == qualified {
                                    add(span.start + package.len() + 1..span.end, false);
                                }
                            }

                            // `target = "x"` of the [deps] entry that loads the package
                            if let [Part::Key(table), Part::Key(dependency), Part::Key(key)] = token.path.as_slice() {
                                let entry = (table == "deps" || table == "dev-deps") && key == "target";
                                if entry && token.text == *name && view.dependency_package(file, dependency).as_deref() == Some(package.as_str()) {
                                    add(token.content.clone(), false);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Symbol::Dependency { package, name } => {
                if file.package != *package {
                    continue;
                }
                for token in &scan.tokens {
                    match token.kind {
                        TokenKind::Header | TokenKind::Key => {
                            if matches!(key_names(&token.path).as_slice(), ["deps" | "dev-deps", key] if key == name) {
                                add(token.content.clone(), true);
                            }
                        }
                        TokenKind::String if view::names_dependency(&token.path) => {
                            if token.text == *name {
                                add(token.content.clone(), false);
                            } else if token.text.strip_prefix(name.as_str()).is_some_and(|rest| rest.starts_with('/')) {
                                add(token.content.start..token.content.start + name.len(), false);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Symbol::Option(name) | Symbol::Profile(name) => {
                let table = if matches!(symbol, Symbol::Option(_)) { "options" } else { "profile" };
                for token in &scan.tokens {
                    if matches!(token.kind, TokenKind::Header | TokenKind::Key) && key_names(&token.path) == [table, name.as_str()] {
                        add(token.content.clone(), true);
                    }
                    if table == "profile" && token.kind == TokenKind::String && view::last_key(&token.path) == Some("inherits") && token.text == *name
                    {
                        add(token.content.clone(), false);
                    }
                    if view::is_condition(token) {
                        for (word, span) in view::condition_words(&file.text[token.content.clone()]) {
                            if word == *name {
                                add(shift(token.content.start, span), false);
                            }
                        }
                    }
                }
            }
            Symbol::Preset { file: owner, name } => {
                if index != *owner {
                    continue;
                }
                for token in &scan.tokens {
                    match token.kind {
                        TokenKind::Header | TokenKind::Key if key_names(&token.path) == ["preset", name.as_str()] => add(token.content.clone(), true),
                        TokenKind::String
                            if view::last_key(&token.path) == Some("preset") && view::in_target_table(&token.path) && token.text == *name =>
                        {
                            add(token.content.clone(), false)
                        }
                        _ => {}
                    }
                }
            }
            Symbol::Test { package, name } => {
                if file.package != *package {
                    continue;
                }
                for token in &scan.tokens {
                    if token.kind != TokenKind::String || token.text != *name {
                        continue;
                    }
                    match token.path.as_slice() {
                        [Part::Key(test), Part::Index(_), Part::Key(key)] if test == "test" && key == "name" => add(token.content.clone(), true),
                        _ if view::last_key(&token.path) == Some("after") => add(token.content.clone(), false),
                        _ => {}
                    }
                }
            }
        }
    }

    found
}

/// The edits renaming a symbol: each occurrence's name replaced (a declaration's key quoted when it must be).
pub fn rename(view: &View, symbol: &Symbol, new_name: &str) -> Vec<(usize, Range<usize>, String)> {
    let bare = !new_name.is_empty()
        && new_name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character));

    occurrences(view, symbol)
        .into_iter()
        .map(|occurrence| {
            let file = view.files[occurrence.file];
            let quoted = occurrence.span.start > 0 && matches!(file.text.as_bytes()[occurrence.span.start - 1], b'"' | b'\'');
            let in_key = file
                .scan
                .tokens
                .iter()
                .any(|token| matches!(token.kind, TokenKind::Header | TokenKind::Key) && token.content == occurrence.span);
            let text = match (in_key, quoted, bare) {
                (true, false, false) => format!("\"{}\"", super::completion::escape(new_name)),
                _ => super::completion::escape(new_name),
            };
            (occurrence.file, occurrence.span, text)
        })
        .collect()
}

/// Whether a symbol may be renamed: what the project declares (not a built-in profile, not a [deps] entry, whose
/// name is the package's).
pub fn can_rename(symbol: &Symbol) -> bool {
    match symbol {
        Symbol::Profile(name) => name != "debug" && name != "release",
        Symbol::Dependency { .. } => false,
        _ => true,
    }
}

/// `pkg/target` (or a plain name) at a cursor: the target, the span of its name.
fn target_symbol(view: &View, own: &str, reference: &str, span: Range<usize>) -> (Symbol, Range<usize>) {
    match reference.split_once('/') {
        Some((package, name)) => {
            let package = view
                .dependency_package(view.document(), package)
                .filter(|resolved| view.package_manifest(resolved).is_some())
                .unwrap_or_else(|| package.to_string());
            let name_start = span.start + reference.len() - name.len();
            (
                Symbol::Target {
                    package,
                    name: name.to_string(),
                },
                name_start..span.end,
            )
        }
        None => (
            Symbol::Target {
                package: own.to_string(),
                name: reference.to_string(),
            },
            span,
        ),
    }
}

/// A path's keys, indices left out.
fn key_names(path: &[Part]) -> Vec<&str> {
    path.iter()
        .filter_map(|part| match part {
            Part::Key(key) => Some(key.as_str()),
            Part::Index(_) => None,
        })
        .collect()
}

fn shift(by: usize, span: Range<usize>) -> Range<usize> {
    span.start + by..span.end + by
}

/// For tests: a source file of a text.
#[cfg(test)]
pub fn source(path: &str, text: &str, package: &str) -> super::locate::SourceFile {
    super::locate::SourceFile::new(PathBuf::from(path), text.to_string(), package.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = r#"[project]
name = "game"
[workspace]
members = ["tools"]
[deps]
tools = { path = "tools" }
[options]
fast = true
[target.core]
kind = "lib"
when.fast.defines = ["FAST"]
[target.app]
deps = ["core", "tools/gen"]
[[target.app.run]]
command = ["{target:tools/gen}", "{target:core}"]
output = "x.h"
"#;
    const TOOLS: &str = "[project]\nname = \"tools\"\n[target.gen]\nif = \"fast\"\n";
    const TESTS: &str = "[[test]]\nname = \"a\"\ncommand = \"{target:app}\"\n[[test]]\nname = \"b\"\ncommand = \"x\"\nafter = [\"a\"]\n";

    fn spans(view: &View, symbol: &Symbol) -> Vec<(usize, String)> {
        occurrences(view, symbol)
            .into_iter()
            .map(|occurrence| (occurrence.file, view.files[occurrence.file].text[occurrence.span].to_string()))
            .collect()
    }

    #[test]
    fn targets_options_and_tests_across_files() {
        let root = source("C:/p/anvil.toml", ROOT, "game");
        let tools = source("C:/p/tools/anvil.toml", TOOLS, "tools");
        let tests = source("C:/p/tests.toml", TESTS, "game");
        let view = View {
            files: vec![&root, &tools, &tests],
            model: None,
            ports: &[],
        };

        // `tools/gen` in deps and in a command: the target of the other package, the name part only
        let offset = ROOT.find("tools/gen\"]").unwrap() + 7;
        let (symbol, span) = symbol_at(&view, offset).unwrap();
        assert_eq!(
            symbol,
            Symbol::Target {
                package: "tools".into(),
                name: "gen".into()
            }
        );
        assert_eq!(&ROOT[span], "gen");
        assert_eq!(spans(&view, &symbol), vec![(0, "gen".into()), (0, "gen".into()), (1, "gen".into())]);
        assert!(occurrences(&view, &symbol)
            .iter()
            .any(|occurrence| occurrence.file == 1 && occurrence.definition));

        // `core`: header, deps, {target:core}
        let core = Symbol::Target {
            package: "game".into(),
            name: "core".into(),
        };
        assert_eq!(occurrences(&view, &core).len(), 3);
        let brace = ROOT.find("{target:core}").unwrap();
        assert_eq!(symbol_at(&view, brace).unwrap().0, core);
        assert_eq!(symbol_at(&view, brace + 3).unwrap().0, core);

        // An option: its declaration and the condition words, in every package
        let fast = symbol_at(&view, ROOT.find("when.fast").unwrap() + 6).unwrap().0;
        assert_eq!(fast, Symbol::Option("fast".into()));
        assert_eq!(occurrences(&view, &fast).len(), 3);

        // Tests: `after` leads to the [[test]]
        let view_tests = View {
            files: vec![&tests, &root, &tools],
            model: None,
            ports: &[],
        };
        let after = TESTS.find("[\"a\"]").unwrap() + 2;
        match definition(&view_tests, after).unwrap() {
            Target::Span(file, span) => assert_eq!((file, &TESTS[span]), (0, "a")),
            Target::File(_) => panic!("a span"),
        }

        // Rename: every occurrence, the name part of `pkg/name`
        let edits = rename(&view, &symbol, "generate");
        assert_eq!(edits.len(), 3);
        assert!(edits.iter().all(|(_, _, text)| text == "generate"));
    }
}
