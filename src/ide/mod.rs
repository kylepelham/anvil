//! IDE support. `anvil targets --json` describes the project for editor extensions; `anvil ide` writes the project
//! files of Visual Studio, VS Code, CLion and Zed. What it writes only calls anvil (no flags or source lists a build
//! depends on), and every build refreshes the files `anvil ide` wrote (`refresh`), writing one only when its text
//! changes, so nothing an IDE reads goes stale.

mod clion;
mod jsonc;
mod vs;
mod vscode;
mod zed;

use crate::commands::{self, RunSetup};
use crate::plan;
use crate::platform::{Os, Platform};
use crate::project::Model;
use crate::targets::{self, Kind};
use crate::toolchain::Family;
use crate::ui;
use crate::util::{self, slash};
use crate::BuildArgs;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// Anvil's folder in the project: what `anvil ide` set up, the refresh stamps, Visual Studio's projects
pub const STATE_FOLDER: &str = ".anvil";
const RECORD_FILE: &str = "ide.toml";
const RECORD_HEADER: &str = "# Written by `anvil ide`: the IDE files every anvil build refreshes, and the options their builds use.\n";

// The start of the name of every entry Anvil owns in a file the user edits too (tasks, launch configurations)
pub const OWNED_PREFIX: &str = "anvil: ";

// The version of `anvil targets --json`'s shape: fields may be added, never renamed
const TARGETS_VERSION: u32 = 1;

/// An IDE `anvil ide` writes files for.
#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ide {
    /// Visual Studio: a solution with a project per target.
    Vs,
    /// VS Code: build tasks, launch configurations and settings in .vscode/.
    Vscode,
    /// CLion: custom build targets and run configurations in .idea/.
    Clion,
    /// Zed: tasks and debug configurations in .zed/.
    Zed,
    /// Every one of them (Visual Studio on Windows only).
    All,
}

/// What `anvil ide` set up: which IDEs' files every build refreshes, and how those IDEs call anvil.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub ides: Vec<Ide>,
    /// The anvil the IDEs run: `anvil` when it is the one on PATH, else its path.
    pub anvil: String,
    /// The profile of VS Code's, CLion's and Zed's builds (Visual Studio has a configuration per profile).
    pub profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolchain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

/// What the IDE files describe, gathered from the model without building.
#[derive(Debug)]
pub struct Project {
    pub root: PathBuf,
    pub name: String,
    pub out_root: PathBuf,
    pub out_dir: PathBuf,
    pub profile: String,
    pub profiles: Vec<String>,
    pub toolchain: ToolchainInfo,
    pub default_run: Option<String>,
    pub targets: Vec<TargetInfo>,
    /// A workspace: targets are grouped by package.
    pub packages: Vec<String>,
}

#[derive(Debug)]
pub struct ToolchainInfo {
    pub family: Family,
    pub version: String,
    pub msvc_abi: bool,
    pub platform: Platform,
    pub debugger: Debugger,
    pub debugger_path: Option<PathBuf>,
    /// The C++ compiler.
    pub compiler: PathBuf,
}

/// The debugger for the toolchain's programs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Debugger {
    Msvc,
    Gdb,
    Lldb,
}

#[derive(Debug)]
pub struct TargetInfo {
    /// Its index in the model.
    pub index: usize,
    pub name: String,
    pub package: String,
    pub qualified: String,
    pub kind: Kind,
    pub default: bool,
    /// A program this machine can start.
    pub runnable: bool,
    pub output: Option<PathBuf>,
    /// How `anvil run` starts it (programs and tests).
    pub run: Option<RunSetup>,
    pub dir: PathBuf,
    pub gui: bool,

    // For Visual Studio's projects: the files, the folders of headers, where the objects go
    pub sources: Vec<PathBuf>,
    pub header_folders: Vec<PathBuf>,
    pub object_dir: PathBuf,
}

/// Writes generated files, each only when its text changed, and lists those it wrote and deleted.
#[derive(Default)]
pub struct Writer {
    pub written: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
}

impl Ide {
    pub fn name(self) -> &'static str {
        match self {
            Ide::Vs => "vs",
            Ide::Vscode => "vscode",
            Ide::Clion => "clion",
            Ide::Zed => "zed",
            Ide::All => "all",
        }
    }

    /// The file or folder whose presence says the IDE is in use; deleting it stops the refreshes.
    fn anchor(self, root: &Path, project_name: &str) -> PathBuf {
        match self {
            Ide::Vs => vs::solution_path(root, project_name),
            Ide::Vscode => root.join(".vscode"),
            Ide::Clion => root.join(".idea"),
            Ide::Zed => root.join(".zed"),
            Ide::All => root.to_path_buf(),
        }
    }
}

impl Record {
    /// The record of the project at `root`, if `anvil ide` wrote one.
    pub fn load(root: &Path) -> Option<Record> {
        let text = std::fs::read_to_string(root.join(STATE_FOLDER).join(RECORD_FILE)).ok()?;
        toml::from_str(&text).ok()
    }

    fn save(&self, root: &Path) -> Result<()> {
        let text = format!("{RECORD_HEADER}{}", toml::to_string(self)?);
        write_file(&root.join(STATE_FOLDER).join(RECORD_FILE), &text)?;
        Ok(())
    }

    /// The arguments of an anvil command an IDE runs: the command's words, then `-p` (for `profile`), `--toolchain`,
    /// `--target` and `-o` as `anvil ide` was given them.
    pub fn args(&self, words: &[&str], profile: Option<&str>) -> Vec<String> {
        let mut args: Vec<String> = words.iter().map(|word| word.to_string()).collect();
        if let Some(profile) = profile {
            args.extend(["-p".to_string(), profile.to_string()]);
        }
        if let Some(toolchain) = &self.toolchain {
            args.extend(["--toolchain".to_string(), toolchain.clone()]);
        }
        if let Some(target) = &self.target {
            args.extend(["--target".to_string(), target.clone()]);
        }
        for option in &self.options {
            args.extend(["-o".to_string(), option.clone()]);
        }
        args
    }
}

impl Project {
    /// The project as the IDE files describe it; programs start in `here` as `anvil run` started there would.
    pub fn gather(model: &Model, here: &Path) -> Result<Project> {
        let toolchain = &model.toolchain;
        let (debugger, debugger_path) = find_debugger(model);

        // The root package and workspace members' targets
        let mut targets = Vec::new();
        for (index, target) in model.targets.iter().enumerate() {
            let package = &model.packages[target.package];
            if !package.member || target.kind == Kind::External {
                continue;
            }

            let run = match target.kind.is_executable() {
                true => Some(commands::run_setup(model, index, here)?),
                false => None,
            };
            let header_folders = target
                .include
                .iter()
                .chain(&target.private_include)
                .filter(|folder| util::strip_prefix_ci(folder, &package.root).is_some())
                .filter(|folder| util::strip_prefix_ci(folder, &model.out_root).is_none())
                .fold(Vec::new(), |mut folders: Vec<PathBuf>, folder| {
                    if !folders.contains(folder) {
                        folders.push(folder.clone());
                    }
                    folders
                });

            targets.push(TargetInfo {
                index,
                name: target.name.clone(),
                package: package.name.clone(),
                qualified: model.qualified_name(index),
                kind: target.kind,
                default: target.default,
                runnable: target.kind.is_executable() && toolchain.platform.runs_on_host(),
                output: plan::artifact_path(model, target),
                run,
                dir: target.dir.clone(),
                gui: target.gui != crate::flags::Gui::Off,
                sources: target.sources.clone(),
                header_folders,
                object_dir: model
                    .package_out_dir(target.package)
                    .join("obj")
                    .join(targets::sanitize_name(&target.name)),
            });
        }

        // The built-in profiles, then those the root manifest declares
        let declared = model.packages[0].manifest.profile.keys();
        let mut profiles = vec!["debug".to_string(), "release".to_string()];
        profiles.extend(declared.filter(|name| *name != "debug" && *name != "release").cloned());

        let packages = model
            .packages
            .iter()
            .filter(|package| package.member)
            .map(|package| package.name.clone())
            .collect();
        let default_run = commands::choose_program(model, None).ok().map(|index| model.qualified_name(index));

        Ok(Project {
            root: model.root.clone(),
            name: model.packages[0].name.clone(),
            out_root: model.out_root.clone(),
            out_dir: model.out_dir.clone(),
            profile: model.profile.name.clone(),
            profiles,
            toolchain: ToolchainInfo {
                family: toolchain.family,
                version: toolchain.version.clone(),
                msvc_abi: toolchain.is_msvc_abi(),
                platform: toolchain.platform.clone(),
                debugger,
                debugger_path,
                compiler: toolchain.cxx.clone(),
            },
            default_run,
            targets,
            packages,
        })
    }

    /// The programs a debugger can start: the one `anvil run` starts first, then the others in the project's order.
    pub fn runnable(&self) -> impl Iterator<Item = (&TargetInfo, &RunSetup)> {
        let is_default = |target: &TargetInfo| self.default_run.as_deref() == Some(target.qualified.as_str());
        let first = self.targets.iter().filter(move |target| is_default(target));
        let rest = self.targets.iter().filter(move |target| !is_default(target));
        first
            .chain(rest)
            .filter(|target| target.runnable)
            .filter_map(|target| target.run.as_ref().map(|run| (target, run)))
    }
}

impl Debugger {
    pub fn name(self) -> &'static str {
        match self {
            Debugger::Msvc => "msvc",
            Debugger::Gdb => "gdb",
            Debugger::Lldb => "lldb",
        }
    }
}

impl Writer {
    /// Writes `text` to `path` unless it already holds it.
    pub fn write(&mut self, path: &Path, text: &str) -> Result<()> {
        if write_file(path, text)? {
            self.written.push(path.to_path_buf());
        }
        Ok(())
    }

    /// Writes Anvil's entries merged into a file the user edits too; a file it could not read stays as it is, with a
    /// warning (the next build after it is fixed merges them).
    pub fn merge(&mut self, path: &Path, merged: Result<String>) -> Result<()> {
        match merged {
            Ok(text) => self.write(path, &text),
            Err(error) => {
                ui::warn(&format!(
                    "{} left as it is ({error:#}): once it reads as JSON with comments again, the next build adds Anvil's entries to it",
                    slash(path)
                ));
                Ok(())
            }
        }
    }

    /// Says what it wrote and deleted, relative to `base`.
    fn report(&self, verb: &str, base: &Path) {
        for path in &self.written {
            ui::status(verb, &util::rel(path, base));
        }
        for path in &self.removed {
            ui::status("Removed", &util::rel(path, base));
        }
    }

    /// Deletes a file Anvil wrote before and no longer writes (another anvil, building at the same time, may have).
    pub fn remove(&mut self, path: &Path) -> Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => self.removed.push(path.to_path_buf()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("deleting {}", slash(path))),
        }
        Ok(())
    }
}

/// `anvil targets --json`: the project's targets and tests, as an editor extension reads them. Plans nothing, builds
/// nothing, installs nothing.
pub fn print_targets(start_dir: &Path, build: &BuildArgs) -> Result<()> {
    let mut model = commands::load_model_for_listing(start_dir, build)?;
    model.collect_tests()?;

    let project = Project::gather(&model, &std::env::current_dir()?)?;
    let description = targets_json(&model, &project)?;
    println!("{}", serde_json::to_string_pretty(&description)?);

    Ok(())
}

/// `anvil ide <ides>`: writes the IDEs' files, and records them for every later build to refresh.
pub fn setup(start_dir: &Path, build: &BuildArgs, requested: &[Ide]) -> Result<()> {
    let model = commands::load_model(start_dir, build)?;
    plan::plan(&model, None, &model.default_targets(true), false)?;
    let root = model.root.clone();
    let name = model.packages[0].name.clone();

    // The IDEs asked for (every one for `all`), or with none those in use here, and those set up before
    let previous = Record::load(&root);
    let mut ides: Vec<Ide> = previous.iter().flat_map(|record| record.ides.clone()).collect();
    let mut chosen: Vec<Ide> = match requested.contains(&Ide::All) {
        true if cfg!(windows) => vec![Ide::Vs, Ide::Vscode, Ide::Clion, Ide::Zed],
        true => vec![Ide::Vscode, Ide::Clion, Ide::Zed],
        false => requested.to_vec(),
    };
    if chosen.is_empty() {
        chosen = [Ide::Vs, Ide::Vscode, Ide::Clion, Ide::Zed]
            .into_iter()
            .filter(|ide| ide.anchor(&root, &name).exists() || (*ide == Ide::Vs && root.join(".vs").is_dir()))
            .collect();
    }
    for ide in chosen {
        if !ides.contains(&ide) {
            ides.push(ide);
        }
    }
    if ides.is_empty() {
        bail!("no IDE files here to refresh; say which IDE: `anvil ide vs`, `vscode`, `clion`, `zed` or `all`");
    }

    let record = Record {
        ides,
        anvil: anvil_command(),
        profile: model.profile.name.clone(),
        toolchain: build.toolchain.clone(),
        target: build.platform.clone(),
        options: build.options.clone(),
    };
    record.save(&root)?;

    // compile_commands.json at the root, where clangd and cpptools find it; every build refreshes it from now on
    let mut writer = Writer::default();
    copy_compile_commands(&model)?;

    // Each IDE's files
    let project = Project::gather(&model, &root)?;
    for ide in &record.ides {
        generate(*ide, &model, &project, &record, &mut writer)?;
        save_stamp(*ide, &model, &project, &record);
    }
    writer.report("Wrote", &std::env::current_dir()?);
    if writer.written.is_empty() && writer.removed.is_empty() {
        ui::status("Fresh", "the IDE files are up to date");
    }

    // How to use them
    for ide in &record.ides {
        let hint = match ide {
            Ide::Vs => format!("Visual Studio: open {name}.sln; Build Solution builds, F5 debugs the startup project"),
            Ide::Vscode => "VS Code: Ctrl+Shift+B builds, F5 debugs (pick a program in Run and Debug)".to_string(),
            Ide::Clion => "CLion: open the folder (compile_commands.json is the project); run configurations build first".to_string(),
            Ide::Zed => "Zed: `task: spawn` builds, `debugger: start` (F4) debugs".to_string(),
            Ide::All => continue,
        };
        ui::note(&hint);
    }
    warn_unignored(&root, &name, &record);

    Ok(())
}

/// Refreshes the IDE files `anvil ide` wrote, for a build about to run: those whose builds this one is (same
/// toolchain, machine and options; same profile, but for Visual Studio, which has them all), each only when what it
/// describes changed. A file that cannot be refreshed is a warning, never a failed build.
pub fn refresh(model: &Model, build: &BuildArgs) {
    if model.options.host_tools_dir.is_some() {
        return;
    }
    refresh_compile_commands(model);

    // Set up for builds like this one
    let Some(record) = Record::load(&model.root) else {
        return;
    };
    if record.toolchain != build.toolchain || record.target != build.platform || record.options != build.options {
        return;
    }

    let name = &model.packages[0].name;
    let mut project = None;
    for ide in &record.ides {
        // Deleting an IDE's files stops their refreshes; only Visual Studio builds other profiles
        if !ide.anchor(&model.root, name).exists() || (*ide != Ide::Vs && model.profile.name != record.profile) {
            continue;
        }

        // Unchanged since the last refresh
        let project = match project.get_or_insert_with(|| Project::gather(model, &model.root)) {
            Ok(project) => &*project,
            Err(error) => {
                ui::warn(&format!("IDE files not refreshed: {error:#}"));
                return;
            }
        };
        let stamp_file = stamp_path(*ide, model);
        if std::fs::read_to_string(&stamp_file).ok() == Some(fingerprint(*ide, model, project, &record)) {
            continue;
        }

        let mut writer = Writer::default();
        match generate(*ide, model, project, &record, &mut writer) {
            Ok(()) => save_stamp(*ide, model, project, &record),
            Err(error) => ui::warn(&format!("{} files not refreshed: {error:#}", ide.name())),
        }
        writer.report("Updated", &model.root);
    }
}

/// The JSON `anvil targets --json` prints.
fn targets_json(model: &Model, project: &Project) -> Result<Value> {
    let toolchain = &project.toolchain;
    let platform = &toolchain.platform;

    let targets: Vec<Value> = project
        .targets
        .iter()
        .map(|target| {
            let run = target.run.as_ref();
            json!({
                "name": target.name,
                "package": target.package,
                "qualified": target.qualified,
                "kind": target.kind.name(),
                "default": target.default,
                "runnable": target.runnable,
                "output": target.output.as_deref().map(slash),
                "workdir": run.map(|run| slash(&run.workdir)),
                "args": run.map(|run| run.args.clone()).unwrap_or_default(),
                "env": environment_object(run.map(|run| run.env.as_slice()).unwrap_or_default()),
                "dir": slash(&target.dir),
                "gui": target.gui,
            })
        })
        .collect();

    // Tests: the program a test runs when it is the project's ({target:X} first), and its command as it runs
    let mut tests = Vec::new();
    for test in &model.tests {
        let program = test
            .command
            .first()
            .filter(|first| first.starts_with("{target:") && first.ends_with('}'))
            .and_then(|first| plan::target_references(std::slice::from_ref(first)).into_iter().next());
        let command = test
            .command
            .iter()
            .map(|word| substitute_targets(word, model))
            .collect::<Result<Vec<_>>>()
            .with_context(|| format!("test `{}`", test.name))?;
        tests.push(json!({
            "name": test.name,
            "program": program,
            "command": command,
            "workdir": slash(&test.workdir),
            "env": environment_object(&test.env),
        }));
    }

    Ok(json!({
        "version": TARGETS_VERSION,
        "root": slash(&project.root),
        "name": project.name,
        "profile": project.profile,
        "profiles": project.profiles,
        "toolchain": {
            "family": toolchain.family.name(),
            "version": toolchain.version,
            "abi": if toolchain.msvc_abi { "msvc" } else { "gnu" },
            "platform": platform_name(platform),
            "debugger": toolchain.debugger.name(),
            "debugger_path": toolchain.debugger_path.as_deref().map(slash),
        },
        "out_dir": slash(&project.out_dir),
        "compile_commands": slash(&project.out_root.join("compile_commands.json")),
        "default_run": project.default_run,
        "targets": targets,
        "tests": tests,
    }))
}

/// `{target:X}` replaced with X's file, as the test runs it, in forward slashes.
pub fn substitute_targets(word: &str, model: &Model) -> Result<String> {
    let mut result = word.to_string();
    for name in plan::target_references(&[word.to_string()]) {
        let index = model.find_target(&name)?;
        match plan::artifact_path(model, &model.targets[index]) {
            Some(file) => result = result.replace(&format!("{{target:{name}}}"), &slash(&file)),
            None => return plan::substitute_targets(word, model),
        }
    }

    Ok(result)
}

/// Writes one IDE's files.
fn generate(ide: Ide, model: &Model, project: &Project, record: &Record, writer: &mut Writer) -> Result<()> {
    match ide {
        Ide::Vs => vs::generate(model, project, record, writer),
        Ide::Vscode => vscode::generate(project, record, writer),
        Ide::Clion => clion::generate(project, record, writer),
        Ide::Zed => zed::generate(project, record, writer),
        Ide::All => Ok(()),
    }
}

/// What an IDE's files are made from, hashed: the project, how its builds run, the anvil writing them, and the files
/// themselves as they are on disk (one the user deleted or edited is written again).
fn fingerprint(ide: Ide, model: &Model, project: &Project, record: &Record) -> String {
    let mut text = format!(
        "{ide:?}\n{record:?}\n{:?}\n",
        (
            &project.out_dir,
            &project.profiles,
            &project.toolchain,
            &project.default_run,
            &project.packages
        )
    );

    // The targets; their (many) files as they are, which formatting each would make slow
    let mut sources = Vec::new();
    for target in &project.targets {
        let TargetInfo {
            index: _,
            name: _,
            package: _,
            qualified,
            kind,
            default,
            runnable,
            output,
            run,
            dir,
            gui,
            sources: files,
            header_folders,
            object_dir,
        } = target;
        let described = (qualified, kind, default, runnable, output, run, dir, gui, header_folders, object_dir);
        text.push_str(&format!("{described:?}\n"));
        for file in files {
            sources.extend_from_slice(file.as_os_str().as_encoded_bytes());
            sources.push(b'\n');
        }
    }
    text.push_str(&format!("{:016x}\n", util::hash_bytes(&sources)));

    // A new anvil may write them differently
    if let Ok(exe) = std::env::current_exe() {
        text.push_str(&format!("{}:{:?}\n", slash(&exe), util::mtime(&exe)));
    }

    // Visual Studio's IntelliSense comes from each profile's compile commands
    let outputs = match ide {
        Ide::Vs => {
            for out_dir in vs::configuration_out_dirs(model, project, record) {
                let stamp = std::fs::read_to_string(out_dir.join(".anvil").join("compile_commands.stamp")).unwrap_or_default();
                text.push_str(&format!("{}={stamp}\n", slash(&out_dir)));
            }
            vs::outputs(project)
        }
        Ide::Vscode => vscode::outputs(&project.root),
        Ide::Clion => clion::outputs(&project.root),
        Ide::Zed => zed::outputs(&project.root),
        Ide::All => Vec::new(),
    };
    for output in outputs {
        let metadata = std::fs::metadata(&output).ok();
        let state = metadata.map(|metadata| (metadata.len(), metadata.modified().ok()));
        text.push_str(&format!("{}:{state:?}\n", slash(&output)));
    }

    format!("{:016x}", util::hash_bytes(text.as_bytes()))
}

fn stamp_path(ide: Ide, model: &Model) -> PathBuf {
    model.root.join(STATE_FOLDER).join(format!("{}-{}.stamp", ide.name(), model.profile.name))
}

fn save_stamp(ide: Ide, model: &Model, project: &Project, record: &Record) {
    let _ = write_file(&stamp_path(ide, model), &fingerprint(ide, model, project, record));
}

/// The root's compile_commands.json, once `anvil compdb` or `anvil ide` wrote it: kept the same as this build's, for
/// clangd and cpptools (and every editor reading it).
fn refresh_compile_commands(model: &Model) {
    let destination = model.root.join("compile_commands.json");
    let Ok(existing) = std::fs::symlink_metadata(&destination) else {
        return;
    };
    let Ok(current) = std::fs::metadata(model.out_dir.join("compile_commands.json")) else {
        return;
    };

    // The copy keeps the time of what it copied: the same size and time are the same file
    let same = current.len() == existing.len() && current.modified().ok() == existing.modified().ok();
    if !existing.is_file() || same {
        return;
    }
    if let Err(error) = copy_compile_commands(model) {
        ui::warn(&format!("compile_commands.json not refreshed: {error:#}"));
    }
}

/// Copies this build's compile_commands.json to the project root, with its time.
fn copy_compile_commands(model: &Model) -> Result<()> {
    let source = model.out_dir.join("compile_commands.json");
    let destination = model.root.join("compile_commands.json");
    let text = std::fs::read(&source).with_context(|| format!("reading {}", slash(&source)))?;
    if std::fs::read(&destination).ok() != Some(text.clone()) {
        util::write_atomic(&destination, &text).with_context(|| format!("writing {}", slash(&destination)))?;
    }

    let modified = std::fs::metadata(&source)?.modified()?;
    std::fs::File::options().write(true).open(&destination)?.set_modified(modified)?;
    Ok(())
}

/// Writes a file when its text differs (atomically, so an IDE never reads half of it). Whether it wrote.
pub fn write_file(path: &Path, text: &str) -> Result<bool> {
    if std::fs::read(path).ok().as_deref() == Some(text.as_bytes()) {
        return Ok(false);
    }

    util::write_atomic(path, text.as_bytes()).with_context(|| format!("writing {}", slash(path)))?;
    Ok(true)
}

/// The anvil an IDE runs: `anvil` when that is this one, else this one's path.
fn anvil_command() -> String {
    let Ok(exe) = std::env::current_exe() else {
        return "anvil".into();
    };

    let on_path = util::which("anvil").and_then(|found| found.canonicalize().ok());
    match on_path == exe.canonicalize().ok() {
        true => "anvil".into(),
        false => slash(&exe),
    }
}

/// The debugger for the toolchain's programs, and its path when Anvil finds it: Visual Studio's for the MSVC ABI,
/// lldb on macOS, else gdb (lldb when only that is there), beside the compiler first, then on PATH.
fn find_debugger(model: &Model) -> (Debugger, Option<PathBuf>) {
    let toolchain = &model.toolchain;
    if toolchain.is_msvc_abi() {
        return (Debugger::Msvc, None);
    }

    let order = match toolchain.platform.os {
        Os::Macos => [Debugger::Lldb, Debugger::Gdb],
        _ => [Debugger::Gdb, Debugger::Lldb],
    };
    for debugger in order {
        let file = format!("{}{}", debugger.name(), std::env::consts::EXE_SUFFIX);
        let beside = toolchain.cxx.parent().map(|folder| folder.join(&file)).filter(|path| path.is_file());
        if let Some(found) = beside.or_else(|| util::which(debugger.name())) {
            return (debugger, Some(found));
        }
    }

    (order[0], None)
}

/// "x64-windows", "arm64-linux", "arm64-macos".
fn platform_name(platform: &Platform) -> String {
    let os = match platform.os {
        Os::Windows => "windows",
        Os::Linux => "linux",
        Os::Macos => "macos",
    };
    format!("{}-{os}", platform.cpu.name())
}

fn environment_object(env: &[(String, String)]) -> Value {
    Value::Object(env.iter().map(|(name, value)| (name.clone(), Value::String(value.clone()))).collect())
}

/// A path as an IDE variable sees it: `<variable>/rel` inside the project root, else in full.
pub fn project_path(path: &Path, root: &Path, variable: &str) -> String {
    match util::strip_prefix_ci(path, root) {
        Some(relative) if relative.as_os_str().is_empty() => variable.to_string(),
        Some(relative) => format!("{variable}/{}", slash(&relative)),
        None => slash(path),
    }
}

/// Says what .gitignore should leave out and does not: what Anvil writes for this machine.
fn warn_unignored(root: &Path, name: &str, record: &Record) {
    let Ok(text) = std::fs::read_to_string(root.join(".gitignore")) else {
        return;
    };

    let mut wanted = vec![format!("/{STATE_FOLDER}/"), "/compile_commands.json".to_string()];
    if record.ides.contains(&Ide::Vs) {
        wanted.extend([format!("/{name}.sln"), "/.vs/".to_string()]);
    }
    let listed = |entry: &String| {
        let bare = entry.trim_matches('/');
        text.lines().any(|line| line.trim().trim_matches('/') == bare)
    };
    let missing: Vec<String> = wanted.into_iter().filter(|entry| !listed(entry)).collect();
    if !missing.is_empty() {
        ui::note(&format!("add to .gitignore what is written for this machine: {}", missing.join(" ")));
    }
}

/// A project of a program and a test, for the generators' tests.
#[cfg(test)]
pub fn sample_project(root: &Path, debugger: Debugger) -> Project {
    let program = |name: &str, kind: Kind| TargetInfo {
        index: 0,
        name: name.into(),
        package: "game".into(),
        qualified: name.into(),
        kind,
        default: kind != Kind::Test,
        runnable: true,
        output: Some(root.join("out/debug").join(format!("{name}.exe"))),
        run: Some(RunSetup {
            program: root.join("out/debug").join(format!("{name}.exe")),
            args: if kind == Kind::Test { vec!["--fast".into()] } else { Vec::new() },
            workdir: root.to_path_buf(),
            env: vec![("MODE".into(), "test".into())],
        }),
        dir: root.to_path_buf(),
        gui: false,
        sources: vec![root.join("src/main.cpp")],
        header_folders: Vec::new(),
        object_dir: root.join("out/debug/obj").join(name),
    };

    Project {
        root: root.to_path_buf(),
        name: "game".into(),
        out_root: root.join("out"),
        out_dir: root.join("out/debug"),
        profile: "debug".into(),
        profiles: vec!["debug".into(), "release".into()],
        toolchain: ToolchainInfo {
            family: Family::Gcc,
            version: "13".into(),
            msvc_abi: debugger == Debugger::Msvc,
            platform: Platform::host(),
            debugger,
            debugger_path: (debugger == Debugger::Gdb).then(|| PathBuf::from("C:/mingw/bin/gdb.exe")),
            compiler: PathBuf::from("C:/mingw/bin/g++.exe"),
        },
        default_run: Some("game".into()),
        targets: vec![program("game", Kind::Exe), program("unit", Kind::Test)],
        packages: vec!["game".into()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_give_the_ides_their_arguments() {
        let record = Record {
            ides: vec![Ide::Vs, Ide::Vscode],
            anvil: "anvil".into(),
            profile: "debug".into(),
            toolchain: Some("clang-cl".into()),
            target: None,
            options: vec!["tracy=true".into()],
        };
        let text = toml::to_string(&record).unwrap();
        assert_eq!(toml::from_str::<Record>(&text).unwrap(), record);
        assert!(text.contains("ides = [\"vs\", \"vscode\"]"));

        assert_eq!(
            record.args(&["build", "game"], Some("release")),
            ["build", "game", "-p", "release", "--toolchain", "clang-cl", "-o", "tracy=true"]
        );
    }

    #[test]
    fn paths_inside_the_project_use_the_ide_variable() {
        let root = Path::new("C:/work/game");
        assert_eq!(
            project_path(Path::new("C:/work/game/out/debug/game.exe"), root, "${workspaceFolder}"),
            "${workspaceFolder}/out/debug/game.exe"
        );
        assert_eq!(project_path(root, root, "$PROJECT_DIR$"), "$PROJECT_DIR$");
        assert_eq!(project_path(Path::new("D:/sdk/tool.exe"), root, "${workspaceFolder}"), "D:/sdk/tool.exe");
    }
}
