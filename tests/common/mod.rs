//! What the integration tests share: a fresh copy of a fixture project per test, running anvil in it, reading what
//! it printed (the summary line, the steps it ran), and skipping tests whose compiler or tool is missing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

/// The anvil binary Cargo built for these tests.
pub const ANVIL: &str = env!("CARGO_BIN_EXE_anvil");

/// The toolchain families `anvil toolchains` lists.
const FAMILIES: [&str; 4] = ["msvc", "clang-cl", "clang", "gcc"];

// Numbers the temp folders of one test run, so two projects of one test never share a folder
static NEXT_FOLDER: AtomicUsize = AtomicUsize::new(0);

/// Skips the test (returns from it) when something it needs is missing: a toolchain (`"msvc"`, `"clang-cl"`,
/// `"clang"`, `"gcc"`), `"default"` (any toolchain), `"git"`, or `"windows"`.
#[macro_export]
macro_rules! require {
    ($($what:expr),+ $(,)?) => {
        $(
            if let Some(reason) = $crate::common::missing($what) {
                eprintln!("skipped: {reason}");
                return;
            }
        )+
    };
}

/// The toolchains found, the default one first. `anvil toolchains` runs once per test binary, before any build, so
/// the toolchain cache it fills is never written by two anvils at once.
pub fn toolchains() -> &'static [String] {
    static FOUND: OnceLock<Vec<String>> = OnceLock::new();

    FOUND.get_or_init(|| {
        let output = Command::new(ANVIL)
            .arg("toolchains")
            .env("NO_COLOR", "1")
            .output()
            .expect("running anvil toolchains");
        let text = String::from_utf8_lossy(&output.stdout);

        // "* msvc  19.51  C:/..." for the default, "  gcc  13.2.0  C:/..." for the others, "  clang  not found (...)"
        let mut found = Vec::new();
        for line in text.lines() {
            let default = line.starts_with('*');
            let mut words = line.trim_start_matches('*').split_whitespace();
            let (Some(name), Some(version)) = (words.next(), words.next()) else {
                continue;
            };
            if !FAMILIES.contains(&name) || version == "not" {
                continue;
            }
            if default {
                found.insert(0, name.to_string());
            } else {
                found.push(name.to_string());
            }
        }
        found
    })
}

/// Why a test cannot run here, or None. A toolchain named in `ANVIL_IT_TOOLCHAINS` (comma-separated, for CI) that is
/// missing fails the test instead.
pub fn missing(what: &str) -> Option<String> {
    let found = toolchains();
    match what {
        "default" if found.is_empty() => Some("no C/C++ toolchain found".into()),
        "default" => None,
        "windows" if !cfg!(windows) => Some("Windows only".into()),
        "windows" => None,
        "git" => {
            let runs = Command::new("git").arg("--version").output().is_ok_and(|output| output.status.success());
            (!runs).then(|| "git is not on PATH".into())
        }
        family if FAMILIES.contains(&family) => {
            if found.iter().any(|name| name == family) {
                return None;
            }
            let required = std::env::var("ANVIL_IT_TOOLCHAINS").unwrap_or_default();
            if required.split(',').any(|name| name.trim() == family) {
                panic!("toolchain {family} is required by ANVIL_IT_TOOLCHAINS but `anvil toolchains` does not find it");
            }
            Some(format!("toolchain {family} not found"))
        }
        other => panic!("require!: unknown requirement `{other}`"),
    }
}

/// A project in a temp folder of its own: a copy of a fixture, or empty. Deleted when the test passes, kept for a
/// look when it fails.
pub struct Project {
    pub root: PathBuf,
}

/// What one anvil run did.
pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
    pub command: String,
}

impl Project {
    /// A copy of `tests/fixtures/<case>` in a fresh temp folder named after the test.
    pub fn fixture(case: &str, test: &str) -> Project {
        let project = Project::empty(test);
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(case);
        copy_folder(&source, &project.root);
        project
    }

    /// An empty temp folder: `std::env::temp_dir()/anvil-it/<test>-<pid>-<n>`.
    pub fn empty(test: &str) -> Project {
        toolchains();

        let number = NEXT_FOLDER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join("anvil-it")
            .join(format!("{test}-{}-{number}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Project { root }
    }

    pub fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// Runs `anvil -C <root> <args>`.
    pub fn anvil(&self, args: &[&str]) -> Run {
        self.anvil_in(".", args)
    }

    /// Runs `anvil -C <root>/<folder> <args>`, from that folder.
    pub fn anvil_in(&self, folder: &str, args: &[&str]) -> Run {
        let folder = if folder == "." { self.root.clone() } else { self.root.join(folder) };
        let mut command = Command::new(ANVIL);
        command.arg("-C").arg(&folder).args(args).current_dir(&folder).env("NO_COLOR", "1");

        let started = Instant::now();
        let output = command.output().expect("starting anvil");

        Run {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            stderr: String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n"),
            elapsed: started.elapsed(),
            command: format!("anvil -C {} {}", folder.display(), args.join(" ")),
        }
    }

    #[track_caller]
    pub fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.path(relative)).unwrap_or_else(|error| panic!("reading {relative}: {error}"))
    }

    pub fn write(&self, relative: &str, text: &str) {
        let path = self.path(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap_or_else(|error| panic!("writing {relative}: {error}"));
    }

    /// Replaces `from` with `to` in a file; `from` must be there.
    #[track_caller]
    pub fn edit(&self, relative: &str, from: &str, to: &str) {
        let text = self.read(relative);
        assert!(text.contains(from), "{relative} has no `{from}`:\n{text}");
        self.write(relative, &text.replacen(from, to, 1));
    }

    /// Moves a file's modification time forward without changing its content.
    #[track_caller]
    pub fn touch(&self, relative: &str) {
        let before = self.modified(relative);
        let file = std::fs::File::options().write(true).open(self.path(relative)).unwrap();
        file.set_modified((before + Duration::from_secs(5)).max(SystemTime::now())).unwrap();
        drop(file);
        assert!(self.modified(relative) > before, "touching {relative} left its time");
    }

    pub fn modified(&self, relative: &str) -> SystemTime {
        std::fs::metadata(self.path(relative)).unwrap().modified().unwrap()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("project kept for a look: {}", self.root.display());
        } else {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

impl Run {
    /// stdout and stderr together.
    pub fn output(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }

    #[track_caller]
    pub fn success(&self) -> &Run {
        self.code(0)
    }

    #[track_caller]
    pub fn code(&self, code: i32) -> &Run {
        assert_eq!(
            self.code,
            code,
            "`{}` exited with {}, expected {code}\n{}",
            self.command,
            self.code,
            self.dump()
        );
        self
    }

    #[track_caller]
    pub fn contains(&self, text: &str) -> &Run {
        assert!(self.output().contains(text), "`{}` printed no `{text}`\n{}", self.command, self.dump());
        self
    }

    #[track_caller]
    pub fn lacks(&self, text: &str) -> &Run {
        assert!(!self.output().contains(text), "`{}` printed `{text}`\n{}", self.command, self.dump());
        self
    }

    /// The build had nothing to do: "Fresh debug [...] up to date".
    #[track_caller]
    pub fn up_to_date(&self) -> &Run {
        assert!(
            self.summary().contains("up to date"),
            "`{}` rebuilt something\n{}",
            self.command,
            self.dump()
        );
        self
    }

    /// How many files the build compiled, from "Finished debug [...] in 1.2s (2 compiled, 1 linked)".
    #[track_caller]
    pub fn compiled(&self) -> usize {
        self.count("compiled")
    }

    #[track_caller]
    pub fn linked(&self) -> usize {
        self.count("linked")
    }

    /// What the build ran, from the "[3/7] Compiling src/main.c" lines: "Compiling src/main.c".
    pub fn actions(&self) -> Vec<String> {
        self.output()
            .lines()
            .filter_map(|line| {
                let rest = line.strip_prefix('[')?;
                let (counts, description) = rest.split_once("] ")?;
                counts.split_once('/')?;
                Some(description.to_string())
            })
            .collect()
    }

    /// The program `anvil run` started, from its "Running <path> <args>" line (relative to the folder anvil ran in).
    #[track_caller]
    pub fn program(&self) -> String {
        let line = self
            .output()
            .lines()
            .find_map(|line| line.trim_start().strip_prefix("Running ").map(str::to_string))
            .unwrap_or_else(|| panic!("`{}` ran no program\n{}", self.command, self.dump()));
        line.trim_end().split(' ').next().unwrap().to_string()
    }

    #[track_caller]
    fn summary(&self) -> String {
        let output = self.output();
        let line = output.lines().find(|line| {
            let line = line.trim_start();
            line.starts_with("Finished ") || line.starts_with("Fresh ")
        });
        line.unwrap_or_else(|| panic!("`{}` printed no build summary\n{}", self.command, self.dump()))
            .to_string()
    }

    #[track_caller]
    fn count(&self, what: &str) -> usize {
        let summary = self.summary();
        let Some((_, counts)) = summary.rsplit_once('(') else { return 0 };
        counts
            .trim_end_matches(')')
            .split(", ")
            .find_map(|part| part.strip_suffix(&format!(" {what}"))?.parse().ok())
            .unwrap_or(0)
    }

    fn dump(&self) -> String {
        format!("--- stdout\n{}--- stderr\n{}---", self.stdout, self.stderr)
    }
}

fn copy_folder(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap_or_else(|error| panic!("reading {}: {error}", from.display())) {
        let entry = entry.unwrap();
        let destination = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_folder(&entry.path(), &destination);
        } else {
            std::fs::copy(entry.path(), &destination).unwrap();
        }
    }
}
