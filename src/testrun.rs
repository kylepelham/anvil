//! Runs tests in parallel: each in its own process with a timeout, output captured and shown for failures.

use crate::plan;
use crate::project::{Model, TestCase};
use crate::ui;
use anyhow::Result;
use std::collections::HashSet;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT_SECONDS: u64 = 600;

// How often a running test is checked for its timeout
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// What the workers share: tests not started yet, the names of finished ones, and what is running.
struct Schedule {
    pending: Vec<TestCase>,
    finished: HashSet<String>,
    running: usize,
    serial_running: bool,
}

struct TestResult {
    name: String,
    passed: bool,
    skipped: bool,
    detail: String,
    output: String,
    elapsed: Duration,
}

/// Runs the tests; the exit code is 0 when all pass.
pub fn run_tests(model: &Model, tests: &[TestCase], jobs: usize) -> Result<i32> {
    let started = Instant::now();
    ui::status("Testing", &format!("{} test{}", tests.len(), if tests.len() == 1 { "" } else { "s" }));

    // A shared schedule; each worker takes the next test whose `after` tests are done
    let schedule = Arc::new((
        Mutex::new(Schedule {
            pending: tests.to_vec(),
            finished: HashSet::new(),
            running: 0,
            serial_running: false,
        }),
        Condvar::new(),
    ));
    let results = Arc::new(Mutex::new(Vec::new()));

    std::thread::scope(|scope| {
        for _ in 0..jobs.clamp(1, tests.len().max(1)) {
            let schedule = schedule.clone();
            let results = results.clone();

            scope.spawn(move || {
                while let Some(test) = take_next(&schedule, &results) {
                    let result = run_one(model, &test);
                    print_result(&result);
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

    // Failures in full, then the tally
    let results = std::mem::take(&mut *results.lock().unwrap());
    let failures: Vec<&TestResult> = results.iter().filter(|result| !result.passed).collect();
    for failure in &failures {
        ui::print_line(&format!("\n{} {} ({})", ui::bold_red("---- failed:"), failure.name, failure.detail));
        if !failure.output.trim().is_empty() {
            ui::print_block(&failure.output);
        }
    }

    let passed = results.len() - failures.len();
    let seconds = started.elapsed().as_secs_f64();
    if failures.is_empty() {
        ui::status("Passed", &format!("{passed} of {} in {seconds:.2}s", results.len()));
        Ok(0)
    } else {
        let names: Vec<&str> = failures.iter().map(|failure| failure.name.as_str()).collect();
        ui::error(&format!("{} of {} tests failed: {}", failures.len(), results.len(), names.join(", ")));
        Ok(1)
    }
}

/// Waits for a test that may start: its `after` tests finished, and no serial test in the way. None when all are taken.
fn take_next(schedule: &(Mutex<Schedule>, Condvar), results: &Mutex<Vec<TestResult>>) -> Option<TestCase> {
    let (state, changed) = schedule;
    let mut state = state.lock().unwrap();

    loop {
        if state.pending.is_empty() {
            return None;
        }

        // The first ready test, in order
        let ready = state.pending.iter().position(|test| {
            let waits = test.after.iter().any(|name| !state.finished.contains(name));
            let blocked = state.serial_running || (test.serial && state.running > 0);
            !waits && !blocked
        });
        if let Some(index) = ready {
            let test = state.pending.remove(index);
            state.running += 1;
            state.serial_running = test.serial;
            return Some(test);
        }

        // Nothing can ever start: the rest wait on each other
        if state.running == 0 {
            for test in std::mem::take(&mut state.pending) {
                let result = TestResult {
                    name: test.name.clone(),
                    passed: false,
                    skipped: false,
                    detail: format!("never ran: after = [{}] waits in a cycle", test.after.join(", ")),
                    output: String::new(),
                    elapsed: Duration::ZERO,
                };
                print_result(&result);
                results.lock().unwrap().push(result);
            }
            return None;
        }

        state = changed.wait(state).unwrap();
    }
}

fn run_one(model: &Model, test: &TestCase) -> TestResult {
    let started = Instant::now();
    let mut result = TestResult {
        name: test.name.clone(),
        passed: false,
        skipped: false,
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

    // `cmake -E` commands Anvil runs itself, as in build steps
    let (program, args) = match crate::cmake_tool::handled(&words) {
        Some(cmake_args) => {
            let anvil = std::env::current_exe().unwrap_or_else(|_| "anvil".into());
            let mut args = vec![crate::cmake_tool::SUBCOMMAND.to_string()];
            args.extend(cmake_args.iter().cloned());
            (anvil.to_string_lossy().into_owned(), args)
        }
        None => (words[0].clone(), words[1..].to_vec()),
    };
    let mut command = Command::new(&program);
    command
        .args(&args)
        .current_dir(&test.workdir)
        .envs(test.env.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            result.detail = format!("could not start {}: {error}", words[0]);
            return result;
        }
    };

    // Drain output on threads so a chatty test never blocks on a full pipe
    let stdout_reader = drain(child.stdout.take());
    let stderr_reader = drain(child.stderr.take());

    // Wait, killing it at the timeout
    let timeout = Duration::from_secs(test.timeout.unwrap_or(DEFAULT_TIMEOUT_SECONDS));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(_) => break None,
        }
    };

    result.output = stdout_reader.join().unwrap_or_default() + &stderr_reader.join().unwrap_or_default();
    result.elapsed = started.elapsed();

    let Some(status) = status else {
        result.detail = format!("timed out after {}s", timeout.as_secs());
        return result;
    };

    // The test said it could not run: skipped, which does not fail the run
    let skip_code = test.skip_exit_code.is_some_and(|code| status.code() == Some(code));
    let skip_output = test.skip_regex.as_ref().is_some_and(|pattern| pattern.is_match(&result.output));
    if skip_code || skip_output {
        result.passed = true;
        result.skipped = true;
        return result;
    }

    // Exit code, or the pass pattern when there is one (CTest's rule), then the fail pattern
    let exit_text = status.code().map(|code| code.to_string()).unwrap_or_else(|| "none (crashed)".into());
    let exit_code = test.exit_code.unwrap_or(0);
    let (mut passed, mut detail) = match &test.pass_regex {
        Some(pattern) if pattern.is_match(&result.output) => (true, String::new()),
        Some(pattern) => (false, format!("output does not match pass-regex \"{pattern}\"")),
        None if status.code() == Some(exit_code) => (true, String::new()),
        None if test.exit_code.is_some() => (false, format!("exit code {exit_text}, expected {exit_code}")),
        None => (false, format!("exit code {exit_text}")),
    };

    // Every expect pattern must match, no reject pattern may
    if let Some(pattern) = test.expect.iter().find(|pattern| !pattern.is_match(&result.output)).filter(|_| passed) {
        passed = false;
        detail = format!("output has no match for expect \"{pattern}\"");
    }
    let rejected = test.reject.iter().find_map(|pattern| Some((pattern, pattern.find(&result.output)?)));
    if let Some((pattern, found)) = rejected.filter(|_| passed) {
        passed = false;
        detail = format!("output matches reject \"{pattern}\": \"{}\"", found.as_str());
    }
    if let Some(pattern) = test.fail_regex.as_ref().filter(|pattern| pattern.is_match(&result.output)) {
        passed = false;
        detail = format!("output matches fail-regex \"{pattern}\"");
    }

    // will-fail turns it around
    if test.will_fail {
        detail = if passed {
            format!("passed, but will-fail is set (exit code {exit_text})")
        } else {
            String::new()
        };
        passed = !passed;
    }

    result.passed = passed;
    result.detail = detail;

    result
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut pipe) = pipe {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            text = String::from_utf8_lossy(&bytes).into_owned();
        }
        text
    })
}

fn print_result(result: &TestResult) {
    let verdict = match (result.passed, result.skipped) {
        (_, true) => ui::dim("skipped"),
        (true, false) => ui::bold_green("ok"),
        (false, false) => ui::bold_red("FAILED"),
    };
    let time = ui::dim(&format!("({:.2}s)", result.elapsed.as_secs_f64()));
    ui::print_line(&format!("  test {} ... {verdict} {time}", result.name));
}
