//! `textDocument/hover`: what a key means (the schema's description), what a value names (a target's kind,
//! package and file; a dependency's origin; a condition word and whether it holds in the checked build).

use super::navigation::{self, Symbol};
use super::scan::{self, TokenKind};
use super::schema_info;
use super::view::{self, View, WordKind};
use crate::util;
use std::ops::Range;

/// Markdown and the span it is about.
pub fn hover(view: &View, offset: usize) -> Option<(String, Range<usize>)> {
    let file = view.document();
    let token = file.scan.token_at(offset)?;

    // A condition word: what it is, and its value in the checked build
    if view::is_condition(token) {
        if let Some((word, span)) = view::word_at(&file.text, token, offset) {
            return Some((describe_word(view, &word), span));
        }
    }

    // A name: the target, dependency, profile or test it names
    if let Some((symbol, span)) = navigation::symbol_at(view, offset) {
        if let Some(text) = describe_symbol(view, &symbol) {
            let key = schema_text(view, &token.path, token.kind);
            let text = match key {
                Some(key) if token.kind == TokenKind::String => format!("{text}\n\n---\n{key}"),
                _ => text,
            };
            return Some((text, span));
        }
    }

    // A key, or a value: the key's description
    let text = schema_text(view, &token.path, token.kind)?;
    Some((text, token.span.clone()))
}

/// `**key** (path)` and the schema's description of the key a token is or belongs to.
fn schema_text(view: &View, path: &[scan::Part], kind: TokenKind) -> Option<String> {
    let file = view.document();
    let key_path: Vec<scan::Part> = match kind {
        TokenKind::Header | TokenKind::Key => path.to_vec(),
        _ => {
            let end = path.iter().rposition(|part| matches!(part, scan::Part::Key(_)))?;
            path[..=end].to_vec()
        }
    };
    let description = schema_info::description_at(file.kind, &key_path)?;

    Some(format!("**{}**\n\n{description}", scan::display(&key_path)))
}

fn describe_word(view: &View, word: &str) -> String {
    let kind = view.word_kind(word);
    let what = match kind {
        Some(WordKind::Platform) => "platform word: the machine built for",
        Some(WordKind::Compiler) => "compiler word",
        Some(WordKind::Constant) => "constant",
        Some(WordKind::Profile) => "profile (true under it and the profiles inheriting it)",
        Some(WordKind::Option) => "option (`-o name=true|false`)",
        None => "unknown condition word",
    };

    let value = view
        .model
        .and_then(|model| model.condition.lookup(word).map(|value| (value, model)))
        .map(|(value, model)| {
            format!(
                "\n\n`{value}` in the checked build ({}, {}, {})",
                model.profile,
                model.toolchain,
                model.condition.platform.name()
            )
        })
        .unwrap_or_default();
    format!("`{word}`: {what}{value}")
}

fn describe_symbol(view: &View, symbol: &Symbol) -> Option<String> {
    match symbol {
        Symbol::Target { package, name } => {
            let info = view
                .model
                .and_then(|model| model.targets.iter().find(|target| target.package == *package && target.name == *name));
            let Some(info) = info else {
                return Some(format!("target `{name}` of package `{package}` (not in the checked build)"));
            };

            let mut text = format!("**{}** — {} target of package `{}`", info.qualified, info.kind, info.package);
            if let Some(output) = &info.output {
                text.push_str(&format!("\n\nfile: `{}`", util::slash(output)));
            }
            if info.kind != "external" {
                text.push_str(&format!("\n\nfolder: `{}`, {} sources", util::slash(&info.dir), info.sources));
            }
            if !info.deps.is_empty() {
                text.push_str(&format!("\n\nuses: {}", info.deps.join(", ")));
            }
            Some(text)
        }
        Symbol::Dependency { package, name } => {
            let (_, manifest) = view.package_manifest(package)?;
            let entry = ["deps", "dev-deps"]
                .iter()
                .find_map(|table| manifest.scan.entry(&scan::keys(&[table, name])));
            let written = entry.map(|entry| manifest.text[entry.value_span.clone()].to_string()).unwrap_or_default();
            Some(format!("dependency `{name}` of package `{package}`: `{}`", written.trim()))
        }
        Symbol::Profile(name) => Some(format!("profile `{name}`")),
        Symbol::Option(name) => {
            let value = view.model.and_then(|model| model.condition.options.get(name).copied());
            Some(match value {
                Some(value) => format!("option `{name}`: `{value}` in the checked build"),
                None => format!("option `{name}`"),
            })
        }
        Symbol::Preset { name, .. } => Some(format!("preset `{name}`")),
        Symbol::Test { package, name } => Some(format!("test `{name}` of package `{package}`")),
    }
}
