//! Build timings: when each action ran and on which job lane, written as a Chrome trace (chrome://tracing,
//! ui.perfetto.dev) and summed up by `--timings`.

use crate::plan::ActionKind;
use crate::ui;
use crate::util;
use anyhow::Result;
use serde_json::json;
use std::path::Path;
use std::time::Duration;

/// The trace's file name, in the profile's `.anvil` folder.
pub const TRACE_FILE: &str = "trace.json";

// How many of the slowest actions --timings lists
const SLOWEST_SHOWN: usize = 10;

/// One action that ran: its process's start and end from the build's start, and the job lane (1 to -j) it ran on.
#[derive(Debug, Clone)]
pub struct Timing {
    pub description: String,
    pub kind: ActionKind,
    pub lane: usize,
    pub start: Duration,
    pub end: Duration,
    pub failed: bool,
}

impl Timing {
    fn duration(&self) -> Duration {
        self.end.saturating_sub(self.start)
    }
}

/// Writes the trace of what ran: one complete event per action, one row per lane.
pub fn write_trace(path: &Path, timings: &[Timing]) -> Result<()> {
    util::write_atomic(path, trace_text(timings).as_bytes())
}

/// Prints the slowest actions and the time spent per kind of action.
pub fn print_report(timings: &[Timing], trace: &str) {
    if timings.is_empty() {
        return;
    }

    // The slowest actions, longest first
    let mut slowest: Vec<&Timing> = timings.iter().collect();
    slowest.sort_by_key(|timing| std::cmp::Reverse(timing.duration()));
    slowest.truncate(SLOWEST_SHOWN);

    let times: Vec<String> = slowest.iter().map(|timing| seconds(timing.duration())).collect();
    let time_width = times.iter().map(String::len).max().unwrap_or(0);
    let kind_width = slowest.iter().map(|timing| kind_name(timing.kind).len()).max().unwrap_or(0);

    ui::print_line(&format!("Slowest actions ({} of {}):", slowest.len(), timings.len()));
    for (timing, time) in slowest.iter().zip(&times) {
        let kind = kind_name(timing.kind);
        let failed = if timing.failed { " (failed)" } else { "" };
        ui::print_line(&format!("  {time:>time_width$}  {kind:<kind_width$}  {}{failed}", what(timing)));
    }

    // Time and count per kind, most time first
    let mut kinds: Vec<(ActionKind, Duration, usize)> = Vec::new();
    for timing in timings {
        match kinds.iter_mut().find(|(kind, _, _)| *kind == timing.kind) {
            Some((_, total, count)) => {
                *total += timing.duration();
                *count += 1;
            }
            None => kinds.push((timing.kind, timing.duration(), 1)),
        }
    }
    kinds.sort_by_key(|(_, total, _)| std::cmp::Reverse(*total));

    let totals: Vec<String> = kinds
        .iter()
        .map(|(kind, total, count)| format!("{} {} ({count})", kind_name(*kind), seconds(*total)))
        .collect();
    ui::print_line(&format!("Per kind: {}", totals.join(", ")));
    ui::print_line(&ui::dim(&format!("Trace: {trace}")));
}

/// The Chrome trace event format: metadata naming the process and each lane, then a complete ("X") event per action
/// with its description, kind, lane and times in microseconds. One event per line.
fn trace_text(timings: &[Timing]) -> String {
    let mut events = vec![json!({"name": "process_name", "ph": "M", "pid": 1, "args": {"name": "anvil build"}})];

    let mut lanes: Vec<usize> = timings.iter().map(|timing| timing.lane).collect();
    lanes.sort_unstable();
    lanes.dedup();
    for lane in lanes {
        events.push(json!({"name": "thread_name", "ph": "M", "pid": 1, "tid": lane, "args": {"name": format!("lane {lane}")}}));
    }

    for timing in timings {
        let mut event = json!({
            "name": timing.description,
            "cat": kind_name(timing.kind),
            "ph": "X",
            "ts": timing.start.as_micros() as u64,
            "dur": timing.duration().as_micros() as u64,
            "pid": 1,
            "tid": timing.lane,
        });
        if timing.failed {
            event["args"] = json!({"failed": true});
        }
        events.push(event);
    }

    let lines: Vec<String> = events.iter().map(|event| event.to_string()).collect();
    format!("{{\"displayTimeUnit\": \"ms\", \"traceEvents\": [\n{}\n]}}\n", lines.join(",\n"))
}

fn kind_name(kind: ActionKind) -> &'static str {
    match kind {
        ActionKind::Compile => "compile",
        ActionKind::Precompile => "precompile",
        ActionKind::Resource => "resource",
        ActionKind::Assemble => "assemble",
        ActionKind::Archive => "archive",
        ActionKind::Link => "link",
        ActionKind::Run => "run",
        ActionKind::Copy => "copy",
    }
}

/// What an action worked on: its description without the verb the kind already says ("Compiling src/a.cpp" is
/// "src/a.cpp"). A build step's description is its own message, kept whole.
fn what(timing: &Timing) -> &str {
    match timing.kind {
        ActionKind::Run => &timing.description,
        _ => timing.description.split_once(' ').map_or(timing.description.as_str(), |(_, rest)| rest),
    }
}

fn seconds(duration: Duration) -> String {
    format!("{:.2}s", duration.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timing(description: &str, kind: ActionKind, lane: usize, start_ms: u64, end_ms: u64) -> Timing {
        Timing {
            description: description.to_string(),
            kind,
            lane,
            start: Duration::from_millis(start_ms),
            end: Duration::from_millis(end_ms),
            failed: false,
        }
    }

    #[test]
    fn trace_is_chrome_trace_json() {
        let mut failed = timing("Compiling src/\"quoted\".cpp", ActionKind::Compile, 2, 5, 30);
        failed.failed = true;
        let timings = vec![
            timing("Compiling src/a.cpp", ActionKind::Compile, 1, 0, 1500),
            failed,
            timing("Linking app.exe", ActionKind::Link, 1, 1500, 1700),
        ];

        let trace: serde_json::Value = serde_json::from_str(&trace_text(&timings)).unwrap();
        let events = trace["traceEvents"].as_array().unwrap();
        let complete: Vec<&serde_json::Value> = events.iter().filter(|event| event["ph"] == "X").collect();
        assert_eq!(complete.len(), 3);

        assert_eq!(complete[0]["name"], "Compiling src/a.cpp");
        assert_eq!(complete[0]["cat"], "compile");
        assert_eq!(complete[0]["ts"], 0);
        assert_eq!(complete[0]["dur"], 1_500_000);
        assert_eq!(complete[0]["tid"], 1);
        assert_eq!(complete[1]["name"], "Compiling src/\"quoted\".cpp");
        assert_eq!(complete[1]["args"]["failed"], true);
        assert_eq!(complete[2]["cat"], "link");
        assert_eq!(complete[2]["ts"], 1_500_000);

        // A name per lane that ran something
        let lanes = events.iter().filter(|event| event["name"] == "thread_name").count();
        assert_eq!(lanes, 2);
    }

    #[test]
    fn what_drops_the_verb() {
        assert_eq!(what(&timing("Compiling src/a b.cpp", ActionKind::Compile, 1, 0, 1)), "src/a b.cpp");
        assert_eq!(what(&timing("Generating shaders", ActionKind::Run, 1, 0, 1)), "Generating shaders");
    }
}
