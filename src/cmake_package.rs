//! Installed CMake packages (`[deps] qt = { cmake = "Qt6", components = ["Widgets"] }`): found where
//! find_package(<name> CONFIG) finds them, then read with the reader vcpkg's packages go through (crate::vcpkg_tree):
//! their imported targets, what those give their users, and their files for the build's configuration.

use crate::platform::{Cpu, Os, Platform};
use crate::profile::Profile;
use crate::toolchain::Toolchain;
use crate::util::{self, slash};
use crate::vcpkg_tree::{self, BuildContext, CmakePackage, ImportPrefix, ImportedFiles, ImportedTarget, Requirements, Usage};
use anyhow::{bail, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

// Namespaces of the targets CMake's own find modules define, which Anvil has built in (crate::vcpkg_tree::module_target)
const FIND_MODULE_NAMESPACES: &[&str] = &["Threads", "OpenMP", "OpenGL", "Vulkan"];

// Release configurations, in the order a build that does not link debug libraries takes them
const RELEASE_CONFIGURATIONS: &[&str] = &["RELEASE", "RELWITHDEBINFO", "MINSIZEREL", "NOCONFIG"];

// Folders a shared library's users find it in without an rpath
const SYSTEM_LIBRARY_FOLDERS: &[&str] = &["/lib", "/lib64", "/usr/lib", "/usr/lib64", "/usr/local/lib"];

// Include folders the compilers search by themselves, which CMake leaves out of command lines: as -isystem they would
// come before the C++ library's own headers and break its #include_next
const IMPLICIT_INCLUDE_FOLDERS: &[&str] = &["/usr/include", "/usr/local/include"];

/// One `cmake` dependency, as `[deps]` gives it.
pub struct Request<'a> {
    /// The [deps] key, for messages.
    pub dependency: &'a str,
    pub package: &'a str,
    pub components: &'a [String],
    pub version: Option<&'a str>,
    pub targets: Option<&'a [String]>,
    /// `prefix`: the install folders to look in instead of CMake's search, absolute.
    pub prefixes: &'a [PathBuf],
}

/// A package found and read: what its users get, and how CMake users of an `anvil install` find it again.
pub struct Found {
    pub package: CmakePackage,
    pub usage: Usage,
}

/// Finds the package `request` names and reads what its targets (or its components', or those `targets` names) give
/// their users for this toolchain and profile.
pub fn find(request: &Request, toolchain: &Toolchain, profile: &Profile) -> Result<Found> {
    let dependency = request.dependency;
    let mut reader = Reader {
        build: BuildContext::new(toolchain, profile),
        debug: vcpkg_tree::uses_debug_libraries(toolchain, profile),
        platform: toolchain.platform.clone(),
        packages: Vec::new(),
        targets: Vec::new(),
        searched: HashSet::new(),
    };

    // The package, compatible with the version asked for
    let wanted_version = match request.version {
        Some(text) => match parse_version(text) {
            Some(version) => Some(version),
            None => bail!("dependency `{dependency}`: version = \"{text}\" is not a version; write numbers such as \"1.2\""),
        },
        None => None,
    };
    let places = match request.prefixes.is_empty() {
        true => default_places(request.package, &reader.platform),
        false => request
            .prefixes
            .iter()
            .map(|prefix| (Place::Prefix(prefix.clone()), "`prefix`".to_string()))
            .collect(),
    };
    let located = match reader.locate(request.package, &places, wanted_version.as_deref()) {
        Ok(located) => located,
        Err(rejected) => bail!("{}", not_found_message(request, &places, &rejected)),
    };

    // Qt 5's packages are written by hand, with variables only running them fills in: read by their own rules
    if crate::qt::is_qt5_package(request.package) {
        return crate::qt::read_qt5(request, located.config_file.parent().unwrap(), toolchain, profile);
    }
    let main = reader.read_package(&located);

    // The targets its users link, then what they need
    let wanted = reader.wanted_targets(request, main)?;
    let usage = reader.usage(dependency, &wanted);

    let package = &reader.packages[main];
    Ok(Found {
        package: CmakePackage {
            name: package.name.clone(),
            config: true,
            targets: wanted,
            components: request.components.to_vec(),
            hint: Some(package.folder.clone()),
        },
        usage,
    })
}

/// "1.2.3" as numbers; None for anything else (at most four numbers).
pub fn parse_version(text: &str) -> Option<Vec<u64>> {
    let numbers: Vec<u64> = text.trim().split('.').map(|part| part.parse().ok()).collect::<Option<_>>()?;
    (1..=4).contains(&numbers.len()).then_some(numbers)
}

/// Where a search looks: an install prefix, under which CMake's search rules apply, or the folder holding the config
/// file itself (`<Package>_DIR`).
#[derive(Debug, Clone)]
enum Place {
    Prefix(PathBuf),
    Folder(PathBuf),
}

/// A config file found, with the prefix it was found under.
struct Located {
    config_file: PathBuf,
    prefix: PathBuf,
}

/// A package read: its name as its config file spells it, that file's folder and its install prefix, and the
/// packages its files name (find_dependency), which define the targets of theirs it links.
struct LoadedPackage {
    name: String,
    folder: PathBuf,
    prefix: PathBuf,
    named: Vec<String>,
    targets: Vec<String>,

    /// The folders its find_package calls give as HINTS or PATHS (an `anvil install` package's dependencies).
    hints: Vec<PathBuf>,
}

/// How a version file judges a requested version (write_basic_package_version_file's COMPATIBILITY).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Compatibility {
    AnyNewer,
    SameMajor,
    SameMinor,
    Exact,
}

/// What a `<Package>ConfigVersion.cmake` says: the version, how it matches requests, and the pointer size the package
/// was built for.
#[derive(Debug, PartialEq)]
struct VersionFile {
    version: Option<String>,
    compatibility: Compatibility,
    pointer_size: Option<u32>,
}

/// The packages read for one dependency and the imported targets they define.
struct Reader {
    build: BuildContext,
    debug: bool,
    platform: Platform,
    packages: Vec<LoadedPackage>,
    targets: Vec<ImportedTarget>,

    /// Package names looked for, each once.
    searched: HashSet<String>,
}

impl Reader {
    /// The first config file of `name` in `places` that suits the build (and the version): else what was found and
    /// why it did not suit.
    fn locate(&self, name: &str, places: &[(Place, String)], version: Option<&[u64]>) -> std::result::Result<Located, Vec<String>> {
        let mut rejected = Vec::new();
        let mut seen = HashSet::new();

        for (place, _) in places {
            let (prefix, folders) = match place {
                Place::Prefix(prefix) => (prefix.clone(), config_folders(prefix, name, &self.platform)),
                Place::Folder(folder) => (folder.clone(), vec![folder.clone()]),
            };
            for folder in folders {
                let Some(config_file) = config_file_in(&folder, name) else { continue };
                if !seen.insert(util::path_key(&config_file)) {
                    continue;
                }

                // CMake skips a package its version file finds unsuitable
                let version_file = read_version_file(&config_file);
                match judge(version_file.as_ref(), version, pointer_size(self.platform.cpu)) {
                    Ok(()) => return Ok(Located { config_file, prefix }),
                    Err(reason) => rejected.push(format!("{} ({reason})", slash(&config_file))),
                }
            }
        }

        Err(rejected)
    }

    /// Reads a package's config file and the files it includes from its folder; returns its index.
    fn read_package(&mut self, located: &Located) -> usize {
        let folder = located.config_file.parent().unwrap().to_path_buf();
        let file_name = located.config_file.file_name().unwrap().to_string_lossy().into_owned();
        let name = file_name
            .strip_suffix("Config.cmake")
            .or_else(|| file_name.strip_suffix("-config.cmake"))
            .unwrap_or(&file_name)
            .to_string();

        // Its files: the config file and those it includes (its targets, each configuration's files)
        let files = included_files(&located.config_file);
        let mut named = Vec::new();
        let mut hints = Vec::new();
        let mut prefix = located.prefix.clone();
        for file in &files {
            let Ok(text) = std::fs::read_to_string(file) else { continue };
            for (package, package_hints) in named_packages(&text) {
                if package != name && !named.contains(&package) {
                    named.push(package);
                }
                hints.extend(package_hints.into_iter().map(PathBuf::from).filter(|hint| hint.is_absolute()));
            }
            if *file == located.config_file {
                if let Some(computed) = vcpkg_tree::file_import_prefix(&text, file) {
                    prefix = computed;
                }
            }
        }

        // Its imported targets; a target another package read already defines stays that one's
        let fallback = ImportPrefix::PerFile { fallback: slash(&prefix) };
        let mut targets = Vec::new();
        for target in vcpkg_tree::read_imported_targets(&files, &fallback, &self.build) {
            if !self.targets.iter().any(|known| known.name == target.name) {
                targets.push(target.name.clone());
                self.targets.push(target);
            }
        }
        self.searched.insert(name.to_lowercase());

        self.packages.push(LoadedPackage {
            name,
            folder,
            prefix,
            named,
            targets,
            hints,
        });
        self.packages.len() - 1
    }

    /// Finds and reads package `name`: beside the packages read already (find_dependency's usual places), then in
    /// CMake's search. False when it is not there, or was looked for before.
    fn read_named(&mut self, name: &str) -> bool {
        if !self.searched.insert(name.to_lowercase()) {
            return false;
        }

        let mut places: Vec<(Place, String)> = Vec::new();
        for package in &self.packages {
            places.extend(package.hints.iter().map(|hint| (Place::Prefix(hint.clone()), String::new())));
            if let Some(parent) = package.folder.parent() {
                places.push((Place::Prefix(parent.to_path_buf()), String::new()));
            }
            places.push((Place::Prefix(package.prefix.clone()), String::new()));
        }
        places.extend(default_places(name, &self.platform));

        match self.locate(name, &places, None) {
            Ok(located) => {
                self.read_package(&located);
                true
            }
            Err(_) => false,
        }
    }

    /// Reads packages until one defines target `name` (`Qt6::Gui` is in package Qt6Gui, `Boost::filesystem` in
    /// boost_filesystem, `fmt::fmt` in fmt): those the packages read name first, then names made from the target's.
    fn read_package_of(&mut self, name: &str) -> bool {
        let Some((namespace, short)) = name.split_once("::") else { return false };
        if FIND_MODULE_NAMESPACES.contains(&namespace) {
            return false;
        }

        let named: Vec<String> = self.packages.iter().flat_map(|package| package.named.iter().cloned()).collect();
        let mut candidates: Vec<String> = named.iter().filter(|package| package.eq_ignore_ascii_case(namespace)).cloned().collect();
        candidates.extend([
            namespace.to_string(),
            format!("{namespace}{short}"),
            format!("{}_{}", namespace.to_lowercase(), short.to_lowercase()),
        ]);
        // Qt's versionless targets (Qt::Core, which Debian's Qt 6.4 packages link) are defined beside Qt6::Core
        if namespace == "Qt" {
            candidates.extend(["Qt6", "Qt5"].map(|major| format!("{major}{short}")));
        }
        candidates.extend(named);

        for candidate in candidates {
            if self.read_named(&candidate) && self.has_target(name) {
                return true;
            }
        }

        false
    }

    fn has_target(&self, name: &str) -> bool {
        self.targets.iter().any(|target| target.name == name)
    }

    /// The targets the dependency's users link: those `targets` names, else its components' (a target
    /// `<namespace>::<component>`, else the libraries of package `<Package><Component>` beside it, as Qt has them), else
    /// the package's libraries (all its targets when it has none).
    fn wanted_targets(&mut self, request: &Request, main: usize) -> Result<Vec<String>> {
        let dependency = request.dependency;
        let package_name = self.packages[main].name.clone();

        // Named
        if let Some(names) = request.targets {
            for name in names {
                if !self.has_target(name) && !self.read_package_of(name) {
                    bail!(
                        "dependency `{dependency}`: CMake package {package_name} ({}) has no target `{name}`; its targets: {}",
                        slash(&self.packages[main].folder),
                        self.packages[main].targets.join(", ")
                    );
                }
            }
            return Ok(names.to_vec());
        }

        // Components
        if !request.components.is_empty() {
            let mut wanted = Vec::new();
            for component in request.components {
                let targets = self.component_targets(main, component);
                if targets.is_empty() {
                    bail!("{}", self.component_message(dependency, main, component));
                }
                for target in targets {
                    if !wanted.contains(&target) {
                        wanted.push(target);
                    }
                }
            }
            return Ok(wanted);
        }

        // Its libraries
        let wanted = self.libraries_of(main);
        if wanted.is_empty() {
            bail!(
                "dependency `{dependency}`: {} defines no imported targets Anvil can read (a package that only sets variables, \
                 such as {package_name}_LIBRARIES, needs CMake to run it); describe it with `include` and `libs` instead",
                slash(&self.packages[main].folder)
            );
        }

        Ok(wanted)
    }

    /// The targets of a component: `<namespace>::<component>` of the package's own (exact case first), else the
    /// libraries of package `<Package><Component>` or `<package>_<component>` found beside it.
    fn component_targets(&mut self, main: usize, component: &str) -> Vec<String> {
        let own = &self.packages[main].targets;
        let suffix = format!("::{component}");
        let exact = own.iter().find(|name| name.ends_with(&suffix));
        let any_case = own.iter().find(|name| name.to_lowercase().ends_with(&suffix.to_lowercase()));
        if let Some(target) = exact.or(any_case) {
            return vec![target.clone()];
        }

        let name = self.packages[main].name.clone();
        for candidate in [
            format!("{name}{component}"),
            format!("{}_{}", name.to_lowercase(), component.to_lowercase()),
        ] {
            let before = self.packages.len();
            if self.read_named(&candidate) {
                return self.libraries_of(before);
            }
        }

        Vec::new()
    }

    /// A package's targets that have a library file, all of them when none has.
    fn libraries_of(&self, package: usize) -> Vec<String> {
        let names = &self.packages[package].targets;
        let with_library: Vec<String> = names
            .iter()
            .filter(|name| self.targets.iter().any(|target| &target.name == *name && target.has_library))
            .cloned()
            .collect();

        if with_library.is_empty() {
            names.clone()
        } else {
            with_library
        }
    }

    /// Why a component was not found, and what the package has.
    fn component_message(&self, dependency: &str, main: usize, component: &str) -> String {
        let package = &self.packages[main];
        let name = &package.name;

        // Packages beside it named after it (Qt6Core, boost_filesystem), and its own targets
        let mut available: Vec<String> = Vec::new();
        if let Some(Ok(entries)) = package.folder.parent().map(std::fs::read_dir) {
            for entry in entries.flatten() {
                let folder = entry.file_name().to_string_lossy().into_owned();
                let rest = folder
                    .get(name.len()..)
                    .filter(|_| folder.to_lowercase().starts_with(&name.to_lowercase()));
                if let Some(rest) = rest.map(|rest| rest.trim_start_matches('_')).filter(|rest| !rest.is_empty()) {
                    available.push(rest.to_string());
                }
            }
        }
        available.sort();
        available.extend(
            package
                .targets
                .iter()
                .filter_map(|target| target.split_once("::"))
                .map(|(_, short)| short.to_string()),
        );
        available.dedup();

        format!(
            "dependency `{dependency}`: CMake package {name} ({}) has no component `{component}`: no target {name}::{component}, and no package \
             {name}{component} beside it{}",
            slash(&package.folder),
            match available.is_empty() {
                true => String::new(),
                false => format!("; it has {}", available.join(", ")),
            }
        )
    }

    /// What linking `wanted` gives its users: what the targets require, through the targets they name (read from
    /// their packages as they are named), their libraries in link order, and the DLLs of the shared ones.
    fn usage(&mut self, dependency: &str, wanted: &[String]) -> Usage {
        let mut requirements = Requirements::default();
        let mut compiled: HashSet<String> = HashSet::new();
        let mut linked: HashSet<String> = HashSet::new();
        let mut tried: HashSet<String> = HashSet::new();

        // The targets named, and theirs, until no more are named; a target no package read defines is looked for
        loop {
            let before = (compiled.len(), linked.len(), self.targets.len());
            let known: HashSet<String> = self.targets.iter().map(|target| target.name.clone()).collect();
            let known: HashSet<&str> = known.iter().map(String::as_str).collect();
            for target in &self.targets {
                let named = wanted.contains(&target.name);
                if !compiled.contains(&target.name) && (named || requirements.visible.contains(&target.name)) {
                    compiled.insert(target.name.clone());
                    requirements.add_compile(target, &self.build);
                }
                if !linked.contains(&target.name) && (named || requirements.linked.contains(&target.name)) {
                    linked.insert(target.name.clone());
                    requirements.add_link(target, &known, None);
                }
            }

            // In a fixed order (the sets have none): the order packages are read in is the order of what users get
            let mut unknown: Vec<String> = requirements
                .visible
                .iter()
                .chain(&requirements.linked)
                .filter(|name| name.contains("::") && !known.contains(name.as_str()) && !tried.contains(*name))
                .cloned()
                .collect();
            unknown.sort();
            unknown.dedup();
            for name in unknown {
                tried.insert(name.clone());
                self.read_package_of(&name);
            }
            if (compiled.len(), linked.len(), self.targets.len()) == before {
                break;
            }
        }

        // The targets of CMake's find modules they name (Threads::Threads, OpenGL::GL)
        let mut usage = Usage::default();
        let mut named: Vec<String> = requirements.visible.union(&requirements.linked).cloned().collect();
        named.sort();
        for name in &named {
            if self.has_target(name) {
                continue;
            }
            let Some(module) = vcpkg_tree::module_target(name, &self.build) else {
                continue;
            };
            if requirements.visible.contains(name) {
                requirements.add_compile(&module, &self.build);
            }
            if requirements.linked.contains(name) {
                requirements.add_link(&module, &HashSet::new(), None);
                vcpkg_tree::push_new(&mut usage.runtime, &module.runtime);
            }
        }

        // Their files, dependents before dependencies, and the shared libraries those load
        let order = self.link_order(wanted, &linked);
        let mut mismatched = Vec::new();
        let mut shared = Vec::new();
        for name in &order {
            let target = self.target(name).unwrap();
            let Some(files) = pick_files(target, self.build.configuration(), self.debug) else {
                continue;
            };
            if self.build.is_msvc_abi() && self.debug != (files.configuration == "DEBUG") && !files.configuration.is_empty() {
                mismatched.push(format!("{name} ({})", files.configuration));
            }
            self.add_files(target, files, &mut usage, &mut shared);
        }
        self.add_dependent_libraries(shared, &mut usage);

        // The C runtime of MSVC libraries must match the build's
        if !mismatched.is_empty() {
            let (runtime, wanted_configuration) = if self.debug { ("debug", "Debug") } else { ("release", "a release") };
            crate::ui::warn(&format!(
                "dependency `{dependency}`: this build uses MSVC's {runtime} C runtime, and the package has no {wanted_configuration} configuration of {}: \
                 linking it with this runtime may fail (LNK2038) or corrupt the heap; install that configuration of the package, \
                 or build with a profile whose `debug-runtime` matches",
                mismatched.join(", ")
            ));
        }

        // What the targets require, then the rpaths
        let rpaths = std::mem::take(&mut usage.link_flags);
        requirements.fill(&mut usage, &self.build);
        usage.link_flags.extend(rpaths);
        if self.build.os() != Os::Windows {
            usage.includes.retain(|folder| {
                let folder = slash(folder);
                let multiarch = library_architecture(&self.platform).map(|architecture| format!("/usr/include/{architecture}"));
                !IMPLICIT_INCLUDE_FOLDERS.contains(&folder.as_str()) && multiarch.as_deref() != Some(folder.as_str())
            });
        }

        usage
    }

    /// The targets linked, each after every target that links it.
    fn link_order(&self, wanted: &[String], linked: &HashSet<String>) -> Vec<String> {
        let mut order = Vec::new();
        let mut visited = HashSet::new();
        for name in wanted {
            self.visit(name, linked, &mut visited, &mut order);
        }

        order.reverse();
        order
    }

    fn visit(&self, name: &str, linked: &HashSet<String>, visited: &mut HashSet<String>, order: &mut Vec<String>) {
        if !visited.insert(name.to_string()) {
            return;
        }

        if let Some(target) = self.target(name) {
            for item in target.link_items.iter().filter(|item| linked.contains(*item)) {
                self.visit(item, linked, visited, order);
            }
        }

        order.push(name.to_string());
    }

    fn target(&self, name: &str) -> Option<&ImportedTarget> {
        self.targets.iter().find(|target| target.name == name)
    }

    /// A target's files: its library linked (a shared one's import library on Windows, with the DLL copied beside the
    /// programs; elsewhere the shared library, with an rpath to it outside the system's folders).
    fn add_files(&self, target: &ImportedTarget, files: &ImportedFiles, usage: &mut Usage, shared: &mut Vec<String>) {
        let windows = self.build.os() == Os::Windows;
        let is_dll = |path: &PathBuf| path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("dll"));

        match target.kind.as_str() {
            "INTERFACE" | "MODULE" | "OBJECT" => {}
            _ if windows && (target.kind == "SHARED" || files.location.as_ref().is_some_and(is_dll)) => {
                vcpkg_tree::push_new(&mut usage.libs, files.import_library.as_slice());
                if let Some(dll) = files.location.as_ref().filter(|location| is_dll(location)) {
                    vcpkg_tree::push_new(&mut usage.runtime, std::slice::from_ref(dll));
                }
                shared.extend(files.dependent_libraries.iter().cloned());
            }
            _ => {
                let Some(library) = files.location.as_ref().or(files.import_library.as_ref()) else {
                    return;
                };
                vcpkg_tree::push_new(&mut usage.libs, std::slice::from_ref(library));
                if target.kind == "SHARED" {
                    self.add_rpath(library, usage);
                    shared.extend(files.dependent_libraries.iter().cloned());
                }

                // A static library is C++ unless the package says it is only C
                if target.kind != "SHARED" && vcpkg_tree::is_static_library(library) {
                    usage.cxx_libraries |= files
                        .languages
                        .as_ref()
                        .is_none_or(|languages| languages.iter().any(|language| language == "CXX"));
                }
            }
        }
    }

    /// The shared libraries the linked ones load without passing them on (IMPORTED_LINK_DEPENDENT_LIBRARIES), and
    /// theirs: their DLLs copied beside the programs, or rpaths to them.
    fn add_dependent_libraries(&mut self, mut pending: Vec<String>, usage: &mut Usage) {
        let mut done = HashSet::new();
        while let Some(name) = pending.pop() {
            if !done.insert(name.clone()) || (!self.has_target(&name) && !self.read_package_of(&name)) {
                continue;
            }
            let target = self.target(&name).unwrap();
            let Some(files) = pick_files(target, self.build.configuration(), self.debug) else {
                continue;
            };
            let Some(location) = &files.location else { continue };

            if self.build.os() == Os::Windows {
                vcpkg_tree::push_new(&mut usage.runtime, std::slice::from_ref(location));
            } else {
                self.add_rpath(location, usage);
            }
            pending.extend(files.dependent_libraries.iter().cloned());
        }
    }

    /// `-Wl,-rpath,<folder>` for a shared library outside the folders the system's loader searches.
    fn add_rpath(&self, library: &Path, usage: &mut Usage) {
        let Some(folder) = library.parent() else { return };
        let folder_text = slash(folder);
        let system = SYSTEM_LIBRARY_FOLDERS.iter().any(|system| {
            folder_text == *system
                || folder_text
                    .strip_prefix(system)
                    .is_some_and(|rest| rest.starts_with('/') && !rest[1..].contains('/'))
        });
        if !system {
            vcpkg_tree::push_new(&mut usage.link_flags, &[format!("-Wl,-rpath,{folder_text}")]);
        }
    }
}

/// The configuration of a target's files a build links, of those whose files are there: its own, else the unsuffixed
/// files, else a debug or release one as the build links, else the first (CMake's choice, but never a debug
/// configuration for a release build when a release one is there).
fn pick_files<'a>(target: &'a ImportedTarget, configuration: &str, debug: bool) -> Option<&'a ImportedFiles> {
    let present = |file: &Option<PathBuf>| file.as_ref().is_some_and(|file| file.is_file());
    let usable: Vec<&ImportedFiles> = target
        .files
        .iter()
        .filter(|files| present(&files.location) || present(&files.import_library))
        .collect();
    let named = |wanted: &str| usable.iter().copied().find(|files| files.configuration == wanted);

    let preferred: &[&str] = if debug { &["DEBUG"] } else { RELEASE_CONFIGURATIONS };
    named(&configuration.to_ascii_uppercase())
        .or_else(|| named(""))
        .or_else(|| preferred.iter().find_map(|wanted| named(wanted)))
        .or_else(|| usable.first().copied())
}

/// Where CMake's search looks when `prefix` is not given: `<name>_DIR`, `<name>_ROOT`, `CMAKE_PREFIX_PATH`, the
/// prefixes of the folders on PATH (bin/ and sbin/ stand for their parent), then the system's.
fn default_places(name: &str, platform: &Platform) -> Vec<(Place, String)> {
    let separator = if cfg!(windows) { ';' } else { ':' };
    let list = |variable: &str| -> Vec<PathBuf> {
        std::env::var(variable)
            .unwrap_or_default()
            .split(separator)
            .filter(|entry| !entry.trim().is_empty())
            .map(PathBuf::from)
            .collect()
    };
    let mut places = Vec::new();

    // The user's
    for folder in list(&format!("{name}_DIR")) {
        places.push((Place::Folder(folder), format!("{name}_DIR")));
    }
    let mut roots = vec![format!("{name}_ROOT")];
    if name.to_uppercase() != name {
        roots.push(format!("{}_ROOT", name.to_uppercase()));
    }
    for variable in roots.iter().chain(&["CMAKE_PREFIX_PATH".to_string()]) {
        for prefix in list(variable) {
            places.push((Place::Prefix(prefix), variable.clone()));
        }
    }

    // PATH's
    for folder in std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default()
    {
        let last = folder.file_name().map(|last| last.to_string_lossy().to_lowercase()).unwrap_or_default();
        let prefix = match (last.as_str(), folder.parent()) {
            ("bin" | "sbin", Some(parent)) => parent.to_path_buf(),
            _ => folder,
        };
        places.push((Place::Prefix(prefix), "PATH".to_string()));
    }

    // The system's
    let system: Vec<PathBuf> = match platform.os {
        Os::Windows => ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"]
            .iter()
            .filter_map(|variable| std::env::var_os(variable).map(PathBuf::from))
            .collect(),
        Os::Linux => ["/usr/local", "/usr", "/", "/opt"].iter().map(PathBuf::from).collect(),
        Os::Macos => ["/opt/homebrew", "/usr/local", "/opt/local", "/usr", "/", "/opt"]
            .iter()
            .map(PathBuf::from)
            .collect(),
    };
    for prefix in system {
        places.push((Place::Prefix(prefix), "system".to_string()));
    }

    // Each once
    let mut seen = HashSet::new();
    places.retain(|(place, _)| {
        let (Place::Prefix(path) | Place::Folder(path)) = place;
        seen.insert(util::path_key(path))
    });

    places
}

/// The folders under `prefix` where find_package(CONFIG) looks for package `name`'s config file, in its order:
/// `<prefix>/`, `(cmake|CMake)/`, `<name>*/` and below it, `(lib/<arch>|lib64|lib32|lib|share)/cmake/<name>*/` and
/// the like ("<name>*" matching any case).
fn config_folders(prefix: &Path, name: &str, platform: &Platform) -> Vec<PathBuf> {
    let cmake = |folder: &Path| -> Vec<PathBuf> { subfolders(folder, &["cmake", "CMake"]) };
    let libraries = |folder: &Path| -> Vec<PathBuf> {
        let mut names = Vec::new();
        if let Some(architecture) = library_architecture(platform) {
            names.push(format!("lib/{architecture}"));
        }
        names.extend(
            match pointer_size(platform.cpu) {
                8 => ["lib64", "lib", "share"],
                _ => ["lib32", "lib", "share"],
            }
            .map(String::from),
        );
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        subfolders(folder, &names)
    };
    let mut folders = vec![prefix.to_path_buf()];

    // Windows-style: the prefix, its cmake/ folder, and the package's folder
    folders.extend(cmake(prefix));
    let named = named_folders(prefix, name);
    for folder in &named {
        folders.push(folder.clone());
        for cmake_folder in cmake(folder) {
            folders.push(cmake_folder.clone());
            folders.extend(named_folders(&cmake_folder, name));
        }
    }

    // Unix-style: the library and share folders, under the prefix, then under the package's folder
    for base in std::iter::once(prefix.to_path_buf()).chain(named) {
        for library_folder in libraries(&base) {
            for cmake_folder in subfolders(&library_folder, &["cmake"]) {
                folders.extend(named_folders(&cmake_folder, name));
            }
            for folder in named_folders(&library_folder, name) {
                folders.push(folder.clone());
                folders.extend(cmake(&folder));
            }
        }
    }

    // Each once (cmake and CMake are one folder on Windows)
    let mut seen = HashSet::new();
    folders.retain(|folder| seen.insert(util::path_key(folder)));

    folders
}

/// The subfolders of `folder` with these names (relative paths) that exist.
fn subfolders(folder: &Path, names: &[&str]) -> Vec<PathBuf> {
    names
        .iter()
        .map(|name| folder.join(name))
        .filter(|candidate| candidate.is_dir())
        .collect()
}

/// The subfolders of `folder` whose names start with `name`, in any case, sorted.
fn named_folders(folder: &Path, name: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(folder) else { return Vec::new() };
    let lower = name.to_lowercase();
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().to_lowercase().starts_with(&lower))
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    found.sort();

    found
}

/// The config file of package `name` in `folder`: `<name>Config.cmake` or `<lowercase name>-config.cmake`, as
/// written first, else in any case.
fn config_file_in(folder: &Path, name: &str) -> Option<PathBuf> {
    let wanted = [format!("{name}Config.cmake"), format!("{}-config.cmake", name.to_lowercase())];
    let entries: Vec<String> = std::fs::read_dir(folder)
        .ok()?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();

    let exact = entries.iter().find(|entry| wanted.contains(entry));
    let any_case = entries.iter().find(|entry| wanted.iter().any(|file| file.eq_ignore_ascii_case(entry)));
    exact.or(any_case).map(|entry| folder.join(entry)).filter(|file| file.is_file())
}

/// A config file and the files of its folder it includes, directly or through others: `include()` and `file(GLOB)`
/// of `${CMAKE_CURRENT_LIST_DIR}/...` (a variable in the name matching anything).
fn included_files(config_file: &Path) -> Vec<PathBuf> {
    static REFERENCE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"\$\{(?:CMAKE_CURRENT_LIST_DIR|_DIR|_IMPORT_DIR)\}/([^"\s)/]+)["\s)]"#).unwrap());
    static VARIABLE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"\$\{([^}]*)\}").unwrap());
    static SET: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"set\(\s*(\w+)\s+"?([^"\s)$]+)"?\s*\)"#).unwrap());

    let folder = config_file.parent().unwrap();
    let names: Vec<String> = std::fs::read_dir(folder)
        .map(|entries| entries.flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    let mut files = vec![config_file.to_path_buf()];

    let mut next = 0;
    while next < files.len() {
        // A name made of variables the file sets (`include("${CMAKE_CURRENT_LIST_DIR}/${targets_file}")`); one it
        // does not set matches anything, but not on its own
        let text = std::fs::read_to_string(&files[next]).unwrap_or_default();
        let variables: Vec<(String, String)> = SET
            .captures_iter(&text)
            .map(|captures| (captures[1].to_string(), captures[2].to_string()))
            .collect();
        for captures in REFERENCE.captures_iter(&text) {
            let pattern = VARIABLE.replace_all(&captures[1], |variable: &regex::Captures| {
                let value = variables.iter().find(|(name, _)| *name == variable[1]).map(|(_, value)| value.clone());
                value.unwrap_or_else(|| "*".to_string())
            });
            if pattern.replace('*', "").len() < 3 {
                continue;
            }
            let cmake_files = names.iter().filter(|name| name.ends_with(".cmake"));
            for name in cmake_files.filter(|name| wildcard_match(&pattern, name)) {
                let file = folder.join(name);
                if !files.contains(&file) {
                    files.push(file);
                }
            }
        }
        next += 1;
    }

    files
}

/// Whether `name` matches `pattern`, whose `*` matches any run of characters (in any case on Windows).
fn wildcard_match(pattern: &str, name: &str) -> bool {
    let (pattern, name) = match cfg!(windows) {
        true => (pattern.to_lowercase(), name.to_lowercase()),
        false => (pattern.to_string(), name.to_string()),
    };
    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, rest) = parts.split_first().unwrap();
    let Some(mut remaining) = name.strip_prefix(first) else { return false };

    for (index, part) in rest.iter().enumerate() {
        if index == rest.len() - 1 {
            return remaining.ends_with(part);
        }
        match remaining.find(part) {
            Some(found) => remaining = &remaining[found + part.len()..],
            None => return false,
        }
    }

    remaining.is_empty()
}

/// The packages a package file looks for, `find_dependency(<name> ...)` and `find_package(<name> ...)`, with the
/// quoted folders the call gives as HINTS or PATHS.
fn named_packages(text: &str) -> Vec<(String, Vec<String>)> {
    static FIND: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\b(?:find_dependency|find_package)\(\s*([A-Za-z0-9_.+-]+)([^)]*)\)").unwrap());
    static HINTS: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"\b(?:HINTS|PATHS)((?:\s+"[^"$]+")+)"#).unwrap());

    FIND.captures_iter(text)
        .map(|captures| {
            let hints = HINTS
                .captures_iter(&captures[2])
                .flat_map(|hint| {
                    hint[1]
                        .split('"')
                        .map(str::trim)
                        .filter(|path| !path.is_empty())
                        .map(String::from)
                        .collect::<Vec<_>>()
                })
                .collect();
            (captures[1].to_string(), hints)
        })
        .collect()
}

/// The version file beside a config file (`<Name>ConfigVersion.cmake`, `<name>-config-version.cmake`), with the files
/// of its folder it includes (Qt's ConfigVersionImpl).
fn read_version_file(config_file: &Path) -> Option<VersionFile> {
    static VERSION: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"set\(\s*PACKAGE_VERSION\s+"?([^"\s)]+)"?\s*\)"#).unwrap());
    static POINTER_SIZE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"CMAKE_SIZEOF_VOID_P\}?"?\s+STREQUAL\s+"(\d+)""#).unwrap());

    let file_name = config_file.file_name()?.to_string_lossy().into_owned();
    let version_name = match file_name.strip_suffix(".cmake")? {
        stem if stem.ends_with("-config") => format!("{stem}-version.cmake"),
        stem => format!("{stem}Version.cmake"),
    };
    let version_file = config_file_in_any_case(config_file.parent()?, &version_name)?;
    let text: String = included_files(&version_file)
        .iter()
        .filter_map(|file| std::fs::read_to_string(file).ok())
        .collect::<Vec<_>>()
        .join("\n");

    let compatibility = if text.contains("PACKAGE_FIND_VERSION_MINOR") {
        Compatibility::SameMinor
    } else if text.contains("PACKAGE_FIND_VERSION_MAJOR") {
        Compatibility::SameMajor
    } else if text.contains("CVF_VERSION_NO_TWEAK") {
        Compatibility::Exact
    } else {
        Compatibility::AnyNewer
    };

    Some(VersionFile {
        version: VERSION.captures(&text).map(|captures| captures[1].to_string()),
        compatibility,
        pointer_size: POINTER_SIZE.captures(&text).and_then(|captures| captures[1].parse().ok()),
    })
}

/// A file of this name in `folder`, in any case.
fn config_file_in_any_case(folder: &Path, name: &str) -> Option<PathBuf> {
    std::fs::read_dir(folder)
        .ok()?
        .flatten()
        .find(|entry| entry.file_name().to_string_lossy().eq_ignore_ascii_case(name))
        .map(|entry| entry.path())
}

/// Whether a package with this version file suits a build for `pointer_size`-byte pointers asking for `wanted`; else
/// why not.
fn judge(version_file: Option<&VersionFile>, wanted: Option<&[u64]>, pointer_size: u32) -> std::result::Result<(), String> {
    if let Some(built_for) = version_file.and_then(|file| file.pointer_size) {
        if built_for != pointer_size {
            return Err(format!("built for {}-bit programs", built_for * 8));
        }
    }

    let Some(wanted) = wanted else { return Ok(()) };
    let Some(version_file) = version_file else {
        return Err("it has no version file, so its version is unknown".to_string());
    };
    let Some(version) = version_file.version.as_deref() else {
        return Err("its version file sets no PACKAGE_VERSION".to_string());
    };
    if compatible(version, wanted, version_file.compatibility) {
        return Ok(());
    }

    let rule = match version_file.compatibility {
        Compatibility::AnyNewer => "it takes requests for its version or older",
        Compatibility::SameMajor => "it takes requests for the same major version, up to its own",
        Compatibility::SameMinor => "it takes requests for the same major and minor version, up to its own",
        Compatibility::Exact => "it takes requests for exactly its version",
    };
    let wanted_text: Vec<String> = wanted.iter().map(u64::to_string).collect();
    Err(format!("version {version}, and {} was asked for: {rule}", wanted_text.join(".")))
}

/// Whether a package of `version` answers a request for `wanted` under its version file's rule.
fn compatible(version: &str, wanted: &[u64], compatibility: Compatibility) -> bool {
    // The leading numbers of the version ("3.4.0-rc1" is 3.4.0)
    let numbers: Vec<u64> = version.split('.').map_while(|part| part.parse().ok()).collect();
    let component = |list: &[u64], index: usize| list.get(index).copied().unwrap_or(0);
    let less = (0..4)
        .map(|index| (component(&numbers, index), component(wanted, index)))
        .find(|(have, want)| have != want)
        .is_some_and(|(have, want)| have < want);

    match compatibility {
        Compatibility::AnyNewer => !less,
        Compatibility::SameMajor => !less && component(&numbers, 0) == component(wanted, 0),
        Compatibility::SameMinor => !less && (0..2).all(|index| component(&numbers, index) == component(wanted, index)),
        Compatibility::Exact => (0..3).all(|index| component(&numbers, index) == component(wanted, index)),
    }
}

fn pointer_size(cpu: Cpu) -> u32 {
    match cpu {
        Cpu::X64 | Cpu::Arm64 => 8,
        Cpu::X86 | Cpu::Arm => 4,
    }
}

/// Debian's multiarch folder name for the machine (lib/x86_64-linux-gnu), on Linux.
fn library_architecture(platform: &Platform) -> Option<&'static str> {
    if platform.os != Os::Linux {
        return None;
    }

    Some(match platform.cpu {
        Cpu::X64 => "x86_64-linux-gnu",
        Cpu::X86 => "i386-linux-gnu",
        Cpu::Arm64 => "aarch64-linux-gnu",
        Cpu::Arm => "arm-linux-gnueabihf",
    })
}

/// The package was not found: where Anvil looked, what it found and turned down, and how to point it at the package.
fn not_found_message(request: &Request, places: &[(Place, String)], rejected: &[String]) -> String {
    let name = request.package;
    let dependency = request.dependency;
    let lower = name.to_lowercase();
    let mut message = match rejected.is_empty() {
        true => format!("dependency `{dependency}`: CMake package `{name}` not found (no {name}Config.cmake or {lower}-config.cmake)"),
        false => format!("dependency `{dependency}`: no CMake package `{name}` that suits this build"),
    };

    // Where: each place the user set, the system's, and how many came from PATH
    let mut looked = Vec::new();
    let mut path_count = 0;
    for (place, origin) in places {
        let (Place::Prefix(path) | Place::Folder(path)) = place;
        match origin.as_str() {
            "PATH" => path_count += 1,
            _ => looked.push(format!("{} ({origin})", slash(path))),
        }
    }
    if path_count > 0 {
        looked.push(format!("the prefixes of the {path_count} folders on PATH"));
    }
    message.push_str(&format!("; looked in {}", looked.join(", ")));

    // The variables that would have pointed at it
    let variables = [format!("{name}_DIR"), format!("{name}_ROOT"), "CMAKE_PREFIX_PATH".to_string()];
    let unset: Vec<&String> = variables.iter().filter(|variable| std::env::var_os(variable).is_none()).collect();
    if request.prefixes.is_empty() && !unset.is_empty() {
        let names: Vec<&str> = unset.iter().map(|variable| variable.as_str()).collect();
        message.push_str(&format!(" ({} not set)", names.join(", ")));
    }

    // Found, but not suitable
    for rejection in rejected {
        message.push_str(&format!("\n  found {rejection}"));
    }

    message.push_str(&format!(
        "\nhint: point at its install folder (the one holding lib/cmake/{name} or {name}Config.cmake): \
         `{dependency} = {{ cmake = \"{name}\", prefix = \"...\" }}` in [deps], or the environment variable {name}_ROOT, or {name}_DIR \
         for the folder of its config file"
    ));

    message
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platform(os: Os) -> Platform {
        Platform {
            cpu: Cpu::X64,
            os,
            triple: None,
        }
    }

    /// A fresh folder for one test.
    fn scratch(test: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("anvil-cmake-package-{test}-{}", std::process::id()));
        std::fs::remove_dir_all(&folder).ok();
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("1.2.3"), Some(vec![1, 2, 3]));
        assert_eq!(parse_version("6"), Some(vec![6]));
        assert_eq!(parse_version("1.2.3.4.5"), None);
        assert_eq!(parse_version("1.x"), None);
        assert_eq!(parse_version("1.2...2.0"), None);

        // Each rule of write_basic_package_version_file
        assert!(compatible("1.4.2", &[1, 2], Compatibility::AnyNewer));
        assert!(compatible("2.0", &[1, 2], Compatibility::AnyNewer));
        assert!(!compatible("1.1", &[1, 2], Compatibility::AnyNewer));
        assert!(compatible("1.4.2", &[1, 2], Compatibility::SameMajor));
        assert!(!compatible("2.0", &[1, 2], Compatibility::SameMajor));
        assert!(!compatible("1.4.2", &[1, 2], Compatibility::SameMinor));
        assert!(compatible("1.2.9", &[1, 2], Compatibility::SameMinor));
        assert!(compatible("1.2.0.7", &[1, 2], Compatibility::Exact));
        assert!(!compatible("1.2.1", &[1, 2], Compatibility::Exact));
        assert!(compatible("3.4.0-rc1", &[3, 4], Compatibility::SameMajor));
    }

    #[test]
    fn reads_version_files() {
        let folder = scratch("version");
        let config = folder.join("FooConfig.cmake");
        write(&config, "");
        write(
            &folder.join("FooConfigVersion.cmake"),
            "set(PACKAGE_VERSION \"2.5.1\")\nif(PACKAGE_FIND_VERSION_MAJOR STREQUAL CVF_VERSION_MAJOR)\nendif()\n\
             if(\"${CMAKE_SIZEOF_VOID_P}\" STREQUAL \"\" OR \"8\" STREQUAL \"\")\nendif()\nif(NOT CMAKE_SIZEOF_VOID_P STREQUAL \"8\")\nendif()\n",
        );
        let version_file = read_version_file(&config).unwrap();
        assert_eq!(
            version_file,
            VersionFile {
                version: Some("2.5.1".into()),
                compatibility: Compatibility::SameMajor,
                pointer_size: Some(8),
            }
        );
        assert!(judge(Some(&version_file), Some(&[2, 1]), 8).is_ok());
        assert!(judge(Some(&version_file), Some(&[3]), 8).unwrap_err().contains("same major version"));
        assert!(judge(Some(&version_file), None, 4).unwrap_err().contains("64-bit"));
        assert!(judge(None, Some(&[1]), 8).unwrap_err().contains("no version file"));

        // Qt's: the version in a file the version file includes; lower-case names
        let qt = folder.join("qt");
        write(&qt.join("bar-config.cmake"), "");
        write(
            &qt.join("bar-config-version.cmake"),
            "include(\"${CMAKE_CURRENT_LIST_DIR}/bar-config-version-impl.cmake\")\n",
        );
        write(&qt.join("bar-config-version-impl.cmake"), "set(PACKAGE_VERSION 6.4.2)\n");
        let version_file = read_version_file(&qt.join("bar-config.cmake")).unwrap();
        assert_eq!(
            (version_file.version.as_deref(), version_file.compatibility),
            (Some("6.4.2"), Compatibility::AnyNewer)
        );

        std::fs::remove_dir_all(&folder).ok();
    }

    #[test]
    fn searches_as_find_package_does() {
        let folder = scratch("search");
        let unix = platform(Os::Linux);

        // The prefix itself, then cmake/, then <name>*/..., then lib/<arch>/cmake/<name>*/, lib/cmake/..., share/...
        write(&folder.join("lib/cmake/Foo-1.2/FooConfig.cmake"), "");
        write(&folder.join("lib/x86_64-linux-gnu/cmake/foo/foo-config.cmake"), "");
        write(&folder.join("share/foo/cmake/FooConfig.cmake"), "");
        write(&folder.join("Foo/cmake/FooConfig.cmake"), "");
        let found: Vec<PathBuf> = config_folders(&folder, "Foo", &unix)
            .iter()
            .filter_map(|folder| config_file_in(folder, "Foo"))
            .collect();
        let relative: Vec<String> = found.iter().map(|file| util::rel(file, &folder)).collect();
        assert_eq!(
            relative,
            vec![
                "Foo/cmake/FooConfig.cmake",
                "lib/x86_64-linux-gnu/cmake/foo/foo-config.cmake",
                "lib/cmake/Foo-1.2/FooConfig.cmake",
                "share/foo/cmake/FooConfig.cmake",
            ]
        );

        // Any case finds it; an unsuitable one is passed over with the reason
        assert!(config_file_in(&folder.join("Foo/cmake"), "FOO").is_some());
        write(&folder.join("Foo/cmake/FooConfigVersion.cmake"), "set(PACKAGE_VERSION \"1.0\")\n");
        let reader = Reader {
            build: test_build(),
            debug: false,
            platform: unix.clone(),
            packages: Vec::new(),
            targets: Vec::new(),
            searched: HashSet::new(),
        };
        let places = vec![(Place::Prefix(folder.clone()), "`prefix`".to_string())];
        let located = reader.locate("Foo", &places, Some(&[1])).ok().unwrap();
        assert_eq!(located.config_file, folder.join("Foo/cmake/FooConfig.cmake"));
        let rejected = reader.locate("Foo", &places, Some(&[2])).err().unwrap();
        assert_eq!(rejected.len(), 4);
        assert!(rejected[0].contains("version 1.0, and 2 was asked for"), "{rejected:?}");
        assert!(rejected[1].contains("no version file"), "{rejected:?}");

        std::fs::remove_dir_all(&folder).ok();
    }

    fn test_build() -> BuildContext {
        let toolchain = Toolchain::for_tests(crate::toolchain::Family::Gcc, platform(Os::Linux), false);
        BuildContext::new(&toolchain, &Profile::resolve("release", &Default::default()).unwrap())
    }

    #[test]
    fn matches_included_file_names() {
        assert!(wildcard_match("fmt-targets-*.cmake", "fmt-targets-release.cmake"));
        assert!(!wildcard_match("fmt-targets-*.cmake", "fmt-targets.cmake"));
        assert!(wildcard_match("*CoreTargets.cmake", "Qt6CoreTargets.cmake"));
        assert!(wildcard_match("a*b*c.cmake", "aXbYc.cmake"));
        assert!(!wildcard_match("a*b*c.cmake", "aXc.cmake"));
    }

    #[test]
    fn picks_the_build_configuration() {
        let folder = scratch("pick");
        let files = |configuration: &str| {
            let location = folder.join(format!("{configuration}.lib"));
            write(&location, "");
            ImportedFiles {
                configuration: configuration.into(),
                location: Some(location),
                ..Default::default()
            }
        };
        let missing = ImportedFiles {
            configuration: "RELEASE_RUNTIME_NOT_INSTALLED".into(),
            location: Some(folder.join("missing.lib")),
            ..Default::default()
        };
        let mut target = ImportedTarget::default();
        target.files = vec![missing.clone(), files("DEBUG"), files("RELWITHDEBINFO"), files("RELEASE")];
        assert_eq!(pick_files(&target, "Debug", true).unwrap().configuration, "DEBUG");
        assert_eq!(pick_files(&target, "MinSizeRel", false).unwrap().configuration, "RELEASE");
        assert_eq!(pick_files(&target, "RelWithDebInfo", false).unwrap().configuration, "RELWITHDEBINFO");

        // Only a debug one: CMake's choice, the first whose files are there
        target.files = vec![missing, files("DEBUG")];
        assert_eq!(pick_files(&target, "Release", false).unwrap().configuration, "DEBUG");

        std::fs::remove_dir_all(&folder).ok();
    }

    #[test]
    fn reads_an_installed_package() {
        let prefix = scratch("read");

        // A CMake-generated package: config, targets and per-configuration files, and a dependency beside it
        let folder = prefix.join("lib/cmake/Foo");
        write(
            &folder.join("FooConfig.cmake"),
            "include(CMakeFindDependencyMacro)\nfind_dependency(Bar)\ninclude(\"${CMAKE_CURRENT_LIST_DIR}/FooTargets.cmake\")\n",
        );
        write(
            &folder.join("FooTargets.cmake"),
            "get_filename_component(_IMPORT_PREFIX \"${CMAKE_CURRENT_LIST_FILE}\" PATH)\n\
             get_filename_component(_IMPORT_PREFIX \"${_IMPORT_PREFIX}\" PATH)\n\
             get_filename_component(_IMPORT_PREFIX \"${_IMPORT_PREFIX}\" PATH)\n\
             get_filename_component(_IMPORT_PREFIX \"${_IMPORT_PREFIX}\" PATH)\n\
             add_library(Foo::core STATIC IMPORTED)\nadd_library(Foo::gui SHARED IMPORTED)\n\
             set_target_properties(Foo::core PROPERTIES\n  INTERFACE_INCLUDE_DIRECTORIES \"${_IMPORT_PREFIX}/include\"\n  \
             INTERFACE_COMPILE_DEFINITIONS \"FOO\"\n  INTERFACE_LINK_LIBRARIES \"Bar::bar;Threads::Threads\"\n)\n\
             set_target_properties(Foo::gui PROPERTIES\n  INTERFACE_LINK_LIBRARIES \"Foo::core\"\n)\n\
             file(GLOB _cmake_config_files \"${CMAKE_CURRENT_LIST_DIR}/FooTargets-*.cmake\")\n",
        );
        write(
            &folder.join("FooTargets-release.cmake"),
            "set_property(TARGET Foo::core APPEND PROPERTY IMPORTED_CONFIGURATIONS RELEASE)\n\
             set_target_properties(Foo::core PROPERTIES\n  IMPORTED_LOCATION_RELEASE \"${_IMPORT_PREFIX}/lib/libcore.a\"\n)\n\
             set_target_properties(Foo::gui PROPERTIES\n  IMPORTED_LOCATION_RELEASE \"${_IMPORT_PREFIX}/opt/libgui.so\"\n  IMPORTED_IMPLIB_RELEASE \"${_IMPORT_PREFIX}/lib/gui.lib\"\n)\n",
        );
        write(&folder.join("Unrelated.cmake"), "add_library(Other::other STATIC IMPORTED)\n");
        write(
            &prefix.join("lib/cmake/Bar/bar-config.cmake"),
            "get_filename_component(PACKAGE_PREFIX_DIR \"${CMAKE_CURRENT_LIST_DIR}/../../../\" ABSOLUTE)\n\
             add_library(Bar::bar INTERFACE IMPORTED)\n\
             set_target_properties(Bar::bar PROPERTIES INTERFACE_COMPILE_DEFINITIONS \"BAR\" INTERFACE_LINK_LIBRARIES \"m\")\n",
        );
        std::fs::create_dir_all(prefix.join("include")).unwrap();
        for library in ["lib/libcore.a", "opt/libgui.so", "lib/gui.lib"] {
            write(&prefix.join(library), "");
        }

        let mut reader = Reader {
            build: test_build(),
            debug: false,
            platform: platform(Os::Linux),
            packages: Vec::new(),
            targets: Vec::new(),
            searched: HashSet::new(),
        };
        let places = vec![(Place::Prefix(prefix.clone()), "`prefix`".to_string())];
        let located = reader.locate("Foo", &places, None).ok().unwrap();
        let main = reader.read_package(&located);
        assert_eq!(reader.packages[main].named, vec!["Bar"]);
        assert_eq!(reader.packages[main].targets, vec!["Foo::core", "Foo::gui"]);

        // Its libraries by default, a component by its target, a missing one with what there is
        let request = |components: &'static [String], targets: Option<&'static [String]>| Request {
            dependency: "foo",
            package: "Foo",
            components,
            version: None,
            targets,
            prefixes: &[],
        };
        assert_eq!(reader.wanted_targets(&request(&[], None), main).unwrap(), vec!["Foo::core", "Foo::gui"]);
        let gui = vec!["gui".to_string()].leak();
        assert_eq!(reader.wanted_targets(&request(gui, None), main).unwrap(), vec!["Foo::gui"]);
        let missing = vec!["Net".to_string()].leak();
        let error = reader.wanted_targets(&request(missing, None), main).unwrap_err().to_string();
        assert!(error.contains("has no component `Net`") && error.contains("it has core, gui"), "{error}");
        let wrong = vec!["Foo::nope".to_string()].leak();
        let error = reader.wanted_targets(&request(&[], Some(wrong)), main).unwrap_err().to_string();
        assert!(error.contains("no target `Foo::nope`; its targets: Foo::core, Foo::gui"), "{error}");

        // What the gui target's users get: Bar read for Bar::bar, dependents linked first, an rpath to libgui.so
        let usage = reader.usage("foo", &["Foo::gui".to_string()]);
        assert_eq!(usage.defines, vec!["FOO", "BAR"]);
        assert_eq!(usage.includes, vec![util::normalize(&prefix.join("include"))]);
        let libs: Vec<String> = usage.libs.iter().map(|lib| slash(lib)).collect();
        let base = slash(&util::normalize(&prefix));
        assert_eq!(
            libs,
            vec![format!("{base}/opt/libgui.so"), format!("{base}/lib/libcore.a"), "m".to_string()]
        );
        assert_eq!(usage.link_flags, vec!["-pthread".to_string(), format!("-Wl,-rpath,{base}/opt")]);
        assert_eq!(usage.flags, vec!["-pthread"]);

        std::fs::remove_dir_all(&prefix).ok();
    }
}
