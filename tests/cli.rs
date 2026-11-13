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
#[ignore = "open bug: a [workspace] root with no [project] and no sources warns `\"*.{c,cc,cpp,cxx}\" matches no files`"]
fn workspace_root_without_sources_does_not_warn() {
    require!("default");
    let project = Project::fixture("workspace", "workspace_warning");
    project.anvil(&["build"]).success().lacks("warning:");
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

/// Deletes the checkouts of these commits from anvil's shared git cache (`<user data>/anvil/git/<name>-<key>`) when
/// the test ends, found by the commit each records in `.anvil-ready`.
struct GitCheckouts(Vec<String>);

impl Drop for GitCheckouts {
    fn drop(&mut self) {
        let user_data = if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        } else {
            std::env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        };
        let Some(cache) = user_data.map(|folder| folder.join("anvil").join("git")) else {
            return;
        };

        for entry in std::fs::read_dir(cache).into_iter().flatten().flatten() {
            let commit = std::fs::read_to_string(entry.path().join(".anvil-ready")).unwrap_or_default();
            if self.0.iter().any(|ours| *ours == commit.trim()) {
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
    let _checkouts = GitCheckouts(vec![v1.clone(), v2.clone()]);

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

// anvil fmt formats the package's files as .clang-format says, past ignored and output folders; anvil tidy reports
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
