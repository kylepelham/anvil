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
    "chdir",
    "sleep",
    "create_symlink",
    "md5sum",
    "sha1sum",
    "sha224sum",
    "sha256sum",
    "sha384sum",
    "sha512sum",
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
        "remove" => return remove(rest.iter().filter(|word| *word != "-f"), false, true),
        "rm" => {
            let options: Vec<&String> = rest.iter().take_while(|word| word.starts_with('-') && *word != "--").collect();
            let recursive = options.iter().any(|word| word.contains(['r', 'R']));
            let force = options.iter().any(|word| word.contains('f'));

            let mut paths = &rest[options.len()..];
            if paths.first().is_some_and(|word| word == "--") {
                paths = &paths[1..];
            }
            return remove(paths.iter(), recursive, force);
        }
        "remove_directory" => return remove(rest.iter().filter(|word| Path::new(word).is_dir()), true, true),
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
        "chdir" => {
            let [folder, program, args @ ..] = rest else {
                bail!("chdir takes a folder and a command")
            };
            if !Path::new(folder).is_dir() {
                bail!("the folder {folder} does not exist");
            }

            // A program given by path is found from the folder, as the shell would (Windows looks from ours)
            let program = match program.contains(['/', '\\']) {
                true => std::path::absolute(folder)?.join(program),
                false => PathBuf::from(program),
            };
            let status = std::process::Command::new(&program)
                .args(args)
                .current_dir(folder)
                .status()
                .with_context(|| format!("running {}", program.display()))?;
            return Ok(status.code().unwrap_or(1));
        }
        "sleep" => {
            if rest.is_empty() {
                bail!("sleep takes a number of seconds");
            }
            for seconds in rest {
                let seconds: f64 = seconds
                    .parse()
                    .ok()
                    .filter(|seconds: &f64| *seconds >= 0.0)
                    .with_context(|| format!("`{seconds}` is not a number of seconds"))?;
                std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
            }
        }
        "create_symlink" => {
            let [target, link] = rest else {
                bail!("create_symlink takes the existing path and the link to create")
            };
            create_symlink(Path::new(target), Path::new(link))?;
        }
        "md5sum" | "sha1sum" | "sha224sum" | "sha256sum" | "sha384sum" | "sha512sum" => return print_hashes(command, rest),
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

/// Removes files (and with `recursive`, folders). As in CMake, a missing one is an error only without `force`, and the
/// rest are still removed.
fn remove<'a>(paths: impl Iterator<Item = &'a String>, recursive: bool, force: bool) -> Result<i32> {
    let mut code = 0;

    for path in paths {
        let path = Path::new(path);
        let is_symlink = path.symlink_metadata().is_ok_and(|metadata| metadata.file_type().is_symlink());
        let result = if is_symlink {
            // The link itself, never what it points to (a link to a folder is a folder to Windows)
            std::fs::remove_file(path).or_else(|_| std::fs::remove_dir(path))
        } else if path.is_dir() && recursive {
            std::fs::remove_dir_all(path)
        } else if path.is_dir() {
            bail!("{} is a folder (use rm -r)", path.display());
        } else {
            std::fs::remove_file(path)
        };

        match result {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !force {
                    eprintln!("cmake -E rm: {} does not exist (rm -f allows that)", path.display());
                    code = 1;
                }
            }
            Err(error) => return Err(error).with_context(|| format!("removing {}", path.display())),
            Ok(()) => {}
        }
    }

    Ok(code)
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

/// What `env` does: its variable changes in order (`None` unsets one), and the command it runs with them.
pub struct EnvCommand<'a> {
    pub changes: Vec<(String, Option<String>)>,
    pub command: &'a [String],
}

impl EnvCommand<'_> {
    /// Applies the changes to a command, in order.
    pub fn apply(&self, command: &mut std::process::Command) {
        for (name, value) in &self.changes {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
    }
}

/// env [--unset=NAME | --modify NAME=OPERATION:VALUE | NAME=VALUE]... [--] command [args...], changes applied in order.
/// `--modify` changes a variable's value in `base` (a test's `env`), else in Anvil's environment.
pub fn parse_env<'a>(rest: &'a [String], base: &[(String, String)]) -> Result<EnvCommand<'a>> {
    let mut changes: Vec<(String, Option<String>)> = Vec::new();
    let mut command_start = rest.len();
    let mut index = 0;

    while index < rest.len() {
        let word = &rest[index];
        if word == "--" {
            command_start = index + 1;
            break;
        } else if let Some(name) = word.strip_prefix("--unset=") {
            set_variable(&mut changes, name, None);
        } else if word == "--modify" {
            let change = rest.get(index + 1).context("--modify needs NAME=OPERATION:VALUE")?;
            modify_variable(&mut changes, base, change)?;
            index += 1;
        } else if word.starts_with("--") {
            bail!("unknown option `{word}` (env takes --unset=NAME, --modify NAME=OPERATION:VALUE and NAME=VALUE)");
        } else if let Some((name, value)) = word.split_once('=').filter(|(name, _)| !name.is_empty() && !name.contains(['/', '\\'])) {
            set_variable(&mut changes, name, Some(value.to_string()));
        } else {
            command_start = index;
            break;
        }
        index += 1;
    }

    Ok(EnvCommand {
        changes,
        command: &rest[command_start.min(rest.len())..],
    })
}

fn run_with_env(rest: &[String]) -> Result<i32> {
    let env = parse_env(rest, &[])?;
    let Some((program, args)) = env.command.split_first() else {
        bail!("env needs a command to run");
    };
    let mut command = std::process::Command::new(program);
    command.args(args);
    env.apply(&mut command);

    let status = command.status().with_context(|| format!("running {program}"))?;
    Ok(status.code().unwrap_or(1))
}

/// One `--modify NAME=OPERATION:VALUE`, against the value earlier changes left.
fn modify_variable(changes: &mut Vec<(String, Option<String>)>, base: &[(String, String)], change: &str) -> Result<()> {
    let Some((name, operation_value)) = change.split_once('=').filter(|(name, _)| !name.is_empty()) else {
        bail!("--modify {change}: expected NAME=OPERATION:VALUE");
    };
    let (operation, value) = operation_value.split_once(':').unwrap_or((operation_value, ""));

    let current = variable(changes, base, name).unwrap_or_default();
    let list_separator = if cfg!(windows) { ";" } else { ":" };
    let joined = |first: &str, separator: &str, second: &str| match (first.is_empty(), second.is_empty()) {
        (true, _) => second.to_string(),
        (_, true) => first.to_string(),
        _ => format!("{first}{separator}{second}"),
    };

    let new_value = match operation {
        "reset" => {
            changes.retain(|(changed, _)| !same_variable(changed, name));
            return Ok(());
        }
        "set" => Some(value.to_string()),
        "unset" => None,
        "string_append" => Some(format!("{current}{value}")),
        "string_prepend" => Some(format!("{value}{current}")),
        "path_list_append" => Some(joined(&current, list_separator, value)),
        "path_list_prepend" => Some(joined(value, list_separator, &current)),
        "cmake_list_append" => Some(joined(&current, ";", value)),
        "cmake_list_prepend" => Some(joined(value, ";", &current)),
        other => bail!(
            "--modify {change}: unknown operation `{other}` (reset, set, unset, string_append, string_prepend, path_list_append, \
             path_list_prepend, cmake_list_append or cmake_list_prepend)"
        ),
    };
    set_variable(changes, name, new_value);

    Ok(())
}

/// A variable's value with the changes so far applied.
fn variable(changes: &[(String, Option<String>)], base: &[(String, String)], name: &str) -> Option<String> {
    match changes.iter().rev().find(|(changed, _)| same_variable(changed, name)) {
        Some((_, value)) => value.clone(),
        None => match base.iter().rev().find(|(given, _)| same_variable(given, name)) {
            Some((_, value)) => Some(value.clone()),
            None => std::env::var(name).ok(),
        },
    }
}

fn set_variable(changes: &mut Vec<(String, Option<String>)>, name: &str, value: Option<String>) {
    changes.retain(|(changed, _)| !same_variable(changed, name));
    changes.push((name.to_string(), value));
}

/// Environment variable names ignore case on Windows.
fn same_variable(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

/// create_symlink: replaces what `link` names. Windows allows symbolic links only in Developer Mode or to
/// administrators, and a link to a folder must say so.
fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    if link.symlink_metadata().is_ok() {
        let _ = std::fs::remove_file(link).or_else(|_| std::fs::remove_dir(link));
    }

    #[cfg(windows)]
    let result = {
        let resolved = link.parent().unwrap_or(Path::new("")).join(target);
        if resolved.is_dir() {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        }
    };
    #[cfg(not(windows))]
    let result = std::os::unix::fs::symlink(target, link);

    result.with_context(|| {
        let hint = if cfg!(windows) {
            " (Windows allows them in Developer Mode or for administrators)"
        } else {
            ""
        };
        format!("creating the symbolic link {} -> {}{hint}", link.display(), target.display())
    })
}

/// md5sum, sha256sum and the like: "<hash>  <file>" per file. A file that cannot be read is reported, and makes the
/// result 1 once the rest are done.
fn print_hashes(command: &str, files: &[String]) -> Result<i32> {
    if files.is_empty() {
        bail!("{command} takes files");
    }

    let mut code = 0;
    for file in files {
        if Path::new(file).is_dir() {
            eprintln!("cmake -E {command}: {file} is a folder");
            code = 1;
            continue;
        }

        match std::fs::read(file) {
            Ok(bytes) => println!("{}  {file}", hex_digest(command, &bytes)),
            Err(error) => {
                eprintln!("cmake -E {command}: {file}: {error}");
                code = 1;
            }
        }
    }

    Ok(code)
}

fn hex_digest(command: &str, bytes: &[u8]) -> String {
    use sha2::Digest;

    let digest = match command {
        "md5sum" => md5::Md5::digest(bytes).to_vec(),
        "sha1sum" => sha1::Sha1::digest(bytes).to_vec(),
        "sha224sum" => sha2::Sha224::digest(bytes).to_vec(),
        "sha256sum" => sha2::Sha256::digest(bytes).to_vec(),
        "sha384sum" => sha2::Sha384::digest(bytes).to_vec(),
        _ => sha2::Sha512::digest(bytes).to_vec(),
    };

    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 0 when the files are the same, 1 when they differ or one cannot be read (as CMake 4 does), 2 for extra arguments.
fn compare_files(rest: &[String]) -> Result<i32> {
    let ignore_eol = rest.iter().any(|word| word == "--ignore-eol");
    let files: Vec<&String> = rest.iter().filter(|word| !word.starts_with("--")).collect();
    if files.len() > 2 {
        eprintln!("cmake -E compare_files: takes two files, not {}", files.len());
        return Ok(2);
    }
    let [first, second] = files.as_slice() else {
        bail!("compare_files takes two files")
    };

    let (Ok(first), Ok(second)) = (std::fs::read(first), std::fs::read(second)) else {
        return Ok(1);
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
        assert_eq!(run(&words(&format!("rm {}", path("missing")))), 1);
        assert_eq!(run(&words(&format!("remove {}", path("missing")))), 0);
        assert_eq!(run(&words(&format!("compare_files {} {}", path("c"), path("missing")))), 1);
        assert_eq!(run(&words("false")), 1);

        // remove_directory leaves files alone, as CMake does
        assert_eq!(run(&words(&format!("remove_directory {} {}", path("c"), path("a")))), 0);
        assert!(folder.join("c").is_file());
        assert!(!folder.join("a").exists());

        let _ = std::fs::remove_dir_all(&folder);
    }

    #[cfg(windows)]
    #[test]
    fn chdir_finds_a_program_from_the_folder() {
        let folder = std::env::temp_dir().join(format!("anvil-cmake-e-chdir-{}", std::process::id()));
        std::fs::create_dir_all(folder.join("sub")).unwrap();
        std::fs::write(folder.join("sub/tool.bat"), "@exit /b 3\r\n").unwrap();

        let folder_text = folder.to_string_lossy().into_owned();
        assert_eq!(run(&words(&format!("chdir {folder_text} sub/tool.bat"))), 3);

        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn modifies_variables() {
        let mut changes = Vec::new();
        set_variable(&mut changes, "ANVIL_TEST_A", Some("a".into()));
        modify_variable(&mut changes, &[], "ANVIL_TEST_A=string_append:b").unwrap();
        modify_variable(&mut changes, &[], "ANVIL_TEST_A=cmake_list_prepend:x").unwrap();
        assert_eq!(variable(&changes, &[], "ANVIL_TEST_A").as_deref(), Some("x;ab"));

        modify_variable(&mut changes, &[], "ANVIL_TEST_B=path_list_append:y").unwrap();
        assert_eq!(variable(&changes, &[], "ANVIL_TEST_B").as_deref(), Some("y"));
        modify_variable(&mut changes, &[], "ANVIL_TEST_B=unset:").unwrap();
        assert_eq!(variable(&changes, &[], "ANVIL_TEST_B"), None);
        modify_variable(&mut changes, &[], "ANVIL_TEST_A=reset:").unwrap();
        assert_eq!(variable(&changes, &[], "ANVIL_TEST_A"), None);

        // A test's env is what --modify starts from
        let base = [("ANVIL_TEST_C".to_string(), "base".to_string())];
        modify_variable(&mut changes, &base, "ANVIL_TEST_C=string_append:_x").unwrap();
        assert_eq!(variable(&changes, &base, "ANVIL_TEST_C").as_deref(), Some("base_x"));

        assert!(modify_variable(&mut changes, &[], "ANVIL_TEST_A=bad:x").is_err());
        assert!(modify_variable(&mut changes, &[], "ANVIL_TEST_A").is_err());
    }

    #[test]
    fn hashes_like_cmake() {
        assert_eq!(
            hex_digest("sha256sum", b"hi\n"),
            "98ea6e4f216f2fb4b69fff9b3a44842c38686ca685f3f55dc48c5d3fb1107be4"
        );
        assert_eq!(hex_digest("md5sum", b"hi\n"), "764efa883dda1e11db47671c4a3bbd9e");
        assert_eq!(hex_digest("sha1sum", b"hi\n"), "55ca6286e3e4f4fba5d0448333fa99fc5a404a73");
    }
}
