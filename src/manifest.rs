//! The `anvil.toml` schema. Every table rejects unknown keys, so a typo is an error with a suggestion instead of a
//! silently ignored setting.

use anyhow::{bail, Context, Result};
use indexmap::IndexMap;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Manifest {
    pub project: Option<ProjectSpec>,
    pub workspace: Option<WorkspaceSpec>,
    #[serde(default)]
    pub defaults: TargetSpec,
    #[serde(default)]
    pub deps: IndexMap<String, toml::Value>,
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
    /// [[test]] tables, each with its cases expanded (crate::test_cases).
    #[serde(default, deserialize_with = "deserialize_tests")]
    pub test: Vec<TestSpec>,
    /// Files written before the build (what CMake writes at configure time).
    #[serde(default)]
    pub write: Vec<WriteSpec>,
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
    /// C standard: "c11", "c17" (default), "c23".
    pub c_std: Option<String>,
    /// msvc | clang-cl | clang | gcc. Default: the best one installed.
    pub toolchain: Option<String>,
    /// Output folder (default "out").
    pub out: Option<String>,
    /// The executable `anvil run` starts when there are several.
    pub default_run: Option<String>,
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
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ExportHeaderSpec {
    Path(String),
    Table {
        path: String,
        #[serde(rename = "macro")]
        macro_name: Option<String>,
    },
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
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
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

/// Reads one `[deps]` entry, from its short form ("vcpkg", "pkg-config", "system") or its table.
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
            "system" => Ok(DepSpec {
                libs: Some(vec![name.into()]),
                ..Default::default()
            }),
            other => bail!(
                "dependency `{name} = \"{other}\"`: the short form is \"vcpkg\", \"pkg-config\" or \"system\"; \
                 otherwise use a table, e.g. {name} = {{ path = \"../{name}\" }} or {{ git = \"https://...\", tag = \"v1.0\" }}"
            ),
        },
        toml::Value::Table(_) => parse_dep_table(name, value),
        _ => bail!("dependency `{name}`: expected a string or a table"),
    }
}

/// `[options]`: `name = true` or `name = { default = true, description = "..." }`.
pub fn parse_options(options: &IndexMap<String, toml::Value>) -> Result<IndexMap<String, bool>> {
    let mut parsed = IndexMap::new();

    for (name, value) in options {
        let default = match value {
            toml::Value::Boolean(default) => *default,
            toml::Value::Table(table) => match table.get("default") {
                Some(toml::Value::Boolean(default)) => *default,
                _ => bail!("option `{name}`: expected `{name} = true|false` or `{name} = {{ default = true, description = \"...\" }}`"),
            },
            _ => bail!("option `{name}`: expected true or false"),
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
        toml::Value::Integer(_) => Ok(None),
        _ => bail!("unity: expected true, false or a number of files per batch"),
    }
}

pub fn load(path: &Path) -> Result<Manifest> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_str(&text).map_err(|error| anyhow::anyhow!("{}: {error}", crate::util::slash(path)))
}

/// A tests.toml file: [[test]] tables only, kept next to what they test.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TestsFile {
    #[serde(default, deserialize_with = "deserialize_tests")]
    test: Vec<TestSpec>,
}

pub fn load_tests(path: &Path) -> Result<Vec<TestSpec>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_tests(&text).map_err(|error| anyhow::anyhow!("{}: {error}", crate::util::slash(path)))
}

/// A tests.toml file's text.
pub fn parse_tests(text: &str) -> Result<Vec<TestSpec>> {
    let file: TestsFile = toml::from_str(text)?;
    Ok(file.test)
}

/// [[test]] tables, one test per case.
fn deserialize_tests<'de, D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Vec<TestSpec>, D::Error> {
    use serde::de::Error;

    let tables = Vec::<toml::Table>::deserialize(deserializer)?;
    let mut tests = Vec::new();
    for table in &tables {
        let expanded = crate::test_cases::expand(table).map_err(|error| D::Error::custom(format!("{error:#}")))?;
        for test in expanded {
            let name = test.get("name").and_then(|name| name.as_str()).unwrap_or("?").to_string();
            let spec = TestSpec::deserialize(toml::Value::Table(test)).map_err(|error| D::Error::custom(format!("test `{name}`: {error}")))?;
            tests.push(spec);
        }
    }

    Ok(tests)
}

pub fn parse_str(text: &str) -> Result<Manifest> {
    let error = match toml::from_str::<Manifest>(text) {
        Ok(manifest) => return Ok(manifest),
        Err(error) => error,
    };

    let mut message = error.to_string();
    if let Some(hint) = misspelled_key_hint(&message) {
        message.push_str(&hint);
    }

    bail!("{message}")
}

fn parse_dep_table(name: &str, value: &toml::Value) -> Result<DepSpec> {
    let spec: DepSpec = value
        .clone()
        .try_into()
        .map_err(|error: toml::de::Error| anyhow::anyhow!("dependency `{name}`: {}", error.message()))?;

    // Exactly one origin, or a prebuilt include/libs description
    let origins = [spec.vcpkg.is_some(), spec.path.is_some(), spec.git.is_some(), spec.pkg_config.is_some()];
    let origin_count = origins.iter().filter(|given| **given).count();
    if origin_count > 1 {
        bail!("dependency `{name}`: give only one of vcpkg, path, git, pkg-config");
    }
    if origin_count == 0 && spec.include.is_none() && spec.libs.is_none() {
        bail!(
            "dependency `{name}`: say where it comes from: vcpkg = \"{name}\", path = \"...\", git = \"...\", pkg-config = \"...\", \
             or include/libs for a prebuilt one"
        );
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

/// serde says "unknown field `x`, expected one of `a`, `b`": turn that into "did you mean `a`?".
fn misspelled_key_hint(message: &str) -> Option<String> {
    let pattern = regex::Regex::new(r"unknown field `([^`]+)`, expected (?:one of )?(.*)").unwrap();
    let captures = pattern.captures(message)?;

    let misspelled = captures[1].to_string();
    if let Some((_, replacement)) = RENAMED_KEYS.iter().find(|(old, _)| *old == misspelled) {
        return Some(format!("\nhint: `{misspelled}` is gone: {replacement}"));
    }

    let known: Vec<String> = captures[2]
        .split(", ")
        .map(|key| key.trim_matches(|character| character == '`' || character == ' ').to_string())
        .collect();

    let suggestion = crate::util::suggest(&misspelled, known.iter().map(|key| key.as_str()))?;
    Some(format!("\nhint: did you mean `{suggestion}`?"))
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
        assert!(error.contains("did you mean `include`"), "{error}");

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
    fn when_blocks() {
        let manifest = parse_str("[target.a]\nsources = [\"a.cpp\"]\n[target.a.when.windows]\nlibs = [\"user32\"]\n").unwrap();
        assert!(manifest.target["a"].when.contains_key("windows"));
    }
}
