//! Runs a plan: decides what is out of date from the build database (command hashes and input stamps, with
//! content hashes so a touched-but-unchanged file rebuilds nothing), runs the rest in parallel, and records what
//! each action read.

use crate::db::{Database, Record, Stamp};
use crate::depfile;
use crate::flags::DepsFormat;
use crate::plan::{Action, ActionKind, Plan, ResponseStyle};
use crate::toolchain::Toolchain;
use crate::ui;
use crate::util::{self, slash};
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

// Command lines longer than this go through a response file (Windows allows 32767 characters)
const RESPONSE_FILE_THRESHOLD: usize = 8000;

pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone)]
pub struct ExecOptions {
    pub jobs: usize,
    pub keep_going: bool,
    pub verbose: bool,
    pub explain: bool,
    pub low_priority: bool,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub ran: usize,
    pub up_to_date: usize,
    pub failed: usize,
    pub compiled: usize,
    pub linked: usize,
    pub elapsed: Duration,
}

// Two seconds in file-time ticks (100 ns): inputs this close to an incremental link's last run defeat its time check
const INCREMENTAL_TIME_GRAIN: i64 = 20_000_000;

// A folder with at least this many files to check is listed rather than stat'ed file by file
const LISTED_FOLDER_MIN_FILES: usize = 8;

/// What a worker tells the scheduler: its process ended, then the action's result.
enum Report {
    ProcessDone,
    Finished(Outcome),
}

/// What a worker reports back for one action.
struct Outcome {
    action: usize,
    success: bool,
    output: String,
    discovered: Vec<String>,
    started: i64,
    error: Option<String>,
    // Every input it read with its time stamp and content hash, measured by the worker
    measured: Vec<(PathBuf, i64, u64)>,
}

/// Time stamps and content hashes, shared by the scheduler and the workers.
struct Stamps {
    mtimes: Mutex<HashMap<String, Option<i64>>>,
    hashes: Mutex<HashMap<(String, i64), u64>>,
}

struct Executor<'a> {
    plan: &'a Plan,
    database: Database,
    stamps: Arc<Stamps>,
    toolchain_identity: u64,
    system_dirs: Vec<String>,
    options: ExecOptions,
}

impl Stamps {
    fn mtime(&self, path: &Path) -> Option<i64> {
        let key = util::path_key(path);
        if let Some(known) = self.mtimes.lock().unwrap().get(&key) {
            return *known;
        }

        let mtime = util::mtime(path);
        self.mtimes.lock().unwrap().insert(key, mtime);
        mtime
    }

    fn forget(&self, path: &Path) {
        self.mtimes.lock().unwrap().remove(&util::path_key(path));
    }

    /// Content hash of a file at a time stamp (each file is hashed once per change).
    fn hash(&self, path: &Path, mtime: i64) -> u64 {
        let key = (util::path_key(path), mtime);
        if let Some(hash) = self.hashes.lock().unwrap().get(&key) {
            return *hash;
        }

        let hash = util::hash_file(path).unwrap_or(0);
        self.hashes.lock().unwrap().insert(key, hash);
        hash
    }
}

/// Builds every action in the plan that is out of date. `database_dir` holds the build database.
pub fn execute(plan: &Plan, toolchain: &Toolchain, database_dir: &Path, options: &ExecOptions) -> Result<Summary> {
    let started = Instant::now();
    let database = Database::open(database_dir)?;

    let stamps = Arc::new(Stamps {
        mtimes: Mutex::new(HashMap::new()),
        hashes: Mutex::new(HashMap::new()),
    });
    let mut executor = Executor {
        plan,
        database,
        stamps,
        toolchain_identity: toolchain.identity(),
        system_dirs: toolchain.system_dirs.clone(),
        options: options.clone(),
    };

    executor.warm_caches();
    let mut summary = executor.run()?;
    executor.database.finish()?;

    summary.elapsed = started.elapsed();
    Ok(summary)
}

impl Executor<'_> {
    /// Stats every recorded input in parallel and seeds the hash cache from the database.
    fn warm_caches(&mut self) {
        let mut paths: Vec<String> = Vec::new();
        for action in &self.plan.actions {
            paths.extend(action.outputs.iter().map(|output| output.to_string_lossy().into_owned()));
            if let Some(record) = self.database.get(&action.outputs[0]) {
                paths.extend(record.inputs.iter().map(|stamp| self.database.path(stamp.path).to_string()));
            }
        }
        paths.sort();
        paths.dedup();

        // Folders holding many of them are listed once; the rest are stat'ed one by one
        let mut by_folder: HashMap<PathBuf, Vec<&String>> = HashMap::new();
        for path in &paths {
            let folder = Path::new(path).parent().map(Path::to_path_buf).unwrap_or_default();
            by_folder.entry(folder).or_default().push(path);
        }

        let stats: Vec<(String, Option<i64>)> = by_folder
            .par_iter()
            .flat_map_iter(|(folder, files)| folder_stats(folder, files))
            .collect();
        self.stamps.mtimes.lock().unwrap().extend(stats);

        let mut hashes = self.stamps.hashes.lock().unwrap();
        for (path, mtime, hash) in self.database.known_stamps() {
            if hash != 0 {
                hashes.insert((util::path_key(Path::new(path)), mtime), hash);
            }
        }
    }

    fn run(&mut self) -> Result<Summary> {
        let actions = &self.plan.actions;
        let count = actions.len();

        // The graph: who waits for whom
        let mut waiting_on = vec![0usize; count];
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); count];
        for (index, action) in actions.iter().enumerate() {
            for before in &action.after {
                waiting_on[index] += 1;
                dependents[*before].push(index);
            }
        }

        // A first estimate of the work, for the progress counter; until something runs, its answers stay true
        let mut dirty = vec![false; count];
        let mut estimates: Vec<Option<Option<String>>> = vec![None; count];
        for index in topological_order(actions) {
            let reason = self.out_of_date(index);
            dirty[index] = actions[index].after.iter().any(|before| dirty[*before]) || reason.is_some();
            estimates[index] = Some(reason);
        }
        let total = dirty.iter().filter(|dirty| **dirty).count();

        // Workers: twice as many as jobs, so one still stamping what its process read never holds a job back
        let jobs_allowed = self.options.jobs.max(1);
        let (job_sender, job_receiver) = mpsc::channel::<(usize, Action)>();
        let (report_sender, report_receiver) = mpsc::channel::<Report>();
        let job_receiver = Arc::new(Mutex::new(job_receiver));
        let mut workers = Vec::new();
        for _ in 0..jobs_allowed * 2 {
            let jobs = job_receiver.clone();
            let reports = report_sender.clone();
            let low_priority = self.options.low_priority;
            let stamps = self.stamps.clone();
            let system_dirs = self.system_dirs.clone();

            // Inputs are stamped here rather than on the scheduler, which would otherwise stall while every core compiles;
            // the job slot is given back first
            workers.push(std::thread::spawn(move || {
                raise_thread_priority();

                loop {
                    let job = jobs.lock().unwrap().recv();
                    let Ok((index, action)) = job else { break };

                    let mut outcome = run_action(index, &action, low_priority);
                    let _ = reports.send(Report::ProcessDone);

                    if outcome.success {
                        outcome.measured = measure_inputs(&action, &outcome, &stamps, &system_dirs);
                        hash_objects(&action, &stamps);
                    }
                    let _ = reports.send(Report::Finished(outcome));
                }
            }));
        }
        drop(report_sender);

        raise_thread_priority();

        // Schedule
        let mut summary = Summary::default();
        let mut ready: VecDeque<usize> = (0..count).filter(|index| waiting_on[*index] == 0).collect();
        let mut blocked = vec![false; count];
        let mut running = 0usize;
        let mut processes = 0usize;
        let mut started = 0usize;
        let mut finished = 0usize;
        let mut stop = false;

        while finished < count {
            // Start what we can
            while !stop && processes < jobs_allowed {
                let Some(index) = ready.pop_front() else { break };

                if blocked[index] {
                    finished += 1;
                    for dependent in &dependents[index] {
                        blocked[*dependent] = true;
                        waiting_on[*dependent] -= 1;
                        if waiting_on[*dependent] == 0 {
                            ready.push_back(*dependent);
                        }
                    }
                    continue;
                }

                let reason = match estimates[index].take() {
                    Some(estimate) if summary.ran == 0 && running == 0 => estimate,
                    _ => self.out_of_date(index),
                };
                match reason {
                    None => {
                        summary.up_to_date += 1;
                        finished += 1;
                        release(index, &dependents, &mut waiting_on, &mut ready);
                    }
                    Some(reason) => {
                        let action = &actions[index];
                        if self.options.explain {
                            ui::print_line(&ui::dim(&format!("  {}: {reason}", action.description)));
                        }
                        if self.options.verbose {
                            ui::print_line(&ui::dim(&command_line(action)));
                        }

                        started += 1;
                        ui::progress(started, total.max(started), &action.description);
                        if !ui::interactive() {
                            ui::print_line(&format!("[{started}/{}] {}", total.max(started), action.description));
                        }

                        job_sender.send((index, action.clone())).context("worker pool stopped")?;
                        running += 1;
                        processes += 1;
                    }
                }
            }

            if finished >= count {
                break;
            }

            // Everything left is blocked behind a failure or an interruption
            if running == 0 && (ready.is_empty() || stop) {
                break;
            }
            if running == 0 {
                continue;
            }

            // Wait for a process to end (its slot is free), or for its result
            let outcome = match report_receiver.recv().context("worker pool stopped")? {
                Report::ProcessDone => {
                    processes -= 1;
                    continue;
                }
                Report::Finished(outcome) => outcome,
            };
            running -= 1;
            finished += 1;
            let index = outcome.action;

            if outcome.success {
                self.record(&outcome)?;
                summary.ran += 1;
                match actions[index].kind {
                    ActionKind::Compile | ActionKind::Precompile | ActionKind::Resource | ActionKind::Assemble => summary.compiled += 1,
                    ActionKind::Link | ActionKind::Archive => summary.linked += 1,
                    _ => {}
                }
                if !outcome.output.trim().is_empty() {
                    ui::print_block(&outcome.output);
                }
                release(index, &dependents, &mut waiting_on, &mut ready);
            } else {
                summary.failed += 1;
                let message = outcome.error.clone().unwrap_or_else(|| format!("{} failed", actions[index].description));
                ui::error(&message);
                if !outcome.output.trim().is_empty() {
                    ui::print_block(&outcome.output);
                }

                // Its dependents can never run
                for dependent in &dependents[index] {
                    blocked[*dependent] = true;
                    waiting_on[*dependent] -= 1;
                    if waiting_on[*dependent] == 0 {
                        ready.push_back(*dependent);
                    }
                }
                if !self.options.keep_going {
                    stop = true;
                }
            }

            if INTERRUPTED.load(Ordering::Relaxed) {
                stop = true;
            }
        }

        // Let the workers go
        drop(job_sender);
        for worker in workers {
            let _ = worker.join();
        }
        ui::clear_progress();

        if INTERRUPTED.load(Ordering::Relaxed) {
            self.database.finish()?;
            bail!("interrupted");
        }

        Ok(summary)
    }

    /// Why an action must run, or None when its outputs are up to date.
    fn out_of_date(&mut self, index: usize) -> Option<String> {
        let action = &self.plan.actions[index];

        for output in &action.outputs {
            if self.stamps.mtime(output).is_none() {
                return Some(format!("{} does not exist", file_name(output)));
            }
        }

        let Some(record) = self.database.get(&action.outputs[0]) else {
            return Some("never built".into());
        };
        if record.command_hash != action.command_hash(self.toolchain_identity) {
            return Some("its command changed".into());
        }

        // Every input the last build read
        let record = record.clone();
        for stamp in &record.inputs {
            let path = PathBuf::from(self.database.path(stamp.path));
            let Some(mtime) = self.stamps.mtime(&path) else {
                return Some(format!("{} was deleted", slash(&path)));
            };
            if mtime == stamp.mtime {
                continue;
            }

            // Touched: only a content change counts
            if stamp.hash == 0 || self.stamps.hash(&path, mtime) != stamp.hash {
                return Some(format!("{} changed", slash(&path)));
            }
        }

        None
    }

    /// Stores what a successful action read, so the next build can tell whether it is stale.
    fn record(&mut self, outcome: &Outcome) -> Result<()> {
        let action = &self.plan.actions[outcome.action];

        // Outputs changed on disk
        for output in &action.outputs {
            self.stamps.forget(output);
        }

        let mut inputs = Vec::with_capacity(outcome.measured.len());
        for (path, mtime, hash) in &outcome.measured {
            let id = self.database.intern(&path.to_string_lossy());
            inputs.push(Stamp {
                path: id,
                mtime: *mtime,
                hash: *hash,
            });
        }

        let record = Record {
            command_hash: action.command_hash(self.toolchain_identity),
            inputs,
        };
        self.database.put(&action.outputs[0], record);

        Ok(())
    }
}

/// Runs one action on a worker thread.
fn run_action(index: usize, action: &Action, low_priority: bool) -> Outcome {
    let started = util::now_ticks();
    let mut outcome = Outcome {
        action: index,
        success: false,
        output: String::new(),
        discovered: Vec::new(),
        started,
        error: None,
        measured: Vec::new(),
    };

    if let Err(error) = prepare_outputs(action) {
        outcome.error = Some(format!("{}: {error:#}", action.description));
        return outcome;
    }

    // Copies need no process
    if let Some(source) = &action.copy_from {
        match copy_file(source, &action.outputs[0]) {
            Ok(()) => outcome.success = true,
            Err(error) => outcome.error = Some(format!("{}: {error:#}", action.description)),
        }
        return outcome;
    }

    let mut command = match build_command(action, low_priority) {
        Ok(command) => command,
        Err(error) => {
            outcome.error = Some(format!("{}: {error:#}", action.description));
            return outcome;
        }
    };

    let output = match command.output() {
        Ok(output) => output,
        Err(error) => {
            outcome.error = Some(format!("{}: could not start {}: {error}", action.description, action.program.display()));
            return outcome;
        }
    };

    // Collect the output, minus the noise
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if action.deps == DepsFormat::ShowIncludes {
        let (headers, rest) = depfile::split_show_includes(&text);
        outcome.discovered = headers;
        text = rest;
    }
    outcome.output = clean_output(&text, action.echo_line.as_deref());
    outcome.success = output.status.success();

    if !outcome.success {
        return outcome;
    }

    // Stamps mark that a pre-build or post-build command ran
    if action.stamp {
        for stamp in &action.outputs {
            if let Err(error) = std::fs::write(stamp, b"") {
                outcome.success = false;
                outcome.error = Some(format!("{}: writing {}: {error}", action.description, stamp.display()));
                return outcome;
            }
        }
    }

    // Files the action promised to create
    for (file, message) in &action.must_create {
        if !file.exists() {
            outcome.success = false;
            outcome.error = Some(message.clone());
            return outcome;
        }
    }

    // Headers it read
    match &action.deps {
        DepsFormat::Make(depfile_path) => {
            let text = std::fs::read_to_string(depfile_path).unwrap_or_default();
            outcome.discovered = depfile::parse_make(&text);
        }
        DepsFormat::MsvcJson(json_path) => {
            let text = std::fs::read_to_string(json_path).unwrap_or_default();
            outcome.discovered = depfile::parse_msvc_json(&text);
        }
        DepsFormat::ShowIncludes | DepsFormat::None => {}
    }

    outcome
}

/// Time stamps of files in one folder, keyed by `path_key`: from one listing of the folder when it holds many of them.
fn folder_stats(folder: &Path, files: &[&String]) -> Vec<(String, Option<i64>)> {
    let listing = if files.len() >= LISTED_FOLDER_MIN_FILES {
        util::folder_mtimes(folder)
    } else {
        None
    };

    let mut stats = Vec::with_capacity(files.len());
    for file in files {
        let path = Path::new(file.as_str());
        let mtime = match &listing {
            Some(entries) => {
                let name = path.file_name().map(|name| name.to_string_lossy().to_lowercase()).unwrap_or_default();
                entries.get(&name).copied()
            }
            None => util::mtime(path),
        };
        stats.push((util::path_key(path), mtime));
    }

    stats
}

/// What a successful action read: its declared inputs, then the headers it discovered (minus the toolchain's own),
/// each with its time stamp and content hash. A file changed since the action started gets hash 0, which never
/// matches, because the action may have read an older version.
fn measure_inputs(action: &Action, outcome: &Outcome, stamps: &Stamps, system_dirs: &[String]) -> Vec<(PathBuf, i64, u64)> {
    let mut paths: Vec<PathBuf> = action.inputs.clone();
    for discovered in &outcome.discovered {
        let key = util::path_key(Path::new(discovered));
        if !system_dirs.iter().any(|prefix| key.starts_with(prefix.as_str())) {
            paths.push(PathBuf::from(discovered));
        }
    }
    paths.sort_by_key(|path| util::path_key(path));
    paths.dedup_by_key(|path| util::path_key(path));

    paths
        .into_iter()
        .filter_map(|path| {
            let mtime = stamps.mtime(&path)?;
            let hash = if mtime > outcome.started { 0 } else { stamps.hash(&path, mtime) };
            Some((path, mtime, hash))
        })
        .collect()
}

/// Hashes the objects a compile wrote, while other workers still compile: an archive or link reading hundreds of
/// them then finds every hash cached instead of reading them one by one at the end of the build.
fn hash_objects(action: &Action, stamps: &Stamps) {
    if !matches!(action.kind, ActionKind::Compile | ActionKind::Assemble | ActionKind::Resource) {
        return;
    }

    for output in &action.outputs {
        if let Some(mtime) = util::mtime(output) {
            stamps.hash(output, mtime);
        }
    }
}

fn build_command(action: &Action, low_priority: bool) -> Result<Command> {
    let mut command = Command::new(&action.program);
    command.envs(action.env.iter().cloned());
    if let Some(workdir) = &action.workdir {
        command.current_dir(workdir);
    }

    // Long command lines go through a response file
    let length: usize = action.args.iter().map(|argument| argument.len() + 3).sum();
    if action.response != ResponseStyle::Never && length > RESPONSE_FILE_THRESHOLD {
        let response_file = PathBuf::from(format!("{}.rsp", action.outputs[0].display()));
        let content: Vec<String> = action.args.iter().map(|argument| quote_response(argument, action.response)).collect();
        std::fs::write(&response_file, content.join("\n")).context("writing a response file")?;
        command.arg(format!("@{}", response_file.display()));
    } else if action.raw_args {
        add_raw_args(&mut command, &action.args);
    } else {
        command.args(&action.args);
    }

    set_priority(&mut command, low_priority);
    Ok(command)
}

/// Arguments exactly as written: cmd.exe reads its own command line, not as programs split it.
#[cfg(windows)]
fn add_raw_args(command: &mut Command, args: &[String]) {
    use std::os::windows::process::CommandExt;

    for argument in args {
        command.raw_arg(argument);
    }
}

#[cfg(not(windows))]
fn add_raw_args(command: &mut Command, args: &[String]) {
    command.args(args);
}

/// Above-normal priority for Anvil's own threads: they sleep almost always, and when one wakes it should not wait
/// behind a core full of compilers before handing out the next job.
#[cfg(windows)]
fn raise_thread_priority() {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};

    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
}

#[cfg(not(windows))]
fn raise_thread_priority() {}

#[cfg(windows)]
fn set_priority(command: &mut Command, low_priority: bool) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Console::GetConsoleCP;
    use windows_sys::Win32::System::Threading::{BELOW_NORMAL_PRIORITY_CLASS, CREATE_NO_WINDOW};

    // Children share our console, even a hidden one (a terminal or tool that starts us without a window). Only with
    // none at all (started from an IDE) would each pop up a window of its own; CREATE_NO_WINDOW prevents that, but
    // gives every child a console of its own, a third slower per compile
    static HAS_CONSOLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let has_console = *HAS_CONSOLE.get_or_init(|| unsafe { GetConsoleCP() } != 0);

    let mut flags = if has_console { 0 } else { CREATE_NO_WINDOW };
    if low_priority {
        flags |= BELOW_NORMAL_PRIORITY_CLASS;
    }
    command.creation_flags(flags);
}

#[cfg(not(windows))]
fn set_priority(command: &mut Command, low_priority: bool) {
    use std::os::unix::process::CommandExt;

    if low_priority {
        unsafe {
            command.pre_exec(|| {
                libc::nice(10);
                Ok(())
            });
        }
    }
}

/// Quotes one argument for a response file (cl/link rules, or gcc's).
fn quote_response(argument: &str, style: ResponseStyle) -> String {
    let needs_quotes = argument.is_empty() || argument.contains([' ', '\t', '"']) || (style == ResponseStyle::Gnu && argument.contains(['\\', '\'']));
    if !needs_quotes {
        return argument.to_string();
    }

    let mut quoted = String::from("\"");
    match style {
        ResponseStyle::Gnu => {
            for character in argument.chars() {
                if character == '"' || character == '\\' {
                    quoted.push('\\');
                }
                quoted.push(character);
            }
        }
        // CommandLineToArgv rules: backslashes double only before a quote
        _ => {
            let mut backslashes = 0;
            for character in argument.chars() {
                match character {
                    '\\' => backslashes += 1,
                    '"' => {
                        // The N backslashes are already out; N more plus one escape the quote
                        quoted.push_str(&"\\".repeat(backslashes + 1));
                        backslashes = 0;
                    }
                    _ => backslashes = 0,
                }
                if character != '"' {
                    quoted.push(character);
                } else {
                    quoted.push('"');
                }
            }
            quoted.push_str(&"\\".repeat(backslashes));
        }
    }
    quoted.push('"');

    quoted
}

/// Removes tool noise: cl's echo of the file name, link's "Creating library" note.
fn clean_output(text: &str, echo_line: Option<&str>) -> String {
    let mut cleaned = String::new();

    for line in text.lines() {
        let trimmed = line.trim();
        let is_echo =
            echo_line.is_some_and(|echo| trimmed.eq_ignore_ascii_case(echo)) || (trimmed.starts_with("tmpxft_") && trimmed.ends_with(".cudafe1.cpp"));
        let is_link_chatter = trimmed.starts_with("LINK : ") && trimmed.ends_with("performing full link");
        let is_noise = trimmed.starts_with("Creating library ") || is_link_chatter || trimmed.is_empty();
        if is_echo || is_noise {
            continue;
        }

        cleaned.push_str(line);
        cleaned.push('\n');
    }

    cleaned
}

/// Creates output folders and deletes outputs that must not be appended to.
fn prepare_outputs(action: &Action) -> Result<()> {
    guard_incremental_link(action);

    for output in &action.outputs {
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        if action.remove_outputs_first {
            let _ = std::fs::remove_file(output);
        }
    }

    if let DepsFormat::Make(depfile_path) | DepsFormat::MsvcJson(depfile_path) = &action.deps {
        if let Some(parent) = depfile_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(depfile_path);
    }

    Ok(())
}

/// link.exe's incremental mode remembers its inputs' times in whole seconds, so an input rewritten within a second
/// of the last link can look unchanged and its new code be left out. Such a link starts over instead.
fn guard_incremental_link(action: &Action) {
    let Some(state) = &action.incremental_state else { return };
    let Some(last_link) = action.outputs.first().and_then(|output| util::mtime(output)) else {
        return;
    };

    let recently_changed = action
        .inputs
        .iter()
        .filter_map(|input| util::mtime(input))
        .any(|changed| changed > last_link - INCREMENTAL_TIME_GRAIN && changed < last_link + INCREMENTAL_TIME_GRAIN);
    if recently_changed {
        let _ = std::fs::remove_file(state);
    }
}

/// Copies through a temporary file, so a running program holding the old DLL never sees half a file.
fn copy_file(source: &Path, destination: &Path) -> Result<()> {
    let temporary = destination.with_extension("anvil-tmp");
    std::fs::copy(source, &temporary).with_context(|| format!("copying {}", source.display()))?;

    if std::fs::rename(&temporary, destination).is_err() {
        let _ = std::fs::remove_file(&temporary);
        bail!("{} is in use (is the program still running?)", destination.display());
    }

    Ok(())
}

/// Marks an action done for its dependents. Those it frees go to the front of the queue, so archives and links
/// overlap the remaining compiles instead of waiting behind all of them.
fn release(index: usize, dependents: &[Vec<usize>], waiting_on: &mut [usize], ready: &mut VecDeque<usize>) {
    for dependent in &dependents[index] {
        waiting_on[*dependent] -= 1;
        if waiting_on[*dependent] == 0 {
            ready.push_front(*dependent);
        }
    }
}

/// Actions ordered so each comes after everything in its `after` list.
fn topological_order(actions: &[Action]) -> Vec<usize> {
    let mut order = Vec::with_capacity(actions.len());
    let mut state = vec![0u8; actions.len()];

    fn visit(actions: &[Action], index: usize, state: &mut Vec<u8>, order: &mut Vec<usize>) {
        if state[index] != 0 {
            return;
        }
        state[index] = 1;

        for before in &actions[index].after {
            visit(actions, *before, state, order);
        }
        order.push(index);
    }

    for index in 0..actions.len() {
        visit(actions, index, &mut state, &mut order);
    }

    order
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| slash(path))
}

/// A printable command line (for --verbose).
pub fn command_line(action: &Action) -> String {
    if let Some(source) = &action.copy_from {
        return format!("copy {} {}", source.display(), action.outputs[0].display());
    }

    let mut line = quote_for_display(&action.program.to_string_lossy());
    for argument in &action.args {
        line.push(' ');
        if action.raw_args {
            line.push_str(argument);
        } else {
            line.push_str(&quote_for_display(argument));
        }
    }
    line
}

fn quote_for_display(text: &str) -> String {
    if text.contains(' ') {
        format!("\"{text}\"")
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_quoting() {
        assert_eq!(quote_response("/IC:/a b", ResponseStyle::Msvc), "\"/IC:/a b\"");
        assert_eq!(quote_response("plain", ResponseStyle::Msvc), "plain");
        assert_eq!(quote_response("C:\\x y\\", ResponseStyle::Msvc), "\"C:\\x y\\\\\"");
        assert_eq!(quote_response("C:\\x", ResponseStyle::Gnu), "\"C:\\\\x\"");
    }

    #[test]
    fn cleans_noise() {
        let text = "main.cpp\nmain.cpp(3): warning C4100\n   Creating library x.lib and object x.exp\n";
        assert_eq!(clean_output(text, Some("main.cpp")), "main.cpp(3): warning C4100\n");
    }
}
