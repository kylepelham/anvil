//! Resolves one target: `[defaults]`, the target's table and its `when` blocks merged; layout conventions filled in;
//! globs expanded; paths made absolute.

use crate::cond::CondContext;
use crate::flags::{normalize_standard, Arch, Gui, Language, Warnings};
use crate::manifest::{self, ExportHeaderSpec, FileRule, RunSpec, StepSpec, TargetSpec};
use crate::profile::Opt;
use crate::util::{self, slash};
use anyhow::{bail, Context, Result};
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use indexmap::IndexMap;
use std::path::{Path, PathBuf};

pub const SOURCE_GLOB: &str = "**/*.{c,cc,cpp,cxx,c++,cu}";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Exe,
    Test,
    Static,
    Shared,
    Object,
    Headers,
    Custom,
    External,
}

/// A resolved target: everything the planner needs, with absolute paths.
#[derive(Debug, Clone)]
pub struct Target {
    pub name: String,
    pub package: usize,
    pub kind: Kind,
    pub dir: PathBuf,
    pub default: bool,

    // What it compiles, and whether the layout convention chose it
    pub sources: Vec<PathBuf>,
    pub conventional_sources: bool,
    pub pch: Option<PathBuf>,
    pub unity: Option<usize>,
    pub file_rules: Vec<ResolvedFileRule>,
    pub runs: Vec<RunSpec>,
    pub pre_build: Vec<StepSpec>,
    pub post_build: Vec<StepSpec>,
    /// The export macro header: its path under {gen} and the macro's name.
    pub export_header: Option<(String, String)>,

    // Include folders: public, private, private with warnings silenced, public with warnings silenced
    pub include: Vec<PathBuf>,
    pub private_include: Vec<PathBuf>,
    pub system_include: Vec<PathBuf>,
    pub public_system_include: Vec<PathBuf>,

    // Flags
    pub defines: Vec<String>,
    pub public_defines: Vec<String>,
    pub interface_defines: Vec<String>,
    pub interface_include: Vec<PathBuf>,
    pub interface_flags: Vec<String>,
    pub flags: Vec<String>,
    pub public_flags: Vec<String>,

    // Linking: these all pass on to whatever links this target
    pub link_flags: Vec<String>,
    pub libs: Vec<String>,
    pub lib_dirs: Vec<PathBuf>,
    pub runtime_files: Vec<PathBuf>,

    // Dependencies: names as written (public?), then resolved target indices (public?)
    pub dep_names: Vec<(String, bool)>,
    pub deps: Vec<(usize, bool)>,

    // Compile-only dependencies: their include folders and definitions, never their files
    pub header_dep_names: Vec<String>,
    pub header_deps: Vec<usize>,

    // Build-order-only dependencies
    pub need_names: Vec<String>,
    pub needs: Vec<usize>,

    // Static libraries linked whole
    pub whole_archive_names: Vec<String>,
    pub whole_archive: Vec<usize>,

    // Code generation settings
    pub std: String,
    pub c_std: String,
    pub warnings: Warnings,
    pub werror: bool,
    pub arch: Option<Arch>,
    pub cuda_arch: Vec<String>,
    pub cuda_flags: Vec<String>,
    pub gui: Gui,
    pub exports: bool,
    pub link_depends: Vec<PathBuf>,

    // The output file: prefix, name and extension (None: the platform's)
    pub output_prefix: Option<String>,
    pub output_name: String,
    pub output_suffix: Option<String>,
    pub output_dir: Option<PathBuf>,

    // Test settings
    pub args: Vec<String>,
    pub workdir: Option<PathBuf>,
    pub timeout: Option<u64>,
    pub env: Vec<(String, String)>,
    pub will_fail: bool,
    pub after: Vec<String>,
    pub serial: bool,
    pub pass_regex: Option<String>,
    pub fail_regex: Option<String>,
    pub skip_exit_code: Option<i32>,
    pub skip_regex: Option<String>,
    pub expect: Vec<String>,
    pub reject: Vec<String>,
    pub exit_code: Option<i32>,

    /// For External targets: the vcpkg package (name, only these libs) to fill in after install.
    pub vcpkg: Option<(String, Option<Vec<String>>)>,
}

/// Per-file settings, with the patterns compiled.
#[derive(Debug, Clone)]
pub struct ResolvedFileRule {
    /// The patterns as written, for {hash:flags}.
    pub patterns: Vec<String>,
    pub matcher: GlobSet,
    pub defines: Vec<String>,
    pub flags: Vec<String>,
    pub warnings: Option<Warnings>,
    pub arch: Option<Arch>,
    pub opt: Option<Opt>,
}

/// What a target's paths and placeholders resolve against.
pub struct PackageScope<'a> {
    pub index: usize,
    pub name: &'a str,
    pub root: &'a Path,
    pub out_dir: &'a Path,
    pub out_root: &'a Path,
    pub std: &'a str,
    pub c_std: &'a str,
    pub version: &'a str,
    /// The vcpkg triplet's install folder, for {vcpkg}.
    pub vcpkg_dir: &'a Path,
    /// The compilers, for {cxx} and {cc}.
    pub cxx: &'a Path,
    pub cc: &'a Path,
    pub condition: &'a CondContext,
}

/// Every setting a target table can hold, accumulated over `[defaults]`, the table and its `when` blocks.
#[derive(Default)]
struct Accumulated {
    kind: Option<String>,

    // Sources: the target's own (None: the convention), those of holding when blocks, and every when block's
    sources: Option<Vec<String>>,
    when_sources: Vec<String>,
    claimed_sources: Vec<String>,

    // Own settings; None for include folders means the convention
    include: Option<Vec<String>>,
    system_include: Vec<String>,
    defines: Vec<String>,
    flags: Vec<String>,

    // What dependents get too, and only they
    public_include: Option<Vec<String>>,
    public_system_include: Vec<String>,
    public_defines: Vec<String>,
    public_flags: Vec<String>,
    interface_include: Vec<String>,
    interface_defines: Vec<String>,
    interface_flags: Vec<String>,

    link_flags: Vec<String>,
    libs: Vec<String>,
    lib_dirs: Vec<String>,
    deps: Vec<(String, bool)>,
    header_deps: Vec<String>,
    needs: Vec<String>,
    link_depends: Vec<String>,
    whole_archive: Vec<String>,
    pch: Option<String>,
    unity: Option<toml::Value>,
    warnings: Option<String>,
    werror: Option<bool>,
    arch: Option<String>,
    cuda_arch: Option<Vec<String>>,
    cuda_flags: Vec<String>,
    std: Option<String>,
    c_std: Option<String>,
    gui: Option<toml::Value>,
    exports: Option<bool>,
    export_header: Option<ExportHeaderSpec>,
    output_prefix: Option<String>,
    output_name: Option<String>,
    output_suffix: Option<String>,
    output_dir: Option<String>,
    file_rules: Vec<FileRule>,
    runs: Vec<RunSpec>,
    pre_build: Vec<StepSpec>,
    post_build: Vec<StepSpec>,
    args: Vec<String>,
    workdir: Option<String>,
    timeout: Option<u64>,
    env: Vec<(String, String)>,
    will_fail: Option<bool>,
    after: Vec<String>,
    serial: Option<bool>,
    pass_regex: Option<String>,
    fail_regex: Option<String>,
    skip_exit_code: Option<i32>,
    skip_regex: Option<String>,
    expect: Vec<String>,
    reject: Vec<String>,
    exit_code: Option<i32>,
}

/// Which table settings come from: some keys belong only to a target, `kind` also to its when blocks.
#[derive(Clone, Copy, PartialEq)]
enum Table {
    Target,
    When,
    Shared,
}

impl Kind {
    pub fn parse(text: &str) -> Result<Kind> {
        Ok(match text {
            "exe" | "executable" | "bin" | "app" => Kind::Exe,
            "test" => Kind::Test,
            "lib" | "static" | "staticlib" => Kind::Static,
            "shared" | "dll" | "dylib" | "so" => Kind::Shared,
            "object" => Kind::Object,
            "headers" | "header-only" | "interface" => Kind::Headers,
            "custom" => Kind::Custom,
            other => bail!("kind = \"{other}\": expected exe, lib, shared, object, headers, test or custom"),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Exe => "exe",
            Kind::Test => "test",
            Kind::Static => "lib",
            Kind::Shared => "shared",
            Kind::Object => "object",
            Kind::Headers => "headers",
            Kind::Custom => "custom",
            Kind::External => "external",
        }
    }

    /// Something dependents link or include.
    pub fn is_library(self) -> bool {
        matches!(self, Kind::Static | Kind::Shared | Kind::Object | Kind::Headers | Kind::External)
    }

    /// Produces a program.
    pub fn is_executable(self) -> bool {
        matches!(self, Kind::Exe | Kind::Test)
    }
}

impl Target {
    /// An empty target; callers fill in what they know.
    pub fn new(name: &str, package: usize, kind: Kind, dir: &Path) -> Target {
        Target {
            name: name.to_string(),
            package,
            kind,
            dir: dir.to_path_buf(),
            default: kind != Kind::Test,
            sources: Vec::new(),
            conventional_sources: false,
            pch: None,
            unity: None,
            file_rules: Vec::new(),
            runs: Vec::new(),
            pre_build: Vec::new(),
            post_build: Vec::new(),
            export_header: None,
            include: Vec::new(),
            private_include: Vec::new(),
            system_include: Vec::new(),
            public_system_include: Vec::new(),
            defines: Vec::new(),
            public_defines: Vec::new(),
            interface_defines: Vec::new(),
            interface_include: Vec::new(),
            interface_flags: Vec::new(),
            flags: Vec::new(),
            public_flags: Vec::new(),
            link_flags: Vec::new(),
            libs: Vec::new(),
            lib_dirs: Vec::new(),
            runtime_files: Vec::new(),
            dep_names: Vec::new(),
            deps: Vec::new(),
            header_dep_names: Vec::new(),
            header_deps: Vec::new(),
            need_names: Vec::new(),
            needs: Vec::new(),
            whole_archive_names: Vec::new(),
            whole_archive: Vec::new(),
            std: "c++20".into(),
            c_std: "c17".into(),
            warnings: Warnings::Default,
            werror: false,
            arch: None,
            cuda_arch: Vec::new(),
            cuda_flags: Vec::new(),
            gui: Gui::Off,
            exports: false,
            link_depends: Vec::new(),
            output_prefix: None,
            output_name: name.to_string(),
            output_suffix: None,
            output_dir: None,
            args: Vec::new(),
            workdir: None,
            timeout: None,
            env: Vec::new(),
            will_fail: false,
            after: Vec::new(),
            serial: false,
            pass_regex: None,
            fail_regex: None,
            skip_exit_code: None,
            skip_regex: None,
            expect: Vec::new(),
            reject: Vec::new(),
            exit_code: None,
            vcpkg: None,
        }
    }

    /// Resolves `[target.<name>]`; None when its `if` condition is false.
    pub fn from_spec(
        name: &str,
        spec: &TargetSpec,
        defaults: &TargetSpec,
        presets: &IndexMap<String, TargetSpec>,
        scope: &PackageScope,
    ) -> Result<Option<Target>> {
        let context = |error: anyhow::Error| error.context(format!("in [target.{name}]"));

        if let Some(condition) = &spec.cond {
            if !scope.condition.test(condition).map_err(context)? {
                return Ok(None);
            }
        }

        let dir = match &spec.dir {
            Some(dir) => util::absolute(scope.root, Path::new(&expand_path_text(dir, scope, name)?)),
            None => scope.root.to_path_buf(),
        };
        if !dir.is_dir() {
            return Err(context(anyhow::anyhow!("dir = \"{}\" does not exist", spec.dir.as_deref().unwrap_or(""))));
        }

        // Merge [defaults], the presets it names, then the target, each with its when blocks
        let mut accumulated = Accumulated::default();
        accumulated.apply(defaults, scope.condition, Table::Shared).map_err(context)?;
        for preset_name in spec.preset.iter().flat_map(|names| names.to_vec()) {
            let Some(preset) = presets.get(&preset_name) else {
                let hint = util::suggest(&preset_name, presets.keys().map(String::as_str))
                    .map(|known| format!(" (did you mean `{known}`?)"))
                    .unwrap_or_default();
                return Err(context(anyhow::anyhow!(
                    "preset = \"{preset_name}\": there is no [preset.{preset_name}]{hint}"
                )));
            };
            accumulated
                .apply(preset, scope.condition, Table::Shared)
                .map_err(|error| context(error.context(format!("in [preset.{preset_name}]"))))?;
        }
        accumulated.apply(spec, scope.condition, Table::Target).map_err(context)?;

        // The kind, which a when block may change (lib or shared by an option)
        let kind = Kind::parse(accumulated.kind.as_deref().unwrap_or("exe")).map_err(context)?;

        let mut target = Target::new(name, scope.index, kind, &dir);
        target.default = spec.default.unwrap_or(kind != Kind::Test);
        target.fill(accumulated, scope).map_err(context)?;

        Ok(Some(target))
    }

    /// Conventional targets for a package with no `[target]` tables: an executable when src/ has a main file,
    /// otherwise a library; one test per file in tests/ (or one test from all of tests/ when it has a main).
    pub fn conventional(scope: &PackageScope, defaults: &TargetSpec, all_deps: &[String]) -> Result<Vec<Target>> {
        let root = scope.root;
        let source_dir = root.join("src");
        let include_dir = root.join("include");
        let mut targets = Vec::new();

        // The main target
        let (sources, kind) = if source_dir.is_dir() {
            let sources = expand_patterns(root, &[format!("src/{SOURCE_GLOB}")], scope.out_root, false)?;
            let has_main = sources
                .iter()
                .any(|source| is_main_file(source) && source.parent() == Some(source_dir.as_path()));
            let kind = if has_main { Kind::Exe } else { Kind::Static };
            (sources, kind)
        } else {
            let sources = expand_patterns(root, &["*.{c,cc,cpp,cxx}".to_string()], scope.out_root, false)?;
            let kind = if sources.is_empty() { Kind::Headers } else { Kind::Exe };
            (sources, kind)
        };

        let has_main_target = !sources.is_empty() || include_dir.is_dir();
        if has_main_target {
            let mut accumulated = Accumulated::default();
            accumulated.apply(defaults, scope.condition, Table::Shared)?;
            accumulated
                .sources
                .get_or_insert_with(|| sources.iter().map(|source| slash(source)).collect());
            accumulated.deps.extend(all_deps.iter().map(|dep| (dep.clone(), true)));

            let mut target = Target::new(scope.name, scope.index, kind, root);
            target.fill(accumulated, scope)?;
            targets.push(target);
        }

        // Tests
        let library = targets
            .first()
            .filter(|target| target.kind.is_library())
            .map(|target| target.name.clone());
        targets.extend(Target::conventional_tests(scope, defaults, all_deps, library)?);

        Ok(targets)
    }

    fn conventional_tests(scope: &PackageScope, defaults: &TargetSpec, all_deps: &[String], library: Option<String>) -> Result<Vec<Target>> {
        let tests_dir = scope.root.join("tests");
        if !tests_dir.is_dir() {
            return Ok(Vec::new());
        }

        let files = expand_patterns(&tests_dir, &["*.{c,cc,cpp,cxx}".to_string()], scope.out_root, false)?;
        let single_program = files.iter().any(|file| is_main_file(file));

        // One test program from all of tests/, or one per file
        let groups: Vec<(String, Vec<PathBuf>)> = if single_program {
            let all = expand_patterns(&tests_dir, &[SOURCE_GLOB.to_string()], scope.out_root, false)?;
            vec![("tests".to_string(), all)]
        } else {
            files
                .iter()
                .map(|file| (file.file_stem().unwrap().to_string_lossy().into_owned(), vec![file.clone()]))
                .collect()
        };

        let mut tests = Vec::new();
        for (name, sources) in groups {
            let mut accumulated = Accumulated::default();
            accumulated.apply(defaults, scope.condition, Table::Shared)?;
            accumulated.sources = Some(sources.iter().map(|source| slash(source)).collect());
            accumulated.include.get_or_insert_with(Vec::new).push(slash(&tests_dir));
            accumulated.deps.extend(library.iter().chain(all_deps).map(|dep| (dep.clone(), true)));

            let mut target = Target::new(&name, scope.index, Kind::Test, scope.root);
            target.fill(accumulated, scope)?;
            tests.push(target);
        }

        Ok(tests)
    }

    /// Turns accumulated settings into the target's final fields.
    fn fill(&mut self, settings: Accumulated, scope: &PackageScope) -> Result<()> {
        let dir = self.dir.clone();
        let paths = |list: &[String]| -> Result<Vec<PathBuf>> {
            list.iter()
                .map(|text| Ok(util::absolute(&dir, Path::new(&expand_path_text(text, scope, &self.name)?))))
                .collect()
        };

        // Sources: given, or the layout convention for compiled kinds minus what when blocks claim; then those of
        // the when blocks that hold
        let compiles = !matches!(self.kind, Kind::Headers | Kind::Custom | Kind::External);
        self.conventional_sources = compiles && settings.sources.is_none();
        let expand = |patterns: &[String], check_literals: bool| -> Result<Vec<PathBuf>> {
            let expanded: Vec<String> = patterns
                .iter()
                .map(|pattern| expand_path_text(pattern, scope, &self.name))
                .collect::<Result<_>>()?;
            expand_patterns(&dir, &expanded, scope.out_root, check_literals)
        };

        self.sources = match &settings.sources {
            Some(patterns) => expand(patterns, true)?,
            None if compiles => {
                let convention = if dir.join("src").is_dir() {
                    format!("src/{SOURCE_GLOB}")
                } else {
                    "*.{c,cc,cpp,cxx}".to_string()
                };
                let claimed: Vec<String> = settings
                    .claimed_sources
                    .iter()
                    .filter(|pattern| !pattern.starts_with('!'))
                    .cloned()
                    .collect();
                let claimed = expand(&claimed, false)?;
                let mut sources = expand(&[convention], false)?;
                sources.retain(|source| !claimed.contains(source));
                sources
            }
            None => Vec::new(),
        };
        for source in expand(&settings.when_sources, true)? {
            if !self.sources.contains(&source) {
                self.sources.push(source);
            }
        }

        // Include folders: given, or src/ (the target's own) and include/ (public) when they exist
        let existing = |folder: &str| {
            if dir.join(folder).is_dir() {
                vec![folder.to_string()]
            } else {
                Vec::new()
            }
        };
        self.private_include = paths(&settings.include.unwrap_or_else(|| existing("src")))?;
        self.include = paths(&settings.public_include.unwrap_or_else(|| existing("include")))?;
        self.system_include = paths(&settings.system_include)?;
        self.public_system_include = paths(&settings.public_system_include)?;
        self.interface_include = paths(&settings.interface_include)?;

        // Flags and linking
        // Definitions and flags may name folders: {root}, {out}, {gen}
        let placeholders = |list: Vec<String>| -> Vec<String> { list.iter().map(|text| expand_placeholders(text, scope, &self.name)).collect() };
        self.defines = placeholders(settings.defines);
        self.public_defines = placeholders(settings.public_defines);
        self.interface_defines = placeholders(settings.interface_defines);
        self.flags = placeholders(settings.flags);
        self.public_flags = placeholders(settings.public_flags);
        self.interface_flags = placeholders(settings.interface_flags);

        self.link_flags = placeholders(settings.link_flags);
        self.libs = settings
            .libs
            .iter()
            .map(|lib| expand_lib(lib, &dir, scope, &self.name))
            .collect::<Result<_>>()?;
        self.lib_dirs = paths(&settings.lib_dirs)?;
        self.link_depends = paths(&settings.link_depends)?;
        self.cuda_arch = settings.cuda_arch.unwrap_or_default();
        self.cuda_flags = placeholders(settings.cuda_flags);
        self.dep_names = settings.deps;
        self.header_dep_names = settings.header_deps;
        self.need_names = settings.needs;

        // A library linked whole is linked: a dependency of its own if not one already
        for name in &settings.whole_archive {
            if !self.dep_names.iter().any(|(dep, _)| dep == name) {
                self.dep_names.push((name.clone(), false));
            }
        }
        self.whole_archive_names = settings.whole_archive;

        // Code generation
        self.std = normalize_standard(settings.std.as_deref().unwrap_or(scope.std), Language::Cxx)?;
        self.c_std = normalize_standard(settings.c_std.as_deref().unwrap_or(scope.c_std), Language::C)?;
        self.warnings = settings
            .warnings
            .as_deref()
            .map(Warnings::parse)
            .transpose()?
            .unwrap_or(Warnings::Default);
        self.werror = settings.werror.unwrap_or(false);
        self.arch = settings.arch.as_deref().map(Arch::parse).transpose()?;

        // Output
        self.gui = settings.gui.as_ref().map(Gui::parse).transpose()?.unwrap_or(Gui::Off);
        self.exports = settings.exports.unwrap_or(false);
        self.output_prefix = settings.output_prefix;
        self.output_name = settings.output_name.unwrap_or_else(|| self.name.clone());
        self.output_suffix = settings.output_suffix;
        self.output_dir = settings
            .output_dir
            .map(|folder| paths(&[folder]).map(|mut list| list.remove(0)))
            .transpose()?;
        self.export_header = settings.export_header.map(|spec| self.resolve_export_header(spec));

        // The export header is on the public include path, and exports while the target builds as a shared library
        if let Some((_, macro_name)) = &self.export_header {
            self.include.push(gen_dir(scope.out_dir, &self.name));
            if self.kind == Kind::Shared {
                let base = crate::export_header::base(macro_name);
                self.public_defines.push(format!("{base}_SHARED"));
                self.defines.push(format!("{base}_BUILDING"));
            }
        }

        // Unity batches and the precompiled header
        self.unity = settings.unity.as_ref().map(manifest::parse_unity).transpose()?.flatten();

        self.pch = settings.pch.map(|pch| paths(&[pch]).map(|mut list| list.remove(0))).transpose()?;
        if let Some(pch) = &self.pch {
            if !pch.is_file() {
                bail!("pch = \"{}\" does not exist", slash(pch));
            }
        }

        // Per-file rules and build steps
        self.file_rules = settings
            .file_rules
            .iter()
            .filter_map(|rule| resolve_file_rule(rule, &dir, scope, &self.name).transpose())
            .collect::<Result<_>>()?;
        self.runs = settings.runs;
        self.pre_build = settings.pre_build;
        self.post_build = settings.post_build;

        // Tests
        self.args = settings.args;
        self.workdir = settings
            .workdir
            .map(|workdir| paths(&[workdir]).map(|mut list| list.remove(0)))
            .transpose()?;
        self.timeout = settings.timeout;
        self.env = settings.env;
        self.will_fail = settings.will_fail.unwrap_or(false);
        self.after = settings.after;
        self.serial = settings.serial.unwrap_or(false);
        self.pass_regex = settings.pass_regex;
        self.fail_regex = settings.fail_regex;
        self.skip_exit_code = settings.skip_exit_code;
        self.skip_regex = settings.skip_regex;
        self.expect = settings.expect;
        self.reject = settings.reject;
        self.exit_code = settings.exit_code;

        Ok(())
    }

    /// The export header's path and macro: `<NAME>_EXPORT` unless the table names one.
    fn resolve_export_header(&self, spec: ExportHeaderSpec) -> (String, String) {
        match spec {
            ExportHeaderSpec::Path(path) => (path, default_export_macro(&self.name)),
            ExportHeaderSpec::Table { path, macro_name } => {
                let macro_name = macro_name.unwrap_or_else(|| default_export_macro(&self.name));
                (path, macro_name)
            }
        }
    }
}

pub fn default_export_macro(target_name: &str) -> String {
    format!("{}_EXPORT", sanitize_name(target_name).to_uppercase().replace(['-', '.'], "_"))
}

impl Accumulated {
    /// Adds one table's settings (lists append, single values replace), then its true `when` blocks, recursively.
    fn apply(&mut self, spec: &TargetSpec, condition: &CondContext, table: Table) -> Result<()> {
        let target_only = spec.dir.is_some() || spec.cond.is_some() || spec.default.is_some() || spec.preset.is_some();
        if table != Table::Target && target_only {
            bail!("dir, if, default and preset belong to a [target] table, not to [defaults], a preset or a when block");
        }
        if table == Table::Shared && spec.kind.is_some() {
            bail!("kind belongs to a [target] table or its when blocks, not to [defaults] or a preset");
        }

        // Sources: a when block's belong to its condition
        if table == Table::When {
            self.when_sources.extend(spec.sources.iter().flatten().cloned());
        } else {
            replace_or_extend(&mut self.sources, &spec.sources);
        }

        // Include folders replace the convention when given
        let public = spec.public.clone().unwrap_or_default();
        let interface = spec.interface.clone().unwrap_or_default();
        replace_or_extend(&mut self.include, &spec.include);
        replace_or_extend(&mut self.public_include, &public.include);

        // Lists that add up
        self.system_include.extend(spec.system_include.iter().flatten().cloned());
        self.defines.extend(spec.defines.iter().flatten().cloned());
        self.flags.extend(spec.flags.iter().flatten().cloned());
        self.public_system_include.extend(public.system_include.iter().flatten().cloned());
        self.public_defines.extend(public.defines.iter().flatten().cloned());
        self.public_flags.extend(public.flags.iter().flatten().cloned());
        self.interface_include.extend(interface.include.iter().flatten().cloned());
        self.interface_defines.extend(interface.defines.iter().flatten().cloned());
        self.interface_flags.extend(interface.flags.iter().flatten().cloned());
        self.link_flags.extend(spec.link_flags.iter().flatten().cloned());
        self.libs.extend(spec.libs.iter().flatten().cloned());
        self.lib_dirs.extend(spec.lib_dirs.iter().flatten().cloned());
        self.deps.extend(spec.deps.iter().flatten().map(|dep| (dep.clone(), false)));
        self.deps.extend(public.deps.iter().flatten().map(|dep| (dep.clone(), true)));
        self.header_deps.extend(spec.header_deps.iter().flatten().cloned());
        self.needs.extend(spec.needs.iter().flatten().cloned());
        self.link_depends.extend(spec.link_depends.iter().flatten().cloned());
        self.whole_archive.extend(spec.whole_archive.iter().flatten().cloned());
        self.cuda_flags.extend(spec.cuda_flags.iter().flatten().cloned());
        if let Some(cuda_arch) = &spec.cuda_arch {
            self.cuda_arch = Some(cuda_arch.to_vec());
        }
        self.file_rules.extend(spec.file.iter().cloned());
        self.runs.extend(spec.run.iter().cloned());
        self.pre_build.extend(spec.pre_build.iter().cloned());
        self.post_build.extend(spec.post_build.iter().cloned());
        self.args.extend(spec.args.iter().flatten().cloned());
        self.after.extend(spec.after.iter().flatten().cloned());
        self.env
            .extend(spec.env.iter().flatten().map(|(name, value)| (name.clone(), value.clone())));

        // Single values
        set_if_some(&mut self.kind, &spec.kind);
        set_if_some(&mut self.pch, &spec.pch);
        set_if_some(&mut self.unity, &spec.unity);
        set_if_some(&mut self.warnings, &spec.warnings);
        set_if_some(&mut self.werror, &spec.werror);
        set_if_some(&mut self.arch, &spec.arch);
        set_if_some(&mut self.std, &spec.std);
        set_if_some(&mut self.c_std, &spec.c_std);
        set_if_some(&mut self.gui, &spec.gui);
        set_if_some(&mut self.exports, &spec.exports);
        set_if_some(&mut self.export_header, &spec.export_header);
        set_if_some(&mut self.output_prefix, &spec.output_prefix);
        set_if_some(&mut self.output_name, &spec.output_name);
        set_if_some(&mut self.output_suffix, &spec.output_suffix);
        set_if_some(&mut self.output_dir, &spec.output_dir);
        set_if_some(&mut self.workdir, &spec.workdir);
        set_if_some(&mut self.timeout, &spec.timeout);
        set_if_some(&mut self.will_fail, &spec.will_fail);
        set_if_some(&mut self.serial, &spec.serial);
        set_if_some(&mut self.pass_regex, &spec.pass_regex);
        set_if_some(&mut self.fail_regex, &spec.fail_regex);
        set_if_some(&mut self.skip_exit_code, &spec.skip_exit_code);
        set_if_some(&mut self.skip_regex, &spec.skip_regex);
        self.expect.extend(spec.expect.iter().flatten().cloned());
        self.reject.extend(spec.reject.iter().flatten().cloned());
        set_if_some(&mut self.exit_code, &spec.exit_code);

        // Conditional blocks; the sources of every one, holding or not, leave the convention
        for (condition_text, block) in &spec.when {
            collect_sources(block, &mut self.claimed_sources);
            let holds = condition.test(condition_text).with_context(|| format!("in when.\"{condition_text}\""))?;
            if holds {
                self.apply(block, condition, Table::When)
                    .with_context(|| format!("in when.\"{condition_text}\""))?;
            }
        }

        Ok(())
    }
}

/// The source patterns of a when block and those inside it.
fn collect_sources(spec: &TargetSpec, patterns: &mut Vec<String>) {
    patterns.extend(spec.sources.iter().flatten().cloned());
    for block in spec.when.values() {
        collect_sources(block, patterns);
    }
}

/// Expands globs (`*`, `**`, `{a,b}`) relative to `dir`; `!pattern` removes matches. A plain path must exist unless
/// it lies in the output folder (generated). Results are sorted and without duplicates.
pub fn expand_patterns(dir: &Path, patterns: &[String], out_root: &Path, check_literals: bool) -> Result<Vec<PathBuf>> {
    let mut included: Vec<PathBuf> = Vec::new();
    let mut excludes = GlobSetBuilder::new();
    let mut has_excludes = false;

    for pattern in patterns {
        if let Some(excluded) = pattern.strip_prefix('!') {
            let absolute_pattern = slash(&util::absolute(dir, Path::new(excluded)));
            excludes.add(compile_glob(&absolute_pattern).with_context(|| format!("bad pattern \"{pattern}\""))?);
            has_excludes = true;
            continue;
        }

        // A plain file
        if !pattern.contains(['*', '?', '[', '{']) {
            let file = util::absolute(dir, Path::new(pattern));
            let generated = util::strip_prefix_ci(&file, out_root).is_some();
            if check_literals && !generated && !file.exists() {
                bail!("file not found: {}", slash(&file));
            }
            included.push(file);
            continue;
        }

        let found = walk_glob(dir, pattern, out_root)?;
        if found.is_empty() {
            crate::ui::warn(&format!("\"{pattern}\" matches no files (in {})", slash(dir)));
        }
        included.extend(found);
    }

    // Remove exclusions, sort, drop duplicates
    if has_excludes {
        let excludes = excludes.build()?;
        included.retain(|file| !excludes.is_match(slash(file)));
    }
    included.sort_by_key(|file| util::path_key(file));
    included.dedup_by_key(|file| util::path_key(file));

    Ok(included)
}

/// Placeholders in manifest paths: {root} {out} {gen} {dir} {name} {profile}, then environment variables.
pub fn expand_path_text(text: &str, scope: &PackageScope, target_name: &str) -> Result<String> {
    let expanded = expand_placeholders(text, scope, target_name);
    util::expand_env(&expanded)
}

pub fn expand_placeholders(text: &str, scope: &PackageScope, target_name: &str) -> String {
    if !text.contains('{') {
        return text.to_string();
    }

    let profile = scope
        .out_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    text.replace("{root}", &slash(scope.root))
        .replace("{out}", &slash(scope.out_dir))
        .replace("{gen}", &slash(&gen_dir(scope.out_dir, target_name)))
        .replace("{name}", target_name)
        .replace("{profile}", &profile)
        .replace(
            "{config}",
            &config_name(scope.condition.profiles.first().map(String::as_str).unwrap_or_default()),
        )
        .replace("{version}", scope.version)
        .replace("{vcpkg}", &slash(scope.vcpkg_dir))
        .replace("{cxx}", &slash(scope.cxx))
        .replace("{cc}", &slash(scope.cc))
}

/// A profile's name as CMake writes configurations ({config}): Debug, Release, RelWithDebInfo, Shipping.
pub fn config_name(profile: &str) -> String {
    match profile {
        "relwithdebinfo" => "RelWithDebInfo".to_string(),
        "minsizerel" => "MinSizeRel".to_string(),
        _ => {
            let mut characters = profile.chars();
            characters
                .next()
                .map(|first| first.to_uppercase().chain(characters).collect())
                .unwrap_or_default()
        }
    }
}

/// Generated files of a target live here.
pub fn gen_dir(out_dir: &Path, target_name: &str) -> PathBuf {
    out_dir.join("gen").join(sanitize_name(target_name))
}

/// A target name usable as a folder name.
pub fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || "-_.".contains(character) {
                character
            } else {
                '_'
            }
        })
        .collect()
}

pub fn walk_glob(dir: &Path, pattern: &str, out_root: &Path) -> Result<Vec<PathBuf>> {
    // Walk from the pattern's literal folder prefix, matching the rest
    let absolute_pattern = util::absolute(dir, Path::new(pattern));
    let pattern_text = slash(&absolute_pattern);
    let literal_end = pattern_text.find(['*', '?', '[', '{']).unwrap_or(pattern_text.len());
    let base = match pattern_text[..literal_end].rfind('/') {
        Some(slash_index) => PathBuf::from(&pattern_text[..slash_index.max(1)]),
        None => dir.to_path_buf(),
    };

    let matcher = compile_glob(&pattern_text)
        .with_context(|| format!("bad pattern \"{pattern}\""))?
        .compile_matcher();
    let recursive = pattern_text[literal_end..].contains("**") || pattern_text[literal_end..].contains('/');
    let max_depth = if recursive { usize::MAX } else { 1 };

    let mut files = Vec::new();
    let walker = walkdir::WalkDir::new(&base)
        .max_depth(max_depth)
        .into_iter()
        .filter_entry(|entry| !is_skipped_dir(entry, out_root));
    for entry in walker.flatten() {
        if entry.file_type().is_file() && matcher.is_match(slash(entry.path())) {
            files.push(util::normalize(entry.path()));
        }
    }

    Ok(files)
}

/// Folders a source glob never descends into: hidden ones, Anvil's output, CMake build folders, package trees.
pub fn is_skipped_dir(entry: &walkdir::DirEntry, out_root: &Path) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_dir() {
        return false;
    }

    let name = entry.file_name().to_string_lossy();
    if name.starts_with('.') || name == "node_modules" || name == "vcpkg_installed" {
        return true;
    }

    util::path_key(entry.path()) == util::path_key(out_root) || entry.path().join("CMakeCache.txt").is_file()
}

fn is_main_file(path: &Path) -> bool {
    path.file_stem().is_some_and(|stem| stem == "main")
}

/// A lib entry: a name ("user32", "Cocoa.framework") stays; a path (has a slash or an extension) becomes absolute.
fn expand_lib(lib: &str, dir: &Path, scope: &PackageScope, target_name: &str) -> Result<String> {
    let expanded = expand_path_text(lib, scope, target_name)?;
    let separators = expanded.contains('/') || expanded.contains('\\');
    let is_path = separators || (Path::new(&expanded).extension().is_some() && !expanded.ends_with(".framework"));

    if is_path && !expanded.starts_with('-') {
        Ok(slash(&util::absolute(dir, Path::new(&expanded))))
    } else {
        Ok(expanded)
    }
}

fn resolve_file_rule(rule: &FileRule, dir: &Path, scope: &PackageScope, target_name: &str) -> Result<Option<ResolvedFileRule>> {
    if let Some(condition) = &rule.cond {
        if !scope.condition.test(condition)? {
            return Ok(None);
        }
    }

    // Patterns match absolute source paths
    let mut builder = GlobSetBuilder::new();
    for pattern in rule.patterns.to_vec() {
        let absolute_pattern = slash(&util::absolute(dir, Path::new(&pattern)));
        builder.add(compile_glob(&absolute_pattern).with_context(|| format!("bad file pattern \"{pattern}\""))?);
    }

    // Definitions and flags may name folders, as the target's do
    let placeholders =
        |list: &Option<Vec<String>>| -> Vec<String> { list.iter().flatten().map(|text| expand_placeholders(text, scope, target_name)).collect() };

    Ok(Some(ResolvedFileRule {
        patterns: rule.patterns.to_vec(),
        matcher: builder.build()?,
        defines: placeholders(&rule.defines),
        flags: placeholders(&rule.flags),
        warnings: rule.warnings.as_deref().map(Warnings::parse).transpose()?,
        arch: rule.arch.as_deref().map(Arch::parse).transpose()?,
        opt: rule.opt.as_ref().map(Opt::parse).transpose()?,
    }))
}

fn replace_or_extend(target: &mut Option<Vec<String>>, value: &Option<Vec<String>>) {
    if let Some(value) = value {
        target.get_or_insert_with(Vec::new).extend(value.iter().cloned());
    }
}

fn set_if_some<T: Clone>(target: &mut Option<T>, value: &Option<T>) {
    if value.is_some() {
        *target = value.clone();
    }
}

/// A glob where `*` stays within one folder, case-insensitive on Windows.
pub fn compile_glob(pattern: &str) -> std::result::Result<Glob, globset::Error> {
    GlobBuilder::new(pattern).literal_separator(true).case_insensitive(cfg!(windows)).build()
}
