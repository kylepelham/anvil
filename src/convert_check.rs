//! `anvil convert --check`: configures the CMake project with Ninja and the same compiler and vcpkg tree, plans the
//! Anvil build, and compares the two compile_commands.json file by file: which files each side compiles, and each
//! common file's definitions, include folders, standard and other options.

use crate::project::Model;
use crate::util::{self, slash};
use anyhow::{bail, Context, Result};
use indexmap::{IndexMap, IndexSet};
use std::path::{Path, PathBuf};
use std::process::Command;

// Definitions CMake adds on its own (CMAKE_<LANG>_FLAGS defaults, Ninja Multi-Config, shared library exports)
const CMAKE_IMPLICIT_DEFINES: &[&str] = &["WIN32", "_WINDOWS", "_MBCS", "UNICODE", "_UNICODE"];

// Options each side adds by itself (output and dependency files, precompiled headers, optimization, debug info,
// warning level, conformance, diagnostics), left out of the comparison; a trailing `*` matches any rest.
// MSVC options are compared with `-`, however written; `@file` (response files, module maps) is CMake's own too
const OWN_OPTIONS: &[&str] = &[
    "-nologo",
    "-utf-8",
    "-bigobj",
    "-EHsc",
    "-EHs",
    "-GR",
    "-permissive-",
    "-Zc:__cplusplus",
    "-FS",
    "-MP*",
    "-c",
    "-Fo*",
    "-Fd*",
    "-Fp*",
    "-Yu*",
    "-Yc*",
    "-FI*",
    "-Tc*",
    "-Tp*",
    "-TC",
    "-TP",
    "-showIncludes",
    "-O*",
    "-Z7",
    "-Zi",
    "-ZI",
    "-RTC*",
    "-MD",
    "-W0",
    "-W1",
    "-W2",
    "-W3",
    "-W4",
    "-Wall",
    "-external:W*",
    "-Gy",
    "-Gw",
    "-GL",
    "-JMC",
    "-diagnostics:*",
    "-g*",
    "-MMD",
    "-MP",
    "-fdiagnostics-color*",
    "-fcolor-diagnostics",
    "-fPIC",
    "-fpic",
    "-w",
    "-Wextra",
    "-Winvalid-pch",
    "-flto*",
    "-fms-runtime-lib=*",
    "@*",
];

// Options whose value is the next word (files, a language), left out with it: GCC's and Clang's, and cl's (whose `-MT`
// is the static runtime)
const GNU_OPTIONS_WITH_VALUE: &[&str] = &["-o", "-MF", "-MT", "-MQ", "-x", "-include", "-include-pch"];
const MSVC_OPTIONS_WITH_VALUE: &[&str] = &["-sourceDependencies"];

// cl's C runtime: compared, since the profile's `static-runtime` and `debug-runtime` must match CMake's
const MSVC_RUNTIME_OPTIONS: &[&str] = &["-MD", "-MDd", "-MT", "-MTd"];

// CMake's own source of a precompiled header (`target_precompile_headers`; `cmake_pch.hxx.cxx` with GCC); Anvil
// compiles the header itself
const CMAKE_PCH_SOURCE: &str = "cmake_pch.";

// Files and differences listed per group before "and N more"
const LISTED_FILES: usize = 5;

// Lines of CMake's output shown when it cannot configure
const CMAKE_LOG_LINES: usize = 30;

/// One compiled file's settings that matter for a correct build.
#[derive(Default, Clone)]
struct FileFlags {
    shown_path: String,
    defines: IndexSet<String>,
    includes: IndexSet<String>,
    standard: Option<String>,
    options: IndexSet<String>,
}

/// Configures CMake, plans Anvil, and prints the differences. Returns the exit code: 0 when both compile the same.
pub fn check(model: &Model) -> Result<i32> {
    let cmake_folder = model.out_root.join("convert-check");
    configure_cmake(model, &cmake_folder)?;

    // Both databases, by file
    let mut cmake_entries = read_database(&cmake_folder.join("compile_commands.json"), &cmake_folder, model)?;
    cmake_entries.retain(|_, entries| {
        !entries[0]
            .shown_path
            .rsplit('/')
            .next()
            .is_some_and(|name| name.starts_with(CMAKE_PCH_SOURCE))
    });
    let roots: Vec<usize> = model.default_targets(true);
    crate::plan::plan(model, None, &roots, false)?;
    let mut anvil_entries = read_database(&model.out_dir.join("compile_commands.json"), &model.out_dir, model)?;

    // Resource files a target lists but compile_commands.json leaves out: compiled the same as CMake's
    let prefixes = Prefixes {
        build: util::path_key(&model.out_dir),
        root: util::path_key(&model.root),
    };
    let listed: IndexSet<String> = roots
        .iter()
        .flat_map(|index| &model.targets[*index].sources)
        .map(|source| comparable_path(source, &prefixes))
        .collect();
    for (key, entries) in &cmake_entries {
        if key.ends_with(".rc") && listed.contains(key) && !anvil_entries.contains_key(key) {
            anvil_entries.insert(key.clone(), entries.clone());
        }
    }

    Ok(report(&cmake_entries, &anvil_entries))
}

/// Runs `cmake -G Ninja` with Anvil's compiler, profile, options and installed vcpkg packages.
fn configure_cmake(model: &Model, folder: &Path) -> Result<()> {
    let cmake = util::which("cmake").context("--check needs cmake on PATH")?;
    if util::which("ninja").is_none() && !tool_on_path(model, "ninja.exe") {
        bail!("--check needs ninja on PATH");
    }

    // Configured for another compiler (--toolchain changed): CMake keeps its old compile commands, so start over; and
    // for another source folder (the project was copied with its out/): CMake refuses that cache
    let cache = std::fs::read_to_string(folder.join("CMakeCache.txt")).unwrap_or_default();
    let differs = |name: &str, path: &Path| cached_value(&cache, name).is_some_and(|value| util::path_key(Path::new(value)) != util::path_key(path));
    if differs("CMAKE_C_COMPILER", &model.toolchain.cc) || differs("CMAKE_HOME_DIRECTORY", &model.root) {
        std::fs::remove_dir_all(folder).with_context(|| format!("removing {}", slash(folder)))?;
    }

    let mut args: Vec<String> = vec![
        "-S".into(),
        slash(&model.root),
        "-B".into(),
        slash(folder),
        "-G".into(),
        "Ninja".into(),
        "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON".into(),
        // (projects asking for CMake older than 3.5 configure only with this)
        "-DCMAKE_POLICY_VERSION_MINIMUM=3.5".into(),
        format!("-DCMAKE_BUILD_TYPE={}", cmake_build_type(model)),
        format!("-DCMAKE_C_COMPILER={}", slash(&model.toolchain.cc)),
        format!("-DCMAKE_CXX_COMPILER={}", slash(&model.toolchain.cxx)),
    ];

    // The options given with -o; the others keep CMake's own defaults (also after an earlier --check set them), so
    // the converted defaults are checked too
    for name in model.condition.options.keys() {
        match model.options.options.iter().find(|(given, _)| given == name) {
            Some((_, value)) => args.push(format!("-D{name}={}", if *value { "ON" } else { "OFF" })),
            None => args.push(format!("-U{name}")),
        }
    }

    // vcpkg: the packages Anvil installed, without installing again
    if let Some(instance) = &model.vcpkg {
        let installed = model.vcpkg_dir.parent().unwrap();
        args.push(format!(
            "-DCMAKE_TOOLCHAIN_FILE={}",
            slash(&instance.root.join("scripts/buildsystems/vcpkg.cmake"))
        ));
        args.push(format!("-DVCPKG_TARGET_TRIPLET={}", model.vcpkg_triplet));
        args.push(format!("-DVCPKG_INSTALLED_DIR={}", slash(installed)));
        args.push("-DVCPKG_MANIFEST_INSTALL=OFF".into());
    }

    crate::ui::status("Configuring", &format!("CMake in {}", util::rel(folder, &model.root)));
    let output = Command::new(&cmake)
        .args(&args)
        .envs(model.toolchain.env.iter().map(|(name, value)| (name, value)))
        .output()
        .context("running cmake")?;
    if !output.status.success() {
        let log = String::from_utf8_lossy(&output.stdout) + String::from_utf8_lossy(&output.stderr);
        let lines: Vec<&str> = log.lines().collect();
        let tail = &lines[lines.len().saturating_sub(CMAKE_LOG_LINES)..];
        bail!("CMake could not configure the project:\n{}", tail.join("\n"));
    }

    Ok(())
}

/// A CMakeCache.txt entry's value (`NAME:TYPE=value`).
fn cached_value<'a>(cache: &'a str, name: &str) -> Option<&'a str> {
    cache
        .lines()
        .find_map(|line| line.strip_prefix(name).and_then(|rest| rest.strip_prefix(':')))
        .and_then(|rest| rest.split_once('=').map(|(_, value)| value.trim()))
        .filter(|value| !value.is_empty())
}

/// The CMake configuration for Anvil's profile.
fn cmake_build_type(model: &Model) -> &'static str {
    let profiles = &model.condition.profiles;
    match profiles.first().map(String::as_str) {
        Some("relwithdebinfo") => "RelWithDebInfo",
        Some("minsizerel") => "MinSizeRel",
        _ if profiles.iter().any(|profile| profile == "release") => "Release",
        _ => "Debug",
    }
}

fn tool_on_path(model: &Model, name: &str) -> bool {
    let path = model
        .toolchain
        .env
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
        .map(|(_, value)| value.as_str());
    std::env::split_paths(path.unwrap_or_default()).any(|folder| folder.join(name).is_file())
}

/// A compile_commands.json by file (a file several targets compile has several entries).
type Database = IndexMap<String, Vec<FileFlags>>;

/// The folders a database's paths compare relative to.
struct Prefixes {
    build: String,
    root: String,
}

/// Reads a compile_commands.json. Build-folder paths become <build> and project paths <root>, so CMake's and
/// Anvil's generated folders compare.
fn read_database(file: &Path, build_folder: &Path, model: &Model) -> Result<Database> {
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", slash(file)))?;
    let entries: Vec<serde_json::Value> = serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", slash(file)))?;

    let prefixes = Prefixes {
        build: util::path_key(build_folder),
        root: util::path_key(&model.root),
    };

    // cl's own header folders (its INCLUDE), which Anvil's database names for editors and CMake's leaves out
    let system_folders: Vec<String> = model
        .toolchain
        .env
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("INCLUDE"))
        .flat_map(|(_, value)| std::env::split_paths(value).collect::<Vec<_>>())
        .map(|folder| util::path_key(&util::normalize(&folder)))
        .collect();

    let mut files = Database::new();
    for entry in &entries {
        let directory = PathBuf::from(entry["directory"].as_str().unwrap_or_default());
        let Some(source) = entry["file"].as_str() else { continue };
        let source = util::absolute(&directory, Path::new(source));

        let words: Vec<String> = match (&entry["arguments"], entry["command"].as_str()) {
            (serde_json::Value::Array(words), _) => words.iter().filter_map(|word| word.as_str().map(String::from)).collect(),
            (_, Some(command)) => split_command(command),
            _ => continue,
        };

        let mut flags = FileFlags {
            shown_path: util::rel(&source, &model.root),
            ..Default::default()
        };
        let source_key = util::path_key(&source);
        let is_resource = source.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("rc"));
        read_flags(&words, &mut |kind, value| match kind {
            FlagKind::Define => {
                flags.defines.insert(value.to_string());
            }
            FlagKind::Include => {
                let folder = util::absolute(&directory, Path::new(value));
                let system = system_folders.contains(&util::path_key(&util::normalize(&folder)));
                if !system && !is_empty_gen_folder(&folder, model) {
                    flags.includes.insert(comparable_path(&folder, &prefixes));
                }
            }
            FlagKind::Standard => flags.standard = Some(value.to_string()),

            // The source itself aside; a resource compiler's options are its own
            FlagKind::Option if is_resource || util::path_key(&util::absolute(&directory, Path::new(value))) == source_key => {}
            FlagKind::Option => {
                flags.options.insert(value.to_string());
            }
        });

        files.entry(comparable_path(&source, &prefixes)).or_default().push(flags);
    }

    Ok(files)
}

/// Anvil's folder for a target's generated headers ({gen}) while nothing is in it: CMake has none to match.
fn is_empty_gen_folder(folder: &Path, model: &Model) -> bool {
    let gen_root = format!("{}/", util::path_key(&model.out_dir.join("gen")));
    let is_gen = util::path_key(folder).starts_with(&gen_root);
    let has_files = walkdir::WalkDir::new(folder)
        .into_iter()
        .flatten()
        .any(|entry| entry.file_type().is_file());

    is_gen && !has_files
}

/// A path as both databases can compare it: case-folded, with the build folder as <build> and the project as <root>.
fn comparable_path(path: &Path, prefixes: &Prefixes) -> String {
    let key = util::path_key(&util::normalize(path));
    if let Some(rest) = key.strip_prefix(&prefixes.build) {
        return format!("<build>{rest}");
    }

    match key.strip_prefix(&prefixes.root) {
        Some(rest) => format!("<root>{rest}"),
        None => key,
    }
}

enum FlagKind {
    Define,
    Include,
    Standard,
    /// Any other option the compiler is given, but those each side adds by itself (OWN_OPTIONS).
    Option,
}

/// Calls `found` for each definition, include folder, standard and other option in a compile command (attached or
/// separate values).
fn read_flags(words: &[String], found: &mut dyn FnMut(FlagKind, &str)) {
    const DEFINE_FLAGS: &[&str] = &["-D", "/D"];
    const INCLUDE_FLAGS: &[&str] = &["-I", "/I", "-external:I", "/external:I", "-isystem", "-imsvc", "-iquote"];
    const STANDARD_FLAGS: &[&str] = &["-std=", "/std:", "-std:"];

    // The longest flag first: /external:I before /I
    let mut flags: Vec<(&str, bool)> = DEFINE_FLAGS.iter().map(|flag| (*flag, true)).collect();
    flags.extend(INCLUDE_FLAGS.iter().map(|flag| (*flag, false)));
    flags.sort_by_key(|(flag, _)| std::cmp::Reverse(flag.len()));

    // MSVC takes `/` and `-` alike: compared as `-`
    let program = words
        .first()
        .map(|word| Path::new(word).file_stem().unwrap_or_default().to_string_lossy().to_lowercase());
    let msvc = matches!(program.as_deref(), Some("cl" | "clang-cl"));

    let mut index = 1;
    while index < words.len() {
        let word = &words[index];
        index += 1;

        if let Some(standard) = STANDARD_FLAGS.iter().find_map(|flag| word.strip_prefix(flag)) {
            found(FlagKind::Standard, standard);
            continue;
        }

        let Some((flag, is_define)) = flags.iter().find(|(flag, _)| word.starts_with(*flag)) else {
            // Another option, unless the side adds it by itself
            let option = match word.strip_prefix('/') {
                Some(rest) if msvc => format!("-{rest}"),
                _ => word.clone(),
            };
            let with_value = if msvc { MSVC_OPTIONS_WITH_VALUE } else { GNU_OPTIONS_WITH_VALUE };
            if with_value.contains(&option.as_str()) {
                index += 1;
            } else {
                // cl's runtime is compared though GCC's `-MD` (a dependency file) is each side's own
                let runtime = msvc && MSVC_RUNTIME_OPTIONS.contains(&option.as_str());
                if runtime || !OWN_OPTIONS.iter().any(|own| matches_pattern(own, &option)) {
                    found(FlagKind::Option, &option);
                }
            }
            continue;
        };
        let attached = &word[flag.len()..];
        let value = if attached.is_empty() {
            index += 1;
            words.get(index - 1).map(String::as_str).unwrap_or_default()
        } else {
            attached
        };

        let kind = if *is_define { FlagKind::Define } else { FlagKind::Include };
        found(kind, value);
    }
}

/// `pattern` with a trailing `*` matching any rest.
fn matches_pattern(pattern: &str, text: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(start) => text.starts_with(start),
        None => text == pattern,
    }
}

/// Splits a command line the way CMake writes them: double quotes group, \" is a quote, other backslashes stay.
fn split_command(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quoted = false;
    let mut characters = command.chars().peekable();

    while let Some(character) = characters.next() {
        match character {
            '\\' if characters.peek() == Some(&'"') => {
                characters.next();
                current.push('"');
                in_word = true;
            }
            '"' => {
                quoted = !quoted;
                in_word = true;
            }
            ' ' | '\t' if !quoted => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            _ => {
                current.push(character);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(current);
    }

    words
}

/// Prints what differs, grouping files with the same difference. Returns 0 when nothing does.
fn report(cmake: &Database, anvil: &Database) -> i32 {
    let only_cmake: Vec<&str> = cmake
        .iter()
        .filter(|(key, _)| !anvil.contains_key(*key))
        .map(|(_, entries)| entries[0].shown_path.as_str())
        .collect();
    let only_anvil: Vec<&str> = anvil
        .iter()
        .filter(|(key, _)| !cmake.contains_key(*key))
        .map(|(_, entries)| entries[0].shown_path.as_str())
        .collect();

    // Differences of the files both compile, grouped by what differs
    let mut groups: IndexMap<String, Vec<&str>> = IndexMap::new();
    let mut common = 0;
    for (key, cmake_entries) in cmake {
        let Some(anvil_entries) = anvil.get(key) else { continue };

        for (cmake_flags, anvil_flags) in pair_entries(cmake_entries, anvil_entries) {
            common += 1;
            let difference = describe_difference(cmake_flags, anvil_flags);
            if !difference.is_empty() {
                groups.entry(difference).or_default().push(&cmake_flags.shown_path);
            }
        }
    }

    // The report
    let differing: usize = groups.values().map(Vec::len).sum();
    print_files("compiled only by CMake", &only_cmake);
    print_files("compiled only by Anvil", &only_anvil);
    for (difference, files) in &groups {
        println!("\n{} file(s): {}\n{difference}", files.len(), list_shown(files));
    }

    println!();
    let same = common - differing;
    let summary = format!(
        "{common} files compiled by both: {same} the same, {differing} different; {} only by CMake, {} only by Anvil",
        only_cmake.len(),
        only_anvil.len()
    );
    if differing == 0 && only_cmake.is_empty() && only_anvil.is_empty() {
        crate::ui::status("Checked", &summary);
        0
    } else {
        crate::ui::warn(&summary);
        1
    }
}

/// One file's entries on both sides, each CMake entry with an identical Anvil one where there is one, the rest in
/// order (one target compiling a file differently from another).
fn pair_entries<'a>(cmake: &'a [FileFlags], anvil: &'a [FileFlags]) -> Vec<(&'a FileFlags, &'a FileFlags)> {
    let mut pairs = Vec::new();
    let mut anvil_left: Vec<&FileFlags> = anvil.iter().collect();
    let mut cmake_left = Vec::new();

    for cmake_flags in cmake {
        match anvil_left.iter().position(|anvil_flags| difference_size(cmake_flags, anvil_flags) == 0) {
            Some(index) => pairs.push((cmake_flags, anvil_left.remove(index))),
            None => cmake_left.push(cmake_flags),
        }
    }

    // The rest with the closest entry left
    for cmake_flags in cmake_left {
        let closest = (0..anvil_left.len()).min_by_key(|index| difference_size(cmake_flags, anvil_left[*index]));
        let Some(index) = closest else { break };
        pairs.push((cmake_flags, anvil_left.remove(index)));
    }

    pairs
}

/// How many definitions, include folders and options one side has and the other lacks.
fn difference_size(cmake: &FileFlags, anvil: &FileFlags) -> usize {
    let defines = cmake
        .defines
        .iter()
        .filter(|define| !is_cmake_implicit(define) && !anvil.defines.contains(*define))
        .count()
        + anvil.defines.iter().filter(|define| !cmake.defines.contains(*define)).count();
    let includes = cmake.includes.symmetric_difference(&anvil.includes).count();
    let options = cmake.options.symmetric_difference(&anvil.options).count();

    defines + includes + options + different_standards(cmake, anvil) as usize
}

/// Both say which standard, and not the same one (a side without one compiles with the compiler's default).
fn different_standards(cmake: &FileFlags, anvil: &FileFlags) -> bool {
    matches!((&cmake.standard, &anvil.standard), (Some(cmake), Some(anvil)) if cmake != anvil)
}

/// Definitions and include folders one side has and the other lacks, one line each; empty when they match.
fn describe_difference(cmake: &FileFlags, anvil: &FileFlags) -> String {
    let only_cmake_defines = cmake
        .defines
        .iter()
        .filter(|define| !is_cmake_implicit(define) && !anvil.defines.contains(*define));
    let only_anvil_defines = anvil.defines.iter().filter(|define| !cmake.defines.contains(*define));
    let only_cmake_includes = cmake.includes.iter().filter(|folder| !anvil.includes.contains(*folder));
    let only_anvil_includes = anvil.includes.iter().filter(|folder| !cmake.includes.contains(*folder));
    let only_cmake_options = cmake.options.iter().filter(|option| !anvil.options.contains(*option));
    let only_anvil_options = anvil.options.iter().filter(|option| !cmake.options.contains(*option));
    let standards = match (&cmake.standard, &anvil.standard) {
        (Some(cmake), Some(anvil)) if cmake != anvil => Some(format!("  standard: CMake {cmake}, Anvil {anvil}")),
        _ => None,
    };

    let lines = [
        difference_line("definitions only CMake has", only_cmake_defines),
        difference_line("definitions only Anvil has", only_anvil_defines),
        difference_line("include folders only CMake has", only_cmake_includes),
        difference_line("include folders only Anvil has", only_anvil_includes),
        difference_line("options only CMake has", only_cmake_options),
        difference_line("options only Anvil has", only_anvil_options),
        standards,
    ];

    lines.into_iter().flatten().collect::<Vec<_>>().join("\n")
}

/// "  label: a, b", or None when there is nothing to list.
fn difference_line<'a>(label: &str, items: impl Iterator<Item = &'a String>) -> Option<String> {
    let shown: Vec<&str> = items.map(String::as_str).collect();
    if shown.is_empty() {
        return None;
    }

    Some(format!("  {label}: {}", shown.join(", ")))
}

fn is_cmake_implicit(define: &str) -> bool {
    let name = define.split('=').next().unwrap_or(define);
    // generate_export_header's _STATIC_DEFINE: the converter writes its own export header instead
    let implicit_suffix = name.ends_with("_EXPORTS") || name.ends_with("_STATIC_DEFINE");
    CMAKE_IMPLICIT_DEFINES.contains(&name) || implicit_suffix || name == "CMAKE_INTDIR"
}

fn print_files(label: &str, files: &[&str]) {
    if !files.is_empty() {
        println!("\n{} file(s) {label}: {}", files.len(), list_shown(files));
    }
}

fn list_shown(files: &[&str]) -> String {
    let mut shown: Vec<String> = files.iter().take(LISTED_FILES).map(|file| file.to_string()).collect();
    if files.len() > LISTED_FILES {
        shown.push(format!("and {} more", files.len() - LISTED_FILES));
    }
    shown.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_cmake_commands() {
        let words = split_command(r#"C:\VS\cl.exe /nologo "-DNAME=\"x y\"" /I"C:/a b/include" -external:I C:/v/include /c src\main.cpp"#);
        assert_eq!(words[0], r"C:\VS\cl.exe");
        assert_eq!(words[2], r#"-DNAME="x y""#);
        assert_eq!(words[3], "/IC:/a b/include");

        let mut defines = Vec::new();
        let mut includes = Vec::new();
        read_flags(&words, &mut |kind, value| match kind {
            FlagKind::Define => defines.push(value.to_string()),
            FlagKind::Include => includes.push(value.to_string()),
            _ => {}
        });
        assert_eq!(defines, vec![r#"NAME="x y""#]);
        assert_eq!(includes, vec!["C:/a b/include", "C:/v/include"]);
    }

    #[test]
    fn options_each_side_adds_are_left_out() {
        let options = |command: &str| {
            let mut found = Vec::new();
            read_flags(&split_command(command), &mut |kind, value| match kind {
                FlagKind::Option => found.push(value.to_string()),
                FlagKind::Standard => found.push(format!("std {value}")),
                _ => {}
            });
            found
        };

        // MSVC options read the same with `-` or `/`; output, debug and warning level are each side's own, the runtime
        // is compared (the source file is left out by the caller)
        let cmake = options(r"C:\VS\cl.exe /nologo -wd4251 /W3 /MDd /Zi /Ob0 /Od /RTC1 -std:c++17 /Foa.obj /Fdx\ /FS @a.modmap -c a.cpp");
        assert_eq!(cmake, ["-wd4251", "-MDd", "std c++17", "a.cpp"]);
        let anvil = options(r"C:\VS\cl.exe /nologo /utf-8 /std:c++17 /Od /Z7 /W4 /wd4251 /MDd /sourceDependencies a.json /c /Foa.obj /Tpa.cpp");
        assert_eq!(anvil, ["std c++17", "-wd4251", "-MDd"]);

        let gnu = options("g++ -std=c++17 -O0 -g -Wshadow -MD -MF a.d -MT a.o -c a.cpp -o a.o");
        assert_eq!(gnu, ["std c++17", "-Wshadow", "a.cpp"]);
    }

    #[test]
    fn cl_runtime_options_take_no_value() {
        let options = |command: &str| {
            let mut found = Vec::new();
            read_flags(&split_command(command), &mut |kind, value| {
                if let FlagKind::Option = kind {
                    found.push(value.to_string());
                }
            });
            found
        };

        // cl's -MT is the static runtime, compared; the next word is an option of its own
        assert_eq!(options(r"C:\VS\cl.exe -MT -wd4100 -c a.cpp"), ["-MT", "-wd4100", "a.cpp"]);
        assert_eq!(options(r"C:\VS\cl.exe /MTd /wd4100 -c a.cpp"), ["-MTd", "-wd4100", "a.cpp"]);
    }

    #[test]
    fn reads_the_cached_compiler_and_source_folder() {
        let cache = "# comment\nCMAKE_BUILD_TYPE:STRING=Debug\nCMAKE_C_COMPILER:STRING=C:/gcc/bin/gcc.exe\n\
                     CMAKE_HOME_DIRECTORY:INTERNAL=C:/old/project\n";
        assert_eq!(cached_value(cache, "CMAKE_C_COMPILER"), Some("C:/gcc/bin/gcc.exe"));
        assert_eq!(cached_value(cache, "CMAKE_HOME_DIRECTORY"), Some("C:/old/project"));
        assert_eq!(cached_value("CMAKE_C_COMPILER:FILEPATH=\n", "CMAKE_C_COMPILER"), None);
        assert_eq!(cached_value("CMAKE_C_COMPILER_ID:STRING=GNU\n", "CMAKE_C_COMPILER"), None);
        assert_eq!(cached_value("", "CMAKE_C_COMPILER"), None);
    }
}
