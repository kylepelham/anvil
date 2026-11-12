//! `anvil convert --check`: configures the CMake project with Ninja and the same compiler and vcpkg tree, plans the
//! Anvil build, and compares the two compile_commands.json file by file: which files each side compiles, and each
//! common file's definitions and include folders.

use crate::project::Model;
use crate::util::{self, slash};
use anyhow::{bail, Context, Result};
use indexmap::{IndexMap, IndexSet};
use std::path::{Path, PathBuf};
use std::process::Command;

// Definitions CMake adds on its own (CMAKE_<LANG>_FLAGS defaults, Ninja Multi-Config, shared library exports)
const CMAKE_IMPLICIT_DEFINES: &[&str] = &["WIN32", "_WINDOWS", "_MBCS", "UNICODE", "_UNICODE"];

// Files and differences listed per group before "and N more"
const LISTED_FILES: usize = 5;

// Lines of CMake's output shown when it cannot configure
const CMAKE_LOG_LINES: usize = 30;

/// One compiled file's settings that matter for a correct build.
#[derive(Default)]
struct FileFlags {
    shown_path: String,
    defines: IndexSet<String>,
    includes: IndexSet<String>,
}

/// Configures CMake, plans Anvil, and prints the differences. Returns the exit code: 0 when both compile the same.
pub fn check(model: &Model) -> Result<i32> {
    let cmake_folder = model.out_root.join("convert-check");
    configure_cmake(model, &cmake_folder)?;

    // Both databases, by file
    let cmake_entries = read_database(&cmake_folder.join("compile_commands.json"), &cmake_folder, model)?;
    let roots: Vec<usize> = model.default_targets(true);
    crate::plan::plan(model, None, &roots, false)?;
    let anvil_entries = read_database(&model.out_dir.join("compile_commands.json"), &model.out_dir, model)?;

    Ok(report(&cmake_entries, &anvil_entries))
}

/// Runs `cmake -G Ninja` with Anvil's compiler, profile, options and installed vcpkg packages.
fn configure_cmake(model: &Model, folder: &Path) -> Result<()> {
    let cmake = util::which("cmake").context("--check needs cmake on PATH")?;
    if util::which("ninja").is_none() && !tool_on_path(model, "ninja.exe") {
        bail!("--check needs ninja on PATH");
    }

    let mut args: Vec<String> = vec![
        "-S".into(),
        slash(&model.root),
        "-B".into(),
        slash(folder),
        "-G".into(),
        "Ninja".into(),
        "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON".into(),
        format!("-DCMAKE_BUILD_TYPE={}", cmake_build_type(model)),
        format!("-DCMAKE_C_COMPILER={}", slash(&model.toolchain.cc)),
        format!("-DCMAKE_CXX_COMPILER={}", slash(&model.toolchain.cxx)),
    ];

    // The options as Anvil resolved them
    for (name, value) in &model.condition.options {
        args.push(format!("-D{name}={}", if *value { "ON" } else { "OFF" }));
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
        read_flags(&words, &mut |kind, value| match kind {
            FlagKind::Define => {
                flags.defines.insert(value.to_string());
            }
            FlagKind::Include => {
                flags
                    .includes
                    .insert(comparable_path(&util::absolute(&directory, Path::new(value)), &prefixes));
            }
        });

        files.entry(comparable_path(&source, &prefixes)).or_insert_with(Vec::new).push(flags);
    }

    Ok(files)
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
}

/// Calls `found` for each definition and include folder in a compile command (attached or separate values).
fn read_flags(words: &[String], found: &mut dyn FnMut(FlagKind, &str)) {
    const DEFINE_FLAGS: &[&str] = &["-D", "/D"];
    const INCLUDE_FLAGS: &[&str] = &["-I", "/I", "-external:I", "/external:I", "-isystem", "-imsvc", "-iquote"];

    // The longest flag first: /external:I before /I
    let mut flags: Vec<(&str, bool)> = DEFINE_FLAGS.iter().map(|flag| (*flag, true)).collect();
    flags.extend(INCLUDE_FLAGS.iter().map(|flag| (*flag, false)));
    flags.sort_by_key(|(flag, _)| std::cmp::Reverse(flag.len()));

    let mut index = 1;
    while index < words.len() {
        let word = &words[index];
        index += 1;

        let Some((flag, is_define)) = flags.iter().find(|(flag, _)| word.starts_with(*flag)) else {
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

/// How many definitions and include folders one side has and the other lacks.
fn difference_size(cmake: &FileFlags, anvil: &FileFlags) -> usize {
    let defines = cmake
        .defines
        .iter()
        .filter(|define| !is_cmake_implicit(define) && !anvil.defines.contains(*define))
        .count()
        + anvil.defines.iter().filter(|define| !cmake.defines.contains(*define)).count();
    let includes = cmake.includes.symmetric_difference(&anvil.includes).count();

    defines + includes
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

    let lines = [
        difference_line("definitions only CMake has", only_cmake_defines),
        difference_line("definitions only Anvil has", only_anvil_defines),
        difference_line("include folders only CMake has", only_cmake_includes),
        difference_line("include folders only Anvil has", only_anvil_includes),
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
        });
        assert_eq!(defines, vec![r#"NAME="x y""#]);
        assert_eq!(includes, vec!["C:/a b/include", "C:/v/include"]);
    }
}
