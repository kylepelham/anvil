//! Anvil: a fast, zero-ceremony build system for C and C++.

mod cmake_package;
mod cmake_tool;
mod commands;
mod cond;
mod convert;
mod convert_check;
mod cuda;
mod db;
mod delay_load;
mod depfile;
mod exec;
mod export_header;
mod flags;
mod ide;
mod install;
mod job;
mod lint;
mod lockfile;
mod lsp;
mod manifest;
mod plan;
mod platform;
mod profile;
mod project;
mod qt;
mod scaffold;
mod schema;
mod settings;
mod source_hash;
mod targets;
mod test_cases;
mod testrun;
mod timings;
mod toolchain;
mod ui;
mod util;
mod vcpkg;
mod vcpkg_tree;

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "anvil",
    bin_name = "anvil",
    version,
    about = "A fast, zero-ceremony build system for C and C++",
    propagate_version = true
)]
struct Cli {
    /// Run as if started in this folder.
    #[arg(short = 'C', long = "dir", global = true, value_name = "DIR")]
    dir: Option<PathBuf>,

    /// Print less.
    #[arg(short, long, global = true)]
    quiet: bool,

    #[command(subcommand)]
    command: Option<CommandLine>,
}

#[derive(Subcommand)]
enum CommandLine {
    /// Build the project (the default command).
    #[command(visible_alias = "b")]
    Build {
        /// Targets to build (default: all but tests).
        targets: Vec<String>,

        /// Build the tests too.
        #[arg(long)]
        all: bool,

        #[command(flatten)]
        plan: PlanArgs,

        #[command(flatten)]
        run: RunArgs,
    },

    /// Build and run a program; arguments after `--` go to it.
    #[command(visible_alias = "r")]
    Run {
        /// The program (default: the only one, or [project] default-run).
        target: Option<String>,

        #[command(flatten)]
        plan: PlanArgs,

        #[command(flatten)]
        run: RunArgs,

        /// Arguments for the program.
        #[arg(last = true)]
        args: Vec<String>,
    },

    /// Build and run the tests; arguments after `--` go to every test that runs a program of the project
    /// ({target:...}).
    #[command(visible_alias = "t")]
    Test {
        #[command(flatten)]
        test: TestArgs,

        #[command(flatten)]
        plan: PlanArgs,

        #[command(flatten)]
        run: RunArgs,
    },

    /// Build (with the release profile unless -p names another), then copy the programs, libraries and public headers
    /// into a prefix (bin/, lib/, include/), with CMake and pkg-config files for the libraries.
    Install {
        /// Targets to install (default: every default program and library).
        targets: Vec<String>,

        /// Where to install (default: out/install).
        #[arg(long, value_name = "DIR")]
        prefix: Option<PathBuf>,

        #[command(flatten)]
        plan: PlanArgs,

        #[command(flatten)]
        run: RunArgs,
    },

    /// Delete build output (the profile's folder, or everything with --all).
    Clean {
        /// Only these targets' objects, program or library (what an IDE's Clean and Rebuild of a project delete).
        targets: Vec<String>,

        /// The release profile's folders.
        #[arg(short, long, conflicts_with = "profile")]
        release: bool,

        /// A named profile's folders.
        #[arg(short, long, value_name = "NAME")]
        profile: Option<String>,

        /// Only the folder of builds with this toolchain.
        #[arg(long, value_name = "NAME")]
        toolchain: Option<String>,

        /// Only the folder of builds for this machine.
        #[arg(long = "target", id = "platform", value_name = "PLATFORM")]
        platform: Option<String>,

        /// An option the builds set (-o name=true|false), for finding their targets and folder.
        #[arg(short = 'o', long = "option", value_name = "NAME=VALUE")]
        options: Vec<String>,

        /// The whole output folder, vcpkg packages included.
        #[arg(long, conflicts_with_all = ["targets", "toolchain", "platform"])]
        all: bool,
    },

    /// Create a new project folder.
    New {
        /// The project's name and folder.
        name: String,

        /// A library (with a test) instead of a program.
        #[arg(long)]
        lib: bool,
    },

    /// Create an anvil.toml for the sources in this folder (or convert its CMakeLists.txt).
    Init,

    /// Add a dependency to anvil.toml (vcpkg by default).
    Add(scaffold::AddArgs),

    /// Remove a dependency from anvil.toml (and from vcpkg.json).
    Remove {
        /// The [deps] or [dev-deps] entry.
        name: String,
    },

    /// Convert a CMake project's CMakeLists.txt into anvil.toml.
    #[command(mut_group("PlanArgs", |group| group.requires("check")))]
    Convert {
        /// The CMake project's folder (default: here).
        path: Option<PathBuf>,

        /// Print the result instead of writing anvil.toml.
        #[arg(long)]
        stdout: bool,

        /// Overwrite an existing anvil.toml.
        #[arg(long)]
        force: bool,

        /// Compare the converted build with CMake's: configure CMake (Ninja), plan Anvil, and diff each file's
        /// definitions, include folders, standard and other options. Needs cmake and ninja; uses the existing anvil.toml.
        #[arg(long, conflicts_with_all = ["stdout", "force"])]
        check: bool,

        #[command(flatten)]
        plan: PlanArgs,
    },

    /// List the project's targets.
    Targets {
        /// Print the targets, tests and toolchain as JSON, for editors: each program's file and how `anvil run`
        /// starts it. Builds and installs nothing.
        #[arg(long)]
        json: bool,

        #[command(flatten)]
        plan: PlanArgs,
    },

    /// Write the project files of an IDE (vs: a solution and projects; vscode: tasks, launch and settings; clion,
    /// zed): build, run and debug every program from it. Every build keeps them up to date.
    Ide {
        /// The IDEs (default: those set up before, and those whose folders are here).
        #[arg(value_enum)]
        ides: Vec<ide::Ide>,

        #[command(flatten)]
        plan: PlanArgs,
    },

    /// Show the compilers Anvil can use.
    #[command(visible_alias = "doctor")]
    Toolchains {
        /// Detect again instead of using the cache.
        #[arg(long)]
        refresh: bool,

        /// Make this toolchain the default for projects that do not name one.
        #[arg(long, value_name = "NAME")]
        set_default: Option<String>,
    },

    /// See and choose vcpkg installations, or run vcpkg (`anvil vcpkg -- search fmt`).
    #[command(
        args_conflicts_with_subcommands = true,
        override_usage = "anvil vcpkg [COMMAND]\n       anvil vcpkg -- <ARGS>..."
    )]
    Vcpkg {
        #[command(subcommand)]
        action: Option<VcpkgCommand>,

        /// Arguments passed to the selected vcpkg.
        #[arg(last = true)]
        args: Vec<String>,
    },

    /// Write compile_commands.json to the project root (for clangd).
    Compdb {
        #[command(flatten)]
        plan: PlanArgs,
    },

    /// Print the JSON Schema of anvil.toml for editors (Taplo, Even Better TOML, JetBrains).
    Schema {
        /// The schema of tests.toml instead.
        #[arg(long, conflicts_with = "install")]
        tests: bool,

        /// Write both schemas to the user's data folder (beside settings.json) and say how to point an editor at them.
        #[arg(long)]
        install: bool,
    },

    /// Format the C and C++ files with clang-format, as the project's .clang-format says.
    Fmt {
        /// Files or folders (default: every package's sources and headers).
        paths: Vec<PathBuf>,

        /// Change nothing: list the files that are not formatted, and exit with 1 when there are any.
        #[arg(long)]
        check: bool,
    },

    /// Check the targets' sources with clang-tidy (the project's .clang-tidy) and the profile's compile commands.
    Tidy {
        /// Targets to check (default: every one of the project's, tests too).
        targets: Vec<String>,

        /// Apply the fixes clang-tidy suggests.
        #[arg(long)]
        fix: bool,

        #[command(flatten)]
        plan: PlanArgs,

        #[command(flatten)]
        exec: ExecArgs,
    },

    /// Move git dependencies to the newest commit of their branch or tag and vcpkg to its newest ports, and rewrite
    /// anvil.lock (`anvil update json` for one dependency, `anvil update vcpkg` for vcpkg only).
    Update {
        /// Git dependencies to update, or `vcpkg` (default: everything).
        names: Vec<String>,

        #[command(flatten)]
        plan: PlanArgs,
    },

    /// Run the language server for anvil.toml and tests.toml (LSP over stdin and stdout, for editors).
    Lsp {
        /// Log what the server does to stderr and to the editor's log.
        #[arg(long)]
        verbose: bool,

        /// Accepted for editors that pass it; stdio is the only transport.
        #[arg(long, hide = true)]
        stdio: bool,

        /// Accepted for editors that pass it.
        #[arg(long = "clientProcessId", hide = true)]
        client_process_id: Option<String>,
    },

    /// Print a shell completion script (`anvil completions powershell >> $PROFILE`).
    Completions { shell: clap_complete::Shell },
}

#[derive(Subcommand)]
enum VcpkgCommand {
    /// List every vcpkg found on this machine.
    List,

    /// Use this vcpkg from now on (a number from `list`, or a folder).
    Use {
        /// A number from `anvil vcpkg list`, or a vcpkg folder.
        which: String,
    },

    /// Register a vcpkg in a non-standard folder.
    Add { path: PathBuf },

    /// Download and bootstrap a private vcpkg (only needed without one).
    Setup,

    /// Search the selected vcpkg's ports.
    Search {
        /// Part of a port's name or description.
        term: String,
    },
}

#[derive(Args)]
pub struct TestArgs {
    /// Only tests whose name contains this.
    pub filter: Option<String>,

    /// Print the tests that would run, without building or running them.
    #[arg(long)]
    pub list: bool,

    /// Run only the tests that failed last time (and have not passed since).
    #[arg(long)]
    pub rerun_failed: bool,

    /// Print the output of passing tests too.
    #[arg(long)]
    pub show_output: bool,

    /// Write a JUnit XML report to this file.
    #[arg(long, value_name = "FILE")]
    pub junit: Option<PathBuf>,

    /// Arguments for the tests that run a program of the project.
    #[arg(last = true)]
    pub args: Vec<String>,
}

/// What a command plans: profile, toolchain, machine, options, lock.
#[derive(Args, Clone, Default)]
pub struct PlanArgs {
    /// Build with the release profile.
    #[arg(short, long)]
    pub release: bool,

    /// Build with a named profile.
    #[arg(short, long, value_name = "NAME", conflicts_with = "release")]
    pub profile: Option<String>,

    /// msvc, clang-cl, clang or gcc (default: the project's, else `anvil toolchains --set-default`'s, else the
    /// compiler CXX names, else the best installed).
    #[arg(long, value_name = "NAME")]
    pub toolchain: Option<String>,

    /// Build for another machine: x64, x86, arm64, arm, or a triple such as aarch64-linux-gnu.
    #[arg(long = "target", id = "platform", value_name = "PLATFORM")]
    pub platform: Option<String>,

    /// Fail instead of changing anvil.lock (for CI).
    #[arg(long)]
    pub locked: bool,

    /// Set an option: -o name=true|false (or -o name, -o !name).
    #[arg(short = 'o', long = "option", value_name = "NAME=VALUE")]
    pub options: Vec<String>,
}

/// How a command runs the actions it planned.
#[derive(Args, Clone, Default)]
pub struct ExecArgs {
    /// Parallel jobs (default: one per CPU, plus two).
    #[arg(short, long)]
    pub jobs: Option<usize>,

    /// Keep building after a failure.
    #[arg(short, long)]
    pub keep_going: bool,

    /// Print every command.
    #[arg(short, long)]
    pub verbose: bool,

    /// Say why each action runs.
    #[arg(long)]
    pub explain: bool,

    /// Run compilers at below-normal priority (keeps the machine responsive).
    #[arg(long)]
    pub low_priority: bool,
}

/// How a build runs: its actions, and the timings it reports after.
#[derive(Args, Clone, Default)]
pub struct RunArgs {
    #[command(flatten)]
    pub exec: ExecArgs,

    /// After the build, print the slowest actions and the time per kind of action.
    #[arg(long)]
    pub timings: bool,
}

/// What the commands that load a project take: the command line's planning and run options together.
#[derive(Clone, Default)]
pub struct BuildArgs {
    pub release: bool,
    pub profile: Option<String>,
    pub jobs: Option<usize>,
    pub keep_going: bool,
    pub verbose: bool,
    pub explain: bool,
    pub timings: bool,
    pub toolchain: Option<String>,
    pub platform: Option<String>,
    pub locked: bool,
    pub options: Vec<String>,
    pub low_priority: bool,
}

impl BuildArgs {
    pub fn new(plan: PlanArgs, run: RunArgs) -> BuildArgs {
        BuildArgs {
            release: plan.release,
            profile: plan.profile,
            jobs: run.exec.jobs,
            keep_going: run.exec.keep_going,
            verbose: run.exec.verbose,
            explain: run.exec.explain,
            timings: run.timings,
            toolchain: plan.toolchain,
            platform: plan.platform,
            locked: plan.locked,
            options: plan.options,
            low_priority: run.exec.low_priority,
        }
    }

    /// A command that only plans: the run options keep their defaults.
    pub fn planning(plan: PlanArgs) -> BuildArgs {
        BuildArgs::new(plan, RunArgs::default())
    }
}

fn main() {
    // Build steps' `anvil cmake-e <command>`: no project, no output of Anvil's own; not a subcommand clap knows, so
    // help and completions do not offer it
    if std::env::args_os().nth(1).is_some_and(|first| first == cmake_tool::SUBCOMMAND) {
        let args: Vec<String> = std::env::args().skip(2).collect();
        std::process::exit(cmake_tool::run(&args));
    }

    let cli = Cli::parse();
    ui::init(cli.quiet);

    // Ctrl-C: finish what is running, save the build database, stop. `anvil run`'s program handles its own.
    let _ = ctrlc::set_handler(|| {
        if job::FOREGROUND_PROGRAM.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        if exec::INTERRUPTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            std::process::exit(130);
        }
    });

    // Whoever kills Anvil stops what it started too, and only that
    job::contain_self();
    job::keep_handles_to_self();
    job::separate_pdb_server();

    // -C: run in that folder, so every relative path (`anvil -C x convert sub`) starts there
    let shell_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Some(dir) = &cli.dir {
        let entered = if dir.is_dir() {
            std::env::set_current_dir(dir).map_err(|error| error.to_string())
        } else {
            Err("no such folder".to_string())
        };
        if let Err(reason) = entered {
            ui::error(&format!("-C {}: {reason}", dir.display()));
            std::process::exit(1);
        }
    }

    let start_dir = PathBuf::from(".");
    let command = cli.command.unwrap_or(CommandLine::Build {
        targets: Vec::new(),
        all: false,
        plan: PlanArgs::default(),
        run: RunArgs::default(),
    });

    let result = match command {
        CommandLine::Build { targets, all, plan, run } => commands::build(&start_dir, &BuildArgs::new(plan, run), &targets, all).map(|_| 0),
        CommandLine::Run { target, plan, run, args } => commands::run(&start_dir, &BuildArgs::new(plan, run), target.as_deref(), &args),
        CommandLine::Test { test, plan, run } => commands::test(&start_dir, &BuildArgs::new(plan, run), &test),
        CommandLine::Install { targets, prefix, plan, run } => {
            install::install(&start_dir, &BuildArgs::new(plan, run), &targets, prefix.as_deref()).map(|_| 0)
        }
        CommandLine::Clean {
            targets,
            release,
            profile,
            toolchain,
            platform,
            options,
            all,
        } => {
            let plan = PlanArgs {
                release,
                profile,
                toolchain,
                platform,
                options,
                ..Default::default()
            };
            commands::clean(&start_dir, &BuildArgs::planning(plan), &targets, all).map(|_| 0)
        }
        CommandLine::New { name, lib } => scaffold::new_project(&start_dir, &name, lib, &shell_dir).map(|_| 0),
        CommandLine::Init => scaffold::init(&start_dir).map(|_| 0),
        CommandLine::Add(args) => scaffold::add(&start_dir, &args).map(|_| 0),
        CommandLine::Remove { name } => scaffold::remove(&start_dir, &name).map(|_| 0),
        CommandLine::Convert { path, check: true, plan, .. } => commands::convert_check(&start_dir, path.as_deref(), &BuildArgs::planning(plan)),
        CommandLine::Convert { path, stdout, force, .. } => commands::convert(&start_dir, path.as_deref(), stdout, force).map(|_| 0),
        CommandLine::Targets { json: true, plan } => ide::print_targets(&start_dir, &BuildArgs::planning(plan)).map(|_| 0),
        CommandLine::Targets { plan, .. } => commands::list_targets(&start_dir, &BuildArgs::planning(plan)).map(|_| 0),
        CommandLine::Ide { ides, plan } => ide::setup(&start_dir, &BuildArgs::planning(plan), &ides).map(|_| 0),
        CommandLine::Toolchains { refresh, set_default } => commands::toolchains(refresh, set_default.as_deref()).map(|_| 0),
        CommandLine::Vcpkg { action, args } => commands::vcpkg(&start_dir, action, &args),
        CommandLine::Compdb { plan } => commands::compdb(&start_dir, &BuildArgs::planning(plan)).map(|_| 0),
        CommandLine::Schema { install: true, .. } => schema::install().map(|_| 0),
        CommandLine::Schema { tests, .. } => schema::print(tests).map(|_| 0),
        CommandLine::Fmt { paths, check } => lint::format(&start_dir, check, &paths),
        CommandLine::Tidy { targets, fix, plan, exec } => {
            let run = RunArgs { exec, timings: false };
            lint::tidy(&start_dir, &BuildArgs::new(plan, run), fix, &targets)
        }
        CommandLine::Update { names, plan } => commands::update(&start_dir, &BuildArgs::planning(plan), &names).map(|_| 0),
        CommandLine::Lsp { verbose, .. } => lsp::run(verbose),
        CommandLine::Completions { shell } => {
            clap_complete::generate(shell, &mut <Cli as clap::CommandFactory>::command(), "anvil", &mut std::io::stdout());
            Ok(0)
        }
    };

    job::release();
    match result {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            ui::clear_progress();
            ui::error(&format!("{error:#}"));
            std::process::exit(1);
        }
    }
}
