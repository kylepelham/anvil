//! Code actions: a did-you-mean replaced by its suggestion, an unused dependency removed, an unknown dependency
//! that is a vcpkg port added to [deps]. Only text edits.

use super::locate::SourceFile;
use super::scan::{self, Part};
use std::ops::Range;

pub struct Action {
    pub title: String,
    pub edits: Vec<(Range<usize>, String)>,
    pub preferred: bool,
}

/// What a diagnostic of the document says, as the client sends it back.
pub struct Reported {
    pub span: Range<usize>,
    pub code: String,
    /// The did-you-mean's span and word, from the diagnostic's data.
    pub replacement: Option<(Range<usize>, String)>,
}

pub fn actions(file: &SourceFile, diagnostics: &[Reported], ports: &[String]) -> Vec<Action> {
    let mut actions = Vec::new();

    for diagnostic in diagnostics {
        if let Some((span, word)) = &diagnostic.replacement {
            actions.push(Action {
                title: format!("Change to `{word}`"),
                edits: vec![(span.clone(), word.clone())],
                preferred: true,
            });
        }

        match diagnostic.code.as_str() {
            "unused-dep" => actions.extend(remove_dependency(file, &diagnostic.span)),
            "unknown-dependency" => {
                let name = file.text.get(diagnostic.span.clone()).unwrap_or_default().to_string();
                if ports.contains(&name) {
                    actions.push(add_dependency(file, &name));
                }
            }
            _ => {}
        }
    }

    actions
}

/// Deletes the [deps] or [dev-deps] entry whose key is at `span`: its lines, or its own table.
fn remove_dependency(file: &SourceFile, span: &Range<usize>) -> Option<Action> {
    let scan = &file.scan;
    let token = scan.token_at(span.start)?;
    let [Part::Key(table), Part::Key(name)] = token.path.as_slice() else {
        return None;
    };

    let remove = match scan.entry(&token.path) {
        Some(entry) => whole_lines(&file.text, entry.key_span.start..entry.value_span.end),
        None => {
            let header = scan.headers.iter().find(|header| header.path == token.path)?;
            scan::line_start(&file.text, header.span.start)..header.end
        }
    };

    Some(Action {
        title: format!("Remove `[{table}] {name}`"),
        edits: vec![(remove, String::new())],
        preferred: false,
    })
}

/// Adds `name = "vcpkg"` under [deps] (after its last entry), or a [deps] table at the end.
fn add_dependency(file: &SourceFile, name: &str) -> Action {
    let scan = &file.scan;
    let line = format!("{} = \"vcpkg\"\n", super::completion::quote_key(name));

    let deps = scan.headers.iter().find(|header| header.path == scan::keys(&["deps"]));
    let edit = match deps {
        Some(header) => {
            // After the table's last line with something on it
            let body = &file.text[header.span.end..header.end];
            let last = body.trim_end().len();
            let at = header.span.end + body[last..].find('\n').map_or(body.len(), |offset| last + offset + 1);
            let text = if at == file.text.len() && !file.text.ends_with('\n') {
                format!("\n{line}")
            } else {
                line
            };
            (at..at, text)
        }
        None => {
            let separator = if file.text.ends_with('\n') || file.text.is_empty() { "" } else { "\n" };
            (file.text.len()..file.text.len(), format!("{separator}\n[deps]\n{line}"))
        }
    };

    Action {
        title: format!("Add `{name} = \"vcpkg\"` to [deps]"),
        edits: vec![edit],
        preferred: false,
    }
}

/// A span grown to the whole lines it is on, with the line break after it.
fn whole_lines(text: &str, span: Range<usize>) -> Range<usize> {
    let start = scan::line_start(text, span.start);
    let end = text[span.end..].find('\n').map_or(text.len(), |offset| span.end + offset + 1);

    start..end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::navigation::source;

    fn apply(text: &str, action: &Action) -> String {
        let mut result = text.to_string();
        for (span, new_text) in action.edits.iter().rev() {
            result.replace_range(span.clone(), new_text);
        }
        result
    }

    #[test]
    fn remove_and_add_dependencies() {
        let text = "[deps]\nfmt = \"vcpkg\"\nzlib = { vcpkg = \"zlib\" }\n\n[target.app]\ndeps = [\"fmt\", \"spdlog\"]\n";
        let file = source("C:/p/anvil.toml", text, "g");

        let zlib = text.find("zlib").unwrap();
        let unused = Reported {
            span: zlib..zlib + 4,
            code: "unused-dep".into(),
            replacement: None,
        };
        let spdlog = text.find("spdlog").unwrap();
        let unknown = Reported {
            span: spdlog..spdlog + 6,
            code: "unknown-dependency".into(),
            replacement: Some((spdlog..spdlog + 6, "fmt".into())),
        };
        let found = actions(&file, &[unused, unknown], &["spdlog".to_string()]);

        assert_eq!(found.len(), 3);
        assert_eq!(
            apply(text, &found[0]),
            "[deps]\nfmt = \"vcpkg\"\n\n[target.app]\ndeps = [\"fmt\", \"spdlog\"]\n"
        );
        assert_eq!(
            apply(text, &found[1]),
            "[deps]\nfmt = \"vcpkg\"\nzlib = { vcpkg = \"zlib\" }\n\n[target.app]\ndeps = [\"fmt\", \"fmt\"]\n"
        );
        assert_eq!(
            apply(text, &found[2]),
            "[deps]\nfmt = \"vcpkg\"\nzlib = { vcpkg = \"zlib\" }\nspdlog = \"vcpkg\"\n\n[target.app]\ndeps = [\"fmt\", \"spdlog\"]\n"
        );
    }
}
