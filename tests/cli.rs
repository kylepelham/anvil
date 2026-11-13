//! End-to-end tests of the anvil binary: each copies a fixture project from `tests/fixtures/` into a temp folder and
//! runs anvil in it. Tests whose compiler is missing are skipped (`ANVIL_IT_TOOLCHAINS=gcc,msvc` makes that a
//! failure); tests that need the network are ignored.

#[macro_use]
mod common;

use common::{Project, Run};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// `--toolchain <name>` when a test runs on a named toolchain, nothing for the default one.
fn with_toolchain<'a>(toolchain: Option<&'a str>, args: &[&'a str]) -> Vec<&'a str> {
    let mut all = args.to_vec();
    if let Some(name) = toolchain {
        all.extend(["--toolchain", name]);
    }
    all
}

/// The program's own output from `anvil run`: stdout, where anvil's lines are not.
fn program_output(run: &Run) -> &str {
    run.stdout.trim()
}

// 1. A new program runs, and a second build has nothing to do

fn new_program(toolchain: Option<&str>, test: &str) {
    let project = Project::empty(test);
    project.anvil(&["new", "hello"]).success();

    let run = project.anvil_in("hello", &with_toolchain(toolchain, &["run"]));
    run.success();
    assert_eq!(program_output(&run), "Hello from Anvil!");

    project.anvil_in("hello", &with_toolchain(toolchain, &["build"])).success().up_to_date();
}

#[test]
fn new_program_runs_then_is_up_to_date() {
    require!("default");
    new_program(None, "new_program");
}

#[test]
fn new_program_runs_then_is_up_to_date_gcc() {
    require!("gcc");
    new_program(Some("gcc"), "new_program_gcc");
}

// 2. A new library's test passes; a broken one fails the run and is named

#[test]
fn new_library_test_passes_then_names_the_failure() {
    require!("default");
    let project = Project::empty("new_library");
    project.anvil(&["new", "x", "--lib"]).success();

    project
        .anvil_in("x", &["test"])
        .success()
        .contains("test test_x ... ok")
        .contains("Passed 1 of 1");

    project.edit("x/src/x.cpp", "\"Hello, \"", "\"Bye, \"");
    project
        .anvil_in("x", &["test"])
        .code(1)
        .contains("test test_x ... FAILED")
        .contains("---- failed: test_x")
        .contains("1 of 1 tests failed: test_x");
}

// 3. A misspelled key is an error that names the right one

#[test]
fn manifest_typo_suggests_the_key() {
    let project = Project::fixture("typo", "typo");
    project
        .anvil(&["build"])
        .code(1)
        .contains("unknown field `linkflags`")
        .contains("did you mean `link-flags`");
}

// 4. Rebuilds follow content: touching a header rebuilds nothing, editing one recompiles only its includers

fn content_hash_rebuilds(toolchain: Option<&str>, test: &str) {
    let project = Project::fixture("headers", test);
    let build = || project.anvil(&with_toolchain(toolchain, &["build"]));

    let first = build();
    first.success();
    assert_eq!(first.compiled(), 3);

    project.touch("src/a.h");
    project.touch("src/b.h");
    project.touch("src/main.c");
    build().success().up_to_date();

    project.write("src/a.h", "#define A_VALUE 3\n");
    let edited = build();
    edited.success();
    assert_eq!(edited.compiled(), 2, "{:?}", edited.actions());
    assert_eq!(edited.linked(), 1, "{:?}", edited.actions());
    let actions = edited.actions();
    assert!(actions.contains(&"Compiling src/main.c".to_string()), "{actions:?}");
    assert!(actions.contains(&"Compiling src/third.c".to_string()), "{actions:?}");
    assert!(!actions.iter().any(|action| action.contains("other.c")), "{actions:?}");

    let run = project.anvil(&with_toolchain(toolchain, &["run"]));
    run.success();
    assert_eq!(program_output(&run), "8");
}

#[test]
fn content_hash_rebuilds_only_what_changed() {
    require!("default");
    content_hash_rebuilds(None, "content_hash");
}

#[test]
fn content_hash_rebuilds_only_what_changed_gcc() {
    require!("gcc");
    content_hash_rebuilds(Some("gcc"), "content_hash_gcc");
}

// 5. An option toggled with -o changes the command line, so the file recompiles each way

#[test]
fn option_toggle_recompiles() {
    require!("default");
    let project = Project::fixture("feature", "option_toggle");

    let slow = project.anvil(&["run"]);
    slow.success();
    assert_eq!(program_output(&slow), "slow");

    let fast = project.anvil(&["run", "-o", "fast"]);
    fast.success();
    assert_eq!(program_output(&fast), "fast");
    assert_eq!(fast.compiled(), 1);
    project.anvil(&["build", "-o", "fast=true"]).success().up_to_date();

    // Back to the default: the last build's command line is the one remembered, so it compiles again once
    let back = project.anvil(&["build", "-o", "!fast"]);
    back.success();
    assert_eq!(back.compiled(), 1);
    project.anvil(&["build"]).success().up_to_date();
}

// 6. A library's own defines stay its own, public ones reach its users too, interface ones only its users

fn visibility(toolchain: Option<&str>, test: &str) {
    let project = Project::fixture("visibility", test);
    let run = project.anvil(&with_toolchain(toolchain, &["run", "app"]));
    run.success();
    assert_eq!(program_output(&run), "core 7");
}

#[test]
fn visibility_of_defines() {
    require!("default");
    visibility(None, "visibility");
}

#[test]
fn visibility_of_defines_gcc() {
    require!("gcc");
    visibility(Some("gcc"), "visibility_gcc");
}

// 7. when.windows / when.linux sources apply on their platform only; an unknown condition word is an error

#[test]
fn conditions_pick_platform_sources() {
    require!("default");
    let project = Project::fixture("conditions", "conditions");

    let run = project.anvil(&["run"]);
    run.success();
    let expected = if cfg!(windows) { "platform windows" } else { "platform linux" };
    assert_eq!(program_output(&run), expected);

    let manifest = project.read("anvil.toml");
    project.write("anvil.toml", &format!("{manifest}\n[target.app.when.windwos]\ndefines = [\"TYPO\"]\n"));
    project.anvil(&["build"]).code(1).contains("unknown condition `windwos`");
}

// 8. Build steps: a for-each step runs a generator target once per input, and only for the input that changed;
//    [[write]] leaves a file alone when its text is the same

#[test]
fn build_steps_rerun_only_for_changed_inputs() {
    require!("default");
    let project = Project::fixture("steps", "steps");

    let first = project.anvil(&["run", "app"]);
    first.success();
    assert_eq!(program_output(&first), "one two 1.2.3");
    let actions = first.actions();
    assert!(
        actions
            .iter()
            .any(|action| action.starts_with("Running gen") && action.ends_with("alpha.txt")),
        "{actions:?}"
    );
    assert!(
        actions
            .iter()
            .any(|action| action.starts_with("Running gen") && action.ends_with("beta.txt")),
        "{actions:?}"
    );

    let written = project.modified("src/config.h");
    std::thread::sleep(Duration::from_millis(50));
    project.anvil(&["build"]).success().up_to_date();
    assert_eq!(
        project.modified("src/config.h"),
        written,
        "[[write]] rewrote a file whose text did not change"
    );

    project.write("data/alpha.txt", "uno\n");
    let edited = project.anvil(&["build"]);
    edited.success();
    let steps: Vec<String> = edited.actions().into_iter().filter(|action| action.starts_with("Running")).collect();
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert!(steps[0].ends_with("alpha.txt"), "{steps:?}");

    // A changed text is written
    project.edit("anvil.toml", "version = \"1.2.3\"", "version = \"1.2.4\"");
    let run = project.anvil(&["run", "app"]);
    run.success();
    assert_eq!(program_output(&run), "uno two 1.2.4");
}

// 9. A shared library with an export header: the program runs with the library beside it

fn shared_library(toolchain: Option<&str>, test: &str) {
    let project = Project::fixture("shared", test);

    let run = project.anvil(&with_toolchain(toolchain, &["run", "app"]));
    run.success();
    assert_eq!(program_output(&run), "answer 42");

    // Started by hand from its folder's copy of the library, not through anvil
    let program = project.root.join(run.program());
    let folder = program.parent().unwrap();
    if cfg!(windows) {
        let libraries: Vec<String> = std::fs::read_dir(folder)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".dll") && name.contains("mylib"))
            .collect();
        assert_eq!(libraries.len(), 1, "no mylib DLL beside {}", program.display());
    }
    let output = Command::new(&program).output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "answer 42");
}

#[test]
fn shared_library_runs_beside_its_program() {
    require!("default");
    shared_library(None, "shared");
}

#[test]
fn shared_library_runs_beside_its_program_gcc() {
    require!("gcc");
    shared_library(Some("gcc"), "shared_gcc");
}

// 10. Test keys: expect, reject, exit-code, will-fail, after, serial pass; failures say why; a timeout kills

#[test]
fn test_keys_judge_and_schedule_tests() {
    require!("default");
    let project = Project::fixture("testkeys", "testkeys");

    // ok_second needs ok_first's file (after); the probes fail if they ever see ok_serial's lock (serial)
    project
        .anvil(&["test", "ok_"])
        .success()
        .contains("Passed 9 of 9")
        .contains("test ok_second ... ok")
        .contains("test ok_serial ... ok");

    project
        .anvil(&["test", "bad_"])
        .code(1)
        .contains("---- failed: bad_expect (output has no match for expect \"goodbye\")")
        .contains("---- failed: bad_reject (output matches reject \"ERROR\": \"ERROR\")")
        .contains("---- failed: bad_exit (exit code 2)")
        .contains("---- failed: bad_will_fail (passed, but will-fail is set")
        .contains("4 of 4 tests failed");

    let slow = project.anvil(&["test", "slow_timeout"]);
    slow.code(1).contains("---- failed: slow_timeout (timed out after 1s)");
    assert!(
        slow.elapsed < Duration::from_secs(20),
        "the sleeping test was not killed: {:?}",
        slow.elapsed
    );
}

// 11. A [[test]] matrix in a subfolder's tests.toml expands to one test per combination; a filter picks tests

#[test]
fn test_matrix_expands_and_filters() {
    require!("default");
    let project = Project::fixture("matrix", "matrix");

    let targets = project.anvil(&["targets"]);
    targets.success();
    let line = targets
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("tests: "))
        .expect("a tests: line");
    let mut names: Vec<&str> = line.split(", ").collect();
    names.sort();
    assert_eq!(names, ["m_large_fast", "m_large_slow", "m_small_fast", "m_small_slow", "root_test"]);

    project
        .anvil(&["test", "large"])
        .success()
        .contains("Testing 2 tests")
        .contains("test m_large_fast ... ok")
        .contains("test m_large_slow ... ok")
        .lacks("m_small")
        .lacks("root_test");

    project.anvil(&["test"]).success().contains("Passed 5 of 5");
}

// 12. compdb writes valid JSON with one entry per source

#[test]
fn compdb_lists_every_source() {
    require!("default");
    let project = Project::fixture("compdb", "compdb");
    project.anvil(&["compdb"]).success();

    let text = project.read("compile_commands.json");
    let entries: Vec<serde_json::Value> = serde_json::from_str(&text).expect("compile_commands.json is JSON");
    let mut files: Vec<String> = entries
        .iter()
        .map(|entry| {
            assert!(entry["directory"].is_string(), "{entry}");
            assert!(entry["arguments"].is_array() || entry["command"].is_string(), "{entry}");
            let file = entry["file"].as_str().expect("a file").replace('\\', "/");
            file.rsplit('/').next().unwrap().to_string()
        })
        .collect();
    files.sort();
    assert_eq!(files, ["main.c", "one.c", "two.c"]);
}

// 13. A workspace with a path dependency builds from its root and from a member folder

#[test]
fn workspace_builds_from_root_and_member() {
    require!("default");
    let project = Project::fixture("workspace", "workspace");

    let first = project.anvil(&["build"]);
    first.success();
    assert_eq!(first.compiled(), 2);

    project.anvil_in("app", &["build"]).success().up_to_date();
    let run = project.anvil_in("app", &["run"]);
    run.success();
    assert_eq!(program_output(&run), "core says 5");

    project.anvil_in("core", &["targets"]).success().contains("core").contains("app");
}

#[test]
fn workspace_root_without_sources_does_not_warn() {
    require!("default");
    let project = Project::fixture("workspace", "workspace_warning");
    project.anvil(&["build"]).success().lacks("warning:");
}

#[test]
fn workspace_member_cannot_set_root_only_keys() {
    let project = Project::fixture("workspace", "workspace_root_keys");
    project.edit("core/anvil.toml", "name = \"core\"", "name = \"core\"\nlauncher = \"sccache\"");
    project
        .anvil(&["targets"])
        .code(1)
        .contains("core/anvil.toml: [project] launcher is read only from the workspace root");
}

// 14. A CMake project converts to the expected anvil.toml, which builds and tests

#[test]
fn convert_cmake_project_builds_and_tests() {
    require!("default");
    let project = Project::fixture("cmake", "convert");
    project.anvil(&["convert"]).success();

    let golden = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("cmake.anvil.toml");
    let converted = project.read("anvil.toml").replace("\r\n", "\n");
    if std::env::var_os("ANVIL_BLESS").is_some() {
        std::fs::create_dir_all(golden.parent().unwrap()).unwrap();
        std::fs::write(&golden, &converted).unwrap();
    }
    let expected = std::fs::read_to_string(&golden)
        .expect("the golden file (ANVIL_BLESS=1 writes it)")
        .replace("\r\n", "\n");
    assert_eq!(
        converted,
        expected,
        "anvil.toml differs from {} (ANVIL_BLESS=1 refreshes it)",
        golden.display()
    );

    project.anvil(&["build"]).success();
    project.anvil(&["test"]).success().contains("test calc_runs ... ok");

    let fast = project.anvil(&["build", "-o", "CALC_FAST"]);
    fast.success();
    assert_eq!(fast.compiled(), 1);
}

// 15. A git dependency is pinned in anvil.lock; under --locked a changed tag is an error

fn git(folder: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=anvil-it",
            "-c",
            "user.email=anvil-it@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(["-c", "tag.gpgsign=false", "-c", "init.defaultBranch=main", "-c", "core.autocrlf=false"])
        .args(args)
        .current_dir(folder)
        .output()
        .expect("running git");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Anvil's shared git cache: `<user data>/anvil/git`.
fn git_cache() -> Option<PathBuf> {
    let user_data = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
    };
    user_data.map(|folder| folder.join("anvil").join("git"))
}

/// Deletes the checkouts of the test's repositories from anvil's shared git cache (`<user data>/anvil/git/<name>-<key>`)
/// when the test ends: those whose remote is inside the test's folder, so another test's checkouts of a repository
/// with the same commit stay.
struct GitCheckouts(PathBuf);

impl GitCheckouts {
    fn of(project: &Project) -> GitCheckouts {
        GitCheckouts(project.root.clone())
    }
}

impl Drop for GitCheckouts {
    fn drop(&mut self) {
        let Some(cache) = git_cache() else { return };

        // "url = file:///C:/..." in the checkout's .git/config, compared without the scheme, slashes one way
        let comparable = |text: &str| {
            text.trim_start_matches("file://")
                .trim_start_matches('/')
                .replace('\\', "/")
                .to_lowercase()
        };
        let ours = format!("{}/", comparable(&self.0.display().to_string()));
        for entry in std::fs::read_dir(cache).into_iter().flatten().flatten() {
            let config = std::fs::read_to_string(entry.path().join(".git").join("config")).unwrap_or_default();
            let remote = config.lines().find_map(|line| line.trim().strip_prefix("url = "));
            if remote.is_some_and(|url| comparable(url).starts_with(&ours)) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

#[test]
fn lock_file_pins_git_dependency() {
    require!("default", "git");
    let project = Project::empty("lock_file");

    // The dependency: a library whose value is its tag's number
    let dependency = project.path("itdep");
    project.write("itdep/anvil.toml", "[project]\nname = \"itdep\"\n");
    project.write("itdep/include/itdep/itdep.h", "#pragma once\nint itdep_value(void);\n");
    project.write("itdep/src/itdep.c", "int itdep_value(void) { return 1; }\n");
    git(&dependency, &["init", "--quiet"]);
    git(&dependency, &["add", "-A"]);
    git(&dependency, &["commit", "--quiet", "-m", "one"]);
    git(&dependency, &["tag", "v1"]);
    project.write("itdep/src/itdep.c", "int itdep_value(void) { return 2; }\n");
    git(&dependency, &["commit", "--quiet", "-am", "two"]);
    git(&dependency, &["tag", "v2"]);
    let v1 = git(&dependency, &["rev-parse", "v1^{commit}"]);
    let v2 = git(&dependency, &["rev-parse", "v2^{commit}"]);
    let _checkouts = GitCheckouts::of(&project);

    let url = format!("file:///{}", dependency.display().to_string().replace('\\', "/").trim_start_matches('/'));
    project.write(
        "user/anvil.toml",
        &format!("[project]\nname = \"user\"\n\n[deps]\nitdep = {{ git = \"{url}\", tag = \"v1\" }}\n"),
    );
    project.write(
        "user/src/main.c",
        "#include <stdio.h>\n#include \"itdep/itdep.h\"\nint main(void) { printf(\"dep %d\\n\", itdep_value()); return 0; }\n",
    );

    let run = project.anvil_in("user", &["run"]);
    run.success();
    assert_eq!(program_output(&run), "dep 1");
    let lock = project.read("user/anvil.lock");
    assert!(lock.contains(&v1), "anvil.lock does not pin v1 ({v1}):\n{lock}");

    // Another tag under --locked: refused, the lock file untouched
    project.edit("user/anvil.toml", "tag = \"v1\"", "tag = \"v2\"");
    project.anvil_in("user", &["build", "--locked"]).code(1).contains("--locked");
    assert_eq!(project.read("user/anvil.lock"), lock);

    let moved = project.anvil_in("user", &["run"]);
    moved.success();
    assert_eq!(program_output(&moved), "dep 2");
    assert!(project.read("user/anvil.lock").contains(&v2));
}

// 16. A command line longer than Windows allows cl goes through a response file

fn long_command_line(toolchain: &str, test: &str) {
    let project = Project::fixture("longdefines", test);
    let defines: Vec<String> = (0..400)
        .map(|index| format!("    \"LONG_DEFINE_{index:04}_PADDING_PADDING_PADDING={index}\","))
        .collect();
    project.write(
        "anvil.toml",
        &format!(
            "[project]\nname = \"longdefines\"\n\n[target.app]\nkind = \"exe\"\ndefines = [\n{}\n]\n",
            defines.join("\n")
        ),
    );

    let run = project.anvil(&["run", "--toolchain", toolchain]);
    run.success();
    assert_eq!(program_output(&run), "value 399");
}

#[test]
fn long_command_line_uses_response_file_msvc() {
    require!("msvc");
    long_command_line("msvc", "long_command_line_msvc");
}

#[test]
fn long_command_line_uses_response_file_gcc() {
    require!("gcc");
    long_command_line("gcc", "long_command_line_gcc");
}

// 17. gui = true is a WIN32 program entered through WinMain; gui = "main" keeps main()

#[test]
fn gui_program_with_winmain_links_msvc() {
    require!("msvc", "windows");
    let project = Project::fixture("gui_winmain", "gui_winmain");
    project.anvil(&["build", "--toolchain", "msvc"]).success();
}

#[test]
fn gui_program_with_main_links_msvc() {
    require!("msvc", "windows");
    let project = Project::fixture("gui_main", "gui_main");
    project.anvil(&["build", "--toolchain", "msvc"]).success();
}

// 18. A compiler cache: a clean rebuild hits it, a header edit still recompiles, and it stays out of the commands

/// A private sccache server for one test (its own cache folder and port), stopped when the test ends.
struct Sccache {
    folder: PathBuf,
    port: String,
}

impl Sccache {
    /// Starts the server, or None when sccache is not on PATH.
    fn start(project: &Project, port: u16) -> Option<Sccache> {
        let sccache = Sccache {
            folder: project.path("sccache"),
            port: port.to_string(),
        };
        let started = sccache.command(&["--start-server"]).output().ok()?;
        assert!(
            started.status.success(),
            "sccache --start-server: {}",
            String::from_utf8_lossy(&started.stderr)
        );
        Some(sccache)
    }

    /// Runs `anvil -C <root> <args>` with this server and without ANVIL_LAUNCHER, so the manifest's `launcher` counts.
    fn anvil(&self, project: &Project, args: &[&str]) -> Run {
        let mut command = Command::new(common::ANVIL);
        command.arg("-C").arg(&project.root).args(args).current_dir(&project.root);
        command.env("NO_COLOR", "1").env_remove("ANVIL_LAUNCHER");
        command.env("SCCACHE_DIR", &self.folder).env("SCCACHE_SERVER_PORT", &self.port);

        let started = std::time::Instant::now();
        let output = command.output().expect("starting anvil");
        Run {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            stderr: String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n"),
            elapsed: started.elapsed(),
            command: format!("anvil -C {} {}", project.root.display(), args.join(" ")),
        }
    }

    /// The server's cache hits since its statistics were last zeroed.
    fn hits(&self) -> usize {
        let output = self.command(&["--show-stats"]).output().expect("running sccache --show-stats");
        let text = String::from_utf8_lossy(&output.stdout).to_string();
        let line = text
            .lines()
            .find(|line| line.starts_with("Cache hits "))
            .unwrap_or_else(|| panic!("no hits in:\n{text}"));
        line.split_whitespace().last().unwrap().parse().unwrap()
    }

    fn zero_stats(&self) {
        self.command(&["--zero-stats"]).output().expect("running sccache --zero-stats");
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("sccache");
        command.args(args).env("SCCACHE_DIR", &self.folder).env("SCCACHE_SERVER_PORT", &self.port);
        command
    }
}

impl Drop for Sccache {
    fn drop(&mut self) {
        let _ = self.command(&["--stop-server"]).output();
    }
}

fn compiler_cache(toolchain: Option<&str>, test: &str, port: u16) {
    let project = Project::fixture("headers", test);
    project.write("anvil.toml", "[project]\nname = \"headers\"\nlauncher = \"sccache\"\n");
    let Some(sccache) = Sccache::start(&project, port) else {
        eprintln!("skipped: sccache is not on PATH");
        return;
    };
    let anvil = |args: &[&str]| sccache.anvil(&project, &with_toolchain(toolchain, args));

    // The first build fills the cache; after a clean every compile is a hit
    assert_eq!(anvil(&["build", "-v"]).success().contains("sccache").compiled(), 3);
    sccache.anvil(&project, &["clean", "--all"]).success();
    sccache.zero_stats();
    assert_eq!(anvil(&["build"]).success().compiled(), 3);
    assert_eq!(sccache.hits(), 3);
    assert_eq!(program_output(anvil(&["run"]).success()), "4");

    // Headers are still tracked after hits
    project.write("src/a.h", "#define A_VALUE 3\n");
    let edited = anvil(&["build"]);
    assert_eq!(edited.success().compiled(), 2, "{:?}", edited.actions());
    assert_eq!(program_output(anvil(&["run"]).success()), "8");

    // The cache is not part of any command: turning it off rebuilds nothing, and compile_commands.json omits it
    project.write("anvil.toml", "[project]\nname = \"headers\"\n");
    anvil(&["build"]).success().up_to_date();
    assert!(!project.read("out/compile_commands.json").contains("sccache"));
}

#[test]
fn compiler_cache_hits_after_clean() {
    require!("default");
    compiler_cache(None, "compiler_cache", 4361);
}

#[test]
fn compiler_cache_hits_after_clean_gcc() {
    require!("gcc");
    compiler_cache(Some("gcc"), "compiler_cache_gcc", 4362);
}

#[test]
fn missing_compiler_cache_is_an_error() {
    require!("default");
    let project = Project::fixture("headers", "missing_compiler_cache");
    project.write("anvil.toml", "[project]\nname = \"headers\"\nlauncher = \"anvil-no-such-cache\"\n");

    let mut command = Command::new(common::ANVIL);
    command
        .arg("-C")
        .arg(&project.root)
        .arg("build")
        .env("NO_COLOR", "1")
        .env_remove("ANVIL_LAUNCHER");
    let output = command.output().expect("starting anvil");
    let text = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert!(
        text.contains("[project] launcher in anvil.toml: no program `anvil-no-such-cache`"),
        "{text}"
    );

    // Commands that compile nothing do not need it
    for args in [["targets"], ["compdb"]] {
        let listing = Command::new(common::ANVIL)
            .arg("-C")
            .arg(&project.root)
            .args(args)
            .env_remove("ANVIL_LAUNCHER")
            .output()
            .expect("starting anvil");
        assert!(listing.status.success(), "anvil {args:?}: {}", String::from_utf8_lossy(&listing.stderr));
    }

    // ANVIL_LAUNCHER set empty turns it off
    command.env("ANVIL_LAUNCHER", "");
    let output = command.output().expect("starting anvil");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

// 19. A build that runs something writes a Chrome trace of it, failed actions too; --timings sums it up

/// The trace's complete events ("ph": "X"), from the file `--timings` names in its "Trace: <path>" line.
#[track_caller]
fn trace_events(project: &Project, run: &Run) -> Vec<serde_json::Value> {
    let output = run.output();
    let path = output
        .lines()
        .find_map(|line| line.strip_prefix("Trace: "))
        .unwrap_or_else(|| panic!("`{}` named no trace\n{output}", run.command));
    let text = project.read(path.trim());

    let trace: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|error| panic!("{path} is not JSON: {error}\n{text}"));
    let events = trace["traceEvents"]
        .as_array()
        .unwrap_or_else(|| panic!("{path} has no traceEvents\n{text}"));
    events.iter().filter(|event| event["ph"] == "X").cloned().collect()
}

fn build_trace(toolchain: Option<&str>, test: &str) {
    let project = Project::fixture("headers", test);

    // Every action is one event with its description, kind and lane, and the report lists them
    let first = project.anvil(&with_toolchain(toolchain, &["run", "--timings"]));
    first
        .success()
        .contains("Slowest actions (4 of 4):")
        .contains("Per kind: ")
        .contains("compile ")
        .contains(" (3)");
    assert_eq!(program_output(&first), "4");

    let events = trace_events(&project, &first);
    let mut compiled: Vec<&str> = events
        .iter()
        .filter(|event| event["cat"] == "compile")
        .map(|event| event["name"].as_str().unwrap())
        .collect();
    compiled.sort();
    assert_eq!(
        compiled,
        ["Compiling src/main.c", "Compiling src/other.c", "Compiling src/third.c"],
        "{events:?}"
    );
    assert_eq!(events.iter().filter(|event| event["cat"] == "link").count(), 1, "{events:?}");
    for event in &events {
        assert!(event["tid"].as_u64().is_some_and(|lane| lane >= 1), "{event}");
        assert!(event["ts"].is_u64() && event["dur"].is_u64(), "{event}");
    }

    // A build with nothing to do leaves the trace alone and says where it is
    let trace_file = first
        .output()
        .lines()
        .find_map(|line| line.strip_prefix("Trace: "))
        .unwrap()
        .trim()
        .to_string();
    let trace = project.read(&trace_file);
    project
        .anvil(&with_toolchain(toolchain, &["build", "--timings"]))
        .success()
        .up_to_date()
        .contains(&format!("Timings: nothing ran; the last trace: {trace_file}"))
        .lacks("Slowest actions");
    assert_eq!(project.read(&trace_file), trace);

    // A failed build still writes what ran, the failure marked
    project.write("src/other.c", "int other(void) { return B_VALUE }\n");
    let failed = project.anvil(&with_toolchain(toolchain, &["build", "--timings"]));
    failed.code(1).contains("(failed)");
    let events = trace_events(&project, &failed);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["name"], "Compiling src/other.c");
    assert_eq!(events[0]["args"]["failed"], true);
}

#[test]
fn build_writes_trace_and_timings() {
    require!("default");
    build_trace(None, "build_trace");
}

#[test]
fn build_writes_trace_and_timings_gcc() {
    require!("gcc");
    build_trace(Some("gcc"), "build_trace_gcc");
}

// 20. anvil fmt formats the package's files as .clang-format says, past ignored and output folders; anvil tidy reports
// clang-tidy's findings (once for a header two sources include), through a precompiled header, and fixes them

/// Says why a test is skipped when anvil found no `tool` (clang-format, clang-tidy) on this machine.
fn lint_tool_missing(run: &Run, tool: &str) -> bool {
    let missing = run.output().contains(&format!("{tool} not found (looked on"));
    if missing {
        eprintln!("skipped: {tool} not found");
    }
    missing
}

#[test]
fn fmt_checks_then_formats_the_package() {
    let project = Project::fixture("lint", "fmt");
    project.write("out/debug/generated.cpp", "int  generated( ){return 1;}\n");

    let check = project.anvil(&["fmt", "--check"]);
    if lint_tool_missing(&check, "clang-format") {
        return;
    }
    check
        .code(1)
        .contains("src/main.cpp")
        .contains("1 of 4 files are not formatted")
        .lacks("pick.cpp")
        .lacks("ugly.cpp")
        .lacks("generated.cpp");
    assert!(project.read("src/main.cpp").contains("int main(){"), "--check changed a file");

    project.anvil(&["fmt"]).success().contains("Formatted src/main.cpp");
    assert!(project.read("src/main.cpp").contains("int main() { return pick(twice(1)); }"));
    assert_eq!(project.read("third_party/ugly.cpp"), "int  ugly( ){return 1;}\n");
    project.anvil(&["fmt", "--check"]).success().contains("4 files already formatted");

    // Without a .clang-format, nothing is rewritten in clang-format's own style
    std::fs::remove_file(project.path(".clang-format")).unwrap();
    project.edit("src/pick.cpp", "  return 0;", "      return 0;");
    project.anvil(&["fmt"]).code(1).contains("have no .clang-format");
    assert!(project.read("src/pick.cpp").contains("      return 0;"));
}

#[test]
fn fmt_keeps_ignored_files_and_bytes() {
    let project = Project::fixture("lint", "fmt_bytes");
    project.write("gen/.clang-format-ignore", "!keep.cpp\n");
    project.write("gen/keep.cpp", "int  keep( ){return 1;}\n");
    project.write("gen/other.cpp", "int  other( ){return 2;}\n");
    std::fs::write(project.path("src/latin1.cpp"), b"// caf\xe9\r\nint  latin( ){return 3;}\r\n").unwrap();

    let run = project.anvil(&["fmt"]);
    if lint_tool_missing(&run, "clang-format") {
        return;
    }
    run.success().contains("Formatted gen/keep.cpp").contains("Formatted src/latin1.cpp");

    // A file clang-format leaves out (all but keep.cpp up to clang-format 22) is never emptied
    let other = project.read("gen/other.cpp");
    assert!(other.contains("other("), "{other:?}");

    // A CP1252 file keeps its bytes and line endings
    let latin = std::fs::read(project.path("src/latin1.cpp")).unwrap();
    assert_eq!(latin, b"// caf\xe9\r\nint latin() { return 3; }\r\n");
}

fn tidy_reports_and_fixes(toolchain: Option<&str>, test: &str) {
    let project = Project::fixture("lint", test);

    let run = project.anvil(&with_toolchain(toolchain, &["tidy"]));
    if lint_tool_missing(&run, "clang-tidy") {
        return;
    }
    run.code(1)
        .contains("pick.cpp:4:13: warning:")
        .contains("[bugprone-assignment-in-if-condition]")
        .contains("found 2 warnings checking 2 files")
        .lacks("clang-diagnostic-error");
    assert_eq!(run.output().matches("[modernize-use-nullptr]").count(), 1, "{}", run.output());

    // --fix changes the header once, though both sources include it
    project.anvil(&with_toolchain(toolchain, &["tidy", "--fix"])).code(1).contains("Fixed");
    assert!(project.read("include/lint/twice.h").contains("int *unused = nullptr;"));

    project.edit("src/pick.cpp", "value = twice(value)", "value == twice(value)");
    project
        .anvil(&with_toolchain(toolchain, &["tidy", "app"]))
        .success()
        .contains("2 files, nothing found");
}

#[test]
fn tidy_reports_and_fixes_findings() {
    require!("default");
    tidy_reports_and_fixes(None, "tidy");
}

#[test]
fn tidy_reports_and_fixes_findings_gcc() {
    require!("gcc");
    tidy_reports_and_fixes(Some("gcc"), "tidy_gcc");
}

#[test]
fn tidy_runs_the_build_steps_sources_need() {
    require!("default");
    let project = Project::empty("tidy_generated");
    project.write(
        "anvil.toml",
        "[project]\nname = \"gen\"\n\n[[target.gen.run]]\ninputs = [\"gen.h.in\"]\noutput = \"{gen}/gen.h\"\ncommand = \"cmake -E copy {root}/gen.h.in {output}\"\n",
    );
    project.write("gen.h.in", "#define GEN 1\n");
    project.write("src/main.cpp", "#include \"gen.h\"\nint main() { return GEN; }\n");
    project.write(".clang-tidy", "Checks: '-*,bugprone-*'\n");

    let run = project.anvil(&["tidy"]);
    if lint_tool_missing(&run, "clang-tidy") {
        return;
    }
    run.success().contains("1 files, nothing found");
}

// 21. Dependencies: anvil.lock keeps only the pins something declares; dev-deps; anvil remove, init, completions

/// A git repository at `<project>/<folder>` holding a static library `<name>` (and a second one, `<name>_extra`),
/// tagged v1: its file:/// url. The commit names the repository's folder, so two tests never make the same
/// one (and share a checkout in anvil's cache).
fn git_library(project: &Project, folder: &str, name: &str) -> String {
    project.write(
        &format!("{folder}/anvil.toml"),
        &format!(
            "[project]\nname = \"{name}\"\n\n[target.{name}]\nkind = \"lib\"\nsources = [\"src/*.c\"]\n\n\
             [target.{name}_extra]\nkind = \"lib\"\nsources = [\"src/*.c\"]\n"
        ),
    );
    project.write(&format!("{folder}/src/{name}.c"), &format!("int {name}_value(void) {{ return 1; }}\n"));

    let repository = project.path(folder);
    git(&repository, &["init", "--quiet"]);
    git(&repository, &["add", "-A"]);
    git(&repository, &["commit", "--quiet", "-m", &format!("one, in {}", repository.display())]);
    git(&repository, &["tag", "v1"]);

    format!("file:///{}", repository.display().to_string().replace('\\', "/").trim_start_matches('/'))
}

#[test]
fn lock_keeps_only_declared_pins() {
    require!("default", "git");
    let project = Project::empty("lock_declared");
    let alpha = git_library(&project, "alpha", "alpha");
    let beta = git_library(&project, "beta", "beta");
    let _checkouts = GitCheckouts::of(&project);

    // `opt` (a path dependency behind an option) declares `beta`; the project itself `alpha`
    project.write(
        "app/anvil.toml",
        &format!("[project]\nname = \"app\"\n\n[options]\nwithopt = false\n\n[deps]\nalpha = {{ git = \"{alpha}\", tag = \"v1\" }}\nopt = {{ path = \"opt\", if = \"withopt\" }}\n"),
    );
    project.write("app/src/main.c", "int main(void) { return 0; }\n");
    project.write(
        "app/opt/anvil.toml",
        &format!("[project]\nname = \"opt\"\n\n[deps]\nbeta = {{ git = \"{beta}\", tag = \"v1\" }}\n"),
    );
    project.write("app/opt/src/opt.c", "int opt_value(void) { return 1; }\n");

    // beta's pin, made with the option on, stays while opt is off, and --locked accepts the lock either way
    project.anvil_in("app", &["build", "-o", "withopt"]).success();
    assert!(project.read("app/anvil.lock").contains("parent = \"opt\""));
    project.anvil_in("app", &["build"]).success();
    project.anvil_in("app", &["build", "--locked"]).success();
    project.anvil_in("app", &["build", "--locked", "-o", "withopt"]).success();
    assert!(project.read("app/anvil.lock").contains("[git.beta]"));

    // Once opt no longer declares it, it goes
    project.edit("app/opt/anvil.toml", "beta = ", "# beta = ");
    project.anvil_in("app", &["build", "-o", "withopt"]).success().contains("beta: removed");
    assert!(!project.read("app/anvil.lock").contains("beta"));

    // A gated git dependency whose tag changed: --locked notices without fetching it
    project.edit(
        "app/anvil.toml",
        "[deps]\n",
        &format!("[deps]\ngated = {{ git = \"{beta}\", tag = \"v1\", if = \"withopt\" }}\n"),
    );
    project.anvil_in("app", &["build", "-o", "withopt"]).success();
    project.edit("app/anvil.toml", "tag = \"v1\", if", "branch = \"main\", if");
    project
        .anvil_in("app", &["build", "--locked"])
        .code(1)
        .contains("pins git dependency `gated`");
}

#[test]
fn targets_lists_package_targets_of_unfetched_git_dependency() {
    require!("default", "git");
    let project = Project::empty("list_unfetched");
    let alpha = git_library(&project, "alpha", "alpha");
    let _checkouts = GitCheckouts::of(&project);

    project.write(
        "app/anvil.toml",
        &format!("[project]\nname = \"app\"\n\n[deps]\nalpha = {{ git = \"{alpha}\", tag = \"v1\" }}\n\n[target.app]\nkind = \"exe\"\ndeps = [\"alpha/alpha_extra\"]\n"),
    );
    project.write("app/src/main.c", "int alpha_value(void);\nint main(void) { return alpha_value() - 1; }\n");

    // Listing fetches nothing, then a build fetches and links the named target
    project.anvil_in("app", &["targets"]).success().contains("-> alpha").lacks("Fetching");
    project.anvil_in("app", &["run"]).success();
    project.anvil_in("app", &["targets"]).success().contains("-> alpha_extra");
}

#[test]
fn dev_deps_reach_test_support_libraries_only() {
    require!("default");
    let project = Project::empty("dev_deps_support");
    project.write(
        "helper/anvil.toml",
        "[project]\nname = \"helper\"\n\n[target.helper]\nkind = \"lib\"\nsources = [\"*.c\"]\n",
    );
    project.write("helper/helper.c", "int helper_value(void) { return 0; }\n");
    project.write(
        "src/support.c",
        "int helper_value(void);\nint support_value(void) { return helper_value(); }\n",
    );
    project.write("tests/t.c", "int support_value(void);\nint main(void) { return support_value(); }\n");
    project.write(
        "anvil.toml",
        "[project]\nname = \"app\"\n\n[dev-deps]\nhelper = { path = \"helper\" }\n\n\
         [target.support]\nkind = \"lib\"\nsources = [\"src/support.c\"]\ndeps = [\"helper\"]\n\n\
         [target.t]\nkind = \"test\"\nsources = [\"tests/t.c\"]\ndeps = [\"support\"]\n",
    );

    // A library only tests link may use a dev-dep
    project.anvil(&["test"]).success();

    // Not once a program uses it; nor through header-deps
    project.write("src/main.c", "int main(void) { return 0; }\n");
    project.edit(
        "anvil.toml",
        "[target.t]",
        "[target.app]\nkind = \"exe\"\nsources = [\"src/main.c\"]\ndeps = [\"support\"]\n\n[target.t]",
    );
    project
        .anvil(&["build"])
        .code(1)
        .contains("`support` is used by `app` (exe), which is not a test");
    project.edit(
        "anvil.toml",
        "deps = [\"support\"]\n\n[target.t]",
        "header-deps = [\"helper\"]\n\n[target.t]",
    );
    project
        .anvil(&["build"])
        .code(1)
        .contains("target `app` (exe) uses `helper`, which is under [dev-deps]");
}

#[test]
fn remove_takes_every_use_in_the_member_it_runs_in() {
    let project = Project::empty("remove_uses");
    project.write("anvil.toml", "[workspace]\nmembers = [\"app\"]\n\n[deps]\nmylib = \"system\"\n");
    project.write(
        "app/anvil.toml",
        "[project]\nname = \"app\"\n\n[deps]\nmylib = { path = \"lib\" }\nzlib = \"system\"\n\n\
         [target.app]\nkind = \"exe\"\ndeps = [\n    \"mylib/core\", # the core\n    \"zlib\",\n]\nheader-deps = [\"mylib\"]\n\n\
         [target.app.when.windows]\ndeps = [\"mylib\"]\n",
    );

    project.anvil_in("app", &["remove", "mylib"]).success();
    assert_eq!(
        project.read("app/anvil.toml"),
        "[project]\nname = \"app\"\n\n[deps]\nzlib = \"system\"\n\n[target.app]\nkind = \"exe\"\ndeps = [\n    \"zlib\",\n]\n"
    );
    assert!(project.read("anvil.toml").contains("mylib = \"system\""), "the workspace root was edited");

    // `anvil add` in the member writes there too
    project.anvil_in("app", &["add", "user32", "--system"]).success();
    assert!(project.read("app/anvil.toml").contains("user32 = \"system\""));
}

#[test]
fn init_makes_a_program_per_folder_with_a_main() {
    require!("default");
    let project = Project::empty("init_programs");
    project.write("server/main.c", "int shared_value(void);\nint main(void) { return shared_value(); }\n");
    project.write("client/main.c", "int shared_value(void);\nint main(void) { return shared_value(); }\n");
    project.write("common/shared.c", "#include \"shared.h\"\nint shared_value(void) { return 0; }\n");
    project.write("include/shared.h", "#pragma once\n");

    project.anvil(&["init"]).success().contains("exe `server`").contains("exe `client`");
    let manifest = project.read("anvil.toml");
    assert!(manifest.contains("public = { include = [\"common\", \"include\"] }"), "{manifest}");
    project.anvil(&["build"]).success();

    // A folder with two main files cannot be split
    let mixed = Project::empty("init_two_mains");
    mixed.write("tools/a/main.c", "int main(void) { return 0; }\n");
    mixed.write("tools/b/main.c", "int main(void) { return 0; }\n");
    mixed.anvil(&["init"]).code(1).contains("several main files");
}

#[test]
fn vcpkg_triplet_must_use_the_profile_runtime() {
    require!("msvc", "windows");
    let project = Project::empty("triplet_runtime");
    project.write("src/main.c", "int main(void) { return 0; }\n");
    project.write(
        "anvil.toml",
        "[project]\nname = \"app\"\n\n[vcpkg]\ntriplet = \"x64-windows-static\"\n\n[deps]\nfmt = \"vcpkg\"\n",
    );

    project
        .anvil(&["targets", "--toolchain", "msvc"])
        .code(1)
        .contains("static C runtime (/MT)")
        .contains("static-runtime = true");
    project.edit("anvil.toml", "[deps]", "[profile.debug]\nstatic-runtime = true\n\n[deps]");
    project.anvil(&["targets", "--toolchain", "msvc"]).success();
}

#[test]
fn vcpkg_interface_options_reach_users() {
    require!("msvc", "windows", "vcpkg");
    let project = Project::empty("vcpkg_options");
    project.write("src/main.cpp", "int main() { return 0; }\n");
    project.write(
        "anvil.toml",
        "[project]\nname = \"app\"\n\n[vcpkg]\ntriplet = \"x64-windows-static-md\"\n\n[deps]\nglaze = \"vcpkg\"\n\n\
         [target.app]\nkind = \"exe\"\nsources = [\"src/main.cpp\"]\ndeps = [\"glaze\"]\n",
    );

    // glaze's INTERFACE_COMPILE_OPTIONS, and in release its $<$<CONFIG:Release>:...> compile and link options
    project.anvil(&["build", "--toolchain", "msvc"]).success();
    let commands = project.read("out/compile_commands.json");
    assert!(commands.contains("\"/Zc:preprocessor\"") && !commands.contains("\"/GL\""), "{commands}");
    project
        .anvil(&["build", "--release", "--toolchain", "msvc", "-v"])
        .success()
        .contains("/LTCG");
    assert!(project.read("out/compile_commands.json").contains("\"/GL\""));
}

#[test]
fn completions_leave_out_hidden_commands() {
    let project = Project::empty("completions");
    let run = project.anvil(&["completions", "bash"]);
    run.success().contains("remove").lacks("cmake-e");
}

// 22. A test's background server outlives it for later tests; a process holding a test's output open does not keep
// it waiting; arguments after `--` go to programs of the project only

#[test]
fn test_processes_outlive_tests_and_arguments_reach_programs() {
    require!("default");
    let project = Project::fixture("processes", "processes");

    project.anvil(&["test", "server_"]).success().contains("test server_use ... ok");

    let holder = project.anvil(&["test", "holder", "--show-output"]);
    holder
        .success()
        .contains("parent done")
        .contains("[output cut: a process the test started still holds its output open]");
    assert!(
        holder.elapsed < Duration::from_secs(4),
        "the test waited for its child: {:?}",
        holder.elapsed
    );

    project.anvil(&["test", "args_", "--", "extra"]).success().contains("Passed 2 of 2");
}

// 23. Libraries in link-flags link after the objects, as GNU ld needs them

#[test]
fn link_flags_libraries_follow_the_objects_gcc() {
    require!("gcc");
    let project = Project::empty("link_order_gcc");
    project.write(
        "anvil.toml",
        "[project]\nname = \"order\"\n\n[target.helper]\nkind = \"lib\"\nsources = [\"helper/*.c\"]\n\n\
         [target.app]\nkind = \"exe\"\nsources = [\"src/*.c\"]\nlink-flags = [\"{target:helper}\"]\n",
    );
    project.write("helper/helper.c", "int helper_value(void) { return 42; }\n");
    project.write(
        "src/main.c",
        "#include <stdio.h>\nint helper_value(void);\nint main(void) { printf(\"%d\\n\", helper_value()); return 0; }\n",
    );

    let run = project.anvil(&["run", "--toolchain", "gcc"]);
    run.success();
    assert_eq!(program_output(&run), "42");
}

// 24. Tests of the same name in two packages get their package's name; in one package the name is an error

#[test]
fn test_names_are_qualified_by_package() {
    require!("default");
    let project = Project::fixture("workspace", "test_names");
    let smoke = "[[test]]\nname = \"smoke\"\ncommand = [\"cmake\", \"-E\", \"true\"]\n";
    let after = "[[test]]\nname = \"later\"\ncommand = [\"cmake\", \"-E\", \"true\"]\nafter = [\"smoke\"]\n";
    project.write("core/anvil.toml", &format!("{}\n{smoke}\n{after}", project.read("core/anvil.toml")));
    project.write("app/anvil.toml", &format!("{}\n{smoke}", project.read("app/anvil.toml")));

    let list = project.anvil(&["test", "--list"]);
    list.success().contains("core/smoke").contains("app/smoke").contains("later");
    project.anvil(&["test"]).success().contains("Passed 3 of 3");

    project.write("app/one/tests.toml", smoke);
    project.write("app/two/tests.toml", smoke);
    project
        .anvil(&["test", "--list"])
        .code(1)
        .contains("two tests are named `smoke`, in app/anvil.toml and in app/one/tests.toml");
}

// 25. anvil install lays out programs (with their data), libraries, headers and the CMake and pkg-config files, copies
//     only what changed, and installed programs run from the prefix; a CMake project builds against the install

/// The files under a folder, relative to it, sorted.
fn files_under(folder: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut pending = vec![folder.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in std::fs::read_dir(&current).unwrap().map(|entry| entry.unwrap()) {
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else {
                let relative = entry.path().strip_prefix(folder).unwrap().to_string_lossy().replace('\\', "/");
                files.push(relative);
            }
        }
    }
    files.sort();
    files
}

/// Runs an installed program, with the prefix's bin/ first on PATH (where Windows finds DLLs).
fn run_installed(prefix: &Path, program: &Path) -> String {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut folders = vec![prefix.join("bin")];
    folders.extend(std::env::split_paths(&path));
    let output = Command::new(program)
        .env("PATH", std::env::join_paths(folders).unwrap())
        .output()
        .unwrap_or_else(|error| panic!("starting {}: {error}", program.display()));
    assert!(
        output.status.success(),
        "{} failed: {}",
        program.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn install_layout(toolchain: Option<&str>, test: &str) {
    let project = Project::fixture("install", test);
    let exe = if cfg!(windows) { ".exe" } else { "" };

    let first = project.anvil(&with_toolchain(toolchain, &["install"]));
    first.success().contains("Installed out/install:");
    let files = files_under(&project.path("out/install"));
    for expected in [
        format!("bin/tool{exe}"),
        "bin/assets/logo.png".to_string(),
        "bin/assets/maps/one.txt".to_string(),
        "include/geo/core.h".to_string(),
        "include/geo/core_export.h".to_string(),
        "include/geo/util.h".to_string(),
        "lib/cmake/geo/geoConfig.cmake".to_string(),
        "lib/cmake/geo/geoConfigVersion.cmake".to_string(),
        "lib/pkgconfig/geo.pc".to_string(),
    ] {
        assert!(files.contains(&expected), "no {expected} in {files:?}");
    }
    assert_eq!(
        files.iter().filter(|file| file.starts_with("lib/") && file.contains("core")).count(),
        1,
        "{files:?}"
    );
    assert!(!files.iter().any(|file| file.contains("check")), "a test was installed: {files:?}");

    // The installed program finds its data beside it
    let program = project.path(&format!("out/install/bin/tool{exe}"));
    assert_eq!(run_installed(&project.path("out/install"), &program), "area 12 map hills");

    // The package names the static library and what its users link
    let config = project.read("out/install/lib/cmake/geo/geoConfig.cmake");
    assert!(config.contains("add_library(geo::core STATIC IMPORTED)"), "{config}");
    assert!(config.contains("INTERFACE_COMPILE_DEFINITIONS \"GEO_UNITS=1\""), "{config}");
    assert!(config.contains("$<LINK_ONLY:geo::util>"), "{config}");
    assert!(project.read("out/install/lib/pkgconfig/geo.pc").contains("Version: 1.2.0"));

    // Nothing changed: nothing copied
    project
        .anvil(&with_toolchain(toolchain, &["install"]))
        .success()
        .contains("0 files copied");

    // Shared: the DLL beside the program on Windows, the library in lib/ elsewhere, and its definition for users
    let shared = project.anvil(&with_toolchain(toolchain, &["install", "-o", "SHARED", "--prefix", "out/shared"]));
    shared.success();
    let files = files_under(&project.path("out/shared"));
    let library_folder = if cfg!(windows) { "bin/" } else { "lib/" };
    assert!(
        files.iter().any(|file| file.starts_with(library_folder) && file.contains("core.")),
        "no shared core in {library_folder}: {files:?}"
    );
    let program = project.path(&format!("out/shared/bin/tool{exe}"));
    assert_eq!(run_installed(&project.path("out/shared"), &program), "area 12 map hills");
    let config = project.read("out/shared/lib/cmake/geo/geoConfig.cmake");
    assert!(config.contains("add_library(geo::core SHARED IMPORTED)"), "{config}");
    assert!(config.contains("CORE_SHARED"), "{config}");
}

#[test]
fn install_lays_out_programs_libraries_and_headers() {
    require!("default");
    install_layout(None, "install");
}

#[test]
fn install_lays_out_programs_libraries_and_headers_gcc() {
    require!("gcc");
    install_layout(Some("gcc"), "install_gcc");
}

/// Whether a program runs from PATH (`cmake --version`).
fn on_path(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

#[test]
fn cmake_project_uses_an_installed_library() {
    require!("gcc");
    if !on_path("cmake") || !on_path("ninja") {
        eprintln!("skipped: cmake and ninja must be on PATH");
        return;
    }

    let project = Project::fixture("install", "install_cmake");
    let consumer = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("install_consumer");
    for (kind, options) in [("static", vec![]), ("shared", vec!["-o", "SHARED"])] {
        let prefix_name = format!("out/{kind}");
        let mut args = vec!["install", "--toolchain", "gcc", "--prefix", prefix_name.as_str()];
        args.extend(options);
        project.anvil(&args).success();

        // Configure and build the consumer with the same compiler, against the prefix
        let prefix = project.path(&prefix_name);
        let build = project.path(&format!("consumer-{kind}"));
        let cmake = |args: &[&str]| {
            let output = Command::new("cmake").args(args).output().expect("running cmake");
            assert!(
                output.status.success(),
                "cmake {args:?} failed:\n{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        };
        cmake(&[
            "-S",
            &consumer.to_string_lossy(),
            "-B",
            &build.to_string_lossy(),
            "-G",
            "Ninja",
            "-DCMAKE_BUILD_TYPE=Debug",
            "-DCMAKE_CXX_COMPILER=g++",
            &format!("-DCMAKE_PREFIX_PATH={}", prefix.to_string_lossy().replace('\\', "/")),
        ]);
        cmake(&["--build", &build.to_string_lossy()]);

        let app = build.join(if cfg!(windows) { "app.exe" } else { "app" });
        assert_eq!(run_installed(&prefix, &app), "consumer 10", "{kind}");
    }
}

// 26. data files go beside the program at their path under the target's folder, and are copied again only when they
//     change; a pattern that matches nothing is an error

#[test]
fn data_files_copy_beside_the_program_when_changed() {
    require!("default");
    let project = Project::fixture("install", "data_files");

    let run = project.anvil(&["run", "tool"]);
    run.success();
    assert_eq!(program_output(&run), "area 12 map hills");
    assert!(run.actions().contains(&"Copying assets/maps/one.txt".to_string()), "{:?}", run.actions());
    assert_eq!(project.read("out/debug/assets/logo.png").trim(), "logo");
    project.anvil(&["build"]).success().up_to_date();

    // An edited file is copied again, alone; a touched one is not
    project.write("tool/assets/maps/one.txt", "valleys\n");
    let edited = project.anvil(&["build"]);
    edited.success();
    assert_eq!(edited.actions(), ["Copying assets/maps/one.txt"]);
    project.touch("tool/assets/logo.png");
    project.anvil(&["build"]).success().up_to_date();
    assert_eq!(program_output(&project.anvil(&["run", "tool"])), "area 12 map valleys");

    project.edit("anvil.toml", "data = [\"assets/**\"]", "data = [\"assets/**\", \"sounds/*.wav\"]");
    project
        .anvil(&["build"])
        .code(1)
        .contains("data: \"sounds/*.wav\" matches no files (in tool)");
}

// 27. anvil schema prints the JSON Schemas of anvil.toml and tests.toml; --install writes them to the data folder

#[test]
fn schema_prints_and_installs() {
    let project = Project::empty("schema");

    let run = project.anvil(&["schema"]);
    run.success();
    let schema: serde_json::Value = serde_json::from_str(&run.stdout).expect("anvil schema prints JSON");
    assert_eq!(schema["$schema"], "http://json-schema.org/draft-07/schema#");
    assert_eq!(schema["properties"]["target"]["additionalProperties"]["$ref"], "#/definitions/target");

    let run = project.anvil(&["schema", "--tests"]);
    run.success();
    let tests: serde_json::Value = serde_json::from_str(&run.stdout).expect("anvil schema --tests prints JSON");
    let keys: Vec<&String> = tests["properties"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["test"]);

    // --install, into a data folder of the test's own
    let data = project.path("data");
    let output = Command::new(common::ANVIL)
        .args(["schema", "--install"])
        .env("LOCALAPPDATA", &data)
        .env("XDG_CACHE_HOME", &data)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("evenBetterToml.schema.associations"));
    let installed = std::fs::read_to_string(data.join("anvil/anvil.schema.json")).expect("anvil.schema.json written");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&installed).unwrap(), schema);
    assert!(data.join("anvil/anvil-tests.schema.json").is_file());
}

// 28. Build steps that rewrite files settle: a step touching a file it names, a step reading another step's output
//     and a post-build step touching the program all leave the next build with nothing to do

const REWRITE_STEPS: &str = r#"[project]
name = "rewrite"

[target.app]
kind = "exe"

[[target.app.run]]
output = "data/table.txt"
command = ["cmake", "-E", "copy", "data/source.txt", "data/table.txt"]

[[target.app.run]]
output = "{gen}/table.h"
command = ["cmake", "-E", "copy", "data/table.txt", "{output}"]

[[target.app.run]]
output = "{gen}/lang.qm"
command = ["cmake", "-E", "touch", "i18n/lang.ts", "{output}"]

[[target.app.post-build]]
command = ["cmake", "-E", "touch", "{out}/app.exe"]
"#;

#[test]
fn build_steps_that_rewrite_files_settle() {
    require!("default");
    let project = Project::empty("rewrite_steps");
    project.write(
        "anvil.toml",
        &REWRITE_STEPS.replace("app.exe", &format!("app{}", std::env::consts::EXE_SUFFIX)),
    );
    project.write("src/main.c", "int main(void) { return 0; }\n");
    project.write("data/source.txt", "one\n");
    project.write("i18n/lang.ts", "<TS/>\n");

    project.anvil(&["build"]).success();
    project.anvil(&["build", "--explain"]).success().up_to_date();

    // Only the post-build step changed: the program is not linked again, and then all is done
    project.edit("anvil.toml", "\"touch\", \"{out}", "\"touch\", \"{gen}/signed.txt\", \"{out}");
    let changed = project.anvil(&["build", "--explain"]);
    changed.success();
    assert_eq!(changed.linked(), 0, "{}", changed.output());
    assert_eq!(changed.actions().len(), 1, "{:?}", changed.actions());
    project.anvil(&["build", "--explain"]).success().up_to_date();

    // A step reading another step's output runs after it, and again when it changes
    project.write("data/source.txt", "two\n");
    let edited = project.anvil(&["build", "--explain"]);
    edited.success();
    assert_eq!(project.read("out/debug/gen/app/table.h"), "two\n");
    project.anvil(&["build"]).success().up_to_date();
}

// 29. A header with a time stamp in the future (another machine's clock) does not rebuild every time, and a damaged
//     build database costs one rebuild, not one every build

#[test]
fn future_time_stamps_and_damaged_database_settle() {
    require!("default");
    let project = Project::empty("future_stamps");
    project.write("anvil.toml", "[project]\nname = \"future\"\n\n[target.app]\nkind = \"exe\"\n");
    project.write("src/main.c", "#include \"a.h\"\nint main(void) { return A; }\n");
    project.write("src/a.h", "#define A 0\n");
    let header = std::fs::File::options().write(true).open(project.path("src/a.h")).unwrap();
    header.set_modified(std::time::SystemTime::now() + Duration::from_secs(86_400)).unwrap();
    drop(header);

    project.anvil(&["build"]).success();
    project.anvil(&["build", "--explain"]).success().up_to_date();

    project.write("out/debug/.anvil/db", "ANVILDB3 damaged");
    project.anvil(&["build"]).success().contains("build database unreadable");
    project.anvil(&["build", "--explain"]).success().up_to_date();
}

// 30. Shell steps in a folder whose path has a space and an `&`: values are quoted, redirections still work, and a
//     step that does not write an output fails

#[test]
fn shell_steps_quote_paths_and_check_outputs() {
    require!("default");
    let project = Project::empty("shell_quoting");
    let manifest = r#"[project]
name = "quoting"

[target.app]
kind = "exe"

[[target.app.run]]
output = "{gen}/a.txt"
shell = true
command = "echo hello>{output}"

[[target.app.run]]
output = "{gen}/b.txt"
shell = true
command = ["cmake", "-E", "copy", "{root}/in.txt", "{output}"]
"#;
    project.write("a&b dir/anvil.toml", manifest);
    project.write("a&b dir/src/main.c", "int main(void) { return 0; }\n");
    project.write("a&b dir/in.txt", "in\n");

    project.anvil_in("a&b dir", &["build"]).success();
    assert_eq!(project.read("a&b dir/out/debug/gen/app/a.txt").trim(), "hello");
    assert_eq!(project.read("a&b dir/out/debug/gen/app/b.txt"), "in\n");
    project.anvil_in("a&b dir", &["build"]).success().up_to_date();

    let missing = "\n[[target.app.run]]\noutputs = [\"{gen}/c.txt\", \"{gen}/d.txt\"]\ncommand = [\"cmake\", \"-E\", \"touch\", \"{output}\"]\n";
    project.write("a&b dir/anvil.toml", &format!("{manifest}{missing}"));
    project
        .anvil_in("a&b dir", &["build"])
        .code(1)
        .contains("the step succeeded but did not write out/debug/gen/app/d.txt");
}

// 31. The project model: what tests and the command line name in a dependency stays; `{target:X}` in a member means
//     its own X; dependencies' [[write]] files never clash; options of dependencies that are off; a member's broken
//     workspace; a test-only library is not installed; a renamed conventional test

#[test]
fn dependency_targets_named_by_tests_and_the_command_line_stay() {
    require!("default");
    let project = Project::empty("model_references");
    project.write(
        "anvil.toml",
        "[project]\nname = \"app\"\n\n[deps]\ndep = { path = \"dep\" }\n\n\
         [target.app]\nkind = \"exe\"\nsources = [\"src/main.c\"]\ndeps = [\"dep\"]\n\n\
         [target.t]\nkind = \"test\"\nsources = [\"tests/t.c\"]\ndeps = [\"dep/z_support\"]\n\n\
         [[test]]\nname = \"check\"\ncommand = [\"{target:dep/checker}\"]\nexpect = [\"checked\"]\n",
    );
    project.write("src/main.c", "int dep_value(void);\nint main(void) { return dep_value(); }\n");
    project.write("tests/t.c", "int support_value(void);\nint main(void) { return support_value(); }\n");

    // The dependency's test-support library uses its dev-deps, after targets nothing reaches
    project.write(
        "dep/anvil.toml",
        "[project]\nname = \"dep\"\n\n[dev-deps]\nhdr = { path = \"../hdr\" }\n\n\
         [target.a_unused]\nkind = \"exe\"\nsources = [\"unused.c\"]\n\n\
         [target.z_support]\nkind = \"lib\"\nsources = [\"support.c\"]\ndeps = [\"hdr\"]\n\n\
         [target.dep]\nkind = \"lib\"\nsources = [\"dep.c\"]\n\n\
         [target.checker]\nkind = \"exe\"\nsources = [\"checker.c\"]\n\n\
         [target.a_unused2]\nkind = \"exe\"\nsources = [\"unused.c\"]\noutput-name = \"unused2\"\n",
    );
    project.write("dep/unused.c", "int main(void) { return 0; }\n");
    project.write("dep/support.c", "#include \"hdr.h\"\nint support_value(void) { return HDR_ZERO; }\n");
    project.write("dep/dep.c", "int dep_value(void) { return 0; }\n");
    project.write(
        "dep/checker.c",
        "#include <stdio.h>\nint main(void) { printf(\"checked\\n\"); return 0; }\n",
    );
    project.write("hdr/include/hdr.h", "#define HDR_ZERO 0\n");

    project.anvil(&["test"]).success().contains("test check ... ok").contains("test t ... ok");
    let run = project.anvil(&["run", "dep/checker"]);
    run.success();
    assert_eq!(program_output(&run), "checked");
    project.anvil(&["build", "dep/a_unused"]).success();
}

#[test]
fn member_target_references_mean_its_own_targets() {
    require!("default");
    let project = Project::empty("model_same_names");
    let unit = |text: &str| format!("#include <stdio.h>\nint main(void) {{ printf(\"{text}\\n\"); return 0; }}\n");
    project.write(
        "anvil.toml",
        "[project]\nname = \"root\"\n\n[workspace]\nmembers = [\"b\"]\n\n\
         [target.unit]\nkind = \"test\"\nsources = [\"unit.c\"]\noutput-name = \"root_unit\"\nexpect = [\"ROOT UNIT\"]\n",
    );
    project.write("unit.c", &unit("ROOT UNIT"));
    project.write(
        "b/anvil.toml",
        "[project]\nname = \"b\"\n\n\
         [target.unit]\nkind = \"test\"\nsources = [\"unit.c\"]\noutput-name = \"b_unit\"\nexpect = [\"B UNIT\"]\n\n\
         [[test]]\nname = \"again\"\ncommand = [\"{target:unit}\"]\nexpect = [\"B UNIT\"]\n",
    );
    project.write("b/unit.c", &unit("B UNIT"));

    project
        .anvil(&["test"])
        .success()
        .contains("test b/unit ... ok")
        .contains("Passed 3 of 3");
}

#[test]
fn written_files_never_clash_nor_land_in_git_checkouts() {
    require!("default");
    let project = Project::empty("model_writes");
    project.write(
        "anvil.toml",
        "[project]\nname = \"app\"\n\n[deps]\na = { path = \"a\" }\nb = { path = \"b\" }\n\n\
         [target.app]\nkind = \"exe\"\nsources = [\"src/main.c\"]\ndeps = [\"a\", \"b\"]\n",
    );
    project.write("src/main.c", "int main(void) { return 0; }\n");
    for name in ["a", "b"] {
        project.write(
            &format!("{name}/anvil.toml"),
            &format!(
                "[project]\nname = \"{name}\"\n\n[[write]]\npath = \"{{out}}/config.h\"\ntext = \"#define VALUE 1\\n\"\n\n\
                 [target.{name}]\nkind = \"lib\"\nsources = [\"{name}.c\"]\n"
            ),
        );
        project.write(&format!("{name}/{name}.c"), &format!("int {name}_value(void) {{ return 0; }}\n"));
    }

    // The same text twice is one file; different text is an error naming both packages
    project.anvil(&["targets"]).success();
    project.edit("b/anvil.toml", "VALUE 1", "VALUE 2");
    project
        .anvil(&["targets"])
        .code(1)
        .contains("both [[write]] out/debug/config.h, with different text");

    // Nothing writes into anvil's git cache, which other projects share (refused before anything is written)
    let Some(cache) = git_cache() else { return };
    let checkout = cache.join("anvil-it-model-writes");
    let inside = checkout.join("config.h").display().to_string().replace('\\', "/");
    project.edit("b/anvil.toml", "{out}/config.h", &inside);
    let refused = project.anvil(&["targets"]);
    let _ = std::fs::remove_dir_all(&checkout);
    refused.code(1).contains("inside a git checkout Anvil's cache shares between projects");
}

#[test]
fn options_of_dependencies_that_are_off_are_known() {
    require!("default");
    let project = Project::empty("model_options");
    project.write(
        "anvil.toml",
        "[project]\nname = \"app\"\n\n[options]\nwithx = false\n\n[deps]\nx = { path = \"x\", if = \"withx\" }\n",
    );
    project.write("src/main.c", "int main(void) { return 0; }\n");
    project.write(
        "x/anvil.toml",
        "[project]\nname = \"x\"\n\n[options]\nx_fast = false\n\n[target.x]\nkind = \"lib\"\nsources = [\"x.c\"]\n",
    );
    project.write("x/x.c", "int x_value(void) { return 0; }\n");

    project.anvil(&["targets", "-o", "x_fast"]).success();
    project
        .anvil(&["targets", "-o", "x_fastt"])
        .code(1)
        .contains("unknown option `x_fastt`. Did you mean `x_fast`?");
}

#[test]
fn member_reports_its_workspace_errors() {
    require!("default");
    let project = Project::empty("model_workspace");
    project.write(
        "anvil.toml",
        "[project]\nname = \"root\"\n\n[workspace]\nmembers = [\"libs/*\", \"tools/*\"]\n\n[options]\nshared_opt = true\n",
    );
    project.write(
        "libs/a/anvil.toml",
        "[project]\nname = \"a\"\n\n[target.a]\nkind = \"lib\"\nsources = [\"src/*.c\"]\nwhen.shared_opt.defines = [\"X\"]\n",
    );
    project.write("libs/a/src/a.c", "int a_value(void) { return 0; }\n");

    // The workspace's error, not one about the member alone
    project
        .anvil_in("libs/a", &["targets"])
        .code(1)
        .contains("in the workspace")
        .contains("workspace member \"tools/*\" matches no folder");

    // A manifest above that does not load: a warning, and the member alone
    project.write("anvil.toml", "[project]\nname = \"root\"\nnot_a_key = 1\n");
    project.edit("libs/a/anvil.toml", "when.shared_opt.", "");
    project
        .anvil_in("libs/a", &["targets"])
        .success()
        .contains("warning: not checking whether")
        .contains("is a workspace holding this package");
}

#[test]
fn install_leaves_out_libraries_only_tests_use() {
    require!("default");
    let project = Project::empty("model_install");
    project.write(
        "anvil.toml",
        "[project]\nname = \"app\"\n\n[dev-deps]\nhelper = { path = \"helper\" }\n\n\
         [target.app]\nkind = \"exe\"\nsources = [\"src/main.c\"]\n\n\
         [target.support]\nkind = \"lib\"\nsources = [\"support/support.c\"]\ndeps = [\"helper\"]\n\n\
         [target.t]\nkind = \"test\"\nsources = [\"tests/t.c\"]\ndeps = [\"support\"]\n",
    );
    project.write("src/main.c", "int main(void) { return 0; }\n");
    project.write(
        "support/support.c",
        "int helper_value(void);\nint support_value(void) { return helper_value(); }\n",
    );
    project.write("tests/t.c", "int support_value(void);\nint main(void) { return support_value(); }\n");
    project.write(
        "helper/anvil.toml",
        "[project]\nname = \"helper\"\n\n[target.helper]\nkind = \"lib\"\nsources = [\"*.c\"]\n",
    );
    project.write("helper/helper.c", "int helper_value(void) { return 0; }\n");

    project.anvil(&["install"]).success();
    let files = files_under(&project.path("out/install"));
    assert!(
        !files.iter().any(|file| file.contains("support") || file.contains("helper")),
        "a test-only library was installed: {files:?}"
    );
}

#[test]
fn conventional_test_named_like_another_is_an_error() {
    require!("default");
    let project = Project::empty("model_convention");
    project.write("anvil.toml", "[project]\nname = \"foo\"\n");
    project.write("src/main.c", "int main(void) { return 0; }\n");
    project.write("tests/foo.c", "int main(void) { return 0; }\n");
    project.write("tests/test_foo.c", "int main(void) { return 0; }\n");

    project
        .anvil(&["targets"])
        .code(1)
        .contains("tests/foo.c and tests/test_foo.c would both be the test `test_foo`")
        .contains("for example tests/foo.c to tests/foo_tests.c");
    std::fs::rename(project.path("tests/foo.c"), project.path("tests/foo_tests.c")).unwrap();
    project.anvil(&["test"]).success().contains("Passed 2 of 2");
}

// 32. OpenMP and threads as built-in system dependencies: the program runs its parallel region on every toolchain

fn openmp_program(toolchain: Option<&str>, test: &str) {
    let project = Project::empty(test);
    project.write(
        "anvil.toml",
        "[project]\nname = \"omp\"\n\n[deps]\nopenmp = \"system\"\nthreads = \"system\"\n",
    );
    project.write(
        "src/main.cpp",
        "#include <omp.h>\n#include <cstdio>\n#include <thread>\nint main() {\n    int count = 0;\n#pragma omp parallel\n    {\n#pragma omp atomic\n        count++;\n    }\n    std::thread([] {}).join();\n    std::printf(\"%d\\n\", count == omp_get_max_threads());\n}\n",
    );

    let run = project.anvil(&with_toolchain(toolchain, &["run"]));
    run.success();
    assert_eq!(program_output(&run), "1");
}

#[test]
fn openmp_system_dependency_runs() {
    require!("default");
    openmp_program(None, "openmp");
}

#[test]
fn openmp_system_dependency_runs_gcc() {
    require!("gcc");
    openmp_program(Some("gcc"), "openmp_gcc");
}

#[test]
fn openmp_system_dependency_runs_clang() {
    require!("clang");
    openmp_program(Some("clang"), "openmp_clang");
}

#[test]
fn openmp_system_dependency_runs_clang_cl() {
    require!("clang-cl");
    openmp_program(Some("clang-cl"), "openmp_clang_cl");
}

// 33. A port's language-specific definitions reach only that language's files; $<CONFIG:Debug> holds exactly when
//     its debug libraries are linked; its OpenMP::OpenMP_CXX gives C++ files OpenMP

fn vcpkg_port_usage(toolchain: Option<&str>, test: &str, debug_libraries: bool) {
    let project = Project::fixture("vcpkg_overlay", test);
    let debug = if debug_libraries { " OPT_DEBUG" } else { "" };

    // The program's line comes last (vcpkg's install prints on stdout too)
    let run = project.anvil(&with_toolchain(toolchain, &["run"]));
    run.success();
    assert_eq!(
        program_output(&run).lines().last(),
        Some(format!("c=1 OPT_CXX_DEF{debug} openmp").as_str())
    );

    let run = project.anvil(&with_toolchain(toolchain, &["run", "--release"]));
    run.success();
    assert_eq!(program_output(&run).lines().last(), Some("c=1 OPT_CXX_DEF openmp"));
}

#[test]
fn vcpkg_port_usage_follows_language_and_libraries() {
    require!("msvc", "vcpkg");
    vcpkg_port_usage(Some("msvc"), "vcpkg_usage", true);
}

#[test]
fn vcpkg_port_usage_follows_language_and_libraries_clang() {
    require!("clang", "windows", "vcpkg");
    vcpkg_port_usage(Some("clang"), "vcpkg_usage_clang", true);
}

#[test]
fn vcpkg_port_usage_follows_language_and_libraries_gcc() {
    require!("gcc", "vcpkg");
    vcpkg_port_usage(Some("gcc"), "vcpkg_usage_gcc", false);
}
