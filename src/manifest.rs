//! The `anvil.toml` schema. Every table rejects unknown keys, so a typo is an error with a suggestion instead of a
//! silently ignored setting.

use crate::bail_at;
use crate::located::{self, At, Located, Part, Place, Scope};
use anyhow::{bail, Context, Result};
use indexmap::IndexMap;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Manifest {
    pub project: Option<ProjectSpec>,
    pub workspace: Option<WorkspaceSpec>,
    #[serde(default)]
    pub defaults: TargetSpec,
    #[serde(default)]
    pub deps: IndexMap<String, toml::Value>,
    /// Dependencies of the tests only: never linked into the package's programs and libraries.
    #[serde(default)]
    pub dev_deps: IndexMap<String, toml::Value>,
    #[serde(default)]
    pub target: IndexMap<String, TargetSpec>,
    /// Named groups of settings that targets opt into with `preset = "name"`.
    #[serde(default)]
    pub preset: IndexMap<String, TargetSpec>,
    #[serde(default)]
    pub profile: IndexMap<String, ProfileSpec>,
    #[serde(default)]
    pub options: IndexMap<String, toml::Value>,
    pub vcpkg: Option<VcpkgSpec>,
    /// [[test]] tables as written; `test` holds their tests.
    #[serde(default, rename = "test")]
    pub test_tables: Vec<toml::Table>,
    /// The [[test]] tables' tests, each table with its cases expanded (crate::test_cases).
    #[serde(skip)]
    pub test: Vec<TestSpec>,
    /// Files written before the build (what CMake writes at configure time).
    #[serde(default)]
    pub write: Vec<WriteSpec>,

    /// The file it was read from (none for a package made from a folder's layout).
    #[serde(skip)]
    pub file: Option<PathBuf>,
}

/// `[[write]] path = "{out}/generated/list.txt", text = "..."`: rewritten only when the text changes.
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct WriteSpec {
    pub path: String,
    pub text: String,
    #[serde(rename = "if")]
    pub cond: Option<String>,
}

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ProjectSpec {
    pub name: Option<String>,

    /// The {version} placeholder.
    pub version: Option<String>,

    // Package metadata: accepted for the record, Anvil does not use it
    #[allow(dead_code)]
    pub description: Option<String>,
    #[allow(dead_code)]
    pub license: Option<String>,
    #[allow(dead_code)]
    pub authors: Option<Vec<String>>,

    /// C++ standard: "c++17", "c++20" (default), "c++23", "c++26" (also "17", "20" ...).
    pub std: Option<String>,
    /// C standard: "gnu17" (default), "c17" (strict ISO), "c11", "c23".
    pub c_std: Option<String>,
    /// msvc | clang-cl | clang | gcc. Default: the best one installed.
    pub toolchain: Option<String>,
    /// A compiler cache C and C++ compiles run through: "sccache", "ccache" or a path (ANVIL_LAUNCHER overrides it).
    pub launcher: Option<String>,
    /// Output folder (default "out").
    pub out: Option<String>,
    /// The executable `anvil run` starts when there are several.
    pub default_run: Option<String>,
    /// `anvil install` writes CMake and pkg-config files for the installed libraries (default true).
    pub export: Option<bool>,
}

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct WorkspaceSpec {
    pub members: Vec<String>,
}

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct VcpkgSpec {
    /// A vcpkg checkout to use for this project (overrides the user's choice).
    pub root: Option<String>,
    pub triplet: Option<String>,
    /// "static" or "dynamic" libraries: the triplet for each machine and compiler follows.
    pub linkage: Option<String>,
    pub baseline: Option<String>,
    /// Path to a vcpkg.json to install from (default: the project's own vcpkg.json when there is one).
    pub manifest: Option<String>,
    #[serde(default)]
    pub overlay_ports: Vec<String>,
    #[serde(default)]
    pub overlay_triplets: Vec<String>,
}

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ProfileSpec {
    pub inherits: Option<String>,
    /// The machine to build for: "arm64", "x86", or a triple such as "aarch64-linux-gnu" (a cross build).
    pub target: Option<String>,
    /// 0, 1, 2, 3, "s" (size), "z" (smallest).
    pub opt: Option<toml::Value>,
    pub debug: Option<bool>,
    pub lto: Option<bool>,
    pub sanitize: Option<Vec<String>>,
    /// MSVC's debug C runtime (/MDd) and vcpkg's debug libraries.
    pub debug_runtime: Option<bool>,
    /// Link the C/C++ runtime statically (/MT, -static).
    pub static_runtime: Option<bool>,
    /// false defines NDEBUG.
    pub assertions: Option<bool>,
    pub defines: Option<Vec<String>>,
    pub flags: Option<Vec<String>>,
    pub link_flags: Option<Vec<String>>,
}

/// A target, and also the shape of `[defaults]` and `[...when.<cond>]` blocks.
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct TargetSpec {
    /// exe | lib | shared | object | headers | test | custom
    pub kind: Option<String>,
    /// Folder the target's relative paths start from (default: the package root).
    pub dir: Option<String>,
    /// The target exists only when this condition holds.
    #[serde(rename = "if")]
    pub cond: Option<String>,
    /// Build it on a plain `anvil build` (default true; tests: false).
    pub default: Option<bool>,
    /// `[preset.<name>]` settings to apply before the target's own: a name or a list of names.
    pub preset: Option<StrOrList>,

    /// Source globs. Under a `when` block they belong to that condition: added when it holds, and taken out of
    /// the layout convention when it does not.
    pub sources: Option<Vec<String>>,

    // Everything here is the target's own; `public` and `interface` say what its dependents get
    /// Include folders (default: src/ when it exists).
    pub include: Option<Vec<String>>,
    /// Include folders whose warnings are silenced (vendored code).
    pub system_include: Option<Vec<String>>,
    pub defines: Option<Vec<String>>,
    pub flags: Option<Vec<String>>,
    pub link_flags: Option<Vec<String>>,
    /// System or prebuilt libraries: "user32", "pthread", or a path to a .lib/.a.
    pub libs: Option<Vec<String>>,
    pub lib_dirs: Option<Vec<String>>,
    /// Targets and [deps] this one uses: linked, their public settings seen, not passed on.
    pub deps: Option<Vec<String>>,
    /// Settings the target and its dependents both get.
    pub public: Option<PublicSpec>,
    /// Settings only its dependents get (`FOO_API=__declspec(dllimport)`).
    pub interface: Option<InterfaceSpec>,

    pub pch: Option<String>,
    /// true, false, or the number of sources per unity file.
    pub unity: Option<toml::Value>,
    /// off | default | high | all
    pub warnings: Option<String>,
    pub werror: Option<bool>,
    /// avx | avx2 | avx512 | native
    pub arch: Option<String>,
    /// Link-time optimization of the target's code, in place of the profile's `lto`.
    pub lto: Option<bool>,
    /// GPUs .cu files compile for: "native", "all", "86" (code and PTX), "86-real", "86-virtual".
    pub cuda_arch: Option<StrOrList>,
    /// Extra nvcc options for .cu files.
    pub cuda_flags: Option<Vec<String>>,
    pub std: Option<String>,
    pub c_std: Option<String>,
    /// Windows GUI program: true for one entered through WinMain (CMake's WIN32), "main" for one with main().
    pub gui: Option<toml::Value>,
    pub output_name: Option<String>,
    /// Replace the platform's file name prefix ("lib") and extension (".dll", ".so", ".exe").
    pub output_prefix: Option<String>,
    pub output_suffix: Option<String>,
    /// The folder the target's file goes to (default: {out}, static libraries {out}/lib).
    pub output_dir: Option<String>,
    /// A shared library's version and ABI version: libfoo.so.1.2.3 with the SONAME libfoo.so.1 and the links
    /// libfoo.so.1 and libfoo.so (macOS: libfoo.1.2.3.dylib); a missing one is the other. Other kinds ignore them.
    pub version: Option<String>,
    pub soversion: Option<String>,
    /// A program that exports symbols for the modules it loads: those modules link against it.
    pub exports: Option<bool>,
    /// Write an export macro header for this library: a path under {gen}, or a table with `path` and `macro`.
    pub export_header: Option<ExportHeaderSpec>,
    /// Targets whose include folders and definitions this one uses without linking them.
    pub header_deps: Option<Vec<String>>,
    /// Targets built before this one, without linking or including anything of theirs.
    pub needs: Option<Vec<String>>,
    /// Files whose change relinks the target (a .def file, a linker script).
    pub link_depends: Option<Vec<String>>,
    /// Static libraries linked whole, every object in them, even those nothing here calls (a program that loads
    /// plugins and exports the engine to them).
    pub whole_archive: Option<Vec<String>>,
    /// DLLs a Windows binary loads on its first call into them instead of at start ("nvml.dll"); ignored elsewhere.
    pub delay_load: Option<Vec<String>>,
    /// "c++" or "c": the compiler driver that links it, as CMake's LINKER_LANGUAGE (default: by the languages compiled
    /// into it and its static libraries). On a static or object library: what the binaries linking it count it as.
    pub link_language: Option<String>,
    /// Files (globs relative to `dir`) copied beside the program or shared library, keeping their path under `dir`.
    pub data: Option<Vec<String>>,
    /// `anvil install` installs it (default: whether it is a default target; tests never).
    pub install: Option<bool>,
    /// Qt's moc, uic and rcc for a target that uses Qt (default true); false leaves its files to build steps.
    pub qt: Option<bool>,

    #[serde(default)]
    pub when: IndexMap<String, TargetSpec>,
    #[serde(default, alias = "files")]
    pub file: Vec<FileRule>,
    #[serde(default)]
    pub run: Vec<RunSpec>,
    /// Commands run before the target compiles, and after it links.
    #[serde(default)]
    pub pre_build: Vec<StepSpec>,
    #[serde(default)]
    pub post_build: Vec<StepSpec>,

    // Tests (kind = "test"):
    pub args: Option<Vec<String>>,
    pub workdir: Option<String>,
    pub timeout: Option<u64>,
    pub env: Option<IndexMap<String, String>>,
    /// The test passes when it fails.
    pub will_fail: Option<bool>,
    /// Tests that must finish first.
    pub after: Option<Vec<String>>,
    /// Run with no other test at the same time.
    pub serial: Option<bool>,
    /// Pass when the output matches (the exit code is then ignored); fail when it matches.
    pub pass_regex: Option<String>,
    pub fail_regex: Option<String>,
    /// The test reports it could not run (a missing tool): skipped, not failed.
    pub skip_exit_code: Option<i32>,
    pub skip_regex: Option<String>,
    /// Regexes the output must each match, and regexes it must not match.
    pub expect: Option<Vec<String>>,
    pub reject: Option<Vec<String>>,
    /// The exit code that passes (default 0).
    pub exit_code: Option<i32>,
    /// Names `anvil test -L` picks tests by.
    pub labels: Option<Vec<String>>,
}

/// `[target.X.public]`: what the target and every target using it get.
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct PublicSpec {
    /// Include folders (default: include/ when it exists).
    pub include: Option<Vec<String>>,
    pub system_include: Option<Vec<String>>,
    pub defines: Option<Vec<String>>,
    pub flags: Option<Vec<String>>,
    /// Dependencies whose public settings pass on to this target's dependents too.
    pub deps: Option<Vec<String>>,
}

/// `[target.X.interface]`: what only the targets using it get.
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct InterfaceSpec {
    pub include: Option<Vec<String>>,
    pub defines: Option<Vec<String>>,
    pub flags: Option<Vec<String>>,
}

/// `export-header = "lib/export.h"` or `{ path = "lib/export.h", macro = "LIB_API" }`.
#[derive(Debug, Clone)]
pub enum ExportHeaderSpec {
    Path(String),
    Table { path: String, macro_name: Option<String> },
}

/// The table form of `export-header`, which rejects unknown keys like every other table.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportHeaderTable {
    path: String,
    #[serde(rename = "macro")]
    macro_name: Option<String>,
}

/// A pre-build or post-build command: no outputs, it runs when the target's sources (pre) or file (post) change.
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct StepSpec {
    pub command: StrOrList,
    #[serde(default)]
    pub shell: bool,
    pub message: Option<String>,
    pub workdir: Option<String>,
    #[serde(rename = "if")]
    pub cond: Option<String>,
}

/// Settings for some of a target's source files: `[[target.x.file]] match = "src/simd/*.cpp", arch = "avx2"`.
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct FileRule {
    #[serde(rename = "match")]
    pub patterns: StrOrList,
    #[serde(rename = "if")]
    pub cond: Option<String>,
    pub defines: Option<Vec<String>>,
    pub flags: Option<Vec<String>>,
    pub warnings: Option<String>,
    pub arch: Option<String>,
    pub opt: Option<toml::Value>,
    pub lto: Option<bool>,
}

/// A build step: code generation, shader compilation, copying.
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct RunSpec {
    /// Run the command once per file matching these globs ({input}, {stem}, {name}).
    pub for_each: Option<StrOrList>,
    /// Extra files whose change re-runs it.
    #[serde(default)]
    pub inputs: Vec<String>,
    pub output: Option<String>,
    #[serde(default)]
    pub outputs: Vec<String>,
    pub command: StrOrList,
    /// The command writes a make-style depfile to {depfile}.
    #[serde(default)]
    pub depfile: bool,
    /// Run through the shell (cmd /c, sh -c).
    #[serde(default)]
    pub shell: bool,
    pub message: Option<String>,
    pub workdir: Option<String>,
    #[serde(rename = "if")]
    pub cond: Option<String>,
}

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct TestSpec {
    pub name: String,
    pub command: StrOrList,
    pub workdir: Option<String>,
    pub timeout: Option<u64>,
    pub env: Option<IndexMap<String, String>>,
    #[serde(rename = "if")]
    pub cond: Option<String>,
    /// The test passes when it fails.
    pub will_fail: Option<bool>,
    /// Tests that must finish first.
    pub after: Option<Vec<String>>,
    /// Run with no other test at the same time.
    pub serial: Option<bool>,
    /// Pass when the output matches (the exit code is then ignored); fail when it matches.
    pub pass_regex: Option<String>,
    pub fail_regex: Option<String>,
    /// The test reports it could not run (a missing tool): skipped, not failed.
    pub skip_exit_code: Option<i32>,
    pub skip_regex: Option<String>,
    /// Regexes the output must each match, and regexes it must not match.
    pub expect: Option<Vec<String>>,
    pub reject: Option<Vec<String>>,
    /// The exit code that passes (default 0).
    pub exit_code: Option<i32>,
    /// Names `anvil test -L` picks tests by.
    pub labels: Option<Vec<String>>,

    /// Which [[test]] table of its file it comes from.
    #[serde(skip)]
    pub table: usize,
}

#[derive(Debug, Clone)]
pub enum StrOrList {
    Str(String),
    List(Vec<String>),
}

impl Default for StrOrList {
    fn default() -> Self {
        StrOrList::List(vec![])
    }
}

impl StrOrList {
    pub fn to_vec(&self) -> Vec<String> {
        match self {
            StrOrList::Str(single) => vec![single.clone()],
            StrOrList::List(list) => list.clone(),
        }
    }

    /// A command: a list as is, a string split into words.
    pub fn to_command(&self) -> Vec<String> {
        match self {
            StrOrList::Str(command_line) => crate::util::split_command(command_line),
            StrOrList::List(words) => words.clone(),
        }
    }
}

// Written by hand: an untagged enum's error says only "data did not match any variant"
impl<'de> Deserialize<'de> for StrOrList {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct TextOrTexts;

        impl<'de> serde::de::Visitor<'de> for TextOrTexts {
            type Value = StrOrList;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a text or a list of texts")
            }

            fn visit_str<E: serde::de::Error>(self, text: &str) -> std::result::Result<StrOrList, E> {
                Ok(StrOrList::Str(text.to_string()))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut sequence: A) -> std::result::Result<StrOrList, A::Error> {
                let mut list = Vec::new();
                while let Some(item) = sequence.next_element::<toml::Value>()? {
                    match item {
                        toml::Value::String(text) => list.push(text),
                        other => {
                            return Err(serde::de::Error::custom(format!(
                                "expected a text or a list of texts, found {} in the list",
                                other.type_str()
                            )))
                        }
                    }
                }
                Ok(StrOrList::List(list))
            }
        }

        deserializer.deserialize_any(TextOrTexts)
    }
}

impl<'de> Deserialize<'de> for ExportHeaderSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct PathOrTable;

        impl<'de> serde::de::Visitor<'de> for PathOrTable {
            type Value = ExportHeaderSpec;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a header path, or a table with `path` and `macro`")
            }

            fn visit_str<E: serde::de::Error>(self, text: &str) -> std::result::Result<ExportHeaderSpec, E> {
                Ok(ExportHeaderSpec::Path(text.to_string()))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> std::result::Result<ExportHeaderSpec, A::Error> {
                let table = ExportHeaderTable::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(ExportHeaderSpec::Table {
                    path: table.path,
                    macro_name: table.macro_name,
                })
            }
        }

        deserializer.deserialize_any(PathOrTable)
    }
}

/// One `[deps]` entry, normalized from its shorthand or table form.
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct DepSpec {
    pub vcpkg: Option<String>,
    #[serde(default)]
    pub features: Vec<String>,
    pub default_features: Option<bool>,
    pub path: Option<String>,
    pub git: Option<String>,
    pub tag: Option<String>,
    pub branch: Option<String>,
    pub rev: Option<String>,
    pub pkg_config: Option<String>,
    /// An installed CMake package, found as find_package(<name> CONFIG) finds it: its components, the version it must
    /// be compatible with, which of its imported targets (default: those of the components, else its libraries), and
    /// the install prefixes to look in instead of CMake's search.
    pub cmake: Option<String>,
    pub components: Option<Vec<String>>,
    pub version: Option<String>,
    pub targets: Option<Vec<String>>,
    pub prefix: Option<StrOrList>,
    /// For path/git packages: which of its targets (default: its library).
    pub target: Option<String>,
    #[serde(rename = "if")]
    pub cond: Option<String>,
    // Prebuilt libraries, and overrides for packages without an anvil.toml:
    pub include: Option<Vec<String>>,
    pub sources: Option<Vec<String>>,
    pub defines: Option<Vec<String>>,
    pub flags: Option<Vec<String>>,
    pub libs: Option<Vec<String>>,
    pub lib_dirs: Option<Vec<String>>,
    pub link_flags: Option<Vec<String>>,
    /// Files copied next to the executables (DLLs).
    pub runtime: Option<Vec<String>>,
}

const DEFAULT_UNITY_BATCH: usize = 8;

// The keys of an option's table form
pub const OPTION_KEYS: &[&str] = &["default", "description"];

/// Reads one `[deps]` entry, from its short form ("vcpkg", "pkg-config", "cmake", "system") or its table.
pub fn parse_dep(name: &str, value: &toml::Value) -> Result<DepSpec> {
    match value {
        toml::Value::String(short_form) => match short_form.as_str() {
            "vcpkg" => Ok(DepSpec {
                vcpkg: Some(name.into()),
                ..Default::default()
            }),
            "pkg-config" => Ok(DepSpec {
                pkg_config: Some(name.into()),
                ..Default::default()
            }),
            "cmake" => Ok(DepSpec {
                cmake: Some(name.into()),
                ..Default::default()
            }),
            "system" => Ok(DepSpec {
                libs: Some(vec![name.into()]),
                ..Default::default()
            }),
            other => bail_at!(
                Place::keys(&["deps", name]).value(other),
                "invalid-dependency",
                "dependency `{name} = \"{other}\"`: the short form is \"vcpkg\", \"pkg-config\", \"cmake\" or \"system\"; \
                 otherwise use a table, e.g. {name} = {{ path = \"../{name}\" }} or {{ git = \"https://...\", tag = \"v1.0\" }}"
            ),
        },
        toml::Value::Table(_) => parse_dep_table(name, value),
        _ => {
            let place = Place::keys(&["deps", name]);
            let place = match located::value_text(value) {
                Some(text) => place.value(text),
                None => place,
            };
            bail_at!(place, "invalid-dependency", "dependency `{name}`: expected a string or a table")
        }
    }
}

/// `[options]`: `name = true` or `name = { default = true, description = "..." }`.
pub fn parse_options(options: &IndexMap<String, toml::Value>) -> Result<IndexMap<String, bool>> {
    let mut parsed = IndexMap::new();

    for (name, value) in options {
        let option = Place::keys(&["options", name]);
        let default = match value {
            toml::Value::Boolean(default) => *default,
            toml::Value::Table(table) => {
                if let Some(key) = table.keys().find(|key| !OPTION_KEYS.contains(&key.as_str())) {
                    let hint = crate::util::did_you_mean(key, OPTION_KEYS.iter().copied());
                    bail_at!(
                        option.join(key),
                        "bad-option",
                        "option `{name}`: unknown key `{key}`{hint}; an option takes `default` and `description`"
                    );
                }
                match (table.get("default"), table.get("description")) {
                    (_, Some(description)) if !description.is_str() => bail_at!(
                        with_value(option.join("description"), description),
                        "bad-option",
                        "option `{name}`: description must be a text"
                    ),
                    (Some(toml::Value::Boolean(default)), _) => *default,
                    (default, _) => bail_at!(
                        match default {
                            Some(default) => with_value(option.join("default"), default),
                            None => option,
                        },
                        "bad-option",
                        "option `{name}`: expected `{name} = true|false` or `{name} = {{ default = true, description = \"...\" }}`"
                    ),
                }
            }
            _ => bail_at!(with_value(option, value), "bad-option", "option `{name}`: expected true or false"),
        };
        parsed.insert(name.clone(), default);
    }

    Ok(parsed)
}

/// `unity`: true (8 per file), false, or a batch size.
pub fn parse_unity(value: &toml::Value) -> Result<Option<usize>> {
    match value {
        toml::Value::Boolean(true) => Ok(Some(DEFAULT_UNITY_BATCH)),
        toml::Value::Boolean(false) => Ok(None),
        toml::Value::Integer(batch) if *batch >= 2 => Ok(Some(*batch as usize)),
        toml::Value::Integer(batch) => bail_at!(
            Place::key("unity").value(batch.to_string()),
            "invalid-value",
            "unity = {batch}: a batch holds 2 files or more; write unity = false to turn it off"
        ),
        _ => bail_at!(
            with_value(Place::key("unity"), value),
            "invalid-value",
            "unity: expected true, false or a number of files per batch"
        ),
    }
}

pub fn load(path: &Path) -> Result<Manifest> {
    let text = crate::lsp::session::read_text(path).with_context(|| format!("reading {}", path.display()))?;
    let mut manifest = parse_str(&text).at(Scope::file(path).text(crate::util::slash(path)))?;
    manifest.file = Some(path.to_path_buf());

    Ok(manifest)
}

/// A tests.toml file: [[test]] tables only, kept next to what they test.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TestsFile {
    #[serde(default)]
    test: Vec<toml::Table>,
}

pub fn load_tests(path: &Path) -> Result<Vec<TestSpec>> {
    let text = crate::lsp::session::read_text(path).with_context(|| format!("reading {}", path.display()))?;
    parse_tests(&text).at(Scope::file(path).text(crate::util::slash(path)))
}

/// A tests.toml file's text.
pub fn parse_tests(text: &str) -> Result<Vec<TestSpec>> {
    let file: TestsFile = toml::from_str(text)?;
    expand_tests(&file.test)
}

/// [[test]] tables, one test per case.
fn expand_tests(tables: &[toml::Table]) -> Result<Vec<TestSpec>> {
    let mut tests = Vec::new();

    for (index, table) in tables.iter().enumerate() {
        let expanded = crate::test_cases::expand(table).at(Scope::item("test", index))?;
        for test in expanded {
            let mut spec = test_spec(test).at(Scope::item("test", index))?;
            spec.table = index;
            tests.push(spec);
        }
    }

    Ok(tests)
}

/// One test's table.
fn test_spec(test: toml::Table) -> Result<TestSpec> {
    let name = test.get("name").and_then(|name| name.as_str()).unwrap_or("?").to_string();
    let probe = toml::Table::from_iter([("name".to_string(), "x".into()), ("command".to_string(), "x".into())]);
    let spec = TestSpec::deserialize(toml::Value::Table(test.clone()))
        .map_err(|error| table_error::<TestSpec>(&test, &error, &probe, &format!("test `{name}`: ")))?;

    if spec.command.to_command().is_empty() {
        bail_at!(
            Place::key("command"),
            "empty-command",
            "test `{name}`: `command` is empty; give the program and its arguments, such as [\"{{target:app}}\", \"--self-test\"]"
        );
    }

    Ok(spec)
}

/// A table's deserialize error, at the key it is about: an unknown one (with a did-you-mean), else the first whose
/// value alone fails beside `probe` (the keys the table needs). `prefix` goes before serde's message.
fn table_error<T: serde::de::DeserializeOwned>(table: &toml::Table, error: &toml::de::Error, probe: &toml::Table, prefix: &str) -> anyhow::Error {
    let message = explain_unknown_key(error.message());

    let unknown = regex::Regex::new(r"unknown field `([^`]+)`")
        .unwrap()
        .captures(&message)
        .map(|captures| captures[1].to_string());
    let place = match unknown {
        Some(key) => Place::key(&key),
        None => {
            let failing = table.iter().find(|(key, value)| {
                let mut alone = probe.clone();
                alone.insert(key.to_string(), (*value).clone());
                T::deserialize(toml::Value::Table(alone)).is_err()
            });
            match failing {
                Some((key, value)) => with_value(Place::key(key), value),
                None => Place::default(),
            }
        }
    };

    Located::new(place, serde_code(&message), format!("{prefix}{message}")).into()
}

/// The diagnostic code of serde's message.
fn serde_code(message: &str) -> &'static str {
    match message {
        _ if message.starts_with("unknown field") => "unknown-key",
        _ if message.starts_with("invalid type") => "wrong-type",
        _ if message.starts_with("missing field") => "missing-key",
        _ if message.starts_with("duplicate") => "duplicate-key",
        _ => "invalid-value",
    }
}

/// A place with the scalar value it is about, when the value is one.
fn with_value(place: Place, value: &toml::Value) -> Place {
    match located::value_text(value) {
        Some(text) => place.value(text),
        None => place,
    }
}

/// A build step's command must name a program: an empty one is a mistake, not a step that does nothing. `path` is
/// the table's key path.
fn check_step_commands(table: &str, path: &[Part], spec: &TargetSpec) -> Result<()> {
    let commands = [
        ("run", spec.run.iter().map(|run| &run.command).collect::<Vec<_>>()),
        ("pre-build", spec.pre_build.iter().map(|step| &step.command).collect()),
        ("post-build", spec.post_build.iter().map(|step| &step.command).collect()),
    ];
    for (key, list) in commands {
        for (index, command) in list.into_iter().enumerate() {
            if command.to_command().is_empty() {
                let mut place = path.to_vec();
                place.extend([Part::Key(key.to_string()), Part::Index(index), Part::Key("command".to_string())]);
                bail_at!(
                    Place::absolute(place),
                    "empty-command",
                    "{table} [[{key}]]: `command` is empty; give the program and its arguments, such as [\"python\", \"tools/gen.py\"]"
                );
            }
        }
    }
    for (condition, block) in &spec.when {
        let mut block_path = path.to_vec();
        block_path.extend(located::keys(&["when", condition]));
        check_step_commands(&format!("{table} when.\"{condition}\""), &block_path, block)?;
    }

    Ok(())
}

pub fn parse_str(text: &str) -> Result<Manifest> {
    let error = match toml::from_str::<Manifest>(text) {
        Ok(manifest) => return finish(manifest),
        Err(error) => error,
    };

    bail!("{}", explain_unknown_key(&error.to_string()))
}

/// A manifest's checks after its tables parse: its tests expanded, its name, its build steps' commands.
pub fn finish(mut manifest: Manifest) -> Result<Manifest> {
    manifest.test = expand_tests(&manifest.test_tables)?;

    // The name becomes target and folder names
    if let Some(name) = manifest.project.as_ref().and_then(|project| project.name.as_deref()) {
        if name.contains(['/', '\\']) || name.trim().is_empty() {
            bail_at!(
                Place::keys(&["project", "name"]).value(name),
                "invalid-name",
                "[project] name = \"{name}\": a name cannot be empty or hold `/` or `\\` (it names the main target)"
            );
        }
    }
    check_step_commands("[defaults]", &located::keys(&["defaults"]), &manifest.defaults)?;
    for (table, specs) in [("target", &manifest.target), ("preset", &manifest.preset)] {
        for (name, spec) in specs {
            check_step_commands(&format!("[{table}.{name}]"), &located::keys(&[table, name]), spec)?;
        }
    }

    Ok(manifest)
}

fn parse_dep_table(name: &str, value: &toml::Value) -> Result<DepSpec> {
    let table = value.as_table().cloned().unwrap_or_default();
    let spec: DepSpec = value
        .clone()
        .try_into()
        .map_err(|error: toml::de::Error| table_error::<DepSpec>(&table, &error, &toml::Table::new(), &format!("dependency `{name}`: ")))
        .at(Scope::keys(&["deps", name]))?;
    let entry = |key: &str| Place::keys(&["deps", name, key]);

    // Exactly one origin, or a prebuilt include/libs description
    let origins = [
        ("vcpkg", spec.vcpkg.is_some()),
        ("path", spec.path.is_some()),
        ("git", spec.git.is_some()),
        ("pkg-config", spec.pkg_config.is_some()),
        ("cmake", spec.cmake.is_some()),
    ];
    let given_origins: Vec<&str> = origins.iter().filter(|(_, given)| *given).map(|(key, _)| *key).collect();
    if given_origins.len() > 1 {
        bail_at!(
            entry(given_origins[1]),
            "invalid-dependency",
            "dependency `{name}`: give only one of vcpkg, path, git, pkg-config and cmake, not {}",
            given_origins.join(" and ")
        );
    }
    if given_origins.is_empty() && spec.include.is_none() && spec.libs.is_none() {
        bail_at!(
            Place::keys(&["deps", name]),
            "invalid-dependency",
            "dependency `{name}`: say where it comes from: vcpkg = \"{name}\", path = \"...\", git = \"...\", pkg-config = \"...\", \
             cmake = \"<Package>\" (an installed CMake package), or include/libs for a prebuilt one"
        );
    }

    // Keys that belong to one kind of dependency
    let source_package = spec.path.is_some() || spec.git.is_some();
    let references = [("tag", &spec.tag), ("branch", &spec.branch), ("rev", &spec.rev)];
    let given: Vec<&str> = references.iter().filter(|(_, value)| value.is_some()).map(|(key, _)| *key).collect();
    if given.len() > 1 {
        bail_at!(
            entry(given[1]),
            "invalid-dependency",
            "dependency `{name}`: give one of tag, branch and rev, not {}",
            given.join(" and ")
        );
    }
    if let (Some(key), None) = (given.first(), &spec.git) {
        bail_at!(
            entry(key),
            "invalid-dependency",
            "dependency `{name}`: `{key}` picks a commit of a git dependency; this one has no `git`"
        );
    }
    if spec.vcpkg.is_none() && (!spec.features.is_empty() || spec.default_features.is_some()) {
        let key = if spec.features.is_empty() { "default-features" } else { "features" };
        bail_at!(
            entry(key),
            "invalid-dependency",
            "dependency `{name}`: `features` and `default-features` are vcpkg's; this one has no `vcpkg`"
        );
    }
    let cmake_keys = [
        ("components", spec.components.is_some()),
        ("version", spec.version.is_some()),
        ("targets", spec.targets.is_some()),
        ("prefix", spec.prefix.is_some()),
    ];
    if let (Some((key, _)), None) = (cmake_keys.iter().find(|(_, given)| *given), &spec.cmake) {
        bail_at!(
            entry(key),
            "invalid-dependency",
            "dependency `{name}`: `{key}` belongs to an installed CMake package; this one has no `cmake = \"<Package>\"`"
        );
    }
    if let Some(version) = &spec.version {
        if crate::cmake_package::parse_version(version).is_none() {
            bail_at!(
                entry("version").value(version),
                "invalid-dependency",
                "dependency `{name}`: version = \"{version}\" is not a version; write numbers such as \"1.2\" or \"6.5.3\" (the oldest compatible one)"
            );
        }
    }
    if !source_package {
        if spec.target.is_some() || spec.sources.is_some() {
            let key = if spec.target.is_some() { "target" } else { "sources" };
            bail_at!(
                entry(key),
                "invalid-dependency",
                "dependency `{name}`: `target` and `sources` belong to a path or git dependency"
            );
        }
    } else {
        let prebuilt_keys = [
            ("libs", spec.libs.is_some()),
            ("lib-dirs", spec.lib_dirs.is_some()),
            ("flags", spec.flags.is_some()),
            ("link-flags", spec.link_flags.is_some()),
            ("runtime", spec.runtime.is_some()),
        ];
        if let Some((key, _)) = prebuilt_keys.iter().find(|(_, given)| *given) {
            bail_at!(
                entry(key),
                "invalid-dependency",
                "dependency `{name}`: `{key}` describes a prebuilt or vcpkg library, not a path or git package; \
                 put it in the package's anvil.toml (a target's public settings) instead"
            );
        }
    }

    Ok(spec)
}

// Target keys an older format had, and what says the same now
const RENAMED_KEYS: &[(&str, &str)] = &[
    ("private-include", "settings are the target's own unless public, so write `include`"),
    ("public-defines", "write `defines` under [target.X.public]"),
    ("public-flags", "write `flags` under [target.X.public]"),
    ("interface-defines", "write `defines` under [target.X.interface]"),
    ("private-deps", "deps are private unless public, so write `deps`"),
];

/// serde says "unknown field `x`, expected one of `a`, `b`": add "(did you mean `a`?)" after the key, or what a key
/// an older format had is written as now.
pub fn explain_unknown_key(message: &str) -> String {
    let pattern = regex::Regex::new(r"unknown field `([^`]+)`, expected (?:one of )?(.*)").unwrap();
    let Some(captures) = pattern.captures(message) else {
        return message.to_string();
    };

    let misspelled = captures[1].to_string();
    if let Some((_, replacement)) = RENAMED_KEYS.iter().find(|(old, _)| *old == misspelled) {
        return format!("{message}\nhint: `{misspelled}` is gone: {replacement}");
    }

    let known: Vec<String> = captures[2]
        .split(", ")
        .map(|key| key.trim_matches(|character| character == '`' || character == ' ').to_string())
        .collect();
    let hint = crate::util::did_you_mean(&misspelled, known.iter().map(|key| key.as_str()));
    let field = format!("unknown field `{misspelled}`");

    message.replacen(&field, &format!("{field}{hint}"), 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal() {
        let manifest = parse_str("[project]\nname = \"x\"\n").unwrap();
        assert_eq!(manifest.project.unwrap().name.unwrap(), "x");
    }

    #[test]
    fn typo_hint() {
        let error = parse_str("[target.a]\nincldue = []\n").unwrap_err().to_string();
        assert!(error.contains("unknown field `incldue` (did you mean `include`?), expected"), "{error}");

        let manifest = parse_str("[options]\nx = { defualt = true }\n").unwrap();
        let error = parse_options(&manifest.options).unwrap_err().to_string();
        assert!(error.contains("unknown key `defualt` (did you mean `default`?)"), "{error}");

        let error = parse_str("[target.a]\npublic-defines = []\n").unwrap_err().to_string();
        assert!(error.contains("[target.X.public]"), "{error}");
    }

    #[test]
    fn deps() {
        let manifest = parse_str("[deps]\nfmt = \"vcpkg\"\nsdl3 = { vcpkg = \"sdl3\", features = [\"vulkan\"] }\n").unwrap();
        assert_eq!(parse_dep("fmt", &manifest.deps["fmt"]).unwrap().vcpkg.unwrap(), "fmt");
        assert_eq!(parse_dep("sdl3", &manifest.deps["sdl3"]).unwrap().features, vec!["vulkan"]);
        assert!(parse_dep("x", &toml::Value::String("bogus".into())).is_err());
    }

    #[test]
    fn cmake_deps() {
        let dep = |text: &str| {
            let manifest = parse_str(&format!("[deps]\nx = {text}\n")).unwrap();
            parse_dep("x", &manifest.deps["x"]).map_err(|error| error.to_string())
        };

        assert_eq!(dep("\"cmake\"").unwrap().cmake.unwrap(), "x");
        let qt = dep("{ cmake = \"Qt6\", components = [\"Widgets\"], version = \"6.5\", prefix = \"{env:QTDIR}\" }").unwrap();
        assert_eq!(
            (qt.cmake.unwrap(), qt.components.unwrap()),
            ("Qt6".to_string(), vec!["Widgets".to_string()])
        );
        assert!(dep("{ cmake = \"Qt6\", vcpkg = \"qtbase\" }")
            .unwrap_err()
            .contains("not vcpkg and cmake"));
        assert!(dep("{ pkg-config = \"x\", components = [\"a\"] }")
            .unwrap_err()
            .contains("`components` belongs to an installed CMake package"));
        assert!(dep("{ cmake = \"X\", version = \"1.x\" }").unwrap_err().contains("is not a version"));
        assert!(dep("{ cmake = \"X\", compnents = [] }")
            .unwrap_err()
            .contains("unknown field `compnents`"));
    }

    #[test]
    fn empty_commands_are_errors() {
        let error = parse_str("[target.a]\n[[target.a.run]]\ncommand = []\noutput = \"x.h\"\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("[target.a] [[run]]: `command` is empty"), "{error}");

        let error = parse_str("[target.a.when.windows]\npost-build = [{ command = \"\" }]\n")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("[target.a] when.\"windows\" [[post-build]]: `command` is empty"),
            "{error}"
        );

        let error = parse_str("[[test]]\nname = \"t\"\ncommand = []\n").unwrap_err().to_string();
        assert!(error.contains("test `t`: `command` is empty"), "{error}");
        assert!(parse_tests("[[test]]\nname = \"t\"\ncommand = \" \"\n").is_err());
    }

    #[test]
    fn when_blocks() {
        let manifest = parse_str("[target.a]\nsources = [\"a.cpp\"]\n[target.a.when.windows]\nlibs = [\"user32\"]\n").unwrap();
        assert!(manifest.target["a"].when.contains_key("windows"));
    }
}
