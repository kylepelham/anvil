//! Where a load error or warning is: the loader's messages name what they are about (`in [target.app]`, `in
//! when."windows"`, `dependency \`fmt\``, `kind = "exe2"`, a file), so their context chain is read back into a key path
//! and the value it names, which crate::lsp::scan finds in the manifests and tests.toml files of the project. A message
//! no table of it names goes on the first line of the file it names, or of the root manifest; none is dropped.

use super::scan::{self, KeyPath, Part, Scan, TokenKind};
use super::schema_info::FileKind;
use crate::util;
use regex::Regex;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// The keys whose items name targets and dependencies
pub const DEPENDENCY_KEYS: &[&str] = &["deps", "header-deps", "needs", "whole-archive"];

/// A manifest or tests.toml of the project, as read for the load.
pub struct SourceFile {
    pub path: PathBuf,
    pub text: String,
    pub scan: Scan,
    pub kind: FileKind,
    /// The package's name (`[project] name`, else its folder's), for `package/target` and `in target \`pkg/x\``.
    pub package: String,
}

/// A place in one of the files, and other places the message names.
#[derive(Debug, Clone)]
pub struct Location {
    pub file: usize,
    pub span: Range<usize>,
    pub related: Vec<(usize, Range<usize>, String)>,
    /// The span replaced by a did-you-mean suggestion: the misspelled word.
    pub word: Option<Range<usize>>,
}

/// What a message says about where it is.
#[derive(Debug, Default, Clone)]
struct Clues {
    file: Option<usize>,
    package: Option<String>,
    scope: Vec<Scope>,
    key: Option<String>,
    needle: Option<Needle>,
    related: Vec<(Vec<Scope>, Option<Needle>, String)>,
}

#[derive(Debug, Clone)]
enum Scope {
    Path(KeyPath),
    /// A [[test]] by its name (a matrix test by its template's).
    Test(String),
    /// A [[write]] by its path.
    Write(String),
    /// A [deps] or [dev-deps] entry.
    Dependency(String),
    /// A target, a [deps] entry or a [[test]]: whichever of them the files have.
    Named(String),
}

#[derive(Debug, Clone)]
enum Needle {
    /// A string value equal to this.
    Value(String),
    /// The last string value equal to this (the second of two that should differ).
    LastValue(String),
    /// A word of a condition: a `when` key or an `if` value.
    Word(String),
    /// A whole condition text.
    Condition(String),
    /// A string value that begins with this (`pkg/`).
    Prefix(String),
    /// A path a string value ends (a file or folder the message gives absolute or relative).
    PathEnd(String),
    /// A key of the scope's table.
    Key(String),
}

impl SourceFile {
    pub fn new(path: PathBuf, text: String, package: String) -> SourceFile {
        let kind = match path.file_name().is_some_and(|name| name == crate::project::TESTS_FILE_NAME) {
            true => FileKind::Tests,
            false => FileKind::Manifest,
        };
        let scan = scan::scan(&text);

        SourceFile {
            path,
            text,
            scan,
            kind,
            package,
        }
    }
}

/// Where a message (its context chain, outermost first) is in `files`; `fallback` is the file to use when none
/// fits (the root manifest).
pub fn locate(layers: &[String], files: &[SourceFile], fallback: usize) -> Location {
    let clues = read_clues(layers, files);
    let file_order: Vec<usize> = match clues.file {
        Some(file) => std::iter::once(file).chain((0..files.len()).filter(|index| *index != file)).collect(),
        None => (0..files.len()).collect(),
    };

    // The file where the most of the clues are found
    let mut best: Option<(u8, Location)> = None;
    for index in file_order {
        let file = &files[index];
        if clues.package.as_ref().is_some_and(|package| *package != file.package) {
            continue;
        }
        let Some((score, span, word)) = find(file, &clues.scope, clues.key.as_deref(), clues.needle.as_ref()) else {
            continue;
        };
        let score = score + u8::from(clues.file == Some(index));
        if best.as_ref().is_none_or(|(best_score, _)| score > *best_score) {
            best = Some((
                score,
                Location {
                    file: index,
                    span,
                    related: Vec::new(),
                    word,
                },
            ));
        }
    }

    let mut location = match best {
        Some((_, location)) => location,
        None => {
            let file = clues.file.unwrap_or(fallback);
            Location {
                file,
                span: first_line(files.get(file).map(|file| file.text.as_str()).unwrap_or_default()),
                related: Vec::new(),
                word: None,
            }
        }
    };

    // Other places the message names
    for (scope, needle, label) in &clues.related {
        let found = (0..files.len()).find_map(|index| find(&files[index], scope, None, needle.as_ref()).map(|(_, span, _)| (index, span)));
        if let Some((index, span)) = found {
            if index != location.file || span != location.span {
                location.related.push((index, span, label.clone()));
            }
        }
    }

    location
}

/// The first line of a text, without its line break.
pub fn first_line(text: &str) -> Range<usize> {
    0..text.find(['\r', '\n']).unwrap_or(text.len())
}

/// Finds the scope, then the key, then the needle in one file: (how much was found, the span, the misspelled word).
fn find(file: &SourceFile, scope: &[Scope], key: Option<&str>, needle: Option<&Needle>) -> Option<(u8, Range<usize>, Option<Range<usize>>)> {
    let scan = &file.scan;
    let (paths, mut span) = resolve_scope(file, scope)?;
    let mut score = if scope.is_empty() { 0 } else { 2 };

    // The key in the scope (or a when block of it)
    let mut key_paths: Vec<KeyPath> = Vec::new();
    if let Some(key) = key {
        for path in &paths {
            let found = scan
                .keys_under(path)
                .filter(|token| token.path.last() == Some(&Part::Key(key.to_string())))
                .min_by_key(|token| token.path.len());
            if let Some(token) = found {
                key_paths.push(token.path.clone());
            }
        }
        if let Some(path) = key_paths.first() {
            span = scan.key_span(path).unwrap_or(span);
            score += 1;
        }
    }

    // The needle under the key, else anywhere in the scope
    let Some(needle) = needle else {
        return Some((score, span, None));
    };
    let under: Vec<KeyPath> = if key_paths.is_empty() { paths.clone() } else { key_paths };
    if let Some((found, word)) = find_needle(file, &under, needle).or_else(|| find_needle(file, &paths, needle)) {
        return Some((score + 3, found, word));
    }

    Some((score, span, None))
}

/// The paths a scope stands for in a file, and the span of its table; None when the file has no such table.
fn resolve_scope(file: &SourceFile, scope: &[Scope]) -> Option<(Vec<KeyPath>, Range<usize>)> {
    let scan = &file.scan;
    let mut paths: Vec<KeyPath> = vec![Vec::new()];

    for part in scope {
        let mut next = Vec::new();
        for base in &paths {
            match part {
                Scope::Path(path) => {
                    let mut full = base.clone();
                    full.extend(path.iter().cloned());
                    if scan.key_span(&full).is_some() {
                        next.push(full);
                    }
                }
                Scope::Test(name) => next.extend(find_tests(scan, name)),
                Scope::Write(path) => {
                    let items = scan.array_items(&scan::keys(&["write"]));
                    next.extend(items.into_iter().filter(|item| {
                        let mut key = item.clone();
                        key.push(Part::Key("path".into()));
                        scan.string(&key).is_some_and(|token| token.text == *path)
                    }));
                }
                Scope::Dependency(name) => {
                    for table in ["deps", "dev-deps"] {
                        let path = scan::keys(&[table, name]);
                        if scan.key_span(&path).is_some() {
                            next.push(path);
                        }
                    }
                }
                Scope::Named(name) => {
                    for table in ["target", "deps", "dev-deps"] {
                        let path = scan::keys(&[table, name]);
                        if scan.key_span(&path).is_some() {
                            next.push(path);
                        }
                    }
                    next.extend(find_tests(scan, name));
                }
            }
        }
        if next.is_empty() {
            return None;
        }
        paths = next;
    }

    let span = paths
        .first()
        .and_then(|path| scan.key_span(path))
        .unwrap_or_else(|| first_line(&file.text));
    Some((paths, span))
}

/// The [[test]] tables named `name`: exactly, else by a matrix template (`stills_{view}` for `stills_glass`).
fn find_tests(scan: &Scan, name: &str) -> Vec<KeyPath> {
    let items = scan.array_items(&scan::keys(&["test"]));
    let name_of = |item: &KeyPath| {
        let mut key = item.clone();
        key.push(Part::Key("name".into()));
        scan.string(&key).map(|token| token.text.clone())
    };

    let exact: Vec<KeyPath> = items.iter().filter(|item| name_of(item).as_deref() == Some(name)).cloned().collect();
    if !exact.is_empty() {
        return exact;
    }

    items
        .into_iter()
        .filter(|item| name_of(item).is_some_and(|template| template_matches(&template, name)))
        .collect()
}

/// Whether a test name fits a matrix template: each `{param}` stands for any text.
fn template_matches(template: &str, name: &str) -> bool {
    if !template.contains('{') {
        return false;
    }

    let mut pattern = String::from("^");
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        pattern.push_str(&regex::escape(&rest[..start]));
        pattern.push_str(".*");
        rest = rest[start..].find('}').map_or("", |end| &rest[start + end + 1..]);
    }
    pattern.push_str(&regex::escape(rest));
    pattern.push('$');

    Regex::new(&pattern).is_ok_and(|pattern| pattern.is_match(name))
}

/// A needle in the tokens under any of `paths`: its span, and the misspelled word's (a word inside a value).
fn find_needle(file: &SourceFile, paths: &[KeyPath], needle: &Needle) -> Option<(Range<usize>, Option<Range<usize>>)> {
    let scan = &file.scan;

    if let Needle::LastValue(value) = needle {
        return paths
            .iter()
            .flat_map(|path| scan.values_under(path))
            .filter(|token| token.kind == TokenKind::String && token.text == *value)
            .last()
            .map(|token| (token.content.clone(), None));
    }

    for path in paths {
        let found = match needle {
            Needle::Value(value) | Needle::LastValue(value) => scan
                .values_under(path)
                .find(|token| token.kind == TokenKind::String && token.text == *value)
                .map(|token| (token.content.clone(), Some(token.content.clone()))),
            // The package part of `pkg/target` (the word a did-you-mean replaces)
            Needle::Prefix(prefix) => scan
                .values_under(path)
                .find(|token| token.kind == TokenKind::String && token.text.starts_with(prefix.as_str()))
                .map(|token| {
                    let package = token.content.start..token.content.start + prefix.trim_end_matches('/').len();
                    (package.clone(), Some(package))
                }),
            Needle::PathEnd(end) => {
                let end = end.to_lowercase();
                scan.values_under(path)
                    .filter(|token| token.kind == TokenKind::String)
                    .find(|token| {
                        let written = token
                            .text
                            .trim_start_matches("./")
                            .trim_end_matches('/')
                            .replace('\\', "/")
                            .to_lowercase();
                        !written.is_empty() && (end == written || end.ends_with(&format!("/{written}")))
                    })
                    .map(|token| (token.content.clone(), None))
            }
            Needle::Key(key) => scan
                .keys_under(path)
                .find(|token| token.path.len() == path.len() + 1 && token.text == *key)
                .map(|token| (token.span.clone(), Some(token.content.clone()))),
            Needle::Condition(condition) => conditions_under(scan, path)
                .find(|token| token.text == *condition)
                .map(|token| (token.content.clone(), None)),
            Needle::Word(word) => conditions_under(scan, path).find_map(|token| {
                let raw = &file.text[token.content.clone()];
                let start = find_word(raw, word)?;
                let span = token.content.start + start..token.content.start + start + word.len();
                Some((span.clone(), Some(span)))
            }),
        };
        if found.is_some() {
            return found;
        }
    }

    None
}

/// The condition texts under a path: `when` keys and `if` values.
fn conditions_under<'a>(scan: &'a Scan, path: &'a [Part]) -> impl Iterator<Item = &'a scan::Token> + 'a {
    scan.tokens.iter().filter(move |token| {
        if !token.path.starts_with(path) || token.path.len() < 2 {
            return false;
        }
        let parent = &token.path[token.path.len() - 2];
        match token.kind {
            TokenKind::Header | TokenKind::Key => *parent == Part::Key("when".into()),
            TokenKind::String => token.path.last() == Some(&Part::Key("if".into())),
            TokenKind::Bare => false,
        }
    })
}

/// A condition word in a text, as a whole word.
pub fn find_word(text: &str, word: &str) -> Option<usize> {
    let is_word = |character: char| character.is_ascii_alphanumeric() || "_-.+".contains(character);

    text.match_indices(word).map(|(start, _)| start).find(|start| {
        let before = text[..*start].chars().next_back();
        let after = text[start + word.len()..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(is_word)
    })
}

fn patterns() -> &'static [(Regex, Rule)] {
    static PATTERNS: OnceLock<Vec<(Regex, Rule)>> = OnceLock::new();

    PATTERNS.get_or_init(|| {
        RULES
            .iter()
            .map(|(pattern, rule)| (Regex::new(pattern).expect("a valid pattern"), *rule))
            .collect()
    })
}

/// What a message pattern's captures are.
#[derive(Debug, Clone, Copy)]
enum Rule {
    Target,
    TargetKey,
    Preset,
    Defaults,
    Profile,
    When,
    Test,
    Write,
    Dependency,
    DependencyKey,
    KeyPrefix,
    OptionKey,
    ProjectKey,
    WorkspaceMember,
    Package,
    UnknownField,
    ConditionWord,
    Condition,
    PresetValue,
    KeyValue,
    KeyColonValue,
    KeyColonWord,
    TestSetting,
    TargetOnlyKeys,
    KindPlacement,
    UnknownDependency,
    UnknownPackage,
    PackageTarget,
    PackageLibraries,
    DependsOnItself,
    WholeArchive,
    Cycle,
    TwoTargets,
    TargetWouldWrite,
    TwoPackages,
    BothDeps,
    DevDepUse,
    NothingToCompile,
    TwoTestsOneFile,
    TwoTests,
    AfterUnknown,
    AfterCircle,
    UnknownProfile,
    ProfileCycle,
    UnusedDependency,
    MissingInclude,
    NoFiles,
    FileNotFound,
    BadPattern,
    OptionName,
    ProfileName,
    TestCommand,
}

const RULES: &[(&str, Rule)] = &[
    // Context layers: tables
    (r"^in \[target\.(.+)\]$", Rule::Target),
    (r"^in target `([^`]+)`(?: \(([a-z-]+)\))?$", Rule::TargetKey),
    (r"^target `([^`]+)`$", Rule::Target),
    (r"^in \[preset\.(.+)\]$", Rule::Preset),
    (r"^in \[defaults\]$", Rule::Defaults),
    (r"^in \[profile\.(.+)\]$", Rule::Profile),
    (r#"^in when\."(.*)"$"#, Rule::When),
    (r"^in \[\[test\]\] `(.*)`$", Rule::Test),
    (r"^(?:in )?\[\[write\]\] (.*)$", Rule::Write),
    (r"^in the workspace (.*)$", Rule::Package),
    (r"^package `([^`]+)` \((.*)\)$", Rule::Package),
    (r"^dependency `([^`]+)`$", Rule::Dependency),
    // Leaf messages
    (r"unknown field `([^`]+)`", Rule::UnknownField),
    (r"^unknown condition `([^`]+)`", Rule::ConditionWord),
    (r#"^condition "(.*)": "#, Rule::Condition),
    (r"^empty condition", Rule::Condition),
    (r"^dependency `([^`]+)`(?:: (?:unknown field )?`([a-z-]+)`)?", Rule::DependencyKey),
    (r"^option `([^`]+)`: unknown key `([^`]+)`", Rule::OptionKey),
    (r"^option `([^`]+)`", Rule::OptionName),
    (r"^profile `([^`]+)` inherits from itself", Rule::ProfileCycle),
    (r"^profile `([^`]+)`", Rule::ProfileName),
    (r"^unknown profile `([^`]+)`", Rule::UnknownProfile),
    (r"^\[project\] ([a-z-]+)", Rule::ProjectKey),
    (r#"^workspace member "([^"]*)""#, Rule::WorkspaceMember),
    (r#"^preset = "([^"]*)": there is no"#, Rule::PresetValue),
    (r"^`([a-z-]+)` is a test setting", Rule::TestSetting),
    (r"^dir, if, default and preset belong to", Rule::TargetOnlyKeys),
    (r"^kind belongs to", Rule::KindPlacement),
    (r"^unknown dependency `([^`]+)`", Rule::UnknownDependency),
    (r"^unknown package `([^`]+)`", Rule::UnknownPackage),
    (r"^`([^`/]+)/\.\.\.`: ", Rule::UnknownPackage),
    (r"^package `([^`]+)` has no target `([^`]+)`", Rule::PackageTarget),
    (r"^package `([^`]+)` has (?:several libraries|no library target)", Rule::PackageLibraries),
    (r"^target `([^`]+)` depends on itself", Rule::DependsOnItself),
    (r"^target `([^`]+)`: whole-archive `([^`]+)`", Rule::WholeArchive),
    (r"^dependency cycle: (.*)$", Rule::Cycle),
    (r"^targets `([^`]+)` and `([^`]+)` (?:would|both)", Rule::TwoTargets),
    (r"^target `([^`]+)` would write .*, which the .* of `([^`]+)`", Rule::TargetWouldWrite),
    (r"^two packages are named `([^`]+)`: (.+) and (.+), so", Rule::TwoPackages),
    (r"^`([^`]+)` is under both \[deps\] and \[dev-deps\]", Rule::BothDeps),
    (
        r"^target `([^`]+)` \([a-z]+\) uses `([^`]+)`, which is under \[dev-deps\]",
        Rule::DevDepUse,
    ),
    (r"^target `([^`]+)` has nothing to compile", Rule::NothingToCompile),
    (r"^two tests are named `([^`]+)` in (.+); test names", Rule::TwoTestsOneFile),
    (r"^two tests are named `([^`]+)`, in (.+) and in (.+?)(?:;|$)", Rule::TwoTests),
    (r#"^test `([^`]+)`: after = \["([^"]*)"\]"#, Rule::AfterUnknown),
    (r"^test `([^`]+)`: `command`", Rule::TestCommand),
    (r"^tests wait for each other in a circle: (.*) \(", Rule::AfterCircle),
    (r"^`\[(deps|dev-deps)\] ([^`]+)`: no (?:target|test) uses it", Rule::UnusedDependency),
    (r"^include folder (.+) does not exist \(included by (.*)\)$", Rule::MissingInclude),
    (r#"^(?:data: )?"(.*)" matches no files"#, Rule::NoFiles),
    (r"^file not found: (.*)$", Rule::FileNotFound),
    (r#"^(?:data: )?bad (?:file )?pattern "!?(.*)"$"#, Rule::BadPattern),
    (
        r#"^(?:\[\[(?:run|pre-build|post-build)\]\]: )?([a-z][a-z-]*) = \[?"([^"]*)""#,
        Rule::KeyValue,
    ),
    (r"^([a-z][a-z-]*) = (\S+?)[:;,]", Rule::KeyValue),
    (r#"^([a-z][a-z-]*): "([^"]*)""#, Rule::KeyColonValue),
    (r"^([a-z][a-z-]*): [a-z ]*`([^`]+)`", Rule::KeyColonWord),
    (r"^([a-z][a-z-]*): ", Rule::KeyPrefix),
];

/// Reads the clues of every layer, outermost first.
fn read_clues(layers: &[String], files: &[SourceFile]) -> Clues {
    let mut clues = Clues {
        file: find_file(layers, files),
        ..Default::default()
    };

    for layer in layers {
        // A file's path in front (`C:/x/anvil.toml: ...`) is not part of the message's own words
        let text = strip_file_prefix(layer, files);
        let Some((pattern, rule)) = patterns().iter().find(|(pattern, _)| pattern.is_match(text)) else {
            continue;
        };
        let captures = pattern.captures(text).unwrap();
        let capture = |index: usize| captures.get(index).map(|capture| capture.as_str().to_string()).unwrap_or_default();
        read_rule(&mut clues, *rule, &capture, files);
    }

    clues
}

fn read_rule(clues: &mut Clues, rule: Rule, capture: &dyn Fn(usize) -> String, files: &[SourceFile]) {
    let path = |names: &[&str]| Scope::Path(scan::keys(names));

    match rule {
        Rule::Target => target_scope(clues, &capture(1)),
        Rule::TargetKey => {
            target_scope(clues, &capture(1));
            if !capture(2).is_empty() {
                clues.key = Some(capture(2));
            }
        }
        Rule::KeyPrefix => {
            if clues.key.is_none() {
                clues.key = Some(capture(1));
            }
        }
        Rule::Preset => clues.scope.push(path(&["preset", &capture(1)])),
        Rule::Defaults => clues.scope.push(path(&["defaults"])),
        Rule::Profile => clues.scope.push(path(&["profile", &capture(1)])),
        Rule::When => clues.scope.push(path(&["when", &capture(1)])),
        Rule::Test => clues.scope.push(Scope::Test(capture(1))),
        Rule::Write => clues.scope.push(Scope::Write(capture(1))),
        Rule::Package => {
            if let Some(file) = files.iter().position(|file| file.package == capture(1)) {
                clues.file.get_or_insert(file);
            }
        }
        Rule::Dependency => clues.scope.push(Scope::Dependency(capture(1))),
        Rule::DependencyKey => {
            if !clues.scope.iter().any(|scope| matches!(scope, Scope::Dependency(_))) {
                clues.scope.push(Scope::Dependency(capture(1)));
            }
            if !capture(2).is_empty() {
                clues.needle = Some(Needle::Key(capture(2)));
            }
        }
        Rule::OptionName => {
            clues.scope = vec![path(&["options"])];
            clues.needle = Some(Needle::Key(capture(1)));
        }
        Rule::OptionKey => {
            clues.scope = vec![path(&["options", &capture(1)])];
            clues.needle = Some(Needle::Key(capture(2)));
        }
        Rule::ProfileName => {
            clues.scope = vec![path(&["profile"])];
            clues.needle = Some(Needle::Key(capture(1)));
        }
        Rule::ProfileCycle => {
            clues.scope = vec![path(&["profile", &capture(1)])];
            clues.key = Some("inherits".into());
        }
        Rule::UnknownProfile => {
            clues.key = Some("inherits".into());
            clues.needle = Some(Needle::Value(capture(1)));
        }
        Rule::ProjectKey => {
            clues.scope = vec![path(&["project"])];
            clues.key = Some(capture(1));
        }
        Rule::WorkspaceMember => {
            clues.scope = vec![path(&["workspace"])];
            clues.key = Some("members".into());
            clues.needle = Some(Needle::Value(capture(1)));
        }
        Rule::UnknownField => clues.needle = Some(Needle::Key(capture(1))),
        Rule::ConditionWord => clues.needle = Some(Needle::Word(capture(1))),
        Rule::Condition => clues.needle = Some(Needle::Condition(capture(1))),
        Rule::PresetValue => {
            clues.key = Some("preset".into());
            clues.needle = Some(Needle::Value(capture(1)));
        }
        Rule::KeyValue | Rule::KeyColonValue => {
            clues.key = Some(capture(1));
            clues.needle = Some(Needle::Value(capture(2)));
        }
        Rule::KeyColonWord => {
            clues.key = Some(capture(1));
            clues.needle = Some(Needle::Value(capture(2)));
        }
        Rule::TestSetting => clues.key = Some(capture(1)),
        Rule::TargetOnlyKeys => clues.needle = Some(Needle::Key(first_present_key(clues, files, &["dir", "if", "default", "preset"]))),
        Rule::KindPlacement => clues.key = Some("kind".into()),
        Rule::UnknownDependency => clues.needle = Some(Needle::Value(capture(1))),
        Rule::UnknownPackage => clues.needle = Some(Needle::Prefix(format!("{}/", capture(1)))),
        Rule::PackageTarget => {
            let qualified = format!("{}/{}", capture(1), capture(2));
            match clues.needle {
                Some(_) => {}
                None => clues.needle = Some(Needle::Value(qualified)),
            }
        }
        Rule::PackageLibraries => {
            if clues.needle.is_none() {
                clues.needle = Some(Needle::Value(capture(1)));
            }
        }
        Rule::DependsOnItself => {
            clues.scope = vec![path(&["target", &capture(1)])];
            clues.needle = Some(Needle::Value(capture(1)));
        }
        Rule::WholeArchive => {
            target_scope(clues, &capture(1));
            clues.key = Some("whole-archive".into());
            clues.needle = Some(Needle::Value(capture(2)));
        }
        Rule::Cycle => {
            let names: Vec<String> = capture(1).split(" -> ").map(str::to_string).collect();
            if let [first, second, ..] = names.as_slice() {
                clues.scope = vec![Scope::Path(scan::keys(&["target", first]))];
                clues.needle = Some(Needle::Value(second.clone()));
                for pair in names.windows(2).skip(1) {
                    clues.related.push((
                        vec![Scope::Path(scan::keys(&["target", &pair[0]]))],
                        Some(Needle::Value(pair[1].clone())),
                        format!("`{}` depends on `{}`", pair[0], pair[1]),
                    ));
                }
            }
        }
        Rule::TwoTargets | Rule::TargetWouldWrite => {
            target_scope(clues, &capture(2));
            let (_, name) = split_qualified(&capture(1));
            clues
                .related
                .push((vec![Scope::Path(scan::keys(&["target", &name]))], None, format!("`{}`", capture(1))));
        }
        Rule::TwoPackages => {
            let second = Path::new(&capture(3)).join(crate::project::MANIFEST_NAME);
            let first = Path::new(&capture(2)).join(crate::project::MANIFEST_NAME);
            clues.file = file_index(files, &second).or(clues.file);
            clues.scope = vec![path(&["project"])];
            clues.key = Some("name".into());
            if let Some(first) = file_index(files, &first) {
                clues.related.push((
                    vec![Scope::Path(scan::keys(&["project"]))],
                    None,
                    format!("the other `{}`: {}", capture(1), util::slash(&files[first].path)),
                ));
            }
        }
        Rule::BothDeps => {
            clues.scope = vec![path(&["dev-deps", &capture(1)])];
            clues
                .related
                .push((vec![Scope::Path(scan::keys(&["deps", &capture(1)]))], None, "under [deps] too".into()));
        }
        Rule::DevDepUse => {
            target_scope(clues, &capture(1));
            clues.needle = Some(Needle::Value(capture(2)));
            clues.related.push((
                vec![Scope::Path(scan::keys(&["dev-deps", &capture(2)]))],
                None,
                "declared under [dev-deps]".into(),
            ));
        }
        Rule::NothingToCompile => {
            target_scope(clues, &capture(1));
            clues.key = Some("sources".into());
        }
        Rule::TwoTestsOneFile => {
            // The second name, and the first beside it
            clues.scope = vec![Scope::Named(capture(1))];
            clues.key = Some("name".into());
            clues.needle = Some(Needle::LastValue(capture(1)));
            clues.related.push((
                vec![Scope::Named(capture(1))],
                Some(Needle::Value(capture(1))),
                format!("the other `{}`", capture(1)),
            ));
        }
        Rule::TwoTests => {
            clues.file = place_file(files, &capture(3)).or(clues.file);
            clues.scope = vec![Scope::Named(capture(1))];
            clues.key = Some("name".into());
            clues.needle = Some(Needle::Value(capture(1)));
            if let Some(first) = place_file(files, &capture(2)) {
                clues.related.push((
                    vec![Scope::Named(capture(1))],
                    Some(Needle::Value(capture(1))),
                    format!("the other `{}`: {}", capture(1), util::slash(&files[first].path)),
                ));
            }
        }
        Rule::AfterUnknown => {
            clues.scope = vec![Scope::Named(last_part(&capture(1)))];
            clues.key = Some("after".into());
            clues.needle = Some(Needle::Value(capture(2)));
        }
        Rule::TestCommand => {
            clues.scope = vec![Scope::Test(capture(1))];
            clues.key = Some("command".into());
        }
        Rule::AfterCircle => {
            let names: Vec<String> = capture(1).split(" -> ").map(last_part).collect();
            if let [first, second, ..] = names.as_slice() {
                clues.scope = vec![Scope::Named(first.clone())];
                clues.key = Some("after".into());
                clues.needle = Some(Needle::Value(second.clone()));
            }
        }
        Rule::UnusedDependency => {
            clues.scope = vec![path(&[&capture(1), &capture(2)])];
        }
        Rule::MissingInclude => {
            let users: Vec<String> = capture(2).split(", ").map(|user| user.trim_matches('`').to_string()).collect();
            if let Some(user) = users.first() {
                target_scope(clues, user);
            }
            clues.needle = Some(Needle::PathEnd(capture(1)));
        }
        Rule::NoFiles => clues.needle = Some(Needle::Value(capture(1))),
        Rule::FileNotFound => clues.needle = Some(Needle::PathEnd(capture(1))),
        Rule::BadPattern => clues.needle = Some(Needle::Value(capture(1))),
    }
}

/// The scope of a target `name` or `package/name`.
fn target_scope(clues: &mut Clues, qualified: &str) {
    let (package, name) = split_qualified(qualified);
    if package.is_some() {
        clues.package = package;
    }
    if !clues
        .scope
        .iter()
        .any(|scope| matches!(scope, Scope::Path(path) if path.first() == Some(&Part::Key("target".into()))))
    {
        clues.scope.insert(0, Scope::Path(scan::keys(&["target", &name])));
    }
}

fn split_qualified(qualified: &str) -> (Option<String>, String) {
    match qualified.split_once('/') {
        Some((package, name)) => (Some(package.to_string()), name.to_string()),
        None => (None, qualified.to_string()),
    }
}

fn last_part(name: &str) -> String {
    name.rsplit('/').next().unwrap_or(name).to_string()
}

/// The first of `keys` the scope's table has (for messages that list several).
fn first_present_key(clues: &Clues, files: &[SourceFile], keys: &[&str]) -> String {
    for file in files {
        let Some((paths, _)) = resolve_scope(file, &clues.scope) else { continue };
        for path in &paths {
            let present = file.scan.children(path);
            if let Some(key) = keys.iter().find(|key| present.iter().any(|present| present == *key)) {
                return key.to_string();
            }
        }
    }

    keys[0].to_string()
}

/// The file a message names by its path, the longest match first.
fn find_file(layers: &[String], files: &[SourceFile]) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;

    for layer in layers {
        let lowered = layer.to_lowercase();
        for (index, file) in files.iter().enumerate() {
            let written = util::slash(&file.path).to_lowercase();
            if lowered.contains(&written) && best.is_none_or(|(_, length)| written.len() > length) {
                best = Some((index, written.len()));
            }
        }
        if best.is_some() {
            return best.map(|(index, _)| index);
        }
    }

    None
}

/// A layer without the file path in front of it.
fn strip_file_prefix<'a>(layer: &'a str, files: &[SourceFile]) -> &'a str {
    for file in files {
        let written = util::slash(&file.path);
        if layer.len() > written.len() + 2 && layer[..written.len()].eq_ignore_ascii_case(&written) && layer[written.len()..].starts_with(": ") {
            return &layer[written.len() + 2..];
        }
    }

    layer
}

fn file_index(files: &[SourceFile], path: &Path) -> Option<usize> {
    let key = util::path_key(path);
    files.iter().position(|file| util::path_key(&file.path) == key)
}

/// The file a test's place names: a path relative to the root, or `[target.x] in <path>`.
fn place_file(files: &[SourceFile], place: &str) -> Option<usize> {
    let written = place.rsplit(" in ").next().unwrap_or(place).replace('\\', "/").to_lowercase();
    files.iter().position(|file| util::slash(&file.path).to_lowercase().ends_with(&written))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(texts: &[(&str, &str, &str)]) -> Vec<SourceFile> {
        texts
            .iter()
            .map(|(path, text, package)| SourceFile::new(PathBuf::from(path), text.to_string(), package.to_string()))
            .collect()
    }

    fn located(layers: &[&str], files: &[SourceFile]) -> (usize, String) {
        let layers: Vec<String> = layers.iter().map(|layer| layer.to_string()).collect();
        let location = locate(&layers, files, 0);
        (location.file, files[location.file].text[location.span].to_string())
    }

    #[test]
    fn context_chains_become_spans() {
        let manifest = r#"[project]
name = "game"

[deps]
fmt = "vcpkg"
zlib = { vcpkg = "zlib", tag = "v1" }

[target.core]
kind = "lib"
when.lnux.defines = ["X"]

[target.app]
kind = "exe2"
deps = ["core", "fmtt", "lib/thing"]
preset = "fast"
if = "windows & msvcc"
"#;
        let tests = "[[test]]\nname = \"unit\"\ncommand = \"{target:app}\"\nafter = [\"nope\"]\n";
        let files = files(&[("C:/p/anvil.toml", manifest, "game"), ("C:/p/tests/tests.toml", tests, "game")]);

        assert_eq!(
            located(&["C:/p/anvil.toml", "in [target.app]", "kind = \"exe2\": expected exe, lib"], &files),
            (0, "exe2".into())
        );
        assert_eq!(
            located(&["in target `app`", "unknown dependency `fmtt` (did you mean `fmt`?)"], &files),
            (0, "fmtt".into())
        );
        assert_eq!(
            located(
                &["C:/p/anvil.toml", "in [target.core]", "in when.\"lnux\"", "unknown condition `lnux`"],
                &files
            ),
            (0, "lnux".into())
        );
        assert_eq!(
            located(&["C:/p/anvil.toml", "in [target.app]", "unknown condition `msvcc`"], &files),
            (0, "msvcc".into())
        );
        assert_eq!(
            located(
                &["C:/p/anvil.toml", "in [target.app]", "preset = \"fast\": there is no [preset.fast]"],
                &files
            ),
            (0, "fast".into())
        );
        assert_eq!(
            located(
                &["C:/p/anvil.toml", "dependency `zlib`: `tag` picks a commit of a git dependency"],
                &files
            ),
            (0, "tag".into())
        );
        assert_eq!(
            located(&["in target `app`", "unknown package `lib`. Name a path"], &files),
            (0, "lib".into())
        );
        assert_eq!(located(&["test `unit`: after = [\"nope\"] names no test"], &files), (1, "nope".into()));
        assert_eq!(
            located(&["C:/p/anvil.toml: `[deps] zlib`: no target uses it; add it"], &files),
            (0, "zlib".into())
        );
        assert_eq!(located(&["dependency cycle: app -> core -> app"], &files), (0, "core".into()));

        // Nothing to go by: the first line of the root manifest
        assert_eq!(located(&["something odd"], &files), (0, "[project]".into()));
    }

    #[test]
    fn matrix_templates_and_words() {
        assert!(template_matches("stills_{path}_{view}", "stills_rt_glass"));
        assert!(!template_matches("stills_{path}", "other_rt"));
        assert_eq!(find_word("windows & !winx", "win"), None);
        assert_eq!(find_word("linux | win", "win"), Some(8));
    }
}
