//! `anvil schema`: JSON Schemas (draft-07) of anvil.toml and tests.toml, for the editors that read them (Taplo and Even
//! Better TOML in VS Code, JetBrains' TOML plugin): completion, hover text, and unknown keys and values underlined.
//! Written by hand after crate::manifest; the tests below fail when a key is in one and not in the other.

use crate::util::{self, slash};
use anyhow::Result;
use serde_json::{json, Map, Value};

const DRAFT_07: &str = "http://json-schema.org/draft-07/schema#";

// The files `anvil schema --install` writes, beside settings.json
const MANIFEST_SCHEMA_FILE: &str = "anvil.schema.json";
const TESTS_SCHEMA_FILE: &str = "anvil-tests.schema.json";

// Closed sets of values: the usual spellings, then the others Anvil accepts too
const KINDS: &[&str] = &["exe", "lib", "shared", "object", "headers", "test", "custom"];
const KIND_ALIASES: &[&str] = &[
    "executable",
    "bin",
    "app",
    "static",
    "staticlib",
    "dll",
    "dylib",
    "so",
    "header-only",
    "interface",
];
const WARNINGS: &[&str] = &["off", "default", "high", "all"];
const WARNINGS_ALIASES: &[&str] = &["none", "extra", "pedantic"];
const ARCHES: &[&str] = &["avx", "avx2", "avx512", "native"];
const TOOLCHAINS: &[&str] = &["msvc", "clang-cl", "clang", "gcc"];
const TOOLCHAIN_ALIASES: &[&str] = &["auto", "cl", "g++", "mingw"];
const LINKAGES: &[&str] = &["static", "dynamic"];
const CXX_STANDARDS: &[&str] = &[
    "c++17", "c++20", "c++23", "c++26", "c++14", "c++11", "gnu++17", "gnu++20", "gnu++23", "gnu++26",
];
const C_STANDARDS: &[&str] = &["c17", "c11", "c23", "c99", "c89", "c90", "c18", "gnu17", "gnu11", "gnu23", "gnu99"];

// What crate::project::check_version takes
const VERSION_PATTERN: &str = r"^\S+$";

// Every spelling crate::flags::normalize_standard takes ("20", "cxx20", "GNU++20")
const CXX_STANDARD_PATTERN: &str = r"^([cC]\+\+)*([cC][xX][xX])*([cC][pP][pP])*([gG][nN][uU]\+\+)*[cC]*(11|14|17|20|23|26)$";
const C_STANDARD_PATTERN: &str = r"^([cC]\+\+)*([cC][xX][xX])*([cC][pP][pP])*([gG][nN][uU]\+\+)*([gG][nN][uU])*[cC]*(89|90|99|11|17|18|23)$";

/// The tables that take a target's keys: a target, a when block, `[defaults]` or a preset, and a when block of those.
#[derive(Clone, Copy, PartialEq)]
enum Table {
    Target,
    When,
    Shared,
    SharedWhen,
}

/// `anvil schema`: prints the schema of anvil.toml, or of tests.toml.
pub fn print(tests: bool) -> Result<()> {
    let schema = if tests { tests_schema() } else { manifest_schema() };
    println!("{}", serde_json::to_string_pretty(&schema)?);

    Ok(())
}

/// `anvil schema --install`: writes both schemas to the user's data folder, and says how to point an editor at them.
pub fn install() -> Result<()> {
    let folder = util::user_data_dir();
    let manifest_file = folder.join(MANIFEST_SCHEMA_FILE);
    let tests_file = folder.join(TESTS_SCHEMA_FILE);

    // Write both
    for (file, schema) in [(&manifest_file, manifest_schema()), (&tests_file, tests_schema())] {
        util::write_atomic(file, serde_json::to_string_pretty(&schema)?.as_bytes())?;
        crate::ui::status("Wrote", &slash(file));
    }

    // How to use them
    let manifest_url = file_url(&manifest_file);
    let tests_url = file_url(&tests_file);
    println!("VS Code (Even Better TOML), in settings.json:");
    println!("    \"evenBetterToml.schema.associations\": {{");
    println!("        \".*/anvil\\\\.toml$\": \"{manifest_url}\",");
    println!("        \".*/tests\\\\.toml$\": \"{tests_url}\"");
    println!("    }}");
    println!("Or, for one file, this as its first line (Taplo, Even Better TOML):");
    println!("    #:schema {manifest_url}");
    println!("Run `anvil schema --install` again after updating Anvil.");

    Ok(())
}

/// The schema of anvil.toml.
pub fn manifest_schema() -> Value {
    let properties = vec![
        ("project", project_table()),
        ("workspace", workspace_table()),
        (
            "defaults",
            described(
                reference("shared"),
                "Settings every target of this package gets: a target's keys, but kind, dir, if, default and preset.",
            ),
        ),
        (
            "deps",
            map(
                "Dependencies: `name = \"vcpkg\"`, `\"pkg-config\"` or `\"system\"`, or a table (vcpkg, path, git, pkg-config, or a prebuilt \
                 include/libs). Without [target] tables the main target uses them all; otherwise the targets that list them in `deps`.",
                reference("dep"),
            ),
        ),
        (
            "dev-deps",
            map(
                "Dependencies of the tests only: never linked into the package's programs and libraries.",
                reference("dep"),
            ),
        ),
        (
            "target",
            map(
                "The package's targets, by name. Without any, the layout decides: src/, include/, tests/.",
                reference("target"),
            ),
        ),
        (
            "preset",
            map(
                "Named groups of settings that targets opt into with `preset = \"name\"`.",
                reference("shared"),
            ),
        ),
        (
            "profile",
            map(
                "Profiles besides the built-in debug and release (`anvil build -p name`).",
                profile_table(),
            ),
        ),
        (
            "options",
            map(
                "Options for conditions and `-o name=true|false`: `name = true` or `name = { default = true, description = \"...\" }`.",
                option_value(),
            ),
        ),
        ("vcpkg", vcpkg_table()),
        (
            "test",
            array(
                "Tests that run a command. A `matrix` makes one test per combination of its axes.",
                reference("test"),
            ),
        ),
        (
            "write",
            array(
                "Files written before the build, rewritten only when their text changes (CMake's configure-time file(WRITE)).",
                write_table(),
            ),
        ),
    ];

    let mut schema = table("anvil.toml: an Anvil package.", properties, &[]);
    schema["$schema"] = json!(DRAFT_07);
    schema["title"] = json!("anvil.toml");
    schema["definitions"] = definitions();

    schema
}

/// The schema of a tests.toml: [[test]] tables only.
pub fn tests_schema() -> Value {
    let properties = vec![(
        "test",
        array(
            "Tests that run a command. A `matrix` makes one test per combination of its axes.",
            reference("test"),
        ),
    )];

    let mut schema = table("tests.toml: Anvil tests, kept beside what they test.", properties, &[]);
    schema["$schema"] = json!(DRAFT_07);
    schema["title"] = json!("tests.toml");
    schema["definitions"] = definitions();

    schema
}

/// Tables used in several places (and the recursive `when`).
fn definitions() -> Value {
    json!({
        "target": target_table(Table::Target),
        "when": target_table(Table::When),
        "shared": target_table(Table::Shared),
        "shared-when": target_table(Table::SharedWhen),
        "dep": dep_value(),
        "test": test_table(),
    })
}

fn project_table() -> Value {
    let metadata = "Package metadata, kept for the record: Anvil does not use it.";
    let properties = vec![
        ("name", text("The package's name, and its main target's (no `/` or `\\`).")),
        (
            "version",
            json!({
                "type": "string",
                "pattern": VERSION_PATTERN,
                "examples": ["0.1.0"],
                "description": "The package's version, without spaces (\"2.1.0\", \"1.4.0-beta.2\"): the {version} placeholder, and the installed package's (whose CMake package file needs one to four numbers).",
            }),
        ),
        ("description", text(metadata)),
        ("license", text(metadata)),
        ("authors", texts(metadata)),
        (
            "std",
            cxx_standard("C++ standard: c++17, c++20 (default), c++23, c++26 (also \"20\", \"gnu++20\")."),
        ),
        ("c-std", c_standard("C standard: c11, c17 (default), c23 (also \"17\", c99, \"gnu17\").")),
        (
            "toolchain",
            choice(
                TOOLCHAINS,
                TOOLCHAIN_ALIASES,
                "msvc, clang-cl, clang or gcc. Default (\"auto\"): the best one installed, or the one `anvil toolchains --set-default` chose.",
            ),
        ),
        (
            "launcher",
            text("A compiler cache C and C++ compiles run through: \"sccache\", \"ccache\" or a path (ANVIL_LAUNCHER overrides it)."),
        ),
        ("out", text("Output folder (default \"out\").")),
        ("default-run", text("The program `anvil run` starts when there are several.")),
        (
            "export",
            flag("`anvil install` writes CMake and pkg-config files for the installed libraries (default true)."),
        ),
    ];

    table("The package: its name, version, language standards and toolchain.", properties, &[])
}

fn workspace_table() -> Value {
    let properties = vec![(
        "members",
        texts("Member package folders, globs allowed (\"tools/*\"). Members depend on each other with path deps."),
    )];

    table("A workspace: several Anvil packages built together.", properties, &["members"])
}

fn vcpkg_table() -> Value {
    let properties = vec![
        (
            "root",
            text("A vcpkg checkout to use for this project (overrides the user's choice, `anvil vcpkg use`)."),
        ),
        (
            "triplet",
            text("The vcpkg triplet (default: picked from the toolchain and profile, e.g. x64-windows)."),
        ),
        (
            "linkage",
            choice(
                LINKAGES,
                &[],
                "\"static\" or \"dynamic\" libraries: the triplet for each machine and compiler follows.",
            ),
        ),
        (
            "baseline",
            text("The builtin-baseline: the vcpkg ports commit that fixes every port's version (default: the one in anvil.lock)."),
        ),
        (
            "manifest",
            text("Path to a vcpkg.json to install from (default: the project's own vcpkg.json when there is one)."),
        ),
        ("overlay-ports", texts("Folders of overlay ports.")),
        ("overlay-triplets", texts("Folders of overlay triplets.")),
    ];

    table("vcpkg settings: the checkout, triplet, linkage, baseline and overlays.", properties, &[])
}

fn profile_table() -> Value {
    let properties = vec![
        ("inherits", text("The profile this one starts from (default: release).")),
        (
            "target",
            text("The machine to build for: x64, x86, arm64, arm, or a triple such as aarch64-linux-gnu (a cross build)."),
        ),
        ("opt", optimization()),
        ("debug", flag("Debug information.")),
        ("lto", flag("Link-time optimization.")),
        (
            "sanitize",
            array(
                "Sanitizers: address, undefined, thread, memory, leak, ...",
                json!({ "enum": crate::profile::SANITIZERS }),
            ),
        ),
        ("debug-runtime", flag("MSVC's debug C runtime (/MDd) and vcpkg's debug libraries.")),
        ("static-runtime", flag("Link the C/C++ runtime statically (/MT, -static).")),
        ("assertions", flag("Assertions; false defines NDEBUG.")),
        ("defines", texts("Definitions for every target.")),
        ("flags", texts("Compiler flags for every target.")),
        ("link-flags", texts("Linker flags for every target.")),
    ];

    table("A build profile.", properties, &[])
}

/// A target's keys; when blocks leave out what picks and places a target, `[defaults]`, presets and their when blocks
/// `kind` too.
fn target_table(kind: Table) -> Value {
    let mut properties = Vec::new();
    let shared = matches!(kind, Table::Shared | Table::SharedWhen);

    // What makes and places a target
    if !shared {
        properties.push((
            "kind",
            choice(KINDS, KIND_ALIASES, "exe, lib (static), shared, object, headers, test or custom."),
        ));
    }
    if kind == Table::Target {
        properties.extend([
            ("dir", text("Folder the target's relative paths start from (default: the package root).")),
            ("if", text("The target exists only when this condition holds (`windows & !mingw`).")),
            ("default", flag("Built by a plain `anvil build` (default true; tests false).")),
            (
                "preset",
                text_or_texts("A [preset.<name>] or a list of them, applied before the target's own settings."),
            ),
        ]);
    }

    // Sources and settings
    properties.extend([
        (
            "sources",
            texts(
                "Source globs; `!pattern` excludes (default: src/**/*.{c,cc,cpp,cxx,c++,cu}). In a when block they belong to its \
                 condition: compiled only where it holds.",
            ),
        ),
        (
            "include",
            texts("Include folders (default: src/ when it exists); `[]` turns the convention off."),
        ),
        ("system-include", texts("Include folders whose warnings are silenced (vendored code).")),
        ("defines", texts("Definitions: `NAME` or `NAME=value`.")),
        ("flags", texts("Raw compiler flags.")),
        ("link-flags", texts("Raw linker flags. `{target:X}` is X's file, linked first.")),
        (
            "libs",
            texts("System or prebuilt libraries: \"user32\", \"pthread\", or a path to a .lib/.a."),
        ),
        ("lib-dirs", texts("Folders the linker searches for libs.")),
        (
            "deps",
            texts("Targets and [deps] this one uses: linked, their public settings seen, not passed on."),
        ),
        ("public", public_table()),
        ("interface", interface_table()),
        ("pch", text("Precompiled header.")),
        ("unity", unity()),
        ("warnings", choice(WARNINGS, WARNINGS_ALIASES, "off, default, high or all.")),
        ("werror", flag("Warnings are errors.")),
        ("arch", choice(ARCHES, &[], "Instruction set: avx, avx2, avx512 or native.")),
        (
            "cuda-arch",
            text_or_texts(
                "GPUs .cu files compile for: \"native\", \"all\", \"86\" (code and PTX), \"86-real\", \"86-virtual\", a list or \"75;86\".",
            ),
        ),
        ("cuda-flags", texts("Extra nvcc options for .cu files.")),
        ("std", cxx_standard("C++ standard of this target: c++17, c++20, c++23, c++26.")),
        ("c-std", c_standard("C standard of this target: c11, c17, c23.")),
        (
            "gui",
            json!({
                "enum": [true, false, "main"],
                "description": "Windows GUI program: true for one entered through WinMain (CMake's WIN32), \"main\" for one with main().",
            }),
        ),
        ("output-name", text("The output file's name (default: the target's name).")),
        ("output-prefix", text("Replaces the platform's file name prefix (\"lib\").")),
        ("output-suffix", text("Replaces the platform's extension (\".dll\", \".so\", \".exe\").")),
        (
            "output-dir",
            text("The folder the target's file goes to (default: {out}, static libraries {out}/lib)."),
        ),
        (
            "exports",
            flag("A program that exports symbols for the modules it loads: those modules link against it."),
        ),
        ("export-header", export_header()),
        (
            "header-deps",
            texts("Targets whose include folders and definitions this one uses, without linking them."),
        ),
        (
            "needs",
            texts("Targets built before this one, without linking or including anything of theirs."),
        ),
        (
            "link-depends",
            texts("Files whose change relinks the target (a .def file, a linker script)."),
        ),
        (
            "whole-archive",
            texts("Static libraries linked whole, every object in them, even those nothing here calls."),
        ),
        (
            "data",
            texts("Files (globs relative to `dir`) copied beside the program or shared library, keeping their path under `dir`."),
        ),
        (
            "install",
            flag("`anvil install` installs it (default: whether it is a default target; tests never)."),
        ),
        (
            "when",
            map(
                "Settings that apply when a condition holds: `[target.x.when.\"windows & msvc\"]`.",
                reference(if shared { "shared-when" } else { "when" }),
            ),
        ),
    ]);

    // Per-file rules and build steps
    properties.extend([
        ("file", array("Settings for some of the target's source files.", file_rule())),
        ("files", array("Another name for `file`.", file_rule())),
        ("run", array("Build steps: code generation, shader compilation, copying.", run_step())),
        (
            "pre-build",
            array("Commands run before the target compiles, whenever its sources change.", step()),
        ),
        (
            "post-build",
            array("Commands run after the target's file is made; dependents wait for them.", step()),
        ),
    ]);

    // Test settings (kind = "test")
    properties.push(("args", texts("Arguments the test program runs with.")));
    properties.extend(test_settings());

    let description = match kind {
        Table::Target => "A target: a program, library or test.",
        Table::When => "Settings that apply when the condition holds: a target's keys, but dir, if, default and preset.",
        Table::Shared => "Settings for several targets: a target's keys, but kind, dir, if, default and preset.",
        Table::SharedWhen => "Settings that apply when the condition holds: a target's keys, but kind, dir, if, default and preset.",
    };

    // `files` is another name for `file`, so not both
    let mut schema = table(description, properties, &[]);
    schema["not"] = json!({ "required": ["file", "files"] });

    schema
}

fn public_table() -> Value {
    let properties = vec![
        ("include", texts("Include folders (default: include/ when it exists).")),
        ("system-include", texts("Include folders whose warnings are silenced.")),
        ("defines", texts("Definitions.")),
        ("flags", texts("Compiler flags.")),
        (
            "deps",
            texts("Dependencies whose public settings pass on to this target's dependents too."),
        ),
    ];

    table("Settings the target and every target using it get.", properties, &[])
}

fn interface_table() -> Value {
    let properties = vec![
        ("include", texts("Include folders.")),
        ("defines", texts("Definitions (`FOO_API=__declspec(dllimport)`).")),
        ("flags", texts("Compiler flags.")),
    ];

    table("Settings only the targets using this one get.", properties, &[])
}

fn export_header() -> Value {
    let properties = vec![
        ("path", text("The header's path under {gen}.")),
        ("macro", text("The export macro (default: <TARGET>_EXPORT).")),
    ];

    json!({
        "description": "Write an export macro header for this library: a path under {gen}, or a table with `path` and `macro`.",
        "anyOf": [{ "type": "string" }, table("An export header.", properties, &["path"])],
    })
}

fn file_rule() -> Value {
    let properties = vec![
        ("match", text_or_texts("Globs of the source files these settings are for.")),
        ("if", text("The rule applies only when this condition holds.")),
        ("defines", texts("Definitions for these files.")),
        ("flags", texts("Compiler flags for these files.")),
        ("warnings", choice(WARNINGS, WARNINGS_ALIASES, "off, default, high or all.")),
        ("arch", choice(ARCHES, &[], "Instruction set: avx, avx2, avx512 or native.")),
        ("opt", optimization()),
    ];

    table("Settings for the source files `match` names.", properties, &["match"])
}

fn run_step() -> Value {
    let properties = vec![
        (
            "for-each",
            text_or_texts("Run the command once per file matching these globs ({input}, {stem}, {filename})."),
        ),
        ("inputs", texts("Extra files whose change re-runs it.")),
        ("output", text("The file it writes ({output}).")),
        ("outputs", texts("The files it writes ({outputs}).")),
        ("command", command()),
        ("depfile", flag("The command writes a make-style depfile to {depfile}.")),
        ("shell", flag("Run through the shell (cmd /c, sh -c).")),
        ("message", text("Printed when it runs.")),
        ("workdir", text("The folder it runs in.")),
        ("if", text("Runs only when this condition holds.")),
    ];

    table(
        "A build step; it re-runs when its command, inputs or outputs change.",
        properties,
        &["command"],
    )
}

fn step() -> Value {
    let properties = vec![
        ("command", command()),
        ("shell", flag("Run through the shell (cmd /c, sh -c).")),
        ("message", text("Printed when it runs.")),
        ("workdir", text("The folder it runs in.")),
        ("if", text("Runs only when this condition holds.")),
    ];

    table("A command with no outputs.", properties, &["command"])
}

fn write_table() -> Value {
    let properties = vec![
        ("path", text("The file: `{out}/generated/list.txt`.")),
        ("text", text("What it holds.")),
        ("if", text("Written only when this condition holds.")),
    ];

    table("A file written before the build.", properties, &["path", "text"])
}

/// A `[deps]` entry: its short form or its table.
fn dep_value() -> Value {
    let properties = vec![
        ("vcpkg", text("The vcpkg port.")),
        ("features", texts("The port's features.")),
        ("default-features", flag("The port's default features (default true).")),
        (
            "path",
            text("A folder with an Anvil package (or with only sources: then `include`, `sources` say what to build)."),
        ),
        ("git", text("A git repository with an Anvil package.")),
        ("tag", text("The git tag to use.")),
        ("branch", text("The git branch to follow (`anvil update` moves it on).")),
        ("rev", text("The git commit to use.")),
        ("pkg-config", text("The pkg-config package.")),
        ("target", text("For path and git packages: which of its targets (default: its library).")),
        ("if", text("Used only when this condition holds.")),
        ("include", texts("Include folders: of a prebuilt library, or overriding a package's.")),
        ("sources", texts("For path and git packages without an anvil.toml: the sources to build.")),
        ("defines", texts("Definitions its users get.")),
        ("flags", texts("Compiler flags its users get (prebuilt and vcpkg libraries).")),
        ("libs", texts("Libraries to link (prebuilt and vcpkg libraries).")),
        ("lib-dirs", texts("Folders the linker searches for libs.")),
        ("link-flags", texts("Linker flags.")),
        ("runtime", texts("Files copied next to the programs (DLLs).")),
    ];

    json!({
        "description": "A dependency: \"vcpkg\", \"pkg-config\", \"system\", or a table.",
        "anyOf": [
            {
                "enum": ["vcpkg", "pkg-config", "system"],
                "description": "The vcpkg port, pkg-config package or system library of this name.",
            },
            table("A dependency: one of vcpkg, path, git or pkg-config, or a prebuilt include/libs.", properties, &[]),
        ],
    })
}

fn option_value() -> Value {
    let properties = vec![
        ("default", flag("The option's value unless `-o` sets it.")),
        ("description", text("What the option does.")),
    ];

    json!({
        "description": "An option: its default, or a table with `default` and `description`.",
        "anyOf": [{ "type": "boolean" }, table("An option.", properties, &["default"])],
    })
}

/// A `[[test]]` table: a test, or with a `matrix`, a template of several.
fn test_table() -> Value {
    let mut properties = test_keys();
    properties.push(("matrix", matrix()));

    let mut schema = table(
        "A test that runs a command; it passes when it exits with `exit-code` (0).",
        properties,
        &[],
    );

    // A template's command is its own, or every entry of one axis sets one
    let axis_with_commands = json!({ "type": "array", "items": { "type": "object", "required": ["command"] } });
    schema["if"] = json!({ "required": ["matrix"] });
    schema["then"] = json!({
        "anyOf": [
            { "required": ["command"] },
            { "not": { "properties": { "matrix": { "additionalProperties": { "not": axis_with_commands } } } } },
        ],
    });
    schema["else"] = json!({ "required": ["name", "command"] });

    schema
}

/// The keys of one test (crate::test_cases::TEST_KEYS): a [[test]] table's, and those a matrix entry may set.
fn test_keys() -> Vec<(&'static str, Value)> {
    let mut properties = vec![
        (
            "name",
            text("The test's name; with a matrix, a pattern of its parameters (\"stills_{view}\")."),
        ),
        ("command", command()),
        ("if", text("The test exists only when this condition holds.")),
    ];
    properties.extend(test_settings());

    properties
}

/// What a test runs with and how it is judged: [[test]] keys, and those of a `kind = "test"` target.
fn test_settings() -> Vec<(&'static str, Value)> {
    vec![
        ("workdir", text("The folder the test runs in.")),
        (
            "timeout",
            integer("Seconds before the test is stopped and fails (default 600).", 0, i64::MAX),
        ),
        ("env", map("Environment variables the test runs with.", json!({ "type": "string" }))),
        ("will-fail", flag("The test passes when it fails.")),
        ("after", texts("Tests that must finish first.")),
        ("serial", flag("Run with no other test at the same time.")),
        ("pass-regex", text("Pass when the output matches, whatever the exit code.")),
        ("fail-regex", text("Fail when the output matches.")),
        (
            "skip-exit-code",
            integer(
                "The exit code by which the test says it could not run (a missing tool): skipped, not failed.",
                i32::MIN as i64,
                i32::MAX as i64,
            ),
        ),
        ("skip-regex", text("Output by which the test says it could not run: skipped, not failed.")),
        ("expect", texts("Regexes the output (stdout and stderr) must each match.")),
        ("reject", texts("Regexes the output must not match.")),
        (
            "exit-code",
            integer("The exit code that passes (default 0).", i32::MIN as i64, i32::MAX as i64),
        ),
    ]
}

/// `matrix.<axis> = [entries]`: a text entry sets the axis's parameter, a table entry sets parameters and test keys.
fn matrix() -> Value {
    let parameter = json!({
        "description": "A parameter: text, or a list (a whole item it fills becomes its items).",
        "anyOf": [{ "type": ["string", "integer"] }, { "type": "array", "items": { "type": "string" } }],
    });

    let mut entry_table = table("Parameters ({name} in the template), and test keys for its tests.", test_keys(), &[]);
    entry_table["additionalProperties"] = parameter;
    let entry = json!({
        "anyOf": [
            entry_table,
            { "type": ["string", "integer"], "description": "The value of the parameter named after the axis." },
            { "type": "array", "items": { "type": "string" }, "description": "A list parameter named after the axis." },
        ],
    });

    map(
        "Makes this table a template: one test per combination of the axes' entries (`matrix.view = [\"a\", \"b\"]`).",
        json!({ "type": "array", "minItems": 1, "items": entry }),
    )
}

// Value shapes

fn command() -> Value {
    text_or_texts("The command: a list of words, or one line split like a shell's. `{target:X}` is program X.")
}

fn optimization() -> Value {
    json!({
        "enum": [0, 1, 2, 3, "s", "z", "0", "1", "2", "3"],
        "description": "Optimization: 0, 1, 2, 3, \"s\" (size), \"z\" (smallest).",
    })
}

fn unity() -> Value {
    json!({
        "description": "Unity build: true (8 sources per unity file), false, or the number of sources per unity file.",
        "anyOf": [{ "type": "boolean" }, { "type": "integer", "minimum": 2 }],
    })
}

fn cxx_standard(description: &str) -> Value {
    standard(CXX_STANDARDS, CXX_STANDARD_PATTERN, description)
}

fn c_standard(description: &str) -> Value {
    standard(C_STANDARDS, C_STANDARD_PATTERN, description)
}

/// The usual spellings for completion; the pattern takes the rest.
fn standard(values: &[&str], pattern: &str, description: &str) -> Value {
    json!({
        "description": description,
        "anyOf": [{ "enum": values }, { "type": "string", "pattern": pattern, "examples": ["17"] }],
    })
}

/// One of `values`; `aliases` are other spellings of them.
fn choice(values: &[&str], aliases: &[&str], description: &str) -> Value {
    if aliases.is_empty() {
        return json!({ "type": "string", "enum": values, "description": description });
    }

    json!({
        "description": description,
        "anyOf": [{ "enum": values }, { "enum": aliases, "description": "Other spellings Anvil accepts." }],
    })
}

fn text(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn texts(description: &str) -> Value {
    array(description, json!({ "type": "string" }))
}

fn text_or_texts(description: &str) -> Value {
    json!({
        "description": description,
        "anyOf": [{ "type": "string" }, { "type": "array", "items": { "type": "string" } }],
    })
}

fn flag(description: &str) -> Value {
    json!({ "type": "boolean", "description": description })
}

fn integer(description: &str, minimum: i64, maximum: i64) -> Value {
    json!({ "type": "integer", "minimum": minimum, "maximum": maximum, "description": description })
}

fn array(description: &str, items: Value) -> Value {
    json!({ "type": "array", "items": items, "description": description })
}

/// A table of any keys, each holding `values`.
fn map(description: &str, values: Value) -> Value {
    json!({ "type": "object", "additionalProperties": values, "description": description })
}

/// A table of these keys only, as `deny_unknown_fields` reads it.
fn table(description: &str, properties: Vec<(&str, Value)>, required: &[&str]) -> Value {
    let properties: Map<String, Value> = properties.into_iter().map(|(key, value)| (key.to_string(), value)).collect();

    let mut schema = json!({
        "type": "object",
        "description": description,
        "properties": properties,
        "additionalProperties": false,
    });
    if !required.is_empty() {
        schema["required"] = json!(required);
    }

    schema
}

fn reference(definition: &str) -> Value {
    json!({ "$ref": format!("#/definitions/{definition}") })
}

/// A reference with a description of its own (draft-07 ignores the keys beside a `$ref`).
fn described(schema: Value, description: &str) -> Value {
    json!({ "allOf": [schema], "description": description })
}

fn file_url(path: &std::path::Path) -> String {
    let path = slash(path).replace(' ', "%20");
    match path.starts_with('/') {
        true => format!("file://{path}"),
        false => format!("file:///{path}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest;

    // A key no table has
    const UNKNOWN: &str = "zz-unknown";

    /// The keys serde accepts in a table: the list its "unknown field" error gives.
    fn parser_keys(error: &str) -> Vec<String> {
        let expected = error
            .split_once("expected")
            .unwrap_or_else(|| panic!("not an unknown-field error: {error}"))
            .1;
        let quoted = regex::Regex::new(r"`([^`]+)`").unwrap();
        quoted.captures_iter(expected).map(|captures| captures[1].to_string()).collect()
    }

    fn manifest_keys(text: &str) -> Vec<String> {
        parser_keys(&manifest::parse_str(text).unwrap_err().to_string())
    }

    /// The keys a table's schema lists, following references.
    fn schema_keys(schema: &Value, root: &Value) -> Vec<String> {
        let schema = resolve(schema, root);
        let table = schema["anyOf"]
            .as_array()
            .and_then(|alternatives| alternatives.iter().find(|alternative| alternative.get("properties").is_some()));
        let table = table.unwrap_or(schema);
        table["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("no properties in {schema}"))
            .keys()
            .cloned()
            .collect()
    }

    fn resolve<'a>(schema: &'a Value, root: &'a Value) -> &'a Value {
        if let Some(reference) = schema["$ref"].as_str() {
            let name = reference.trim_start_matches("#/definitions/");
            return resolve(&root["definitions"][name], root);
        }
        if let Some(all) = schema["allOf"].as_array() {
            return resolve(&all[0], root);
        }
        match schema["type"].as_str() {
            Some("array") => resolve(&schema["items"], root),
            Some("object") if schema.get("properties").is_none() && schema["additionalProperties"].is_object() => {
                resolve(&schema["additionalProperties"], root)
            }
            _ => schema,
        }
    }

    /// The schema at a path of keys from the root, through arrays and maps.
    fn at<'a>(root: &'a Value, keys: &[&str]) -> &'a Value {
        let mut schema = resolve(root, root);
        for key in keys {
            let table = schema["anyOf"]
                .as_array()
                .and_then(|alternatives| alternatives.iter().find(|alternative| alternative.get("properties").is_some()));
            schema = resolve(&table.unwrap_or(schema)["properties"][*key], root);
        }
        schema
    }

    #[track_caller]
    fn assert_same_keys(mut schema: Vec<String>, mut parser: Vec<String>, table: &str) {
        schema.sort();
        parser.sort();
        assert_eq!(
            schema, parser,
            "{table}: the schema's keys (left) differ from what crate::manifest reads (right)"
        );
    }

    #[test]
    fn every_table_has_the_keys_the_parser_reads() {
        let root = manifest_schema();
        let target_keys = manifest_keys(&format!("[target.a]\n{UNKNOWN} = 1\n"));

        // Each table against the "unknown field" error of the same table
        let tables: &[(&[&str], String)] = &[
            (&[], format!("{UNKNOWN} = 1\n")),
            (&["project"], format!("[project]\n{UNKNOWN} = 1\n")),
            (&["workspace"], format!("[workspace]\nmembers = []\n{UNKNOWN} = 1\n")),
            (&["vcpkg"], format!("[vcpkg]\n{UNKNOWN} = 1\n")),
            (&["profile"], format!("[profile.a]\n{UNKNOWN} = 1\n")),
            (&["target", "public"], format!("[target.a.public]\n{UNKNOWN} = 1\n")),
            (&["target", "interface"], format!("[target.a.interface]\n{UNKNOWN} = 1\n")),
            (
                &["target", "export-header"],
                format!("[target.a]\nexport-header = {{ path = \"a\", {UNKNOWN} = 1 }}\n"),
            ),
            (&["target", "file"], format!("[[target.a.file]]\nmatch = \"a\"\n{UNKNOWN} = 1\n")),
            (&["target", "run"], format!("[[target.a.run]]\ncommand = \"a\"\n{UNKNOWN} = 1\n")),
            (
                &["target", "pre-build"],
                format!("[[target.a.pre-build]]\ncommand = \"a\"\n{UNKNOWN} = 1\n"),
            ),
            (
                &["target", "post-build"],
                format!("[[target.a.post-build]]\ncommand = \"a\"\n{UNKNOWN} = 1\n"),
            ),
            (&["write"], format!("[[write]]\npath = \"a\"\ntext = \"a\"\n{UNKNOWN} = 1\n")),
        ];
        for (path, text) in tables {
            assert_same_keys(schema_keys(at(&root, path), &root), manifest_keys(text), &path.join("."));
        }

        // A target has every key; a when block all but those that make and place a target; [defaults], presets and their
        // when blocks not kind
        let without = |left_out: &[&str]| {
            target_keys
                .iter()
                .filter(|key| !left_out.contains(&key.as_str()))
                .cloned()
                .collect::<Vec<_>>()
        };
        let placing = ["dir", "if", "default", "preset"];
        assert_same_keys(schema_keys(at(&root, &["target"]), &root), target_keys.clone(), "target");
        assert_same_keys(schema_keys(at(&root, &["target", "when"]), &root), without(&placing), "when");
        for shared in [&["defaults"][..], &["preset"], &["defaults", "when"], &["preset", "when"]] {
            let keys = schema_keys(at(&root, shared), &root);
            assert_same_keys(keys, without(&[&placing[..], &["kind"]].concat()), &shared.join("."));
        }

        // [[test]]: the test's keys and the matrix; a matrix entry's test keys are the ones test_cases knows
        let mut test_keys = manifest_keys(&format!("[[test]]\nname = \"a\"\ncommand = \"a\"\n{UNKNOWN} = 1\n"));
        test_keys.push("matrix".into());
        assert_same_keys(schema_keys(at(&root, &["test"]), &root), test_keys, "test");
        let entry = &at(&root, &["test", "matrix"])["anyOf"][0];
        let known: Vec<String> = crate::test_cases::TEST_KEYS.iter().map(|key| key.to_string()).collect();
        assert_same_keys(schema_keys(entry, &root), known, "matrix entry");

        // Dependencies and options
        let mut dep_table = toml::Table::new();
        dep_table.insert(UNKNOWN.into(), toml::Value::Integer(1));
        let error = manifest::parse_dep("a", &toml::Value::Table(dep_table)).unwrap_err().to_string();
        assert_same_keys(schema_keys(at(&root, &["deps"]), &root), parser_keys(&error), "deps");
        assert_same_keys(schema_keys(at(&root, &["dev-deps"]), &root), parser_keys(&error), "dev-deps");
        let option_keys = manifest::OPTION_KEYS.iter().map(|key| key.to_string()).collect();
        assert_same_keys(schema_keys(at(&root, &["options"]), &root), option_keys, "options");

        // tests.toml
        let tests_root = tests_schema();
        let error = manifest::parse_tests(&format!("{UNKNOWN} = 1\n")).unwrap_err().to_string();
        assert_same_keys(schema_keys(&tests_root, &tests_root), parser_keys(&error), "tests.toml");
    }

    /// A value of `schema`, taking alternative `alternative` (or the last) of each anyOf; texts are `{x}`, so a matrix
    /// parameter named `x` is used.
    fn example(schema: &Value, root: &Value, alternative: usize, depth: usize) -> Value {
        if let Some(reference) = schema["$ref"].as_str() {
            let name = reference.trim_start_matches("#/definitions/");
            return example(&root["definitions"][name], root, alternative, depth);
        }
        if let Some(all) = schema["allOf"].as_array() {
            return example(&all[0], root, alternative, depth);
        }
        if let Some(alternatives) = schema["anyOf"].as_array() {
            return example(&alternatives[alternative.min(alternatives.len() - 1)], root, alternative, depth);
        }
        if let Some(values) = schema["enum"].as_array().or(schema["examples"].as_array()) {
            return values[alternative.min(values.len() - 1)].clone();
        }

        let kind = match &schema["type"] {
            Value::Array(kinds) => kinds[0].as_str().unwrap(),
            kind => kind.as_str().unwrap_or_else(|| panic!("no type in {schema}")),
        };
        match kind {
            "string" => json!("{x}"),
            "integer" => json!(schema["minimum"].as_i64().unwrap_or(0).max(2)),
            "boolean" => json!(true),
            "array" => json!([example(&schema["items"], root, alternative, depth)]),
            _ => {
                let mut object = Map::new();
                if let Some(properties) = schema["properties"].as_object() {
                    // `files` is `file` again: serde would call it a duplicate
                    for (key, value) in properties.iter().filter(|(key, _)| *key != "files") {
                        object.insert(key.clone(), example(value, root, alternative, depth + 1));
                    }
                } else if depth < 4 {
                    object.insert("x".into(), example(&schema["additionalProperties"], root, alternative, depth + 1));
                }
                Value::Object(object)
            }
        }
    }

    #[test]
    fn every_key_of_the_schema_parses() {
        let root = manifest_schema();

        for alternative in 0..3 {
            let instance = example(&root, &root, alternative, 0);
            assert_eq!(
                validate(&root, &instance),
                Vec::<String>::new(),
                "the example does not fit its own schema"
            );

            // The manifest, then its deps and options, which it keeps as TOML values
            let text = toml::to_string(&instance).unwrap();
            let manifest = manifest::parse_str(&text).unwrap_or_else(|error| panic!("{error:#}\n{text}"));
            for value in manifest.deps.values().chain(manifest.dev_deps.values()) {
                match value {
                    toml::Value::Table(_) => drop(value.clone().try_into::<manifest::DepSpec>().unwrap()),
                    _ => drop(manifest::parse_dep("x", value).unwrap()),
                }
            }
            manifest::parse_options(&manifest.options).unwrap();
            assert!(!manifest.test.is_empty() && !manifest.target["x"].when.is_empty());

            // tests.toml
            let tests_root = tests_schema();
            let text = toml::to_string(&example(&tests_root, &tests_root, alternative, 0)).unwrap();
            manifest::parse_tests(&text).unwrap_or_else(|error| panic!("{error:#}\n{text}"));
        }
    }

    #[test]
    fn closed_values_are_the_ones_anvil_accepts() {
        use crate::flags::{normalize_standard, Arch, Gui, Language, Warnings};

        for kind in KINDS.iter().chain(KIND_ALIASES) {
            crate::targets::Kind::parse(kind).unwrap();
        }
        for warnings in WARNINGS.iter().chain(WARNINGS_ALIASES) {
            Warnings::parse(warnings).unwrap();
        }
        for arch in ARCHES {
            Arch::parse(arch).unwrap();
        }
        for toolchain in TOOLCHAINS.iter().chain(TOOLCHAIN_ALIASES).filter(|name| **name != "auto") {
            crate::toolchain::Family::parse(toolchain).unwrap();
        }
        for value in optimization()["enum"].as_array().unwrap() {
            crate::profile::Opt::parse(&toml::Value::try_from(value).unwrap()).unwrap();
        }
        for value in [true, false] {
            Gui::parse(&toml::Value::Boolean(value)).unwrap();
        }
        Gui::parse(&toml::Value::String("main".into())).unwrap();

        // Standards: the pattern takes exactly what normalize_standard does
        let spellings = [
            "20", "c++20", "C++20", "cxx20", "cpp20", "gnu++20", "GNU++23", "c20", "c17", "17", "c99", "gnu17", "c++21", "2a", "c++2b", "", "c",
        ];
        for (values, pattern, language) in [
            (CXX_STANDARDS, CXX_STANDARD_PATTERN, Language::Cxx),
            (C_STANDARDS, C_STANDARD_PATTERN, Language::C),
        ] {
            let pattern = regex::Regex::new(pattern).unwrap();
            for spelling in values.iter().chain(&spellings) {
                let accepted = normalize_standard(spelling, language).is_ok();
                assert_eq!(pattern.is_match(spelling), accepted, "standard `{spelling}`");
                assert!(!values.contains(spelling) || accepted, "standard `{spelling}`");
            }
        }
    }

    #[test]
    fn fixtures_fit_and_typos_do_not() {
        let root = manifest_schema();
        let tests_root = tests_schema();
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");

        // Every manifest Anvil reads fits; one it calls a typo does not
        let mut checked = 0;
        for entry in walkdir::WalkDir::new(&fixtures).into_iter().flatten() {
            let path = entry.path();
            let (schema, parsed) = match entry.file_name().to_str() {
                Some("anvil.toml") => (&root, manifest::load(path).map(drop)),
                Some("tests.toml") => (&tests_root, manifest::load_tests(path).map(drop)),
                _ => continue,
            };
            let errors = validate(schema, &as_json(&std::fs::read_to_string(path).unwrap()));
            match parsed {
                Err(error) if error.to_string().contains("unknown field") => {
                    assert!(!errors.is_empty(), "{}: a typo the schema misses", path.display())
                }
                _ => assert_eq!(errors, Vec::<String>::new(), "{}", path.display()),
            }
            checked += 1;
        }
        assert!(checked > 10, "only {checked} manifests under {}", fixtures.display());

        // Typos and wrong values
        let wrong = [
            "[target.a]\nincldue = []\n",
            "[target.a]\nkind = \"exe2\"\n",
            "[target.a.when.windows]\ndir = \"x\"\n",
            "[defaults]\nkind = \"lib\"\n",
            "[target.a]\nwarnings = \"hihg\"\n",
            "[target.a]\nstd = \"c++21\"\n",
            "[target.a]\nunity = 1\n",
            "[profile.a]\nsanitize = [\"adress\"]\n",
            "[deps]\nfmt = \"vcpkgg\"\n",
            "[deps]\nfmt = { vcpkg = \"fmt\", feature = [] }\n",
            "[options]\nx = { default = true, desc = \"\" }\n",
            "[[test]]\nname = \"a\"\ncommand = \"a\"\ntimout = 1\n",
            "[[test]]\nname = \"a\"\n",
            "[[test]]\nname = \"a_{v}\"\ncommand = \"a\"\nmatrix.v = [{ v = \"x\", timeout = \"1\" }]\n",
            "[[target.a.run]]\noutput = \"a\"\n",
            "[project]\ngui = true\n",
        ];
        for text in wrong {
            assert!(!validate(&root, &as_json(text)).is_empty(), "the schema takes:\n{text}");
        }

        // Values the parser or the model rejects, which the schema must too; and some it takes
        let rejected = [
            "[defaults.when.windows]\nkind = \"lib\"\n",
            "[preset.p.when.windows]\nkind = \"shared\"\n",
            "[target.a]\nfile = [{ match = \"*.c\" }]\nfiles = [{ match = \"*.c\" }]\n",
            "[[test]]\nname = \"a_{v}\"\ncommand = \"a\"\nmatrix.v = [true]\n",
            "[[test]]\nname = \"a_{v}\"\nmatrix.v = [\"x\"]\n",
            "[project]\nversion = \"2.1 beta\"\n",
            "[project]\nversion = \"\"\n",
        ];
        let accepted = [
            "[target.t]\nkind = \"test\"\ntimeout = 5000000000\n",
            "[[test]]\nname = \"a_{v}\"\nmatrix.v = [{ v = \"x\", command = \"a\" }]\n",
            "[project]\nversion = \"2.1\"\n",
            "[project]\nversion = \"v1.4.0-beta.2\"\n",
        ];
        let condition = crate::cond::CondContext {
            family: crate::toolchain::Family::Msvc,
            platform: crate::platform::Platform::host(),
            profiles: vec!["debug".into()],
            all_profiles: Vec::new(),
            options: Default::default(),
        };
        for (texts, rejects) in [(&rejected[..], true), (&accepted[..], false)] {
            for text in texts {
                // What Model::load checks of each package: the version, then every target table
                let loads = manifest::parse_str(text).is_ok_and(|manifest| {
                    let version = manifest.project.as_ref().and_then(|project| project.version.as_deref()).unwrap_or("1");
                    crate::project::check_version(version).is_ok() && crate::targets::check_package(&manifest, &condition).is_ok()
                });
                assert_eq!(loads, !rejects, "anvil {}:\n{text}", if rejects { "takes" } else { "rejects" });
                assert_eq!(validate(&root, &as_json(text)).is_empty(), !rejects, "the schema disagrees:\n{text}");
            }
        }
        assert!(!validate(&tests_root, &as_json("[project]\nname = \"a\"\n")).is_empty());
    }

    fn as_json(text: &str) -> Value {
        serde_json::to_value(toml::from_str::<toml::Value>(text).unwrap()).unwrap()
    }

    /// The draft-07 keywords this schema uses, checked as a validator would; the problems found, by path.
    fn validate(root: &Value, value: &Value) -> Vec<String> {
        let mut errors = Vec::new();
        check(root, root, value, "", &mut errors);
        errors
    }

    fn check(schema: &Value, root: &Value, value: &Value, path: &str, errors: &mut Vec<String>) {
        if let Some(reference) = schema["$ref"].as_str() {
            let name = reference.trim_start_matches("#/definitions/");
            return check(&root["definitions"][name], root, value, path, errors);
        }
        let fits = |schema: &Value| {
            let mut found = Vec::new();
            check(schema, root, value, path, &mut found);
            found.is_empty()
        };

        // Combinations
        for part in schema["allOf"].as_array().into_iter().flatten() {
            check(part, root, value, path, errors);
        }
        if let Some(alternatives) = schema["anyOf"].as_array() {
            if !alternatives.iter().any(fits) {
                errors.push(format!("{path}: fits none of the alternatives"));
            }
        }
        if let Some(negated) = schema.get("not") {
            if fits(negated) {
                errors.push(format!("{path}: fits what `not` excludes"));
            }
        }
        if let Some(condition) = schema.get("if") {
            let branch = if fits(condition) { &schema["then"] } else { &schema["else"] };
            if !branch.is_null() {
                check(branch, root, value, path, errors);
            }
        }

        // The value itself
        if let Some(values) = schema["enum"].as_array() {
            if !values.contains(value) {
                errors.push(format!("{path}: {value} is not one of {values:?}"));
            }
        }
        let kinds: Vec<&str> = match &schema["type"] {
            Value::String(kind) => vec![kind],
            Value::Array(kinds) => kinds.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        let kind = match value {
            Value::String(_) => "string",
            Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
            Value::Number(_) => "number",
            Value::Bool(_) => "boolean",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
            Value::Null => "null",
        };
        if !kinds.is_empty() && !kinds.contains(&kind) {
            errors.push(format!("{path}: a {kind}, expected {kinds:?}"));
            return;
        }
        if let (Some(number), Some(minimum)) = (value.as_i64(), schema["minimum"].as_i64()) {
            if number < minimum {
                errors.push(format!("{path}: {number} is below {minimum}"));
            }
        }
        if let (Some(text), Some(pattern)) = (value.as_str(), schema["pattern"].as_str()) {
            if !regex::Regex::new(pattern).unwrap().is_match(text) {
                errors.push(format!("{path}: `{text}` does not match {pattern}"));
            }
        }

        // Lists and tables
        if let Value::Array(items) = value {
            if items.len() < schema["minItems"].as_u64().unwrap_or(0) as usize {
                errors.push(format!("{path}: too few items"));
            }
            for (index, item) in items.iter().enumerate().filter(|_| schema.get("items").is_some()) {
                check(&schema["items"], root, item, &format!("{path}[{index}]"), errors);
            }
        }
        if let Value::Object(table) = value {
            for required in schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                if !table.contains_key(required) {
                    errors.push(format!("{path}: `{required}` is missing"));
                }
            }
            for (key, item) in table {
                let inner = format!("{path}.{key}");
                match (schema["properties"].get(key), &schema["additionalProperties"]) {
                    (Some(property), _) => check(property, root, item, &inner, errors),
                    (None, Value::Bool(false)) => errors.push(format!("{inner}: unknown key")),
                    (None, other) if other.is_object() => check(other, root, item, &inner, errors),
                    _ => {}
                }
            }
        }
    }
}
