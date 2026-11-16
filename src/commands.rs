//! The subcommands: build, run, test, clean, targets, toolchains, vcpkg, compdb, convert.

use crate::exec::{self, ExecOptions, Summary};
use crate::lockfile::Refresh;
use crate::plan::{self, Plan};
use crate::platform::Platform;
use crate::project::{self, LoadOptions, Model};
use crate::settings::Settings;
use crate::targets::Kind;
use crate::toolchain::{self, Family, Toolchain};
use crate::ui;
use crate::util::{self, slash};
use crate::vcpkg::Instance;
use crate::{BuildArgs, TestArgs, VcpkgCommand};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

// Marks a folder as Anvil's output, so `clean --all` never deletes anything else
const OUT_MARKER: &str = ".anvil-out";
const OUT_MARKER_TEXT: &str = "Anvil build output. `anvil clean --all` deletes this folder.\n";

// A line of the marker naming what else Anvil wrote at the top of the output folder (a JUnit report, a prefix)
const WRITTEN_PREFIX: &str = "wrote ";

// In a profile's output folder (`<out>/<folder>/.anvil/`): the profile it is built with, for `anvil clean -p`
const PROFILE_MARKER: &str = "profile";

// What else Anvil writes at the top of the output folder, beside the profiles' folders
const OUT_FILES: &[&str] = &[OUT_MARKER, "compile_commands.json"];
const OUT_FOLDERS: &[&str] = &["vcpkg", "install", "convert-check"];

// Tries at deleting a profile's folder that another clean may be deleting too
const CLEAN_ATTEMPTS: usize = 3;

// The machines `--target` takes by name, for a mistyped one
const MACHINES: &[&str] = &["x64", "x86", "arm64", "arm"];

pub fn build(start_dir: &Path, build: &BuildArgs, names: &[String], include_tests: bool) -> Result<(Model, Plan)> {
    let model = load_model_naming(start_dir, build, names)?;

    // What to build
    let roots = if names.is_empty() {
        model.default_targets(include_tests)
    } else {
        names.iter().map(|name| model.find_target(name)).collect::<Result<Vec<_>>>()?
    };
    if roots.is_empty() {
        bail!("nothing to build: the project has no targets (add sources under src/, or [target] tables)");
    }

    build_roots(model, build, &roots)
}

pub fn run(start_dir: &Path, build: &BuildArgs, name: Option<&str>, args: &[String]) -> Result<i32> {
    let names: Vec<String> = name.iter().map(|name| name.to_string()).collect();
    let model = load_model_naming(start_dir, build, &names)?;
    let target = choose_program(&model, name)?;
    check_runs_here(&model, "run")?;

    let (model, _plan) = build_roots(model, build, &[target])?;
    let mut setup = run_setup(&model, target, &std::env::current_dir()?)?;
    setup.args.extend(args.iter().cloned());

    ui::status(
        "Running",
        &format!("{} {}", util::rel(&setup.program, &std::env::current_dir()?), setup.args.join(" ")),
    );
    // Ctrl-C reaches the program too: it decides how to end, and Anvil waits for it
    crate::job::FOREGROUND_PROGRAM.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut command = std::process::Command::new(&setup.program);
    command.args(&setup.args).current_dir(&setup.workdir).envs(setup.env.iter().cloned());
    crate::job::end_with_anvil(&mut command);
    let status = command.status();
    crate::job::FOREGROUND_PROGRAM.store(false, std::sync::atomic::Ordering::Relaxed);
    let status = status.with_context(|| format!("starting {}", setup.program.display()))?;
    drop(model);

    // Killed by a signal (a crash) off Windows: said, and the exit code a shell gives (128 + the signal)
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        if signal != libc::SIGINT {
            ui::error(&format!(
                "{} was {}",
                util::rel(&setup.program, &std::env::current_dir()?),
                crate::testrun::describe_exit(status)
            ));
        }
        return Ok(128 + signal);
    }

    Ok(status.code().unwrap_or(1))
}

/// How `anvil run` starts a program (and an IDE's debugger with it): the file, its arguments, folder and environment.
#[derive(Debug)]
pub struct RunSetup {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub workdir: PathBuf,
    pub env: Vec<(String, String)>,
}

/// A test target runs as its test does (its `args`, `workdir` and `env`); another program in `here`, the folder Anvil
/// was started in.
pub fn run_setup(model: &Model, target: usize, here: &Path) -> Result<RunSetup> {
    let program =
        plan::artifact_path(model, &model.targets[target]).with_context(|| format!("`{}` produces no program to run", model.targets[target].name))?;
    if model.targets[target].kind != Kind::Test {
        return Ok(RunSetup {
            program,
            args: Vec::new(),
            workdir: here.to_path_buf(),
            env: Vec::new(),
        });
    }

    // Its test's command is `{target:<itself>} args...`
    let test = model.target_test(target)?;
    let args = test.command[1..]
        .iter()
        .map(|word| plan::substitute_targets(word, model))
        .collect::<Result<Vec<_>>>()?;

    Ok(RunSetup {
        program,
        args,
        workdir: test.workdir,
        env: test.env,
    })
}

pub fn test(start_dir: &Path, build: &BuildArgs, options: &TestArgs) -> Result<i32> {
    // --list only names the tests: nothing to fetch or install
    let mut model = if options.list {
        load_model_for_listing(start_dir, build)?
    } else {
        load_model(start_dir, build)?
    };
    model.collect_tests()?;

    // The tests asked for, and with --rerun-failed only those that failed last time
    let filter = options.filter.as_deref();
    let failed = if options.rerun_failed {
        crate::testrun::last_failed(&model)
    } else {
        Vec::new()
    };
    let mut tests: Vec<_> = model
        .tests
        .iter()
        .filter(|test| filter.is_none_or(|filter| test.name.contains(filter)))
        .filter(|test| !options.rerun_failed || failed.contains(&test.name))
        .cloned()
        .collect();
    if tests.is_empty() && options.rerun_failed {
        ui::status("Testing", "nothing: no test failed last time");
        return Ok(0);
    }
    if tests.is_empty() {
        let hint = if model.tests.is_empty() {
            " (add files under tests/, a kind = \"test\" target, or [[test]] tables)"
        } else {
            ""
        };

        // Listing nothing is an answer, not a failure
        if options.list {
            ui::status("Listed", &format!("no tests{hint}"));
            return Ok(0);
        }
        bail!("no tests to run{hint}");
    }

    if options.list {
        for test in &tests {
            println!("{}", test.name);
        }
        return Ok(0);
    }
    check_runs_here(&model, "test")?;

    // Arguments after `--` go to the tests that run a program of the project, not to commands such as cmake -E
    for test in &mut tests {
        let runs_target = test
            .command
            .first()
            .is_some_and(|program| program.starts_with("{target:") && program.ends_with('}'));
        if runs_target {
            test.command.extend(options.args.iter().cloned());
        }
    }

    // Build every program the tests use
    let mut roots = Vec::new();
    for test in &tests {
        for name in plan::target_references(&test.command) {
            roots.push(model.find_target(&name)?);
        }
    }
    roots.sort();
    roots.dedup();

    let (model, _plan) = if roots.is_empty() {
        (
            model,
            Plan {
                actions: Vec::new(),
                artifacts: Default::default(),
                import_libs: Default::default(),
                runtime_files: Default::default(),
            },
        )
    } else {
        build_roots(model, build, &roots)?
    };

    // The results go into the output folder, and so may the report, which is then Anvil's to delete
    let junit = options
        .junit
        .as_ref()
        .map(|junit| util::absolute(&std::env::current_dir().unwrap_or_default(), junit));
    mark_output(&model.out_root, junit.as_deref())?;
    let options = crate::testrun::TestOptions {
        jobs: jobs(build),
        show_output: options.show_output,
        junit: options.junit.clone(),
    };
    crate::testrun::run_tests(&model, &tests, &options)
}

pub fn clean(start_dir: &Path, release: bool, profile: Option<&str>, all: bool) -> Result<()> {
    let (root, manifest) = project::locate(start_dir)?;
    let out_root = project::output_root(&root, &manifest)?;

    if all {
        let Ok(marker) = std::fs::read_to_string(out_root.join(OUT_MARKER)) else {
            bail!("{} does not look like Anvil's output folder; not deleting it", slash(&out_root));
        };

        // Only what Anvil wrote: profiles' folders, its own files and folders, and what the marker names
        let written = written_entries(&marker);
        let profiles: Vec<&str> = ["debug", "release"]
            .into_iter()
            .chain(manifest.profile.keys().map(|name| name.as_str()))
            .collect();
        let mut strays: Vec<String> = std::fs::read_dir(&out_root)
            .with_context(|| format!("reading {}", slash(&out_root)))?
            .flatten()
            .filter(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let path = entry.path();
                let known = if path.is_dir() {
                    OUT_FOLDERS.contains(&name.as_str())
                        || path.join(".anvil").is_dir()
                        || profiles.iter().any(|profile| is_profile_folder(&path, profile))
                } else {
                    OUT_FILES.contains(&name.as_str())
                };
                !known && !written.iter().any(|written| same_name(written, &name))
            })
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        strays.sort();
        if !strays.is_empty() {
            bail!(
                "{} holds what Anvil did not write ({}); not deleting it. Delete it yourself, or `anvil clean -p <profile>` for one profile's folders",
                slash(&out_root),
                strays.join(", ")
            );
        }

        std::fs::remove_dir_all(&out_root).with_context(|| format!("deleting {}", slash(&out_root)))?;
        ui::status("Removed", &slash(&out_root));
        return Ok(());
    }

    // The profile's folder and its per-toolchain and per-machine variants (debug, debug-clang, debug-arm64, ...)
    let profile = profile.unwrap_or(if release { "release" } else { "debug" });
    let mut removed = 0;
    for entry in std::fs::read_dir(&out_root).into_iter().flatten().flatten() {
        let ours = entry.path().join(".anvil").is_dir();
        if ours && is_profile_folder(&entry.path(), profile) {
            remove_folder(&entry.path())?;
            ui::status("Removed", &slash(&entry.path()));
            removed += 1;
        }
    }

    if removed == 0 {
        ui::status("Clean", &format!("nothing built for `{profile}`"));
    }
    Ok(())
}

/// Deletes a profile's folder. Another `anvil clean` may be deleting it at the same time (an IDE cleans each of its
/// projects at once): it is done when the folder is gone, whoever deleted it.
fn remove_folder(folder: &Path) -> Result<()> {
    for _ in 0..CLEAN_ATTEMPTS {
        if std::fs::remove_dir_all(folder).is_ok() || !folder.exists() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    std::fs::remove_dir_all(folder).with_context(|| format!("deleting {}", slash(folder)))
}

/// Whether `folder`, in the output folder, holds a build with `profile`: the profile its build wrote down, else its
/// name: the profile's, then the toolchain and machine picked on the command line (debug-gcc, debug-clang-arm64).
fn is_profile_folder(folder: &Path, profile: &str) -> bool {
    if let Ok(written) = std::fs::read_to_string(folder.join(".anvil").join(PROFILE_MARKER)) {
        return written.trim() == profile;
    }

    let name = folder.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    if name == profile {
        return true;
    }
    let Some(suffix) = name.strip_prefix(profile).and_then(|rest| rest.strip_prefix('-')) else {
        return false;
    };
    let families = [Family::Msvc, Family::ClangCl, Family::Clang, Family::Gcc].map(Family::name);
    let after_family = families.iter().any(|family| match suffix.strip_prefix(family) {
        Some("") => true,
        Some(rest) => rest.strip_prefix('-').is_some_and(|machine| Platform::parse(machine).is_ok()),
        None => false,
    });

    after_family || Platform::parse(suffix).is_ok()
}

pub fn list_targets(start_dir: &Path, build: &BuildArgs) -> Result<()> {
    let mut model = load_model_for_listing(start_dir, build)?;
    model.collect_tests()?;

    for target in &model.targets {
        let package = &model.packages[target.package];
        if !package.member && target.kind != Kind::External {
            continue;
        }

        // name, kind, sources, dependencies
        let deps: Vec<&str> = target
            .deps
            .iter()
            .map(|(dependency, _)| model.targets[*dependency].name.as_str())
            .collect();
        let sources = if target.sources.is_empty() {
            String::new()
        } else {
            format!("{} sources", target.sources.len())
        };
        let deps_text = if deps.is_empty() {
            String::new()
        } else {
            format!("-> {}", deps.join(", "))
        };

        println!("{:<24} {:<9} {:<12} {}", target.name, target.kind.name(), sources, ui::dim(&deps_text));
    }

    if !model.tests.is_empty() {
        println!();
        println!(
            "tests: {}",
            model.tests.iter().map(|test| test.name.as_str()).collect::<Vec<_>>().join(", ")
        );
    }

    Ok(())
}

pub fn toolchains(refresh: bool, set_default: Option<&str>) -> Result<()> {
    if refresh {
        Toolchain::clear_cache();
    }

    if let Some(name) = set_default {
        Family::parse(name)?;
        let mut settings = Settings::load();
        settings.toolchain = Some(name.to_string());
        settings.save()?;
        ui::status("Default", &format!("toolchain is now {name}"));
    }

    let chosen = Toolchain::find(Settings::load().toolchain.as_deref(), &Platform::host())
        .ok()
        .map(|toolchain| toolchain.family);
    for (family, found) in Toolchain::find_all() {
        let marker = if chosen == Some(family) { ui::bold_green("*") } else { " ".into() };
        match found {
            Ok(toolchain) => println!("{marker} {:<9} {:<14} {}", family.name(), toolchain.version, slash(&toolchain.cxx)),
            Err(error) => println!("  {:<9} {}", family.name(), ui::dim(&format!("not found ({error:#})"))),
        }
    }

    println!();
    println!(
        "{}",
        ui::dim("* = used when a project names none. Change with `anvil toolchains --set-default <name>`.")
    );
    Ok(())
}

pub fn vcpkg(start_dir: &Path, action: Option<VcpkgCommand>, args: &[String]) -> Result<i32> {
    // `anvil vcpkg -- <args>`: run the selected vcpkg
    if action.is_none() && !args.is_empty() {
        let mut instance = Instance::select(project_vcpkg_root(start_dir).as_deref())?;
        return instance.run_passthrough(args);
    }

    match action.unwrap_or(VcpkgCommand::List) {
        VcpkgCommand::List => list_vcpkgs(start_dir)?,
        VcpkgCommand::Use { which } => use_vcpkg(&which)?,
        VcpkgCommand::Add { path } => {
            let instance = Instance::at(&path, "added").with_context(|| format!("{} is not a vcpkg folder (no .vcpkg-root)", slash(&path)))?;
            let mut settings = Settings::load();
            settings.vcpkg_extra.push(instance.root.clone());
            settings.save()?;
            ui::status("Added", &slash(&instance.root));
        }
        VcpkgCommand::Setup => {
            let instance = Instance::setup()?;
            ui::status("Ready", &slash(&instance.root));
        }
        VcpkgCommand::Search { term } => {
            let mut instance = Instance::select(project_vcpkg_root(start_dir).as_deref())?;
            return instance.run_passthrough(&["search".to_string(), term]);
        }
    }

    Ok(0)
}

pub fn compdb(start_dir: &Path, build: &BuildArgs) -> Result<()> {
    let model = load_model(start_dir, build)?;
    let roots = model.default_targets(true);
    plan::plan(&model, None, &roots, false)?;

    let source = model.out_dir.join("compile_commands.json");
    let destination = model.root.join("compile_commands.json");
    std::fs::copy(&source, &destination).with_context(|| format!("copying {}", source.display()))?;
    ui::status("Wrote", &slash(&destination));

    Ok(())
}

pub fn convert(start_dir: &Path, path: Option<&Path>, to_stdout: bool, force: bool) -> Result<()> {
    let folder = util::absolute(&std::env::current_dir()?, path.unwrap_or(start_dir));
    let conversion = crate::convert::convert(&folder)?;

    if to_stdout {
        print!("{}", conversion.toml);
        for (path, text) in &conversion.tests_files {
            print!("\n# ==> {} <==\n{text}", slash(path));
        }
    } else {
        let destination = folder.join(project::MANIFEST_NAME);
        let destinations = std::iter::once(&destination).chain(conversion.tests_files.iter().map(|(path, _)| path));
        for existing in destinations.filter(|path| path.exists()) {
            if !force {
                bail!(
                    "{} already exists (use --force to replace it, or --stdout to see the result)",
                    slash(existing)
                );
            }
        }
        std::fs::write(&destination, &conversion.toml)?;
        for (path, text) in &conversion.tests_files {
            std::fs::write(path, text)?;
        }

        // Headers CMake generated at configure time; an existing file is the user's
        for (path, content) in &conversion.files {
            if path.exists() && !force {
                ui::warn(&format!("{} already exists; not replaced", slash(path)));
                continue;
            }
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(path, content)?;
            ui::status("Generated", &slash(path));
        }

        ui::status(
            "Converted",
            &format!("{} -> {}", slash(&folder.join("CMakeLists.txt")), slash(&destination)),
        );
    }

    for warning in &conversion.warnings {
        ui::warn(warning);
    }
    ui::note(&format!(
        "{} targets, {} dependencies, {} things to review (marked TODO in the file)",
        conversion.target_count, conversion.dependency_count, conversion.todo_count
    ));

    Ok(())
}

/// `anvil convert --check`: the converted project's compile commands against CMake's.
pub fn convert_check(start_dir: &Path, path: Option<&Path>, build: &BuildArgs) -> Result<i32> {
    let folder = util::absolute(&std::env::current_dir()?, path.unwrap_or(start_dir));
    if !folder.join(project::MANIFEST_NAME).is_file() {
        bail!("no {} in {}; run `anvil convert` first", project::MANIFEST_NAME, slash(&folder));
    }

    let model = load_model(&folder, build)?;
    crate::convert_check::check(&model)
}

/// `anvil update [names]`: re-resolves git dependencies (all, or those named) and the vcpkg baseline (all, or
/// `vcpkg`) to what their references name now, installs that, and rewrites anvil.lock.
pub fn update(start_dir: &Path, build: &BuildArgs, names: &[String]) -> Result<()> {
    let refresh = if names.is_empty() {
        Refresh::Everything
    } else {
        Refresh::Only(names.to_vec())
    };

    // What can be updated: the lock's pins, and the git and vcpkg dependencies anvil.toml declares
    let (root, manifest) = project::locate(start_dir)?;
    let lock_file = root.join(crate::lockfile::LOCK_NAME);
    let before = std::fs::read_to_string(&lock_file).ok();
    let mut known = match &before {
        Some(text) => toml::from_str::<crate::lockfile::Lock>(text)
            .with_context(|| format!("{} is not a valid lock file", slash(&lock_file)))?
            .names(),
        None => Vec::new(),
    };
    for (name, value) in manifest.deps.iter().chain(&manifest.dev_deps) {
        let spec = crate::manifest::parse_dep(name, value)?;
        let updatable = if spec.git.is_some() {
            Some(name.as_str())
        } else {
            spec.vcpkg.as_ref().map(|_| "vcpkg")
        };
        if let Some(updatable) = updatable.filter(|updatable| !known.iter().any(|known| known == updatable)) {
            known.push(updatable.to_string());
        }
    }

    // A name that is not one of them is a typo, not a request to do nothing
    if let Some(unknown) = names.iter().find(|name| !known.contains(name)) {
        let hint = util::did_you_mean(unknown, known.iter().map(|known| known.as_str()));
        if known.is_empty() {
            bail!("`{unknown}` is not a git dependency or vcpkg: this project has neither, so there is nothing to update");
        }
        bail!(
            "`{unknown}`{hint} is not a git dependency or vcpkg. `anvil update` takes: {}",
            known.join(", ")
        );
    }

    let model = load_model_with(start_dir, build, refresh, false, &[])?;
    let after = std::fs::read_to_string(&lock_file).ok();
    if after.is_none() {
        ui::status("Updated", "nothing to lock (no git or vcpkg dependencies)");
    } else if after != before {
        ui::status("Updated", &slash(&lock_file));
    } else {
        ui::status("Fresh", "anvil.lock already has the newest versions");

        // A vcpkg checkout offers only the ports it has: the newest are a pull away
        let updates_vcpkg = names.is_empty() || names.iter().any(|name| name == "vcpkg");
        if let Some(instance) = model.vcpkg.as_ref().filter(|instance| updates_vcpkg && instance.has_ports) {
            ui::note(&format!(
                "vcpkg's newest ports are those its checkout has; `git -C {} pull` fetches newer ones",
                slash(&instance.root)
            ));
        }
    }

    Ok(())
}

/// Loads the model for the command line's profile, toolchain and options.
pub fn load_model(start_dir: &Path, build: &BuildArgs) -> Result<Model> {
    load_model_with(start_dir, build, Refresh::Nothing, false, &[])
}

/// Loads the model keeping the targets `names` name, though nothing of the project reaches them (`dep/tool`).
pub fn load_model_naming(start_dir: &Path, build: &BuildArgs, names: &[String]) -> Result<Model> {
    load_model_with(start_dir, build, Refresh::Nothing, false, names)
}

/// Loads the model to list what it holds: no vcpkg install, no git fetch, anvil.lock untouched.
pub fn load_model_for_listing(start_dir: &Path, build: &BuildArgs) -> Result<Model> {
    load_model_with(start_dir, build, Refresh::Nothing, true, &[])
}

fn load_model_with(start_dir: &Path, build: &BuildArgs, refresh: Refresh, list_only: bool, named_targets: &[String]) -> Result<Model> {
    let profile = match (&build.profile, build.release) {
        (Some(profile), _) => profile.clone(),
        (None, true) => "release".to_string(),
        (None, false) => "debug".to_string(),
    };

    // `--target game`: a target's name where the machine goes; else a machine's name mistyped
    if let Some(platform) = &build.platform {
        if let Err(error) = Platform::parse(platform) {
            let declared = project::locate(start_dir).is_ok_and(|(root, manifest)| {
                manifest.target.contains_key(platform) || (manifest.target.is_empty() && project::package_name(&root, &manifest) == *platform)
            });
            if declared {
                bail!("`--target {platform}`: `--target` picks the machine to build for (x64, x86, arm64, arm, or a triple such as aarch64-linux-gnu); to build target `{platform}`, write `anvil build {platform}`");
            }
            let hint = match platform.contains('-') {
                true => String::new(),
                false => util::did_you_mean(&platform.to_ascii_lowercase(), MACHINES.iter().copied()),
            };
            bail!("{error}{hint}");
        }
    }

    Model::load(&LoadOptions {
        start_dir: start_dir.to_path_buf(),
        profile,
        toolchain: build.toolchain.clone(),
        options: parse_option_overrides(&build.options)?,
        target: build.platform.clone(),
        locked: build.locked,
        refresh,
        host_tools_dir: None,
        list_only,
        named_targets: named_targets.to_vec(),
    })
}

/// Programs built for another machine cannot start here.
fn check_runs_here(model: &Model, command: &str) -> Result<()> {
    let platform = &model.toolchain.platform;
    if platform.runs_on_host() {
        return Ok(());
    }

    bail!(
        "`anvil {command}` cannot start programs built for {} on this machine ({}); copy them to one that runs them",
        platform.name(),
        Platform::host().name()
    )
}

/// Creates the output folder marked as Anvil's, so that `clean --all` deletes it. `written` is a path Anvil writes
/// besides its own folders (a JUnit report, an install prefix): when it is inside, the marker names its top entry, which
/// `clean --all` then deletes too.
pub fn mark_output(out_root: &Path, written: Option<&Path>) -> Result<()> {
    std::fs::create_dir_all(out_root).with_context(|| format!("creating {}", slash(out_root)))?;
    let marker = out_root.join(OUT_MARKER);
    let mut text = std::fs::read_to_string(&marker).unwrap_or_else(|_| OUT_MARKER_TEXT.to_string());

    let entry = written
        .and_then(|path| util::strip_prefix_ci(path, out_root))
        .and_then(|inside| inside.components().next().map(|first| first.as_os_str().to_string_lossy().into_owned()));
    if let Some(entry) = entry.filter(|entry| !written_entries(&text).iter().any(|known| same_name(known, entry))) {
        text.push_str(&format!("{WRITTEN_PREFIX}{entry}\n"));
    }

    util::write_if_changed(&marker, text.as_bytes())?;
    Ok(())
}

/// The entries the output folder's marker says Anvil wrote at its top.
fn written_entries(marker: &str) -> Vec<&str> {
    marker.lines().filter_map(|line| line.strip_prefix(WRITTEN_PREFIX)).collect()
}

fn same_name(left: &str, right: &str) -> bool {
    util::path_key(Path::new(left)) == util::path_key(Path::new(right))
}

/// Plans and runs the actions `roots` need, then prints the summary line.
pub fn build_roots(mut model: Model, build: &BuildArgs, roots: &[usize]) -> Result<(Model, Plan)> {
    // The compiler cache, needed now that compiles run
    model.launcher = toolchain::find_launcher(model.launcher_setting.as_deref(), &model.root)?;
    if let Some(launcher) = &model.launcher {
        toolchain::check_launcher(launcher, model.toolchain.family);
    }

    mark_output(&model.out_root, None)?;
    let state_dir = model.out_dir.join(".anvil");
    std::fs::create_dir_all(&state_dir)?;
    std::fs::write(state_dir.join(PROFILE_MARKER), format!("{}\n", model.profile.name))?;

    // A cross build's steps run programs built for this machine, from the project loaded for it
    let host = if model.toolchain.platform.is_cross() && !plan::step_programs(&model, roots)?.is_empty() {
        let mut host = model
            .load_host_tools()
            .context("loading the project for this machine (for programs build steps run)")?;
        host.launcher = model.launcher.clone();
        Some(host)
    } else {
        None
    };

    let plan = plan::plan(&model, host.as_ref(), roots, ui::color())?;
    crate::ide::refresh(&model, build);
    let options = ExecOptions {
        jobs: jobs(build),
        keep_going: build.keep_going,
        verbose: build.verbose,
        explain: build.explain,
        low_priority: build.low_priority,
        root: model.root.clone(),
    };
    let summary = exec::execute(&plan, &model.toolchain, &model.out_dir.join(".anvil"), &options)?;

    if summary.failed > 0 {
        if build.timings {
            print_timings(&model, &summary);
        }
        bail!("build failed ({} of {} steps)", summary.failed, summary.failed + summary.ran);
    }
    print_summary(&model, &summary);
    if build.timings {
        print_timings(&model, &summary);
    }

    Ok((model, plan))
}

/// --timings: the slowest actions and the time per kind, and where the trace is.
fn print_timings(model: &Model, summary: &Summary) {
    let trace = model.out_dir.join(".anvil").join(crate::timings::TRACE_FILE);
    let here = std::env::current_dir().unwrap_or_default();

    // Nothing ran: say so, and where the last build's trace is
    if summary.timings.is_empty() {
        let last = if trace.is_file() {
            format!("; the last trace: {}", util::rel(&trace, &here))
        } else {
            String::new()
        };
        ui::print_line(&format!("Timings: nothing ran{last}"));
        return;
    }

    crate::timings::print_report(&summary.timings, &util::rel(&trace, &here));
}

fn print_summary(model: &Model, summary: &Summary) {
    let seconds = summary.elapsed.as_secs_f64();
    let mut toolchain = format!("{} {}", model.toolchain.family.name(), model.toolchain.version);
    if model.toolchain.platform.is_cross() {
        toolchain.push_str(&format!(" for {}", model.toolchain.platform.name()));
    }

    if summary.ran == 0 {
        ui::status("Fresh", &format!("{} [{toolchain}] up to date ({seconds:.2}s)", model.profile.name));
        return;
    }

    let mut counts = Vec::new();
    if summary.compiled > 0 {
        counts.push(format!("{} compiled", summary.compiled));
    }
    if summary.linked > 0 {
        counts.push(format!("{} linked", summary.linked));
    }
    let other = summary.ran - summary.compiled - summary.linked;
    if other > 0 {
        counts.push(format!("{other} other"));
    }

    ui::status(
        "Finished",
        &format!("{} [{toolchain}] in {seconds:.2}s ({})", model.profile.name, counts.join(", ")),
    );
}

/// One job per CPU plus two, like Ninja: a compiler spends part of its life starting up and waiting on the disk.
pub fn jobs(build: &BuildArgs) -> usize {
    build
        .jobs
        .unwrap_or_else(|| std::thread::available_parallelism().map(|count| count.get() + 2).unwrap_or(4))
}

/// `name=true`, `name=off`, `name`, `!name`.
fn parse_option_overrides(texts: &[String]) -> Result<Vec<(String, bool)>> {
    let mut overrides = Vec::new();

    for text in texts {
        let (name, value) = match text.split_once('=') {
            Some((name, value)) => {
                let value = match value.to_ascii_lowercase().as_str() {
                    "true" | "on" | "yes" | "1" => true,
                    "false" | "off" | "no" | "0" => false,
                    other => bail!("-o {text}: `{other}` is not true or false"),
                };
                (name.to_string(), value)
            }
            None => match text.strip_prefix('!') {
                Some(name) => (name.to_string(), false),
                None => (text.clone(), true),
            },
        };
        overrides.push((name, value));
    }

    Ok(overrides)
}

/// The program `anvil run` starts: the named one, [project] default-run, or the only one.
pub fn choose_program(model: &Model, name: Option<&str>) -> Result<usize> {
    if let Some(name) = name.or(model.default_run.as_deref()) {
        let index = model.find_target(name)?;
        if !model.targets[index].kind.is_executable() {
            bail!("`{name}` is a {}, not a program", model.targets[index].kind.name());
        }
        return Ok(index);
    }

    let programs: Vec<usize> = (0..model.targets.len())
        .filter(|index| {
            let target = &model.targets[*index];
            target.kind == Kind::Exe && model.packages[target.package].member
        })
        .collect();

    match programs.as_slice() {
        [only] => Ok(*only),
        [] => bail!("this project has no program to run"),
        _ => {
            let names: Vec<&str> = programs.iter().map(|index| model.targets[*index].name.as_str()).collect();
            bail!(
                "several programs: {}. Say which (`anvil run <name>`) or set [project] default-run",
                names.join(", ")
            )
        }
    }
}

fn list_vcpkgs(start_dir: &Path) -> Result<()> {
    let instances = Instance::discover();
    let selected = Instance::select(project_vcpkg_root(start_dir).as_deref()).ok();

    if instances.is_empty() {
        println!("No vcpkg found. Anvil will set one up the first time a project needs it (or run `anvil vcpkg setup`).");
        return Ok(());
    }

    for (number, instance) in instances.iter().enumerate() {
        let active = selected
            .as_ref()
            .is_some_and(|selected| util::path_key(&selected.root) == util::path_key(&instance.root));
        let marker = if active { ui::bold_green("*") } else { " ".into() };
        let version = instance.version().unwrap_or_else(|| "not bootstrapped".into());
        let kind = if instance.has_ports { "ports tree" } else { "registry only" };

        println!("{marker} {}  {}", number + 1, slash(&instance.root));
        println!("     {}", ui::dim(&format!("found via {}, {kind}, tool {version}", instance.source)));
    }

    println!();
    println!("{}", ui::dim("* = used for builds. Switch with `anvil vcpkg use <number|folder>`."));
    Ok(())
}

fn use_vcpkg(which: &str) -> Result<()> {
    let instances = Instance::discover();

    // A number from the list, else a folder
    let chosen = match which.parse::<usize>() {
        Ok(number) if number >= 1 && number <= instances.len() => instances[number - 1].clone(),
        Ok(number) => bail!("there is no vcpkg number {number} (see `anvil vcpkg list`)"),
        Err(_) => Instance::at(&PathBuf::from(which), "your choice").with_context(|| format!("{which} is not a vcpkg folder (no .vcpkg-root)"))?,
    };

    let mut settings = Settings::load();
    settings.vcpkg_root = Some(chosen.root.clone());
    settings.save()?;
    ui::status("Using", &format!("vcpkg at {}", slash(&chosen.root)));

    Ok(())
}

/// `[vcpkg] root` of the project in `start_dir`, if it sets one.
fn project_vcpkg_root(start_dir: &Path) -> Option<PathBuf> {
    let (root, manifest) = project::locate(start_dir).ok()?;
    let configured = manifest.vcpkg?.root?;
    Some(util::absolute(&root, Path::new(&util::expand_env(&configured).ok()?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_folders_by_marker_else_by_name() {
        let out_root = std::env::temp_dir().join(format!("anvil_profile_folders_{}", std::process::id()));
        for (folder, marker) in [
            ("rel", None),
            ("rel-fast", None),
            ("rel-gcc", None),
            ("rel-clang-cl-arm64", None),
            ("rel-x86", None),
            ("odd", Some("rel")),
        ] {
            std::fs::create_dir_all(out_root.join(folder).join(".anvil")).unwrap();
            if let Some(profile) = marker {
                std::fs::write(out_root.join(folder).join(".anvil").join(PROFILE_MARKER), format!("{profile}\n")).unwrap();
            }
        }
        let matches = |folder: &str, profile: &str| is_profile_folder(&out_root.join(folder), profile);

        // Without a marker: the profile's name, then a toolchain or machine
        assert!(matches("rel", "rel"));
        assert!(matches("rel-gcc", "rel"));
        assert!(matches("rel-clang-cl-arm64", "rel"));
        assert!(matches("rel-x86", "rel"));
        assert!(!matches("rel-fast", "rel"));
        assert!(matches("rel-fast", "rel-fast"));

        // The marker decides
        assert!(matches("odd", "rel"));
        std::fs::write(out_root.join("rel-gcc").join(".anvil").join(PROFILE_MARKER), "rel-gcc\n").unwrap();
        assert!(!matches("rel-gcc", "rel"));

        let _ = std::fs::remove_dir_all(&out_root);
    }

    #[test]
    fn a_mistyped_machine_gets_a_suggestion() {
        assert!(util::did_you_mean("x68", MACHINES.iter().copied()).starts_with(" (did you mean `x"));
        assert_eq!(util::did_you_mean("arm64ec", MACHINES.iter().copied()), " (did you mean `arm64`?)");
        assert_eq!(util::did_you_mean("game", MACHINES.iter().copied()), "");
    }
}
