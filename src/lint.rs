//! `anvil fmt` and `anvil tidy`: clang-format over the packages' C and C++ files, clang-tidy over the targets' sources
//! with the profile's compile_commands.json. Both run one process per file, in parallel.

use crate::commands;
use crate::manifest::{self, Manifest};
use crate::project::{self, Model, MANIFEST_NAME};
use crate::targets::{self, Kind};
use crate::toolchain::{self, Family};
use crate::ui;
use crate::util::{self, slash};
use crate::BuildArgs;
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

// What `anvil fmt` formats: C, C++ and CUDA sources and headers
const FORMAT_EXTENSIONS: &[&str] = &[
    "c", "cc", "cpp", "cxx", "c++", "cppm", "ixx", "cu", "cuh", "h", "hh", "hpp", "hxx", "h++", "inl", "ipp", "tpp",
];

// What `anvil tidy` checks: C and C++ sources (not resource scripts, assembly or CUDA)
const TIDY_EXTENSIONS: &[&str] = &["c", "cc", "cpp", "cxx", "c++", "cppm", "ixx"];

// The files clang-format and clang-tidy read their settings from, in a file's folder or above
const FORMAT_STYLE_FILES: &[&str] = &[".clang-format", "_clang-format"];
const TIDY_CONFIG_FILES: &[&str] = &[".clang-tidy"];
const FORMAT_IGNORE_FILE: &str = ".clang-format-ignore";

/// `anvil fmt [--check] [paths]`: formats the files in place, or with `check` lists those that are not formatted and
/// returns 1 when there are any.
pub fn format(start_dir: &Path, check: bool, paths: &[PathBuf]) -> Result<i32> {
    let (root, files) = format_files(start_dir, paths)?;
    if files.is_empty() {
        ui::status("Formatted", "nothing: no C or C++ files");
        return Ok(0);
    }

    // Without a .clang-format, clang-format would rewrite every file in its own LLVM style
    let mut styles = ConfigLookup::new(FORMAT_STYLE_FILES);
    let unstyled: Vec<&PathBuf> = files.iter().filter(|file| !styles.covers(file)).collect();
    if let Some(first) = unstyled.first() {
        bail!(
            "{} of {} files have no .clang-format in their folder or above (first: {}), and clang-format would rewrite \
             them in its default LLVM style. Write one at the project root first, from a base style you then adjust: \
             `clang-format --style=llvm --dump-config > .clang-format` (or google, chromium, microsoft, mozilla, webkit, gnu)",
            unstyled.len(),
            files.len(),
            util::rel(first, &root)
        );
    }

    let clang_format = toolchain::find_llvm_program("clang-format")?;

    // Each file through clang-format, its output compared with the file
    let results: Vec<(&PathBuf, Result<Option<String>>)> = files.par_iter().map(|file| (file, reformat(&clang_format, file))).collect();

    let mut changed = Vec::new();
    let mut failures = Vec::new();
    for (file, result) in results {
        match result {
            Ok(Some(text)) => changed.push((file, text)),
            Ok(None) => {}
            Err(error) => failures.push(format!("{}: {error:#}", util::rel(file, &root))),
        }
    }

    // --check: name the files, change nothing
    if check {
        for (file, _) in &changed {
            println!("{}", util::rel(file, &root));
        }
    } else {
        for (file, text) in &changed {
            std::fs::write(file, text).with_context(|| format!("writing {}", slash(file)))?;
            ui::status("Formatted", &util::rel(file, &root));
        }
    }

    for failure in &failures {
        ui::error(failure);
    }
    if !failures.is_empty() {
        bail!("clang-format failed on {} of {} files", failures.len(), files.len());
    }

    if changed.is_empty() {
        ui::status("Fresh", &format!("{} files already formatted", files.len()));
        Ok(0)
    } else if check {
        ui::error(&format!(
            "{} of {} files are not formatted (`anvil fmt` formats them)",
            changed.len(),
            files.len()
        ));
        Ok(1)
    } else {
        ui::status("Finished", &format!("{} of {} files reformatted", changed.len(), files.len()));
        Ok(0)
    }
}

/// `anvil tidy [--fix] [targets]`: clang-tidy over the targets' C and C++ sources with the profile's compile commands
/// (planned first, so they are current); returns 1 when it reports anything.
pub fn tidy(start_dir: &Path, build: &BuildArgs, fix: bool, names: &[String]) -> Result<i32> {
    let model = commands::load_model(start_dir, build)?;

    // The targets: those named, else every one of the project's
    let roots = if names.is_empty() {
        (0..model.targets.len())
            .filter(|index| {
                let target = &model.targets[*index];
                model.packages[target.package].member && target.kind != Kind::External
            })
            .collect()
    } else {
        names.iter().map(|name| model.find_target(name)).collect::<Result<Vec<_>>>()?
    };

    // compile_commands.json, current, made readable for clang-tidy
    crate::plan::plan(&model, None, &roots, false)?;
    let database = model.out_dir.join("compile_commands.json");
    let tidy_dir = model.out_dir.join(".anvil").join("tidy");
    let compiled = write_tidy_database(&model, &database, &tidy_dir)?;

    // Their C and C++ sources the database compiles
    let mut files = Vec::new();
    let mut missing = Vec::new();
    let mut seen = HashSet::new();
    for root in &roots {
        for source in &model.targets[*root].sources {
            let key = util::path_key(source);
            if !has_extension(source, TIDY_EXTENSIONS) || !seen.insert(key.clone()) {
                continue;
            }
            if compiled.contains(&key) {
                files.push(source.clone());
            } else {
                missing.push(source.clone());
            }
        }
    }
    if let Some(first) = missing.first() {
        ui::warn(&format!(
            "{} sources are not in {} and are not checked (first: {})",
            missing.len(),
            slash(&database),
            util::rel(first, &model.root)
        ));
    }
    if files.is_empty() {
        bail!("nothing to check: the targets have no C or C++ sources");
    }

    let mut configs = ConfigLookup::new(TIDY_CONFIG_FILES);
    let unconfigured = files.iter().filter(|file| !configs.covers(file)).count();
    if unconfigured > 0 {
        ui::note(&format!(
            "{unconfigured} of {} files have no .clang-tidy in their folder or above: clang-tidy checks them with its defaults",
            files.len()
        ));
    }

    // clang-tidy: the one beside a clang toolchain's compiler, else as clang is found
    let beside_compiler = model
        .toolchain
        .cxx
        .parent()
        .map(|folder| folder.join(format!("clang-tidy{}", std::env::consts::EXE_SUFFIX)))
        .filter(|candidate| matches!(model.toolchain.family, Family::Clang | Family::ClangCl) && candidate.is_file());
    let clang_tidy = match beside_compiler {
        Some(found) => found,
        None => toolchain::find_llvm_program("clang-tidy")?,
    };

    // --fix: each run exports its fixes, applied together at the end (two runs may fix the same header)
    let fixes_dir = tidy_dir.join("fixes");
    if fix {
        let _ = std::fs::remove_dir_all(&fixes_dir);
        std::fs::create_dir_all(&fixes_dir).with_context(|| format!("creating {}", slash(&fixes_dir)))?;
    }

    let arguments = tidy_arguments(&model.toolchain, &tidy_dir);
    let report = Mutex::new(TidyReport::default());
    let done = AtomicUsize::new(0);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(commands::jobs(build)).build()?;

    pool.install(|| {
        files.par_iter().enumerate().for_each(|(index, file)| {
            let mut command = Command::new(&clang_tidy);
            command
                .args(&arguments)
                .envs(model.toolchain.env.iter().cloned())
                .current_dir(&model.root);
            if fix {
                command.arg(format!("--export-fixes={}", fixes_dir.join(format!("{index}.yaml")).display()));
            }
            let output = command.arg(file).output();

            let finished = done.fetch_add(1, Ordering::Relaxed) + 1;
            report.lock().unwrap().add(file, &model.root, output);
            ui::progress(finished, files.len(), &format!("Checking {}", util::rel(file, &model.root)));
        });
    });
    ui::clear_progress();

    let report = report.into_inner().unwrap();
    if fix {
        apply_fixes(&clang_tidy, &fixes_dir)?;
    }

    // Runs that failed, each reason once
    for (reason, failed) in &report.failures {
        ui::error(&format!("clang-tidy could not check {}:\n{reason}", failed.join(", ")));
    }
    let failures: usize = report.failures.iter().map(|(_, failed)| failed.len()).sum();
    if failures > 0 {
        ui::error(&format!("clang-tidy could not check {failures} of {} files", files.len()));
    }

    if report.warnings + report.errors == 0 {
        if failures == 0 {
            ui::status("Checked", &format!("{} files, nothing found", files.len()));
            return Ok(0);
        }
        return Ok(1);
    }

    let mut found = Vec::new();
    if report.warnings > 0 {
        found.push(plural(report.warnings, "warning"));
    }
    if report.errors > 0 {
        found.push(plural(report.errors, "error"));
    }
    ui::error(&format!("clang-tidy found {} checking {} files", found.join(" and "), files.len()));

    Ok(1)
}

/// What `anvil tidy` printed so far: each diagnostic once (a header's appears for every source including it).
#[derive(Default)]
struct TidyReport {
    printed: HashSet<String>,
    warnings: usize,
    errors: usize,

    /// Runs that printed no diagnostic and failed: the reason, and the files it failed on.
    failures: Vec<(String, Vec<String>)>,
}

impl TidyReport {
    /// Prints one run's new diagnostics, grouped by the file they are in.
    fn add(&mut self, file: &Path, root: &Path, output: std::io::Result<std::process::Output>) {
        let output = match output {
            Ok(output) => output,
            Err(error) => {
                self.fail(file, root, format!("cannot run clang-tidy: {error}"));
                return;
            }
        };

        // Diagnostics are on stdout, clang-tidy's own complaints (a bad .clang-tidy) on stderr
        let (mut diagnostics, mut other) = parse_diagnostics(&String::from_utf8_lossy(&output.stdout));
        let (more_diagnostics, more_other) = parse_diagnostics(&String::from_utf8_lossy(&output.stderr));
        diagnostics.extend(more_diagnostics);
        other.extend(more_other);

        if !output.status.success() && diagnostics.is_empty() {
            self.fail(file, root, other.join("\n"));
            return;
        }

        diagnostics.retain(|diagnostic| self.printed.insert(diagnostic.text.clone()));
        diagnostics.sort_by_key(|diagnostic| diagnostic.file.clone());
        let mut text = String::new();
        for diagnostic in &diagnostics {
            if diagnostic.error {
                self.errors += 1;
            } else {
                self.warnings += 1;
            }
            text.push_str(&diagnostic.text);
        }
        if !text.is_empty() {
            ui::print_block(&text);
        }
    }

    fn fail(&mut self, file: &Path, root: &Path, reason: String) {
        let file = util::rel(file, root);
        match self.failures.iter_mut().find(|(known, _)| *known == reason) {
            Some((_, failed)) => failed.push(file),
            None => self.failures.push((reason, vec![file])),
        }
    }
}

/// One diagnostic clang-tidy printed: its `file:line:column: warning: ... [check]` line, then its notes and excerpts.
#[derive(Debug)]
struct Diagnostic {
    file: String,
    error: bool,
    text: String,
}

/// Splits clang-tidy's output into diagnostics and the other lines, leaving out its counts and hints.
fn parse_diagnostics(output: &str) -> (Vec<Diagnostic>, Vec<String>) {
    let header = regex::Regex::new(r"^(?:(.+?):\d+:\d+: )?(warning|error): ").unwrap();
    let chatter = regex::Regex::new(
        r"^(\d+ (warnings?|errors?)( and \d+ errors?)? generated\.|Suppressed \d+ warnings|Use -header-filter|Use -system-headers|Found compiler error|Error while processing|\[\d+/\d+\] Processing file|clang-tidy applied)",
    )
    .unwrap();

    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut other = Vec::new();
    let mut in_usage = false;
    for line in output.lines() {
        let line = line.trim_end_matches('\r');
        if chatter.is_match(line) {
            continue;
        }

        // The option list clang-tidy prints before some errors ("Error: no checks enabled.")
        in_usage = (in_usage || line.starts_with("USAGE: ")) && !line.starts_with("Error");
        if in_usage {
            continue;
        }

        if let Some(captures) = header.captures(line) {
            diagnostics.push(Diagnostic {
                file: captures.get(1).map(|file| file.as_str().to_string()).unwrap_or_default(),
                error: &captures[2] == "error",
                text: format!("{line}\n"),
            });
        } else if let Some(last) = diagnostics.last_mut() {
            last.text.push_str(line);
            last.text.push('\n');
        } else if !line.trim().is_empty() {
            other.push(line.to_string());
        }
    }

    (diagnostics, other)
}

/// clang-tidy's options for every file: the compile database, and what clang needs to read a GNU toolchain's
/// command lines as that compiler would (cl's are recognized by the compiler's name).
fn tidy_arguments(toolchain: &toolchain::Toolchain, database_dir: &Path) -> Vec<String> {
    let mut arguments = vec!["--quiet".to_string(), "-p".to_string(), database_dir.display().to_string()];

    // gcc and clang build for their own default machine (MinGW's, say), which clang-tidy's clang does not know
    if matches!(toolchain.family, Family::Gcc | Family::Clang) {
        let machine = Command::new(&toolchain.cxx)
            .arg("-dumpmachine")
            .output()
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|machine| !machine.is_empty());
        if let Some(machine) = machine {
            arguments.push(format!("--extra-arg-before=--target={machine}"));
        }
    }

    // gcc's own warning options are unknown to clang: no warning for each
    if toolchain.family == Family::Gcc {
        arguments.push("--extra-arg=-Wno-unknown-warning-option".to_string());
    }

    arguments
}

/// Applies the fixes the runs exported, each once, with clang-apply-replacements (beside clang-tidy, else as clang is
/// found).
fn apply_fixes(clang_tidy: &Path, fixes_dir: &Path) -> Result<()> {
    let has_fixes = std::fs::read_dir(fixes_dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| std::fs::metadata(entry.path()).is_ok_and(|metadata| metadata.len() > 0));
    if !has_fixes {
        return Ok(());
    }

    let name = format!("clang-apply-replacements{}", std::env::consts::EXE_SUFFIX);
    let beside = clang_tidy
        .parent()
        .map(|folder| folder.join(&name))
        .filter(|candidate| candidate.is_file());
    let program = match beside {
        Some(found) => found,
        None => toolchain::find_llvm_program("clang-apply-replacements")?,
    };

    let output = Command::new(&program)
        .arg(fixes_dir)
        .output()
        .with_context(|| format!("running {}", slash(&program)))?;
    if !output.status.success() {
        bail!(
            "clang-apply-replacements could not apply clang-tidy's fixes:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    ui::status("Fixed", "applied the fixes clang-tidy suggested");
    let _ = std::fs::remove_dir_all(fixes_dir);

    Ok(())
}

/// Writes the compile database clang-tidy reads into `folder`: the build's, with each precompiled header swapped for
/// the header it holds (clang-tidy's clang reads neither cl's .pch files nor another clang version's). Returns the
/// files it compiles, as path keys.
fn write_tidy_database(model: &Model, database: &Path, folder: &Path) -> Result<HashSet<String>> {
    let text = std::fs::read_to_string(database).with_context(|| format!("reading {}", slash(database)))?;
    let mut entries: Vec<serde_json::Value> =
        serde_json::from_str(&text).with_context(|| format!("{} is not a compile database", slash(database)))?;

    // Each source's precompiled header
    let mut headers = HashMap::new();
    for target in &model.targets {
        if let Some(header) = &target.pch {
            for source in &target.sources {
                headers.insert(util::path_key(source), header.clone());
            }
        }
    }

    let mut files = HashSet::new();
    for entry in &mut entries {
        let file = util::path_key(Path::new(entry["file"].as_str().unwrap_or_default()));
        let header = headers.get(&file);
        if let Some(arguments) = entry["arguments"].as_array_mut() {
            let words: Vec<String> = arguments.iter().filter_map(|word| word.as_str().map(str::to_string)).collect();
            *arguments = without_pch(&words, header.map(PathBuf::as_path))
                .into_iter()
                .map(serde_json::Value::from)
                .collect();
        }
        files.insert(file);
    }

    std::fs::create_dir_all(folder).with_context(|| format!("creating {}", slash(folder)))?;
    let text = serde_json::to_string_pretty(&entries)? + "\n";
    util::write_if_changed(&folder.join("compile_commands.json"), text.as_bytes())?;

    Ok(files)
}

/// A compile command without its precompiled header: cl's /Yu, /Yc and /Fp go (its /FI includes the header as text),
/// clang's `-include-pch <file>` becomes `-include <header>`. gcc's `-include <stub>` stays: the stub includes the header.
fn without_pch(arguments: &[String], header: Option<&Path>) -> Vec<String> {
    let mut kept = Vec::new();
    let mut words = arguments.iter();

    while let Some(word) = words.next() {
        if word.starts_with("/Yu") || word.starts_with("/Yc") || word.starts_with("/Fp") {
            continue;
        }
        if word == "-include-pch" {
            words.next();
            if let Some(header) = header {
                kept.push("-include".to_string());
                kept.push(header.display().to_string());
            }
            continue;
        }
        kept.push(word.clone());
    }

    kept
}

/// The files `anvil fmt` formats: those under `paths`, else every package's (the root's and its workspace members'),
/// minus what .clang-format-ignore files leave out; sorted. Also the project root, which messages name files from.
fn format_files(start_dir: &Path, paths: &[PathBuf]) -> Result<(PathBuf, Vec<PathBuf>)> {
    let (root, manifest) = project::locate(start_dir)?;
    let members = project::workspace_members(&root, &manifest)?;

    // Every package's output folder is left alone
    let mut out_roots = vec![out_root(&root, &manifest)];
    for member in &members {
        out_roots.push(out_root(member, &manifest::load(&member.join(MANIFEST_NAME))?));
    }

    let mut files = Vec::new();
    if paths.is_empty() {
        for package in std::iter::once(&root).chain(&members) {
            collect_format_files(package, &out_roots, &mut files);
        }
    } else {
        let current_dir = std::env::current_dir()?;
        for path in paths {
            let absolute = util::absolute(&current_dir, path);
            if absolute.is_dir() {
                collect_format_files(&absolute, &out_roots, &mut files);
            } else if !absolute.is_file() {
                bail!("{}: no such file or folder", slash(path));
            } else if has_extension(&absolute, FORMAT_EXTENSIONS) {
                files.push(absolute);
            } else {
                bail!("{} is not a C or C++ file ({})", slash(path), FORMAT_EXTENSIONS.join(", "));
            }
        }
    }

    files.sort();
    files.dedup();
    let mut ignores = IgnoreLookup::default();
    let mut kept = Vec::new();
    for file in files {
        if !ignores.ignores(&file)? {
            kept.push(file);
        }
    }

    Ok((root, kept))
}

/// A package's output folder (`[project] out`, default `out`).
fn out_root(package_root: &Path, manifest: &Manifest) -> PathBuf {
    let out = manifest.project.as_ref().and_then(|project| project.out.clone());
    util::absolute(package_root, Path::new(out.as_deref().unwrap_or("out")))
}

/// The C and C++ files under `folder`, past the folders a source glob skips, every output folder, vcpkg checkouts and
/// other packages.
fn collect_format_files(folder: &Path, out_roots: &[PathBuf], files: &mut Vec<PathBuf>) {
    let walker = walkdir::WalkDir::new(folder).into_iter().filter_entry(|entry| {
        let skipped = || {
            out_roots.iter().any(|out_root| targets::is_skipped_dir(entry, out_root))
                || entry.path().join(".vcpkg-root").is_file()
                || entry.path().join(MANIFEST_NAME).is_file()
        };
        entry.depth() == 0 || !entry.file_type().is_dir() || !skipped()
    });

    for entry in walker.flatten() {
        if entry.file_type().is_file() && has_extension(entry.path(), FORMAT_EXTENSIONS) {
            files.push(entry.into_path());
        }
    }
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .is_some_and(|extension| extensions.contains(&extension.to_string_lossy().to_ascii_lowercase().as_str()))
}

/// The file as clang-format would write it, or None when it already is.
fn reformat(clang_format: &Path, file: &Path) -> Result<Option<String>> {
    let output = Command::new(clang_format)
        .args(["--style=file", "--fallback-style=none"])
        .arg(file)
        .output()
        .with_context(|| format!("running {}", slash(clang_format)))?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }

    let original = std::fs::read(file).with_context(|| format!("reading {}", slash(file)))?;
    if output.stdout == original {
        return Ok(None);
    }

    Ok(Some(
        String::from_utf8(output.stdout).context("clang-format wrote text that is not UTF-8")?,
    ))
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// Whether a file has one of some settings files in its folder or above, remembered per folder.
struct ConfigLookup {
    names: &'static [&'static str],
    folders: HashMap<PathBuf, bool>,
}

impl ConfigLookup {
    fn new(names: &'static [&'static str]) -> ConfigLookup {
        ConfigLookup {
            names,
            folders: HashMap::new(),
        }
    }

    fn covers(&mut self, file: &Path) -> bool {
        file.parent().is_some_and(|folder| self.folder_covered(folder))
    }

    fn folder_covered(&mut self, folder: &Path) -> bool {
        if let Some(known) = self.folders.get(folder) {
            return *known;
        }

        let here = self.names.iter().any(|name| folder.join(name).is_file());
        let covered = here || folder.parent().is_some_and(|parent| self.folder_covered(parent));
        self.folders.insert(folder.to_path_buf(), covered);

        covered
    }
}

/// .clang-format-ignore files, read as clang-format reads them: the nearest one above a file decides, its last
/// matching pattern wins, `!` negates, and patterns are relative to its folder (`*` stays within a folder, `**` not).
#[derive(Default)]
struct IgnoreLookup {
    files: Vec<Vec<(globset::GlobMatcher, bool)>>,
    folders: HashMap<PathBuf, Option<usize>>,
}

impl IgnoreLookup {
    fn ignores(&mut self, file: &Path) -> Result<bool> {
        let Some(folder) = file.parent() else { return Ok(false) };
        let Some(index) = self.nearest(folder)? else { return Ok(false) };

        let text = slash(file);
        let mut ignored = false;
        for (matcher, negated) in &self.files[index] {
            if matcher.is_match(&text) {
                ignored = !negated;
            }
        }

        Ok(ignored)
    }

    fn nearest(&mut self, folder: &Path) -> Result<Option<usize>> {
        if let Some(known) = self.folders.get(folder) {
            return Ok(*known);
        }

        let ignore_file = folder.join(FORMAT_IGNORE_FILE);
        let found = if ignore_file.is_file() {
            let text = std::fs::read_to_string(&ignore_file).with_context(|| format!("reading {}", slash(&ignore_file)))?;
            self.files.push(ignore_patterns(folder, &text).with_context(|| slash(&ignore_file))?);
            Some(self.files.len() - 1)
        } else {
            match folder.parent() {
                Some(parent) => self.nearest(parent)?,
                None => None,
            }
        };
        self.folders.insert(folder.to_path_buf(), found);

        Ok(found)
    }
}

/// A .clang-format-ignore file's patterns, made absolute, each with whether it is negated.
fn ignore_patterns(folder: &Path, text: &str) -> Result<Vec<(globset::GlobMatcher, bool)>> {
    let mut patterns = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let (negated, pattern) = match line.strip_prefix('!') {
            Some(rest) => (true, rest.trim_start()),
            None => (false, line),
        };
        if pattern.is_empty() {
            continue;
        }

        let absolute = if pattern.starts_with('/') {
            pattern.to_string()
        } else {
            format!("{}/{pattern}", slash(folder).trim_end_matches('/'))
        };
        let matcher = globset::GlobBuilder::new(&remove_dots(&absolute))
            .literal_separator(true)
            .backslash_escape(true)
            .build()
            .with_context(|| format!("bad pattern `{pattern}`"))?
            .compile_matcher();
        patterns.push((matcher, negated));
    }

    Ok(patterns)
}

/// A slash path without its `.` steps and with each `..` step taking the folder before it.
fn remove_dots(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();

    for part in path.split('/') {
        match part {
            "." => {}
            ".." if parts.len() > 1 => {
                parts.pop();
            }
            ".." => {}
            _ => parts.push(part),
        }
    }

    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ignored(patterns: &str, file: &str) -> bool {
        let matchers = ignore_patterns(Path::new("/project"), patterns).unwrap();
        let mut ignored = false;
        for (matcher, negated) in &matchers {
            if matcher.is_match(file) {
                ignored = !negated;
            }
        }
        ignored
    }

    #[test]
    fn ignore_patterns_follow_clang_format() {
        assert!(ignored("third_party/*", "/project/third_party/a.cpp"));
        assert!(!ignored("third_party/*", "/project/third_party/sub/a.cpp"));
        assert!(ignored("third_party/**", "/project/third_party/sub/a.cpp"));
        assert!(ignored("# comment\n\n  *.h  ", "/project/a.h"));
        assert!(!ignored("*.h", "/project/src/a.h"));
        assert!(ignored("**/gen_*.cpp", "/project/src/x/gen_a.cpp"));
        assert!(!ignored("src/*\n!src/keep.cpp", "/project/src/keep.cpp"));
        assert!(ignored("src/*\n!src/keep.cpp", "/project/src/other.cpp"));
        assert!(ignored("src/../lib/*", "/project/lib/a.c"));
        assert!(ignored("/project/abs/*", "/project/abs/a.c"));
    }

    #[test]
    fn swaps_precompiled_headers_for_their_headers() {
        let words = |text: &str| text.split(' ').map(str::to_string).collect::<Vec<_>>();

        let cl = words("cl.exe /nologo /Yuc:/p/pch.h /Fpc:/o/pch.h.pch /FIc:/p/pch.h /fp:fast /c /Tpc:/p/a.cpp");
        assert_eq!(without_pch(&cl, None), words("cl.exe /nologo /FIc:/p/pch.h /fp:fast /c /Tpc:/p/a.cpp"));

        let clang = words("clang++ -std=c++20 -include-pch /o/pch.h.pch -c /p/a.cpp");
        assert_eq!(
            without_pch(&clang, Some(Path::new("/p/pch.h"))),
            words("clang++ -std=c++20 -include /p/pch.h -c /p/a.cpp")
        );

        let gcc = words("g++ -include /o/pch/pch.h -c /p/a.cpp");
        assert_eq!(without_pch(&gcc, Some(Path::new("/p/pch.h"))), gcc);
    }

    #[test]
    fn missing_tool_names_where_it_looked() {
        let error = toolchain::find_llvm_program("anvil-no-such-tool").unwrap_err().to_string();
        assert!(error.contains("anvil-no-such-tool not found (looked on PATH"), "{error}");
    }

    #[test]
    fn removes_dots() {
        assert_eq!(remove_dots("/a/./b/../c"), "/a/c");
        assert_eq!(remove_dots("C:/a/../b"), "C:/b");
    }

    #[test]
    fn parses_tidy_output() {
        let output = "\
C:\\p\\src\\bad.cpp:5:16: warning: an assignment within an 'if' condition is bug-prone [bugprone-assignment-in-if-condition]
    5 |     if (length = 3) return 1;
      |         ~~~~~~~^~~
C:\\p\\src\\bad.cpp:5:16: note: if it should be an assignment, move it out of the 'if' condition
C:\\p\\include\\x.h:2:1: error: unknown type name 'foo' [clang-diagnostic-error]
2 warnings and 1 error generated.
error: unknown argument: '-fnot-a-flag' [clang-diagnostic-error]
Suppressed 7 warnings (5 in non-user code, 2 with check filters).
";
        let (diagnostics, other) = parse_diagnostics(output);
        assert!(other.is_empty(), "{other:?}");
        assert_eq!(diagnostics.len(), 3);
        assert_eq!(diagnostics[0].file, "C:\\p\\src\\bad.cpp");
        assert!(!diagnostics[0].error);
        assert_eq!(diagnostics[0].text.lines().count(), 4);
        assert_eq!(diagnostics[1].file, "C:\\p\\include\\x.h");
        assert!(diagnostics[1].error);
        assert_eq!(diagnostics[2].file, "");

        let (diagnostics, other) =
            parse_diagnostics("USAGE: clang-tidy.exe [options] <source0>\nOPTIONS:\n  --fix - Apply\nError: no checks enabled.\n");
        assert!(diagnostics.is_empty());
        assert_eq!(other, ["Error: no checks enabled."]);
    }
}
