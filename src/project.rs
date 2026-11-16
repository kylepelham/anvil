//! The project model: finds the root manifest (or works with none), loads packages and workspace members, resolves
//! every dependency (local targets, vcpkg, pkg-config, path and git packages, prebuilt libraries) and installs vcpkg
//! packages, giving the planner a complete list of targets.

use crate::cond::CondContext;
use crate::lockfile::{Declared, LockFile, Refresh};
use crate::manifest::{self, DepSpec, Manifest, StrOrList, TestSpec};
use crate::platform::Platform;
use crate::profile::Profile;
use crate::settings::Settings;
use crate::targets::{self, Kind, PackageScope, Target};
use crate::toolchain::Toolchain;
use crate::util::{self, slash};
use crate::vcpkg::{self, Instance, Request};
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const MANIFEST_NAME: &str = "anvil.toml";

// Files of [[test]] tables, beside what they test
pub const TESTS_FILE_NAME: &str = "tests.toml";

// What a version cannot hold besides spaces: it fills file names and quoted definitions
const VERSION_REFUSED: &[char] = &['/', '\\', '"', ';'];

// The keys whose items name dependencies, for the unused-dependency warning
const DEP_LIST_KEYS: &[&str] = &["deps", "whole-archive", "header-deps", "needs"];

#[derive(Debug, Clone)]
pub struct LoadOptions {
    pub start_dir: PathBuf,
    pub profile: String,
    pub toolchain: Option<String>,
    pub options: Vec<(String, bool)>,

    /// The machine to build for (default: the profile's, else this one).
    pub target: Option<String>,
    /// Fail rather than change anvil.lock; what `anvil update` re-resolves.
    pub locked: bool,
    pub refresh: Refresh,

    /// Build for this machine into this folder: the tools a cross build runs.
    pub host_tools_dir: Option<PathBuf>,

    /// Only list the project: no vcpkg install, no git fetch, anvil.lock left as it is.
    pub list_only: bool,

    /// Targets the command line names (`anvil build dep/tool`): kept though no target of the project reaches them.
    pub named_targets: Vec<String>,
}

#[derive(Debug)]
pub struct Package {
    pub name: String,
    pub root: PathBuf,
    pub manifest: Manifest,

    /// The root package or a workspace member (built by default), not a dependency.
    pub member: bool,
}

impl Package {
    /// `[project] version`, for the {version} placeholder.
    pub fn version(&self) -> &str {
        self.manifest
            .project
            .as_ref()
            .and_then(|project| project.version.as_deref())
            .unwrap_or("0.0.0")
    }
}

#[derive(Debug, Clone)]
pub struct TestCase {
    pub name: String,
    pub command: Vec<String>,
    pub workdir: PathBuf,
    pub timeout: Option<u64>,
    pub env: Vec<(String, String)>,

    // Judging and scheduling
    pub will_fail: bool,
    pub after: Vec<String>,
    pub serial: bool,
    pub pass_regex: Option<regex::Regex>,
    pub fail_regex: Option<regex::Regex>,
    pub skip_exit_code: Option<i32>,
    pub skip_regex: Option<regex::Regex>,
    pub expect: Vec<regex::Regex>,
    pub reject: Vec<regex::Regex>,
    pub exit_code: Option<i32>,
}

pub struct Model {
    pub root: PathBuf,
    pub out_root: PathBuf,
    pub out_dir: PathBuf,

    pub packages: Vec<Package>,
    pub targets: Vec<Target>,
    pub tests: Vec<TestCase>,

    pub profile: Profile,
    pub toolchain: Toolchain,
    /// The compiler cache C and C++ compiles run through: `[project] launcher` as written, and the program, found
    /// only by builds that run compiles (crate::toolchain::find_launcher), so other commands work without it.
    pub launcher_setting: Option<String>,
    pub launcher: Option<PathBuf>,
    pub condition: CondContext,
    pub default_run: Option<String>,

    /// The vcpkg used, when the project has vcpkg dependencies.
    pub vcpkg: Option<Instance>,
    /// The CUDA toolkit, when the project has .cu files; else why it cannot be used, said when one is compiled.
    pub cuda: Option<crate::cuda::Cuda>,
    pub cuda_error: Option<String>,
    pub vcpkg_triplet: String,
    /// The triplet's install folder: {vcpkg}.
    pub vcpkg_dir: PathBuf,

    /// How it was loaded, to load the host tools' model of a cross build the same way.
    pub options: LoadOptions,

    // Loading state: externals by (package, dep name), vcpkg requests, path/git packages by folder and the [deps]
    // name that loaded them, the lock file
    externals: HashMap<(usize, String), usize>,
    vcpkg_requests: Vec<Request>,
    package_dirs: HashMap<String, usize>,
    loaded_as: HashMap<usize, String>,
    lock: Option<LockFile>,

    /// Non-test targets naming a [dev-deps] entry, checked once every target is linked.
    dev_dep_uses: Vec<(usize, String)>,
    /// The CMake package files read, each once however many dependencies need them.
    cmake_reads: crate::cmake_package::Reads,
}

impl Model {
    pub fn load(options: &LoadOptions) -> Result<Model> {
        let (root, manifest) = locate(&options.start_dir)?;
        let project = manifest.project.clone().unwrap_or_default();
        let mut profile = Profile::resolve(&options.profile, &manifest.profile)?;

        // The machine to build for: the command line's, the profile's, else this one (always this one for host tools)
        let platform = match options.target.as_deref().or(profile.target.as_deref()) {
            Some(target) if options.host_tools_dir.is_none() => Platform::parse(target)?,
            _ => Platform::host(),
        };

        // Toolchain: command line, then project, then the user's default, then the best installed
        let requested = options
            .toolchain
            .clone()
            .or(project.toolchain.clone())
            .or_else(|| Settings::load().toolchain);
        let toolchain = Toolchain::find(requested.as_deref(), &platform)?;
        toolchain.adapt_profile(&mut profile)?;

        // Output folders: a toolchain or a target machine picked on the command line gets its own
        let out_root = output_root(&root, &manifest)?;
        let mut out_folder = profile.name.clone();
        if options.toolchain.is_some() {
            out_folder.push_str(&format!("-{}", toolchain.family.name()));
        }
        if options.target.is_some() && platform.is_cross() {
            out_folder.push_str(&format!("-{}", platform.name()));
        }
        let out_dir = options.host_tools_dir.clone().unwrap_or_else(|| out_root.join(out_folder));

        // vcpkg's triplet is known before anything installs, for {vcpkg}
        let vcpkg_spec = manifest.vcpkg.clone().unwrap_or_default();
        let vcpkg_triplet = vcpkg::choose_triplet(&vcpkg_spec, &toolchain, profile.static_runtime)?;
        let vcpkg_dir = out_root.join("vcpkg").join("installed").join(&vcpkg_triplet).join(&vcpkg_triplet);

        // Options of the root package and the workspace members, with the command line's values; dependencies add
        // theirs as they load
        let members = workspace_members(&root, &manifest)?;
        let member_manifests: Vec<(PathBuf, Manifest)> = members
            .into_iter()
            .map(|member| Ok((member.clone(), manifest::load(&member.join(MANIFEST_NAME))?)))
            .collect::<Result<_>>()?;
        for (member, member_manifest) in &member_manifests {
            check_member_project(&root, member, member_manifest)?;
            if util::strip_prefix_ci(member, &out_root).is_some() {
                bail!(
                    "{}: [project] out = \"{}\" holds the workspace member {}; name a folder of its own for the build output, such as \"out\"",
                    slash(&root.join(MANIFEST_NAME)),
                    project.out.as_deref().unwrap_or("out"),
                    util::rel(member, &root)
                );
            }
        }
        let package_options = std::iter::once((root.as_path(), &manifest.options)).chain(
            member_manifests
                .iter()
                .map(|(member, member_manifest)| (member.as_path(), &member_manifest.options)),
        );
        let lock = LockFile::load(&root, options.locked, options.refresh.clone())?;
        let condition = CondContext {
            family: toolchain.family,
            platform: platform.clone(),
            profiles: profile.chain.clone(),
            all_profiles: manifest.profile.keys().cloned().collect(),
            options: resolve_options(package_options, &options.options)?,
        };

        let mut model = Model {
            root: root.clone(),
            out_root,
            out_dir,
            vcpkg_triplet,
            vcpkg_dir,
            packages: Vec::new(),
            targets: Vec::new(),
            tests: Vec::new(),
            profile,
            toolchain,
            launcher_setting: project.launcher.clone(),
            launcher: None,
            condition,
            default_run: project.default_run.clone(),
            vcpkg: None,
            cuda: None,
            cuda_error: None,
            options: options.clone(),
            externals: HashMap::new(),
            vcpkg_requests: Vec::new(),
            package_dirs: HashMap::new(),
            loaded_as: HashMap::new(),
            lock: Some(lock),
            dev_dep_uses: Vec::new(),
            cmake_reads: Default::default(),
        };

        // Root package and workspace members
        model.add_package(&root, manifest, true)?;
        for (member, member_manifest) in member_manifests {
            model.add_package(&member, member_manifest, true)?;
        }

        // Dependencies (this may load more packages); of other packages, only the targets ours reach stay
        model.link_reachable()?;
        model.check_option_overrides()?;

        model.check_cycles()?;
        model.check_dev_dep_uses()?;
        model.check_output_names()?;
        model.check_data_files()?;
        model.check_sources()?;
        if options.host_tools_dir.is_none() {
            model.warn_unused_deps();
        }
        model.find_cuda()?;
        let mut declared = model.declared_dependencies()?;
        if !options.list_only {
            model.install_vcpkg()?;
            declared.vcpkg |= model.vcpkg.is_some();
        }
        crate::plan::expand_hashes(&mut model)?;
        let lock = model.lock.take().unwrap();
        if !options.list_only {
            lock.finish(&declared)?;
        }
        model.write_files()?;
        if options.host_tools_dir.is_none() {
            model.warn_missing_include_folders();
        }

        Ok(model)
    }

    /// The same project built for this machine into `<out>/host`, for the programs a cross build's steps run.
    pub fn load_host_tools(&self) -> Result<Model> {
        let mut options = self.options.clone();
        options.host_tools_dir = Some(self.out_dir.join("host"));
        options.refresh = Refresh::Nothing;
        Model::load(&options)
    }

    /// A target by name ("app" or "package/app"), with a suggestion when there is none.
    pub fn find_target(&self, name: &str) -> Result<usize> {
        let found = match name.split_once('/') {
            Some((package, target)) => self
                .targets
                .iter()
                .position(|candidate| candidate.name == target && self.packages[candidate.package].name == package),
            None => self
                .targets
                .iter()
                .position(|candidate| candidate.name == name && candidate.kind != Kind::External),
        };

        if let Some(index) = found {
            return Ok(index);
        }

        let names: Vec<&str> = self
            .targets
            .iter()
            .filter(|target| target.kind != Kind::External)
            .map(|target| target.name.as_str())
            .collect();
        let hint = util::did_you_mean(name, names.iter().copied());
        bail!("no target named `{name}`{hint}. Targets: {}", names.join(", "))
    }

    /// The targets a plain `anvil build` builds: every default target of the root package and workspace members.
    pub fn default_targets(&self, include_tests: bool) -> Vec<usize> {
        (0..self.targets.len())
            .filter(|index| {
                let target = &self.targets[*index];
                let wanted = target.default || (include_tests && target.kind == Kind::Test);
                self.packages[target.package].member && target.kind != Kind::External && wanted
            })
            .collect()
    }

    /// Every target `roots` need, dependencies first.
    pub fn closure(&self, roots: &[usize]) -> Vec<usize> {
        let mut order = Vec::new();
        let mut visited = vec![false; self.targets.len()];

        fn visit(model: &Model, index: usize, visited: &mut Vec<bool>, order: &mut Vec<usize>) {
            if visited[index] {
                return;
            }
            visited[index] = true;

            for (dependency, _) in &model.targets[index].deps {
                visit(model, *dependency, visited, order);
            }
            for dependency in model.targets[index].header_deps.iter().chain(&model.targets[index].needs) {
                visit(model, *dependency, visited, order);
            }
            order.push(index);
        }

        for root in roots {
            visit(self, *root, &mut visited, &mut order);
        }

        order
    }

    /// Where a package's objects, generated files and static libraries go (see `targets::package_out_dir`).
    pub fn package_out_dir(&self, package: usize) -> PathBuf {
        targets::package_out_dir(&self.out_dir, package, &self.packages[package].name)
    }

    /// A test, or a test-support library: one that reaches [dev-deps] and only tests use. Not what the project
    /// ships, so `anvil install` leaves it out unless named. A library its own tests use still ships.
    pub fn only_for_tests(&self, index: usize) -> bool {
        if self.targets[index].kind == Kind::Test {
            return true;
        }

        self.reaches_dev_deps(index, &mut Vec::new()) && self.shipping_reason(index, &mut Vec::new()).is_none()
    }

    /// Whether a target, or a target it uses, names a [dev-deps] entry.
    fn reaches_dev_deps(&self, index: usize, visiting: &mut Vec<usize>) -> bool {
        if self.dev_dep_uses.iter().any(|(user, _)| *user == index) {
            return true;
        }

        visiting.push(index);
        let target = &self.targets[index];
        let used = target
            .deps
            .iter()
            .map(|(dependency, _)| *dependency)
            .chain(target.header_deps.iter().copied())
            .chain(target.needs.iter().copied());
        let used: Vec<usize> = used.collect();
        used.into_iter()
            .any(|dependency| !visiting.contains(&dependency) && self.reaches_dev_deps(dependency, visiting))
    }

    fn add_package(&mut self, root: &Path, manifest: Manifest, member: bool) -> Result<usize> {
        let name = package_name(root, &manifest);
        if let Some(other) = self.packages.iter().find(|package| package.name == name) {
            bail!(
                "two packages are named `{name}`: {} and {}, so `{name}/<target>` and `{{target:...}}` could not tell them apart. \
                 Give one another [project] name in its anvil.toml (a folder without one is named by its [deps] key: rename that)",
                slash(&other.root),
                slash(root)
            );
        }
        let index = self.packages.len();
        self.package_dirs.insert(package_key(root), index);

        if let Some(both) = manifest.dev_deps.keys().find(|dev_dep| manifest.deps.contains_key(*dev_dep)) {
            bail!("`{both}` is under both [deps] and [dev-deps] of package `{name}`; keep it in one (tests see [deps] too)");
        }

        // Options declared by members and dependencies join the root's, with the command line's values
        let declared = manifest::parse_options(&manifest.options).with_context(|| package_context(index, &name, root))?;
        let option_names: Vec<String> = declared.keys().cloned().collect();
        crate::cond::check_names(&self.condition.all_profiles, &option_names).with_context(|| package_context(index, &name, root))?;
        for (option, default) in declared {
            let given = self
                .options
                .options
                .iter()
                .rev()
                .find(|(given, _)| *given == option)
                .map(|(_, value)| *value);
            self.condition.options.entry(option).or_insert(given.unwrap_or(default));
        }

        // Every table once, whether or not it applies here
        let check = || -> Result<()> {
            if let Some(version) = manifest.project.as_ref().and_then(|project| project.version.as_deref()) {
                check_version(version)?;
            }
            targets::check_package(&manifest, &self.condition)?;
            Profile::check_all(&manifest.profile)?;
            check_deps(&manifest, &self.condition)
        };
        check().with_context(|| package_context(index, &name, root))?;

        // Members declare their own git dependencies (anvil.lock records whose a pin is)
        if member {
            self.loaded_as.insert(index, name.clone());
        }

        self.packages.push(Package {
            name,
            root: root.to_path_buf(),
            manifest,
            member,
        });
        self.create_targets(index)?;

        Ok(index)
    }

    fn create_targets(&mut self, package_index: usize) -> Result<()> {
        let package = &self.packages[package_index];
        let manifest = &package.manifest;
        let project = manifest.project.clone().unwrap_or_default();

        let scope = PackageScope {
            index: package_index,
            name: &package.name,
            root: &package.root,
            out_dir: &self.out_dir,
            out_root: &self.out_root,
            std: project.std.as_deref().unwrap_or("c++20"),
            c_std: project.c_std.as_deref().unwrap_or("gnu17"),
            version: package.version(),
            vcpkg_dir: &self.vcpkg_dir,
            cxx: &self.toolchain.cxx,
            cc: &self.toolchain.cc,
            condition: &self.condition,
        };

        // Explicit targets, or the layout conventions
        let mut created = Vec::new();
        if manifest.target.is_empty() {
            let enabled_deps = enabled_dep_names(&manifest.deps, &self.condition)?;
            let enabled_dev_deps = enabled_dep_names(&manifest.dev_deps, &self.condition)?;
            created = Target::conventional(&scope, &manifest.defaults, &enabled_deps, &enabled_dev_deps)?;
            if package.member {
                targets::check_renamed_test(&created, &package.name, &package.root)?;
            }
        } else {
            for (name, spec) in &manifest.target {
                if let Some(target) = Target::from_spec(name, spec, &manifest.defaults, &manifest.preset, &scope)? {
                    created.push(target);
                }
            }
            check_shared_convention(&created)?;
        }

        // Outside the root package, `{target:X}` naming the package's own X says so, so it never finds another's X
        if package_index != 0 {
            let own: Vec<String> = created.iter().map(|target| target.name.clone()).collect();
            for target in &mut created {
                qualify_target_references(target, &package.name, &own);
            }
        }

        // Dependencies' tests are not ours to build, and none of their targets build by default
        let member = package.member;
        for mut target in created {
            if !member {
                if target.kind == Kind::Test {
                    continue;
                }
                target.default = false;
            }
            self.targets.push(target);
        }

        Ok(())
    }

    /// Resolves the dependencies of the root package's and the members' targets, then of every target they reach in
    /// other packages (through dependencies, or `{target:X}` in build steps, link flags, tests and the command line).
    /// Targets of other packages nothing reaches are dropped unresolved, so a dependency's examples and tools cost
    /// nothing.
    fn link_reachable(&mut self) -> Result<()> {
        let mut reached = vec![false; self.targets.len()];
        let mut pending: std::collections::VecDeque<usize> = (0..self.targets.len())
            .filter(|index| self.packages[self.targets[*index].package].member)
            .collect();
        for (package, name) in self.test_and_command_line_references() {
            pending.extend(self.find_reference(package, &name));
        }

        while let Some(index) = pending.pop_front() {
            reached.resize(self.targets.len(), false);
            if reached[index] {
                continue;
            }
            reached[index] = true;
            self.link_target(index)?;

            let target = &self.targets[index];
            let package = target.package;
            pending.extend(target.deps.iter().map(|(dependency, _)| *dependency));
            pending.extend(target.header_deps.iter().chain(&target.needs).chain(&target.whole_archive).copied());

            let steps = target.pre_build.iter().chain(&target.post_build).map(|step| step.command.to_command());
            let mut words: Vec<String> = target.runs.iter().map(|run| run.command.to_command()).chain(steps).flatten().collect();
            words.extend(target.link_flags.iter().chain(&target.args).cloned());
            for name in crate::plan::target_references(&words) {
                pending.extend(self.find_reference(package, &name));
            }
        }

        reached.resize(self.targets.len(), false);
        self.drop_unreached(&reached);
        Ok(())
    }

    /// `{target:X}` in the members' [[test]] tables and tests.toml files, by the package that has them, and the
    /// targets the command line and [project] default-run name (the root package's). Only a member with path or git
    /// dependencies can name a target the members' targets do not reach, so only theirs are read.
    fn test_and_command_line_references(&self) -> Vec<(usize, String)> {
        let mut references: Vec<(usize, String)> = self.options.named_targets.iter().map(|name| (0, name.clone())).collect();
        references.extend(self.default_run.iter().map(|name| (0, name.clone())));

        for (index, package) in self.packages.iter().enumerate().filter(|(_, package)| package.member) {
            let manifest = &package.manifest;
            let has_source_deps = manifest
                .deps
                .iter()
                .chain(&manifest.dev_deps)
                .any(|(name, value)| manifest::parse_dep(name, value).is_ok_and(|spec| spec.path.is_some() || spec.git.is_some()));
            if !has_source_deps {
                continue;
            }

            // Errors in tests.toml files are said when the tests are collected
            let mut specs = manifest.test.clone();
            for file in tests_files(&package.root, &self.out_root) {
                specs.extend(manifest::load_tests(&file).unwrap_or_default());
            }
            for spec in specs {
                let enabled = spec.cond.as_ref().is_none_or(|condition| self.condition.test(condition).unwrap_or(true));
                if enabled {
                    references.extend(
                        crate::plan::target_references(&spec.command.to_command())
                            .into_iter()
                            .map(|name| (index, name)),
                    );
                }
            }
        }

        references
    }

    /// The target `{target:X}` (or a name on the command line) means, seen from `package`: its own X first, then any
    /// X; `dep/X` loads that dependency's package when nothing loaded it yet. None when there is no such target (the
    /// error comes where it is used).
    fn find_reference(&mut self, package: usize, name: &str) -> Option<usize> {
        if !name.contains('/') {
            let own = self
                .targets
                .iter()
                .position(|target| target.package == package && target.name == name && target.kind != Kind::External);
            return own.or_else(|| self.find_target(name).ok());
        }

        self.find_target(name).ok().or_else(|| self.resolve_dep(package, name).ok().flatten())
    }

    /// Keeps only the reached targets, renumbering what refers to them.
    fn drop_unreached(&mut self, reached: &[bool]) {
        if reached.iter().all(|reached| *reached) {
            return;
        }

        let mut new_index = vec![usize::MAX; reached.len()];
        let mut kept = Vec::new();
        for (index, target) in std::mem::take(&mut self.targets).into_iter().enumerate() {
            if reached[index] {
                new_index[index] = kept.len();
                kept.push(target);
            }
        }

        for target in &mut kept {
            for (dependency, _) in &mut target.deps {
                *dependency = new_index[*dependency];
            }
            for dependency in target.header_deps.iter_mut().chain(&mut target.needs).chain(&mut target.whole_archive) {
                *dependency = new_index[*dependency];
            }
        }
        self.externals.retain(|_, index| reached[*index]);
        for index in self.externals.values_mut() {
            *index = new_index[*index];
        }
        self.dev_dep_uses.retain(|(index, _)| reached[*index]);
        for (index, _) in &mut self.dev_dep_uses {
            *index = new_index[*index];
        }

        self.targets = kept;
    }

    /// Every `-o name=value` names an option some package declares: a loaded one, or a path dependency that is off
    /// here. While a git dependency is neither loaded nor in the cache (off, or listing), an unknown one may be its
    /// own: a warning then, not an error.
    fn check_option_overrides(&self) -> Result<()> {
        let unknown: Vec<&String> = self
            .options
            .options
            .iter()
            .map(|(name, _)| name)
            .filter(|name| !self.condition.options.contains_key(*name))
            .collect();
        if unknown.is_empty() {
            return Ok(());
        }

        let (declared, unread_git) = self.options_of_unloaded_dependencies();
        for name in unknown.into_iter().filter(|name| !declared.contains(*name)) {
            if let Some(git) = &unread_git {
                crate::ui::warn(&format!(
                    "-o {name}: no package loaded here declares the option `{name}`; it may be one of git dependency `{git}`, which is not fetched"
                ));
                continue;
            }

            let known = self.condition.options.keys().chain(&declared).map(|key| key.as_str());
            let hint = util::did_you_mean(name, known);
            bail!("unknown option `{name}`{hint}. Declare it under [options].");
        }

        Ok(())
    }

    /// The options path and git dependencies declare that are not loaded (their `if` is false), read from their
    /// anvil.toml and those of their own path and git dependencies; and a git dependency not in the cache, whose
    /// options cannot be known.
    fn options_of_unloaded_dependencies(&self) -> (HashSet<String>, Option<String>) {
        let mut declared = HashSet::new();
        let mut unread_git: Option<String> = None;
        let mut seen: HashSet<String> = self.package_dirs.keys().cloned().collect();
        let entries = |manifest: &Manifest| -> Vec<(String, toml::Value)> {
            manifest
                .deps
                .iter()
                .chain(&manifest.dev_deps)
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect()
        };
        let mut pending: Vec<(PathBuf, Vec<(String, toml::Value)>)> = self
            .packages
            .iter()
            .map(|package| (package.root.clone(), entries(&package.manifest)))
            .collect();

        while let Some((root, dependencies)) = pending.pop() {
            for (name, value) in &dependencies {
                let Ok(spec) = manifest::parse_dep(name, value) else { continue };
                let folder = match (&spec.path, &spec.git) {
                    (Some(path), _) => match util::expand_env(path) {
                        Ok(path) => util::absolute(&root, Path::new(&path)),
                        Err(_) => continue,
                    },
                    (None, Some(url)) => match cached_git_checkout(name, url, &spec, self.lock.as_ref().unwrap()) {
                        Some(folder) => folder,
                        None => {
                            unread_git.get_or_insert_with(|| name.clone());
                            continue;
                        }
                    },
                    (None, None) => continue,
                };
                if !seen.insert(package_key(&folder)) {
                    continue;
                }

                let Ok(dependency) = manifest::load(&folder.join(MANIFEST_NAME)) else {
                    continue;
                };
                declared.extend(
                    manifest::parse_options(&dependency.options)
                        .into_iter()
                        .flat_map(|options| options.into_keys()),
                );
                pending.push((folder, entries(&dependency)));
            }
        }

        (declared, unread_git)
    }

    /// A target's name for messages: `package/target` outside the root package.
    pub fn qualified_name(&self, index: usize) -> String {
        let target = &self.targets[index];
        match target.package {
            0 => target.name.clone(),
            package => format!("{}/{}", self.packages[package].name, target.name),
        }
    }

    /// Resolves the dependency names of one target.
    fn link_target(&mut self, index: usize) -> Result<()> {
        let dep_names = self.targets[index].dep_names.clone();
        let package = self.targets[index].package;

        let mut deps = Vec::new();
        for (name, public) in dep_names {
            self.note_dev_dep_use(index, &name);
            let resolved = self
                .resolve_dep_of(package, &name, Some(index))
                .with_context(|| format!("in target `{}`", self.qualified_name(index)))?;
            if let Some(dependency) = resolved {
                if dependency == index {
                    bail!("target `{name}` depends on itself");
                }
                deps.push((dependency, public));
            }
        }

        // Header-only dependencies: compiled against, never linked
        let mut header_deps = Vec::new();
        for name in self.targets[index].header_dep_names.clone() {
            self.note_dev_dep_use(index, &name);
            let resolved = self
                .resolve_dep_of(package, &name, Some(index))
                .with_context(|| format!("in target `{}` (header-deps)", self.qualified_name(index)))?;
            if let Some(dependency) = resolved.filter(|dependency| *dependency != index) {
                header_deps.push(dependency);
            }
        }

        // Build-order-only dependencies
        let mut needs = Vec::new();
        for name in self.targets[index].need_names.clone() {
            let resolved = self
                .resolve_dep(package, &name)
                .with_context(|| format!("in target `{}` (needs)", self.qualified_name(index)))?;
            if let Some(dependency) = resolved.filter(|dependency| *dependency != index) {
                needs.push(dependency);
            }
        }

        // Libraries linked whole
        let mut whole_archive = Vec::new();
        for name in self.targets[index].whole_archive_names.clone() {
            let resolved = self
                .resolve_dep_of(package, &name, Some(index))
                .with_context(|| format!("in target `{}` (whole-archive)", self.qualified_name(index)))?;
            if let Some(dependency) = resolved {
                if !matches!(
                    self.targets[dependency].kind,
                    crate::targets::Kind::Static | crate::targets::Kind::External
                ) {
                    bail!("target `{}`: whole-archive `{name}` is not a static library", self.targets[index].name);
                }
                whole_archive.push(dependency);
            }
        }

        self.targets[index].deps = deps;
        self.targets[index].header_deps = header_deps;
        self.targets[index].needs = needs;
        self.targets[index].whole_archive = whole_archive;
        Ok(())
    }

    /// Remembers a non-test target naming a [dev-deps] entry (or `entry/target`), for `check_dev_dep_uses`.
    fn note_dev_dep_use(&mut self, index: usize, name: &str) {
        let target = &self.targets[index];
        let manifest = &self.packages[target.package].manifest;
        let declared = name.split_once('/').map_or(name, |(package, _)| package);
        if target.kind == Kind::Test || !manifest.dev_deps.contains_key(declared) {
            return;
        }

        // A target of the same name comes first
        let local = self
            .targets
            .iter()
            .any(|other| other.package == target.package && other.name == name && other.kind != Kind::External);
        if !local {
            self.dev_dep_uses.push((index, name.to_string()));
        }
    }

    /// [dev-deps] are for tests only: a program, or a library something other than tests uses, would put one into
    /// what the project ships. A library only tests use (a test-support library) may use them.
    fn check_dev_dep_uses(&self) -> Result<()> {
        for (index, name) in &self.dev_dep_uses {
            let Some(reason) = self.shipping_reason(*index, &mut Vec::new()) else {
                continue;
            };
            let target = &self.targets[*index];
            bail!(
                "target `{}` ({}) uses `{name}`, which is under [dev-deps]: only tests (kind = \"test\") use those, and libraries that only tests use; {reason}. Move `{name}` to [deps] if `{}` needs it.",
                target.name,
                target.kind.name(),
                target.name
            );
        }

        Ok(())
    }

    /// Why a target may end up in what the project ships, or None when only tests use it (directly, or through
    /// libraries only tests use).
    fn shipping_reason(&self, index: usize, visiting: &mut Vec<usize>) -> Option<String> {
        let target = &self.targets[index];
        if target.kind == Kind::Test {
            return None;
        }
        if target.kind.is_executable() {
            return Some(format!("`{}` is a program", target.name));
        }
        if !target.kind.is_library() {
            return Some(format!("`{}` is a {} target", target.name, target.kind.name()));
        }

        // Every target using it, through deps, header-deps or needs
        let uses = |user: &Target| {
            user.deps.iter().any(|(dependency, _)| *dependency == index) || user.header_deps.contains(&index) || user.needs.contains(&index)
        };
        let users: Vec<usize> = (0..self.targets.len()).filter(|user| uses(&self.targets[*user])).collect();
        if users.is_empty() {
            return Some(format!("nothing uses `{}`, so it is built on its own", target.name));
        }

        visiting.push(index);
        for user in users {
            if !visiting.contains(&user) && self.shipping_reason(user, visiting).is_some() {
                let user = &self.targets[user];
                return Some(format!(
                    "`{}` is used by `{}` ({}), which is not a test",
                    target.name,
                    user.name,
                    user.kind.name()
                ));
            }
        }

        None
    }

    /// A dependency name to a target: a local target, a [deps] entry, a workspace member, or "package/target".
    /// None when the [deps] entry's `if` is false.
    fn resolve_dep(&mut self, package: usize, name: &str) -> Result<Option<usize>> {
        self.resolve_dep_of(package, name, None)
    }

    /// `resolve_dep` for the lists of target `asking`: its own name there is the [deps] entry of that name when there
    /// is one (a program `threads` with threads = "system" links the system's), not itself.
    fn resolve_dep_of(&mut self, package: usize, name: &str, asking: Option<usize>) -> Result<Option<usize>> {
        // package/target: a target of a [deps] entry's package (loading it), else of a loaded package by its name
        if let Some((package_name, target_name)) = name.split_once('/') {
            return match self.find_dep_package(package, package_name)? {
                None => Ok(None),
                Some(FoundPackage::Package(found)) => self.main_target(found, Some(target_name)).map(Some),
                Some(FoundPackage::Placeholder(target)) => Ok(Some(target)),
            };
        }

        // A target of the same package
        let manifest = &self.packages[package].manifest;
        let declared = manifest.deps.contains_key(name) || manifest.dev_deps.contains_key(name);
        let local = self
            .targets
            .iter()
            .position(|target| target.package == package && target.name == name && target.kind != Kind::External);
        if let Some(local) = local.filter(|local| !(declared && asking == Some(*local))) {
            return Ok(Some(local));
        }

        // A [deps] entry, or a [dev-deps] one (link_target lets only tests name those)
        let manifest = &self.packages[package].manifest;
        if let Some(value) = manifest.deps.get(name).or_else(|| manifest.dev_deps.get(name)).cloned() {
            let spec = manifest::parse_dep(name, &value)?;
            if let Some(condition) = &spec.cond {
                if !self.condition.test(condition)? {
                    return Ok(None);
                }
            }
            return self.resolve_dep_spec(package, name, &spec).map(Some);
        }

        // A workspace member by package name
        if let Some(member) = self.packages.iter().position(|candidate| candidate.member && candidate.name == name) {
            return self.main_target(member, None).map(Some);
        }

        // Unknown: suggest something close
        let mut known: Vec<String> = self
            .targets
            .iter()
            .filter(|target| target.package == package)
            .map(|target| target.name.clone())
            .collect();
        known.extend(self.packages[package].manifest.deps.keys().cloned());
        known.extend(self.packages[package].manifest.dev_deps.keys().cloned());
        let hint = util::did_you_mean(name, known.iter().map(|known| known.as_str()));
        bail!("unknown dependency `{name}`{hint}. Declare it under [deps] (e.g. `anvil add {name}`), or name one of this package's targets.")
    }

    /// The package `package/target` names: a path or git [deps] entry of that key (loaded now), a loaded package of
    /// that name, or a [deps] package whose [project] name it is (a placeholder when listing, and it may be a git
    /// dependency not fetched). None when the entry's `if` is false.
    fn find_dep_package(&mut self, package: usize, package_name: &str) -> Result<Option<FoundPackage>> {
        let manifest = &self.packages[package].manifest;
        let source_deps: Vec<(String, DepSpec)> = manifest
            .deps
            .iter()
            .chain(&manifest.dev_deps)
            .map(|(key, value)| Ok((key.clone(), manifest::parse_dep(key, value)?)))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|(_, spec)| spec.path.is_some() || spec.git.is_some())
            .collect();

        // By [deps] key
        if let Some((key, spec)) = source_deps.iter().find(|(key, _)| key == package_name) {
            if let Some(condition) = &spec.cond {
                if !self.condition.test(condition)? {
                    return Ok(None);
                }
            }
            return Ok(Some(match self.load_dep_package(package, key, spec)? {
                Some(loaded) => FoundPackage::Package(loaded),
                // Listing without the checkout: the placeholder stands for its targets
                None => FoundPackage::Placeholder(self.resolve_dep_spec(package, key, spec)?),
            }));
        }
        let manifest = &self.packages[package].manifest;
        if let Some(table) = [("[deps]", &manifest.deps), ("[dev-deps]", &manifest.dev_deps)]
            .iter()
            .find_map(|(table, entries)| entries.contains_key(package_name).then_some(*table))
        {
            bail!("`{package_name}/...`: {table} {package_name} is not a path or git package, so it has no targets to pick from");
        }

        // By package name: one loaded already, else one of this package's path dependencies, then its git ones in the
        // cache, then (building, not listing) its git ones fetched now
        if let Some(found) = self.packages.iter().position(|candidate| candidate.name == package_name) {
            return Ok(Some(FoundPackage::Package(found)));
        }
        let mut unfetched = false;
        for pass in 0..3 {
            for (key, spec) in &source_deps {
                let cached = || {
                    let url = spec.git.as_ref().unwrap();
                    cached_git_checkout(key, url, spec, self.lock.as_ref().unwrap()).is_some()
                };
                let in_pass = match pass {
                    0 => spec.path.is_some(),
                    1 => spec.path.is_none() && cached(),
                    _ => spec.path.is_none() && !cached(),
                };
                if !in_pass {
                    continue;
                }
                if let Some(condition) = &spec.cond {
                    if !self.condition.test(condition)? {
                        continue;
                    }
                }
                if pass == 2 && self.options.list_only {
                    unfetched = true;
                    continue;
                }

                let Some(loaded) = self.load_dep_package(package, key, spec)? else {
                    continue;
                };
                if self.packages[loaded].name == package_name {
                    return Ok(Some(FoundPackage::Package(loaded)));
                }
            }
        }

        // Listing: it may be a git dependency not fetched yet, which a placeholder stands for
        if unfetched {
            let key = (package, package_name.to_string());
            let placeholder = match self.externals.get(&key) {
                Some(existing) => *existing,
                None => self.add_placeholder(package, package_name),
            };
            self.externals.insert(key, placeholder);
            return Ok(Some(FoundPackage::Placeholder(placeholder)));
        }

        let hint = util::did_you_mean(package_name, source_deps.iter().map(|(key, _)| key.as_str()));
        bail!("unknown package `{package_name}`{hint}. Name a path or git entry of [deps] (or its package's [project] name)")
    }

    fn resolve_dep_spec(&mut self, package: usize, name: &str, spec: &DepSpec) -> Result<usize> {
        let key = match &spec.target {
            Some(target) => (package, format!("{name}/{target}")),
            None => (package, name.to_string()),
        };
        if let Some(existing) = self.externals.get(&key) {
            return Ok(*existing);
        }

        let resolved = if spec.path.is_some() || spec.git.is_some() {
            match self.load_dep_package(package, name, spec)? {
                Some(loaded) => self.main_target(loaded, spec.target.as_deref())?,
                // Listing names it without fetching it
                None => self.add_placeholder(package, name),
            }
        } else {
            self.add_external(package, name, spec)?
        };

        self.externals.insert(key, resolved);
        Ok(resolved)
    }

    /// A target standing for a git dependency listing does not fetch: it has its name and nothing else.
    fn add_placeholder(&mut self, package: usize, name: &str) -> usize {
        let mut placeholder = Target::new(name, package, Kind::External, &self.packages[package].root);
        placeholder.default = false;
        self.targets.push(placeholder);

        self.targets.len() - 1
    }

    /// The package of a path or git dependency, loaded once. None when only listing and the git checkout is not in
    /// the cache yet.
    fn load_dep_package(&mut self, package: usize, name: &str, spec: &DepSpec) -> Result<Option<usize>> {
        let package_root = self.packages[package].root.clone();
        let folder = match (&spec.path, &spec.git) {
            (Some(path), _) => util::absolute(&package_root, Path::new(&util::expand_env(path)?)),
            (None, Some(url)) if self.options.list_only => match cached_git_checkout(name, url, spec, self.lock.as_ref().unwrap()) {
                Some(folder) => folder,
                None => return Ok(None),
            },
            (None, Some(url)) => {
                let parent = self.loaded_as.get(&package).cloned();
                fetch_git(name, url, spec, parent.as_deref(), self.lock.as_mut().unwrap())?
            }
            (None, None) => bail!("dependency `{name}` is not a path or git dependency"),
        };

        self.load_source_folder(name, &folder, spec).map(Some)
    }

    /// vcpkg, pkg-config and prebuilt dependencies become External targets carrying their usage.
    fn add_external(&mut self, package: usize, name: &str, spec: &DepSpec) -> Result<usize> {
        let package_root = self.packages[package].root.clone();
        let mut target = Target::new(name, package, Kind::External, &package_root);
        target.default = false;

        // Prebuilt parts and overrides, with environment variables (`$NAME`, `{env:NAME}`; in definitions and flags only
        // the latter)
        let key_context = |key: &str| format!("dependency `{name}`, `{key}`");
        let expand = |key: &str, values: &Option<Vec<String>>| -> Result<Vec<PathBuf>> {
            let mut paths = Vec::new();
            for text in values.iter().flatten() {
                let expanded = util::expand_env_path(text, key == "prefix").with_context(|| key_context(key))?;
                paths.extend(expanded.iter().map(|path| util::absolute(&package_root, Path::new(path))));
            }
            Ok(paths)
        };
        let expand_words = |key: &str, values: &Option<Vec<String>>| -> Result<Vec<String>> {
            values
                .iter()
                .flatten()
                .map(|text| util::expand_env_placeholders(text).with_context(|| key_context(key)))
                .collect()
        };
        target.public_system_include = expand("include", &spec.include)?;
        target.lib_dirs = expand("lib-dirs", &spec.lib_dirs)?;
        target.runtime_files = expand("runtime", &spec.runtime)?;
        let folders = spec.runtime.iter().flatten().zip(&target.runtime_files);
        if let Some((written, _)) = folders.into_iter().find(|(_, file)| file.is_dir() || file.file_name().is_none()) {
            bail!("dependency `{name}`: runtime = [\"{written}\"] is a folder; `runtime` lists the files copied beside the programs, such as \"bin/{name}.dll\"");
        }
        target.public_defines = expand_words("defines", &spec.defines)?;
        target.public_flags = expand_words("flags", &spec.flags)?;
        target.link_flags = expand_words("link-flags", &spec.link_flags)?;

        // `openmp`, `threads`, `opengl` and `vulkan` = "system": what CMake's find modules give their users
        let build = crate::vcpkg_tree::BuildContext::new(&self.toolchain, &self.profile);
        let system_form =
            spec.libs.as_deref() == Some(&[name.to_string()]) && spec.vcpkg.is_none() && spec.pkg_config.is_none() && spec.cmake.is_none();
        let system = system_form.then(|| crate::vcpkg_tree::system_dependency(name, &build)).flatten();
        if name == "vulkan" && system.as_ref().is_some_and(|usage| usage.libs.is_empty()) {
            if let Some(sdk) = std::env::var_os("VULKAN_SDK").filter(|sdk| !sdk.is_empty()) {
                let sdk = PathBuf::from(sdk);
                let cpu = self.toolchain.platform.cpu;
                let library = match self.toolchain.platform.os {
                    crate::platform::Os::Windows => crate::vcpkg_tree::vulkan_sdk_library(&sdk, cpu),
                    crate::platform::Os::Macos => sdk.join("lib/libvulkan.dylib"),
                    crate::platform::Os::Linux => sdk.join("lib/libvulkan.so"),
                };
                bail!(
                    "dependency `vulkan`: the Vulkan SDK in VULKAN_SDK ({}) has no {}, the loader library for {} programs; \
                     install the SDK's {} libraries (a component of its installer), or build for another processor",
                    slash(&sdk),
                    slash(&library),
                    cpu.name(),
                    cpu.name()
                );
            }
            bail!(
                "dependency `vulkan`: the Vulkan SDK is not installed (VULKAN_SDK is not set); install it from https://vulkan.lunarg.com \
                 (its installer sets VULKAN_SDK), or describe it with `include` and `libs`"
            );
        }
        if let Some(usage) = system {
            target.public_system_include.extend(usage.includes);
            target.public_defines.extend(usage.defines);
            target.public_flags.extend(usage.flags);
            target.public_language_flags.extend(usage.language_flags);
            target.libs.extend(usage.libs.iter().map(|lib| slash(lib)));
            target.link_flags.extend(usage.link_flags);
            target.runtime_files.extend(usage.runtime);
        } else if name == "cuda" && system_form {
            // The CUDA toolkit's headers and libraries, its static runtime and the driver's (CMake's CUDA::cudart_static
            // and CUDA::cuda_driver)
            let cuda = crate::cuda::Cuda::find(&self.toolchain).context("dependency `cuda`")?;
            target.public_system_include.push(cuda.include_dir.clone());
            target.lib_dirs.push(cuda.lib_dir.clone());
            target.libs.extend(cuda.runtime_libs(&self.toolchain));
            target.libs.push("cuda".to_string());
            target.cxx_libraries = true;
            if self.toolchain.is_msvc_abi() && !self.profile.static_runtime {
                target.link_flags.push(self.toolchain.linker_option("/NODEFAULTLIB:LIBCMT"));
            }
        } else if spec.vcpkg.is_none() {
            for lib in spec.libs.iter().flatten() {
                let expanded = util::expand_env(lib).with_context(|| key_context("libs"))?;
                let is_path = expanded.contains('/') || expanded.contains('\\') || Path::new(&expanded).extension().is_some();
                let path = util::absolute(&package_root, Path::new(&expanded));
                if is_path && (path.is_dir() || path.file_name().is_none()) {
                    bail!(
                        "dependency `{name}`: libs = [\"{lib}\"] is a folder; `libs` names libraries (\"lib/{name}.lib\", or \"{name}\" found in `lib-dirs`), and `lib-dirs` the folders"
                    );
                }
                target.libs.push(if is_path { slash(&path) } else { expanded });
            }
        }

        // vcpkg: filled in after the install
        if let Some(port) = &spec.vcpkg {
            target.vcpkg = Some((port.clone(), spec.libs.clone()));
            self.request_vcpkg(port, spec);
        }

        // pkg-config: ask it now
        if let Some(module) = &spec.pkg_config {
            apply_pkg_config(&mut target, module, &self.toolchain.platform)?;
        }

        // An installed CMake package: found now, where find_package finds it
        if let Some(package) = &spec.cmake {
            let prefixes = expand("prefix", &spec.prefix.as_ref().map(StrOrList::to_vec))?;
            let request = crate::cmake_package::Request {
                dependency: name,
                package,
                components: spec.components.as_deref().unwrap_or_default(),
                version: spec.version.as_deref(),
                targets: spec.targets.as_deref(),
                prefixes: &prefixes,
            };
            // Its result kept in the output folder, once there is one (loading writes no output folder of its own)
            let cache_folder = self.out_dir.join(".anvil").join("cmake-packages");
            let cache = self.out_root.is_dir().then_some(cache_folder.as_path());
            let found = crate::cmake_package::find(&request, &self.toolchain, &self.profile, cache, &self.cmake_reads)?;
            let usage = found.usage;
            target.public_system_include.extend(usage.includes);
            target.public_defines.extend(usage.defines);
            target.public_flags.extend(usage.flags);
            target.public_language_flags.extend(usage.language_flags);
            target.libs.extend(usage.libs.iter().map(|lib| slash(lib)));
            target.link_flags.extend(usage.link_flags);
            target.runtime_files.extend(usage.runtime);
            target.cxx_libraries = usage.cxx_libraries;
            target.cmake_package = Some(found.package);
        }

        self.targets.push(target);
        Ok(self.targets.len() - 1)
    }

    fn request_vcpkg(&mut self, port: &str, spec: &DepSpec) {
        let default_features = spec.default_features.unwrap_or(true);

        match self.vcpkg_requests.iter_mut().find(|request| request.name == port) {
            Some(existing) => {
                for feature in &spec.features {
                    if !existing.features.contains(feature) {
                        existing.features.push(feature.clone());
                    }
                }
                existing.default_features |= default_features;
            }
            None => self.vcpkg_requests.push(Request {
                name: port.to_string(),
                features: spec.features.clone(),
                default_features,
            }),
        }
    }

    /// What the loaded packages declare (and the members' [dev-deps]), used or not, for what anvil.lock keeps.
    fn declared_dependencies(&self) -> Result<Declared> {
        let mut declared = Declared {
            loaded: self.loaded_as.values().cloned().collect(),
            ..Default::default()
        };

        for package in &self.packages {
            let dev_deps = package.member.then_some(&package.manifest.dev_deps).into_iter().flatten();
            for (name, value) in package.manifest.deps.iter().chain(dev_deps) {
                let spec = manifest::parse_dep(name, value)?;
                declared.names.push(name.clone());
                declared.vcpkg |= spec.vcpkg.is_some();
                if let Some(url) = &spec.git {
                    declared.git.push((name.clone(), url.clone(), git_reference(&spec)));
                }
                if spec.git.is_some() || spec.path.is_some() {
                    declared.packages.push(name.clone());
                }
            }
        }

        Ok(declared)
    }

    /// A source package from a path or git checkout: its anvil.toml, or a library made from its layout.
    fn load_source_folder(&mut self, name: &str, folder: &Path, spec: &DepSpec) -> Result<usize> {
        if !folder.is_dir() {
            bail!("dependency `{name}`: folder {} does not exist", slash(folder));
        }

        // Already loaded (shared by several packages, or a workspace member)
        if let Some(existing) = self.package_dirs.get(&package_key(folder)).copied() {
            return Ok(existing);
        }

        let manifest_file = folder.join(MANIFEST_NAME);
        if manifest_file.is_file() {
            let overrides = [
                ("include", spec.include.is_some()),
                ("sources", spec.sources.is_some()),
                ("defines", spec.defines.is_some()),
            ];
            if let Some((key, _)) = overrides.iter().find(|(_, given)| *given) {
                bail!(
                    "dependency `{name}`: `{key}` describes a folder without an anvil.toml, and {} has one; \
                     set it in that package's targets instead",
                    slash(folder)
                );
            }
            let package = self.add_package(folder, manifest::load(&manifest_file)?, false)?;
            self.loaded_as.insert(package, name.to_string());
            return Ok(package);
        }

        // No anvil.toml: a library from the given include/sources, or include/ and src/
        let has_overrides = spec.include.is_some() || spec.sources.is_some();
        if !has_overrides && folder.join("CMakeLists.txt").is_file() && self.options.host_tools_dir.is_none() {
            // A git checkout is shared by every project using it: converting it in place would write into the cache
            let shown = util::rel(folder, &std::env::current_dir().unwrap_or_default());
            let advice = match spec.git.is_some() {
                true => "give its [deps] entry `include` and `sources` keys, or `anvil convert` a fork of it and depend on that".to_string(),
                false => format!("`anvil convert {shown}` writes an {MANIFEST_NAME} that follows them"),
            };
            crate::ui::warn(&format!(
                "dependency `{name}`: {shown} has a CMakeLists.txt but no {MANIFEST_NAME}, so it builds by Anvil's layout (include/, src/), not by its CMake rules; {advice}"
            ));
        }
        let synthetic = Manifest {
            project: Some(manifest::ProjectSpec {
                name: Some(name.to_string()),
                ..Default::default()
            }),
            target: if has_overrides {
                synthetic_target(name, folder, spec)
            } else {
                Default::default()
            },

            // Without include/sources, the defines go to the targets the layout makes
            defaults: manifest::TargetSpec {
                public: (!has_overrides).then(|| manifest::PublicSpec {
                    defines: spec.defines.clone(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let package = self.add_package(folder, synthetic, false)?;
        self.loaded_as.insert(package, name.to_string());
        Ok(package)
    }

    /// The library a package offers to dependents: the named one, the one named like the package, or its only one.
    fn main_target(&self, package: usize, wanted: Option<&str>) -> Result<usize> {
        let package_name = &self.packages[package].name;
        let candidates: Vec<usize> = (0..self.targets.len())
            .filter(|index| self.targets[*index].package == package && self.targets[*index].kind != Kind::External)
            .collect();

        if let Some(wanted) = wanted {
            return candidates
                .iter()
                .copied()
                .find(|index| self.targets[*index].name == wanted)
                .with_context(|| format!("package `{package_name}` has no target `{wanted}`"));
        }

        let libraries: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|index| self.targets[*index].kind.is_library())
            .collect();
        if let Some(same_name) = libraries.iter().copied().find(|index| &self.targets[*index].name == package_name) {
            return Ok(same_name);
        }

        match libraries.as_slice() {
            [only] => Ok(*only),
            [] => bail!("package `{package_name}` has no library target to depend on"),
            _ => bail!("package `{package_name}` has several libraries; pick one with `target = \"...\"` in its [deps] entry or depend on \"{package_name}/<target>\""),
        }
    }

    fn check_cycles(&self) -> Result<()> {
        // 0 = unvisited, 1 = on the current path, 2 = done
        let mut state = vec![0u8; self.targets.len()];
        let mut path = Vec::new();

        fn visit(model: &Model, index: usize, state: &mut Vec<u8>, path: &mut Vec<usize>) -> Result<()> {
            match state[index] {
                2 => return Ok(()),
                1 => {
                    let start = path.iter().position(|entry| *entry == index).unwrap_or(0);
                    let cycle: Vec<&str> = path[start..]
                        .iter()
                        .chain(std::iter::once(&index))
                        .map(|entry| model.targets[*entry].name.as_str())
                        .collect();
                    bail!("dependency cycle: {}", cycle.join(" -> "));
                }
                _ => {}
            }

            state[index] = 1;
            path.push(index);
            let target = &model.targets[index];
            let dependencies = target
                .deps
                .iter()
                .map(|(dependency, _)| dependency)
                .chain(&target.header_deps)
                .chain(&target.needs);
            for dependency in dependencies {
                visit(model, *dependency, state, path)?;
            }
            path.pop();
            state[index] = 2;

            Ok(())
        }

        for index in 0..self.targets.len() {
            visit(self, index, &mut state, &mut path)?;
        }

        Ok(())
    }

    /// A target that compiles must have something to compile here: sources, a build step that makes them, or object
    /// libraries it takes in. Otherwise its sources are all under conditions that do not hold.
    fn check_sources(&self) -> Result<()> {
        for (index, target) in self.targets.iter().enumerate() {
            let compiles = matches!(target.kind, Kind::Exe | Kind::Test | Kind::Static | Kind::Shared | Kind::Object);
            let takes_objects = target.deps.iter().any(|(dependency, _)| self.targets[*dependency].kind == Kind::Object);
            if compiles && target.sources.is_empty() && target.runs.is_empty() && !takes_objects {
                bail!(
                    "target `{}` has nothing to compile for this platform: its sources are all under `when` blocks that do not \
                     hold, or its patterns match no files. Give it sources here, or an `if` so it exists only where it has some",
                    self.qualified_name(index)
                );
            }
        }

        Ok(())
    }

    /// Two targets must not write the same file, nor share an object or generated-files folder (or nest one in the
    /// other's), nor write a file where such a folder goes.
    fn check_output_names(&self) -> Result<()> {
        let mut files: HashMap<String, (usize, PathBuf)> = HashMap::new();
        let mut folders: Vec<(String, PathBuf, &str, usize)> = Vec::new();

        for (index, target) in self.targets.iter().enumerate() {
            if target.kind == Kind::External {
                continue;
            }

            let package_out = self.package_out_dir(target.package);
            for (folder, what) in [
                (package_out.join("obj").join(targets::sanitize_name(&target.name)), "object folder"),
                (targets::gen_dir(&package_out, &target.name), "generated-files folder"),
            ] {
                folders.push((format!("{}/", util::path_key(&folder)), folder, what, index));
            }

            let Some(file) = crate::plan::artifact_path(self, target) else {
                continue;
            };
            if let Some((other, _)) = files.insert(util::path_key(&file), (index, file.clone())) {
                bail!(
                    "targets `{}` and `{}` would both write {}; give one an `output-name` or an `output-dir`",
                    self.qualified_name(other),
                    self.qualified_name(index),
                    slash(&file)
                );
            }
        }

        // Sorted, a folder comes right before those inside it
        folders.sort_by(|left, right| left.0.cmp(&right.0));
        for pair in folders.windows(2) {
            let ((first, folder, what, first_index), (second, _, _, second_index)) = (&pair[0], &pair[1]);
            if second.starts_with(first.as_str()) {
                bail!(
                    "targets `{}` and `{}` would share the {what} {}; rename one of them",
                    self.qualified_name(*first_index),
                    self.qualified_name(*second_index),
                    slash(folder)
                );
            }
        }

        // A program without an extension named like a folder Anvil writes
        for (key, (index, file)) in &files {
            let inside = format!("{key}/");
            let after = folders.partition_point(|(folder_key, ..)| folder_key.as_str() < inside.as_str());
            if let Some((_, folder, what, other)) = folders.get(after).filter(|(folder_key, ..)| folder_key.starts_with(&inside)) {
                bail!(
                    "target `{}` would write {}, which the {what} of `{}` ({}) needs as a folder; rename one of them",
                    self.qualified_name(*index),
                    slash(file),
                    self.qualified_name(*other),
                    slash(folder)
                );
            }
        }

        Ok(())
    }

    /// `[deps]` and `[dev-deps]` entries nothing of a package with [target] tables names (its targets, presets,
    /// defaults, steps and tests): a warning, as one left over still costs an install or a fetch. Under the layout
    /// conventions every target uses every entry.
    fn warn_unused_deps(&self) {
        for package in self
            .packages
            .iter()
            .filter(|package| package.member && !package.manifest.target.is_empty())
        {
            let manifest = &package.manifest;
            let manifest_file = package.root.join(MANIFEST_NAME);

            // The words naming dependencies in the manifest, then in its tests.toml files if needed
            let mut words = Vec::new();
            collect_dep_words(&manifest_file, &mut words);
            let used = |words: &[String], name: &str| {
                let prefix = format!("{name}/");
                words.iter().any(|word| word == name || word.starts_with(&prefix))
            };
            let entries = || manifest.deps.keys().chain(manifest.dev_deps.keys());
            if entries().all(|name| used(&words, name)) {
                continue;
            }
            for file in tests_files(&package.root, &self.out_root) {
                collect_dep_words(&file, &mut words);
            }

            for (table, entries, user) in [("deps", &manifest.deps, "target"), ("dev-deps", &manifest.dev_deps, "test")] {
                for name in entries.keys().filter(|name| !used(&words, name)) {
                    crate::ui::warn(&format!(
                        "{}: `[{table}] {name}`: no {user} uses it; add it to a {user}'s `deps`, or remove it",
                        slash(&manifest_file)
                    ));
                }
            }
        }
    }

    /// Include folders of the root package and the members that do not exist: a warning, as the compiler skips them
    /// silently. Folders under the output folder ({gen}, {vcpkg}) are made by the build, and so may those of a
    /// package with build steps.
    fn warn_missing_include_folders(&self) {
        let mut missing: indexmap::IndexMap<String, (PathBuf, Vec<String>)> = indexmap::IndexMap::new();
        let with_steps: HashSet<usize> = self
            .targets
            .iter()
            .filter(|target| !target.runs.is_empty() || !target.pre_build.is_empty())
            .map(|target| target.package)
            .collect();

        for (index, target) in self.targets.iter().enumerate() {
            if !self.packages[target.package].member || with_steps.contains(&target.package) {
                continue;
            }
            let folders = target
                .include
                .iter()
                .chain(&target.private_include)
                .chain(&target.system_include)
                .chain(&target.public_system_include)
                .chain(&target.interface_include);
            for folder in folders {
                if folder.exists() || util::strip_prefix_ci(folder, &self.out_root).is_some() {
                    continue;
                }
                let users = &mut missing.entry(util::path_key(folder)).or_insert_with(|| (folder.clone(), Vec::new())).1;
                users.push(format!("`{}`", self.qualified_name(index)));
            }
        }

        for (folder, users) in missing.values() {
            crate::ui::warn(&format!(
                "include folder {} does not exist (included by {})",
                util::rel(folder, &self.root),
                users.join(", ")
            ));
        }
    }

    /// Two targets must not copy different data files to the same place.
    fn check_data_files(&self) -> Result<()> {
        let mut destinations: HashMap<String, (usize, &Path)> = HashMap::new();

        for (index, target) in self.targets.iter().enumerate() {
            let Some(folder) = crate::plan::artifact_path(self, target).and_then(|file| file.parent().map(Path::to_path_buf)) else {
                continue;
            };
            for (file, relative) in &target.data {
                let destination = folder.join(relative);
                let Some((other, other_file)) = destinations.insert(util::path_key(&destination), (index, file)) else {
                    continue;
                };
                if util::path_key(other_file) != util::path_key(file) {
                    bail!(
                        "targets `{}` and `{}` both copy a data file to {}: {} and {}; rename one, or give one target an `output-dir`",
                        self.qualified_name(other),
                        self.qualified_name(index),
                        util::rel(&destination, &self.root),
                        util::rel(other_file, &self.root),
                        util::rel(file, &self.root)
                    );
                }
            }
        }

        Ok(())
    }

    /// With .cu files: the CUDA toolkit, its headers left out of dependency tracking, and its runtime linked into
    /// whatever uses the targets holding them.
    fn find_cuda(&mut self) -> Result<()> {
        let has_cuda = |target: &Target| {
            target
                .sources
                .iter()
                .any(|source| source.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("cu")))
        };
        if !self.targets.iter().any(has_cuda) {
            return Ok(());
        }

        // Without a usable toolkit only building a .cu file fails, not every build of the project
        let cuda = match crate::cuda::Cuda::find(&self.toolchain) {
            Ok(cuda) => cuda,
            Err(error) => {
                self.cuda_error = Some(format!("{error:#}"));
                return Ok(());
            }
        };
        self.toolchain.system_dirs.extend(cuda.system_dirs());

        // The static runtime is built against the static C runtime: say it is not wanted with the DLL one
        let runtime_libs = cuda.runtime_libs(&self.toolchain);
        let dynamic_msvc_runtime = self.toolchain.is_msvc_abi() && !self.profile.static_runtime;
        for target in self.targets.iter_mut().filter(|target| has_cuda(target)) {
            target.public_system_include.push(cuda.include_dir.clone());
            target.lib_dirs.push(cuda.lib_dir.clone());
            target.libs.extend(runtime_libs.iter().cloned());
            if dynamic_msvc_runtime {
                target.link_flags.push(self.toolchain.linker_option("/NODEFAULTLIB:LIBCMT"));
            }
        }

        self.cuda = Some(cuda);
        Ok(())
    }

    fn install_vcpkg(&mut self) -> Result<()> {
        let spec = self.packages[0].manifest.vcpkg.clone().unwrap_or_default();
        let root = self.root.clone();
        let resolve = |text: &String| -> Result<PathBuf> { Ok(util::absolute(&root, Path::new(&util::expand_env(text)?))) };

        // The project's own vcpkg.json installs even when no [deps] entry is a vcpkg port
        let configured_manifest = spec.manifest.as_ref().map(resolve).transpose()?;
        let own_manifest = vcpkg::own_manifest(&self.root, configured_manifest.as_deref())?;
        if self.vcpkg_requests.is_empty() && own_manifest.is_none() {
            return Ok(());
        }

        // A generated manifest needs a baseline: under --locked, the lock's, before anything runs
        let lock = self.lock.as_ref().unwrap();
        let locked_baseline = lock.vcpkg_baseline();
        if own_manifest.is_none() && spec.baseline.is_none() && locked_baseline.is_none() {
            lock.check_can_pin("the vcpkg baseline")?;
        }
        let refresh_baseline = lock.refreshes("vcpkg");

        // The same requests in the same order whatever resolved them first, so the install stamp holds
        self.vcpkg_requests.sort_by(|left, right| left.name.cmp(&right.name));
        for request in &mut self.vcpkg_requests {
            request.features.sort();
        }

        // The instance (when nobody chose one, the one the last install used) and the install settings
        let root_override = spec.root.as_ref().map(resolve).transpose()?;
        let remembered = vcpkg::remembered_root(&self.out_root, &self.vcpkg_triplet);
        let mut instance = Instance::select_with(root_override.as_deref(), remembered.as_deref())?;
        let overlay_triplets: Vec<PathBuf> = spec.overlay_triplets.iter().map(resolve).collect::<Result<_>>()?;
        if spec.triplet.is_some() {
            vcpkg::check_triplet_runtime(
                &self.vcpkg_triplet,
                &self.toolchain,
                self.profile.static_runtime,
                &overlay_triplets,
                &instance.root,
            )?;
        }
        let config = vcpkg::InstallConfig {
            project_name: &self.packages[0].name,
            project_version: self.packages[0].version(),
            out_root: &self.out_root,
            triplet: self.vcpkg_triplet.clone(),
            manifest: own_manifest,
            baseline: spec.baseline.clone(),
            locked_baseline,
            refresh_baseline,
            overlay_ports: spec.overlay_ports.iter().map(resolve).collect::<Result<_>>()?,
            overlay_triplets,
        };

        crate::commands::mark_output(&self.out_root, None)?;
        let vcpkg::Installed {
            tree,
            own_manifest,
            baseline,
        } = instance.install(&self.vcpkg_requests, &config)?;
        self.lock.as_mut().unwrap().set_vcpkg(baseline, &config.triplet, tree.versions());
        let debug_libs = crate::vcpkg_tree::uses_debug_libraries(&self.toolchain, &self.profile);
        let build = crate::vcpkg_tree::BuildContext::new(&self.toolchain, &self.profile);

        // A package nothing links here (one used only on other platforms) may be missing
        let mut linked = vec![false; self.targets.len()];
        for target in &self.targets {
            for dependency in target
                .deps
                .iter()
                .map(|(dependency, _)| *dependency)
                .chain(target.header_deps.iter().copied())
            {
                linked[dependency] = true;
            }
        }

        // Each vcpkg dependency gets the headers, libraries and DLLs of its package and what that depends on
        for (index, target) in self.targets.iter_mut().enumerate() {
            let Some((port, only_libs)) = &target.vcpkg else { continue };
            if !tree.contains(port) {
                if !linked[index] {
                    continue;
                }
                if let Some(manifest) = &own_manifest {
                    bail!(
                        "vcpkg did not install `{port}` (used in anvil.toml) for {}: {} does not list it for this platform, and no package it lists needs it. Add \"{port}\" to its \"dependencies\", or run `anvil add {port}`, which does it for you.",
                        config.triplet,
                        slash(manifest)
                    );
                }
            }

            let usage = tree
                .usage(port, debug_libs, only_libs.as_deref(), &build)
                .with_context(|| format!("dependency `{}`", target.name))?;
            target.public_system_include.extend(usage.includes);
            target.public_defines.extend(usage.defines);
            target.public_flags.extend(usage.flags);
            target.public_language_flags.extend(usage.language_flags);
            target.libs.extend(usage.libs.iter().map(|lib| slash(lib)));
            target.link_flags.extend(usage.link_flags);
            target.runtime_files.extend(usage.runtime);
            target.cxx_libraries = usage.cxx_libraries;
        }

        self.vcpkg = Some(instance);
        Ok(())
    }

    /// [[write]] files of every package that builds something here, rewritten only when their text changed. Two
    /// packages must not write different text to one file, and none may write into a git checkout, which other
    /// projects share.
    fn write_files(&self) -> Result<()> {
        let mut used = vec![false; self.packages.len()];
        for target in &self.targets {
            used[target.package] = true;
        }
        let git_cache = util::user_data_dir().join("git");
        let mut written: HashMap<String, (usize, String)> = HashMap::new();

        let packages = self.packages.iter().enumerate().filter(|(index, package)| package.member || used[*index]);
        for (index, package) in packages {
            for spec in &package.manifest.write {
                if let Some(condition) = &spec.cond {
                    if !self.condition.test(condition).with_context(|| format!("[[write]] {}", spec.path))? {
                        continue;
                    }
                }

                let fill = |text: &str| fill_test_text(text, package, &self.out_dir, &self.vcpkg_dir, &self.profile.name);
                let path = util::absolute(&package.root, Path::new(&fill(&spec.path)));
                let text = fill(&spec.text);

                // A cross build's host tools write only into their own folder: the files elsewhere are the target's
                if self.options.host_tools_dir.is_some() && util::strip_prefix_ci(&path, &self.out_dir).is_none() {
                    continue;
                }

                if util::strip_prefix_ci(&path, &git_cache).is_some() {
                    bail!(
                        "{}: [[write]] path = \"{}\" would write {}, inside a git checkout Anvil's cache shares between projects; write under the build folder instead (path = \"{{out}}/...\")",
                        package_context(index, &package.name, &package.root),
                        spec.path,
                        slash(&path)
                    );
                }
                if let Some((other, other_text)) = written.insert(util::path_key(&path), (index, text.clone())) {
                    if other_text != text {
                        bail!(
                            "{} and {} both [[write]] {}, with different text; give one of them another path",
                            package_context(other, &self.packages[other].name, &self.packages[other].root),
                            package_context(index, &package.name, &package.root),
                            util::rel(&path, &self.root)
                        );
                    }
                }

                if crate::lsp::session::active() || std::fs::read_to_string(&path).ok().as_deref() == Some(text.as_str()) {
                    continue;
                }

                if let Some(folder) = path.parent() {
                    std::fs::create_dir_all(folder).with_context(|| format!("creating {}", slash(folder)))?;
                }
                util::write_atomic(&path, text.as_bytes())?;
            }
        }

        Ok(())
    }

    /// The tests: test targets, [[test]] tables, and the [[test]] tables of every tests.toml in a member package.
    pub fn collect_tests(&mut self) -> Result<()> {
        // Tests and targets that are declared but off (their `if` is false): `after` may name them
        let mut switched_off = Vec::new();
        let mut origins = Vec::new();

        // Test targets run their program
        for (index, target) in self.targets.iter().enumerate() {
            let package = &self.packages[target.package];
            if target.kind != Kind::Test || !package.member {
                continue;
            }

            let test = self.target_test(index)?;
            self.tests.push(test);
            origins.push(TestOrigin {
                package: target.package,
                place: format!("[target.{}] in {}", target.name, util::rel(&package.root.join(MANIFEST_NAME), &self.root)),
            });
        }
        for package in self.packages.iter().filter(|package| package.member) {
            let off = package
                .manifest
                .target
                .keys()
                .filter(|name| !self.targets.iter().any(|target| target.name == **name));
            switched_off.extend(off.cloned());
        }

        // [[test]] tables run any command
        for (package_index, package) in self.packages.iter().enumerate().filter(|(_, package)| package.member) {
            let manifest_file = package.root.join(MANIFEST_NAME);
            let mut specs: Vec<(TestSpec, &Path)> = package.manifest.test.iter().map(|spec| (spec.clone(), manifest_file.as_path())).collect();
            let files = tests_files(&package.root, &self.out_root);
            for file in &files {
                specs.extend(manifest::load_tests(file)?.into_iter().map(|spec| (spec, file.as_path())));
            }

            // Outside the root package, `{target:X}` naming the package's own X says so (as in its targets)
            let own: Vec<String> = match package_index {
                0 => Vec::new(),
                _ => self
                    .targets
                    .iter()
                    .filter(|target| target.package == package_index)
                    .map(|target| target.name.clone())
                    .collect(),
            };

            for (spec, file) in &specs {
                let context = || format!("in [[test]] `{}`", spec.name);
                if let Some(condition_text) = &spec.cond {
                    if !self.condition.test(condition_text).with_context(context)? {
                        switched_off.push(spec.name.clone());
                        continue;
                    }
                }

                let workdir = match &spec.workdir {
                    Some(workdir) => {
                        let filled = fill_test_text(workdir, package, &self.out_dir, &self.vcpkg_dir, &self.profile.name);
                        util::absolute(&package.root, Path::new(&util::expand_env(&filled).with_context(context)?))
                    }
                    None => package.root.clone(),
                };
                let mut test = test_case(spec, workdir, package, &self.out_dir, &self.vcpkg_dir, &self.profile.name).with_context(context)?;
                for word in &mut test.command {
                    *word = qualify_references(word, &package.name, &own);
                }
                self.tests.push(test);
                origins.push(TestOrigin {
                    package: package_index,
                    place: util::rel(file, &self.root),
                });
            }
        }

        let package_names: Vec<&str> = self.packages.iter().map(|package| package.name.as_str()).collect();
        check_tests(&mut self.tests, &origins, &package_names, &switched_off)
    }

    /// The test a `kind = "test"` target is: its program with its `args`, `env` and `workdir`.
    pub fn target_test(&self, index: usize) -> Result<TestCase> {
        let target = &self.targets[index];
        let package = &self.packages[target.package];

        let mut command = vec![format!("{{target:{}}}", self.qualified_name(index))];
        command.extend(target.args.iter().cloned());
        let spec = TestSpec {
            name: target.name.clone(),
            command: StrOrList::List(command),
            timeout: target.timeout,
            env: Some(target.env.iter().cloned().collect()),
            will_fail: Some(target.will_fail),
            after: Some(target.after.clone()),
            serial: Some(target.serial),
            pass_regex: target.pass_regex.clone(),
            fail_regex: target.fail_regex.clone(),
            skip_exit_code: target.skip_exit_code,
            skip_regex: target.skip_regex.clone(),
            expect: Some(target.expect.clone()),
            reject: Some(target.reject.clone()),
            exit_code: target.exit_code,
            ..Default::default()
        };

        // Its workdir was resolved with its other paths, relative to its folder
        let workdir = target.workdir.clone().unwrap_or_else(|| package.root.clone());
        test_case(&spec, workdir, package, &self.out_dir, &self.vcpkg_dir, &self.profile.name).with_context(|| format!("in [target.{}]", target.name))
    }
}

/// Finds the project root: the nearest anvil.toml (or the workspace that lists it). With none, a folder that
/// looks like a C++ project builds with no manifest at all.
/// The build output folder `[project] out` names (default `out`), refused where building and `anvil clean --all`
/// would write over or delete the project: its own folder or one above it, a package's folder.
pub fn output_root(root: &Path, manifest: &Manifest) -> Result<PathBuf> {
    let out = manifest.project.as_ref().and_then(|project| project.out.as_deref()).unwrap_or("out");
    let out_root = util::absolute(root, Path::new(out));
    let key = format!("{}: [project] out = \"{out}\"", slash(&root.join(MANIFEST_NAME)));
    let instead = "name a folder of its own for the build output, such as \"out\"";

    // The project's folder, or one holding it
    if util::strip_prefix_ci(root, &out_root).is_some() {
        let which = if util::path_key(root) == util::path_key(&out_root) {
            "the project's own folder"
        } else {
            "a folder holding the project"
        };
        bail!("{key} is {which}, which `anvil clean --all` would delete; {instead}");
    }

    // A package's folder (a folder holding anything else, `anvil clean --all` refuses to delete)
    if out_root.join(MANIFEST_NAME).is_file() {
        bail!("{key} holds a package ({}); {instead}", slash(&out_root.join(MANIFEST_NAME)));
    }

    Ok(out_root)
}

pub fn locate(start: &Path) -> Result<(PathBuf, Manifest)> {
    let start = util::absolute(&std::env::current_dir()?, start);

    let nearest = start
        .ancestors()
        .find(|folder| folder.join(MANIFEST_NAME).is_file())
        .map(|folder| folder.to_path_buf());
    let Some(nearest) = nearest else {
        return zero_config(&start);
    };

    // A workspace further up that includes this package is the real root; packages up there that are not workspaces
    // are not this project's business, and one that does not parse is skipped with a warning
    for ancestor in nearest.ancestors().skip(1) {
        let candidate = ancestor.join(MANIFEST_NAME);
        if !candidate.is_file() {
            continue;
        }

        let manifest = match manifest::load(&candidate) {
            Ok(manifest) => manifest,
            Err(error) => {
                crate::ui::warn(&format!(
                    "not checking whether {} is a workspace holding this package, as it does not load: {error:#}",
                    slash(&candidate)
                ));
                continue;
            }
        };
        if manifest.workspace.is_none() {
            continue;
        }
        // A broken workspace is said only when this package is, or could be, one of its members
        let members = match workspace_members(ancestor, &manifest) {
            Ok(members) => members,
            Err(_) if !could_be_member(ancestor, &manifest, &nearest) => continue,
            Err(error) => return Err(error.context(format!("in the workspace {}", slash(&candidate)))),
        };
        if members.iter().any(|member| util::path_key(member) == util::path_key(&nearest)) {
            return Ok((ancestor.to_path_buf(), manifest));
        }
    }

    let manifest = manifest::load(&nearest.join(MANIFEST_NAME))?;
    Ok((nearest, manifest))
}

/// Whether one of a workspace's member patterns names a package's folder, whether or not the workspace loads.
fn could_be_member(root: &Path, manifest: &Manifest, package: &Path) -> bool {
    let Some(workspace) = &manifest.workspace else { return false };

    workspace.members.iter().any(|pattern| match targets::split_pattern(root, pattern) {
        (base, None) => util::path_key(&base) == util::path_key(package),
        (base, Some(glob)) => util::strip_prefix_ci(package, &base)
            .is_some_and(|relative| targets::compile_glob(&glob).map_or(true, |glob| glob.compile_matcher().is_match(slash(&relative)))),
    })
}

fn zero_config(start: &Path) -> Result<(PathBuf, Manifest)> {
    let has_sources = start.join("src").is_dir()
        || std::fs::read_dir(start)
            .with_context(|| format!("reading {}", slash(start)))?
            .flatten()
            .any(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| util::is_source_ext(&extension.to_string_lossy()))
            });
    if has_sources {
        return Ok((start.to_path_buf(), Manifest::default()));
    }

    if start.join("CMakeLists.txt").is_file() {
        bail!("no anvil.toml here, but there is a CMakeLists.txt: run `anvil convert` to create one from it");
    }
    bail!(
        "no anvil.toml in {} or above, and no sources here. Run `anvil new <name>` or `anvil init`.",
        slash(start)
    )
}

/// A package folder's key: one for the folder however it is reached (a junction or link to it, another spelling).
fn package_key(folder: &Path) -> String {
    util::path_key(&std::fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf()))
}

pub fn package_name(root: &Path, manifest: &Manifest) -> String {
    let declared = manifest.project.as_ref().and_then(|project| project.name.clone());
    declared.unwrap_or_else(|| {
        root.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "app".into())
    })
}

/// What `package/target` names: a loaded package, or (listing, the git checkout not fetched) a placeholder target.
enum FoundPackage {
    Package(usize),
    Placeholder(usize),
}

/// Whether a version is one to four numbers separated by dots ("2.1.0"), as CMake and package files read it.
pub fn is_version(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    parts.len() <= 4
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|character| character.is_ascii_digit()))
}

/// `[project] version` is free-form ("1.4.0-beta.2"), but one word: it fills {version} in commands, file names
/// (`output-name`) and quoted definitions. Only `anvil install`'s CMake package needs numbers (`is_version`).
pub fn check_version(version: &str) -> Result<()> {
    if !version.is_empty() && !version.contains(|character: char| character.is_whitespace() || VERSION_REFUSED.contains(&character)) {
        return Ok(());
    }

    bail!("[project] version = \"{version}\": write a version without spaces, `/`, `\\`, `\"` or `;` (\"2.1.0\", \"1.4.0-beta.2\")");
}

/// Where a package's errors come from: its anvil.toml, and its name outside the root package.
fn package_context(index: usize, name: &str, root: &Path) -> String {
    let file = slash(&root.join(MANIFEST_NAME));
    match index {
        0 => file,
        _ => format!("package `{name}` ({file})"),
    }
}

/// A workspace member's `[project]` keys that only the root's manifest sets (the toolchain, compiler cache, output
/// folder, `anvil run`'s program and install's exports): an error, not silently ignored.
fn check_member_project(root: &Path, member: &Path, manifest: &Manifest) -> Result<()> {
    let Some(project) = &manifest.project else { return Ok(()) };
    let root_only = [
        ("toolchain", project.toolchain.is_some()),
        ("launcher", project.launcher.is_some()),
        ("out", project.out.is_some()),
        ("default-run", project.default_run.is_some()),
        ("export", project.export.is_some()),
    ];

    match root_only.iter().find(|(_, set)| *set) {
        Some((key, _)) => bail!(
            "{}: [project] {key} is read only from the workspace root, for every member: set it in {} instead",
            slash(&member.join(MANIFEST_NAME)),
            slash(&root.join(MANIFEST_NAME))
        ),
        None => Ok(()),
    }
}

pub fn workspace_members(root: &Path, manifest: &Manifest) -> Result<Vec<PathBuf>> {
    let Some(workspace) = &manifest.workspace else { return Ok(Vec::new()) };
    let out_root = util::absolute(
        root,
        Path::new(manifest.project.as_ref().and_then(|project| project.out.as_deref()).unwrap_or("out")),
    );
    let mut members = Vec::new();

    for pattern in &workspace.members {
        // A plain folder
        let (base, glob) = targets::split_pattern(root, pattern);
        let Some(glob) = glob else {
            if !base.join(MANIFEST_NAME).is_file() {
                bail!("workspace member \"{pattern}\" has no {MANIFEST_NAME}");
            }
            members.push(base);
            continue;
        };

        // A glob over folders: walk from its literal part only as deep as it reaches
        let matcher = targets::compile_glob(&glob)
            .with_context(|| format!("workspace member \"{pattern}\": bad pattern"))?
            .compile_matcher();
        let max_depth = if glob.contains("**") { usize::MAX } else { glob.split('/').count() };
        let walker = walkdir::WalkDir::new(&base)
            .min_depth(1)
            .max_depth(max_depth)
            .into_iter()
            .filter_entry(|entry| !targets::is_skipped_dir(entry, &out_root));

        let mut found = false;
        for entry in walker.flatten() {
            let Ok(relative) = entry.path().strip_prefix(&base) else { continue };
            if entry.file_type().is_dir() && matcher.is_match(slash(relative)) && entry.path().join(MANIFEST_NAME).is_file() {
                members.push(util::normalize(entry.path()));
                found = true;
            }
        }
        if !found {
            bail!("workspace member \"{pattern}\" matches no folder with an {MANIFEST_NAME}");
        }
    }

    // Each member once (`b` and `./b`), never the root itself
    let mut seen = std::collections::HashSet::new();
    members.retain(|member| util::path_key(member) != util::path_key(root) && seen.insert(util::path_key(member)));
    Ok(members)
}

/// [options] defaults of the root package and the members, with `-o name=value` overrides from the command line.
/// An override no package here declares may belong to a dependency (`check_option_overrides`).
fn resolve_options<'a>(
    package_options: impl Iterator<Item = (&'a Path, &'a indexmap::IndexMap<String, toml::Value>)>,
    overrides: &[(String, bool)],
) -> Result<indexmap::IndexMap<String, bool>> {
    let mut options = indexmap::IndexMap::new();
    for (root, declared) in package_options {
        let parsed = manifest::parse_options(declared).with_context(|| slash(&root.join(MANIFEST_NAME)))?;
        for (name, default) in parsed {
            options.entry(name).or_insert(default);
        }
    }

    for (name, value) in overrides {
        if let Some(option) = options.get_mut(name) {
            *option = *value;
        }
    }

    Ok(options)
}

/// Every [deps] and [dev-deps] entry, used or not: its form, its keys, its condition's words.
fn check_deps(manifest: &Manifest, condition: &CondContext) -> Result<()> {
    for (name, value) in manifest.deps.iter().chain(&manifest.dev_deps) {
        let spec = manifest::parse_dep(name, value)?;
        if let Some(text) = &spec.cond {
            condition.check(text).with_context(|| format!("dependency `{name}`"))?;
        }
    }

    Ok(())
}

/// The names in a [deps] or [dev-deps] table whose `if` holds (conventional targets use all of them).
fn enabled_dep_names(deps: &indexmap::IndexMap<String, toml::Value>, condition: &CondContext) -> Result<Vec<String>> {
    let mut names = Vec::new();

    for (name, value) in deps {
        let spec = manifest::parse_dep(name, value)?;
        let enabled = match &spec.cond {
            Some(condition_text) => condition.test(condition_text)?,
            None => true,
        };
        if enabled {
            names.push(name.clone());
        }
    }

    Ok(names)
}

fn synthetic_target(name: &str, folder: &Path, spec: &DepSpec) -> indexmap::IndexMap<String, manifest::TargetSpec> {
    let has_sources = spec.sources.as_ref().is_some_and(|sources| !sources.is_empty());

    let target = manifest::TargetSpec {
        kind: Some(if has_sources { "lib" } else { "headers" }.to_string()),
        sources: Some(spec.sources.clone().unwrap_or_default()),
        include: Some(Vec::new()),
        public: Some(manifest::PublicSpec {
            include: Some(
                spec.include
                    .clone()
                    .unwrap_or_else(|| vec![if folder.join("include").is_dir() { "include" } else { "." }.to_string()]),
            ),
            defines: spec.defines.clone(),
            ..Default::default()
        }),
        ..Default::default()
    };

    let mut targets = indexmap::IndexMap::new();
    targets.insert(name.to_string(), target);
    targets
}

/// The tag, branch or commit a git dependency asks for ("HEAD" for none).
fn git_reference(spec: &DepSpec) -> String {
    spec.rev
        .clone()
        .or(spec.tag.clone())
        .or(spec.branch.clone())
        .unwrap_or_else(|| "HEAD".into())
}

/// A git dependency's checkout at the commit anvil.lock pins, else at the commit its tag, branch or rev names now
/// (which the lock then keeps). Each commit is checked out once into the shared cache. `parent` is the dependency
/// that declares it (none: the project itself).
fn fetch_git(name: &str, url: &str, spec: &DepSpec, parent: Option<&str>, lock: &mut LockFile) -> Result<PathBuf> {
    let reference = git_reference(spec);

    let commit = match lock.git_commit(name, url, &reference) {
        Some(commit) => commit,
        None => {
            lock.check_can_pin(&format!("git dependency `{name}` ({url} @ {reference})"))?;
            resolve_git_commit(name, url, &reference)?
        }
    };
    lock.set_git(name, url, &reference, &commit, parent);

    let folder = git_checkout_folder(name, url, &commit);
    if folder.join(".anvil-ready").is_file() {
        return Ok(folder);
    }

    crate::ui::status("Fetching", &format!("{name} ({url} @ {reference}, {})", &commit[..commit.len().min(12)]));
    let _ = std::fs::remove_dir_all(&folder);
    checkout_commit(name, url, &commit, &folder)?;

    std::fs::write(folder.join(".anvil-ready"), &commit)?;
    Ok(folder)
}

/// Where a git dependency's commit is checked out, in the shared cache.
fn git_checkout_folder(name: &str, url: &str, commit: &str) -> PathBuf {
    let key = format!("{:016x}", util::hash_bytes(format!("{url}@{commit}").as_bytes()));
    util::user_data_dir()
        .join("git")
        .join(format!("{}-{}", targets::sanitize_name(name), &key[..10]))
}

/// The checkout of the commit anvil.lock pins for a git dependency, when the cache has it already (listing fetches
/// nothing).
fn cached_git_checkout(name: &str, url: &str, spec: &DepSpec, lock: &LockFile) -> Option<PathBuf> {
    let commit = lock.git_commit(name, url, &git_reference(spec))?;
    let folder = git_checkout_folder(name, url, &commit);

    folder.join(".anvil-ready").is_file().then_some(folder)
}

/// The commit a reference names in a repository now: a full commit id as is, else asked of the remote (an annotated
/// tag's commit, not the tag object), else by cloning (short commit ids).
fn resolve_git_commit(name: &str, url: &str, reference: &str) -> Result<String> {
    let is_full_commit = reference.len() == 40 && reference.chars().all(|character| character.is_ascii_hexdigit());
    if is_full_commit {
        return Ok(reference.to_lowercase());
    }

    let output = Command::new("git")
        .args(["ls-remote", url, reference, &format!("{reference}^{{}}")])
        .output()
        .context("running git (is it installed?)")?;
    if !output.status.success() {
        bail!(
            "dependency `{name}`: git ls-remote {url} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    // "<commit>\t<ref>" lines: a peeled tag (^{}) wins, then an exact tag or branch, then HEAD
    let listing = String::from_utf8_lossy(&output.stdout).into_owned();
    let refs: Vec<(&str, &str)> = listing.lines().filter_map(|line| line.split_once('\t')).collect();
    let pick = |wanted: &str| refs.iter().find(|(_, name)| *name == wanted).map(|(commit, _)| commit.to_string());
    let found = pick(&format!("refs/tags/{reference}^{{}}"))
        .or_else(|| pick(&format!("refs/tags/{reference}")))
        .or_else(|| pick(&format!("refs/heads/{reference}")))
        .or_else(|| pick(reference));
    if let Some(commit) = found {
        return Ok(commit);
    }

    // A short commit id: only a clone can say which commit it is
    let scratch = util::user_data_dir()
        .join("git")
        .join(format!(".resolve-{}", targets::sanitize_name(name)));
    let _ = std::fs::remove_dir_all(&scratch);
    let cloned = Command::new("git")
        .args(["clone", "--quiet", "--no-checkout", url])
        .arg(&scratch)
        .status()
        .context("running git")?;
    let parsed = cloned
        .success()
        .then(|| {
            Command::new("git")
                .arg("-C")
                .arg(&scratch)
                .args(["rev-parse", "--verify", "--quiet", &format!("{reference}^{{commit}}")])
                .output()
                .ok()
        })
        .flatten();
    let _ = std::fs::remove_dir_all(&scratch);

    match parsed.filter(|output| output.status.success()) {
        Some(output) => Ok(String::from_utf8_lossy(&output.stdout).trim().to_string()),
        None => bail!("dependency `{name}`: {url} has no tag, branch or commit `{reference}`"),
    }
}

/// Checks out one commit into `folder`: a shallow fetch of just that commit, else a full clone.
fn checkout_commit(name: &str, url: &str, commit: &str, folder: &Path) -> Result<()> {
    let git = |arguments: &[&str]| -> bool {
        Command::new("git")
            .arg("-C")
            .arg(folder)
            .args(arguments)
            .output()
            .is_ok_and(|output| output.status.success())
    };

    std::fs::create_dir_all(folder)?;
    let shallow = git(&["init", "--quiet"])
        && git(&["remote", "add", "origin", url])
        && git(&["fetch", "--quiet", "--depth", "1", "origin", commit])
        && git(&["checkout", "--quiet", "FETCH_HEAD"]);
    if shallow {
        return update_submodules(name, url, folder);
    }

    // Servers that refuse fetching a commit by id: the whole history
    std::fs::remove_dir_all(folder)?;
    let cloned = Command::new("git")
        .args(["clone", "--quiet", url])
        .arg(folder)
        .status()
        .context("running git (is it installed?)")?;
    if !cloned.success() {
        bail!("dependency `{name}`: git clone {url} failed");
    }
    if !git(&["checkout", "--quiet", commit]) {
        bail!("dependency `{name}`: commit {commit} is not in {url} (was it force-pushed away? `anvil update {name}` picks a new one)");
    }

    update_submodules(name, url, folder)
}

/// A checkout's submodules, recursively: shallow, else with their history (servers that refuse fetching a commit
/// by id).
fn update_submodules(name: &str, url: &str, folder: &Path) -> Result<()> {
    if !folder.join(".gitmodules").is_file() {
        return Ok(());
    }

    let update = |shallow: bool| -> Result<std::process::Output> {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(folder)
            .args(["submodule", "update", "--init", "--recursive", "--quiet"]);
        if shallow {
            command.args(["--depth", "1"]);
        }
        command.output().context("running git submodule update")
    };

    if update(true)?.status.success() {
        return Ok(());
    }
    let output = update(false)?;
    if !output.status.success() {
        bail!(
            "dependency `{name}`: fetching the submodules of {url} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

/// Fills an external target from `pkg-config --cflags --libs`.
fn apply_pkg_config(target: &mut Target, module: &str, platform: &Platform) -> Result<()> {
    let program = pkg_config_program(platform)?;
    let query = |argument: &str| -> Result<Vec<String>> {
        let output = Command::new(&program)
            .arg(argument)
            .arg(module)
            .output()
            .with_context(|| format!("running {}", slash(&program)))?;
        if !output.status.success() {
            bail!(
                "{} does not know `{module}`: {}",
                slash(&program),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(util::split_command(&String::from_utf8_lossy(&output.stdout)))
    };

    for flag in query("--cflags")? {
        if let Some(folder) = flag.strip_prefix("-I") {
            target.public_system_include.push(PathBuf::from(folder));
        } else if let Some(define) = flag.strip_prefix("-D") {
            target.public_defines.push(define.to_string());
        } else {
            target.public_flags.push(flag);
        }
    }

    for flag in query("--libs")? {
        if let Some(folder) = flag.strip_prefix("-L") {
            target.lib_dirs.push(PathBuf::from(folder));
        } else if let Some(lib) = flag.strip_prefix("-l") {
            target.libs.push(lib.to_string());
        } else {
            target.link_flags.push(flag);
        }
    }

    Ok(())
}

/// The pkg-config to run: PKG_CONFIG, else `<triple>-pkg-config` in a cross build, else pkg-config (or pkgconf) on
/// PATH, wrappers (.bat, .cmd) included.
fn pkg_config_program(platform: &Platform) -> Result<PathBuf> {
    if let Some(configured) = std::env::var_os("PKG_CONFIG").filter(|value| !value.is_empty()) {
        let configured = configured.to_string_lossy().into_owned();
        return util::which(&configured).with_context(|| format!("PKG_CONFIG names {configured}, which is not a program on PATH"));
    }

    let cross = platform
        .is_cross()
        .then(|| util::which(&format!("{}pkg-config", platform.gnu_prefix())))
        .flatten();
    cross
        .or_else(|| util::which("pkg-config"))
        .or_else(|| util::which("pkgconf"))
        .context("pkg-config is not on PATH: install it, or set PKG_CONFIG to its path")
}

/// Two targets in one folder that both take the convention's sources would compile the same files twice.
fn check_shared_convention(targets: &[Target]) -> Result<()> {
    let conventional: Vec<&Target> = targets
        .iter()
        .filter(|target| target.conventional_sources && !target.sources.is_empty())
        .collect();

    for (position, first) in conventional.iter().enumerate() {
        let same_folder = conventional[position + 1..]
            .iter()
            .find(|second| util::path_key(&second.dir) == util::path_key(&first.dir));
        if let Some(second) = same_folder {
            bail!(
                "targets `{}` and `{}` both take the sources in {} by convention, so both would compile them; \
                 give one of them `sources = [...]` (or `sources = []` for none)",
                first.name,
                second.name,
                slash(&first.dir)
            );
        }
    }

    Ok(())
}

/// `{target:X}` in a target's build steps, link flags and test arguments becomes `{target:package/X}` when X is one of
/// `own`, its package's targets.
fn qualify_target_references(target: &mut Target, package: &str, own: &[String]) {
    let qualify_command = |command: &mut StrOrList| match command {
        StrOrList::Str(text) => *text = qualify_references(text, package, own),
        StrOrList::List(words) => {
            for word in words {
                *word = qualify_references(word, package, own);
            }
        }
    };

    for run in &mut target.runs {
        qualify_command(&mut run.command);
    }
    for step in target.pre_build.iter_mut().chain(&mut target.post_build) {
        qualify_command(&mut step.command);
    }
    for word in target.link_flags.iter_mut().chain(&mut target.args) {
        *word = qualify_references(word, package, own);
    }
}

/// `text` with each `{target:X}` whose X is one of `own` written `{target:package/X}`.
fn qualify_references(text: &str, package: &str, own: &[String]) -> String {
    let mut qualified = text.to_string();

    for name in crate::plan::target_references(&[text.to_string()]) {
        if !name.contains('/') && own.contains(&name) {
            qualified = qualified.replace(&format!("{{target:{name}}}"), &format!("{{target:{package}/{name}}}"));
        }
    }

    qualified
}

/// The tests.toml files of the package at `root`, not those of packages inside it, in a stable order.
/// The strings of a manifest or tests.toml, past its [deps] and [dev-deps] tables: where dependencies are named.
/// The words of a manifest or tests.toml that name dependencies: the items of the keys that list them (in targets,
/// `public`, `when`, `[defaults]` and presets), and the targets `{target:...}` names in any text (a step's command).
fn collect_dep_words(file: &Path, words: &mut Vec<String>) {
    fn collect(value: &toml::Value, naming: bool, words: &mut Vec<String>) {
        match value {
            toml::Value::String(text) => {
                if naming {
                    words.push(text.clone());
                }
                words.extend(crate::plan::target_references(std::slice::from_ref(text)));
            }
            toml::Value::Array(items) => items.iter().for_each(|item| collect(item, naming, words)),
            toml::Value::Table(table) => {
                for (key, item) in table {
                    collect(item, DEP_LIST_KEYS.contains(&key.as_str()), words);
                }
            }
            _ => {}
        }
    }

    let Ok(text) = crate::lsp::session::read_text(file) else { return };
    let Ok(table) = text.parse::<toml::Table>() else { return };
    for (key, value) in &table {
        if key != "deps" && key != "dev-deps" {
            collect(value, false, words);
        }
    }
}

fn tests_files(root: &Path, out_root: &Path) -> Vec<PathBuf> {
    // The language server reuses a walk while only manifests are edited
    let walk = || Ok::<_, ()>(tests_files_uncached(root, out_root));
    crate::lsp::session::walk("tests", &[root, out_root], walk).unwrap_or_default()
}

fn tests_files_uncached(root: &Path, out_root: &Path) -> Vec<PathBuf> {
    let walker = walkdir::WalkDir::new(root).sort_by_file_name().into_iter().filter_entry(|entry| {
        let other_package = entry.depth() > 0 && entry.file_type().is_dir() && entry.path().join(MANIFEST_NAME).is_file();
        !targets::is_skipped_dir(entry, out_root) && !other_package
    });
    walker
        .flatten()
        .filter(|entry| entry.file_type().is_file() && entry.file_name() == TESTS_FILE_NAME)
        .map(|entry| entry.into_path())
        .collect()
}

/// One test from its keys, as [[test]] tables and test targets both give them, with its workdir resolved.
fn test_case(spec: &TestSpec, workdir: PathBuf, package: &Package, out_dir: &Path, vcpkg_dir: &Path, profile: &str) -> Result<TestCase> {
    let fill = |text: &String| fill_test_text(text, package, out_dir, vcpkg_dir, profile);
    let pattern = |key: &str, pattern: &Option<String>| compile_test_regex(key, &pattern.as_ref().map(fill));

    Ok(TestCase {
        name: spec.name.clone(),
        command: spec.command.to_command().iter().map(fill).collect(),
        workdir,
        timeout: spec.timeout,
        env: spec.env.iter().flatten().map(|(name, value)| (name.clone(), fill(value))).collect(),
        will_fail: spec.will_fail.unwrap_or(false),
        after: spec.after.clone().unwrap_or_default(),
        serial: spec.serial.unwrap_or(false),
        pass_regex: pattern("pass-regex", &spec.pass_regex)?,
        fail_regex: pattern("fail-regex", &spec.fail_regex)?,
        skip_exit_code: spec.skip_exit_code,
        skip_regex: pattern("skip-regex", &spec.skip_regex)?,
        expect: compile_test_regexes("expect", spec.expect.as_deref().unwrap_or_default(), &fill)?,
        reject: compile_test_regexes("reject", spec.reject.as_deref().unwrap_or_default(), &fill)?,
        exit_code: spec.exit_code,
    })
}

/// Where a test was declared: its package, and the file (or target) that declares it.
struct TestOrigin {
    package: usize,
    place: String,
}

/// Test names are unique in a package, and the same name in two packages becomes `<package>/<name>` outside the
/// root package; `after` names a test (of its own package first; one that is off is not waited for), and no tests
/// wait for each other in a circle.
fn check_tests(tests: &mut [TestCase], origins: &[TestOrigin], package_names: &[&str], switched_off: &[String]) -> Result<()> {
    // Unique in a package
    let mut in_package: HashMap<(usize, &str), usize> = HashMap::new();
    for (index, test) in tests.iter().enumerate() {
        let Some(first) = in_package.insert((origins[index].package, test.name.as_str()), index) else {
            continue;
        };
        let (first_place, second_place) = (&origins[first].place, &origins[index].place);
        if first_place == second_place {
            bail!(
                "two tests are named `{}` in {first_place}; test names must be unique in a package",
                test.name
            );
        }
        bail!(
            "two tests are named `{}`, in {first_place} and in {second_place}; test names must be unique in a package: rename one",
            test.name
        );
    }

    // The same name in several packages: those outside the root package get their package's name in front
    let mut packages_with: HashMap<&str, HashSet<usize>> = HashMap::new();
    for (index, test) in tests.iter().enumerate() {
        packages_with.entry(test.name.as_str()).or_default().insert(origins[index].package);
    }
    let final_names: Vec<String> = tests
        .iter()
        .enumerate()
        .map(|(index, test)| {
            let package = origins[index].package;
            if packages_with[test.name.as_str()].len() > 1 && package != 0 {
                format!("{}/{}", package_names[package], test.name)
            } else {
                test.name.clone()
            }
        })
        .collect();
    let mut by_final_name: HashMap<&str, usize> = HashMap::new();
    for (index, name) in final_names.iter().enumerate() {
        if let Some(first) = by_final_name.insert(name.as_str(), index) {
            bail!(
                "two tests are named `{name}`, in {} and in {}; rename one",
                origins[first].place,
                origins[index].place
            );
        }
    }

    // `after`: a test of the same package by its name, else any test by its full name
    let mut afters = Vec::new();
    for (index, test) in tests.iter().enumerate() {
        let mut resolved = Vec::new();
        for name in &test.after {
            let own = in_package.get(&(origins[index].package, name.as_str()));
            match own.or_else(|| by_final_name.get(name.as_str())) {
                Some(other) => resolved.push(final_names[*other].clone()),
                None if switched_off.contains(name) => {}
                None => {
                    let hint = util::did_you_mean(name, final_names.iter().map(String::as_str));
                    bail!("test `{}`: after = [\"{name}\"] names no test{hint}", final_names[index]);
                }
            }
        }
        afters.push(resolved);
    }
    for ((test, name), after) in tests.iter_mut().zip(final_names).zip(afters) {
        test.name = name;
        test.after = after;
    }

    if let Some(circle) = find_after_circle(tests) {
        bail!("tests wait for each other in a circle: {} (each waits for the next)", circle.join(" -> "));
    }

    Ok(())
}

/// A chain of `after` that comes back to its start: the names along it, the first again at the end.
fn find_after_circle(tests: &[TestCase]) -> Option<Vec<String>> {
    let index_of: HashMap<&str, usize> = tests.iter().enumerate().map(|(index, test)| (test.name.as_str(), index)).collect();
    let mut done = vec![false; tests.len()];
    let mut path = Vec::new();

    (0..tests.len()).find_map(|start| visit_after(start, tests, &index_of, &mut done, &mut path))
}

fn visit_after(index: usize, tests: &[TestCase], index_of: &HashMap<&str, usize>, done: &mut [bool], path: &mut Vec<usize>) -> Option<Vec<String>> {
    if done[index] {
        return None;
    }

    // Back on the current path: the circle is the path from there
    if let Some(start) = path.iter().position(|on_path| *on_path == index) {
        let circle = path[start..].iter().chain([&index]);
        return Some(circle.map(|on_path| tests[*on_path].name.clone()).collect());
    }

    path.push(index);
    for name in &tests[index].after {
        if let Some(circle) = visit_after(index_of[name.as_str()], tests, index_of, done, path) {
            return Some(circle);
        }
    }
    path.pop();
    done[index] = true;

    None
}

/// {root}, {out}, {profile}, {config}, {version} and {vcpkg} in a test's or written file's text.
pub fn fill_test_text(text: &str, package: &Package, out_dir: &Path, vcpkg_dir: &Path, profile: &str) -> String {
    if !text.contains('{') {
        return text.to_string();
    }

    text.replace("{root}", &slash(&package.root))
        .replace("{out}", &slash(out_dir))
        .replace("{profile}", profile)
        .replace("{config}", &crate::targets::config_name(profile))
        .replace("{version}", package.version())
        .replace("{vcpkg}", &slash(vcpkg_dir))
}

/// `expect` and `reject` patterns, with placeholders filled in (`{out}/x.png written`).
fn compile_test_regexes(key: &str, patterns: &[String], fill: &dyn Fn(&String) -> String) -> Result<Vec<regex::Regex>> {
    patterns
        .iter()
        .map(|pattern| {
            let pattern = fill(pattern);
            regex::Regex::new(&pattern).with_context(|| format!("{key}: \"{pattern}\" is not a valid regex"))
        })
        .collect()
}

fn compile_test_regex(key: &str, pattern: &Option<String>) -> Result<Option<regex::Regex>> {
    pattern
        .as_ref()
        .map(|pattern| regex::Regex::new(pattern).with_context(|| format!("{key} = \"{pattern}\" is not a valid regex")))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_target_references_get_the_package_name() {
        let own = ["gen".to_string(), "unit".to_string()];

        assert_eq!(
            qualify_references("{target:gen} --in {target:gen}", "tools", &own),
            "{target:tools/gen} --in {target:tools/gen}"
        );
        assert_eq!(qualify_references("--lib={target:unit}.lib", "b", &own), "--lib={target:b/unit}.lib");

        // Another package's target, and one already qualified, stay as they are
        assert_eq!(qualify_references("{target:other}", "b", &own), "{target:other}");
        assert_eq!(qualify_references("{target:a/gen}", "b", &own), "{target:a/gen}");
    }

    #[test]
    fn output_folder_never_holds_the_project() {
        let root = std::env::temp_dir().join(format!("anvil_out_root_{}", std::process::id())).join("game");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("lib/inner")).unwrap();
        std::fs::write(root.join("src/main.cpp"), "").unwrap();
        std::fs::write(root.join("lib/inner").join(MANIFEST_NAME), "").unwrap();
        let out_root = |out: &str| {
            let manifest = manifest::parse_str(&format!("[project]\nout = \"{out}\"\n")).unwrap();
            output_root(&root, &manifest).map_err(|error| error.to_string())
        };

        assert_eq!(out_root("build").unwrap(), root.join("build"));
        assert!(out_root(".").unwrap_err().contains("is the project's own folder"));
        assert!(out_root("").unwrap_err().contains("is the project's own folder"));
        assert!(out_root("..").unwrap_err().contains("is a folder holding the project"));
        assert_eq!(out_root("src").unwrap(), root.join("src"));
        assert!(out_root("lib/inner").unwrap_err().contains("holds a package"));
        assert!(output_root(&root, &Manifest::default()).is_ok());

        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }
}
