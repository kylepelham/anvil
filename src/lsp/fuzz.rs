//! Random edits on real manifests, each followed by every handler at random places: none may panic.
//! The test fixtures always; more manifests with `ANVIL_LSP_FUZZ=<folder>` (every anvil.toml and tests.toml under it)
//! more edits with `ANVIL_LSP_FUZZ_EDITS=<count>`, other ones with `ANVIL_LSP_FUZZ_SEED=<number>`.

use std::path::{Path, PathBuf};

use lsp_types::{Position, Range};

use super::actions::{self, Reported};
use super::analysis::{self, ModelSummary, TargetInfo};
use super::locate::{self, SourceFile};
use super::text::{Encoding, Text};
use super::view::View;
use super::{completion, hover, navigation, scan, symbols};

/// What an edit inserts: TOML's punctuation, keys and words the handlers look for, text that is not ASCII.
const PIECES: &[&str] = &[
    "[",
    "]",
    "[[",
    "]]",
    "{",
    "}",
    "\"",
    "'",
    "'''",
    "\"\"\"",
    "=",
    ".",
    ",",
    "\n",
    "\r\n",
    "#",
    " ",
    "\\",
    "\\u",
    "when.",
    "when.\"windows & !",
    "if = \"",
    "{target:",
    "{target:app}",
    "{out}",
    "[target.",
    "[[test]]\nname = \"",
    "deps = [\"",
    "kind = ",
    "inherits = \"",
    "after = [\"",
    "[deps]\n",
    "path = \"..\"",
    "é",
    "😀",
    "\u{feff}",
    "\t",
    "0x",
    "1e9",
    "true",
    "-",
];

/// A small deterministic random number generator (xorshift).
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, limit: usize) -> usize {
        (self.next() % limit.max(1) as u64) as usize
    }
}

/// The manifests to edit: the fixtures, and those under `ANVIL_LSP_FUZZ`.
fn manifests() -> Vec<PathBuf> {
    let mut folders = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")];
    folders.extend(std::env::var_os("ANVIL_LSP_FUZZ").map(PathBuf::from));

    let mut found = Vec::new();
    for folder in folders {
        for entry in walkdir::WalkDir::new(folder).into_iter().flatten() {
            let name = entry.file_name().to_string_lossy();
            if name == "anvil.toml" || name == "tests.toml" {
                found.push(entry.into_path());
            }
        }
    }
    found.sort();

    found
}

/// A random position: often a real one, sometimes past a line's end or the text's.
fn random_position(random: &mut Random, text: &str) -> Position {
    let lines = text.split('\n').count();
    let line = random.below(lines + 2);
    let width = text.split('\n').nth(line).map_or(4, |line| line.chars().count() + 3);

    Position::new(line as u32, random.below(width) as u32)
}

/// One random edit, as an editor sends it (a range in one encoding, the new text).
fn random_edit(random: &mut Random, text: &mut Text, encoding: Encoding) {
    // Mostly a few characters, sometimes up to two lines further: the file keeps most of its text
    let start = random_position(random, &text.text);
    let end = match random.below(4) {
        0 => start,
        1 => Position::new(start.line + random.below(3) as u32, random.below(40) as u32),
        _ => Position::new(start.line, start.character + random.below(12) as u32),
    };

    let mut inserted = String::new();
    for _ in 0..random.below(3) {
        inserted.push_str(PIECES[random.below(PIECES.len())]);
    }
    text.apply(Some(Range::new(start, end.max(start))), &inserted, encoding);
}

/// Every handler at random places of the edited text.
fn run_handlers(random: &mut Random, path: &Path, text: &Text, encoding: Encoding, model: &ModelSummary) {
    let file = SourceFile::new(path.to_path_buf(), text.text.clone(), "game".into());
    let files = [file];
    let view = View {
        files: vec![&files[0]],
        model: Some(model),
        ports: &[],
    };

    // The whole document
    let _ = symbols::document_symbols(&files[0]);
    let _ = symbols::target_symbols(files.iter(), "a");
    let _ = symbols::semantic_tokens(&view);
    let (problems, _) = analysis::parse_problems(path, &text.text);
    let reported: Vec<Reported> = problems
        .iter()
        .map(|problem| Reported {
            span: problem.span.clone(),
            code: problem.code.to_string(),
            replacement: problem.replacement.clone(),
        })
        .collect();
    let _ = actions::actions(&files[0], &reported, &["fmt".to_string()]);
    for problem in &problems {
        let _ = text.range(&problem.span, encoding);
    }

    // Messages the loader gives, placed in the edited text
    for message in [
        "in [target.app]: in when.\"lnux\": unknown condition `lnux`",
        "in target `app`: unknown dependency `fmtt`",
        "test `smoke`: after = [\"nope\"] names no test",
        "dependency cycle: app -> core -> app",
        "two tests are named `a` in tests.toml; test names must be unique in a package",
        "`[deps] fmt`: no target uses it",
    ] {
        let location = locate::locate(&[message.to_string()], &files, 0);
        assert!(location.span.end <= text.text.len());
    }

    // Each place: what the editor asks there
    for _ in 0..8 {
        let offset = text.offset(random_position(random, &text.text), encoding);
        let _ = scan::cursor(&text.text, offset);
        let _ = completion::complete(&view, offset);
        let _ = hover::hover(&view, offset);
        let _ = navigation::definition(&view, offset);
        if let Some((symbol, _)) = navigation::symbol_at(&view, offset) {
            let _ = navigation::occurrences(&view, &symbol);
            if navigation::can_rename(&symbol) {
                let _ = navigation::rename(&view, &symbol, "renamed");
            }
        }
    }
}

#[test]
fn handlers_survive_random_edits() {
    let edits: usize = std::env::var("ANVIL_LSP_FUZZ_EDITS")
        .ok()
        .and_then(|count| count.parse().ok())
        .unwrap_or(60);
    let seed: u64 = std::env::var("ANVIL_LSP_FUZZ_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or(0x9e37_79b9_7f4a_7c15);
    let model = ModelSummary {
        targets: vec![TargetInfo {
            name: "app".into(),
            package: "game".into(),
            qualified: "app".into(),
            kind: "exe",
            output: None,
            dir: PathBuf::from("C:/p"),
            sources: 1,
            deps: vec!["core".into()],
        }],
        tests: vec!["smoke".into()],
        condition: crate::cond::CondContext {
            family: crate::toolchain::Family::Msvc,
            platform: crate::platform::Platform::host(),
            profiles: vec!["debug".into()],
            all_profiles: vec!["debug".into(), "release".into()],
            options: [("fast".to_string(), true)].into_iter().collect(),
        },
        toolchain: "msvc".into(),
        profile: "debug".into(),
    };

    let manifests = manifests();
    assert!(!manifests.is_empty());
    eprintln!("{} manifests, {edits} edits each", manifests.len());
    for (index, path) in manifests.iter().enumerate() {
        let Ok(original) = std::fs::read_to_string(path) else { continue };
        let mut random = Random(seed ^ (index as u64 + 1));
        let encoding = if index % 2 == 0 { Encoding::Utf16 } else { Encoding::Utf8 };
        let mut text = Text::new(original);

        for edit in 0..edits {
            random_edit(&mut random, &mut text, encoding);
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_handlers(&mut random, path, &text, encoding, &model)));
            if outcome.is_err() {
                panic!("{} after edit {edit}: a handler panicked on this text:\n{}", path.display(), text.text);
            }
        }
    }
}
