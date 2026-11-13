//! Reads a vcpkg install tree: the `status` database (which packages and features are installed, and what each
//! depends on) and the per-package file lists, to give each package's headers, libraries and DLLs without running
//! CMake.

use crate::flags::Language;
use crate::platform::Os;
use crate::profile::Profile;
use crate::toolchain::{Family, Toolchain};
use crate::util::{self, slash};
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

// The languages compile options are evaluated for, with CMake's names for them
const LANGUAGES: [(Language, &str); 4] = [
    (Language::Cxx, "CXX"),
    (Language::C, "C"),
    (Language::ObjCxx, "OBJCXX"),
    (Language::ObjC, "OBJC"),
];

// Imported targets of CMake's own find modules that packages link, which no package file defines
const MODULE_TARGETS: &[&str] = &["OpenMP::OpenMP_CXX", "OpenMP::OpenMP_C", "Threads::Threads", "OpenGL::GL", "OpenGL::GLU"];

#[derive(Debug, Default)]
pub struct InstalledTree {
    pub root: PathBuf,
    pub triplet: String,
    packages: HashMap<String, Package>,
}

#[derive(Debug, Default)]
struct Package {
    depends: Vec<String>,
    /// "11.0.2", or "1.3.1#2" with a port version.
    version: Option<String>,

    /// Installed files, relative to the install root ("x64-windows/lib/fmt.lib").
    files: Vec<String>,
}

/// An imported target from a package's CMake files.
#[derive(Default)]
struct ImportedTarget {
    name: String,
    has_library: bool,
    includes: Vec<PathBuf>,
    defines: Vec<String>,
    /// Targets its users link too (not those under $<LINK_ONLY:...>).
    links: Vec<String>,
    /// Everything its users link, $<LINK_ONLY:...> too: targets, system libraries and linker flags.
    link_items: Vec<String>,
    /// INTERFACE_COMPILE_OPTIONS as written: evaluated per language.
    compile_options: Vec<String>,
    link_options: Vec<String>,
}

/// What a project needs from one vcpkg dependency (and everything it depends on).
#[derive(Debug, Default, Clone)]
pub struct Usage {
    /// The triplet's include folder, then the subfolders packages export (include/vma, include/libpng16).
    pub includes: Vec<PathBuf>,
    /// Definitions the packages' libraries require of their users (FMT_SHARED for fmt's DLL).
    pub defines: Vec<String>,
    /// Compile options the packages require of their users: for every file, then for one language's files only.
    pub flags: Vec<String>,
    pub language_flags: Vec<(Language, String)>,
    /// The packages' libraries, then the system libraries they link (ws2_32, libomp.lib).
    pub libs: Vec<PathBuf>,
    pub link_flags: Vec<String>,
    pub runtime: Vec<PathBuf>,
}

/// How CMake users find and link an installed port: `find_package(<name> [CONFIG])`, then its targets.
#[derive(Debug, Clone, PartialEq)]
pub struct CmakePackage {
    pub name: String,
    /// find_package needs CONFIG (a config file only), else CMake's own Find module finds it too.
    pub config: bool,
    pub targets: Vec<String>,
}

/// The build a package's generator expressions are evaluated for, in CMake's names (configuration, compiler and
/// platform), and while evaluating, the language of the file compiled and whether it is for linking.
#[derive(Debug, Clone)]
pub struct BuildContext {
    family: Family,
    os: Os,
    msvc_abi: bool,
    configuration: String,
    compiler_id: &'static str,
    frontend: &'static str,
    language: Option<&'static str>,
    link: bool,
}

impl BuildContext {
    pub fn new(toolchain: &Toolchain, profile: &Profile) -> BuildContext {
        let os = toolchain.platform.os;
        let compiler_id = match (toolchain.family, os) {
            (Family::Msvc, _) => "MSVC",
            (Family::Gcc, _) => "GNU",
            (Family::Clang, Os::Macos) => "AppleClang",
            (Family::Clang | Family::ClangCl, _) => "Clang",
        };

        // The configuration CMake builds for the profile: the first built-in one along its chain
        let configuration = profile
            .chain
            .iter()
            .find(|name| matches!(name.as_str(), "debug" | "release" | "relwithdebinfo" | "minsizerel"))
            .unwrap_or(&profile.name);

        BuildContext {
            family: toolchain.family,
            os,
            msvc_abi: toolchain.is_msvc_abi(),
            configuration: crate::targets::config_name(configuration),
            compiler_id,
            frontend: if toolchain.family.is_msvc_style() { "MSVC" } else { "GNU" },
            language: None,
            link: false,
        }
    }

    fn for_language(&self, language: &'static str) -> BuildContext {
        BuildContext {
            language: Some(language),
            ..self.clone()
        }
    }

    fn for_link(&self) -> BuildContext {
        BuildContext { link: true, ..self.clone() }
    }

    fn platform_id(&self) -> &'static str {
        match self.os {
            Os::Windows => "Windows",
            Os::Linux => "Linux",
            Os::Macos => "Darwin",
        }
    }
}

impl InstalledTree {
    pub fn load(install_root: &Path, triplet: &str) -> Result<InstalledTree> {
        let status_file = install_root.join("vcpkg").join("status");
        let status = std::fs::read_to_string(&status_file).with_context(|| format!("reading {} (did vcpkg install run?)", slash(&status_file)))?;

        let mut tree = InstalledTree {
            root: install_root.to_path_buf(),
            triplet: triplet.to_string(),
            packages: HashMap::new(),
        };
        tree.read_status(&status.replace("\r\n", "\n"));
        tree.read_file_lists()?;

        Ok(tree)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.packages.contains_key(name)
    }

    /// Every installed package's version, by name.
    pub fn versions(&self) -> std::collections::BTreeMap<String, String> {
        self.packages
            .iter()
            .filter_map(|(name, package)| Some((name.clone(), package.version.clone()?)))
            .collect()
    }

    /// Headers, libraries and DLLs of `name` and its dependencies, dependents before dependencies (link order), and
    /// what their CMake packages require of users, evaluated for `build`. `debug` picks the debug libraries where the
    /// package has them. `only_libs` keeps libraries whose file stem matches one of the names.
    pub fn usage(&self, name: &str, debug: bool, only_libs: Option<&[String]>, build: &BuildContext) -> Result<Usage> {
        if !self.contains(name) {
            bail!("vcpkg package `{name}` is not installed for {} (is it in the manifest?)", self.triplet);
        }

        let mut usage = Usage {
            includes: vec![self.root.join(&self.triplet).join("include")],
            ..Default::default()
        };

        // Every package's imported targets (host-only tools like vcpkg-cmake are not installed for the triplet).
        // A linked name that is none of them is a system library
        let packages: Vec<(String, &Package, Vec<ImportedTarget>)> = self
            .closure(name)
            .into_iter()
            .filter_map(|package_name| {
                let package = self.packages.get(&package_name)?;
                Some((package_name, package, self.imported_targets(package, build)))
            })
            .collect();
        let known: HashSet<&str> = packages
            .iter()
            .flat_map(|(_, _, imported)| imported.iter().map(|target| target.name.as_str()))
            .collect();

        // Imported targets users compile with: the package's own, then those they name publicly, as CMake propagates
        // them. Those users link: these, and those named under $<LINK_ONLY:...>
        let mut requirements = Requirements::default();
        for (package_name, package, imported) in &packages {
            let requested = package_name == name;
            let (mut libs, runtime) = self.package_binaries(package, debug);

            // The requested package: its library targets (all of them when it is header-only). Dependencies: the
            // targets something used names, until no more are named (targets of one package name each other)
            let has_library = imported.iter().any(|target| target.has_library);
            let wanted = |target: &ImportedTarget| requested && (target.has_library || !has_library);
            let mut compiled = vec![false; imported.len()];
            let mut linked = vec![false; imported.len()];
            loop {
                let named = (requirements.visible.len(), requirements.linked.len());
                for (index, target) in imported.iter().enumerate() {
                    if !compiled[index] && (wanted(target) || requirements.visible.contains(&target.name)) {
                        compiled[index] = true;
                        requirements.add_compile(target, build);
                    }
                    if !linked[index] && (wanted(target) || requirements.linked.contains(&target.name)) {
                        linked[index] = true;
                        requirements.add_link(target, &known, self);
                    }
                }
                if (requirements.visible.len(), requirements.linked.len()) == named {
                    break;
                }
            }

            // The requested package's own libraries can be narrowed by name
            if requested {
                if let Some(wanted) = only_libs {
                    libs.retain(|lib| wanted.iter().any(|wanted_name| lib_matches(lib, wanted_name)));
                }
            }

            usage.libs.extend(libs);
            usage.runtime.extend(runtime);
        }

        // The targets of CMake's find modules the packages name (OpenMP::OpenMP_CXX through directxtex)
        for module in MODULE_TARGETS.iter().filter_map(|module_name| module_target(module_name, build)) {
            if requirements.visible.contains(&module.name) {
                requirements.add_compile(&module, build);
            }
            if requirements.linked.contains(&module.name) {
                requirements.add_link(&module, &known, self);
            }
        }

        requirements.fill(&mut usage, build);
        Ok(usage)
    }

    /// How CMake finds and links port `name`: what its `usage` file says, else its config file and imported library
    /// targets. None when it has neither (its files are then linked by path).
    pub fn cmake_package(&self, name: &str, build: &BuildContext) -> Option<CmakePackage> {
        let package = self.packages.get(name)?;
        let share = format!("{}/share/", self.triplet);
        let shared_files: Vec<&String> = package.files.iter().filter(|file| file.starts_with(&share)).collect();

        // The port's own advice
        let usage = shared_files
            .iter()
            .find(|file| file.ends_with("/usage"))
            .and_then(|file| std::fs::read_to_string(self.root.join(file)).ok())
            .and_then(|text| parse_usage(&text));
        if usage.is_some() {
            return usage;
        }

        // Its config file, and the imported targets that have a library (all of them when it is header-only)
        let config = shared_files.iter().find_map(|file| {
            let file_name = file.rsplit('/').next()?;
            file_name
                .strip_suffix("Config.cmake")
                .or_else(|| file_name.strip_suffix("-config.cmake"))
                .map(str::to_string)
        })?;
        let imported = self.imported_targets(package, build);
        let has_library = imported.iter().any(|target| target.has_library);
        let targets: Vec<String> = imported
            .iter()
            .filter(|target| target.has_library || !has_library)
            .map(|target| target.name.clone())
            .collect();
        if targets.is_empty() {
            return None;
        }

        Some(CmakePackage {
            name: config,
            config: true,
            targets,
        })
    }

    /// The imported targets a package's CMake files define, with what each gives its users for `build`.
    fn imported_targets(&self, package: &Package, build: &BuildContext) -> Vec<ImportedTarget> {
        let prefix = slash(&self.root.join(&self.triplet));
        let share = format!("{}/share/", self.triplet);
        let link = build.for_link();
        let mut targets: Vec<ImportedTarget> = Vec::new();

        let cmake_files = package.files.iter().filter(|file| file.starts_with(&share) && file.ends_with(".cmake"));
        for file in cmake_files {
            let path = self.root.join(file);
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let list_dir = slash(path.parent().unwrap());

            // One target's properties may be spread over several files (its configurations' files hold the libraries)
            for (name, properties) in target_property_blocks(&text) {
                let index = match targets.iter().position(|target| target.name == name) {
                    Some(index) => index,
                    None => {
                        targets.push(ImportedTarget {
                            name: name.clone(),
                            ..Default::default()
                        });
                        targets.len() - 1
                    }
                };
                let target = &mut targets[index];
                target.has_library |= properties.contains("IMPORTED_LOCATION") || properties.contains("IMPORTED_IMPLIB");

                for entry in property_values(&properties, "INTERFACE_INCLUDE_DIRECTORIES", build) {
                    let entry = expand_import_prefixes(&entry, &prefix, &list_dir);
                    if entry.contains("${") {
                        continue;
                    }
                    let folder = crate::util::normalize(Path::new(&entry));
                    if folder.is_dir() && !target.includes.contains(&folder) {
                        target.includes.push(folder);
                    }
                }
                for define in property_values(&properties, "INTERFACE_COMPILE_DEFINITIONS", build) {
                    if !define.contains("${") && !target.defines.contains(&define) {
                        target.defines.push(define);
                    }
                }
                target.links.extend(property_values(&properties, "INTERFACE_LINK_LIBRARIES", build));

                // What linking needs: every linked item, and the link options
                for item in property_values(&properties, "INTERFACE_LINK_LIBRARIES", &link) {
                    let item = expand_import_prefixes(&item, &prefix, &list_dir);
                    if !item.contains("${") {
                        target.link_items.push(item);
                    }
                }
                let link_options = property_values(&properties, "INTERFACE_LINK_OPTIONS", &link);
                target
                    .link_options
                    .extend(link_options.into_iter().filter(|option| !option.contains("${")));

                // Compile options depend on the file's language: evaluated for each
                target.compile_options.extend(property_value(&properties, "INTERFACE_COMPILE_OPTIONS"));
            }
        }

        targets
    }

    /// Whether the triplet's library folder holds a library named `name` (a target of a package outside the closure).
    fn has_library_named(&self, name: &str) -> bool {
        let lib_dir = self.root.join(&self.triplet).join("lib");
        [
            format!("{name}.lib"),
            format!("lib{name}.a"),
            format!("lib{name}.so"),
            format!("lib{name}.dylib"),
        ]
        .iter()
        .any(|file| lib_dir.join(file).is_file())
    }

    /// `name` and every package it depends on, depth first, each once, dependents first.
    fn closure(&self, name: &str) -> Vec<String> {
        let mut order = Vec::new();
        let mut visited = HashSet::new();
        self.visit(name, &mut visited, &mut order);

        order.reverse();
        order
    }

    fn visit(&self, name: &str, visited: &mut HashSet<String>, order: &mut Vec<String>) {
        if !visited.insert(name.to_string()) {
            return;
        }

        if let Some(package) = self.packages.get(name) {
            for dependency in &package.depends {
                self.visit(dependency, visited, order);
            }
        }

        order.push(name.to_string());
    }

    /// One package's libraries and DLLs for the build type, falling back to release files when it has no debug ones.
    fn package_binaries(&self, package: &Package, debug: bool) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let triplet = &self.triplet;
        let collect = |folder: &str, extensions: &[&str]| -> Vec<PathBuf> {
            let prefix = format!("{triplet}/{folder}/");
            package
                .files
                .iter()
                .filter(|file| file.starts_with(&prefix) && !file[prefix.len()..].contains('/'))
                .filter(|file| extensions.iter().any(|extension| file.to_ascii_lowercase().ends_with(extension)))
                .map(|file| self.root.join(file))
                .collect()
        };

        let windows = self.triplet.contains("windows") || self.triplet.contains("mingw") || self.triplet.contains("uwp");
        let lib_extensions: &[&str] = if windows { &[".lib", ".a"] } else { &[".a", ".so", ".dylib"] };
        let runtime_extensions: &[&str] = if windows { &[".dll"] } else { &[] };

        // Debug files first when asked for, release otherwise or when there are none. Manual-link libraries (gtest_main,
        // SDL2main) go last: a static library is only pulled in for symbols nothing else defines
        let libraries_in = |folder: &str| [collect(folder, lib_extensions), collect(&format!("{folder}/manual-link"), lib_extensions)].concat();
        let mut libs = if debug { libraries_in("debug/lib") } else { Vec::new() };
        if libs.is_empty() {
            libs = libraries_in("lib");
        }

        let mut runtime = if debug { collect("debug/bin", runtime_extensions) } else { Vec::new() };
        if runtime.is_empty() {
            runtime = collect("bin", runtime_extensions);
        }

        (libs, runtime)
    }

    /// Parses the dpkg-style status file: paragraphs of "Key: value" lines.
    fn read_status(&mut self, status: &str) {
        for paragraph in status.split("\n\n") {
            let field = |key: &str| -> Option<&str> {
                paragraph
                    .lines()
                    .find_map(|line| line.strip_prefix(key).and_then(|rest| rest.strip_prefix(": ")).map(|value| value.trim()))
            };

            let (Some(name), Some(architecture), Some(state)) = (field("Package"), field("Architecture"), field("Status")) else {
                continue;
            };
            if architecture != self.triplet || !state.ends_with("install ok installed") {
                continue;
            }

            // Feature paragraphs add their dependencies to the package's
            let depends: Vec<String> = field("Depends")
                .unwrap_or("")
                .split(',')
                .filter_map(|dependency| self.dependency_name(dependency))
                .collect();

            let package = self.packages.entry(name.to_string()).or_default();
            if field("Feature").is_none() {
                if let Some(version) = field("Version") {
                    package.version = Some(match field("Port-Version").filter(|port_version| *port_version != "0") {
                        Some(port_version) => format!("{version}#{port_version}"),
                        None => version.to_string(),
                    });
                }
            }
            for dependency in depends {
                if dependency != name && !package.depends.contains(&dependency) {
                    package.depends.push(dependency);
                }
            }
        }
    }

    /// "zlib", "zlib:x64-windows", "curl[ssl]" -> the package name; dependencies for another (host) triplet -> None.
    fn dependency_name(&self, dependency: &str) -> Option<String> {
        let dependency = dependency.trim();
        if dependency.is_empty() {
            return None;
        }

        let (name, triplet) = match dependency.split_once(':') {
            Some((name, triplet)) => (name, Some(triplet)),
            None => (dependency, None),
        };
        if triplet.is_some_and(|triplet| triplet != self.triplet) {
            return None;
        }

        Some(name.split('[').next().unwrap_or(name).trim().to_string())
    }

    /// vcpkg/info/<name>_<version>_<triplet>.list hold each package's installed files.
    fn read_file_lists(&mut self) -> Result<()> {
        let info_folder = self.root.join("vcpkg").join("info");
        let suffix = format!("_{}.list", self.triplet);

        for entry in std::fs::read_dir(&info_folder).with_context(|| format!("reading {}", slash(&info_folder)))? {
            let entry = entry?;
            let file_name = entry.file_name().to_string_lossy().into_owned();

            let Some(stem) = file_name.strip_suffix(&suffix) else { continue };
            // Port names have no `_`; versions may
            let Some((name, _version)) = stem.split_once('_') else { continue };
            let Some(package) = self.packages.get_mut(name) else { continue };

            let list = std::fs::read_to_string(entry.path())?;
            package.files.extend(
                list.lines()
                    .map(|line| line.trim().to_string())
                    .filter(|line| !line.is_empty() && !line.ends_with('/')),
            );
        }

        Ok(())
    }
}

/// A port's usage file: its first `find_package(...)` and the targets of the first `target_link_libraries(...)` after it.
fn parse_usage(text: &str) -> Option<CmakePackage> {
    let start = text.find("find_package(")? + "find_package(".len();
    let after = &text[start..];
    let arguments: Vec<&str> = after[..after.find(')')?].split_whitespace().collect();
    let name = arguments.first()?.to_string();
    let config = arguments.iter().any(|word| *word == "CONFIG" || *word == "NO_MODULE");

    let link_start = after.find("target_link_libraries(")? + "target_link_libraries(".len();
    let link = &after[link_start..];
    let targets: Vec<String> = link[..link.find(')')?]
        .split_whitespace()
        .skip(1)
        .filter(|word| !matches!(*word, "PRIVATE" | "PUBLIC" | "INTERFACE"))
        .map(str::to_string)
        .collect();
    if targets.is_empty() {
        return None;
    }

    Some(CmakePackage { name, config, targets })
}

/// What the imported targets used so far require of their users, and the targets they name.
#[derive(Default)]
struct Requirements {
    /// Targets named for compiling (publicly), and for linking (under $<LINK_ONLY:...> too).
    visible: HashSet<String>,
    linked: HashSet<String>,

    includes: Vec<PathBuf>,
    defines: Vec<String>,
    /// Compile options for each language of LANGUAGES, each once, as written (`SHELL:` ones whole).
    options: [Vec<String>; 4],
    link_options: Vec<String>,
    system_libs: Vec<PathBuf>,
}

impl Requirements {
    /// Adds what users compile with from `target`, and the targets it names publicly.
    fn add_compile(&mut self, target: &ImportedTarget, build: &BuildContext) {
        push_new(&mut self.includes, &target.includes);
        push_new(&mut self.defines, &target.defines);

        for (index, (_, language)) in LANGUAGES.iter().enumerate() {
            let build = build.for_language(language);
            for value in &target.compile_options {
                let options = evaluate_list(value, &build);
                push_new(
                    &mut self.options[index],
                    &options.into_iter().filter(|option| !option.contains("${")).collect::<Vec<_>>(),
                );
            }
        }

        self.visible.extend(target.links.iter().cloned());
    }

    /// Adds what users link with from `target`: its link options, the flags and system libraries among what it links
    /// (packages' own library files, in `tree`, are linked from their file lists), and the targets it names.
    fn add_link(&mut self, target: &ImportedTarget, known: &HashSet<&str>, tree: &InstalledTree) {
        push_new(&mut self.link_options, &target.link_options);

        for item in &target.link_items {
            let path = Path::new(item);
            if item.starts_with('-') {
                push_new(&mut self.link_options, std::slice::from_ref(item));
            } else if item.contains("::") || known.contains(item.as_str()) {
                self.linked.insert(item.clone());
            } else if item.contains('/') || item.contains('\\') {
                // A library file elsewhere (the system's): linked when it exists here
                let outside_tree = util::strip_prefix_ci(path, &tree.root).is_none();
                if path.is_absolute() && path.is_file() && outside_tree {
                    push_new(&mut self.system_libs, &[path.to_path_buf()]);
                }
            } else if !tree.has_library_named(item) {
                push_new(&mut self.system_libs, &[path.to_path_buf()]);
            }
        }
    }

    /// Fills `usage` in: options every language's files get, then those of one language only, as words.
    fn fill(self, usage: &mut Usage, build: &BuildContext) {
        push_new(&mut usage.includes, &self.includes);
        usage.defines = self.defines;

        let [first, others @ ..] = &self.options;
        let common: Vec<&String> = first.iter().filter(|option| others.iter().all(|list| list.contains(option))).collect();
        for option in &common {
            usage.flags.extend(option_words(option, build));
        }
        for ((language, _), options) in LANGUAGES.iter().zip(&self.options) {
            for option in options.iter().filter(|option| !common.contains(option)) {
                usage
                    .language_flags
                    .extend(option_words(option, build).into_iter().map(|word| (*language, word)));
            }
        }

        usage.link_flags = self.link_options.iter().flat_map(|option| option_words(option, build)).collect();
        usage.libs.extend(self.system_libs);
    }
}

/// Appends the values `list` does not hold yet.
fn push_new<T: Clone + PartialEq>(list: &mut Vec<T>, values: &[T]) {
    for value in values {
        if !list.contains(value) {
            list.push(value.clone());
        }
    }
}

/// A compile or link option's words: a `SHELL:` option split as a shell would, a `LINKER:` one passed to the linker.
fn option_words(option: &str, build: &BuildContext) -> Vec<String> {
    if let Some(words) = option.strip_prefix("SHELL:") {
        return util::split_command(words);
    }
    if let Some(arguments) = option.strip_prefix("LINKER:") {
        let arguments: Vec<&str> = arguments.split(',').collect();
        return if build.frontend == "MSVC" {
            arguments.iter().map(|argument| argument.to_string()).collect()
        } else {
            vec![format!("-Wl,{}", arguments.join(","))]
        };
    }

    vec![option.to_string()]
}

/// An imported target of a CMake find module, with what the module gives it for `build`: FindOpenMP's flag (cl's
/// -openmp brings its runtime; clang-cl and clang on Windows link LLVM's libomp, which Visual Studio ships), FindThreads'
/// -pthread outside Windows, FindOpenGL's system library.
fn module_target(name: &str, build: &BuildContext) -> Option<ImportedTarget> {
    let mut target = ImportedTarget {
        name: name.to_string(),
        ..Default::default()
    };

    match name {
        "OpenMP::OpenMP_CXX" | "OpenMP::OpenMP_C" => {
            let language = if name.ends_with("_CXX") { "CXX" } else { "C" };
            let (flag, library) = match (build.family, build.os) {
                (Family::Msvc, _) => ("-openmp", None),
                (Family::ClangCl, _) => ("-Xclang -fopenmp", Some("libomp.lib")),
                (Family::Clang, Os::Windows) if build.msvc_abi => ("-fopenmp=libomp", Some("libomp.lib")),
                (Family::Clang, Os::Macos) => ("-Xclang -fopenmp", Some("omp")),
                (Family::Clang, _) => ("-fopenmp=libomp", None),
                (Family::Gcc, _) => ("-fopenmp", None),
            };
            target.compile_options.push(format!("$<$<COMPILE_LANGUAGE:{language}>:SHELL:{flag}>"));
            match library {
                Some(library) => target.link_items.push(library.to_string()),
                None if !flag.contains(' ') && build.frontend == "GNU" => target.link_options.push(flag.to_string()),
                None => {}
            }
        }
        "Threads::Threads" if build.os != Os::Windows => {
            target.compile_options.push("-pthread".into());
            target.link_options.push("-pthread".into());
        }
        "OpenGL::GL" | "OpenGL::GLU" => {
            let library = match (build.os, name == "OpenGL::GL") {
                (Os::Windows, true) => "opengl32",
                (Os::Windows, false) => "glu32",
                (Os::Linux, true) => "GL",
                (Os::Linux, false) => "GLU",
                (Os::Macos, _) => "OpenGL.framework",
            };
            target.link_items.push(library.into());
        }
        _ => return None,
    }

    Some(target)
}

/// An imported target's path with the variables package files write for the install prefix and their own folder.
fn expand_import_prefixes(entry: &str, prefix: &str, list_dir: &str) -> String {
    entry
        .replace("${_IMPORT_PREFIX}", prefix)
        .replace("${VCPKG_IMPORT_PREFIX}", prefix)
        .replace("${PACKAGE_PREFIX_DIR}", prefix)
        .replace("${CMAKE_CURRENT_LIST_DIR}", list_dir)
}

/// Each `set_target_properties(NAME PROPERTIES ...)` call: the target name and the text of its properties.
fn target_property_blocks(text: &str) -> Vec<(String, String)> {
    let mut blocks = Vec::new();
    let mut rest = text;

    while let Some(start) = rest.find("set_target_properties(") {
        let after = &rest[start + "set_target_properties(".len()..];

        // The closing parenthesis outside quoted values (__declspec(dllimport) is inside one)
        let mut quoted = false;
        let mut end = after.len();
        for (index, character) in after.char_indices() {
            match character {
                '"' => quoted = !quoted,
                ')' if !quoted => {
                    end = index;
                    break;
                }
                _ => {}
            }
        }

        let body = &after[..end];
        if let Some(name) = body.split_whitespace().next() {
            blocks.push((name.to_string(), body.to_string()));
        }
        rest = &after[end..];
    }

    blocks
}

/// A property's quoted value in a set_target_properties body, as a list with its generator expressions evaluated for
/// `build` (items with one that cannot be are left out).
fn property_values(properties: &str, property: &str, build: &BuildContext) -> Vec<String> {
    match property_value(properties, property) {
        Some(value) => evaluate_list(&value, build),
        None => Vec::new(),
    }
}

/// A property's quoted value in a set_target_properties body, as written.
fn property_value(properties: &str, property: &str) -> Option<String> {
    let found = properties
        .match_indices(property)
        .find(|(index, _)| properties[index + property.len()..].starts_with(char::is_whitespace));
    let (start, _) = found?;
    let value = properties[start + property.len()..].split('"').nth(1)?;

    // Generator expressions written inside the file's own quoted strings come escaped (\$<...>)
    Some(value.replace("\\$", "$"))
}

/// A ;-list whose items may hold generator expressions, evaluated for `build`. An item holding one Anvil cannot
/// evaluate is left out, so no option or definition is guessed.
fn evaluate_list(value: &str, build: &BuildContext) -> Vec<String> {
    let mut items = Vec::new();

    for item in split_top_level(value, ';') {
        let Some(expanded) = expand(item.trim(), build) else { continue };
        let parts = expanded.split(';').map(str::trim).filter(|part| !part.is_empty());
        items.extend(parts.map(String::from));
    }

    items
}

/// `text` with its generator expressions replaced by their values; None when one cannot be evaluated.
fn expand(text: &str, build: &BuildContext) -> Option<String> {
    let mut output = String::new();
    let mut rest = text;

    while let Some(start) = rest.find("$<") {
        output.push_str(&rest[..start]);
        let inner = &rest[start + 2..];
        let end = closing_bracket(inner)?;
        output.push_str(&evaluate_expression(&inner[..end], build)?);
        rest = &inner[end + 1..];
    }
    output.push_str(rest);

    Some(output)
}

/// One generator expression's value, from the text between `$<` and `>`: conditions ($<CONFIG:Debug>, $<AND:..>,
/// $<CXX_COMPILER_ID:MSVC>, $<COMPILE_LANGUAGE:CXX>, ...), the values they test ($<CONFIG>, ...), and `$<1:..>`,
/// `$<0:..>`, $<IF:..>, $<INSTALL_INTERFACE:..>, $<LINK_ONLY:..> (only for linking) of content.
fn evaluate_expression(inner: &str, build: &BuildContext) -> Option<String> {
    let (head, arguments) = split_at_colon(inner);
    let head = expand(head, build)?;
    let truth = |holds: bool| Some(if holds { "1" } else { "0" }.to_string());

    // Without arguments: the values conditions test
    let Some(arguments) = arguments else {
        return match head.as_str() {
            "CONFIG" => Some(build.configuration.clone()),
            "CXX_COMPILER_ID" | "C_COMPILER_ID" => Some(build.compiler_id.to_string()),
            "CXX_COMPILER_FRONTEND_VARIANT" | "C_COMPILER_FRONTEND_VARIANT" => Some(build.frontend.to_string()),
            "PLATFORM_ID" => Some(build.platform_id().to_string()),
            "COMPILE_LANGUAGE" => build.language.map(String::from),
            "SEMICOLON" => Some(";".into()),
            "COMMA" => Some(",".into()),
            "ANGLE-R" => Some(">".into()),
            _ => None,
        };
    };
    let values = || -> Option<Vec<String>> {
        split_top_level(arguments, ',')
            .into_iter()
            .map(|argument| expand(argument, build))
            .collect()
    };

    match head.as_str() {
        // Content, kept or dropped
        "1" | "INSTALL_INTERFACE" | "HOST_LINK" => expand(arguments, build),
        "0" | "BUILD_INTERFACE" | "BUILD_LOCAL_INTERFACE" | "DEVICE_LINK" => Some(String::new()),
        "LINK_ONLY" if build.link => expand(arguments, build),
        "LINK_ONLY" => Some(String::new()),
        "IF" => {
            let [condition, then, otherwise] = split_top_level(arguments, ',')[..] else {
                return None;
            };
            match expand(condition, build)?.as_str() {
                "1" => expand(then, build),
                "0" => expand(otherwise, build),
                _ => None,
            }
        }

        // Logic, each argument evaluated only as far as the answer needs
        "BOOL" => {
            let value = expand(arguments, build)?.to_ascii_uppercase();
            let false_constant = matches!(value.as_str(), "" | "0" | "OFF" | "NO" | "N" | "FALSE" | "IGNORE" | "NOTFOUND");
            truth(!false_constant && !value.ends_with("-NOTFOUND"))
        }
        "NOT" => match expand(arguments, build)?.as_str() {
            "0" => truth(true),
            "1" => truth(false),
            _ => None,
        },
        "AND" | "OR" => {
            let deciding = if head == "AND" { "0" } else { "1" };
            for argument in split_top_level(arguments, ',') {
                match expand(argument, build)?.as_str() {
                    value if value == deciding => return Some(deciding.to_string()),
                    "0" | "1" => {}
                    _ => return None,
                }
            }
            truth(head == "AND")
        }
        "STREQUAL" => match &values()?[..] {
            [left, right] => truth(left == right),
            _ => None,
        },

        // What the build is
        "CONFIG" => truth(values()?.iter().any(|name| name.eq_ignore_ascii_case(&build.configuration))),
        "CXX_COMPILER_ID" | "C_COMPILER_ID" => truth(values()?.iter().any(|id| id == build.compiler_id)),
        "CXX_COMPILER_FRONTEND_VARIANT" | "C_COMPILER_FRONTEND_VARIANT" => truth(values()?.iter().any(|variant| variant == build.frontend)),
        "PLATFORM_ID" => truth(values()?.iter().any(|id| id == build.platform_id())),
        "COMPILE_LANGUAGE" => {
            let language = build.language?;
            truth(values()?.iter().any(|name| name == language))
        }
        "COMPILE_LANG_AND_ID" => {
            let language = build.language?;
            let values = values()?;
            let (first, ids) = values.split_first()?;
            truth(first == language && ids.iter().any(|id| id == build.compiler_id))
        }
        _ => None,
    }
}

/// The index of the `>` closing a generator expression whose text (after `$<`) starts `text`.
fn closing_bracket(text: &str) -> Option<usize> {
    let mut depth = 0;
    let bytes = text.as_bytes();

    for (index, character) in text.char_indices() {
        match character {
            '<' if index > 0 && bytes[index - 1] == b'$' => depth += 1,
            '>' if depth == 0 => return Some(index),
            '>' => depth -= 1,
            _ => {}
        }
    }

    None
}

/// `head:arguments` at the first colon outside nested generator expressions; no arguments without one.
fn split_at_colon(text: &str) -> (&str, Option<&str>) {
    let mut depth = 0;
    let bytes = text.as_bytes();
    for (index, character) in text.char_indices() {
        match character {
            '<' if index > 0 && bytes[index - 1] == b'$' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            ':' if depth == 0 => return (&text[..index], Some(&text[index + 1..])),
            _ => {}
        }
    }
    (text, None)
}

/// Splits at `separator` outside generator expressions.
fn split_top_level(text: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    let bytes = text.as_bytes();

    for (index, character) in text.char_indices() {
        match character {
            '<' if index > 0 && bytes[index - 1] == b'$' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            character if character == separator && depth == 0 => {
                parts.push(&text[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);

    parts
}

/// "fmt" matches fmt.lib, libfmt.a and fmtd.lib.
fn lib_matches(lib: &Path, wanted: &str) -> bool {
    let stem = lib.file_stem().map(|stem| stem.to_string_lossy().to_lowercase()).unwrap_or_default();
    let stem = stem.strip_prefix("lib").unwrap_or(&stem).to_string();
    let wanted = wanted.to_lowercase();

    stem == wanted || stem == format!("{wanted}d")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A build with `family` on `os` for `configuration`.
    fn build(family: Family, os: Os, configuration: &str) -> BuildContext {
        BuildContext {
            family,
            os,
            msvc_abi: os == Os::Windows && family != Family::Gcc,
            configuration: configuration.into(),
            compiler_id: match family {
                Family::Msvc => "MSVC",
                Family::Gcc => "GNU",
                Family::Clang | Family::ClangCl => "Clang",
            },
            frontend: if family.is_msvc_style() { "MSVC" } else { "GNU" },
            language: None,
            link: false,
        }
    }

    #[test]
    fn evaluates_exported_values() {
        let debug = build(Family::Msvc, Os::Windows, "Debug");
        let release = build(Family::Msvc, Os::Windows, "Release");

        let value = "FMT_SHARED;$<$<CONFIG:Debug>:DEBUG;_DEBUG>;$<$<NOT:$<CONFIG:Debug>>:NDEBUG>;API=__declspec(dllimport)";
        assert_eq!(
            evaluate_list(value, &debug),
            vec!["FMT_SHARED", "DEBUG", "_DEBUG", "API=__declspec(dllimport)"]
        );
        assert_eq!(evaluate_list(value, &release), vec!["FMT_SHARED", "NDEBUG", "API=__declspec(dllimport)"]);
        assert_eq!(evaluate_list("zlib::zlib;$<LINK_ONLY:brotli::dec>", &debug), vec!["zlib::zlib"]);
        assert_eq!(
            evaluate_list("zlib::zlib;$<LINK_ONLY:brotli::dec>", &debug.for_link()),
            vec!["zlib::zlib", "brotli::dec"]
        );
        assert_eq!(
            evaluate_list("$<$<NOT:$<BOOL:ON>>:A=0>;$<$<BOOL:OFF>:B=1>;$<$<BOOL:ON>:C>", &debug),
            vec!["C"]
        );

        let blocks = target_property_blocks("set_target_properties(a::b PROPERTIES\n  INTERFACE_COMPILE_DEFINITIONS \"X=f(1)\"\n)\nset_target_properties(a::c PROPERTIES IMPORTED_IMPLIB \"x.lib\")");
        assert_eq!(blocks.len(), 2);
        assert_eq!(property_values(&blocks[0].1, "INTERFACE_COMPILE_DEFINITIONS", &debug), vec!["X=f(1)"]);
    }

    #[test]
    fn evaluates_interface_options() {
        let msvc = build(Family::Msvc, Os::Windows, "Debug");
        let gcc = build(Family::Gcc, Os::Linux, "Release");

        // glaze: unconditional options and per-configuration ones, link options with a ; inside a condition
        let glaze = "/Zc:preprocessor;/permissive-;$<$<CONFIG:Release>:/GL>;$<$<CONFIG:MinSizeRel>:/GL>";
        assert_eq!(evaluate_list(glaze, &msvc), vec!["/Zc:preprocessor", "/permissive-"]);
        assert_eq!(
            evaluate_list("$<$<CONFIG:Release>:/LTCG;/INCREMENTAL:NO>", &gcc.for_link()),
            vec!["/LTCG", "/INCREMENTAL:NO"]
        );

        // Compiler and language tests, alone and combined
        let options = "$<$<CXX_COMPILER_ID:MSVC>:/utf-8>;$<$<CXX_COMPILER_ID:GNU,Clang>:-Wno-psabi>;$<$<AND:$<COMPILE_LANGUAGE:CXX>,$<CXX_COMPILER_ID:MSVC>>:/Zc:__cplusplus>";
        assert_eq!(evaluate_list(options, &msvc.for_language("CXX")), vec!["/utf-8", "/Zc:__cplusplus"]);
        assert_eq!(evaluate_list(options, &msvc.for_language("C")), vec!["/utf-8"]);
        assert_eq!(evaluate_list(options, &gcc.for_language("CXX")), vec!["-Wno-psabi"]);
        assert_eq!(
            evaluate_list("$<$<COMPILE_LANG_AND_ID:CXX,MSVC>:/bigobj>", &msvc.for_language("CXX")),
            vec!["/bigobj"]
        );
        assert_eq!(evaluate_list("$<IF:$<PLATFORM_ID:Windows>,-DWIN,-DUNIX>", &gcc), vec!["-DUNIX"]);
        assert_eq!(evaluate_list("$<$<STREQUAL:$<CXX_COMPILER_ID>,MSVC>:/W3>", &msvc), vec!["/W3"]);
        assert_eq!(evaluate_list("$<$<CXX_COMPILER_FRONTEND_VARIANT:MSVC>:/EHsc>", &msvc), vec!["/EHsc"]);

        // Unknown expressions, and languages outside a compile, drop their item only
        let unknown = "-a;$<$<VERSION_GREATER:$<CXX_COMPILER_VERSION>,19>:-b>;$<$<COMPILE_LANGUAGE:CXX>:-c>;$<TARGET_PROPERTY:x,y>";
        assert_eq!(evaluate_list(unknown, &msvc), vec!["-a"]);

        // A false condition keeps what it guards from being evaluated; AND stops at its first false
        assert_eq!(
            evaluate_list("$<$<CONFIG:Release>:$<UNKNOWN:x>>;$<$<AND:0,$<UNKNOWN>>:y>", &msvc),
            Vec::<String>::new()
        );

        // SHELL: and LINKER: options
        assert_eq!(option_words("SHELL:-Xclang -fopenmp", &msvc), vec!["-Xclang", "-fopenmp"]);
        assert_eq!(option_words("LINKER:--as-needed,-z,now", &gcc), vec!["-Wl,--as-needed,-z,now"]);
        assert_eq!(option_words("LINKER:/DEBUG", &msvc), vec!["/DEBUG"]);

        // FindOpenMP's target: only C++ files get its flag, per compiler
        let openmp = module_target("OpenMP::OpenMP_CXX", &msvc).unwrap();
        assert_eq!(
            evaluate_list(&openmp.compile_options[0], &msvc.for_language("CXX")),
            vec!["SHELL:-openmp"]
        );
        assert!(evaluate_list(&openmp.compile_options[0], &msvc.for_language("C")).is_empty());
        let openmp = module_target("OpenMP::OpenMP_CXX", &gcc).unwrap();
        assert_eq!(openmp.link_options, vec!["-fopenmp"]);
        let openmp = module_target("OpenMP::OpenMP_C", &build(Family::ClangCl, Os::Windows, "Debug")).unwrap();
        assert_eq!(openmp.link_items, vec!["libomp.lib"]);
        assert!(module_target("Threads::Threads", &msvc).is_none());
        assert_eq!(module_target("Threads::Threads", &gcc).unwrap().link_options, vec!["-pthread"]);
    }

    #[test]
    fn reads_usage_files() {
        let glm = "The package glm provides CMake targets:\n\n    find_package(glm CONFIG REQUIRED)\n    target_link_libraries(main PRIVATE glm::glm)\n\n    # Or use the header-only version\n    find_package(glm CONFIG REQUIRED)\n    target_link_libraries(main PRIVATE glm::glm-header-only)\n";
        let package = parse_usage(glm).unwrap();
        assert_eq!(
            (package.name.as_str(), package.config, package.targets),
            ("glm", true, vec!["glm::glm".to_string()])
        );

        let zlib = "    find_package(ZLIB REQUIRED)\n    target_link_libraries(main PRIVATE ZLIB::ZLIB)\n";
        assert!(!parse_usage(zlib).unwrap().config);

        // Includes only: nothing to link
        assert!(parse_usage("    find_package(Stb REQUIRED)\n    target_include_directories(main PRIVATE ${Stb_INCLUDE_DIR})").is_none());
    }

    #[test]
    fn reads_a_tree() {
        let root = std::env::temp_dir().join(format!("anvil-vcpkg-tree-{}", std::process::id()));
        let info = root.join("vcpkg").join("info");
        std::fs::create_dir_all(&info).unwrap();

        // Two packages: sdl3 (with a feature adding a dependency) and its dependency
        let status = "Package: sdl3\nVersion: 3.2\nDepends: vcpkg-cmake:x64-windows, vcpkg-cmake-config\nArchitecture: x64-windows\nStatus: install ok installed\n\n\
                      Package: sdl3\nFeature: vulkan\nDepends: volk\nArchitecture: x64-windows\nStatus: install ok installed\n\n\
                      Package: volk\nVersion: 1.4\nArchitecture: x64-windows\nStatus: install ok installed\n";
        std::fs::write(root.join("vcpkg").join("status"), status).unwrap();
        std::fs::write(info.join("sdl3_3.2_x64-windows.list"), "x64-windows/lib/\nx64-windows/lib/SDL3.lib\nx64-windows/lib/manual-link/SDL3main.lib\nx64-windows/debug/lib/SDL3.lib\nx64-windows/bin/SDL3.dll\nx64-windows/share/sdl3/SDL3Targets.cmake\n").unwrap();
        std::fs::write(
            info.join("volk_1.4_x64-windows.list"),
            "x64-windows/lib/volk.lib\nx64-windows/share/volk/volkTargets.cmake\n",
        )
        .unwrap();

        // Their CMake files: sdl3's options reach its users, volk's (linked only) only its link items
        let share = root.join("x64-windows").join("share");
        std::fs::create_dir_all(share.join("sdl3")).unwrap();
        std::fs::create_dir_all(share.join("volk")).unwrap();
        let sdl3_targets = r#"set_target_properties(SDL3::SDL3 PROPERTIES
  INTERFACE_COMPILE_OPTIONS "/Zc:preprocessor;\$<\$<CONFIG:Release>:/GL>"
  INTERFACE_LINK_LIBRARIES "OpenMP::OpenMP_CXX;\$<LINK_ONLY:ws2_32>;\$<LINK_ONLY:volk::volk>"
  INTERFACE_LINK_OPTIONS "\$<\$<CONFIG:Release>:/LTCG>"
)
set_target_properties(SDL3::SDL3 PROPERTIES IMPORTED_IMPLIB_RELEASE "${_IMPORT_PREFIX}/lib/SDL3.lib")"#;
        let volk_targets = r#"set_target_properties(volk::volk PROPERTIES
  INTERFACE_COMPILE_OPTIONS "-volk"
  INTERFACE_LINK_LIBRARIES "-ignore:4221;user32"
  IMPORTED_IMPLIB "${_IMPORT_PREFIX}/lib/volk.lib"
)"#;
        std::fs::write(share.join("sdl3").join("SDL3Targets.cmake"), sdl3_targets).unwrap();
        std::fs::write(share.join("volk").join("volkTargets.cmake"), volk_targets).unwrap();

        let tree = InstalledTree::load(&root, "x64-windows").unwrap();
        let usage = tree.usage("sdl3", false, None, &build(Family::Msvc, Os::Windows, "Release")).unwrap();
        let names: Vec<String> = usage
            .libs
            .iter()
            .map(|lib| lib.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert_eq!(names, vec!["SDL3.lib", "SDL3main.lib", "volk.lib", "ws2_32", "user32"]);
        assert_eq!(usage.runtime.len(), 1);
        assert_eq!(usage.flags, vec!["/Zc:preprocessor", "/GL"]);
        assert_eq!(usage.language_flags, vec![(Language::Cxx, "-openmp".to_string())]);
        assert_eq!(usage.link_flags, vec!["/LTCG", "-ignore:4221"]);
        assert!(tree.usage("missing", false, None, &build(Family::Msvc, Os::Windows, "Release")).is_err());

        std::fs::remove_dir_all(&root).ok();
    }
}
