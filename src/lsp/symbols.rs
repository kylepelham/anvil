//! Document symbols (the tables of a manifest or tests.toml, nested), workspace symbols (targets) and semantic
//! tokens (condition words in `when` keys and `if` values, placeholders in texts).

use super::locate::SourceFile;
use super::scan::{self, Part, TokenKind};
use super::view::{self, View, WordKind};
use std::ops::Range;

// LSP SymbolKind values
const SYMBOL_MODULE: u32 = 2;
const SYMBOL_NAMESPACE: u32 = 3;
const SYMBOL_PACKAGE: u32 = 4;
const SYMBOL_CLASS: u32 = 5;
const SYMBOL_PROPERTY: u32 = 7;
const SYMBOL_FUNCTION: u32 = 12;
const SYMBOL_VARIABLE: u32 = 13;
const SYMBOL_CONSTANT: u32 = 14;
const SYMBOL_EVENT: u32 = 24;

/// The semantic token types, in the legend's order.
pub const TOKEN_TYPES: &[&str] = &["enumMember", "keyword", "type", "variable", "macro", "operator"];

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub detail: Option<String>,
    pub kind: u32,
    /// The whole table or entry, and the name in it.
    pub range: Range<usize>,
    pub selection: Range<usize>,
    pub children: Vec<Symbol>,
}

/// The document's symbols: [project], [deps] and each dependency, targets with their when blocks and steps,
/// presets, profiles, options, tests, [[write]] files.
pub fn document_symbols(file: &SourceFile) -> Vec<Symbol> {
    let scan = &file.scan;
    let mut symbols = Vec::new();

    for top in scan.children(&[]) {
        let path = scan::keys(&[top.as_str()]);
        let kind = match top.as_str() {
            "project" | "workspace" | "vcpkg" | "defaults" => SYMBOL_NAMESPACE,
            _ => SYMBOL_MODULE,
        };

        match top.as_str() {
            "test" | "write" => {
                for item in scan.array_items(&path) {
                    let key = if top == "test" { "name" } else { "path" };
                    let mut name_path = item.clone();
                    name_path.push(Part::Key(key.into()));
                    let name = scan.string(&name_path);
                    let label = name.map(|token| token.text.clone()).unwrap_or_else(|| format!("[[{top}]]"));
                    let range = table_range(file, &item);
                    let selection = name.map(|token| token.content.clone()).unwrap_or_else(|| range.start..range.start);
                    let kind = if top == "test" { SYMBOL_EVENT } else { SYMBOL_PROPERTY };
                    symbols.push(symbol(label, Some(format!("[[{top}]]")), kind, range, selection, Vec::new()));
                }
            }
            "deps" | "dev-deps" | "target" | "preset" | "profile" | "options" => {
                let children = scan
                    .children(&path)
                    .into_iter()
                    .map(|name| {
                        let mut child = path.clone();
                        child.push(Part::Key(name.clone()));
                        let child_kind = match top.as_str() {
                            "deps" | "dev-deps" => SYMBOL_PACKAGE,
                            "target" => SYMBOL_CLASS,
                            "options" => SYMBOL_VARIABLE,
                            "profile" => SYMBOL_CONSTANT,
                            _ => SYMBOL_FUNCTION,
                        };
                        let grandchildren = if top == "target" || top == "preset" {
                            blocks(file, &child)
                        } else {
                            Vec::new()
                        };
                        let detail = (top == "target").then(|| target_kind(file, &child)).flatten();
                        symbol(
                            name,
                            detail,
                            child_kind,
                            table_range(file, &child),
                            name_span(file, &child),
                            grandchildren,
                        )
                    })
                    .collect();
                symbols.push(symbol(
                    format!("[{top}]"),
                    None,
                    kind,
                    table_range(file, &path),
                    name_span(file, &path),
                    children,
                ));
            }
            _ => symbols.push(symbol(
                format!("[{top}]"),
                None,
                kind,
                table_range(file, &path),
                name_span(file, &path),
                Vec::new(),
            )),
        }
    }

    symbols
}

/// A target's when blocks and build steps.
fn blocks(file: &SourceFile, target: &[Part]) -> Vec<Symbol> {
    let scan = &file.scan;
    let mut children = Vec::new();

    let mut when = target.to_vec();
    when.push(Part::Key("when".into()));
    for condition in scan.children(&when) {
        let mut path = when.clone();
        path.push(Part::Key(condition.clone()));
        children.push(symbol(
            format!("when \"{condition}\""),
            None,
            SYMBOL_NAMESPACE,
            table_range(file, &path),
            name_span(file, &path),
            Vec::new(),
        ));
    }
    for step in ["run", "pre-build", "post-build", "file"] {
        let mut path = target.to_vec();
        path.push(Part::Key(step.into()));
        for item in scan.array_items(&path) {
            children.push(symbol(
                format!("[[{step}]]"),
                None,
                SYMBOL_FUNCTION,
                table_range(file, &item),
                name_span(file, &item),
                Vec::new(),
            ));
        }
    }

    children
}

fn target_kind(file: &SourceFile, target: &[Part]) -> Option<String> {
    let mut path = target.to_vec();
    path.push(Part::Key("kind".into()));
    file.scan.string(&path).map(|token| token.text.clone())
}

/// Workspace symbols: the targets of every file whose name holds the query.
pub fn target_symbols<'a>(files: impl Iterator<Item = &'a SourceFile>, query: &str) -> Vec<(usize, Symbol, &'a SourceFile)> {
    let query = query.to_lowercase();
    let mut found = Vec::new();

    for (index, file) in files.enumerate() {
        let targets = scan::keys(&["target"]);
        for name in file.scan.children(&targets) {
            if !name.to_lowercase().contains(&query) {
                continue;
            }
            let mut path = targets.clone();
            path.push(Part::Key(name.clone()));
            let detail = Some(file.package.clone());
            found.push((
                index,
                symbol(name, detail, SYMBOL_CLASS, table_range(file, &path), name_span(file, &path), Vec::new()),
                file,
            ));
        }
    }

    found
}

/// A table's or entry's whole span: a header to the next header, an entry's key to its value's end, else the
/// spans of every key under it.
fn table_range(file: &SourceFile, path: &[Part]) -> Range<usize> {
    let scan = &file.scan;
    if let Some(header) = scan.headers.iter().find(|header| header.path == path) {
        return header.span.start..header.end.max(header.span.end);
    }
    if let Some(entry) = scan.entry(path) {
        return entry.key_span.start..entry.value_span.end;
    }

    // Spread over several headers (`[target.a]`, `[target.a.when.x]`) or dotted keys
    let starts = scan
        .headers
        .iter()
        .filter(|header| header.path.starts_with(path))
        .map(|header| (header.span.start, header.end));
    let entries = scan
        .entries
        .iter()
        .filter(|entry| entry.path.starts_with(path))
        .map(|entry| (entry.key_span.start, entry.value_span.end));
    let (start, end) = starts
        .chain(entries)
        .fold((usize::MAX, 0), |(start, end), (from, to)| (start.min(from), end.max(to)));
    if start == usize::MAX {
        return 0..0;
    }

    start..end
}

fn name_span(file: &SourceFile, path: &[Part]) -> Range<usize> {
    file.scan
        .tokens
        .iter()
        .find(|token| matches!(token.kind, TokenKind::Header | TokenKind::Key) && token.path == path)
        .map(|token| token.content.clone())
        .unwrap_or_else(|| {
            let range = table_range(file, path);
            range.start..range.start
        })
}

fn symbol(name: String, detail: Option<String>, kind: u32, range: Range<usize>, selection: Range<usize>, children: Vec<Symbol>) -> Symbol {
    // The selection must lie inside the range
    let selection = if selection.start >= range.start && selection.end <= range.end {
        selection
    } else {
        range.start..range.start
    };

    Symbol {
        name,
        detail,
        kind,
        range,
        selection,
        children,
    }
}

/// Semantic tokens: (span, type index in TOKEN_TYPES), in document order, none spanning lines.
pub fn semantic_tokens(view: &View) -> Vec<(Range<usize>, u32)> {
    let file = view.document();
    let words = view.condition_words();
    let mut tokens = Vec::new();

    for token in &file.scan.tokens {
        let content = &file.text[token.content.clone()];

        // Conditions: words by kind, operators
        if view::is_condition(token) {
            for (index, character) in content.char_indices() {
                if "!&|(),".contains(character) {
                    tokens.push((token.content.start + index..token.content.start + index + 1, 5));
                }
            }
            for (word, span) in view::condition_words(content) {
                let kind = words.iter().find(|(known, _)| *known == word).map(|(_, kind)| *kind);
                let type_index = match kind {
                    Some(WordKind::Platform | WordKind::Compiler) => 0,
                    Some(WordKind::Constant) => 1,
                    Some(WordKind::Profile) => 2,
                    Some(WordKind::Option) => 3,
                    None => continue,
                };
                tokens.push((token.content.start + span.start..token.content.start + span.end, type_index));
            }
            continue;
        }

        // Placeholders in texts: `{out}`, `{target:app}`
        if token.kind == TokenKind::String && !content.contains('\n') {
            let mut from = 0;
            while let Some(start) = content[from..].find('{') {
                let start = from + start;
                let Some(length) = content[start..].find('}') else { break };
                let inner = &content[start + 1..start + length];
                if !inner.is_empty()
                    && inner
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric() || "_-:/,.*".contains(character))
                {
                    tokens.push((token.content.start + start..token.content.start + start + length + 1, 4));
                }
                from = start + length + 1;
            }
        }
    }

    tokens.sort_by_key(|(span, _)| span.start);
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::navigation::source;

    #[test]
    fn symbols_nest() {
        let text = "[project]\nname = \"g\"\n[deps]\nfmt = \"vcpkg\"\n[target.app]\nkind = \"exe\"\n[target.app.when.windows]\nlibs = [\"user32\"]\n[[test]]\nname = \"t\"\ncommand = \"x\"\n";
        let file = source("C:/p/anvil.toml", text, "g");
        let symbols = document_symbols(&file);

        let names: Vec<&str> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();
        assert_eq!(names, vec!["[project]", "[deps]", "[target]", "t"]);
        let app = &symbols[2].children[0];
        assert_eq!(app.name, "app");
        assert_eq!(app.detail.as_deref(), Some("exe"));
        assert_eq!(app.children[0].name, "when \"windows\"");
        assert!(app.range.start <= app.selection.start && app.selection.end <= app.range.end);
        assert_eq!(&text[app.selection.clone()], "app");
    }

    #[test]
    fn condition_and_placeholder_tokens() {
        let text = "[options]\nfast = true\n[target.app]\nif = \"windows & !fast\"\ndefines = [\"OUT={out}\"]\n";
        let file = source("C:/p/anvil.toml", text, "g");
        let view = View {
            files: vec![&file],
            model: None,
            ports: &[],
        };
        let found: Vec<(&str, u32)> = semantic_tokens(&view).into_iter().map(|(span, kind)| (&text[span], kind)).collect();
        assert_eq!(found, vec![("windows", 0), ("&", 5), ("!", 5), ("fast", 3), ("{out}", 4)]);
    }
}
