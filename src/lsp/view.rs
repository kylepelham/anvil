//! What a request sees of a project: the document it is about (its current text), the other manifests and
//! tests.toml files the last check read, and what the loaded model said.

use super::analysis::ModelSummary;
use super::locate::{SourceFile, DEPENDENCY_KEYS};
use super::scan::{self, KeyPath, Part, TokenKind};
use super::schema_info::FileKind;
use crate::cond::BUILTIN_WORDS;
use crate::util;
use std::path::Path;

pub struct View<'a> {
    /// The document first, then the project's other files.
    pub files: Vec<&'a SourceFile>,
    pub model: Option<&'a ModelSummary>,
    /// vcpkg's port names, when known.
    pub ports: &'a [String],
}

/// What a condition word is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordKind {
    Platform,
    Compiler,
    Constant,
    Profile,
    Option,
}

// The built-in words by kind (crate::cond::BUILTIN_WORDS)
const PLATFORM_WORDS: &[&str] = &[
    "windows", "win32", "linux", "macos", "osx", "apple", "unix", "x64", "x86_64", "amd64", "x86", "i686", "arm64", "aarch64", "arm", "cross",
];
const COMPILER_WORDS: &[&str] = &["msvc", "gnu", "cl", "clang-cl", "clang", "gcc", "mingw"];

impl<'a> View<'a> {
    pub fn document(&self) -> &'a SourceFile {
        self.files[0]
    }

    /// The manifests of the project.
    pub fn manifests(&self) -> impl Iterator<Item = (usize, &'a SourceFile)> + '_ {
        self.files.iter().copied().enumerate().filter(|(_, file)| file.kind == FileKind::Manifest)
    }

    /// The manifest of a package, by its name.
    pub fn package_manifest(&self, package: &str) -> Option<(usize, &'a SourceFile)> {
        self.manifests().find(|(_, file)| file.package == package)
    }

    /// The files of a package: its manifest and tests.toml files.
    pub fn package_files(&self, package: &str) -> impl Iterator<Item = (usize, &'a SourceFile)> + '_ {
        let package = package.to_string();
        self.files.iter().copied().enumerate().filter(move |(_, file)| file.package == package)
    }

    /// The package of the document.
    pub fn package(&self) -> &'a str {
        &self.document().package
    }

    /// Every name a condition can use: built-in words, profiles and options, with what each is.
    pub fn condition_words(&self) -> Vec<(String, WordKind)> {
        let mut words: Vec<(String, WordKind)> = Vec::new();
        let mut add = |word: &str, kind: WordKind| {
            if !words.iter().any(|(known, _)| known == word) {
                words.push((word.to_string(), kind));
            }
        };

        for word in BUILTIN_WORDS {
            let kind = match *word {
                _ if PLATFORM_WORDS.contains(word) => WordKind::Platform,
                _ if COMPILER_WORDS.contains(word) => WordKind::Compiler,
                _ => WordKind::Constant,
            };
            add(word, kind);
        }
        for profile in self.profiles() {
            add(&profile, WordKind::Profile);
        }
        for option in self.options() {
            add(&option, WordKind::Option);
        }

        words
    }

    pub fn word_kind(&self, word: &str) -> Option<WordKind> {
        self.condition_words().into_iter().find(|(known, _)| known == word).map(|(_, kind)| kind)
    }

    /// Built-in and declared profiles.
    pub fn profiles(&self) -> Vec<String> {
        let mut profiles = vec!["debug".to_string(), "release".to_string()];
        for (_, file) in self.manifests() {
            for name in file.scan.children(&scan::keys(&["profile"])) {
                if !profiles.contains(&name) {
                    profiles.push(name);
                }
            }
        }

        profiles
    }

    /// Every package's options.
    pub fn options(&self) -> Vec<String> {
        let mut options = Vec::new();
        for (_, file) in self.manifests() {
            for name in file.scan.children(&scan::keys(&["options"])) {
                if !options.contains(&name) {
                    options.push(name);
                }
            }
        }

        options
    }

    /// A package's targets: its [target] tables, else (layout conventions) the model's.
    pub fn targets_of(&self, package: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .package_manifest(package)
            .map(|(_, file)| file.scan.children(&scan::keys(&["target"])))
            .unwrap_or_default();

        for target in self.model.iter().flat_map(|model| &model.targets) {
            if target.package == package && target.kind != "external" && !names.contains(&target.name) {
                names.push(target.name.clone());
            }
        }

        names
    }

    /// A package's [deps] and [dev-deps] names.
    pub fn dependencies_of(&self, package: &str) -> Vec<String> {
        let Some((_, file)) = self.package_manifest(package) else {
            return Vec::new();
        };
        let mut names = file.scan.children(&scan::keys(&["deps"]));
        names.extend(file.scan.children(&scan::keys(&["dev-deps"])));

        names
    }

    /// A package's tests: [[test]] names in its files, and its test targets.
    pub fn tests_of(&self, package: &str) -> Vec<String> {
        let mut names = Vec::new();

        for (_, file) in self.package_files(package) {
            for item in file.scan.array_items(&scan::keys(&["test"])) {
                let mut path = item.clone();
                path.push(Part::Key("name".into()));
                if let Some(token) = file.scan.string(&path) {
                    if !names.contains(&token.text) {
                        names.push(token.text.clone());
                    }
                }
            }
        }
        for target in self.model.iter().flat_map(|model| &model.targets) {
            if target.package == package && target.kind == "test" && !names.contains(&target.name) {
                names.push(target.name.clone());
            }
        }

        // Matrix tests by their own names
        for test in self.model.iter().flat_map(|model| &model.tests) {
            if !names.contains(test) && !test.contains('/') {
                names.push(test.clone());
            }
        }

        names
    }

    /// The labels the project's tests carry, as its files write them; the document's `except` (the one being typed)
    /// left out.
    pub fn labels(&self, except: &std::ops::Range<usize>) -> Vec<String> {
        let mut labels: Vec<String> = Vec::new();
        for (position, file) in self.files.iter().enumerate() {
            for token in &file.scan.tokens {
                let typed = position == 0 && token.span == *except;
                if token.kind == TokenKind::String && last_key(&token.path) == Some("labels") && !typed && !labels.contains(&token.text) {
                    labels.push(token.text.clone());
                }
            }
        }
        labels.sort();

        labels
    }

    /// The package names of the project's other manifests.
    pub fn packages(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for (_, file) in self.manifests() {
            if !names.contains(&file.package) {
                names.push(file.package.clone());
            }
        }

        names
    }

    /// The package a path dependency's folder holds: the manifest in that folder.
    pub fn dependency_package(&self, file: &SourceFile, dependency: &str) -> Option<String> {
        let folder = dependency_folder(file, dependency)?;
        let key = util::path_key(&folder.join(crate::project::MANIFEST_NAME));

        self.manifests()
            .find(|(_, manifest)| util::path_key(&manifest.path) == key)
            .map(|(_, manifest)| manifest.package.clone())
            .or_else(|| Some(dependency.to_string()))
    }
}

/// The folder of a `[deps]` entry's `path`, from the file's folder.
pub fn dependency_folder(file: &SourceFile, dependency: &str) -> Option<std::path::PathBuf> {
    let written = ["deps", "dev-deps"]
        .iter()
        .find_map(|table| file.scan.string(&scan::keys(&[table, dependency, "path"])))?;
    let folder = file.path.parent()?;

    Some(util::absolute(folder, Path::new(&written.text)))
}

/// The last key of a path (an array item's key).
pub fn last_key(path: &[Part]) -> Option<&str> {
    path.iter().rev().find_map(|part| match part {
        Part::Key(key) => Some(key.as_str()),
        Part::Index(_) => None,
    })
}

/// Whether a value path is an item of a key that names targets and dependencies (`deps`, `needs`, ...) in a target,
/// [defaults], a preset, a when block or `public`.
pub fn names_dependency(path: &[Part]) -> bool {
    let Some(key) = last_key(path) else { return false };
    let first = path.first();
    let in_targets = [Part::Key("target".into()), Part::Key("defaults".into()), Part::Key("preset".into())];

    DEPENDENCY_KEYS.contains(&key) && first.is_some_and(|first| in_targets.contains(first))
}

/// Whether a token is a condition: a `when` key or an `if` value.
pub fn is_condition(token: &scan::Token) -> bool {
    match token.kind {
        TokenKind::Header | TokenKind::Key => token.path.len() >= 2 && token.path[token.path.len() - 2] == Part::Key("when".into()),
        TokenKind::String => token.path.last() == Some(&Part::Key("if".into())),
        TokenKind::Bare => false,
    }
}

/// The condition word at a byte offset of a condition token: (word, span).
pub fn word_at(text: &str, token: &scan::Token, offset: usize) -> Option<(String, std::ops::Range<usize>)> {
    let content = &text[token.content.clone()];
    condition_words(content)
        .into_iter()
        .map(|(word, span)| (word, token.content.start + span.start..token.content.start + span.end))
        .find(|(_, span)| span.start <= offset && offset <= span.end)
}

/// The words of a condition text and their spans (operators and parentheses left out).
pub fn condition_words(text: &str) -> Vec<(String, std::ops::Range<usize>)> {
    let mut words = Vec::new();
    let is_word = |character: char| character.is_ascii_alphanumeric() || "_-.+".contains(character);

    let mut start: Option<usize> = None;
    for (index, character) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
        match (is_word(character), start) {
            (true, None) => start = Some(index),
            (false, Some(begin)) => {
                let word = &text[begin..index];
                if !matches!(word, "and" | "or" | "not") {
                    words.push((word.to_string(), begin..index));
                }
                start = None;
            }
            _ => {}
        }
    }

    words
}

/// `{target:NAME}` references in a string's text: (name, span of the name in the text).
pub fn target_references(text: &str) -> Vec<(String, std::ops::Range<usize>)> {
    let mut references = Vec::new();
    let mut from = 0;

    while let Some(start) = text[from..].find("{target:") {
        let name_start = from + start + "{target:".len();
        let Some(length) = text[name_start..].find('}') else { break };
        references.push((text[name_start..name_start + length].to_string(), name_start..name_start + length));
        from = name_start + length + 1;
    }

    references
}

/// Whether a path is the table of a target, [defaults] or a preset (or under it).
pub fn in_target_table(path: &KeyPath) -> bool {
    matches!(path.first(), Some(Part::Key(first)) if first == "target" || first == "defaults" || first == "preset")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_and_references() {
        let words: Vec<String> = condition_words("windows & !(msvc | x86_64) and not my-opt")
            .into_iter()
            .map(|(word, _)| word)
            .collect();
        assert_eq!(words, vec!["windows", "msvc", "x86_64", "my-opt"]);

        let references = target_references("{target:tools/gen} --in {target:app}.x {target:");
        assert_eq!(
            references.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            vec!["tools/gen", "app"]
        );
        assert_eq!(&"{target:tools/gen} --in {target:app}.x"[references[1].1.clone()], "app");
    }
}
