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

// 14b. Platform files kept out by HEADER_FILE_ONLY under a condition, a library's per-language definitions and
// CMAKE_MSVC_RUNTIME_LIBRARY convert to a project that builds with each compiler and compiles as CMake does

#[test]
fn convert_platform_files_languages_and_runtime() {
    require!("msvc", "gcc");
    let project = Project::fixture("convert_platform", "convert_platform");
    project.anvil(&["convert"]).success();
    let converted = project.read("anvil.toml");
    assert!(converted.contains("[profile.debug]\nstatic-runtime = true"), "{converted}");

    for toolchain in ["msvc", "gcc"] {
        let run = project.anvil(&["run", "app", "--toolchain", toolchain]);
        run.success().contains("windows 42");
    }

    // CMake's compile commands, configured again for the other compiler
    let runs = |program: &str| {
        Command::new(program)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    };
    if !runs("cmake") || !runs("ninja") {
        eprintln!("skipped --check: cmake or ninja is not on PATH");
        return;
    }
    for toolchain in ["msvc", "gcc"] {
        let check = project.anvil(&["convert", "--check", "--toolchain", toolchain]);
        check.success().contains("3 files compiled by both: 3 the same");
    }
}

// 14c. Each branch of a converted CMake project keeps what it adds under its condition, this machine's too

#[test]
fn convert_keeps_every_branch_under_its_condition() {
    require!("msvc", "gcc");
    let project = Project::empty("convert_branches");
    let cmake = r#"cmake_minimum_required(VERSION 3.20)
project(branches C)
function(add_compiler_source)
  if(NOT MSVC)
    set(SRCS ${SRCS} gnu.c PARENT_SCOPE)
  endif()
endfunction()
set(SRCS main.c)
if(MSVC)
  list(APPEND SRCS cl.c)
  set(COMPILER_DEF USING_CL)
else()
  set(COMPILER_DEF USING_GNU)
endif()
add_compiler_source()
add_executable(app ${SRCS})
target_compile_definitions(app PRIVATE ${COMPILER_DEF} $<$<TARGET_EXISTS:extra>:HAVE_EXTRA>)
add_library(extra STATIC lib/extra.c)
target_link_libraries(app PRIVATE extra)
"#;
    let main = r#"#include <stdio.h>
int which(void);
int extra(void);
int main(void) {
#if defined(USING_CL)
    const char *name = "cl";
#elif defined(USING_GNU)
    const char *name = "gnu";
#endif
#ifdef HAVE_EXTRA
    printf("%s %d %d\n", name, which(), extra());
#endif
    return 0;
}
"#;
    project.write("CMakeLists.txt", cmake);
    project.write("main.c", main);
    project.write("cl.c", "int which(void) { return 1; }\n");
    project.write("gnu.c", "int which(void) { return 2; }\n");
    project.write("lib/extra.c", "int extra(void) { return 7; }\n");
    project.anvil(&["convert"]).success();

    // The compiler's own source and definition, a PARENT_SCOPE set in a branch, and a target made later
    for (toolchain, expected) in [("msvc", "cl 1 7"), ("gcc", "gnu 2 7")] {
        let run = project.anvil(&["run", "--toolchain", toolchain]);
        run.success();
        assert_eq!(program_output(&run), expected, "{toolchain}");
    }
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
    first.success().contains("Installed out/install:").contains("Finished release");
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
        "lib/cmake/geo/geoTargets-release.cmake".to_string(),
        "lib/pkgconfig/geo.pc".to_string(),
        "lib/pkgconfig/geo-core.pc".to_string(),
        "lib/pkgconfig/geo-util.pc".to_string(),
        "share/geo/install_manifest.txt".to_string(),
    ] {
        assert!(files.contains(&expected), "no {expected} in {files:?}");
    }
    let in_lib = |file: &&String| file.strip_prefix("lib/").is_some_and(|name| !name.contains('/'));
    assert_eq!(files.iter().filter(in_lib).filter(|file| file.contains("core")).count(), 1, "{files:?}");
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

    // A debug install shares the prefix: its programs and libraries go to debug/, the package tells the two apart
    project
        .anvil(&with_toolchain(toolchain, &["install", "-p", "debug"]))
        .success()
        .contains("Finished debug");
    let files = files_under(&project.path("out/install"));
    for expected in [
        format!("bin/tool{exe}"),
        format!("debug/bin/tool{exe}"),
        "debug/bin/assets/maps/one.txt".to_string(),
        "debug/lib/pkgconfig/geo-core.pc".to_string(),
        "lib/cmake/geo/geoTargets-debug.cmake".to_string(),
        "lib/cmake/geo/geoTargets-release.cmake".to_string(),
    ] {
        assert!(files.contains(&expected), "no {expected} in {files:?}");
    }
    let program = project.path(&format!("out/install/debug/bin/tool{exe}"));
    assert_eq!(run_installed(&project.path("out/install/debug"), &program), "area 12 map hills");
    let debug_files = project.read("out/install/lib/cmake/geo/geoTargets-debug.cmake");
    assert!(
        debug_files.contains("IMPORTED_LOCATION_DEBUG \"${_IMPORT_PREFIX}/debug/lib/"),
        "{debug_files}"
    );

    // What the last install copied and this one does not is removed, and nothing else
    project.write("core/include/geo/extra.h", "#pragma once\n");
    project.write("out/install/notes.txt", "mine\n");
    project.anvil(&with_toolchain(toolchain, &["install"])).success();
    assert!(project.path("out/install/include/geo/extra.h").is_file());
    std::fs::remove_file(project.path("core/include/geo/extra.h")).unwrap();
    project
        .anvil(&with_toolchain(toolchain, &["install"]))
        .success()
        .contains("0 files copied")
        .contains("1 removed");
    assert!(!project.path("out/install/include/geo/extra.h").exists());
    assert!(project.path("out/install/notes.txt").is_file());
    assert!(project.path(&format!("out/install/debug/bin/tool{exe}")).is_file());

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

/// Visual Studio's x64 developer environment (vcvarsall), for running CMake with cl.
#[cfg(windows)]
fn msvc_environment() -> Vec<(String, String)> {
    use std::os::windows::process::CommandExt;

    let program_files = std::env::var("ProgramFiles(x86)").expect("ProgramFiles(x86)");
    let vswhere = Path::new(&program_files).join("Microsoft Visual Studio/Installer/vswhere.exe");
    let output = Command::new(vswhere)
        .args(["-latest", "-products", "*", "-property", "installationPath"])
        .output()
        .expect("running vswhere");
    let vcvarsall = Path::new(String::from_utf8_lossy(&output.stdout).trim()).join(r"VC\Auxiliary\Build\vcvarsall.bat");

    let output = Command::new("cmd")
        .raw_arg(format!("/c \"\"{}\" x64 >nul && set\"", vcvarsall.display()))
        .output()
        .expect("running vcvarsall");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

// A CMake project built with MSVC links the installed libraries built with its own C runtime: a Debug build the debug
// install's, a Release build the release install's; with only the other one installed, it finds no package

#[test]
#[cfg(windows)]
fn cmake_msvc_project_links_the_install_of_its_runtime() {
    require!("msvc");
    if !on_path("cmake") || !on_path("ninja") {
        eprintln!("skipped: cmake and ninja must be on PATH");
        return;
    }

    let project = Project::fixture("install", "install_msvc_runtime");
    let consumer = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/install_consumer");
    let prefix = project.path("out/prefix");
    let environment = msvc_environment();
    let cmake = |args: &[&str]| {
        let output = Command::new("cmake")
            .args(args)
            .envs(environment.iter().cloned())
            .output()
            .expect("running cmake");
        let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        (output.status.success(), text)
    };
    let configure = |configuration: &str| {
        let build = project.path(&format!("consumer-{configuration}"));
        let _ = std::fs::remove_dir_all(&build);
        cmake(&[
            "-S",
            &consumer.to_string_lossy(),
            "-B",
            &build.to_string_lossy(),
            "-G",
            "Ninja",
            &format!("-DCMAKE_BUILD_TYPE={configuration}"),
            "-DCMAKE_CXX_COMPILER=cl",
            &format!("-DCMAKE_PREFIX_PATH={}", prefix.to_string_lossy().replace('\\', "/")),
        ])
    };

    // Release only: a Debug build would mix the C runtimes
    project.anvil(&["install", "--toolchain", "msvc", "--prefix", "out/prefix"]).success();
    let (configured, output) = configure("Debug");
    assert!(!configured, "a Debug build took the release install:\n{output}");
    assert!(output.contains("libraries built with the release runtime only"), "{output}");

    // With a debug install too, each configuration links its own
    project
        .anvil(&["install", "--toolchain", "msvc", "-p", "debug", "--prefix", "out/prefix"])
        .success();
    for (configuration, folder) in [("Debug", "debug/"), ("Release", "")] {
        let (configured, output) = configure(configuration);
        assert!(configured, "{output}");
        let build = project.path(&format!("consumer-{configuration}"));
        let (built, output) = cmake(&["--build", &build.to_string_lossy()]);
        assert!(built, "{output}");

        let ninja = std::fs::read_to_string(build.join("build.ninja")).unwrap().replace('\\', "/");
        assert!(
            ninja.contains(&format!("out/prefix/{folder}lib/core.lib")),
            "{configuration} links another core.lib"
        );
        assert_eq!(run_installed(&prefix.join(folder), &build.join("app.exe")), "consumer 10");
    }
}

// What a library's public include folders install: headers, also extensionless ones, but not what builds the package
// (sources, build files, tests, other targets' folders, the prefix inside it); a workspace folder beside the member
// counts as the project's

#[test]
fn install_takes_headers_and_leaves_the_build_out() {
    require!("default");
    let project = Project::empty("install_headers");
    project.write(
        "anvil.toml",
        "[project]\nname = \"hdrs\"\n\n\
         [target.eig]\nkind = \"headers\"\npublic.include = [\".\"]\n\n\
         [target.gen]\nkind = \"lib\"\nsources = [\"src/*.cpp\"]\npublic.include = [\"src\"]\n\n\
         [target.check]\nkind = \"test\"\nsources = [\"tests/check.cpp\"]\ndeps = [\"gen\"]\n",
    );
    for (file, text) in [
        ("Eig/Core", "#pragma once\n#include \"src/Block.h\"\n"),
        ("Eig/src/Block.h", "#pragma once\n"),
        ("NOTES.md", "notes\n"),
        (".github/check.h", "#pragma once\n"),
        ("src/gen.h", "#pragma once\nint gen();\n"),
        ("src/gen.cpp", "#include \"gen.h\"\nint gen() { return 1; }\n"),
        ("src/CMakeLists.txt", "add_library(gen gen.cpp)\n"),
        ("src/config.h.in", "#define X @X@\n"),
        ("src/gen.py", "print(1)\n"),
        ("tests/check.cpp", "#include \"gen.h\"\nint main() { return gen() - 1; }\n"),
        ("tests/helper.h", "#pragma once\n"),
    ] {
        project.write(file, text);
    }

    // Twice: the second finds the prefix inside the root folder, and leaves it out
    project.anvil(&["install", "--prefix", "prefix"]).success();
    project.anvil(&["install", "--prefix", "prefix"]).success().contains("0 files copied");
    let headers = files_under(&project.path("prefix/include"));
    assert_eq!(headers, ["Eig/Core", "Eig/src/Block.h", "gen.h"]);

    // A member's include folder beside it, in the workspace
    let workspace = Project::empty("install_workspace_headers");
    workspace.write("anvil.toml", "[workspace]\nmembers = [\"core\"]\n");
    workspace.write(
        "core/anvil.toml",
        "[project]\nname = \"core\"\n\n[target.core]\nkind = \"lib\"\npublic.include = [\"include\", \"../shared\"]\n",
    );
    workspace.write("core/src/core.cpp", "int core() { return 1; }\n");
    workspace.write("core/include/core/core.h", "#pragma once\n");
    workspace.write("shared/shared/common.h", "#pragma once\n");
    workspace.anvil(&["install"]).success();
    let headers = files_under(&workspace.path("out/install/include"));
    assert_eq!(headers, ["core/core.h", "shared/common.h"]);
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

// [defaults] data goes to the programs and shared libraries only; data that only excludes, data landing where a DLL
// is copied, a version with a space, kind in a [defaults] when block and a project that installs nothing are errors

#[test]
fn data_and_manifest_mistakes() {
    require!("default");
    let project = Project::empty("data_mistakes");
    project.write("src/main.cpp", "int main() {}\n");
    project.write("lib/f.cpp", "int f() { return 1; }\n");
    project.write("assets/a.txt", "a\n");
    project.write("runtime/x.dll", "runtime\n");
    project.write("x.dll", "data\n");
    let program = "[target.app]\nkind = \"exe\"\nsources = [\"src/main.cpp\"]\n";

    project.write(
        "anvil.toml",
        &format!("[defaults]\ndata = [\"assets\"]\n\n[target.f]\nkind = \"lib\"\nsources = [\"lib/*.cpp\"]\n\n{program}deps = [\"f\"]\n"),
    );
    project.anvil(&["build"]).success();
    assert_eq!(project.read("out/debug/assets/a.txt"), "a\n");

    project.write("anvil.toml", &format!("{program}data = [\"!assets/a.txt\"]\n"));
    project.anvil(&["targets"]).code(1).contains("data: only `!` patterns");

    project.write(
        "anvil.toml",
        &format!("[deps]\nrt = {{ libs = [\"user32\"], runtime = [\"runtime/x.dll\"] }}\n\n{program}deps = [\"rt\"]\ndata = [\"x.dll\"]\n"),
    );
    project
        .anvil(&["build"])
        .code(1)
        .contains("the runtime file runtime/x.dll and the data file x.dll would both be copied to out/debug/x.dll");

    project.write("anvil.toml", &format!("[project]\nversion = \"2.1 beta\"\n\n{program}"));
    project
        .anvil(&["targets"])
        .code(1)
        .contains("[project] version = \"2.1 beta\": write a version without spaces");

    project.write("anvil.toml", &format!("[defaults.when.windows]\nkind = \"lib\"\n\n{program}"));
    project
        .anvil(&["targets"])
        .code(1)
        .contains("kind belongs to a [target] table or its when blocks");

    project.write("anvil.toml", &format!("{program}install = false\n"));
    project
        .anvil(&["install"])
        .code(1)
        .contains("nothing to install: every program and library of the project says `install = false`");
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
        .contains("unknown option `x_fastt` (did you mean `x_fast`?)");
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

// 34. A resource script sees _DEBUG with the debug C runtime, as the sources do, and not in release

#[test]
fn resource_script_sees_debug_with_the_debug_runtime_msvc() {
    require!("msvc", "windows");
    let project = Project::empty("rc_debug");
    project.write(
        "anvil.toml",
        "[project]\nname = \"app\"\n\n[target.app]\nkind = \"exe\"\nsources = [\"src/*.c\", \"src/*.rc\"]\n",
    );
    project.write("src/main.c", "int main(void) { return 0; }\n");
    project.write(
        "src/app.rc",
        "#ifdef _DEBUG\nSTRINGTABLE { 1, \"debug\" }\n#else\nSTRINGTABLE { 1, \"release\" }\n#endif\n",
    );

    project.anvil(&["build", "--toolchain", "msvc"]).success();
    assert!(project.read("out/compile_commands.json").contains("/D_DEBUG"));
    project.anvil(&["build", "--release", "--toolchain", "msvc"]).success();
    assert!(!project.read("out/compile_commands.json").contains("/D_DEBUG"));
}

// 35. Converted configure checks give each compiler what CMake gives it: type sizes by platform, cl's dash-spelled
//     flags, a value an option's branch sets, an option declared for Unix only

fn convert_checks(toolchain: &str, test: &str) {
    let project = Project::fixture("convert_checks", test);
    project.anvil(&["convert"]).success();
    let toml = project.read("anvil.toml");
    assert!(toml.contains(r#"when."USE_X11 & unix""#), "{toml}");

    let w4 = if toolchain == "msvc" { 1 } else { 0 };
    let run = project.anvil(&["run", "--toolchain", toolchain]);
    run.success();
    assert_eq!(program_output(&run), format!("sizes=1 foo= w4={w4}"));

    let run = project.anvil(&["run", "--toolchain", toolchain, "-o", "USE_FOO"]);
    run.success();
    assert_eq!(program_output(&run), format!("sizes=1 foo=42 w4={w4}"));
}

#[test]
fn convert_checks_msvc() {
    require!("msvc");
    convert_checks("msvc", "convert_checks_msvc");
}

#[test]
fn convert_checks_gcc() {
    require!("gcc");
    convert_checks("gcc", "convert_checks_gcc");
}

// 36. Package loading: a dependency's version is free-form, two packages of one name are an error, a broken workspace
//     above a project it does not list is ignored (and said inside one of its members)

#[test]
fn package_versions_names_and_unrelated_workspaces() {
    require!("default");
    let project = Project::empty("package_loading");
    let library =
        |name: &str| format!("[project]\nname = \"{name}\"\nversion = \"1.4.0-beta.2\"\n\n[target.{name}]\nkind = \"lib\"\nsources = [\"*.cpp\"]\n");
    project.write("a/anvil.toml", &library("util"));
    project.write("a/a.cpp", "int a() { return 1; }\n");
    project.write("b/anvil.toml", &library("util"));
    project.write("b/b.cpp", "int b() { return 2; }\n");
    project.write("app/src/main.cpp", "int main() {}\n");

    project.write("app/anvil.toml", "[project]\nname = \"app\"\n\n[deps]\na = { path = \"../a\" }\n");
    project.anvil_in("app", &["targets"]).success().contains("util");

    project.write(
        "app/anvil.toml",
        "[project]\nname = \"app\"\n\n[deps]\na = { path = \"../a\" }\nb = { path = \"../b\" }\n",
    );
    project
        .anvil_in("app", &["targets"])
        .code(1)
        .contains("two packages are named `util`")
        .contains("Give one another [project] name");

    // `libs/*` matches no package: an error in a member, not in a project the workspace does not list
    project.write(
        "ws/anvil.toml",
        "[project]\nname = \"ws\"\n\n[workspace]\nmembers = [\"member\", \"libs/*\"]\n",
    );
    project.write("ws/member/anvil.toml", "[project]\nname = \"member\"\n");
    project.write("ws/member/src/main.cpp", "int main() {}\n");
    project.write("ws/other/anvil.toml", "[project]\nname = \"other\"\n");
    project.write("ws/other/src/main.cpp", "int main() {}\n");
    project.anvil_in("ws/other", &["targets"]).success().contains("other");
    project
        .anvil_in("ws/member", &["targets"])
        .code(1)
        .contains("workspace member \"libs/*\" matches no folder");
}

// 37. MinGW gcc's OpenMP runtime (libgomp-1.dll and the DLLs it loads) goes beside the program and into the install,
//     so it runs without the compiler's folder on PATH

#[test]
fn openmp_runtime_of_mingw_gcc_is_copied() {
    require!("gcc", "windows");
    let project = Project::empty("openmp_mingw_runtime");
    project.write("anvil.toml", "[project]\nname = \"omp\"\n\n[deps]\nopenmp = \"system\"\n");
    project.write(
        "src/main.cpp",
        "#include <omp.h>\n#include <cstdio>\nint main() { std::printf(\"%d\\n\", omp_get_max_threads() > 0); }\n",
    );
    project.anvil(&["build", "--toolchain", "gcc"]).success();
    project.anvil(&["install", "--toolchain", "gcc", "--prefix", "installed"]).success();

    let system = format!("{}\\System32", std::env::var("SystemRoot").unwrap());
    for program in ["out/debug-gcc/omp.exe", "installed/bin/omp.exe"] {
        let output = Command::new(project.path(program)).env("PATH", &system).output().unwrap();
        assert!(output.status.success(), "{program} exited with {:?}", output.status.code());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "1");
    }
}

// 38. A resource script gets no second _DEBUG when the target defines `_DEBUG=1`

#[test]
fn resource_script_keeps_a_debug_value_msvc() {
    require!("msvc", "windows");
    let project = Project::empty("rc_debug_value");
    project.write(
        "anvil.toml",
        "[project]\nname = \"app\"\n\n[target.app]\nkind = \"exe\"\nsources = [\"src/*.c\", \"src/*.rc\"]\ndefines = [\"_DEBUG=1\"]\n",
    );
    project.write("src/main.c", "int main(void) { return 0; }\n");
    project.write(
        "src/app.rc",
        "#if _DEBUG != 1\n#error _DEBUG is not 1\n#endif\nSTRINGTABLE { 1, \"debug\" }\n",
    );

    let run = project.anvil(&["build", "-v", "--toolchain", "msvc"]);
    run.success();
    let resource = run.output().lines().find(|line| line.contains("rc.exe")).unwrap().to_string();
    assert!(resource.contains("/D_DEBUG=1") && !resource.contains("/D_DEBUG "), "{resource}");
}

// 39. Hardening: an output folder that holds the project is refused and `clean` never deletes what Anvil did not write;
//     `clean -p` takes only that profile's folders; folders where files go are errors; flags a command ignores are
//     refused; `--target` given a target's name; unused and CMake-only dependencies; messages that say what to write

#[test]
fn output_folder_holding_the_project_is_refused() {
    require!("default");
    let project = Project::empty("harden_out");
    project.write("src/main.cpp", "int main() { return 0; }\n");

    for out in [".", "", "..", "src"] {
        project.write("anvil.toml", &format!("[project]\nname = \"keep\"\nout = \"{out}\"\n"));
        project.anvil(&["build"]).code(1).contains(&format!("[project] out = \"{out}\""));
        project.anvil(&["clean", "--all"]).code(1).contains("anvil clean --all");
        assert!(project.path("src/main.cpp").is_file(), "out = \"{out}\" lost the sources");
    }

    // An output folder holding something else is not deleted whole
    project.write("anvil.toml", "[project]\nname = \"keep\"\nout = \"build\"\n");
    project.anvil(&["build"]).success();
    project.write("build/notes.txt", "mine\n");
    project
        .anvil(&["clean", "--all"])
        .code(1)
        .contains("holds what Anvil did not write (notes.txt)");
    assert!(project.path("build/notes.txt").is_file());
    std::fs::remove_file(project.path("build/notes.txt")).unwrap();
    project.anvil(&["clean", "--all"]).success().contains("Removed");
    assert!(!project.path("build").exists() && project.path("src/main.cpp").is_file());
}

#[test]
fn clean_profile_takes_only_that_profiles_folders() {
    require!("default");
    let project = Project::empty("harden_clean");
    project.write(
        "anvil.toml",
        "[project]\nname = \"p\"\n[profile.rel]\ninherits = \"release\"\n[profile.rel-fast]\ninherits = \"release\"\n",
    );
    project.write("src/main.cpp", "int main() { return 0; }\n");
    project.anvil(&["build", "-p", "rel"]).success();
    project.anvil(&["build", "-p", "rel-fast"]).success();

    project.anvil(&["clean", "-p", "rel"]).success().contains("out/rel").lacks("rel-fast");
    assert!(!project.path("out/rel").exists());
    assert!(project.path("out/rel-fast").is_dir());
}

#[test]
fn folders_where_files_go_are_errors() {
    require!("default");
    let project = Project::empty("harden_folders");
    project.write("src/main.cpp", "int main() { return 0; }\n");
    project.write("inc/x.h", "\n");
    let deps = |key: &str| format!("[project]\nname = \"f\"\n[deps]\nx = {{ include = [\"inc\"], {key} }}\n");

    for entry in ["runtime = [\"src\"]", "runtime = [\"C:/\"]", "runtime = [\"/\"]"] {
        project.write("anvil.toml", &deps(entry));
        project
            .anvil(&["build"])
            .code(1)
            .contains("is a folder; `runtime` lists the files copied beside the programs");
    }
    project.write("anvil.toml", &deps("libs = [\"./src\"]"));
    project.anvil(&["build"]).code(1).contains("libs = [\"./src\"] is a folder");
    project.write("anvil.toml", "[project]\nname = \"f\"\n[target.f]\nkind = \"exe\"\npch = \"src\"\n");
    project
        .anvil(&["build"])
        .code(1)
        .contains("pch = \"src\" is a folder; pch names a header");
}

#[test]
fn flags_a_command_ignores_are_refused() {
    let project = Project::empty("harden_flags");
    project.write("anvil.toml", "[project]\nname = \"f\"\n");

    project.anvil(&["convert", "-o", "X", "--toolchain", "gcc"]).code(2).contains("--check");
    project.anvil(&["convert", "-j", "3"]).code(2).contains("unexpected argument '-j'");
    for command in ["targets", "compdb", "update"] {
        project.anvil(&[command, "--timings"]).code(2).contains("unexpected argument '--timings'");
    }
    project.anvil(&["tidy", "--timings"]).code(2);
    project.anvil(&["vcpkg", "list", "--", "x"]).code(2);
}

#[test]
fn target_flag_given_a_target_name_says_how_to_build_it() {
    require!("default");
    let project = Project::empty("harden_target");
    project.write("anvil.toml", "[project]\nname = \"game\"\n");
    project.write("src/main.cpp", "int main() { return 0; }\n");

    project
        .anvil(&["build", "--target", "game"])
        .code(1)
        .contains("`--target` picks the machine to build for")
        .contains("write `anvil build game`");
    project
        .anvil(&["build", "--target", "aarch64-nowhere"])
        .code(1)
        .contains("cannot tell its operating system");
}

#[test]
fn unused_and_cmake_only_dependencies_are_warned() {
    require!("default");
    let project = Project::empty("harden_deps");
    project.write(
        "app/anvil.toml",
        "[project]\nname = \"app\"\n[deps]\nused = { path = \"../used\" }\nglm = { include = [\"inc\"] }\n\
         [dev-deps]\ncheck = { include = [\"inc\"] }\n[target.app]\nkind = \"exe\"\ndeps = [\"used\"]\n",
    );
    project.write("app/inc/x.h", "\n");
    project.write("app/src/main.cpp", "int used();\nint main() { return used(); }\n");
    project.write("used/CMakeLists.txt", "add_library(used src/used.cpp)\n");
    project.write("used/src/used.cpp", "int used() { return 0; }\n");

    project
        .anvil_in("app", &["build"])
        .success()
        .contains("`[deps] glm`: no target uses it")
        .contains("`[dev-deps] check`: no test uses it")
        .contains("has a CMakeLists.txt but no anvil.toml")
        .contains("anvil convert")
        .lacks("`[deps] used`");

    // Under the layout conventions every target uses every entry
    project.write("app/anvil.toml", "[project]\nname = \"app\"\n[deps]\nglm = { include = [\"inc\"] }\n");
    project.write("app/src/main.cpp", "int main() { return 0; }\n");
    project.anvil_in("app", &["build"]).success().lacks("no target uses it");
}

#[test]
fn messages_say_what_to_write() {
    require!("default");
    let project = Project::empty("harden_messages");
    project.write("src/main.cpp", "int main() { return 0; }\n");
    let manifest = |rest: &str| format!("[project]\nname = \"m\"\n{rest}");

    project.write("anvil.toml", &manifest("[[test]]\nname = \"t\"\ncommand = []\n"));
    project.anvil(&["test"]).code(1).contains("test `t`: `command` is empty");
    project.write(
        "anvil.toml",
        &manifest("[target.m]\nkind = \"exe\"\n[[target.m.run]]\ncommand = \"\"\noutput = \"x.h\"\n"),
    );
    project.anvil(&["build"]).code(1).contains("[target.m] [[run]]: `command` is empty");
    project.write("anvil.toml", &manifest("[target.m]\nkind = \"exe\"\nstd = \"c++99\"\n"));
    project.anvil(&["build"]).code(1).contains("C++ standards are 11, 14, 17, 20, 23 and 26");
    project.write("anvil.toml", &manifest("[target.m]\nkind = \"exe\"\nif = \"\"\n"));
    project.anvil(&["build"]).code(1).contains("empty condition: write one");
    project.write("anvil.toml", &manifest("[target.m]\nkind = \"exe\"\ninclude = [\"missing\"]\n"));
    project
        .anvil(&["build"])
        .success()
        .contains("include folder missing does not exist (included by `m`)");
    project.write("anvil.toml", "[project]\nname = \"game\"\n");
    project
        .anvil(&["build", "gmae"])
        .code(1)
        .contains("no target named `gmae` (did you mean `game`?)");

    // Listing no tests is an answer; -q leaves notes out
    project.anvil(&["test", "--list"]).success().contains("no tests");
    project.anvil(&["-q", "new", "quiet"]).success().lacks("note:");
}

// 40. A precompiled header: every C++ file sees it without including it, and editing it rebuilds them all

fn precompiled_header(toolchain: &str, test: &str) {
    let project = Project::empty(test);
    project.write(
        "anvil.toml",
        "[project]\nname = \"pch\"\n\n[target.app]\nkind = \"exe\"\npch = \"src/pch.h\"\n",
    );
    project.write("src/pch.h", "#pragma once\n#include <cstdio>\n#define PCH_VALUE 1\n");
    project.write("src/value.cpp", "int value() { return PCH_VALUE * 10; }\n");
    project.write(
        "src/main.cpp",
        "int value();\nint main() { std::printf(\"pch %d %d\\n\", PCH_VALUE, value()); }\n",
    );

    let first = project.anvil(&["run", "--toolchain", toolchain]);
    first.success();
    assert_eq!(program_output(&first), "pch 1 10");
    assert!(first.actions().contains(&"Precompiling src/pch.h".to_string()), "{:?}", first.actions());
    project.anvil(&["build", "--toolchain", toolchain]).success().up_to_date();

    // An edited header is precompiled again, and every file using it compiles again
    project.edit("src/pch.h", "PCH_VALUE 1", "PCH_VALUE 2");
    let edited = project.anvil(&["run", "--toolchain", toolchain]);
    edited.success();
    assert_eq!(program_output(&edited), "pch 2 20");
    let actions = edited.actions();
    for expected in ["Precompiling src/pch.h", "Compiling src/main.cpp", "Compiling src/value.cpp"] {
        assert!(actions.contains(&expected.to_string()), "no {expected} in {actions:?}");
    }

    // A source edit recompiles that file only, with the header as it was
    project.edit("src/value.cpp", "* 10", "* 100");
    let source = project.anvil(&["run", "--toolchain", toolchain]);
    source.success();
    assert_eq!(program_output(&source), "pch 2 200");
    assert_eq!(source.compiled(), 1, "{:?}", source.actions());
}

#[test]
fn precompiled_header_rebuilds_when_edited_msvc() {
    require!("msvc");
    precompiled_header("msvc", "pch_msvc");
}

#[test]
fn precompiled_header_rebuilds_when_edited_gcc() {
    require!("gcc");
    precompiled_header("gcc", "pch_gcc");
}

// 41. Unity builds: N files per unity source, the program right, and an edit recompiles its batch only

fn unity_build(toolchain: &str, test: &str) {
    let project = Project::empty(test);
    project.write("anvil.toml", "[project]\nname = \"uni\"\n\n[target.app]\nkind = \"exe\"\nunity = 2\n");

    // Sorted into batches: [a, b], [c, d], [main]
    for (name, value) in [("a", 1), ("b", 2), ("c", 3), ("d", 4)] {
        project.write(&format!("src/{name}.cpp"), &format!("int {name}() {{ return {value}; }}\n"));
    }
    project.write(
        "src/main.cpp",
        "#include <cstdio>\nint a(); int b(); int c(); int d();\nint main() { std::printf(\"unity %d\\n\", a() * 1000 + b() * 100 + c() * 10 + d()); }\n",
    );
    let anvil = |args: &[&str]| project.anvil(&with_toolchain(Some(toolchain), args));

    let first = anvil(&["run"]);
    first.success();
    assert_eq!(program_output(&first), "unity 1234");
    assert_eq!(first.compiled(), 3, "{:?}", first.actions());
    let mut batches: Vec<String> = first.actions().into_iter().filter(|action| action.starts_with("Compiling")).collect();
    batches.sort();
    assert_eq!(batches, ["Compiling unity_0.cpp", "Compiling unity_1.cpp", "Compiling unity_2.cpp"]);
    anvil(&["build"]).success().up_to_date();

    // d.cpp is in the second batch: only that batch compiles again
    project.edit("src/d.cpp", "return 4;", "return 9;");
    let edited = anvil(&["run"]);
    edited.success();
    assert_eq!(program_output(&edited), "unity 1239");
    assert_eq!(edited.compiled(), 1, "{:?}", edited.actions());
    assert!(edited.actions().contains(&"Compiling unity_1.cpp".to_string()), "{:?}", edited.actions());
}

#[test]
fn unity_build_recompiles_one_batch_msvc() {
    require!("msvc");
    unity_build("msvc", "unity_msvc");
}

#[test]
fn unity_build_recompiles_one_batch_gcc() {
    require!("gcc");
    unity_build("gcc", "unity_gcc");
}

// 42. lto = true in release: compiles and links with link-time optimization, a static library included

fn link_time_optimization(toolchain: &str, test: &str) {
    let project = Project::empty(test);
    project.write(
        "anvil.toml",
        "[project]\nname = \"lto\"\n\n[profile.release]\nlto = true\n\n\
         [target.twice]\nkind = \"lib\"\nsources = [\"lib/*.c\"]\n\n\
         [target.app]\nkind = \"exe\"\nsources = [\"src/main.c\"]\ndeps = [\"twice\"]\n",
    );
    project.write("lib/twice.c", "int twice(int x) { return 2 * x; }\n");
    project.write(
        "src/main.c",
        "#include <stdio.h>\nint twice(int x);\nint main(void) { printf(\"lto %d\\n\", twice(21)); return 0; }\n",
    );

    let run = project.anvil(&["run", "app", "--release", "-v", "--toolchain", toolchain]);
    run.success();
    assert_eq!(program_output(&run).lines().last(), Some("lto 42"));
    if toolchain == "msvc" {
        run.contains("/GL").contains("/LTCG");
    } else {
        run.contains("-flto").contains("gcc-ar");
    }

    // Debug has none of it
    let debug = project.anvil(&["run", "app", "-v", "--toolchain", toolchain]);
    debug.success().lacks("/LTCG").lacks("-flto");
}

#[test]
fn lto_release_profile_runs_msvc() {
    require!("msvc");
    link_time_optimization("msvc", "lto_msvc");
}

#[test]
fn lto_release_profile_runs_gcc() {
    require!("gcc");
    link_time_optimization("gcc", "lto_gcc");
}

// 43. sanitize = ["address"] with MSVC: a clean run passes, a heap overflow fails with AddressSanitizer's report

#[test]
fn address_sanitizer_reports_heap_overflow_msvc() {
    require!("msvc", "windows");
    let project = Project::empty("asan_msvc");
    project.write(
        "anvil.toml",
        "[project]\nname = \"asan\"\n\n[profile.asan]\ninherits = \"debug\"\nsanitize = [\"address\"]\n",
    );
    project.write(
        "src/main.c",
        "#include <stdio.h>\n#include <stdlib.h>\nint main(int argc, char **argv) {\n    int *values = malloc(4 * sizeof(int));\n    int index = argc > 1 ? 4 : 3;\n    values[index] = 1;\n    printf(\"wrote %d\\n\", index);\n    free(values);\n    return 0;\n}\n",
    );

    let clean = project.anvil(&["run", "-p", "asan", "--toolchain", "msvc"]);
    clean.success();
    assert_eq!(program_output(&clean), "wrote 3");
    let folder = project.root.join(clean.program()).parent().unwrap().to_path_buf();
    let runtime = std::fs::read_dir(folder)
        .unwrap()
        .any(|entry| entry.unwrap().file_name().to_string_lossy().starts_with("clang_rt.asan"));
    assert!(runtime, "no ASan runtime DLL beside the program");

    let overflow = project.anvil(&["run", "-p", "asan", "--toolchain", "msvc", "--", "overflow"]);
    assert_ne!(overflow.code, 0, "{}", overflow.output());
    overflow
        .contains("ERROR: AddressSanitizer: heap-buffer-overflow")
        .contains("main.c:6")
        .lacks("wrote 4");
}

// 44. --target x86 builds a 32-bit program, into its own folder, that sees when.x86; gcc needs multilib

#[test]
fn cross_build_x86_has_four_byte_pointers_msvc() {
    require!("msvc", "windows");
    let project = Project::empty("x86_msvc");
    project.write(
        "anvil.toml",
        "[project]\nname = \"ptr\"\n\n[target.ptr]\nkind = \"exe\"\nwhen.x86.defines = [\"IS_X86\"]\n",
    );
    project.write(
        "src/main.c",
        "#include <stdio.h>\nint main(void) {\n#ifdef IS_X86\n    printf(\"x86 \");\n#endif\n    printf(\"%d\\n\", (int)sizeof(void *));\n    return 0;\n}\n",
    );

    let native = project.anvil(&["run", "--toolchain", "msvc"]);
    native.success();
    assert_eq!(program_output(&native), "8");

    let cross = project.anvil(&["run", "--toolchain", "msvc", "--target", "x86"]);
    cross.success().contains("for x86");
    assert_eq!(program_output(&cross), "x86 4");
    assert_ne!(cross.program(), native.program(), "the x86 build overwrote the x64 one");
    project.anvil(&["build", "--toolchain", "msvc"]).success().up_to_date();
}

#[test]
fn cross_build_x86_has_four_byte_pointers_gcc() {
    require!("gcc", "windows");
    let project = Project::empty("x86_gcc");
    project.write("anvil.toml", "[project]\nname = \"ptr\"\n");
    project.write(
        "src/main.c",
        "#include <stdio.h>\nint main(void) { printf(\"%d\\n\", (int)sizeof(void *)); return 0; }\n",
    );

    let cross = project.anvil(&["run", "--toolchain", "gcc", "--target", "x86"]);
    if cross.code != 0 && cross.output().contains("cannot build 32-bit programs") {
        eprintln!("skipped: this gcc has no 32-bit multilib");
        return;
    }
    cross.success();
    assert_eq!(program_output(&cross), "4");
}

// 45. CUDA: a .cu kernel adding vectors on the GPU, called from C++

/// Whether nvcc is installed (on PATH or under CUDA_PATH).
fn has_nvcc() -> bool {
    let under_cuda_path = std::env::var_os("CUDA_PATH").is_some_and(|folder| {
        let name = if cfg!(windows) { "nvcc.exe" } else { "nvcc" };
        Path::new(&folder).join("bin").join(name).is_file()
    });
    under_cuda_path || on_path("nvcc")
}

#[test]
fn cuda_kernel_adds_vectors() {
    require!("msvc", "windows");
    if !has_nvcc() {
        eprintln!("skipped: nvcc not found");
        return;
    }
    let project = Project::empty("cuda");
    project.write("anvil.toml", "[project]\nname = \"vadd\"\n\n[target.vadd]\nkind = \"exe\"\n");
    project.write(
        "src/add.cu",
        r#"#include <cuda_runtime.h>

__global__ void add(const int *a, const int *b, int *sum, int count) {
    int index = blockIdx.x * blockDim.x + threadIdx.x;
    if (index < count) sum[index] = a[index] + b[index];
}

int add_vectors(const int *a, const int *b, int *sum, int count) {
    int *device_a, *device_b, *device_sum;
    size_t bytes = count * sizeof(int);
    if (cudaMalloc(&device_a, bytes) != cudaSuccess) return -1;
    cudaMalloc(&device_b, bytes);
    cudaMalloc(&device_sum, bytes);
    cudaMemcpy(device_a, a, bytes, cudaMemcpyHostToDevice);
    cudaMemcpy(device_b, b, bytes, cudaMemcpyHostToDevice);
    add<<<(count + 255) / 256, 256>>>(device_a, device_b, device_sum, count);
    cudaError_t status = cudaMemcpy(sum, device_sum, bytes, cudaMemcpyDeviceToHost);
    cudaFree(device_a);
    cudaFree(device_b);
    cudaFree(device_sum);
    return status == cudaSuccess ? 0 : -1;
}
"#,
    );
    project.write(
        "src/main.cpp",
        r#"#include <cstdio>
int add_vectors(const int *a, const int *b, int *sum, int count);
int main() {
    static int a[1000], b[1000], sum[1000];
    for (int i = 0; i < 1000; i++) { a[i] = i; b[i] = 2 * i; }
    if (add_vectors(a, b, sum, 1000) != 0) { std::printf("no CUDA device\n"); return 0; }
    long long total = 0;
    for (int i = 0; i < 1000; i++) total += sum[i];
    std::printf("sum %lld\n", total);
}
"#,
    );

    let run = project.anvil(&["run", "--toolchain", "msvc"]);
    run.success().contains("Compiling src/add.cu");
    let output = program_output(&run);
    if output == "no CUDA device" {
        eprintln!("skipped running the kernel: no CUDA device");
        return;
    }
    assert_eq!(output, "sum 1498500");
    project.anvil(&["build", "--toolchain", "msvc"]).success().up_to_date();
}

// 46. .asm sources assemble with ml64 and link into the program; an edit assembles again

#[test]
fn masm_source_links_into_program_msvc() {
    require!("msvc", "windows");
    let project = Project::empty("masm");
    project.write(
        "anvil.toml",
        "[project]\nname = \"asm\"\n\n[target.app]\nkind = \"exe\"\nsources = [\"src/*.c\", \"src/*.asm\"]\n",
    );
    project.write(
        "src/add.asm",
        ".code\nadd_numbers PROC\n    lea rax, [rcx + rdx]\n    ret\nadd_numbers ENDP\nEND\n",
    );
    project.write(
        "src/main.c",
        "#include <stdio.h>\nlong long add_numbers(long long a, long long b);\nint main(void) { printf(\"asm %lld\\n\", add_numbers(40, 2)); return 0; }\n",
    );

    // The program's line comes last (ml64 names the file it assembles on stdout)
    let first = project.anvil(&["run", "--toolchain", "msvc"]);
    first.success().contains("Assembling src/add.asm");
    assert_eq!(program_output(&first).lines().last(), Some("asm 42"));

    project.edit("src/add.asm", "[rcx + rdx]", "[rcx + rdx + 1]");
    let edited = project.anvil(&["run", "--toolchain", "msvc"]);
    edited.success();
    assert_eq!(program_output(&edited).lines().last(), Some("asm 43"));
    assert_eq!(edited.compiled(), 1, "{:?}", edited.actions());
}

// 47. An object library's files link into the program that uses it, its public defines with them; whole-archive
//     links a static library's self-registering object that nothing calls

const REGISTRY_PLUGIN: &str = r#"extern int registered;
static void register_plugin(void) { registered += 40; }
#ifdef _MSC_VER
#pragma section(".CRT$XCU", read)
__declspec(allocate(".CRT$XCU")) static void (*register_entry)(void) = register_plugin;
#else
__attribute__((constructor)) static void register_entry(void) { register_plugin(); }
#endif
"#;

fn object_library_and_whole_archive(toolchain: &str, test: &str) {
    let project = Project::empty(test);
    project.write(
        "anvil.toml",
        "[project]\nname = \"objlib\"\n\n\
         [target.parts]\nkind = \"object\"\nsources = [\"parts/*.c\"]\npublic.defines = [\"PARTS_COUNT=2\"]\n\n\
         [target.registry]\nkind = \"lib\"\nsources = [\"registry/*.c\"]\n\n\
         [target.app]\nkind = \"exe\"\nsources = [\"src/main.c\"]\ndeps = [\"parts\"]\nwhole-archive = [\"registry\"]\n",
    );
    project.write("parts/one.c", "int part_one(void) { return 1; }\n");
    project.write("parts/two.c", "int part_two(void) { return 2; }\n");
    project.write("registry/registry.c", "int registered = 0;\n");
    project.write("registry/plugin.c", REGISTRY_PLUGIN);
    project.write(
        "src/main.c",
        "#include <stdio.h>\nint part_one(void);\nint part_two(void);\nextern int registered;\n\
         int main(void) { printf(\"parts %d %d registered %d\\n\", PARTS_COUNT, part_one() + part_two(), registered); return 0; }\n",
    );
    let anvil = |args: &[&str]| project.anvil(&with_toolchain(Some(toolchain), args));

    let run = anvil(&["run", "app"]);
    run.success();
    assert_eq!(program_output(&run), "parts 2 3 registered 40");
    assert!(
        !run.actions()
            .iter()
            .any(|action| action.contains("Archiving") && action.contains("parts")),
        "an object library was archived: {:?}",
        run.actions()
    );

    // As a plain dependency, the linker leaves the unreferenced object out
    project.edit("anvil.toml", "whole-archive = [\"registry\"]", "deps = [\"parts\", \"registry\"]");
    project.edit("anvil.toml", "deps = [\"parts\"]\n", "");
    let plain = anvil(&["run", "app"]);
    plain.success();
    assert_eq!(program_output(&plain), "parts 2 3 registered 0");
}

#[test]
fn object_library_and_whole_archive_link_msvc() {
    require!("msvc");
    object_library_and_whole_archive("msvc", "objlib_msvc");
}

#[test]
fn object_library_and_whole_archive_link_gcc() {
    require!("gcc");
    object_library_and_whole_archive("gcc", "objlib_gcc");
}

// 48. exports = true: a program exports a function to the plugin it loads, which links against the program

const EXPORTS_HOST: &str = r#"#include <stdio.h>
#ifdef _WIN32
#include <windows.h>
#define HOST_API __declspec(dllexport)
#else
#include <dlfcn.h>
#define HOST_API
#endif

HOST_API int host_base(void) { return 40; }

int main(void) {
#ifdef _WIN32
    HMODULE plugin = LoadLibraryA("plugin.dll");
    int (*value)(void) = plugin ? (int (*)(void))GetProcAddress(plugin, "plugin_value") : NULL;
#else
    void *plugin = dlopen("./libplugin.so", RTLD_NOW);
    int (*value)(void) = plugin ? (int (*)(void))dlsym(plugin, "plugin_value") : NULL;
#endif
    if (!value) { printf("no plugin\n"); return 1; }
    printf("plugin %d\n", value());
    return 0;
}
"#;

const EXPORTS_PLUGIN: &str = r#"#ifdef _WIN32
#define HOST_API __declspec(dllimport)
#define PLUGIN_API __declspec(dllexport)
#else
#define HOST_API
#define PLUGIN_API
#endif
HOST_API int host_base(void);
PLUGIN_API int plugin_value(void) { return host_base() + 2; }
"#;

fn program_exports(toolchain: &str, test: &str) {
    let project = Project::empty(test);
    project.write(
        "anvil.toml",
        "[project]\nname = \"exports\"\n\n\
         [target.host]\nkind = \"exe\"\nsources = [\"host/*.c\"]\nexports = true\n\n\
         [target.plugin]\nkind = \"shared\"\nsources = [\"plugin/*.c\"]\ndeps = [\"host\"]\n",
    );
    project.write("host/host.c", EXPORTS_HOST);
    project.write("plugin/plugin.c", EXPORTS_PLUGIN);

    let build = project.anvil(&["build", "--toolchain", toolchain]);
    build.success();
    assert_eq!(build.linked(), 2, "{:?}", build.actions());
    let run = project.anvil(&["run", "host", "--toolchain", toolchain]);
    run.success();
    assert_eq!(program_output(&run), "plugin 42");
}

#[test]
fn program_exports_to_its_plugin_msvc() {
    require!("msvc", "windows");
    program_exports("msvc", "exports_msvc");
}

#[test]
fn program_exports_to_its_plugin_gcc() {
    require!("gcc", "windows");
    program_exports("gcc", "exports_gcc");
}

// 49. A pre-build step runs before the first build and when the target's sources change; a depfile step runs again
//     when a file only its depfile names changes

const DEPFILE_GENERATOR: &str = r##"#define _CRT_SECURE_NO_WARNINGS
/* gen <list> <output> <depfile>: the list names a file beside it; the header defines VALUE as that file's first
   line, and the depfile names both */
#include <stdio.h>
#include <string.h>
static void first_line(const char *path, char *line, int size) {
    FILE *file = fopen(path, "r");
    line[0] = 0;
    if (file) {
        if (!fgets(line, size, file)) line[0] = 0;
        fclose(file);
    }
    line[strcspn(line, "\r\n")] = 0;
}
int main(int argc, char **argv) {
    char name[512], folder[512], path[1100], value[256];
    FILE *output, *depfile;
    char *slash, *backslash;
    if (argc != 4) return 2;
    first_line(argv[1], name, sizeof name);
    strcpy(folder, argv[1]);
    slash = strrchr(folder, '/');
    backslash = strrchr(folder, '\\');
    if (backslash > slash) slash = backslash;
    if (slash) slash[1] = 0; else folder[0] = 0;
    snprintf(path, sizeof path, "%s%s", folder, name);
    first_line(path, value, sizeof value);
    output = fopen(argv[2], "w");
    if (!output) return 3;
    fprintf(output, "#define VALUE \"%s\"\n", value);
    fclose(output);
    depfile = fopen(argv[3], "w");
    if (!depfile) return 4;
    fprintf(depfile, "value.h: %s \\\n  %s\n", argv[1], path);
    fclose(depfile);
    return 0;
}
"##;

#[test]
fn pre_build_and_depfile_steps_rerun_when_needed() {
    require!("default");
    let project = Project::empty("depfile_steps");
    project.write(
        "anvil.toml",
        r#"[project]
name = "depfile"

[target.gen]
kind = "exe"
sources = ["tools/gen.c"]

[target.app]
kind = "exe"
sources = ["src/*.c"]

[[target.app.pre-build]]
shell = true
message = "Stamping"
command = "echo ran>>{root}/prebuild.log"

[[target.app.run]]
output = "{gen}/value.h"
depfile = true
command = ["{target:gen}", "{root}/data/list.txt", "{output}", "{depfile}"]
"#,
    );
    project.write("tools/gen.c", DEPFILE_GENERATOR);
    project.write("data/list.txt", "extra.txt\n");
    project.write("data/extra.txt", "one\n");
    project.write(
        "src/main.c",
        "#include <stdio.h>\n#include \"value.h\"\nint main(void) { printf(\"value %s\\n\", VALUE); return 0; }\n",
    );
    let runs = || project.read("prebuild.log").lines().filter(|line| line.trim() == "ran").count();

    let first = project.anvil(&["run", "app"]);
    first.success().contains("Stamping");
    assert_eq!(program_output(&first), "value one");
    assert_eq!(runs(), 1);
    project.anvil(&["build"]).success().up_to_date();
    assert_eq!(runs(), 1);

    // extra.txt is named only by the depfile
    project.write("data/extra.txt", "two\n");
    let edited = project.anvil(&["run", "app"]);
    edited.success();
    assert_eq!(program_output(&edited), "value two");
    assert!(edited.actions().contains(&"Running gen".to_string()), "{:?}", edited.actions());
    project.anvil(&["build"]).success().up_to_date();

    // A source edit runs the pre-build step again
    project.edit("src/main.c", "value %s", "val %s");
    let source = project.anvil(&["run", "app"]);
    source.success().contains("Stamping");
    assert_eq!(program_output(&source), "val two");
    assert_eq!(runs(), 2);
}

// 50. anvil update moves a git dependency on a branch to the branch's newest commit; until then the pin holds

#[test]
fn update_moves_git_branch_pin() {
    require!("default", "git");
    let project = Project::empty("update_git");
    let dependency = project.path("upd");
    project.write("upd/anvil.toml", "[project]\nname = \"upd\"\n");
    project.write("upd/include/upd/upd.h", "#pragma once\nint upd_value(void);\n");
    project.write("upd/src/upd.c", "int upd_value(void) { return 1; }\n");
    git(&dependency, &["init", "--quiet"]);
    git(&dependency, &["add", "-A"]);
    git(&dependency, &["commit", "--quiet", "-m", &format!("one, in {}", dependency.display())]);
    let first_commit = git(&dependency, &["rev-parse", "HEAD"]);
    let _checkouts = GitCheckouts::of(&project);

    let url = format!("file:///{}", dependency.display().to_string().replace('\\', "/").trim_start_matches('/'));
    project.write(
        "user/anvil.toml",
        &format!("[project]\nname = \"user\"\n\n[deps]\nupd = {{ git = \"{url}\", branch = \"main\" }}\n"),
    );
    project.write(
        "user/src/main.c",
        "#include <stdio.h>\n#include \"upd/upd.h\"\nint main(void) { printf(\"upd %d\\n\", upd_value()); return 0; }\n",
    );

    let run = project.anvil_in("user", &["run"]);
    run.success();
    assert_eq!(program_output(&run), "upd 1");
    assert!(project.read("user/anvil.lock").contains(&first_commit));

    // A new commit on the branch: the build stays on the pin
    project.write("upd/src/upd.c", "int upd_value(void) { return 2; }\n");
    git(&dependency, &["commit", "--quiet", "-am", "two"]);
    let second_commit = git(&dependency, &["rev-parse", "HEAD"]);
    let pinned = project.anvil_in("user", &["run"]);
    pinned.success();
    assert_eq!(program_output(&pinned), "upd 1");

    project
        .anvil_in("user", &["update", "nosuchdep"])
        .code(1)
        .contains("`nosuchdep` is not a git dependency");
    project.anvil_in("user", &["update", "upd"]).success().contains("Updated");
    let lock = project.read("user/anvil.lock");
    assert!(lock.contains(&second_commit) && !lock.contains(&first_commit), "{lock}");
    let moved = project.anvil_in("user", &["run"]);
    moved.success();
    assert_eq!(program_output(&moved), "upd 2");
    project.anvil_in("user", &["update"]).success().contains("already has the newest");
}

// 51. anvil test --junit writes a JUnit report with the failures counted; --rerun-failed runs only those

/// Checks that `text` is well-formed XML as far as a JUnit report needs: a declaration, then elements that close in
/// order, attributes quoted.
#[track_caller]
fn assert_well_formed_xml(text: &str) {
    let body = text.trim_start();
    assert!(body.starts_with("<?xml "), "no XML declaration:\n{text}");
    let body = &body[body.find("?>").expect("an ended declaration") + 2..];
    let tag = regex::Regex::new(r#"<(/?)([A-Za-z_][\w.-]*)((?:\s+[\w:.-]+="[^"<]*")*)\s*(/?)>"#).unwrap();
    let mut open: Vec<String> = Vec::new();
    let mut position = 0;
    let mut roots = 0;
    for found in tag.captures_iter(body) {
        let whole = found.get(0).unwrap();
        let between = &body[position..whole.start()];
        assert!(!between.contains('<'), "a malformed tag before byte {}:\n{text}", whole.start());
        position = whole.end();
        let name = found[2].to_string();
        if &found[1] == "/" {
            assert_eq!(open.pop().as_deref(), Some(name.as_str()), "</{name}> closes another element:\n{text}");
        } else if &found[4] != "/" {
            if open.is_empty() {
                roots += 1;
            }
            open.push(name);
        }
    }
    assert!(!body[position..].contains('<'), "a malformed tag at the end:\n{text}");
    assert!(open.is_empty(), "unclosed {open:?}:\n{text}");
    assert_eq!(roots, 1, "not one root element:\n{text}");
}

#[test]
fn test_junit_report_and_rerun_failed() {
    require!("default");
    let project = Project::empty("junit_rerun");
    project.write("anvil.toml", "[project]\nname = \"tt\"\n");
    project.write("src/lib.c", "int lib_value(void) { return 1; }\n");
    project.write("tests/good_one.c", "int main(void) { return 0; }\n");
    project.write("tests/good_two.c", "int main(void) { return 0; }\n");
    project.write(
        "tests/bad.c",
        "#include <stdio.h>\nint main(void) { printf(\"bad <output> & more\\n\"); return 3; }\n",
    );

    project
        .anvil(&["test", "--junit", "report.xml"])
        .code(1)
        .contains("test bad ... FAILED")
        .contains("1 of 3 tests failed: bad");
    let report = project.read("report.xml");
    assert_well_formed_xml(&report);
    assert!(report.contains(r#"<testsuites tests="3" failures="1""#), "{report}");
    assert!(report.contains(r#"<testcase name="bad""#), "{report}");
    assert!(report.contains("bad &lt;output&gt; &amp; more"), "the output is not escaped:\n{report}");
    assert_eq!(report.matches("<failure").count(), 1, "{report}");

    // Only the failed test runs again, until it passes
    project
        .anvil(&["test", "--rerun-failed"])
        .code(1)
        .contains("Testing 1 test")
        .lacks("good_one");
    project.write("tests/bad.c", "int main(void) { return 0; }\n");
    project
        .anvil(&["test", "--rerun-failed", "--junit", "report.xml"])
        .success()
        .contains("Passed 1 of 1");
    let report = project.read("report.xml");
    assert_well_formed_xml(&report);
    assert!(report.contains(r#"tests="1" failures="0""#), "{report}");
    project.anvil(&["test", "--rerun-failed"]).success().contains("no test failed last time");
    project.anvil(&["test"]).success().contains("Passed 3 of 3");
}

// 52. anvil build -k: a file that does not compile fails the build, the others still compile, and a fix compiles it
//     alone

#[test]
fn keep_going_compiles_the_other_files() {
    require!("default");
    let project = Project::empty("keep_going");
    project.write("anvil.toml", "[project]\nname = \"keep\"\n");
    for name in ["a", "b", "d", "e"] {
        project.write(&format!("src/{name}.c"), &format!("int {name}(void) {{ return 1; }}\n"));
    }
    project.write("src/c.c", "int c(void) { return missing; }\n");
    project.write(
        "src/main.c",
        "#include <stdio.h>\nint a(void); int b(void); int c(void); int d(void); int e(void);\n\
         int main(void) { printf(\"sum %d\\n\", a() + b() + c() + d() + e()); return 0; }\n",
    );

    let failed = project.anvil(&["build", "-k", "-j", "1"]);
    assert_ne!(failed.code, 0, "{}", failed.output());
    failed.contains("Compiling src/c.c failed").contains("build failed");
    let actions = failed.actions();
    for name in ["a", "b", "d", "e", "main"] {
        assert!(
            actions.contains(&format!("Compiling src/{name}.c")),
            "{name}.c was not compiled: {actions:?}"
        );
    }
    assert!(!actions.iter().any(|action| action.starts_with("Linking")), "{actions:?}");

    project.write("src/c.c", "int c(void) { return 1; }\n");
    let fixed = project.anvil(&["run"]);
    fixed.success();
    assert_eq!(fixed.compiled(), 1, "{:?}", fixed.actions());
    assert_eq!(program_output(&fixed), "sum 5");
}

// 53. anvil clean deletes the profile's folders (each toolchain's too) and leaves the rest; clean -p <name> that
//     profile's only; clean --all the output folder

#[test]
fn clean_removes_the_profile_folders_only() {
    require!("msvc", "gcc");
    let project = Project::empty("clean_profiles");
    project.write(
        "anvil.toml",
        "[project]\nname = \"cl\"\n\n[profile.rel]\ninherits = \"release\"\n\n[profile.rel-fast]\ninherits = \"release\"\n",
    );
    project.write("src/main.c", "#include <stdio.h>\nint main(void) { printf(\"clean\\n\"); return 0; }\n");
    for args in [
        &["--toolchain", "msvc"][..],
        &["--toolchain", "gcc"],
        &["--release", "--toolchain", "msvc"],
        &["--release", "--toolchain", "gcc"],
        &["-p", "rel", "--toolchain", "msvc"],
        &["-p", "rel-fast", "--toolchain", "msvc"],
    ] {
        let mut run_args = vec!["run"];
        run_args.extend_from_slice(args);
        let run = project.anvil(&run_args);
        run.success();
        assert_eq!(program_output(&run), "clean");
    }
    let folders = || {
        let mut names: Vec<String> = std::fs::read_dir(project.path("out"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    assert_eq!(
        folders(),
        [
            ".anvil-out",
            "compile_commands.json",
            "debug-gcc",
            "debug-msvc",
            "rel-fast-msvc",
            "rel-msvc",
            "release-gcc",
            "release-msvc"
        ]
    );

    // Debug, each toolchain's folder; the source tree untouched
    project.anvil(&["clean"]).success().contains("Removed");
    assert_eq!(
        folders(),
        [
            ".anvil-out",
            "compile_commands.json",
            "rel-fast-msvc",
            "rel-msvc",
            "release-gcc",
            "release-msvc"
        ]
    );
    assert!(project.path("src/main.c").is_file());

    // `clean -p rel` leaves out/rel-fast-msvc, another profile's folder
    project.anvil(&["clean", "-p", "rel"]).success();
    assert_eq!(
        folders(),
        [".anvil-out", "compile_commands.json", "rel-fast-msvc", "release-gcc", "release-msvc"]
    );

    project.anvil(&["clean", "--release"]).success();
    assert!(!folders().iter().any(|name| name.starts_with("release")), "{:?}", folders());
    project.anvil(&["clean", "--release"]).success().contains("nothing built for `release`");

    // A folder anvil did not make is left alone
    project.write("out/notes/keep.txt", "mine\n");
    project.anvil(&["clean", "-p", "notes"]).success();
    assert!(project.path("out/notes/keep.txt").is_file());

    // --all deletes the output folder only when it holds nothing but what anvil wrote
    project
        .anvil(&["clean", "--all"])
        .code(1)
        .contains("holds what Anvil did not write (notes)");
    assert!(project.path("out/notes/keep.txt").is_file());
    std::fs::remove_dir_all(project.path("out/notes")).unwrap();
    project.anvil(&["clean", "--all"]).success();
    assert!(!project.path("out").exists());
    assert!(project.path("anvil.toml").is_file() && project.path("src/main.c").is_file());
}

// 54. anvil add of a vcpkg port, then a build that installs and links it

#[test]
fn add_vcpkg_port_then_build() {
    require!("msvc", "windows", "vcpkg");
    let project = Project::empty("add_vcpkg");
    project.write("anvil.toml", "[project]\nname = \"usefmt\"\n");
    project.write(
        "src/main.cpp",
        "#include <fmt/core.h>\nint main() { fmt::print(\"fmt {}\\n\", 6 * 7); }\n",
    );

    project.anvil(&["add", "fmt"]).success().contains("Added fmt");
    let manifest = project.read("anvil.toml");
    assert!(manifest.contains("[deps]\nfmt = \"vcpkg\""), "{manifest}");

    // The program's line comes last (vcpkg's install prints on stdout too)
    let run = project.anvil(&["run", "--toolchain", "msvc"]);
    run.success();
    assert_eq!(program_output(&run).lines().last(), Some("fmt 42"));
    project.anvil(&["build", "--toolchain", "msvc"]).success().up_to_date();
}
