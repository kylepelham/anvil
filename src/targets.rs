//! Resolves one target: `[defaults]`, the target's table and its `when` blocks merged; layout conventions filled in;
//! globs expanded; paths made absolute.

use crate::cond::CondContext;
use crate::flags::{normalize_standard, Arch, Gui, Language, Warnings};
use crate::manifest::{self, ExportHeaderSpec, FileRule, Manifest, RunSpec, StepSpec, TargetSpec};
use crate::profile::Opt;
use crate::util::{self, slash};
use anyhow::{bail, Context, Result};
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use indexmap::IndexMap;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

pub const SOURCE_GLOB: &str = "**/*.{c,cc,cpp,cxx,c++,cu}";

/// The folder of the output folder holding each target's generated files.
pub const GENERATED_FOLDER: &str = "generated";

// Characters that make a pattern a glob
const GLOB_CHARACTERS: &[char] = &['*', '?', '[', '{'];

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
    /// Flags dependents compile one language's files with (a vcpkg package's $<COMPILE_LANGUAGE:CXX> options).
    pub public_language_flags: Vec<(Language, String)>,

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
    /// DLLs loaded on the first call into them (programs and shared libraries; Windows only).
    pub delay_load: Vec<String>,
    /// `link-language`: C++ (the C++ runtime linked) or C, overriding what the compiled languages say.
    pub link_language: Option<Language>,

    // The output file: prefix, name and extension (None: the platform's)
    pub output_prefix: Option<String>,
    pub output_name: String,
    pub output_suffix: Option<String>,
    pub output_dir: Option<PathBuf>,
    /// A shared library's version and ABI version (libfoo.so.1.2.3 known as libfoo.so.1); ignored for other kinds.
    pub version: Option<String>,
    pub soversion: Option<String>,

    // Files copied beside the output (source, path under `dir`), and whether `anvil install` takes it (None: default)
    pub data: Vec<(PathBuf, PathBuf)>,
    pub install: Option<bool>,

    /// Qt's code generators run for it when it uses Qt (crate::qt).
    pub qt: bool,

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
    /// For External targets: the installed CMake package it is (`cmake = "..."`), for `anvil install`'s CMake files.
    pub cmake_package: Option<crate::vcpkg_tree::CmakePackage>,
    /// For External targets: it links static libraries that may hold C++ code, so its users link as C++.
    pub cxx_libraries: bool,
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

    // Own settings; None for include folders means the convention. Include folders from [defaults], presets and
    // when blocks add to the target's own (or the convention): those applied before the target's table go first
    include: Option<Vec<String>>,
    added_include: (Vec<String>, Vec<String>),
    system_include: Vec<String>,
    defines: Vec<String>,
    flags: Vec<String>,

    // What dependents get too, and only they
    public_include: Option<Vec<String>>,
    added_public_include: (Vec<String>, Vec<String>),
    own_table_applied: bool,
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
    delay_load: Vec<String>,
    /// `delay-load` from [defaults] and presets, which only targets that link a binary take.
    shared_delay_load: Vec<String>,
    link_language: Option<String>,
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
    version: Option<String>,
    soversion: Option<String>,
    data: Vec<String>,
    /// `data` from [defaults] and presets, which only targets that copy files beside their own take.
    shared_data: Vec<String>,
    install: Option<bool>,
    qt: Option<bool>,
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
    /// A when block of `[defaults]` or a preset.
    SharedWhen,
}

impl Table {
    /// The table of a when block inside this one.
    fn when(self) -> Table {
        match self {
            Table::Shared | Table::SharedWhen => Table::SharedWhen,
            _ => Table::When,
        }
    }
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
            public_language_flags: Vec::new(),
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
            c_std: "gnu17".into(),
            warnings: Warnings::Default,
            werror: false,
            arch: None,
            cuda_arch: Vec::new(),
            cuda_flags: Vec::new(),
            gui: Gui::Off,
            exports: false,
            link_depends: Vec::new(),
            delay_load: Vec::new(),
            link_language: None,
            output_prefix: None,
            output_name: name.to_string(),
            output_suffix: None,
            output_dir: None,
            version: None,
            soversion: None,
            data: Vec::new(),
            install: None,
            qt: true,
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
            cmake_package: None,
            cxx_libraries: false,
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
                let hint = util::did_you_mean(&preset_name, presets.keys().map(String::as_str));
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
    /// otherwise a library; one test per file in tests/ (or one test from all of tests/ when it has a main). Tests
    /// also use the [dev-deps].
    pub fn conventional(scope: &PackageScope, defaults: &TargetSpec, all_deps: &[String], dev_deps: &[String]) -> Result<Vec<Target>> {
        let root = scope.root;
        let source_dir = root.join("src");
        let include_dir = root.join("include");
        let mut targets = Vec::new();

        // The main target: a program when src/ has a main file, else a library; without src/, a program from the
        // files beside anvil.toml, else headers. Its sources are the convention's, as for any target
        let kind = if source_dir.is_dir() {
            match folder_has_source(&source_dir, true) {
                true => Kind::Exe,
                false => Kind::Static,
            }
        } else {
            match folder_has_source(root, false) {
                true => Kind::Exe,
                false => Kind::Headers,
            }
        };

        let mut accumulated = Accumulated::default();
        accumulated.apply(defaults, scope.condition, Table::Shared)?;
        accumulated.deps.extend(all_deps.iter().map(|dep| (dep.clone(), true)));

        let mut target = Target::new(scope.name, scope.index, kind, root);
        target.fill(accumulated, scope)?;
        if !target.sources.is_empty() || include_dir.is_dir() {
            targets.push(target);
        }

        // Tests; one named like the main target gets a `test_` prefix
        let main_name = targets.first().map(|target| target.name.clone());
        let library = targets
            .first()
            .filter(|target| target.kind.is_library())
            .map(|target| target.name.clone());
        let test_deps: Vec<String> = all_deps.iter().chain(dev_deps).cloned().collect();
        let mut tests = Target::conventional_tests(scope, defaults, &test_deps, library)?;
        for test in tests.iter_mut().filter(|test| Some(&test.name) == main_name.as_ref()) {
            test.name = format!("test_{}", test.name);
            test.output_name = test.name.clone();
        }
        targets.extend(tests);

        Ok(targets)
    }

    fn conventional_tests(scope: &PackageScope, defaults: &TargetSpec, all_deps: &[String], library: Option<String>) -> Result<Vec<Target>> {
        let tests_dir = scope.root.join("tests");
        if !tests_dir.is_dir() {
            return Ok(Vec::new());
        }

        let files = expand_pattern_list(&tests_dir, &["*.{c,cc,cpp,cxx}".to_string()], scope.out_root, false, false)?;
        let single_program = files.iter().any(|file| is_main_file(file));

        // One test program from all of tests/, or one per file
        let groups: Vec<(String, Vec<PathBuf>)> = if single_program {
            let all = expand_pattern_list(&tests_dir, &[SOURCE_GLOB.to_string()], scope.out_root, false, false)?;
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

        // Sources: given, or the layout convention for compiled kinds minus what when blocks claim (also when the
        // target lists only exclusions); then those of the when blocks that hold; then every `!` exclusion
        let compiles = !matches!(self.kind, Kind::Headers | Kind::Custom | Kind::External);
        let (own_patterns, mut excludes) = split_excludes(settings.sources.as_deref().unwrap_or_default());
        let (when_patterns, when_excludes) = split_excludes(&settings.when_sources);
        let only_excludes = own_patterns.is_empty() && !excludes.is_empty();
        excludes.extend(when_excludes);
        self.conventional_sources = compiles && (settings.sources.is_none() || only_excludes);

        let expand_text =
            |patterns: &[String]| -> Result<Vec<String>> { patterns.iter().map(|pattern| expand_path_text(pattern, scope, &self.name)).collect() };
        let expand = |patterns: &[String], given: bool| -> Result<Vec<PathBuf>> {
            let expanded = expand_text(patterns)?;
            if given {
                for pattern in &expanded {
                    let (base, glob) = split_pattern(&dir, pattern);
                    if glob.is_none() && base.is_dir() {
                        bail!("sources: \"{pattern}\" is a folder; list its files with a glob such as \"{pattern}/**/*.cpp\"");
                    }
                }
            }
            expand_pattern_list(&dir, &expanded, scope.out_root, given, given)
        };

        let mut sources = if self.conventional_sources {
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
            let claimed: HashSet<String> = expand(&claimed, false)?.iter().map(|source| util::path_key(source)).collect();
            let mut sources = expand(&[convention], false)?;
            if !claimed.is_empty() {
                sources.retain(|source| !claimed.contains(&util::path_key(source)));
            }
            sources
        } else {
            expand(&own_patterns, true)?
        };

        // A file both listed and found by a glob (spelled in another case on Windows) compiles once
        let when_sources = expand(&when_patterns, true)?;
        if !when_sources.is_empty() {
            let mut seen: HashSet<String> = sources.iter().map(|source| util::path_key(source)).collect();
            sources.extend(when_sources.into_iter().filter(|source| seen.insert(util::path_key(source))));
        }

        if !excludes.is_empty() {
            let mut builder = GlobSetBuilder::new();
            for pattern in expand_text(&excludes)? {
                builder.add(compile_glob(&absolute_glob(&dir, &pattern)).with_context(|| format!("bad pattern \"!{pattern}\""))?);
            }
            let excluded = builder.build()?;
            sources.retain(|source| !excluded.is_match(slash(source)));
        }
        self.sources = sources;

        // Include folders: given, or src/ (the target's own) and include/ (public) when they exist; with those
        // [defaults], presets and when blocks add
        let existing = |folder: &str| {
            if dir.join(folder).is_dir() {
                vec![folder.to_string()]
            } else {
                Vec::new()
            }
        };
        let combine = |(before, after): (Vec<String>, Vec<String>), own: Vec<String>| [before, own, after].concat();
        self.private_include = paths(&combine(settings.added_include, settings.include.unwrap_or_else(|| existing("src"))))?;
        self.include = paths(&combine(
            settings.added_public_include,
            settings.public_include.unwrap_or_else(|| existing("include")),
        ))?;
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

        // DLLs loaded on first call: only a program or shared library has imports to delay
        let links = matches!(self.kind, Kind::Exe | Kind::Test | Kind::Shared);
        if !settings.delay_load.is_empty() && !links {
            bail!(
                "delay-load: a {} target links no binary of its own; list the DLLs in the program or shared library that calls them",
                self.kind.name()
            );
        }
        if links {
            self.delay_load = [settings.shared_delay_load, settings.delay_load].concat();
            crate::delay_load::check_names(&self.delay_load)?;
        }
        self.link_language = settings.link_language.as_deref().map(parse_link_language).transpose()?;

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
        self.version = settings
            .version
            .map(|text| library_version(&text, "version", scope, &self.name))
            .transpose()?;
        self.soversion = settings
            .soversion
            .map(|text| library_version(&text, "soversion", scope, &self.name))
            .transpose()?;
        self.export_header = settings.export_header.map(|spec| self.resolve_export_header(spec));
        let mut data = settings.data;
        if copies_data(self.kind) {
            data.splice(0..0, settings.shared_data);
        }
        self.data = resolve_data(&data, &dir, scope, &self.name, self.kind)?;
        self.install = settings.install;
        self.qt = settings.qt.unwrap_or(true);

        // The export header is on the public include path, and exports while the target builds as a shared library
        if let Some((_, macro_name)) = &self.export_header {
            self.include
                .push(gen_dir(&package_out_dir(scope.out_dir, scope.index, scope.name), &self.name));
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
            if pch.is_dir() {
                bail!(
                    "pch = \"{}\" is a folder; pch names a header, such as \"src/pch.h\"",
                    util::rel(pch, &dir)
                );
            }
            if !pch.is_file() {
                bail!("pch = \"{}\" does not exist", util::rel(pch, &dir));
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

/// Checks every target, `[defaults]` and preset table of a package once, whether or not it applies here: each
/// condition's words, the values, which table a key belongs in, and the presets targets name.
pub fn check_package(manifest: &Manifest, condition: &CondContext) -> Result<()> {
    check_spec(&manifest.defaults, Table::Shared, condition).context("in [defaults]")?;
    for (name, preset) in &manifest.preset {
        check_spec(preset, Table::Shared, condition).with_context(|| format!("in [preset.{name}]"))?;
    }

    for (name, spec) in &manifest.target {
        let context = || format!("in [target.{name}]");
        if let Some(text) = &spec.cond {
            condition.check(text).with_context(context)?;
        }
        for preset_name in spec.preset.iter().flat_map(|names| names.to_vec()) {
            if !manifest.preset.contains_key(&preset_name) {
                let hint = util::did_you_mean(&preset_name, manifest.preset.keys().map(String::as_str));
                return Err(anyhow::anyhow!("preset = \"{preset_name}\": there is no [preset.{preset_name}]{hint}")).with_context(context);
            }
        }
        check_spec(spec, Table::Target, condition).with_context(context)?;

        // Test settings on a target that is never a test would be ignored
        if !can_be_test(spec) {
            if let Some(key) = test_key(spec) {
                return Err(anyhow::anyhow!(
                    "`{key}` is a test setting, and this target is not kind = \"test\"; for a test that runs a program, \
                     write a [[test]] table with command = [\"{{target:{name}}}\"]"
                ))
                .with_context(context);
            }
        }
    }

    for spec in &manifest.write {
        if let Some(text) = &spec.cond {
            condition.check(text).with_context(|| format!("in [[write]] {}", spec.path))?;
        }
    }
    // A test's condition may take matrix parameters (`{backend}`), filled in per test
    for spec in &manifest.test {
        if let Some(text) = spec.cond.as_ref().filter(|text| !text.contains('{')) {
            condition.check(text).with_context(|| format!("in [[test]] `{}`", spec.name))?;
        }
    }

    Ok(())
}

/// One table's keys and values, and those of its when blocks.
fn check_spec(spec: &TargetSpec, table: Table, condition: &CondContext) -> Result<()> {
    let target_only = spec.dir.is_some() || spec.cond.is_some() || spec.default.is_some() || spec.preset.is_some();
    if table != Table::Target && target_only {
        bail!("dir, if, default and preset belong to a [target] table, not to [defaults], a preset or a when block");
    }
    if matches!(table, Table::Shared | Table::SharedWhen) && spec.kind.is_some() {
        bail!("kind belongs to a [target] table or its when blocks, not to [defaults], a preset or their when blocks");
    }

    // Values
    if let Some(kind) = &spec.kind {
        Kind::parse(kind)?;
    }
    if let Some(warnings) = &spec.warnings {
        Warnings::parse(warnings)?;
    }
    if let Some(arch) = &spec.arch {
        Arch::parse(arch)?;
    }
    if let Some(std) = &spec.std {
        normalize_standard(std, Language::Cxx)?;
    }
    if let Some(c_std) = &spec.c_std {
        normalize_standard(c_std, Language::C)?;
    }
    if let Some(unity) = &spec.unity {
        manifest::parse_unity(unity)?;
    }
    if let Some(names) = &spec.delay_load {
        crate::delay_load::check_names(names)?;
    }
    if let Some(language) = &spec.link_language {
        parse_link_language(language)?;
    }

    // Per-file settings and build steps
    for rule in &spec.file {
        if let Some(text) = &rule.cond {
            condition.check(text)?;
        }
        rule.warnings.as_deref().map(Warnings::parse).transpose()?;
        rule.arch.as_deref().map(Arch::parse).transpose()?;
        rule.opt.as_ref().map(Opt::parse).transpose()?;
    }
    let step_conditions = spec
        .run
        .iter()
        .map(|run| &run.cond)
        .chain(spec.pre_build.iter().chain(&spec.post_build).map(|step| &step.cond));
    for text in step_conditions.flatten() {
        condition.check(text)?;
    }

    for (text, block) in &spec.when {
        let context = || format!("in when.\"{text}\"");
        condition.check(text).with_context(context)?;
        check_spec(block, table.when(), condition).with_context(context)?;
    }

    Ok(())
}

/// A target table, or one of its when blocks, makes it a test.
fn can_be_test(spec: &TargetSpec) -> bool {
    spec.kind.as_deref() == Some("test") || spec.when.values().any(can_be_test)
}

/// The first test-only key a table or its when blocks set.
fn test_key(spec: &TargetSpec) -> Option<&'static str> {
    let keys = [
        ("args", spec.args.is_some()),
        ("workdir", spec.workdir.is_some()),
        ("timeout", spec.timeout.is_some()),
        ("env", spec.env.is_some()),
        ("will-fail", spec.will_fail.is_some()),
        ("after", spec.after.is_some()),
        ("serial", spec.serial.is_some()),
        ("pass-regex", spec.pass_regex.is_some()),
        ("fail-regex", spec.fail_regex.is_some()),
        ("skip-exit-code", spec.skip_exit_code.is_some()),
        ("skip-regex", spec.skip_regex.is_some()),
        ("expect", spec.expect.is_some()),
        ("reject", spec.reject.is_some()),
        ("exit-code", spec.exit_code.is_some()),
    ];

    keys.iter()
        .find(|(_, given)| *given)
        .map(|(key, _)| *key)
        .or_else(|| spec.when.values().find_map(test_key))
}

impl Accumulated {
    /// Adds one table's settings (lists append, single values replace), then its true `when` blocks, recursively.
    fn apply(&mut self, spec: &TargetSpec, condition: &CondContext, table: Table) -> Result<()> {
        let target_only = spec.dir.is_some() || spec.cond.is_some() || spec.default.is_some() || spec.preset.is_some();
        if table != Table::Target && target_only {
            bail!("dir, if, default and preset belong to a [target] table, not to [defaults], a preset or a when block");
        }
        if matches!(table, Table::Shared | Table::SharedWhen) && spec.kind.is_some() {
            bail!("kind belongs to a [target] table or its when blocks, not to [defaults], a preset or their when blocks");
        }

        // Sources: a when block's belong to its condition
        if matches!(table, Table::When | Table::SharedWhen) {
            self.when_sources.extend(spec.sources.iter().flatten().cloned());
        } else {
            replace_or_extend(&mut self.sources, &spec.sources);
        }

        // Include folders: the target's own replace the convention; the rest add to it
        let public = spec.public.clone().unwrap_or_default();
        let interface = spec.interface.clone().unwrap_or_default();
        if table == Table::Target {
            replace_or_extend(&mut self.include, &spec.include);
            replace_or_extend(&mut self.public_include, &public.include);
            self.own_table_applied = true;
        } else {
            let (include, public_include) = match self.own_table_applied {
                false => (&mut self.added_include.0, &mut self.added_public_include.0),
                true => (&mut self.added_include.1, &mut self.added_public_include.1),
            };
            include.extend(spec.include.iter().flatten().cloned());
            public_include.extend(public.include.iter().flatten().cloned());
        }

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
        match table {
            Table::Shared | Table::SharedWhen => self.shared_data.extend(spec.data.iter().flatten().cloned()),
            _ => self.data.extend(spec.data.iter().flatten().cloned()),
        }
        match table {
            Table::Shared | Table::SharedWhen => self.shared_delay_load.extend(spec.delay_load.iter().flatten().cloned()),
            _ => self.delay_load.extend(spec.delay_load.iter().flatten().cloned()),
        }
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
        set_if_some(&mut self.version, &spec.version);
        set_if_some(&mut self.soversion, &spec.soversion);
        set_if_some(&mut self.link_language, &spec.link_language);
        set_if_some(&mut self.install, &spec.install);
        set_if_some(&mut self.qt, &spec.qt);
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
                self.apply(block, condition, table.when())
                    .with_context(|| format!("in when.\"{condition_text}\""))?;
            }
        }

        Ok(())
    }
}

/// A `version` or `soversion` with its placeholders ({version}): numbers and letters joined by dots ("1.2.3", "1").
fn library_version(text: &str, key: &str, scope: &PackageScope, target_name: &str) -> Result<String> {
    let version = expand_placeholders(text, scope, target_name);
    let valid = !version.is_empty()
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|character| character.is_ascii_alphanumeric()));
    if !valid {
        bail!("{key} = \"{text}\": a library version is numbers joined by dots, such as \"1.2.3\" (or \"{{version}}\", the project's)");
    }

    Ok(version)
}

/// `link-language`: "c++" (CMake's CXX) or "c".
fn parse_link_language(text: &str) -> Result<Language> {
    match text {
        "c++" => Ok(Language::Cxx),
        "c" => Ok(Language::C),
        other => bail!("link-language = \"{other}\": expected \"c++\" (link through the C++ compiler, with its runtime) or \"c\""),
    }
}

/// The source patterns of a when block and those inside it.
fn collect_sources(spec: &TargetSpec, patterns: &mut Vec<String>) {
    patterns.extend(spec.sources.iter().flatten().cloned());
    for block in spec.when.values() {
        collect_sources(block, patterns);
    }
}

/// Expands globs (`*`, `**`, `{a,b}`) relative to `dir`; `!pattern` removes matches (`!folder` everything in it). A
/// plain path must exist unless it lies in the output folder (generated). Results keep the list's order (a glob's
/// files sorted), without duplicates.
pub fn expand_patterns(dir: &Path, patterns: &[String], out_root: &Path, check_literals: bool) -> Result<Vec<PathBuf>> {
    expand_pattern_list(dir, patterns, out_root, check_literals, true)
}

/// `expand_patterns`, saying so when a glob matches nothing only when `warn_empty` (not for the layout convention).
fn expand_pattern_list(dir: &Path, patterns: &[String], out_root: &Path, check_literals: bool, warn_empty: bool) -> Result<Vec<PathBuf>> {
    let mut included: Vec<PathBuf> = Vec::new();
    let mut excludes = GlobSetBuilder::new();
    let mut has_excludes = false;

    for pattern in patterns {
        if let Some(excluded) = pattern.strip_prefix('!') {
            excludes.add(compile_glob(&absolute_glob(dir, excluded)).with_context(|| format!("bad pattern \"{pattern}\""))?);
            has_excludes = true;
            continue;
        }

        // A plain file
        let (file, glob) = split_pattern(dir, pattern);
        if glob.is_none() {
            let generated = util::strip_prefix_ci(&file, out_root).is_some();
            if check_literals && !generated && !file.exists() {
                bail!("file not found: {}", slash(&file));
            }
            included.push(file);
            continue;
        }

        // One glob's files sorted (the walk's order differs between systems)
        let mut found = walk_glob(dir, pattern, out_root)?;
        if found.is_empty() && warn_empty {
            crate::ui::warn(&format!("\"{pattern}\" matches no files (in {})", slash(dir)));
        }
        found.sort_by_cached_key(|file| util::path_key(file));
        included.extend(found);
    }

    // Remove exclusions and duplicates, keeping the list's order (it is the link order, which can matter as in CMake)
    if has_excludes {
        let excludes = excludes.build()?;
        included.retain(|file| !excludes.is_match(slash(file)));
    }
    let mut seen = HashSet::new();
    included.retain(|file| seen.insert(util::path_key(file)));

    Ok(included)
}

/// Splits `!` exclusions from a list of patterns: (patterns, exclusions without the `!`).
fn split_excludes(patterns: &[String]) -> (Vec<String>, Vec<String>) {
    let mut included = Vec::new();
    let mut excluded = Vec::new();

    for pattern in patterns {
        match pattern.strip_prefix('!') {
            Some(exclusion) => excluded.push(exclusion.to_string()),
            None => included.push(pattern.clone()),
        }
    }

    (included, excluded)
}

/// A pattern as an absolute path, split where the glob starts: (the folder or file before it, the glob from there).
/// A part with glob characters that names an existing folder (`C:/work/game[1]`) is taken as written.
pub fn split_pattern(dir: &Path, pattern: &str) -> (PathBuf, Option<String>) {
    let mut literal = PathBuf::new();
    let mut components = Path::new(pattern).components();

    while let Some(component) = components.next() {
        let text = component.as_os_str().to_string_lossy();
        let has_glob = matches!(component, Component::Normal(_)) && text.contains(GLOB_CHARACTERS);
        if has_glob && !util::absolute(dir, &literal.join(&*text)).is_dir() {
            let rest: Vec<String> = std::iter::once(text.into_owned())
                .chain(components.map(|component| component.as_os_str().to_string_lossy().into_owned()))
                .collect();
            return (util::absolute(dir, &literal), Some(rest.join("/")));
        }
        literal.push(component);
    }

    (util::absolute(dir, &literal), None)
}

/// An absolute glob for `pattern` that matches files by their full path: the folder part escaped so that `[` and
/// `{` in it match themselves; a plain folder means everything in it.
pub fn absolute_glob(dir: &Path, pattern: &str) -> String {
    let (base, glob) = split_pattern(dir, pattern);
    let escaped = escape_glob(slash(&base).trim_end_matches('/'));

    match glob {
        Some(glob) => format!("{escaped}/{glob}"),
        None if base.is_dir() => format!("{escaped}/**"),
        None => escaped,
    }
}

/// Text that a glob matches literally: each special character in a class of its own (`[` as `[[]`).
fn escape_glob(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());

    for character in text.chars() {
        if "*?[]{}".contains(character) {
            escaped.push('[');
            escaped.push(character);
            escaped.push(']');
        } else {
            escaped.push(character);
        }
    }

    escaped
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

    // The profile's own name, not the output folder's (`debug-gcc`, `host`)
    let profile = scope.condition.profiles.first().map(String::as_str).unwrap_or_default();
    let package_out = package_out_dir(scope.out_dir, scope.index, scope.name);
    text.replace("{root}", &slash(scope.root))
        .replace("{out}", &slash(scope.out_dir))
        .replace("{gen}", &slash(&gen_dir(&package_out, target_name)))
        .replace("{name}", target_name)
        .replace("{profile}", profile)
        .replace("{config}", &config_name(profile))
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

/// Generated files of a target live here (`out_dir` is its package's output folder: `package_out_dir`). Not `gen`:
/// that is a program's name (a code generator's) beside it off Windows.
pub fn gen_dir(out_dir: &Path, target_name: &str) -> PathBuf {
    out_dir.join(GENERATED_FOLDER).join(sanitize_name(target_name))
}

/// Where a package's objects, generated files and static libraries go: the output folder for the root package,
/// `packages/<name>` in it for the others, so targets of the same name in two packages stay apart (and a member's
/// program named like its package does not collide with the folder on Linux).
pub fn package_out_dir(out_dir: &Path, package_index: usize, package_name: &str) -> PathBuf {
    if package_index == 0 {
        out_dir.to_path_buf()
    } else {
        out_dir.join("packages").join(sanitize_name(package_name))
    }
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
    // The language server reuses a walk while only manifests are edited
    crate::lsp::session::walk("glob", &[dir, Path::new(pattern), out_root], || {
        walk_glob_uncached(dir, pattern, out_root)
    })
}

fn walk_glob_uncached(dir: &Path, pattern: &str, out_root: &Path) -> Result<Vec<PathBuf>> {
    // Walk from the pattern's literal folder prefix, matching the rest as deep as it reaches
    let (base, glob) = split_pattern(dir, pattern);
    let Some(glob) = glob else {
        return Ok(if base.is_file() { vec![base] } else { Vec::new() });
    };

    let matcher = compile_glob(&glob)
        .with_context(|| format!("bad pattern \"{pattern}\""))?
        .compile_matcher();
    let max_depth = if glob.contains("**") { usize::MAX } else { glob.split('/').count() };

    let mut files = Vec::new();
    let walker = walkdir::WalkDir::new(&base)
        .max_depth(max_depth)
        .into_iter()
        .filter_entry(|entry| !is_skipped_dir(entry, out_root));
    for entry in walker.flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(&base) else { continue };
        if matcher.is_match(slash(relative)) {
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

/// Whether a folder directly holds a source file the convention takes: a `main` one in src/ (`main_only`), else one
/// of the `*.{c,cc,cpp,cxx}` files beside anvil.toml.
fn folder_has_source(folder: &Path, main_only: bool) -> bool {
    let extensions: &[&str] = if main_only {
        &["c", "cc", "cpp", "cxx", "c++", "cu"]
    } else {
        &["c", "cc", "cpp", "cxx"]
    };
    let Ok(entries) = std::fs::read_dir(folder) else { return false };

    entries.flatten().any(|entry| {
        let path = entry.path();
        let extension = path.extension().map(|extension| extension.to_string_lossy().to_lowercase());
        let wanted = extension.is_some_and(|extension| extensions.contains(&extension.as_str()));
        wanted && (!main_only || is_main_file(&path)) && entry.file_type().is_ok_and(|kind| kind.is_file())
    })
}

/// Of the conventional targets, the test of tests/<main>.c becomes `test_<main>` (<main>: the main target's name),
/// which must not be another file's test too.
pub fn check_renamed_test(targets: &[Target], main_name: &str, root: &Path) -> Result<()> {
    let renamed = format!("test_{main_name}");
    let source = |test: &Target| test.sources.first().cloned().unwrap_or_default();
    let is_own = |test: &Target| source(test).file_stem().is_some_and(|stem| stem.to_string_lossy() == main_name);

    let named: Vec<&Target> = targets
        .iter()
        .filter(|target| target.kind == Kind::Test && target.name == renamed)
        .collect();
    let (Some(own), Some(other)) = (named.iter().find(|test| is_own(test)), named.iter().find(|test| !is_own(test))) else {
        return Ok(());
    };

    let extension = source(own)
        .extension()
        .map(|extension| extension.to_string_lossy().into_owned())
        .unwrap_or_default();
    let example = source(own).with_file_name(format!("{main_name}_tests.{extension}"));
    bail!(
        "{} and {} would both be the test `{renamed}`: a test named like the package's main target `{main_name}` gets a `test_` prefix. Rename one of the files (for example {} to {})",
        util::rel(&source(own), root),
        util::rel(&source(other), root),
        util::rel(&source(own), root),
        util::rel(&example, root)
    )
}

/// A lib entry: a name ("user32", "Cocoa.framework") stays; a path (has a slash or an extension) becomes absolute.
fn expand_lib(lib: &str, dir: &Path, scope: &PackageScope, target_name: &str) -> Result<String> {
    let expanded = expand_path_text(lib, scope, target_name)?;
    let separators = expanded.contains('/') || expanded.contains('\\');
    let is_path = separators || (Path::new(&expanded).extension().is_some() && !expanded.ends_with(".framework"));

    if is_path && !expanded.starts_with('-') {
        let path = util::absolute(dir, Path::new(&expanded));
        if (path.is_dir() || path.file_name().is_none()) && !expanded.ends_with(".framework") {
            bail!("libs = [\"{lib}\"] is a folder; `libs` names libraries (a file, or a name found in `lib-dirs`), and `lib-dirs` the folders");
        }
        Ok(slash(&path))
    } else {
        Ok(expanded)
    }
}

/// A target that copies data files beside its file: a program or a shared library.
fn copies_data(kind: Kind) -> bool {
    matches!(kind, Kind::Exe | Kind::Test | Kind::Shared)
}

/// `data` patterns: each file they match, with its path under `dir` (where it goes beside the target's file). A
/// folder stands for everything in it; a pattern that matches nothing is an error.
fn resolve_data(patterns: &[String], dir: &Path, scope: &PackageScope, target_name: &str, kind: Kind) -> Result<Vec<(PathBuf, PathBuf)>> {
    if patterns.is_empty() {
        return Ok(Vec::new());
    }
    if !copies_data(kind) {
        bail!("data: a {} target makes no program or shared library to copy files beside", kind.name());
    }
    if patterns.iter().all(|pattern| pattern.starts_with('!')) {
        bail!("data: only `!` patterns, which exclude files; name the files to copy too (data = [\"assets\", \"!assets/raw\"])");
    }

    // Each pattern's files, then the `!` exclusions
    let mut files = Vec::new();
    let mut excludes = GlobSetBuilder::new();
    for pattern in patterns {
        let pattern = expand_path_text(pattern, scope, target_name)?;
        if let Some(excluded) = pattern.strip_prefix('!') {
            excludes.add(compile_glob(&absolute_glob(dir, excluded)).with_context(|| format!("data: bad pattern \"{pattern}\""))?);
            continue;
        }

        let (base, glob) = split_pattern(dir, &pattern);
        let found = match glob {
            None if base.is_dir() => walk_glob(&base, "**", scope.out_root)?,
            _ => walk_glob(dir, &pattern, scope.out_root)?,
        };
        if found.is_empty() {
            bail!("data: \"{pattern}\" matches no files (in {})", util::rel(dir, scope.root));
        }
        files.extend(found);
    }
    let excludes = excludes.build()?;
    files.retain(|file| !excludes.is_match(slash(file)));

    // Each file once, at its path under the target's folder
    let mut seen = HashSet::new();
    let mut data = Vec::new();
    for file in files {
        if !seen.insert(util::path_key(&file)) {
            continue;
        }
        let Some(relative) = util::strip_prefix_ci(&file, dir) else {
            bail!(
                "data: {} is outside the target's folder ({}); data files keep their path under it, so give the target a `dir` that holds them",
                util::rel(&file, scope.root),
                util::rel(dir, scope.root)
            );
        };
        data.push((file, relative));
    }

    Ok(data)
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
        builder.add(compile_glob(&absolute_glob(dir, &pattern)).with_context(|| format!("bad file pattern \"{pattern}\""))?);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_in_folders_with_glob_characters() {
        let dir = std::env::temp_dir()
            .join(format!("anvil_targets_{}", std::process::id()))
            .join("game{x}[1]");
        std::fs::create_dir_all(dir.join("src/old")).unwrap();
        for file in ["src/a.cpp", "src/b.cpp", "src/old/c.cpp"] {
            std::fs::write(dir.join(file), "").unwrap();
        }
        let out_root = dir.join("out");
        let names = |patterns: &[&str]| -> Vec<String> {
            let patterns: Vec<String> = patterns.iter().map(|pattern| pattern.to_string()).collect();
            let found = expand_patterns(&dir, &patterns, &out_root, true).unwrap();
            found.iter().map(|file| util::rel(file, &dir)).collect()
        };

        // The folder's `{x}` and `[1]` match themselves; `!folder` takes out everything in it
        assert_eq!(names(&["src/**/*.cpp"]), ["src/a.cpp", "src/b.cpp", "src/old/c.cpp"]);
        assert_eq!(names(&["src/**/*.cpp", "!src/old"]), ["src/a.cpp", "src/b.cpp"]);
        assert_eq!(names(&["src/*.cpp", "!src/b.cpp"]), ["src/a.cpp"]);

        // Listed files keep the list's order (the link order); a glob's are sorted; a repeat keeps its first place
        assert_eq!(
            names(&["src/b.cpp", "src/old/c.cpp", "src/a.cpp"]),
            ["src/b.cpp", "src/old/c.cpp", "src/a.cpp"]
        );
        assert_eq!(
            names(&["src/old/c.cpp", "src/*.cpp", "src/a.cpp"]),
            ["src/old/c.cpp", "src/a.cpp", "src/b.cpp"]
        );

        // A file listed twice, in another case on Windows, once
        if cfg!(windows) {
            assert_eq!(names(&["src/*.cpp", "SRC/A.cpp"]).len(), 2);
        }

        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn exclusions_split_off() {
        let patterns = ["src/*.cpp".to_string(), "!src/win/**".to_string()];
        assert_eq!(split_excludes(&patterns), (vec!["src/*.cpp".to_string()], vec!["src/win/**".to_string()]));
        assert_eq!(escape_glob("C:/a[1]/{b}"), "C:/a[[]1[]]/[{]b[}]");
    }

    #[test]
    fn renamed_test_must_not_take_another_tests_name() {
        let root = Path::new("/project");
        let test = |name: &str, file: &str| {
            let mut target = Target::new(name, 0, Kind::Test, root);
            target.sources = vec![root.join(file)];
            target
        };
        let main = Target::new("foo", 0, Kind::Exe, root);

        // tests/foo.c is the test `test_foo`, beside tests/bar.c: fine; beside tests/test_foo.c: an error naming both
        let fine = [main.clone(), test("test_foo", "tests/foo.c"), test("bar", "tests/bar.c")];
        assert!(check_renamed_test(&fine, "foo", root).is_ok());
        let clash = [main, test("test_foo", "tests/test_foo.c"), test("test_foo", "tests/foo.c")];
        let error = check_renamed_test(&clash, "foo", root).unwrap_err().to_string();
        assert!(
            error.starts_with("tests/foo.c and tests/test_foo.c would both be the test `test_foo`"),
            "{error}"
        );
        assert!(error.contains("for example tests/foo.c to tests/foo_tests.c"), "{error}");
    }
}
