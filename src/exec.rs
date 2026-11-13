//! Runs a plan: decides what is out of date from the build database (command hashes, output time stamps and input
//! stamps, with content hashes so a touched-but-unchanged file rebuilds nothing), runs the rest in parallel, and
//! records what each action read.

use crate::db::{Database, Record, Stamp, NO_TIME_STAMP};
use crate::depfile;
use crate::flags::DepsFormat;
use crate::plan::{Action, ActionKind, Plan, ResponseStyle};
use crate::timings::{self, Timing};
use crate::toolchain::Toolchain;
use crate::ui;
use crate::util;
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

// Command lines longer than this go through a response file (Windows allows 32767 characters)
const RESPONSE_FILE_THRESHOLD: usize = 8000;

// The longest command line Windows starts a process with, and the error (ERROR_FILENAME_EXCED_RANGE) past it
const WINDOWS_COMMAND_LINE_LIMIT: usize = 32767;
const ERROR_COMMAND_LINE_TOO_LONG: i32 = 206;

// A compiler cache reads a response file and starts the compiler with the whole line (sccache does): a compile whose
// arguments are longer than this runs without it, on Windows
const LAUNCHER_LINE_LIMIT: usize = 30_000;

// What gcc and clang color their diagnostics with (added when the process starts, see Action::color_diagnostics)
const COLOR_DIAGNOSTICS: &str = "-fdiagnostics-color=always";

// How many changed arguments --explain shows on each side
const EXPLAIN_ARGUMENTS: usize = 3;

// The hash recorded for an input that could not be read: only its time stamp counts (0 means it changed mid-run)
const UNREADABLE_HASH: u64 = 1;

pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone)]
pub struct ExecOptions {
    pub jobs: usize,
    pub keep_going: bool,
    pub verbose: bool,
    pub explain: bool,
    pub low_priority: bool,

    /// The project's root: --explain shows paths relative to it.
    pub root: PathBuf,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub ran: usize,
    pub up_to_date: usize,
    pub failed: usize,
    pub compiled: usize,
    pub linked: usize,
    pub elapsed: Duration,

    /// The actions that ran, with their times and lanes, in the order they finished.
    pub timings: Vec<Timing>,
}

// Two seconds in file-time ticks (100 ns): inputs this close to an incremental link's last run defeat its time check
const INCREMENTAL_TIME_GRAIN: i64 = 20_000_000;

// A folder with at least this many files to check is listed rather than stat'ed file by file
const LISTED_FOLDER_MIN_FILES: usize = 8;

/// What a worker tells the scheduler: its process ended (its lane is free), then the action's result.
enum Report {
    ProcessDone(usize),
    Finished(Outcome, Timing),
}

/// What a worker reports back for one action.
struct Outcome {
    action: usize,
    success: bool,
    output: String,
    discovered: Vec<String>,
    started: i64,
    error: Option<String>,

    // Every input it read, and each output's time stamp (None: missing), measured by the worker
    measured: Vec<Measured>,
    output_mtimes: Vec<Option<i64>>,
}

/// One file an action read, as the worker found it once the action finished.
struct Measured {
    key: String,
    path: PathBuf,
    mtime: i64,
    hash: u64,

    // It changed while the action ran (its hash is 0 unless the content stayed the same)
    changed_while_running: bool,
}

/// A file an action read on its last run, as this build last saw it when the action was handed out: its time
/// stamp, and its content hash when the record's stamp still holds (0: not known).
struct Known {
    path: String,
    mtime: i64,
    hash: u64,
}

/// Time stamps and content hashes by `util::path_key`, shared by the scheduler and the workers.
struct Stamps {
    mtimes: Mutex<HashMap<String, Option<i64>>>,

    // Each file's content hash, with the time stamp it was hashed at
    hashes: Mutex<HashMap<String, (i64, u64)>>,
}

struct Executor<'a> {
    plan: &'a Plan,
    database: Database,
    stamps: Arc<Stamps>,
    toolchain_identity: u64,
    system_dirs: Vec<String>,
    options: ExecOptions,

    // Each action's outputs as database path ids
    output_ids: Vec<Vec<u32>>,

    // By database path id: the time stamp this build found (None: missing), and whether an action makes the file
    disk_mtimes: Vec<Option<i64>>,
    produced: Vec<bool>,

    // Actions that ran, and files the build makes that a later action rewrote in place
    ran: Vec<usize>,
    rewritten: Vec<u32>,

    // When the build started, and the times of the actions that ran (failed ones too)
    started: Instant,
    timings: Vec<Timing>,
}

impl Stamps {
    fn mtime(&self, key: &str, path: &Path) -> Option<i64> {
        if let Some(known) = self.mtimes.lock().unwrap().get(key) {
            return *known;
        }

        let mtime = util::mtime(path);
        self.mtimes.lock().unwrap().insert(key.to_string(), mtime);
        mtime
    }

    fn set(&self, key: &str, mtime: Option<i64>) {
        self.mtimes.lock().unwrap().insert(key.to_string(), mtime);
    }

    fn forget(&self, path: &Path) {
        self.mtimes.lock().unwrap().remove(&util::path_key(path));
    }

    /// Content hash of a file at a time stamp (each file is hashed once per change); 0 when it changes while read.
    fn hash(&self, key: &str, path: &Path, mtime: i64) -> u64 {
        if let Some((hashed_at, hash)) = self.hashes.lock().unwrap().get(key) {
            if *hashed_at == mtime {
                return *hash;
            }
        }

        // Rewritten while being read: no hash belongs to that time stamp
        let hash = util::hash_file(path).unwrap_or(0);
        if util::mtime(path) != Some(mtime) {
            return 0;
        }

        self.hashes.lock().unwrap().insert(key.to_string(), (mtime, hash));
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
        output_ids: Vec::new(),
        disk_mtimes: Vec::new(),
        produced: Vec::new(),
        ran: Vec::new(),
        rewritten: Vec::new(),
        started,
        timings: Vec::new(),
    };

    executor.stat_known_files();
    let result = executor.run();

    // The trace of what ran, also when the build failed or was interrupted; a build that ran nothing keeps the last
    if !executor.timings.is_empty() {
        if let Err(error) = timings::write_trace(&database_dir.join(timings::TRACE_FILE), &executor.timings) {
            ui::warn(&format!("could not write the build trace: {error:#}"));
        }
    }

    let mut summary = result?;
    executor.database.finish()?;

    summary.elapsed = started.elapsed();
    summary.timings = std::mem::take(&mut executor.timings);
    Ok(summary)
}

impl Executor<'_> {
    /// Stats every output and every recorded input once, in parallel, into a table by path id.
    fn stat_known_files(&mut self) {
        let plan = self.plan;

        // Outputs get ids too, so records, stamps and outputs share one table
        self.output_ids = plan
            .actions
            .iter()
            .map(|action| {
                action
                    .outputs
                    .iter()
                    .map(|output| self.database.intern(&output.to_string_lossy()))
                    .collect()
            })
            .collect();

        // The files this build checks: outputs, and what each action's last run read
        let count = self.database.path_count();
        let mut wanted = vec![false; count];
        self.produced = vec![false; count];
        for output_ids in &self.output_ids {
            for id in output_ids {
                wanted[*id as usize] = true;
                self.produced[*id as usize] = true;
            }
            if let Some(record) = self.database.record(output_ids[0]) {
                for stamp in &record.inputs {
                    wanted[stamp.path as usize] = true;
                }
            }
        }

        // Folders holding many of them are listed once; the rest are stat'ed one by one
        let mut by_folder: HashMap<&Path, Vec<(u32, &Path)>> = HashMap::new();
        for id in (0..count).filter(|id| wanted[*id]) {
            let path = Path::new(self.database.path(id as u32));
            by_folder
                .entry(path.parent().unwrap_or(Path::new("")))
                .or_default()
                .push((id as u32, path));
        }

        let stats: Vec<(u32, Option<i64>)> = by_folder
            .par_iter()
            .flat_map_iter(|(folder, files)| folder_stats(folder, files))
            .collect();
        self.disk_mtimes = vec![None; count];
        for (id, mtime) in stats {
            self.disk_mtimes[id as usize] = mtime;
        }
    }

    fn run(&mut self) -> Result<Summary> {
        let plan = self.plan;
        let actions = &plan.actions;
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

        let jobs_allowed = self.options.jobs.max(1);
        let (job_sender, job_receiver) = mpsc::channel::<(usize, usize, Vec<Known>)>();
        let (report_sender, report_receiver) = mpsc::channel::<Report>();
        let job_receiver = Mutex::new(job_receiver);
        let build_started = self.started;

        let summary = std::thread::scope(|scope| -> Result<Summary> {
            // Dropped on any return, which lets the workers go
            let job_sender = job_sender;

            // Workers: twice as many as jobs, so one still stamping what its process read never holds a job back
            for _ in 0..jobs_allowed * 2 {
                let jobs = &job_receiver;
                let reports = report_sender.clone();
                let low_priority = self.options.low_priority;
                let stamps = self.stamps.clone();
                let system_dirs = self.system_dirs.clone();

                // Inputs are stamped here rather than on the scheduler, which would otherwise stall while every core
                // compiles; the job slot is given back first
                scope.spawn(move || {
                    raise_thread_priority();

                    loop {
                        let job = jobs.lock().unwrap().recv();
                        let Ok((index, lane, known)) = job else { break };
                        let action = &actions[index];
                        let before = stamp_before(action, known, &stamps);

                        let process_started = build_started.elapsed();
                        let mut outcome = run_action(index, action, low_priority);
                        let timing = Timing {
                            description: action.description.clone(),
                            kind: action.kind,
                            lane,
                            start: process_started,
                            end: build_started.elapsed(),
                            failed: !outcome.success,
                        };
                        let _ = reports.send(Report::ProcessDone(lane));

                        if outcome.success {
                            outcome.output_mtimes = action.outputs.iter().map(|output| util::mtime(output)).collect();
                            outcome.measured = measure_inputs(action, &outcome, &before, &stamps, &system_dirs);
                            hash_objects(action, &outcome.output_mtimes, &stamps);
                        }
                        let _ = reports.send(Report::Finished(outcome, timing));
                    }
                });
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

            // Which job lanes have a process: each action runs on the lowest free one, for the trace
            let mut lanes_busy = vec![false; jobs_allowed];

            while finished < count {
                // Start what we can
                while !stop && processes < jobs_allowed {
                    let Some(index) = ready.pop_front() else { break };

                    if blocked[index] {
                        finished += 1;
                        block_dependents(index, &dependents, &mut blocked, &mut waiting_on, &mut ready);
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
                                if let (Some(launcher), None) = (&action.launcher, launcher_for(action)) {
                                    let name = launcher.file_stem().unwrap_or_default().to_string_lossy();
                                    ui::print_line(&ui::dim(&format!(
                                        "  (without {name}: the command line is too long for it to start the compiler with)"
                                    )));
                                }
                            }

                            started += 1;
                            ui::progress(started, total.max(started), &action.description);
                            if !ui::interactive() && !ui::quiet() {
                                ui::print_line(&format!("[{started}/{}] {}", total.max(started), action.description));
                            }

                            // Should it fail or stop halfway, nothing it leaves behind may pass for up to date
                            let known = self.known_inputs(index);
                            self.database.forget(self.output_ids[index][0]);

                            let lane = lanes_busy.iter().position(|busy| !busy).expect("a lane is free below the job limit");
                            lanes_busy[lane] = true;
                            job_sender.send((index, lane + 1, known)).context("worker pool stopped")?;
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
                let mut outcome = match report_receiver.recv().context("worker pool stopped")? {
                    Report::ProcessDone(lane) => {
                        lanes_busy[lane - 1] = false;
                        processes -= 1;
                        continue;
                    }
                    Report::Finished(outcome, timing) => {
                        self.timings.push(timing);
                        outcome
                    }
                };
                running -= 1;
                finished += 1;
                let index = outcome.action;

                if outcome.success {
                    self.record(&mut outcome);
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
                    block_dependents(index, &dependents, &mut blocked, &mut waiting_on, &mut ready);
                    if !self.options.keep_going {
                        stop = true;
                    }
                }

                if INTERRUPTED.load(Ordering::Relaxed) {
                    stop = true;
                }
            }

            Ok(summary)
        })?;
        ui::clear_progress();
        self.restamp_rewritten();

        if INTERRUPTED.load(Ordering::Relaxed) {
            self.database.finish()?;
            bail!("interrupted");
        }

        Ok(summary)
    }

    /// Why an action must run, or None when its outputs are up to date.
    fn out_of_date(&mut self, index: usize) -> Option<String> {
        let plan = self.plan;
        let action = &plan.actions[index];
        let output_ids = &self.output_ids[index];

        for (output, id) in action.outputs.iter().zip(output_ids) {
            if self.disk_mtimes[*id as usize].is_none() {
                return Some(format!("{} does not exist", file_name(output)));
            }
        }

        let Some(record) = self.database.record(output_ids[0]) else {
            return Some("not built yet, or its last run failed or stopped".into());
        };
        if record.command_hash != self.record_hash(action) {
            return Some(match self.options.explain {
                true => self.command_change(action, record),
                false => "its command changed".into(),
            });
        }

        // Outputs as the action left them
        if record.outputs.len() != output_ids.len() {
            return Some("its outputs changed".into());
        }
        for ((output, id), recorded) in action.outputs.iter().zip(output_ids).zip(&record.outputs) {
            if self.disk_mtimes[*id as usize] != Some(*recorded) {
                return Some(format!("{} was modified after it was built", self.display(output)));
            }
        }

        // Every input the last build read: a touched one counts only if its content changed; one without a hash
        // changed while its action ran, which may have read either version
        let mut touched = Vec::new();
        for (position, stamp) in record.inputs.iter().enumerate() {
            let path = Path::new(self.database.path(stamp.path));
            let Some(mtime) = self.disk_mtime(stamp.path) else {
                return Some(format!("{} was deleted", self.display(path)));
            };
            if stamp.hash == 0 {
                return Some(format!("{} changed while it last ran", self.display(path)));
            }
            if mtime == stamp.mtime {
                continue;
            }

            if self.stamps.hash(&util::path_key(path), path, mtime) != stamp.hash {
                return Some(format!("{} changed", self.display(path)));
            }
            touched.push((position, mtime));
        }

        // Touched but unchanged: keep the new time stamps, so the next build need not read those files again
        if !touched.is_empty() {
            let mut record = record.clone();
            for (position, mtime) in touched {
                record.inputs[position].mtime = mtime;
            }
            self.database.put(output_ids[0], record);
        }

        None
    }

    /// Stores what a successful action read and wrote, so the next build can tell whether it is stale.
    fn record(&mut self, outcome: &mut Outcome) {
        let plan = self.plan;
        let action = &plan.actions[outcome.action];
        let output_ids = &self.output_ids[outcome.action];

        // Outputs changed on disk
        for ((output, id), mtime) in action.outputs.iter().zip(output_ids).zip(&outcome.output_mtimes) {
            self.stamps.forget(output);
            self.disk_mtimes[*id as usize] = *mtime;
        }

        // What it read. An input the build makes that changed meanwhile was rewritten in place by this action (a
        // post-build step signing a program): the build's end stamps it as it is then.
        let mut inputs = Vec::with_capacity(outcome.measured.len());
        for measured in outcome.measured.drain(..) {
            let id = self.database.intern_keyed(measured.key, &measured.path.to_string_lossy());
            if measured.changed_while_running && self.produced.get(id as usize) == Some(&true) {
                self.rewritten.push(id);
            }

            inputs.push(Stamp {
                path: id,
                mtime: measured.mtime,
                hash: measured.hash,
            });
        }

        // The command, for --explain to say what changed next time
        let mut command = Vec::with_capacity(action.args.len() + 1);
        command.push(self.database.intern_argument(&action.program.to_string_lossy()));
        for argument in &action.args {
            command.push(self.database.intern_argument(argument));
        }

        let record = Record {
            command_hash: self.record_hash(action),
            command,
            outputs: outcome.output_mtimes.iter().map(|mtime| mtime.unwrap_or(NO_TIME_STAMP)).collect(),
            inputs,
        };
        self.database.put(output_ids[0], record);
        self.ran.push(outcome.action);
    }

    /// Files the build makes that a later action rewrote in place: the records of the actions that make them (whether
    /// or not they ran) and of those that ran take their final time stamps and content, so the next build does not
    /// take them for edited.
    fn restamp_rewritten(&mut self) {
        if self.rewritten.is_empty() {
            return;
        }

        let mut finals: HashMap<u32, (i64, u64)> = HashMap::new();
        for id in &self.rewritten {
            let path = Path::new(self.database.path(*id));
            if let Some(mtime) = util::mtime(path) {
                finals.insert(*id, (mtime, util::hash_file(path).unwrap_or(0)));
            }
        }

        let ran: HashSet<usize> = self.ran.iter().copied().collect();
        for (index, output_ids) in self.output_ids.iter().enumerate() {
            let makes_rewritten = output_ids.iter().any(|id| finals.contains_key(id));
            if !makes_rewritten && !ran.contains(&index) {
                continue;
            }
            let Some(record) = self.database.record(output_ids[0]) else { continue };
            let mut record = record.clone();
            let mut changed = false;

            for (recorded, id) in record.outputs.iter_mut().zip(output_ids) {
                if let Some((mtime, _)) = finals.get(id) {
                    changed |= *recorded != *mtime;
                    *recorded = *mtime;
                }
            }

            // Only an action that ran read the file as it is now
            if ran.contains(&index) {
                for stamp in &mut record.inputs {
                    if let Some((mtime, hash)) = finals.get(&stamp.path) {
                        changed |= stamp.mtime != *mtime || stamp.hash != *hash;
                        stamp.mtime = *mtime;
                        stamp.hash = *hash;
                    }
                }
            }

            if changed {
                self.database.put(output_ids[0], record);
            }
        }
    }

    /// What the action read on its last run, as this build last saw it, for its worker to tell what changed while
    /// it runs.
    fn known_inputs(&self, index: usize) -> Vec<Known> {
        let Some(record) = self.database.record(self.output_ids[index][0]) else {
            return Vec::new();
        };

        record
            .inputs
            .iter()
            .filter_map(|stamp| {
                let mtime = self.disk_mtime(stamp.path)?;
                Some(Known {
                    path: self.database.path(stamp.path).to_string(),
                    mtime,
                    hash: if mtime == stamp.mtime { stamp.hash } else { 0 },
                })
            })
            .collect()
    }

    /// The time stamp this build found for a path id; ids interned during the build are stat'ed now.
    fn disk_mtime(&self, id: u32) -> Option<i64> {
        match self.disk_mtimes.get(id as usize) {
            Some(mtime) => *mtime,
            None => util::mtime(Path::new(self.database.path(id))),
        }
    }

    /// An action's command hash tied to the toolchain that runs it.
    fn record_hash(&self, action: &Action) -> u64 {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&action.command_hash.to_le_bytes());
        bytes[8..].copy_from_slice(&self.toolchain_identity.to_le_bytes());
        util::hash_bytes(&bytes)
    }

    /// What changed in an action's command since its record, briefly: "its command changed: -DFOO=1 -> -DFOO=2".
    fn command_change(&self, action: &Action, record: &Record) -> String {
        let program = action.program.to_string_lossy();
        let new: Vec<&str> = std::iter::once(program.as_ref()).chain(action.args.iter().map(String::as_str)).collect();
        let old: Vec<&str> = record.command.iter().map(|id| self.database.argument(*id)).collect();

        // Same command: something else in the hash changed
        if old == new {
            let added = action.inputs.iter().find(|input| {
                let id = self.database.id(input);
                !record.inputs.iter().any(|stamp| Some(stamp.path) == id)
            });
            return match added {
                Some(input) => format!("its inputs changed: {} is new", self.display(input)),
                None => "its inputs, working folder or toolchain changed".into(),
            };
        }

        // The differing middle, between what both start and end with
        let prefix = old.iter().zip(&new).take_while(|(old, new)| old == new).count();
        let room = old.len().min(new.len()) - prefix;
        let suffix = old
            .iter()
            .rev()
            .zip(new.iter().rev())
            .take(room)
            .take_while(|(old, new)| old == new)
            .count();
        let removed = &old[prefix..old.len() - suffix];
        let added = &new[prefix..new.len() - suffix];

        match (removed.is_empty(), added.is_empty()) {
            (true, _) => format!("its command changed: added {}", brief_arguments(added)),
            (_, true) => format!("its command changed: removed {}", brief_arguments(removed)),
            _ => format!("its command changed: {} -> {}", brief_arguments(removed), brief_arguments(added)),
        }
    }

    /// A path for messages: relative to the project when inside it.
    fn display(&self, path: &Path) -> String {
        util::rel(path, &self.options.root)
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
        output_mtimes: Vec::new(),
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
        Err(error) if error.raw_os_error() == Some(ERROR_COMMAND_LINE_TOO_LONG) => {
            outcome.error = Some(command_too_long(action));
            return outcome;
        }
        Err(error) => {
            let program = launcher_for(action).unwrap_or(&action.program);
            outcome.error = Some(format!("{}: could not start {}: {error}", action.description, program.display()));
            return outcome;
        }
    };

    // Collect the output, minus the noise
    let mut text = decode_output(&output.stdout);
    text.push_str(&decode_output(&output.stderr));
    if action.deps == DepsFormat::ShowIncludes {
        let (headers, rest) = depfile::split_show_includes(&text);
        outcome.discovered = headers;
        text = rest;
    }
    outcome.output = clean_output(&text, action.echo_line.as_deref());
    outcome.success = output.status.success();

    if !outcome.success {
        if let Some(hint) = crate::flags::link_failure_hint(&outcome.output).filter(|_| action.kind == ActionKind::Link) {
            outcome.output.push_str(&format!("\nhint: {hint}\n"));
        }
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

/// Time stamps of files in one folder, by path id: from one listing of the folder when it holds many of them.
fn folder_stats(folder: &Path, files: &[(u32, &Path)]) -> Vec<(u32, Option<i64>)> {
    let listing = if files.len() >= LISTED_FOLDER_MIN_FILES {
        util::folder_mtimes(folder)
    } else {
        None
    };

    let mut stats = Vec::with_capacity(files.len());
    for (id, path) in files {
        let mtime = match &listing {
            Some(entries) => {
                let name = path.file_name().map(|name| name.to_string_lossy().to_lowercase()).unwrap_or_default();
                entries.get(&name).copied()
            }
            None => util::mtime(path),
        };
        stats.push((*id, mtime));
    }

    stats
}

/// What the action reads as it is before it runs, by `util::path_key`: what it read last time (as the scheduler last
/// saw it), and its declared inputs. Each gets its content hash now, mostly from the record or the cache, so a file
/// it rewrites in place with the same content can be told from one that really changed.
fn stamp_before(action: &Action, known: Vec<Known>, stamps: &Stamps) -> HashMap<String, (i64, u64)> {
    let mut before = HashMap::with_capacity(known.len() + action.inputs.len());

    for file in known {
        let path = Path::new(&file.path);
        let key = util::path_key(path);
        if file.hash != 0 {
            before.insert(key, (file.mtime, file.hash));
            continue;
        }

        // Changed since the record: hashed as it is now
        if let Some(mtime) = util::mtime(path) {
            let hash = stamps.hash(&key, path, mtime);
            before.insert(key, (mtime, hash));
        }
    }

    for input in &action.inputs {
        let key = util::path_key(input);
        if before.contains_key(&key) {
            continue;
        }
        if let Some(mtime) = util::mtime(input) {
            let hash = stamps.hash(&key, input, mtime);
            before.insert(key, (mtime, hash));
        }
    }

    before
}

/// What a successful action read: its declared inputs, then the headers it discovered (minus the toolchain's own,
/// relative ones against its working folder), each with its time stamp and content hash. Declared inputs are stat'ed
/// afresh, as the action may have rewritten one. A file whose time stamp moved since `before` changed while the
/// action ran: it keeps its hash only when its content is the same (else 0, so the action runs again next time).
fn measure_inputs(
    action: &Action,
    outcome: &Outcome,
    before: &HashMap<String, (i64, u64)>,
    stamps: &Stamps,
    system_dirs: &[String],
) -> Vec<Measured> {
    let mut paths: Vec<(String, PathBuf, bool)> = Vec::with_capacity(action.inputs.len() + outcome.discovered.len());
    for input in &action.inputs {
        paths.push((util::path_key(input), input.clone(), true));
    }

    let workdir = action.workdir.clone().or_else(|| std::env::current_dir().ok()).unwrap_or_default();
    for discovered in &outcome.discovered {
        let path = util::absolute(&workdir, Path::new(discovered));
        let key = util::path_key(&path);
        if !system_dirs.iter().any(|prefix| key.starts_with(prefix.as_str())) {
            paths.push((key, path, false));
        }
    }

    // One entry per file, the declared one first (the sort is stable)
    paths.sort_by(|left, right| left.0.cmp(&right.0));
    paths.dedup_by(|later, earlier| later.0 == earlier.0);

    let finished = util::now_ticks();
    paths
        .into_iter()
        .filter_map(|(key, path, declared)| {
            let mtime = match declared {
                true => {
                    let mtime = util::mtime(&path);
                    stamps.set(&key, mtime);
                    mtime?
                }
                false => stamps.mtime(&key, &path)?,
            };

            let (changed_while_running, hash) = match before.get(&key) {
                // Unchanged: the hash it had (none when it could not be read then)
                Some((before_mtime, before_hash)) if *before_mtime == mtime && *before_hash != 0 => (false, *before_hash),
                Some((before_mtime, _)) if *before_mtime == mtime => (false, stamps.hash(&key, &path, mtime)),

                // Rewritten while it ran: the same content still counts as read
                Some((_, before_hash)) => {
                    let hash = stamps.hash(&key, &path, mtime);
                    (true, if hash == *before_hash { hash } else { 0 })
                }

                // A header first read this run: written while it ran when its time stamp falls in the run (one in
                // the future, from another machine's clock, was not)
                None if mtime > outcome.started && mtime <= finished => (true, 0),
                None => (false, stamps.hash(&key, &path, mtime)),
            };
            let hash = if !changed_while_running && hash == 0 { UNREADABLE_HASH } else { hash };
            Some(Measured {
                key,
                path,
                mtime,
                hash,
                changed_while_running,
            })
        })
        .collect()
}

/// Hashes the objects a compile wrote, while other workers still compile: an archive or link reading hundreds of
/// them then finds every hash cached instead of reading them one by one at the end of the build.
fn hash_objects(action: &Action, output_mtimes: &[Option<i64>], stamps: &Stamps) {
    if !matches!(action.kind, ActionKind::Compile | ActionKind::Assemble | ActionKind::Resource) {
        return;
    }

    for (output, mtime) in action.outputs.iter().zip(output_mtimes) {
        if let Some(mtime) = mtime {
            stamps.hash(&util::path_key(output), output, *mtime);
        }
    }
}

/// The compiler cache an action runs through: none when its arguments are too long for the cache to pass on.
fn launcher_for(action: &Action) -> Option<&PathBuf> {
    let launcher = action.launcher.as_ref()?;
    if cfg!(windows) && arguments_length(action) > LAUNCHER_LINE_LIMIT {
        return None;
    }

    Some(launcher)
}

/// How long the action's arguments are on a command line, roughly (quotes and a space each).
fn arguments_length(action: &Action) -> usize {
    action.args.iter().map(|argument| argument.len() + 3).sum()
}

fn build_command(action: &Action, low_priority: bool) -> Result<Command> {
    let mut command = match launcher_for(action) {
        Some(launcher) => {
            let mut command = Command::new(launcher);
            command.arg(&action.program);
            command
        }
        None => Command::new(&action.program),
    };
    command.envs(action.env.iter().cloned());
    if let Some(workdir) = &action.workdir {
        command.current_dir(workdir);
    }
    if action.color_diagnostics {
        command.arg(COLOR_DIAGNOSTICS);
    }

    // Long command lines go through a response file: on Windows only, where the limit is low (elsewhere it is far
    // higher, and Apple's ar reads no response files)
    let length = arguments_length(action);
    if cfg!(windows) && action.response != ResponseStyle::Never && length > RESPONSE_FILE_THRESHOLD {
        let response_file = PathBuf::from(format!("{}.rsp", action.outputs[0].display()));
        std::fs::write(&response_file, response_file_content(&action.args, action.response)).context("writing a response file")?;
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

/// A response file's bytes. cl, link and lib read an option's argument only from the same line, and non-ASCII text
/// only as UTF-16: one line, UTF-16 with a byte order mark. gcc takes one argument per line, UTF-8.
fn response_file_content(args: &[String], style: ResponseStyle) -> Vec<u8> {
    let quoted: Vec<String> = args.iter().map(|argument| quote_response(argument, style)).collect();
    if style != ResponseStyle::Msvc {
        return quoted.join("\n").into_bytes();
    }

    let mut bytes = vec![0xFF, 0xFE];
    for unit in quoted.join(" ").encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }

    bytes
}

/// Windows refused to start the action: its command line is too long. A build step can be split.
fn command_too_long(action: &Action) -> String {
    let length = command_line(action).len();
    let mut message = format!(
        "{}: its command line is {length} characters, more than Windows allows ({WINDOWS_COMMAND_LINE_LIMIT})",
        action.description
    );
    if action.kind == ActionKind::Run {
        message.push_str(&format!(
            "; give the [[run]] step making {} fewer inputs, or run it once per file with `for-each`",
            file_name(&action.outputs[0])
        ));
    }

    message
}

/// A tool's output as text: UTF-8 when it is, else the console's code page (cl writes file names in it).
fn decode_output(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => decode_code_page(bytes),
    }
}

#[cfg(windows)]
fn decode_code_page(bytes: &[u8]) -> String {
    use windows_sys::Win32::Globalization::{GetOEMCP, MultiByteToWideChar};
    use windows_sys::Win32::System::Console::GetConsoleOutputCP;

    // Without a console of ours, each child gets a new one, which starts in the OEM code page
    let code_page = match unsafe { GetConsoleOutputCP() } {
        0 => unsafe { GetOEMCP() },
        code_page => code_page,
    };

    let length = unsafe { MultiByteToWideChar(code_page, 0, bytes.as_ptr(), bytes.len() as i32, std::ptr::null_mut(), 0) };
    if length <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut wide = vec![0u16; length as usize];
    unsafe { MultiByteToWideChar(code_page, 0, bytes.as_ptr(), bytes.len() as i32, wide.as_mut_ptr(), length) };

    String::from_utf16_lossy(&wide)
}

#[cfg(not(windows))]
fn decode_code_page(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
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

/// Removes tool noise: cl's echo of the file name, link's "Creating library" note and its /LTCG progress.
fn clean_output(text: &str, echo_line: Option<&str>) -> String {
    let mut cleaned = String::new();

    for line in text.lines() {
        let trimmed = line.trim();
        let is_echo =
            echo_line.is_some_and(|echo| trimmed.eq_ignore_ascii_case(echo)) || (trimmed.starts_with("tmpxft_") && trimmed.ends_with(".cudafe1.cpp"));
        let is_link_chatter = (trimmed.starts_with("LINK : ") && trimmed.ends_with("performing full link"))
            || trimmed == "Generating code"
            || trimmed == "Finished generating code";
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

/// Copies through a temporary file, so a running program holding the old DLL never sees half a file. The temporary
/// name keeps the whole file name, so foo.dll and foo.pdb copied at once never share one.
fn copy_file(source: &Path, destination: &Path) -> Result<()> {
    let mut temporary = destination.as_os_str().to_owned();
    temporary.push(".anvil-tmp");
    let temporary = PathBuf::from(temporary);
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

/// Marks an action's dependents as never able to run; they still pass through the queue, to be counted.
fn block_dependents(index: usize, dependents: &[Vec<usize>], blocked: &mut [bool], waiting_on: &mut [usize], ready: &mut VecDeque<usize>) {
    for dependent in &dependents[index] {
        blocked[*dependent] = true;
        waiting_on[*dependent] -= 1;
        if waiting_on[*dependent] == 0 {
            ready.push_back(*dependent);
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
        .unwrap_or_else(|| util::slash(path))
}

/// A few arguments for a message, long ones shortened.
fn brief_arguments(arguments: &[&str]) -> String {
    const LONGEST: usize = 60;

    let mut words: Vec<String> = arguments
        .iter()
        .take(EXPLAIN_ARGUMENTS)
        .map(|argument| match argument.char_indices().nth(LONGEST) {
            Some((end, _)) => format!("{}...", &argument[..end]),
            None => argument.to_string(),
        })
        .collect();
    if arguments.len() > EXPLAIN_ARGUMENTS {
        words.push(format!("(and {} more)", arguments.len() - EXPLAIN_ARGUMENTS));
    }

    words.join(" ")
}

/// A printable command line (for --verbose).
pub fn command_line(action: &Action) -> String {
    if let Some(source) = &action.copy_from {
        return format!("copy {} {}", source.display(), action.outputs[0].display());
    }

    let mut line = quote_for_display(&action.program.to_string_lossy());
    if let Some(launcher) = launcher_for(action) {
        line = format!("{} {line}", quote_for_display(&launcher.to_string_lossy()));
    }
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
    fn msvc_response_files_are_one_utf16_line() {
        let args = vec!["/sourceDependencies".to_string(), "C:/dé é/a.json".to_string()];
        let bytes = response_file_content(&args, ResponseStyle::Msvc);
        assert_eq!(&bytes[..2], &[0xFF, 0xFE]);

        let units: Vec<u16> = bytes[2..].chunks(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
        assert_eq!(String::from_utf16(&units).unwrap(), "/sourceDependencies \"C:/dé é/a.json\"");
    }

    #[test]
    fn cleans_noise() {
        let text = "main.cpp\nmain.cpp(3): warning C4100\n   Creating library x.lib and object x.exp\nGenerating code\nFinished generating code\n";
        assert_eq!(clean_output(text, Some("main.cpp")), "main.cpp(3): warning C4100\n");
    }

    #[test]
    fn briefs_arguments() {
        assert_eq!(brief_arguments(&["-DFOO=1"]), "-DFOO=1");
        assert_eq!(brief_arguments(&["a", "b", "c", "d", "e"]), "a b c (and 2 more)");
    }
}
