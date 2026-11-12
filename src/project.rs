//! The project model: finds the root manifest (or works with none), loads packages and workspace members, resolves
//! every dependency (local targets, vcpkg, pkg-config, path and git packages, prebuilt libraries) and installs vcpkg
//! packages, giving the planner a complete list of targets.

use crate::cond::CondContext;
use crate::lockfile::{LockFile, Refresh};
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

    // Loading state: externals by (package, dep name), vcpkg requests, path/git packages by folder, the lock file
    externals: HashMap<(usize, String), usize>,
    vcpkg_requests: Vec<Request>,
    package_dirs: HashMap<String, usize>,
    lock: Option<LockFile>,
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
        let requested = options.toolchain.clone().or(project.toolchain.clone()).or(Settings::load().toolchain);
        let toolchain = Toolchain::find(requested.as_deref(), &platform)?;
        toolchain.adapt_profile(&mut profile)?;

        // Output folders: a toolchain or a target machine picked on the command line gets its own
        let out_root = util::absolute(&root, Path::new(project.out.as_deref().unwrap_or("out")));
        let mut out_folder = profile.name.clone();
        if options.toolchain.is_some() {
            out_folder.push_str(&format!("-{}", toolchain.family.name()));
        }
        if options.target.is_some() && platform.is_cross() {
            out_folder.push_str(&format!("-{}", platform.name()));
        }
        let out_dir = options.host_tools_dir.clone().unwrap_or_else(|| out_root.join(out_folder));

        // vcpkg's triplet is known before anything installs, for {vcpkg}
        let vcpkg_triplet = manifest
            .vcpkg
            .as_ref()
            .and_then(|spec| spec.triplet.clone())
            .unwrap_or_else(|| vcpkg::default_triplet(&toolchain, profile.static_runtime));
        let vcpkg_dir = out_root.join("vcpkg").join("installed").join(&vcpkg_triplet).join(&vcpkg_triplet);

        let profile_names: Vec<String> = manifest.profile.keys().cloned().collect();
        let option_names: Vec<String> = manifest.options.keys().cloned().collect();
        crate::cond::check_names(&profile_names, &option_names)?;

        let lock = LockFile::load(&root, options.locked, options.refresh.clone())?;
        let condition = CondContext {
            family: toolchain.family,
            platform: platform.clone(),
            profiles: profile.chain.clone(),
            all_profiles: manifest.profile.keys().cloned().collect(),
            options: resolve_options(&manifest, &options.options)?,
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
            condition,
            default_run: project.default_run.clone(),
            vcpkg: None,
            cuda: None,
            cuda_error: None,
            options: options.clone(),
            externals: HashMap::new(),
            vcpkg_requests: Vec::new(),
            package_dirs: HashMap::new(),
            lock: Some(lock),
        };

        // Root package and workspace members
        let members = workspace_members(&root, &manifest)?;
        model.add_package(&root, manifest, true)?;
        for member in members {
            let member_manifest = manifest::load(&member.join(MANIFEST_NAME))?;
            model.add_package(&member, member_manifest, true)?;
        }

        // Dependencies (this may load more packages, which append targets: iterate by index)
        let mut index = 0;
        while index < model.targets.len() {
            model.link_target(index)?;
            index += 1;
        }

        model.check_cycles()?;
        model.check_output_names()?;
        model.check_sources()?;
        model.find_cuda()?;
        model.install_vcpkg()?;
        crate::plan::expand_hashes(&mut model)?;
        model.lock.take().unwrap().finish(!model.vcpkg_requests.is_empty())?;
        model.write_files()?;

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
        let hint = util::suggest(name, names.iter().copied())
            .map(|close| format!(" Did you mean `{close}`?"))
            .unwrap_or_default();
        bail!("no target named `{name}`.{hint} Targets: {}", names.join(", "))
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

    fn add_package(&mut self, root: &Path, manifest: Manifest, member: bool) -> Result<usize> {
        let name = package_name(root, &manifest);
        let index = self.packages.len();
        self.package_dirs.insert(util::path_key(root), index);

        // Options declared by members join the root's
        for (option, value) in manifest::parse_options(&manifest.options)? {
            self.condition.options.entry(option).or_insert(value);
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
            c_std: project.c_std.as_deref().unwrap_or("c17"),
            version: package.version(),
            vcpkg_dir: &self.vcpkg_dir,
            cxx: &self.toolchain.cxx,
            cc: &self.toolchain.cc,
            condition: &self.condition,
        };

        // Explicit targets, or the layout conventions
        let mut created = Vec::new();
        if manifest.target.is_empty() {
            let enabled_deps = enabled_dep_names(manifest, &self.condition)?;
            created = Target::conventional(&scope, &manifest.defaults, &enabled_deps)?;
        } else {
            for (name, spec) in &manifest.target {
                if let Some(target) = Target::from_spec(name, spec, &manifest.defaults, &manifest.preset, &scope)? {
                    created.push(target);
                }
            }
            check_shared_convention(&created)?;
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

    /// Resolves the dependency names of one target.
    fn link_target(&mut self, index: usize) -> Result<()> {
        let dep_names = self.targets[index].dep_names.clone();
        let package = self.targets[index].package;

        let mut deps = Vec::new();
        for (name, public) in dep_names {
            let resolved = self
                .resolve_dep(package, &name)
                .with_context(|| format!("in target `{}`", self.targets[index].name))?;
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
            let resolved = self
                .resolve_dep(package, &name)
                .with_context(|| format!("in target `{}` (header-deps)", self.targets[index].name))?;
            if let Some(dependency) = resolved.filter(|dependency| *dependency != index) {
                header_deps.push(dependency);
            }
        }

        // Build-order-only dependencies
        let mut needs = Vec::new();
        for name in self.targets[index].need_names.clone() {
            let resolved = self
                .resolve_dep(package, &name)
                .with_context(|| format!("in target `{}` (needs)", self.targets[index].name))?;
            if let Some(dependency) = resolved.filter(|dependency| *dependency != index) {
                needs.push(dependency);
            }
        }

        // Libraries linked whole
        let mut whole_archive = Vec::new();
        for name in self.targets[index].whole_archive_names.clone() {
            let resolved = self
                .resolve_dep(package, &name)
                .with_context(|| format!("in target `{}` (whole-archive)", self.targets[index].name))?;
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

    /// A dependency name to a target: a local target, a [deps] entry, a workspace member, or "package/target".
    /// None when the [deps] entry's `if` is false.
    fn resolve_dep(&mut self, package: usize, name: &str) -> Result<Option<usize>> {
        // package/target
        if let Some((package_name, target_name)) = name.split_once('/') {
            let found = self
                .targets
                .iter()
                .position(|target| target.name == target_name && self.packages[target.package].name == package_name);
            return found
                .map(Some)
                .with_context(|| format!("no target `{target_name}` in package `{package_name}`"));
        }

        // A target of the same package
        let local = self
            .targets
            .iter()
            .position(|target| target.package == package && target.name == name && target.kind != Kind::External);
        if let Some(local) = local {
            return Ok(Some(local));
        }

        // A [deps] entry
        if let Some(value) = self.packages[package].manifest.deps.get(name).cloned() {
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
        let hint = util::suggest(name, known.iter().map(|known| known.as_str()))
            .map(|close| format!(" Did you mean `{close}`?"))
            .unwrap_or_default();
        bail!("unknown dependency `{name}`.{hint} Declare it under [deps] (e.g. `anvil add {name}`), or name one of this package's targets.")
    }

    fn resolve_dep_spec(&mut self, package: usize, name: &str, spec: &DepSpec) -> Result<usize> {
        let key = (package, name.to_string());
        if let Some(existing) = self.externals.get(&key) {
            return Ok(*existing);
        }

        let package_root = self.packages[package].root.clone();
        let resolved = if let Some(path) = &spec.path {
            let folder = util::absolute(&package_root, Path::new(&util::expand_env(path)?));
            self.load_source_package(name, &folder, spec)?
        } else if let Some(url) = &spec.git {
            let folder = fetch_git(name, url, spec, self.lock.as_mut().unwrap())?;
            self.load_source_package(name, &folder, spec)?
        } else {
            self.add_external(package, name, spec)?
        };

        self.externals.insert(key, resolved);
        Ok(resolved)
    }

    /// vcpkg, pkg-config and prebuilt dependencies become External targets carrying their usage.
    fn add_external(&mut self, package: usize, name: &str, spec: &DepSpec) -> Result<usize> {
        let package_root = self.packages[package].root.clone();
        let mut target = Target::new(name, package, Kind::External, &package_root);
        target.default = false;

        // Prebuilt parts and overrides
        let expand = |text: &String| -> Result<PathBuf> { Ok(util::absolute(&package_root, Path::new(&util::expand_env(text)?))) };
        target.public_system_include = spec.include.iter().flatten().map(expand).collect::<Result<_>>()?;
        target.lib_dirs = spec.lib_dirs.iter().flatten().map(expand).collect::<Result<_>>()?;
        target.runtime_files = spec.runtime.iter().flatten().map(expand).collect::<Result<_>>()?;
        target.public_defines = spec.defines.clone().unwrap_or_default();
        target.public_flags = spec.flags.clone().unwrap_or_default();
        target.link_flags = spec.link_flags.clone().unwrap_or_default();

        if spec.vcpkg.is_none() {
            for lib in spec.libs.iter().flatten() {
                let expanded = util::expand_env(lib)?;
                let is_path = expanded.contains('/') || expanded.contains('\\') || Path::new(&expanded).extension().is_some();
                target.libs.push(if is_path {
                    slash(&util::absolute(&package_root, Path::new(&expanded)))
                } else {
                    expanded
                });
            }
        }

        // vcpkg: filled in after the install
        if let Some(port) = &spec.vcpkg {
            target.vcpkg = Some((port.clone(), spec.libs.clone()));
            self.request_vcpkg(port, spec);
        }

        // pkg-config: ask it now
        if let Some(module) = &spec.pkg_config {
            apply_pkg_config(&mut target, module)?;
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

    /// A source package from a path or git checkout: its anvil.toml, or a library made from its layout.
    fn load_source_package(&mut self, name: &str, folder: &Path, spec: &DepSpec) -> Result<usize> {
        if !folder.is_dir() {
            bail!("dependency `{name}`: folder {} does not exist", slash(folder));
        }

        // Already loaded (shared by several packages, or a workspace member)
        if let Some(existing) = self.package_dirs.get(&util::path_key(folder)).copied() {
            return self.main_target(existing, spec.target.as_deref());
        }

        let manifest_file = folder.join(MANIFEST_NAME);
        if manifest_file.is_file() {
            let package = self.add_package(folder, manifest::load(&manifest_file)?, false)?;
            return self.main_target(package, spec.target.as_deref());
        }

        // No anvil.toml: a library from the given include/sources, or include/ and src/
        let has_overrides = spec.include.is_some() || spec.sources.is_some();
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
            ..Default::default()
        };

        let package = self.add_package(folder, synthetic, false)?;
        self.main_target(package, spec.target.as_deref())
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
        for target in &self.targets {
            let compiles = matches!(target.kind, Kind::Exe | Kind::Test | Kind::Static | Kind::Shared | Kind::Object);
            let takes_objects = target.deps.iter().any(|(dependency, _)| self.targets[*dependency].kind == Kind::Object);
            if compiles && target.sources.is_empty() && target.runs.is_empty() && !takes_objects {
                bail!(
                    "target `{}` has nothing to compile for this platform: its sources are all under `when` blocks that do not \
                     hold, or its patterns match no files. Give it sources here, or an `if` so it exists only where it has some",
                    target.name
                );
            }
        }

        Ok(())
    }

    /// Two targets must not write the same file.
    fn check_output_names(&self) -> Result<()> {
        let mut seen: HashMap<String, &str> = HashMap::new();

        for target in &self.targets {
            let Some(file) = crate::plan::artifact_path(self, target) else {
                continue;
            };
            if let Some(other) = seen.insert(util::path_key(&file), &target.name) {
                bail!(
                    "targets `{other}` and `{}` would both write {}; give one an `output-name` or an `output-dir`",
                    target.name,
                    slash(&file)
                );
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
        if self.vcpkg_requests.is_empty() {
            return Ok(());
        }

        let spec = self.packages[0].manifest.vcpkg.clone().unwrap_or_default();
        let root = self.root.clone();
        let resolve = |text: &String| -> Result<PathBuf> { Ok(util::absolute(&root, Path::new(&util::expand_env(text)?))) };

        // The instance and the install settings
        let root_override = spec.root.as_ref().map(resolve).transpose()?;
        let mut instance = Instance::select(root_override.as_deref())?;
        let config = vcpkg::InstallConfig {
            project_root: &self.root,
            out_root: &self.out_root,
            triplet: self.vcpkg_triplet.clone(),
            manifest: spec.manifest.as_ref().map(resolve).transpose()?,
            baseline: spec.baseline.clone(),
            locked_baseline: self.lock.as_ref().unwrap().vcpkg_baseline(),
            overlay_ports: spec.overlay_ports.iter().map(resolve).collect::<Result<_>>()?,
            overlay_triplets: spec.overlay_triplets.iter().map(resolve).collect::<Result<_>>()?,
        };

        let vcpkg::Installed {
            tree,
            own_manifest,
            baseline,
        } = instance.install(&self.vcpkg_requests, &config)?;
        self.lock.as_mut().unwrap().set_vcpkg(baseline, &config.triplet, tree.versions());
        let debug_libs = self.profile.debug_runtime && self.toolchain.is_msvc_abi();

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
                .usage(port, debug_libs, only_libs.as_deref())
                .with_context(|| format!("dependency `{}`", target.name))?;
            target.public_system_include.extend(usage.includes);
            target.public_defines.extend(usage.defines);
            target.libs.extend(usage.libs.iter().map(|lib| slash(lib)));
            target.runtime_files.extend(usage.runtime);
        }

        self.vcpkg = Some(instance);
        Ok(())
    }

    /// [[write]] files of the root package and workspace members, rewritten only when their text changed.
    fn write_files(&self) -> Result<()> {
        for package in self.packages.iter().filter(|package| package.member) {
            for spec in &package.manifest.write {
                if let Some(condition) = &spec.cond {
                    if !self.condition.test(condition).with_context(|| format!("[[write]] {}", spec.path))? {
                        continue;
                    }
                }

                let fill = |text: &str| fill_test_text(text, package, &self.out_dir, &self.vcpkg_dir);
                let path = util::absolute(&package.root, Path::new(&fill(&spec.path)));
                let text = fill(&spec.text);

                // A cross build's host tools write only into their own folder: the files elsewhere are the target's
                if self.options.host_tools_dir.is_some() && util::strip_prefix_ci(&path, &self.out_dir).is_none() {
                    continue;
                }
                if std::fs::read_to_string(&path).ok().as_deref() == Some(text.as_str()) {
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

        // Test targets run their program
        for target in &self.targets {
            let package = &self.packages[target.package];
            if target.kind != Kind::Test || !package.member {
                continue;
            }

            let mut command = vec![format!("{{target:{}}}", target.name)];
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
            let test = test_case(&spec, workdir, package, &self.out_dir, &self.vcpkg_dir).with_context(|| format!("in [target.{}]", target.name))?;
            self.tests.push(test);
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
        for package in self.packages.iter().filter(|package| package.member) {
            let mut specs = package.manifest.test.clone();
            for file in tests_files(&package.root, &self.out_root) {
                specs.extend(manifest::load_tests(&file)?);
            }

            for spec in &specs {
                let context = || format!("in [[test]] `{}`", spec.name);
                if let Some(condition_text) = &spec.cond {
                    if !self.condition.test(condition_text).with_context(context)? {
                        switched_off.push(spec.name.clone());
                        continue;
                    }
                }

                let workdir = match &spec.workdir {
                    Some(workdir) => {
                        let filled = fill_test_text(workdir, package, &self.out_dir, &self.vcpkg_dir);
                        util::absolute(&package.root, Path::new(&util::expand_env(&filled).with_context(context)?))
                    }
                    None => package.root.clone(),
                };
                self.tests
                    .push(test_case(spec, workdir, package, &self.out_dir, &self.vcpkg_dir).with_context(context)?);
            }
        }

        check_tests(&mut self.tests, &switched_off)
    }
}

/// Finds the project root: the nearest anvil.toml (or the workspace that lists it). With none, a folder that
/// looks like a C++ project builds with no manifest at all.
pub fn locate(start: &Path) -> Result<(PathBuf, Manifest)> {
    let start = util::absolute(&std::env::current_dir()?, start);

    let nearest = start
        .ancestors()
        .find(|folder| folder.join(MANIFEST_NAME).is_file())
        .map(|folder| folder.to_path_buf());
    let Some(nearest) = nearest else {
        return zero_config(&start);
    };

    // A workspace further up that includes this package is the real root
    for ancestor in nearest.ancestors().skip(1) {
        let candidate = ancestor.join(MANIFEST_NAME);
        if !candidate.is_file() {
            continue;
        }

        let manifest = manifest::load(&candidate)?;
        if manifest.workspace.is_some()
            && workspace_members(ancestor, &manifest)?
                .iter()
                .any(|member| util::path_key(member) == util::path_key(&nearest))
        {
            return Ok((ancestor.to_path_buf(), manifest));
        }
    }

    let manifest = manifest::load(&nearest.join(MANIFEST_NAME))?;
    Ok((nearest, manifest))
}

fn zero_config(start: &Path) -> Result<(PathBuf, Manifest)> {
    let has_sources = start.join("src").is_dir()
        || std::fs::read_dir(start)?.flatten().any(|entry| {
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

fn package_name(root: &Path, manifest: &Manifest) -> String {
    let declared = manifest.project.as_ref().and_then(|project| project.name.clone());
    declared.unwrap_or_else(|| {
        root.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "app".into())
    })
}

fn workspace_members(root: &Path, manifest: &Manifest) -> Result<Vec<PathBuf>> {
    let Some(workspace) = &manifest.workspace else { return Ok(Vec::new()) };
    let mut members = Vec::new();

    for pattern in &workspace.members {
        // A plain folder, or a glob over folders
        if !pattern.contains(['*', '?', '[', '{']) {
            let folder = util::absolute(root, Path::new(pattern));
            if !folder.join(MANIFEST_NAME).is_file() {
                bail!("workspace member \"{pattern}\" has no {MANIFEST_NAME}");
            }
            members.push(folder);
            continue;
        }

        let matcher = targets::compile_glob(&slash(&util::absolute(root, Path::new(pattern))))?.compile_matcher();
        for entry in walkdir::WalkDir::new(root).max_depth(4).into_iter().flatten() {
            if entry.file_type().is_dir() && matcher.is_match(slash(entry.path())) && entry.path().join(MANIFEST_NAME).is_file() {
                members.push(entry.path().to_path_buf());
            }
        }
    }

    members.retain(|member| util::path_key(member) != util::path_key(root));
    Ok(members)
}

/// [options] defaults with `-o name=value` overrides from the command line.
fn resolve_options(manifest: &Manifest, overrides: &[(String, bool)]) -> Result<indexmap::IndexMap<String, bool>> {
    let mut options = manifest::parse_options(&manifest.options)?;

    for (name, value) in overrides {
        if !options.contains_key(name) {
            let hint = util::suggest(name, options.keys().map(|key| key.as_str()))
                .map(|close| format!(" Did you mean `{close}`?"))
                .unwrap_or_default();
            bail!("unknown option `{name}`.{hint} Declare it under [options].");
        }
        options.insert(name.clone(), *value);
    }

    Ok(options)
}

/// The [deps] names whose `if` holds (conventional targets use all of them).
fn enabled_dep_names(manifest: &Manifest, condition: &CondContext) -> Result<Vec<String>> {
    let mut names = Vec::new();

    for (name, value) in &manifest.deps {
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

/// A git dependency's checkout at the commit anvil.lock pins, else at the commit its tag, branch or rev names now
/// (which the lock then keeps). Each commit is checked out once into the shared cache.
fn fetch_git(name: &str, url: &str, spec: &DepSpec, lock: &mut LockFile) -> Result<PathBuf> {
    let reference = spec
        .rev
        .clone()
        .or(spec.tag.clone())
        .or(spec.branch.clone())
        .unwrap_or_else(|| "HEAD".into());

    let commit = match lock.git_commit(name, url, &reference) {
        Some(commit) => commit,
        None => resolve_git_commit(name, url, &reference)?,
    };
    lock.set_git(name, url, &reference, &commit);

    let key = format!("{:016x}", util::hash_bytes(format!("{url}@{commit}").as_bytes()));
    let folder = util::user_data_dir()
        .join("git")
        .join(format!("{}-{}", targets::sanitize_name(name), &key[..10]));
    if folder.join(".anvil-ready").is_file() {
        return Ok(folder);
    }

    crate::ui::status("Fetching", &format!("{name} ({url} @ {reference}, {})", &commit[..commit.len().min(12)]));
    let _ = std::fs::remove_dir_all(&folder);
    checkout_commit(name, url, &commit, &folder)?;

    std::fs::write(folder.join(".anvil-ready"), &commit)?;
    Ok(folder)
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
        return Ok(());
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

    Ok(())
}

/// Fills an external target from `pkg-config --cflags --libs`.
fn apply_pkg_config(target: &mut Target, module: &str) -> Result<()> {
    let query = |argument: &str| -> Result<Vec<String>> {
        let output = Command::new("pkg-config")
            .arg(argument)
            .arg(module)
            .output()
            .context("running pkg-config (is it installed?)")?;
        if !output.status.success() {
            bail!("pkg-config does not know `{module}`: {}", String::from_utf8_lossy(&output.stderr).trim());
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

/// The tests.toml files of the package at `root`, not those of packages inside it, in a stable order.
fn tests_files(root: &Path, out_root: &Path) -> Vec<PathBuf> {
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
fn test_case(spec: &TestSpec, workdir: PathBuf, package: &Package, out_dir: &Path, vcpkg_dir: &Path) -> Result<TestCase> {
    let fill = |text: &String| fill_test_text(text, package, out_dir, vcpkg_dir);
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

/// Test names are unique, `after` names a test (one that is off is not waited for), and no tests wait for each
/// other in a circle.
fn check_tests(tests: &mut [TestCase], switched_off: &[String]) -> Result<()> {
    let mut seen = HashSet::new();
    if let Some(twice) = tests.iter().find(|test| !seen.insert(test.name.as_str())) {
        bail!("two tests are named `{}`; test names must be unique", twice.name);
    }

    let names: Vec<String> = tests.iter().map(|test| test.name.clone()).collect();
    for test in tests.iter_mut() {
        if let Some(unknown) = test.after.iter().find(|name| !names.contains(name) && !switched_off.contains(name)) {
            let hint = util::suggest(unknown, names.iter().map(String::as_str))
                .map(|close| format!(" (did you mean `{close}`?)"))
                .unwrap_or_default();
            bail!("test `{}`: after = [\"{unknown}\"] names no test{hint}", test.name);
        }
        test.after.retain(|name| names.contains(name));
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

/// Placeholders in test commands, folders and environment values: {root} {out} {profile} {version}.
/// {root}, {out}, {profile}, {config}, {version} and {vcpkg} in a test's or written file's text.
fn fill_test_text(text: &str, package: &Package, out_dir: &Path, vcpkg_dir: &Path) -> String {
    if !text.contains('{') {
        return text.to_string();
    }

    let profile = out_dir.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let base_profile = profile.split('-').next().unwrap_or_default();
    text.replace("{root}", &slash(&package.root))
        .replace("{out}", &slash(out_dir))
        .replace("{profile}", &profile)
        .replace("{config}", &crate::targets::config_name(base_profile))
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
