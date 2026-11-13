//! One check of a project, as `anvil targets` loads it (nothing fetched, installed, built or written), on the
//! editor's unsaved text: TOML errors with the parser's own spans, every unknown key and wrong type (the parser
//! stops at the first, so each is blanked out and the text parsed again), then the project model's checks and
//! warnings, each placed by crate::lsp::locate.

use super::locate::{self, SourceFile};
use super::scan::{self, Part};
use super::schema_info::FileKind;
use super::session::{self, Session, Walks};
use crate::cond::CondContext;
use crate::project::{self, Model, MANIFEST_NAME, TESTS_FILE_NAME};
use crate::util;
use regex::Regex;
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

// How many unknown keys or wrong values of one file are reported (each costs a parse)
const MAX_PARSE_ERRORS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Information,
}

/// A diagnostic in byte offsets of the text that was checked.
#[derive(Debug, Clone)]
pub struct Problem {
    pub file: PathBuf,
    pub span: Range<usize>,
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    pub related: Vec<(PathBuf, Range<usize>, String)>,
    /// A did-you-mean: the misspelled word's span and what to write instead.
    pub replacement: Option<(Range<usize>, String)>,
}

/// What the build settings of one check are: the language server's configuration.
#[derive(Debug, Clone, Default)]
pub struct CheckSettings {
    pub profile: Option<String>,
    pub toolchain: Option<String>,
}

/// A target as hover and completion describe it.
#[derive(Debug, Clone)]
pub struct TargetInfo {
    pub name: String,
    pub package: String,
    pub qualified: String,
    pub kind: &'static str,
    pub output: Option<PathBuf>,
    pub dir: PathBuf,
    pub sources: usize,
    pub deps: Vec<String>,
}

/// What the loaded model says, for hover and completion.
#[derive(Debug, Clone)]
pub struct ModelSummary {
    pub targets: Vec<TargetInfo>,
    pub tests: Vec<String>,
    pub condition: CondContext,
    pub toolchain: String,
    pub profile: String,
}

pub struct Analysis {
    pub root: PathBuf,
    /// Every manifest and tests.toml the check read, with the text it read.
    pub files: Vec<SourceFile>,
    pub problems: Vec<Problem>,
    pub model: Option<ModelSummary>,
    /// False when the compiler is not known yet, so only the files were parsed.
    pub complete: bool,
    pub elapsed: Duration,
}

/// Checks the project `start` (a document's folder) belongs to. `texts` are the open documents, by path key.
pub fn analyze(
    start: &Path,
    texts: &HashMap<String, (PathBuf, String)>,
    own: &[PathBuf],
    settings: &CheckSettings,
    load_model: bool,
    walks: &mut Walks,
) -> Analysis {
    let started = Instant::now();
    let session = Session {
        texts: texts.iter().map(|(key, (_, text))| (key.clone(), text.clone())).collect(),
        walks: std::mem::take(walks),
        ..Default::default()
    };

    let mut problems = Vec::new();
    let (outcome, mut session) = session::run(session, || check(start, texts, own, settings, load_model, &mut problems));
    let (root, loaded) = outcome;
    *walks = std::mem::take(&mut session.walks);

    // Every file read, open documents under the root, and the root manifest
    let mut paths: Vec<PathBuf> = vec![root.join(MANIFEST_NAME)];
    paths.extend(session.read.iter().cloned());
    paths.extend(
        texts
            .values()
            .map(|(path, _)| path.clone())
            .filter(|path| util::strip_prefix_ci(path, &root).is_some()),
    );
    let files = source_files(&paths, texts);

    // The load's error and warnings, placed in those files
    let mut model = None;
    let complete = loaded.is_some();
    match loaded {
        Some(Ok(loaded)) => model = Some(summarize(&loaded)),
        Some(Err(error)) => {
            let layers: Vec<String> = error.chain().map(|layer| layer.to_string()).collect();
            if !is_parse_error(&layers) {
                problems.push(problem(&layers, &files, Severity::Error));
            } else if !problems.iter().any(|problem| problem.severity == Severity::Error) {
                // A file the editor does not have open: parse it for the span
                problems.extend(parse_named_file(&layers, &files));
            }
        }
        None => {}
    }
    for warning in &session.warnings {
        problems.push(problem(std::slice::from_ref(warning), &files, Severity::Warning));
    }
    dedup(&mut problems);

    Analysis {
        root,
        files,
        problems,
        model,
        complete,
        elapsed: started.elapsed(),
    }
}

/// The analysis of a check that failed inside: one error on the first line of the root manifest.
pub fn failed(start: &Path, message: &str) -> Analysis {
    let root = start.to_path_buf();
    let manifest = root.join(MANIFEST_NAME);
    let text = std::fs::read_to_string(&manifest).unwrap_or_default();
    let problem = Problem {
        file: manifest.clone(),
        span: locate::first_line(&text),
        severity: Severity::Error,
        code: "internal-error",
        message: message.to_string(),
        related: Vec::new(),
        replacement: None,
    };

    Analysis {
        root,
        files: vec![SourceFile::new(manifest, text, String::new())],
        problems: vec![problem],
        model: None,
        complete: false,
        elapsed: Duration::ZERO,
    }
}

/// Parse checks of the open documents of the project, then (when they parse) the load; returns the root and the
/// load's outcome.
fn check(
    start: &Path,
    texts: &HashMap<String, (PathBuf, String)>,
    own: &[PathBuf],
    settings: &CheckSettings,
    load_model: bool,
    problems: &mut Vec<Problem>,
) -> (PathBuf, Option<anyhow::Result<Model>>) {
    let root = match project::locate(start) {
        Ok((root, _)) => root,
        Err(_) => start.to_path_buf(),
    };

    // Each open document: its syntax, keys and value types (reported for the project's own documents; the load
    // reads every one without what the parser refused)
    let mut parses = true;
    for (path, text) in texts.values() {
        let (found, repaired) = parse_problems(path, text);
        let owned = own.iter().any(|owned| util::path_key(owned) == util::path_key(path));
        if owned {
            problems.extend(found);
        }
        match repaired {
            Some(repaired) if repaired != *text => session::replace_text(path, repaired),
            Some(_) => {}
            None if owned && is_manifest(path) => parses = false,
            None => {}
        }
    }
    if !parses || !load_model {
        return (root, None);
    }

    // The load, as `anvil targets` does it
    let build = crate::BuildArgs {
        profile: settings.profile.clone(),
        toolchain: settings.toolchain.clone(),
        ..Default::default()
    };
    let loaded = crate::commands::load_model_for_listing(start, &build).and_then(|mut model| {
        model.collect_tests()?;
        Ok(model)
    });

    (root, Some(loaded))
}

/// A manifest or tests.toml checked as TOML and against its tables: the problems, and the text with each unknown key
/// or wrong value blanked out (None when it cannot be repaired: a syntax error or a missing key).
pub fn parse_problems(path: &Path, text: &str) -> (Vec<Problem>, Option<String>) {
    let mut problems = Vec::new();

    // Syntax first: the whole text must be TOML
    if let Err(error) = toml_edit::Document::parse(text) {
        let span = error.span().unwrap_or(0..0);
        problems.push(Problem {
            file: path.to_path_buf(),
            span: widen(text, span),
            severity: Severity::Error,
            code: "toml-syntax",
            message: first_paragraph(error.message()),
            related: Vec::new(),
            replacement: None,
        });
        return (problems, None);
    }

    // Then the tables, each error blanked out in turn so the next shows
    let mut repaired = text.to_string();
    for _ in 0..MAX_PARSE_ERRORS {
        let Some((message, span)) = deserialize(path, &repaired) else {
            return (problems, Some(repaired));
        };
        let span = widen(text, span.unwrap_or(0..0));
        let message = shorten(&crate::manifest::explain_unknown_key(&message));
        let replacement = suggestion(&message).map(|word| (key_word(text, &span), word));
        problems.push(Problem {
            file: path.to_path_buf(),
            span: span.clone(),
            severity: Severity::Error,
            code: parse_code(&message),
            message,
            related: Vec::new(),
            replacement,
        });

        // Blank out what the error is about; one that cannot be blanked out ends the parse
        let Some(blanked) = blank_out(&repaired, &span) else {
            return (problems, None);
        };
        repaired = blanked;
    }

    (problems, None)
}

/// The deserialize error of a manifest's or tests.toml's text: its message and span.
fn deserialize(path: &Path, text: &str) -> Option<(String, Option<Range<usize>>)> {
    if !is_manifest(path) {
        let error = crate::manifest::parse_tests(text).err()?;
        let span = error.downcast_ref::<toml::de::Error>().and_then(|toml_error| toml_error.span());
        let message = match error.downcast_ref::<toml::de::Error>() {
            Some(toml_error) => toml_error.message().to_string(),
            None => format!("{error:#}"),
        };
        return Some((message, span));
    }

    // The manifest's own checks after the tables (the project name, empty commands) have no span
    match toml::from_str::<crate::manifest::Manifest>(text) {
        Ok(_) => match crate::manifest::parse_str(text) {
            Ok(_) => None,
            Err(error) => Some((format!("{error:#}"), None)),
        },
        Err(error) => Some((error.message().to_string(), error.span())),
    }
}

/// The text with the entry or table at `span` replaced by spaces (line breaks kept, so offsets stay the same).
fn blank_out(text: &str, span: &Range<usize>) -> Option<String> {
    let scan = scan::scan(text);
    if span.is_empty() {
        return None;
    }

    // The innermost entry the span is in (a key, or its value), else a table whose header it is in
    let entry = scan
        .entries
        .iter()
        .filter(|entry| overlaps(&(entry.key_span.start..entry.value_span.end), span))
        .min_by_key(|entry| entry.value_span.end - entry.key_span.start)
        .map(|entry| entry.key_span.start..entry.value_span.end);
    let table = || {
        scan.headers
            .iter()
            .find(|header| overlaps(&header.span, span))
            .map(|header| header.span.start..header.end)
    };
    let blank = entry.or_else(table)?;
    if blank.len() >= text.len() {
        return None;
    }

    let mut blanked = text.to_string();
    let spaces: String = text[blank.clone()]
        .chars()
        .map(|character| if character == '\n' { '\n' } else { ' ' })
        .collect();
    blanked.replace_range(blank, &spaces);
    Some(blanked)
}

fn overlaps(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end.max(right.start + 1) && right.start < left.end
}

/// An empty span (an error at the end of the text) made one character wide where it can be.
fn widen(text: &str, span: Range<usize>) -> Range<usize> {
    let start = span.start.min(text.len());
    let end = span.end.clamp(start, text.len());
    if end > start {
        return start..end;
    }

    let next = text[start..].chars().next().map_or(0, char::len_utf8);
    start..start + next
}

/// The key a did-you-mean replaces: the span without the quotes of a quoted key.
fn key_word(text: &str, span: &Range<usize>) -> Range<usize> {
    let written = &text[span.clone()];
    if written.len() >= 2 && (written.starts_with('"') || written.starts_with('\'')) {
        return span.start + 1..span.end - 1;
    }

    span.clone()
}

fn first_paragraph(message: &str) -> String {
    message.trim().lines().next().unwrap_or_default().to_string()
}

/// serde's "unknown field `x` (did you mean `y`?), expected one of `a`, `b`, ...": the list left out (completion
/// offers the keys).
fn shorten(message: &str) -> String {
    match message.find(", expected one of") {
        Some(cut) if message.starts_with("unknown field") => {
            let rest = &message[cut..];
            let hint = rest.find("\nhint:").map_or("", |start| &rest[start..]);
            format!("{}{hint}", &message[..cut])
        }
        _ => message.to_string(),
    }
}

/// The word a "(did you mean `x`?)" suggests.
pub fn suggestion(message: &str) -> Option<String> {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| Regex::new(r"\(did you mean `([^`]+)`\?\)").unwrap());

    pattern.captures(message).map(|captures| captures[1].to_string())
}

fn parse_code(message: &str) -> &'static str {
    match message {
        _ if message.starts_with("unknown field") => "unknown-key",
        _ if message.starts_with("invalid type") => "wrong-type",
        _ if message.starts_with("missing field") => "missing-key",
        _ if message.starts_with("duplicate") => "duplicate-key",
        _ if message.starts_with("unknown variant") || message.starts_with("invalid value") => "invalid-value",
        _ => code(message),
    }
}

// Message patterns and their diagnostic codes, the first match wins
const CODES: &[(&str, &str)] = &[
    (r"unknown field", "unknown-key"),
    (r"unknown condition", "unknown-condition-word"),
    (r#"condition "|empty condition"#, "bad-condition"),
    (r"unknown dependency", "unknown-dependency"),
    (r"unknown package|is not a path or git package", "unknown-package"),
    (r"has no target `|no target named", "unknown-target"),
    (r"has several libraries|has no library target", "ambiguous-package-target"),
    (r"there is no \[preset\.", "unknown-preset"),
    (r"unknown profile", "unknown-profile"),
    (r"inherits from itself", "profile-cycle"),
    (r"is already a condition word|has the name of a profile", "reserved-name"),
    (r"unknown option|no package loaded here declares the option", "unknown-option"),
    (r"^option `", "bad-option"),
    (r"dependency cycle|depends on itself", "dependency-cycle"),
    (r"would both write|would share|needs as a folder|both copy a data file", "output-conflict"),
    (r"both take the sources", "shared-sources"),
    (r"whole-archive", "invalid-whole-archive"),
    (r"\[\[write\]\]", "invalid-write"),
    (r"two packages are named", "duplicate-package"),
    (r"two tests are named|would both be the test", "duplicate-test"),
    (r"names no test", "unknown-test"),
    (r"wait for each other in a circle", "test-cycle"),
    (r"under both \[deps\] and \[dev-deps\]", "duplicate-dependency"),
    (r"which is under \[dev-deps\]", "dev-dep-outside-tests"),
    (r"has nothing to compile|^nothing to build", "nothing-to-compile"),
    (r"is a test setting", "test-setting-on-non-test"),
    (r"belongs? to a \[target\] table", "misplaced-key"),
    (r"is read only from the workspace root", "root-only-key"),
    (r"workspace member|in the workspace", "workspace-member"),
    (r"\[project\] version", "invalid-version"),
    (r"\[project\] name", "invalid-name"),
    (
        r"\[project\] out|`anvil clean --all` would delete|holds a package \(",
        "invalid-output-folder",
    ),
    (r"`command` is empty", "empty-command"),
    (r"is not a valid regex", "invalid-regex"),
    (r"no (?:target|test) uses it", "unused-dep"),
    (r"include folder .* does not exist", "missing-include-folder"),
    (r"has a CMakeLists\.txt but no", "cmake-only-path-dep"),
    (r"matches no files", "glob-matches-nothing"),
    (r"^data: |: data: ", "invalid-data"),
    (r"pkg-config|does not know `", "pkg-config"),
    (r"file not found|does not exist", "missing-path"),
    (r"is a folder", "folder-not-file"),
    (r"matrix", "invalid-matrix"),
    (r"^dependency `|: dependency `", "invalid-dependency"),
    (r"toolchain|compiler", "toolchain"),
    (r"expected|unknown sanitizer|bad (?:file )?pattern|a batch holds", "invalid-value"),
];

/// The stable code of a message.
pub fn code(message: &str) -> &'static str {
    static PATTERNS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| CODES.iter().map(|(pattern, code)| (Regex::new(pattern).unwrap(), *code)).collect());

    patterns
        .iter()
        .find(|(pattern, _)| pattern.is_match(message))
        .map_or("load-error", |(_, code)| code)
}

/// A load error's or warning's problem, placed in the files.
fn problem(layers: &[String], files: &[SourceFile], severity: Severity) -> Problem {
    let location = locate::locate(layers, files, 0);
    let file = &files[location.file];

    // The message without the file it is in (the editor shows that)
    let written = util::slash(&file.path);
    let parts: Vec<&str> = layers
        .iter()
        .map(|layer| layer.strip_prefix(&format!("{written}: ")).unwrap_or(layer))
        .filter(|layer| !layer.eq_ignore_ascii_case(&written))
        .collect();
    let message = parts.join(": ");
    let code = code(&message);
    let replacement = suggestion(&message).and_then(|word| location.word.clone().map(|span| (span, word)));
    let severity = match (severity, code) {
        (Severity::Warning, "toolchain") => Severity::Information,
        (severity, _) => severity,
    };

    Problem {
        file: file.path.clone(),
        span: location.span,
        severity,
        code,
        message,
        related: location
            .related
            .into_iter()
            .map(|(index, span, label)| (files[index].path.clone(), span, label))
            .collect(),
        replacement,
    }
}

/// A TOML error of a file the load read (not open in the editor): parsed again for its span.
fn parse_named_file(layers: &[String], files: &[SourceFile]) -> Vec<Problem> {
    let joined = layers.join(": ");
    let named = files
        .iter()
        .filter(|file| joined.contains(&util::slash(&file.path)))
        .max_by_key(|file| file.path.as_os_str().len());

    match named {
        Some(file) => {
            let (found, _) = parse_problems(&file.path, &file.text);
            if !found.is_empty() {
                return found;
            }
            vec![problem(layers, files, Severity::Error)]
        }
        None => vec![problem(layers, files, Severity::Error)],
    }
}

fn is_parse_error(layers: &[String]) -> bool {
    layers.iter().any(|layer| layer.contains("TOML parse error"))
}

fn is_manifest(path: &Path) -> bool {
    path.file_name().is_none_or(|name| name != TESTS_FILE_NAME)
}

/// The same problem once (a parse error both of an open document and of the load).
fn dedup(problems: &mut Vec<Problem>) {
    let mut seen = std::collections::HashSet::new();
    problems.retain(|problem| seen.insert((util::path_key(&problem.file), problem.span.clone(), problem.message.clone())));
}

/// The files as the check read them: open documents' text, else the file's.
fn source_files(paths: &[PathBuf], texts: &HashMap<String, (PathBuf, String)>) -> Vec<SourceFile> {
    let mut files: Vec<SourceFile> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for path in paths {
        let key = util::path_key(path);
        if !seen.insert(key.clone()) {
            continue;
        }
        let text = match texts.get(&key) {
            Some((_, text)) => text.clone(),
            None => match std::fs::read_to_string(path) {
                Ok(text) => text,
                Err(_) => continue,
            },
        };
        files.push(SourceFile::new(path.clone(), text, String::new()));
    }

    // Package names: a manifest's [project] name, else its folder's; a tests.toml's is its package's
    for file in files.iter_mut().filter(|file| file.kind == FileKind::Manifest) {
        file.package = package_name(file);
    }
    for index in 0..files.len() {
        if files[index].kind == FileKind::Tests {
            let owner = files
                .iter()
                .filter(|file| file.kind == FileKind::Manifest)
                .filter_map(|file| file.path.parent().map(|folder| (file, folder)))
                .filter(|(_, folder)| util::strip_prefix_ci(&files[index].path, folder).is_some())
                .max_by_key(|(_, folder)| folder.as_os_str().len())
                .map(|(file, _)| file.package.clone());
            files[index].package = owner.unwrap_or_default();
        }
    }

    files
}

/// A manifest's package name, as crate::project::package_name gives it.
pub fn package_name(file: &SourceFile) -> String {
    let declared = file.scan.string(&[Part::Key("project".into()), Part::Key("name".into())]);
    match declared {
        Some(token) => token.text.clone(),
        None => file
            .path
            .parent()
            .and_then(|folder| folder.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "app".into()),
    }
}

fn summarize(model: &Model) -> ModelSummary {
    let targets = model
        .targets
        .iter()
        .map(|target| {
            let package = model.packages[target.package].name.clone();
            let qualified = match target.package {
                0 => target.name.clone(),
                _ => format!("{package}/{}", target.name),
            };
            TargetInfo {
                name: target.name.clone(),
                package,
                qualified,
                kind: target.kind.name(),
                output: crate::plan::artifact_path(model, target),
                dir: target.dir.clone(),
                sources: target.sources.len(),
                deps: target
                    .deps
                    .iter()
                    .map(|(dependency, _)| model.targets[*dependency].name.clone())
                    .collect(),
            }
        })
        .collect();

    ModelSummary {
        targets,
        tests: model.tests.iter().map(|test| test.name.clone()).collect(),
        condition: model.condition.clone(),
        toolchain: format!("{} {}", model.toolchain.family.name(), model.toolchain.version),
        profile: model.profile.name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_unknown_key_with_its_span() {
        let text = "[project]\nname = \"x\"\nverison = \"1\"\n\n[target.app]\nincldue = [\"a\"]\nkind = 5\n";
        let (problems, repaired) = parse_problems(Path::new("C:/p/anvil.toml"), text);

        let found: Vec<(&str, &str)> = problems.iter().map(|problem| (problem.code, &text[problem.span.clone()])).collect();
        assert_eq!(found, vec![("unknown-key", "verison"), ("unknown-key", "incldue"), ("wrong-type", "5")]);
        assert_eq!(problems[0].replacement.as_ref().unwrap().1, "version");
        assert!(!problems[0].message.contains("expected one of"), "{}", problems[0].message);
        assert!(repaired.is_some());
    }

    #[test]
    fn syntax_errors_stop_the_parse() {
        let text = "[project\nname = \"x\"\n";
        let (problems, repaired) = parse_problems(Path::new("C:/p/anvil.toml"), text);
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].code, "toml-syntax");
        assert!(repaired.is_none());

        let (problems, _) = parse_problems(Path::new("C:/p/tests.toml"), "[[test]]\nname = \"a\"\ncommand = \"x\"\ntimout = 3\n");
        assert_eq!(problems[0].code, "unknown-key");
    }

    #[test]
    fn codes() {
        assert_eq!(code("unknown condition `lnux` (did you mean `linux`?)"), "unknown-condition-word");
        assert_eq!(code("in target `app`: unknown dependency `fmtt`"), "unknown-dependency");
        assert_eq!(code("`[deps] zlib`: no target uses it"), "unused-dep");
        assert_eq!(code("include folder inc does not exist (included by `app`)"), "missing-include-folder");
        assert_eq!(code("\"src/*.c\" matches no files (in C:/p)"), "glob-matches-nothing");
        assert_eq!(code("something new"), "load-error");
        assert_eq!(suggestion("unknown dependency `fmtt` (did you mean `fmt`?)").unwrap(), "fmt");
    }

    /// A project in a fresh folder under the system's temporary one: anvil.toml, tests/tests.toml when given, and a
    /// source file.
    fn project(name: &str, manifest: &str, tests: Option<&str>) -> PathBuf {
        let root = std::env::temp_dir().join(format!("anvil-lsp-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.cpp"), "int main() {}\n").unwrap();
        std::fs::write(root.join(MANIFEST_NAME), manifest).unwrap();
        if let Some(tests) = tests {
            std::fs::create_dir_all(root.join("tests")).unwrap();
            std::fs::write(root.join("tests").join(crate::project::TESTS_FILE_NAME), tests).unwrap();
        }

        root
    }

    /// The problems of a project as the language server finds them, with the text each one marks.
    fn checked(name: &str, manifest: &str, tests: Option<&str>) -> Vec<(&'static str, String, Severity, Vec<String>)> {
        let root = project(name, manifest, tests);
        let path = root.join(MANIFEST_NAME);
        let texts = HashMap::from([(util::path_key(&path), (path.clone(), manifest.to_string()))]);
        let analysis = analyze(&root, &texts, &[path], &CheckSettings::default(), true, &mut Walks::default());
        let _ = std::fs::remove_dir_all(&root);

        let text_of = |file: &Path, span: &Range<usize>| {
            let source = analysis
                .files
                .iter()
                .find(|source| util::path_key(&source.path) == util::path_key(file))
                .unwrap();
            source.text[span.clone()].to_string()
        };
        analysis
            .problems
            .iter()
            .map(|problem| {
                let related = problem.related.iter().map(|(file, span, _)| text_of(file, span)).collect();
                (problem.code, text_of(&problem.file, &problem.span), problem.severity, related)
            })
            .collect()
    }

    #[test]
    fn loader_problems_land_on_their_words() {
        const BASE: &str = "[project]\nname = \"game\"\n\n[deps]\nfmt = \"vcpkg\"\n\n[target.app]\ndeps = [\"fmt\"]\n";
        const NO_SOURCES: &str = "kind = \"lib\"\nsources = []\n";
        let error = Severity::Error;
        let warning = Severity::Warning;

        // (case, anvil.toml, tests.toml, code, the text marked, its severity, the text of the related places)
        type Case<'a> = (&'a str, String, Option<&'a str>, &'a str, &'a str, Severity, Vec<&'a str>);
        let cases: Vec<Case> = vec![
            (
                "condition",
                format!("{BASE}when.lnux.defines = [\"X\"]\n"),
                None,
                "unknown-condition-word",
                "lnux",
                error,
                vec![],
            ),
            (
                "dependency",
                BASE.replace("[\"fmt\"]", "[\"fmtt\"]"),
                None,
                "unknown-dependency",
                "fmtt",
                error,
                vec![],
            ),
            (
                "package",
                BASE.replace("[\"fmt\"]", "[\"fmt\", \"utl/x\"]"),
                None,
                "unknown-package",
                "utl",
                error,
                vec![],
            ),
            (
                "profile",
                format!("{BASE}\n[profile.ship]\ninherits = \"relase\"\n"),
                None,
                "unknown-profile",
                "relase",
                error,
                vec![],
            ),
            ("kind", format!("{BASE}kind = \"lbi\"\n"), None, "invalid-value", "lbi", error, vec![]),
            (
                "bad-if",
                format!("{BASE}\n[target.tool]\nif = \"linux &&\"\n"),
                None,
                "bad-condition",
                "linux &&",
                error,
                vec![],
            ),
            (
                "cycle",
                format!("{BASE}\n[target.a]\n{NO_SOURCES}deps = [\"b\"]\n\n[target.b]\n{NO_SOURCES}deps = [\"a\"]\n"),
                None,
                "dependency-cycle",
                "b",
                error,
                vec!["a"],
            ),
            (
                "shared",
                format!("{BASE}\n[target.tool]\n"),
                None,
                "shared-sources",
                "[target.tool]",
                error,
                vec!["[target.app]"],
            ),
            (
                "option",
                format!("{BASE}\n[options]\nfast = \"yes\"\n"),
                None,
                "bad-option",
                "fast",
                error,
                vec![],
            ),
            (
                "dev-dep",
                format!("{BASE}\n[dev-deps]\ngtest = \"vcpkg\"\n").replace("[\"fmt\"]", "[\"fmt\", \"gtest\"]"),
                None,
                "dev-dep-outside-tests",
                "gtest",
                error,
                vec!["gtest"],
            ),
            (
                "unused",
                BASE.replace("deps = [\"fmt\"]\n", ""),
                None,
                "unused-dep",
                "fmt",
                warning,
                vec![],
            ),
            (
                "include",
                format!("{BASE}include = [\"nope\"]\n"),
                None,
                "missing-include-folder",
                "nope",
                warning,
                vec![],
            ),
            (
                "glob",
                format!("{BASE}sources = [\"src/*.cpp\", \"gen/*.cpp\"]\n"),
                None,
                "glob-matches-nothing",
                "gen/*.cpp",
                warning,
                vec![],
            ),
            (
                "after",
                BASE.to_string(),
                Some("[[test]]\nname = \"smoke\"\ncommand = \"{target:app}\"\nafter = [\"nope\"]\n"),
                "unknown-test",
                "nope",
                error,
                vec![],
            ),
            (
                "twice",
                BASE.to_string(),
                Some("[[test]]\nname = \"smoke\"\ncommand = \"x\"\n\n[[test]]\nname = \"smoke\"\ncommand = \"y\"\n"),
                "duplicate-test",
                "smoke",
                error,
                vec!["smoke"],
            ),
        ];

        for (name, manifest, tests, code, marked, severity, related) in cases {
            let found = checked(name, &manifest, tests);
            let expected = vec![(
                code,
                marked.to_string(),
                severity,
                related.iter().map(|text| text.to_string()).collect::<Vec<_>>(),
            )];
            assert_eq!(found, expected, "case {name}");
        }
    }
}
