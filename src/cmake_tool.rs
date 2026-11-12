//! `cmake -E` without CMake: build steps converted from CMake copy, create, remove and touch files through it, so
//! Anvil runs those commands itself (`anvil cmake-e <command> ...`) and the project needs no CMake. Commands it does
//! not know still go to cmake.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// The hidden subcommand that runs them.
pub const SUBCOMMAND: &str = "cmake-e";

// The `cmake -E` commands Anvil runs itself
const COMMANDS: &[&str] = &[
    "copy",
    "copy_if_different",
    "copy_directory",
    "copy_directory_if_different",
    "make_directory",
    "remove",
    "rm",
    "remove_directory",
    "rename",
    "touch",
    "touch_nocreate",
    "echo",
    "echo_append",
    "true",
    "false",
    "env",
    "compare_files",
    "cat",
];

/// `cmake -E <command> ...` with a command Anvil runs itself: the words after `-E`.
pub fn handled(words: &[String]) -> Option<&[String]> {
    let [program, mode, command, ..] = words else { return None };
    let is_cmake = Path::new(program).file_stem().is_some_and(|stem| stem.eq_ignore_ascii_case("cmake"));

    (is_cmake && mode == "-E" && COMMANDS.contains(&command.as_str())).then(|| &words[2..])
}

/// A shell command line with its `cmake -E` parts (between `&&`) run by `anvil` instead. Unchanged when Anvil's
/// path has a space: cmd.exe's quoting cannot pass it through reliably.
pub fn rewrite_shell(text: &str, anvil: &Path) -> String {
    let anvil = anvil.to_string_lossy();
    if anvil.contains(' ') {
        return text.to_string();
    }

    text.split("&&")
        .map(|part| {
            let words: Vec<String> = part.split_whitespace().map(str::to_string).collect();
            if handled(&words).is_none() {
                return part.to_string();
            }

            let leading = &part[..part.len() - part.trim_start().len()];
            let rest = part.trim_start().splitn(3, char::is_whitespace).nth(2).unwrap_or_default();
            format!("{leading}{anvil} {SUBCOMMAND} {rest}")
        })
        .collect::<Vec<_>>()
        .join("&&")
}

/// Runs one command; the exit code is CMake's.
pub fn run(args: &[String]) -> i32 {
    match run_command(args) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("cmake -E {}: {error:#}", args.join(" "));
            1
        }
    }
}

fn run_command(args: &[String]) -> Result<i32> {
    let Some((command, rest)) = args.split_first() else {
        bail!("no command");
    };

    match command.as_str() {
        "copy" | "copy_if_different" => copy_files(rest, command == "copy_if_different")?,
        "copy_directory" | "copy_directory_if_different" => copy_directories(rest, command == "copy_directory_if_different")?,
        "make_directory" => {
            for folder in rest {
                std::fs::create_dir_all(folder).with_context(|| format!("creating {folder}"))?;
            }
        }
        "remove" => remove(rest.iter().filter(|word| *word != "-f"), false)?,
        "rm" => {
            let recursive = rest.iter().any(|word| word.starts_with('-') && word.contains(['r', 'R']));
            remove(rest.iter().filter(|word| !word.starts_with('-')), recursive)?;
        }
        "remove_directory" => remove(rest.iter(), true)?,
        "rename" => {
            let [from, to] = rest else { bail!("rename takes two paths") };
            std::fs::rename(from, to).with_context(|| format!("renaming {from} to {to}"))?;
        }
        "touch" | "touch_nocreate" => {
            for file in rest {
                touch(Path::new(file), command == "touch")?;
            }
        }
        "echo" => println!("{}", rest.join(" ")),
        "echo_append" => print!("{}", rest.join(" ")),
        "true" => {}
        "false" => return Ok(1),
        "env" => return run_with_env(rest),
        "compare_files" => return compare_files(rest),
        "cat" => {
            for file in rest {
                let bytes = std::fs::read(file).with_context(|| format!("reading {file}"))?;
                std::io::Write::write_all(&mut std::io::stdout(), &bytes)?;
            }
        }
        other => bail!("`{other}` is not a command Anvil runs (it is passed to cmake)"),
    }

    Ok(0)
}

/// copy <file>... <destination>, or copy -t <folder> <file>...: into the folder when there are several files or the
/// destination is one.
fn copy_files(rest: &[String], only_if_different: bool) -> Result<()> {
    let (destination, files) = match rest {
        [flag, folder, files @ ..] if flag == "-t" => (PathBuf::from(folder), files),
        [files @ .., destination] if !files.is_empty() => (PathBuf::from(destination), files),
        _ => bail!("copy needs files and a destination"),
    };

    let into_folder = files.len() > 1 || destination.is_dir();
    if into_folder {
        std::fs::create_dir_all(&destination)?;
    }
    for file in files {
        let source = Path::new(file);
        let target = if into_folder {
            destination.join(source.file_name().context("a file to copy has no name")?)
        } else {
            destination.clone()
        };
        copy_one(source, &target, only_if_different)?;
    }

    Ok(())
}

fn copy_directories(rest: &[String], only_if_different: bool) -> Result<()> {
    let [sources @ .., destination] = rest else {
        bail!("copy_directory needs folders and a destination")
    };
    if sources.is_empty() {
        bail!("copy_directory needs folders and a destination");
    }

    for source in sources {
        for entry in walkdir::WalkDir::new(source) {
            let entry = entry?;
            let relative = entry.path().strip_prefix(source)?;
            let target = Path::new(destination).join(relative);
            if entry.file_type().is_dir() {
                std::fs::create_dir_all(&target)?;
            } else {
                copy_one(entry.path(), &target, only_if_different)?;
            }
        }
    }

    Ok(())
}

fn copy_one(source: &Path, target: &Path, only_if_different: bool) -> Result<()> {
    if only_if_different {
        if let (Ok(old), Ok(new)) = (std::fs::read(target), std::fs::read(source)) {
            if old == new {
                return Ok(());
            }
        }
    }

    if let Some(parent) = target.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(source, target).with_context(|| format!("copying {} to {}", source.display(), target.display()))?;
    Ok(())
}

/// Removes files (and with `recursive`, folders); what does not exist is no error.
fn remove<'a>(paths: impl Iterator<Item = &'a String>, recursive: bool) -> Result<()> {
    for path in paths {
        let path = Path::new(path);
        let result = if path.is_dir() && recursive {
            std::fs::remove_dir_all(path)
        } else if path.is_dir() {
            bail!("{} is a folder (use rm -r)", path.display());
        } else {
            std::fs::remove_file(path)
        };

        match result {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error).with_context(|| format!("removing {}", path.display()));
            }
            _ => {}
        }
    }

    Ok(())
}

fn touch(file: &Path, create: bool) -> Result<()> {
    if !file.exists() {
        if create {
            std::fs::File::create(file).with_context(|| format!("creating {}", file.display()))?;
        }
        return Ok(());
    }

    let handle = std::fs::File::options()
        .write(true)
        .open(file)
        .with_context(|| format!("opening {}", file.display()))?;
    handle.set_modified(std::time::SystemTime::now())?;
    Ok(())
}

/// env [--unset=NAME]... [NAME=VALUE]... [--] command [args...]
fn run_with_env(rest: &[String]) -> Result<i32> {
    let mut command_start = rest.len();
    let mut unset = Vec::new();
    let mut set = Vec::new();
    for (index, word) in rest.iter().enumerate() {
        if let Some(name) = word.strip_prefix("--unset=") {
            unset.push(name.to_string());
        } else if word == "--" {
            command_start = index + 1;
            break;
        } else if let Some((name, value)) = word.split_once('=').filter(|(name, _)| !name.is_empty() && !name.contains(['/', '\\'])) {
            set.push((name.to_string(), value.to_string()));
        } else {
            command_start = index;
            break;
        }
    }

    let Some((program, args)) = rest[command_start.min(rest.len())..].split_first() else {
        bail!("env needs a command to run");
    };
    let mut command = std::process::Command::new(program);
    command.args(args);
    for name in unset {
        command.env_remove(name);
    }
    command.envs(set);

    let status = command.status().with_context(|| format!("running {program}"))?;
    Ok(status.code().unwrap_or(1))
}

/// 0 when the files are the same, 1 when they differ, 2 when one cannot be read.
fn compare_files(rest: &[String]) -> Result<i32> {
    let ignore_eol = rest.iter().any(|word| word == "--ignore-eol");
    let files: Vec<&String> = rest.iter().filter(|word| !word.starts_with("--")).collect();
    let [first, second] = files.as_slice() else {
        bail!("compare_files takes two files")
    };

    let (Ok(first), Ok(second)) = (std::fs::read(first), std::fs::read(second)) else {
        return Ok(2);
    };
    let same = if ignore_eol {
        let lines = |bytes: &[u8]| String::from_utf8_lossy(bytes).lines().map(str::to_string).collect::<Vec<_>>();
        lines(&first) == lines(&second)
    } else {
        first == second
    };

    Ok(if same { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn recognizes_commands() {
        assert!(handled(&words("cmake -E copy_if_different a b")).is_some());
        assert!(handled(&words("C:/tools/cmake.exe -E make_directory out")).is_some());
        assert!(handled(&words("cmake -E tar cf x.tar a")).is_none());
        assert!(handled(&words("cmake -P script.cmake")).is_none());
    }

    #[test]
    fn rewrites_shell_parts() {
        let anvil = Path::new("C:/bin/anvil.exe");
        let text = "cmake -E make_directory out/x && slangc a.slang -o out/x/a.spv";
        assert_eq!(
            rewrite_shell(text, anvil),
            "C:/bin/anvil.exe cmake-e make_directory out/x && slangc a.slang -o out/x/a.spv"
        );
        assert_eq!(rewrite_shell("cmake -E tar cf x a", anvil), "cmake -E tar cf x a");
        assert_eq!(rewrite_shell(text, Path::new("C:/Program Files/anvil.exe")), text);
    }

    #[test]
    fn runs_file_commands() {
        let folder = std::env::temp_dir().join(format!("anvil-cmake-e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        let path = |name: &str| folder.join(name).to_string_lossy().into_owned();

        assert_eq!(run(&words(&format!("make_directory {}", path("a/b")))), 0);
        std::fs::write(folder.join("a/b/one.txt"), "1").unwrap();
        assert_eq!(run(&words(&format!("copy {} {}", path("a/b/one.txt"), path("c")))), 0);
        assert_eq!(std::fs::read_to_string(folder.join("c")).unwrap(), "1");
        assert_eq!(run(&words(&format!("copy_directory {} {}", path("a"), path("d")))), 0);
        assert!(folder.join("d/b/one.txt").is_file());
        assert_eq!(run(&words(&format!("compare_files {} {}", path("c"), path("d/b/one.txt")))), 0);
        assert_eq!(run(&words(&format!("touch {}", path("e")))), 0);
        assert_eq!(run(&words(&format!("rm -rf {} {}", path("d"), path("missing")))), 0);
        assert!(!folder.join("d").exists());
        assert_eq!(run(&words("false")), 1);

        let _ = std::fs::remove_dir_all(&folder);
    }
}
