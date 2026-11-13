//! Anvil: a fast, zero-ceremony build system for C and C++.

mod cmake_tool;
mod commands;
mod cond;
mod convert;
mod convert_check;
mod cuda;
mod db;
mod depfile;
mod exec;
mod export_header;
mod flags;
mod install;
mod job;
mod lint;
mod lockfile;
mod manifest;
mod plan;
mod platform;
mod profile;
mod project;
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
        build: BuildArgs,
    },

    /// Build and run a program; arguments after `--` go to it.
    #[command(visible_alias = "r")]
    Run {
        /// The program (default: the only one, or [project] default-run).
        target: Option<String>,

        #[command(flatten)]
        build: BuildArgs,

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
        build: BuildArgs,
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
        build: BuildArgs,
    },

    /// Delete build output (the profile's folder, or everything with --all).
    Clean {
        #[arg(short, long, conflicts_with = "profile")]
        release: bool,

        #[arg(short, long, value_name = "NAME")]
        profile: Option<String>,

        /// The whole output folder, vcpkg packages included.
        #[arg(long)]
        all: bool,
    },

    /// Create a new project folder.
    New {
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
        build: BuildArgs,
    },

    /// List the project's targets.
    Targets {
        #[command(flatten)]
        build: BuildArgs,
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
        build: BuildArgs,
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
        build: BuildArgs,
    },

    /// Move git dependencies to the newest commit of their branch or tag and vcpkg to its newest ports, and rewrite
    /// anvil.lock (`anvil update json` for one dependency, `anvil update vcpkg` for vcpkg only).
    Update {
        /// Git dependencies to update, or `vcpkg` (default: everything).
        names: Vec<String>,

        #[command(flatten)]
        build: BuildArgs,
    },

    /// Print a shell completion script (`anvil completions powershell >> $PROFILE`).
    Completions { shell: clap_complete::Shell },
}

#[derive(Subcommand)]
enum VcpkgCommand {
    /// List every vcpkg found on this machine.
    List,

    /// Use this vcpkg from now on (a number from `list`, or a folder).
    Use { which: String },

    /// Register a vcpkg in a non-standard folder.
    Add { path: PathBuf },

    /// Download and bootstrap a private vcpkg (only needed without one).
    Setup,

    /// Search the selected vcpkg's ports.
    Search { term: String },
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

    #[arg(last = true)]
    pub args: Vec<String>,
}

#[derive(Args, Clone, Default)]
pub struct BuildArgs {
    /// Build with the release profile.
    #[arg(short, long)]
    pub release: bool,

    /// Build with a named profile.
    #[arg(short, long, value_name = "NAME", conflicts_with = "release")]
    pub profile: Option<String>,

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

    /// After the build, print the slowest actions and the time per kind of action.
    #[arg(long)]
    pub timings: bool,

    /// msvc, clang-cl, clang or gcc (default: the project's, else the best installed).
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

    /// Run compilers at below-normal priority (keeps the machine responsive).
    #[arg(long)]
    pub low_priority: bool,
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
        build: BuildArgs::default(),
    });

    let result = match command {
        CommandLine::Build { targets, all, build } => commands::build(&start_dir, &build, &targets, all).map(|_| 0),
        CommandLine::Run { target, build, args } => commands::run(&start_dir, &build, target.as_deref(), &args),
        CommandLine::Test { test, build } => commands::test(&start_dir, &build, &test),
        CommandLine::Install { targets, prefix, build } => install::install(&start_dir, &build, &targets, prefix.as_deref()).map(|_| 0),
        CommandLine::Clean { release, profile, all } => commands::clean(&start_dir, release, profile.as_deref(), all).map(|_| 0),
        CommandLine::New { name, lib } => scaffold::new_project(&start_dir, &name, lib, &shell_dir).map(|_| 0),
        CommandLine::Init => scaffold::init(&start_dir).map(|_| 0),
        CommandLine::Add(args) => scaffold::add(&start_dir, &args).map(|_| 0),
        CommandLine::Remove { name } => scaffold::remove(&start_dir, &name).map(|_| 0),
        CommandLine::Convert {
            path, check: true, build, ..
        } => commands::convert_check(&start_dir, path.as_deref(), &build),
        CommandLine::Convert { path, stdout, force, .. } => commands::convert(&start_dir, path.as_deref(), stdout, force).map(|_| 0),
        CommandLine::Targets { build } => commands::list_targets(&start_dir, &build).map(|_| 0),
        CommandLine::Toolchains { refresh, set_default } => commands::toolchains(refresh, set_default.as_deref()).map(|_| 0),
        CommandLine::Vcpkg { action, args } => commands::vcpkg(&start_dir, action, &args),
        CommandLine::Compdb { build } => commands::compdb(&start_dir, &build).map(|_| 0),
        CommandLine::Schema { install: true, .. } => schema::install().map(|_| 0),
        CommandLine::Schema { tests, .. } => schema::print(tests).map(|_| 0),
        CommandLine::Fmt { paths, check } => lint::format(&start_dir, check, &paths),
        CommandLine::Tidy { targets, fix, build } => lint::tidy(&start_dir, &build, fix, &targets),
        CommandLine::Update { names, build } => commands::update(&start_dir, &build, &names).map(|_| 0),
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
