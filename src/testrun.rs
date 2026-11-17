//! Runs tests in parallel: each in its own process tree with a timeout, output captured and shown for failures.

use crate::exec::INTERRUPTED;
use crate::job::ProcessTree;
use crate::plan;
use crate::project::{Model, TestCase};
use crate::ui;
use crate::util::{self, slash};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::io::{PipeReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT_SECONDS: u64 = 600;

// How often a running test is checked for its timeout
const POLL_INTERVAL: Duration = Duration::from_millis(5);

// The tests that failed and have not passed since, one name per line, in the profile's .anvil folder
const LAST_FAILED_FILE: &str = "last-failed";

// The exit code of a run stopped by Ctrl-C
const INTERRUPTED_EXIT_CODE: i32 = 130;

// How long a test's output is still read after it ended: what it wrote is in the pipe by then, but a process that
// left its tree may hold the pipe open, and is not waited for
const OUTPUT_GRACE: Duration = Duration::from_secs(2);

// The same when processes of the test still run after it ended by itself (a server later tests use)
const SURVIVOR_OUTPUT_GRACE: Duration = Duration::from_millis(200);

pub struct TestOptions {
    pub jobs: usize,
    pub show_output: bool,
    pub junit: Option<PathBuf>,
}

/// What the workers share: tests not started yet (with their place in the run), the names of finished ones, and
/// what is running.
struct Schedule {
    pending: Vec<(usize, TestCase)>,
    finished: HashSet<String>,
    running: usize,
    serial_running: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Outcome {
    Passed,
    Skipped,
    Failed,
    Interrupted,
}

struct TestResult {
    index: usize,
    name: String,
    outcome: Outcome,
    detail: String,
    output: String,
    elapsed: Duration,
}

/// Runs the tests; the exit code is 0 when all pass.
pub fn run_tests(model: &Model, tests: &[TestCase], options: &TestOptions) -> Result<i32> {
    let started = Instant::now();
    ui::status("Testing", &format!("{} test{}", tests.len(), if tests.len() == 1 { "" } else { "s" }));

    // `after` names a test outside this run: nothing to wait for
    let selected: HashSet<&str> = tests.iter().map(|test| test.name.as_str()).collect();
    let mut pending: Vec<(usize, TestCase)> = tests.iter().cloned().enumerate().collect();
    for (_, test) in &mut pending {
        test.after.retain(|name| selected.contains(name.as_str()));
    }

    // A shared schedule; each worker takes the next test whose `after` tests are done
    let schedule = Arc::new((
        Mutex::new(Schedule {
            pending,
            finished: HashSet::new(),
            running: 0,
            serial_running: false,
        }),
        Condvar::new(),
    ));
    let results = Arc::new(Mutex::new(Vec::new()));

    std::thread::scope(|scope| {
        for _ in 0..options.jobs.clamp(1, tests.len().max(1)) {
            let schedule = schedule.clone();
            let results = results.clone();

            scope.spawn(move || {
                while let Some((index, test)) = take_next(&schedule) {
                    let result = run_one(model, index, &test);
                    print_result(&result, options.show_output);
                    results.lock().unwrap().push(result);

                    // Let waiting workers look again
                    let (state, changed) = &*schedule;
                    let mut state = state.lock().unwrap();
                    state.running -= 1;
                    state.serial_running &= !test.serial;
                    state.finished.insert(test.name.clone());
                    changed.notify_all();
                }
            });
        }
    });

    // Failures in full, in the run's order
    let mut results = std::mem::take(&mut *results.lock().unwrap());
    results.sort_by_key(|result| result.index);
    let count = |outcome: Outcome| results.iter().filter(|result| result.outcome == outcome).count();
    let failures: Vec<&TestResult> = results.iter().filter(|result| result.outcome == Outcome::Failed).collect();
    for failure in &failures {
        ui::print_line(&format!("\n{} {} ({})", ui::bold_red("---- failed:"), failure.name, failure.detail));
        if !failure.output.trim().is_empty() {
            ui::print_block(&failure.output);
        }
    }

    // What failed, for --rerun-failed; the report
    write_last_failed(model, &results)?;
    if let Some(path) = &options.junit {
        write_junit(path, &model.packages[0].name, tests, &results, started.elapsed())?;
    }

    // The tally
    let seconds = started.elapsed().as_secs_f64();
    let skipped = match count(Outcome::Skipped) {
        0 => String::new(),
        skipped => format!(", {skipped} skipped"),
    };
    if INTERRUPTED.load(Ordering::Relaxed) {
        let passed = count(Outcome::Passed) + count(Outcome::Skipped);
        let not_run = tests.len() - passed - failures.len();
        ui::error(&format!(
            "interrupted: {passed} passed, {} failed, {not_run} stopped or not started",
            failures.len()
        ));
        return Ok(INTERRUPTED_EXIT_CODE);
    }

    if failures.is_empty() {
        ui::status("Passed", &format!("{} of {} in {seconds:.2}s{skipped}", results.len(), results.len()));
        Ok(0)
    } else {
        let names: Vec<&str> = failures.iter().map(|failure| failure.name.as_str()).collect();
        ui::error(&format!("{} of {} tests failed: {}", failures.len(), results.len(), names.join(", ")));
        Ok(1)
    }
}

/// The names of the tests that failed and have not passed since (empty when none did).
pub fn last_failed(model: &Model) -> Vec<String> {
    let text = std::fs::read_to_string(last_failed_path(model)).unwrap_or_default();
    text.lines().filter(|line| !line.is_empty()).map(str::to_string).collect()
}

/// Waits for a test that may start: its `after` tests finished (they wait in no circle, the project was checked),
/// and no serial test in the way. None when all are taken, or after Ctrl-C.
fn take_next(schedule: &(Mutex<Schedule>, Condvar)) -> Option<(usize, TestCase)> {
    let (state, changed) = schedule;
    let mut state = state.lock().unwrap();

    loop {
        if state.pending.is_empty() || INTERRUPTED.load(Ordering::Relaxed) {
            return None;
        }

        // The first ready test, in order
        let ready = state.pending.iter().position(|(_, test)| {
            let waits = test.after.iter().any(|name| !state.finished.contains(name));
            let blocked = state.serial_running || (test.serial && state.running > 0);
            !waits && !blocked
        });
        if let Some(position) = ready {
            let (index, test) = state.pending.remove(position);
            state.running += 1;
            state.serial_running = test.serial;
            return Some((index, test));
        }

        // Wake up now and then to see Ctrl-C
        state = changed.wait_timeout(state, Duration::from_millis(100)).unwrap().0;
    }
}

/// How a test's program ended.
enum Ending {
    Exited(ExitStatus),
    TimedOut,
    Interrupted,
}

fn run_one(model: &Model, index: usize, test: &TestCase) -> TestResult {
    let started = Instant::now();
    let mut result = TestResult {
        index,
        name: test.name.clone(),
        outcome: Outcome::Failed,
        detail: String::new(),
        output: String::new(),
        elapsed: Duration::ZERO,
    };

    // {target:X} becomes the built program
    let words: Result<Vec<String>> = test.command.iter().map(|word| plan::substitute_targets(word, model)).collect();
    let words = match words {
        Ok(words) if !words.is_empty() => words,
        Ok(_) => {
            result.detail = "empty command".into();
            return result;
        }
        Err(error) => {
            result.detail = format!("{error:#}");
            return result;
        }
    };

    // A folder of the build's output is made, as CMake's configure makes those its tests run in; a missing folder
    // elsewhere would only say "could not start"
    if !test.workdir.is_dir() && test.workdir.starts_with(&model.out_dir) {
        let _ = std::fs::create_dir_all(&test.workdir);
    }
    if !test.workdir.is_dir() {
        result.detail = format!("its workdir {} does not exist", slash(&test.workdir));
        return result;
    }

    let mut command = match test_command(&words, test) {
        Ok(command) => command,
        Err(error) => {
            result.detail = error;
            return result;
        }
    };
    command.current_dir(&test.workdir).stdin(Stdio::null());

    // Both streams into one pipe, so the output keeps the order the test wrote it in
    let reader = match output_pipe(&mut command) {
        Ok(reader) => reader,
        Err(error) => {
            result.detail = format!("could not create a pipe for its output: {error}");
            return result;
        }
    };
    let spawned = ProcessTree::spawn(&mut command);

    // The command holds the pipe's write ends: reading would never end
    let program = command.get_program().to_string_lossy().into_owned();
    drop(command);

    let mut tree = match spawned {
        Ok(tree) => tree,
        Err(error) => {
            result.detail = format!("could not start {program}: {error}");
            return result;
        }
    };
    let output_reader = OutputReader::start(reader);

    // Wait, stopping the whole tree at the timeout or Ctrl-C
    let timeout = Duration::from_secs(test.timeout.unwrap_or(DEFAULT_TIMEOUT_SECONDS));
    let ending = loop {
        if INTERRUPTED.load(Ordering::Relaxed) {
            break Ending::Interrupted;
        }
        match tree.try_wait() {
            Ok(Some(status)) => break Ending::Exited(status),
            Ok(None) if started.elapsed() > timeout => break Ending::TimedOut,
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(error) => {
                result.detail = format!("waiting for it failed: {error}");
                tree.kill();
                return result;
            }
        }
    };

    // A timeout or Ctrl-C stops what it left running; after a normal end that goes on (a server later tests use)
    let survivors = match ending {
        Ending::Exited(_) => tree.has_survivors(),
        Ending::TimedOut | Ending::Interrupted => {
            tree.kill();
            false
        }
    };
    drop(tree);
    result.output = output_reader.finish(if survivors { SURVIVOR_OUTPUT_GRACE } else { OUTPUT_GRACE });
    result.elapsed = started.elapsed();

    match ending {
        Ending::Exited(status) => (result.outcome, result.detail) = judge(test, status, &result.output),
        Ending::TimedOut => result.detail = format!("timed out after {}s", timeout.as_secs()),
        Ending::Interrupted => {}
    }

    // Ctrl-C reaches the test's console too: a test that ended badly then did not fail, it was stopped
    if INTERRUPTED.load(Ordering::Relaxed) && result.outcome == Outcome::Failed {
        result.outcome = Outcome::Interrupted;
        result.detail = "interrupted".into();
    }

    result
}

/// The command a test's words run: `cmake -E env` runs its program directly with the variables applied, other
/// `cmake -E` commands run in Anvil (as in build steps), anything else as written.
fn test_command(words: &[String], test: &TestCase) -> std::result::Result<Command, String> {
    let Some(cmake_args) = crate::cmake_tool::handled(words) else {
        let mut command = Command::new(&words[0]);
        command.args(&words[1..]).envs(test.env.iter().cloned());
        return Ok(command);
    };

    if cmake_args[0] == "env" {
        let env = crate::cmake_tool::parse_env(&cmake_args[1..], &test.env).map_err(|error| format!("cmake -E env: {error:#}"))?;
        let Some((program, args)) = env.command.split_first() else {
            return Err("cmake -E env has no command to run".into());
        };

        let mut command = Command::new(program);
        command.args(args).envs(test.env.iter().cloned());
        env.apply(&mut command);
        return Ok(command);
    }

    let anvil = std::env::current_exe().unwrap_or_else(|_| "anvil".into());
    let mut command = Command::new(anvil);
    command.arg(crate::cmake_tool::SUBCOMMAND).args(cmake_args).envs(test.env.iter().cloned());
    Ok(command)
}

/// Passed, skipped or failed (and why) by the exit status and output.
fn judge(test: &TestCase, status: ExitStatus, output: &str) -> (Outcome, String) {
    // The test said it could not run: skipped, which does not fail the run
    let skip_code = test.skip_exit_code.is_some_and(|code| status.code() == Some(code));
    let skip_output = test.skip_regex.as_ref().is_some_and(|pattern| pattern.is_match(output));
    if skip_code || skip_output {
        return (Outcome::Skipped, String::new());
    }

    // Exit code, or the pass pattern when there is one (CTest's rule), then the fail pattern
    let exit_text = describe_exit(status);
    let exit_code = test.exit_code.unwrap_or(0);
    let (mut passed, mut detail) = match &test.pass_regex {
        Some(pattern) if pattern.is_match(output) => (true, String::new()),
        Some(pattern) => (false, format!("output does not match pass-regex \"{pattern}\"")),
        None if status.code() == Some(exit_code) => (true, String::new()),
        None if test.exit_code.is_some() => (false, format!("{exit_text}, expected {exit_code}")),
        None => (false, exit_text.clone()),
    };

    // Every expect pattern must match, no reject pattern may
    if let Some(pattern) = test.expect.iter().find(|pattern| !pattern.is_match(output)).filter(|_| passed) {
        passed = false;
        detail = format!("output has no match for expect \"{pattern}\"");
    }
    let rejected = test.reject.iter().find_map(|pattern| Some((pattern, pattern.find(output)?)));
    if let Some((pattern, found)) = rejected.filter(|_| passed) {
        passed = false;
        detail = format!("output matches reject \"{pattern}\": \"{}\"", found.as_str());
    }
    if let Some(pattern) = test.fail_regex.as_ref().filter(|pattern| pattern.is_match(output)) {
        passed = false;
        detail = format!("output matches fail-regex \"{pattern}\"");
    }

    // will-fail turns it around
    if test.will_fail {
        detail = if passed {
            format!("passed, but will-fail is set ({exit_text})")
        } else {
            String::new()
        };
        passed = !passed;
    }

    (if passed { Outcome::Passed } else { Outcome::Failed }, detail)
}

/// "exit code 3", or how it crashed: "exit code 0xC0000005: access violation", "killed by SIGSEGV: segmentation
/// fault".
pub fn describe_exit(status: ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return match signal_name(signal) {
                Some((name, meaning)) => format!("killed by {name}: {meaning}"),
                None => format!("killed by signal {signal}"),
            };
        }
    }

    match status.code() {
        // Windows reports a crash as an NTSTATUS, negative as an i32
        Some(code) if code < 0 => {
            let status = code as u32;
            match crash_name(status) {
                Some(name) => format!("exit code 0x{status:08X}: {name}"),
                None => format!("exit code 0x{status:08X}"),
            }
        }
        Some(code) => format!("exit code {code}"),
        None => "no exit code".into(),
    }
}

/// What a Windows crash status means.
fn crash_name(status: u32) -> Option<&'static str> {
    Some(match status {
        0x80000003 => "breakpoint",
        0xC0000005 => "access violation",
        0xC0000017 => "out of memory",
        0xC000001D => "illegal instruction",
        0xC000007B => "bad image: a DLL or program built for another processor",
        0xC000008E => "floating-point division by zero",
        0xC0000094 => "integer division by zero",
        0xC0000095 => "integer overflow",
        0xC0000096 => "privileged instruction",
        0xC00000FD => "stack overflow",
        0xC0000135 => "DLL not found: one it needs is not beside it or on PATH",
        0xC0000139 => "DLL entry point not found",
        0xC0000142 => "DLL initialization failed",
        0xC000013A => "Ctrl-C",
        0xC0000374 => "heap corruption",
        0xC0000409 => "fast fail: abort(), std::terminate or a buffer overrun check",
        0xC0000420 => "assertion failure",
        0xC0000602 => "fail fast",
        0xE06D7363 => "unhandled C++ exception",
        _ => return None,
    })
}

#[cfg(unix)]
fn signal_name(signal: i32) -> Option<(&'static str, &'static str)> {
    Some(match signal {
        libc::SIGABRT => ("SIGABRT", "abort"),
        libc::SIGBUS => ("SIGBUS", "bus error"),
        libc::SIGFPE => ("SIGFPE", "arithmetic error"),
        libc::SIGILL => ("SIGILL", "illegal instruction"),
        libc::SIGINT => ("SIGINT", "interrupted"),
        libc::SIGKILL => ("SIGKILL", "killed"),
        libc::SIGPIPE => ("SIGPIPE", "broken pipe"),
        libc::SIGSEGV => ("SIGSEGV", "segmentation fault"),
        libc::SIGTERM => ("SIGTERM", "terminated"),
        libc::SIGTRAP => ("SIGTRAP", "trap"),
        _ => return None,
    })
}

/// Gives the command one pipe for stdout and stderr; the end to read.
fn output_pipe(command: &mut Command) -> std::io::Result<PipeReader> {
    let (reader, writer) = std::io::pipe()?;
    command.stdout(writer.try_clone()?).stderr(writer);
    Ok(reader)
}

/// Reads a test's output on a thread of its own, until every process holding the pipe has closed it.
struct OutputReader {
    /// What was read so far; taken by `finish`, after which the thread throws away the rest.
    bytes: Arc<Mutex<Option<Vec<u8>>>>,
    closed: mpsc::Receiver<()>,
}

impl OutputReader {
    fn start(mut pipe: impl Read + Send + 'static) -> OutputReader {
        let bytes = Arc::new(Mutex::new(Some(Vec::new())));
        let (sender, closed) = mpsc::channel();

        let shared = bytes.clone();
        std::thread::spawn(move || {
            // Keep reading after `finish`, so a process still writing never blocks on a full pipe
            let mut chunk = [0u8; 16384];
            while let Ok(count) = pipe.read(&mut chunk) {
                if count == 0 {
                    break;
                }
                if let Some(bytes) = shared.lock().unwrap().as_mut() {
                    bytes.extend_from_slice(&chunk[..count]);
                }
            }
            let _ = sender.send(());
        });

        OutputReader { bytes, closed }
    }

    /// The output, once the pipe closes or `grace` passes; a pipe still open then is left to the thread, and the
    /// output says it was cut.
    fn finish(self, grace: Duration) -> String {
        let closed = !matches!(self.closed.recv_timeout(grace), Err(mpsc::RecvTimeoutError::Timeout));
        let bytes = self.bytes.lock().unwrap().take().unwrap_or_default();

        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        if !closed {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str("[output cut: a process the test started still holds its output open]\n");
        }
        text
    }
}

/// One line per test, and with --show-output a passing test's output under it.
fn print_result(result: &TestResult, show_output: bool) {
    let verdict = match result.outcome {
        Outcome::Passed => ui::bold_green("ok"),
        Outcome::Skipped => ui::dim("skipped"),
        Outcome::Failed => ui::bold_red("FAILED"),
        Outcome::Interrupted => ui::dim("interrupted"),
    };
    let time = ui::dim(&format!("({:.2}s)", result.elapsed.as_secs_f64()));
    let mut text = format!("  test {} ... {verdict} {time}\n", result.name);

    let passed = matches!(result.outcome, Outcome::Passed | Outcome::Skipped);
    if show_output && passed && !result.output.trim().is_empty() {
        text.push_str(&result.output);
    }
    ui::print_block(&text);
}

fn last_failed_path(model: &Model) -> PathBuf {
    model.out_dir.join(".anvil").join(LAST_FAILED_FILE)
}

/// The last failures, less the tests that passed now, plus those that failed now.
fn write_last_failed(model: &Model, results: &[TestResult]) -> Result<()> {
    let mut names = last_failed(model);
    names.retain(|name| {
        !results
            .iter()
            .any(|result| result.name == *name && matches!(result.outcome, Outcome::Passed | Outcome::Skipped))
    });
    for result in results.iter().filter(|result| result.outcome == Outcome::Failed) {
        if !names.contains(&result.name) {
            names.push(result.name.clone());
        }
    }

    let path = last_failed_path(model);
    if names.is_empty() && !path.exists() {
        return Ok(());
    }
    let text: String = names.iter().map(|name| format!("{name}\n")).collect();
    util::write_atomic(&path, text.as_bytes())
}

/// A JUnit XML report: one testsuite for the project, a testcase per test that ran.
fn write_junit(path: &Path, project: &str, tests: &[TestCase], results: &[TestResult], elapsed: Duration) -> Result<()> {
    let count = |outcome: Outcome| results.iter().filter(|result| result.outcome == outcome).count();
    let project = xml_escape(project);
    let totals = format!(
        "tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\" time=\"{:.3}\"",
        results.len(),
        count(Outcome::Failed),
        count(Outcome::Interrupted),
        count(Outcome::Skipped),
        elapsed.as_secs_f64()
    );

    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str(&format!("<testsuites {totals}>\n"));
    xml.push_str(&format!("  <testsuite name=\"{project}\" {totals}>\n"));
    for result in results {
        let opening = format!(
            "    <testcase name=\"{}\" classname=\"{project}\" time=\"{:.3}\"",
            xml_escape(&result.name),
            result.elapsed.as_secs_f64()
        );

        // Labels as a property, which CI test reports show and filter by
        let labels = &tests[result.index].labels;
        let mut body = String::new();
        if !labels.is_empty() {
            let value = xml_escape(&labels.join(","));
            body.push_str(&format!("      <properties><property name=\"labels\" value=\"{value}\"/></properties>\n"));
        }

        let message = xml_escape(&result.detail);
        match result.outcome {
            Outcome::Passed => {}
            Outcome::Skipped => body.push_str("      <skipped/>\n"),
            Outcome::Failed => body.push_str(&format!(
                "      <failure message=\"{message}\">{}</failure>\n",
                xml_escape(&result.output)
            )),
            Outcome::Interrupted => body.push_str(&format!("      <error message=\"{message}\"/>\n")),
        }
        match body.is_empty() {
            true => xml.push_str(&format!("{opening}/>\n")),
            false => xml.push_str(&format!("{opening}>\n{body}    </testcase>\n")),
        }
    }
    xml.push_str("  </testsuite>\n</testsuites>\n");

    util::write_atomic(path, xml.as_bytes()).with_context(|| format!("writing the JUnit report {}", slash(path)))
}

/// Text for an XML attribute or element; characters XML 1.0 cannot hold (control characters, U+FFFE, U+FFFF) are
/// left out.
fn xml_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            // XML 1.0's Char: #x9 | #xA | #xD | [#x20-#xD7FF] | [#xE000-#xFFFD] | [#x10000-#x10FFFF]
            '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}' => escaped.push(character),
            _ => {}
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_crashes() {
        assert_eq!(crash_name(0xC0000005), Some("access violation"));
        assert!(crash_name(0xC0000135).unwrap().starts_with("DLL not found"));
        assert_eq!(crash_name(0x00000003), None);
    }

    #[test]
    fn escapes_xml() {
        assert_eq!(xml_escape("a<b> & \"c\"\u{1b}[0m\n"), "a&lt;b&gt; &amp; &quot;c&quot;[0m\n");
        assert_eq!(xml_escape("x\u{FFFE}\u{FFFF}\u{FFFD}\u{1F600}y"), "x\u{FFFD}\u{1F600}y");
    }
}
