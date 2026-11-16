//! Reads a vcpkg install tree: the `status` database (which packages and features are installed, and what each
//! depends on) and the per-package file lists, to give each package's headers, libraries and DLLs without running
//! CMake.

use crate::flags::Language;
use crate::platform::{Cpu, Os};
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
pub(crate) struct ImportedTarget {
    pub(crate) name: String,
    pub(crate) has_library: bool,
    /// `add_library(<name> <kind> IMPORTED)`: SHARED, STATIC, UNKNOWN, INTERFACE, ...; empty when no file says.
    pub(crate) kind: String,
    /// INTERFACE_INCLUDE_DIRECTORIES (prefixes expanded) and INTERFACE_COMPILE_DEFINITIONS as written: evaluated per
    /// language, like compile options.
    pub(crate) include_values: Vec<String>,
    define_values: Vec<String>,
    /// Targets its users link too (not those under $<LINK_ONLY:...>).
    pub(crate) links: Vec<String>,
    /// Everything its users link, $<LINK_ONLY:...> too: targets, system libraries and linker flags.
    pub(crate) link_items: Vec<String>,
    /// INTERFACE_COMPILE_OPTIONS as written: evaluated per language.
    compile_options: Vec<String>,
    link_options: Vec<String>,
    /// Files its users' programs need beside them (LLVM's libomp.dll).
    pub(crate) runtime: Vec<PathBuf>,
    /// Its files per configuration (IMPORTED_LOCATION_<CONFIG>, IMPORTED_IMPLIB_<CONFIG>; "" for the unsuffixed ones).
    pub(crate) files: Vec<ImportedFiles>,
}

/// One configuration's files of an imported target: the library (a DLL for a shared one on Windows) and its import
/// library, and the shared libraries it loads without passing them on (IMPORTED_LINK_DEPENDENT_LIBRARIES).
#[derive(Default, Debug, Clone, PartialEq)]
pub(crate) struct ImportedFiles {
    pub(crate) configuration: String,
    pub(crate) location: Option<PathBuf>,
    pub(crate) import_library: Option<PathBuf>,
    pub(crate) dependent_libraries: Vec<String>,
    /// The languages of a static library's code (IMPORTED_LINK_INTERFACE_LANGUAGES: C, CXX), when the package says.
    pub(crate) languages: Option<Vec<String>>,
}

/// Where the `${_IMPORT_PREFIX}` of a package's files is: one folder for all (a vcpkg triplet's), or what each file
/// computes from its own folder (`get_filename_component(_IMPORT_PREFIX ... PATH)`), else `fallback`.
pub(crate) enum ImportPrefix {
    Fixed(String),
    PerFile { fallback: String },
}

/// The OpenMP runtime programs need beside them: LLVM's libomp.dll beside a clang or clang-cl that builds for the MSVC
/// ABI (Visual Studio's `VC/Tools/Llvm/x64/bin`, or LLVM's install), or MinGW gcc's libgomp-1.dll with the DLLs of
/// its bin folder it loads (libwinpthread-1.dll, libgcc_s_seh-1.dll). Built for the target's processor: Visual Studio's
/// LLVM has a folder per processor (`Llvm/bin` is 32-bit); with none for it, a message saying so.
fn openmp_runtime(toolchain: &Toolchain) -> Result<Vec<PathBuf>, String> {
    let Some(bin) = toolchain.cxx.parent() else { return Ok(Vec::new()) };
    let llvm = matches!(toolchain.family, Family::Clang | Family::ClangCl) && toolchain.is_msvc_abi();
    let mingw = toolchain.family == Family::Gcc && toolchain.platform.os == Os::Windows;

    let name = match (llvm, mingw) {
        (true, _) => "libomp.dll",
        (_, true) => "libgomp-1.dll",
        _ => return Ok(Vec::new()),
    };
    if !bin.join(name).is_file() {
        return Ok(Vec::new());
    }

    // The folder with the one for the target's processor: the compiler's, or Visual Studio's for another processor
    let mut folders = vec![bin.to_path_buf()];
    let llvm_root = bin
        .ancestors()
        .take(3)
        .find(|folder| folder.file_name().is_some_and(|name| name.eq_ignore_ascii_case("llvm")));
    if let Some(llvm_root) = llvm_root.filter(|_| llvm) {
        folders.extend(["bin", "x64/bin", "ARM64/bin"].iter().map(|folder| llvm_root.join(folder)));
    }
    let machine = pe_machine_of(toolchain.platform.cpu);
    let Some(folder) = folders.into_iter().find(|folder| file_machine(&folder.join(name)) == Some(machine)) else {
        return Err(format!(
            "{} is for another processor than {}, and no {name} for it was found: programs using OpenMP need one beside them; copy it there yourself",
            slash(&bin.join(name)),
            toolchain.platform.name()
        ));
    };

    // The DLLs it loads from the same folder, and theirs (the system's are never there)
    let mut files = vec![folder.join(name)];
    let mut next = 0;
    while next < files.len() {
        for name in imported_dlls(&files[next]) {
            let file = folder.join(&name);
            if file.is_file() && !files.iter().any(|known| util::path_key(known) == util::path_key(&file)) {
                files.push(file);
            }
        }
        next += 1;
    }

    Ok(files)
}

/// The machine field of a PE file's header for a processor.
fn pe_machine_of(cpu: Cpu) -> u16 {
    match cpu {
        Cpu::X64 => 0x8664,
        Cpu::X86 => 0x14c,
        Cpu::Arm64 => 0xaa64,
        Cpu::Arm => 0x1c4,
    }
}

/// The processor a Windows program or DLL is for, from the start of the file.
fn file_machine(file: &Path) -> Option<u16> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(file).ok()?.take(4096).read_to_end(&mut bytes).ok()?;
    pe_machine(&bytes)
}

/// The processor a Windows program or DLL is for, as its PE header's machine field says.
fn pe_machine(bytes: &[u8]) -> Option<u16> {
    let header = u32::from_le_bytes(bytes.get(0x3c..0x40)?.try_into().ok()?) as usize;
    if bytes.get(header..header + 4)? != b"PE\0\0" {
        return None;
    }

    Some(u16::from_le_bytes(bytes.get(header + 4..header + 6)?.try_into().ok()?))
}

/// The DLLs a Windows program or DLL imports, as its PE import table names them; none when it cannot be read.
fn imported_dlls(file: &Path) -> Vec<String> {
    std::fs::read(file).ok().and_then(|bytes| pe_imports(&bytes)).unwrap_or_default()
}

fn pe_imports(bytes: &[u8]) -> Option<Vec<String>> {
    let number = |offset: usize, size: usize| -> Option<usize> {
        let field = bytes.get(offset..offset + size)?;
        Some(field.iter().rev().fold(0, |value, byte| (value << 8) | *byte as usize))
    };

    // The headers: the PE signature, the optional header (PE32 or PE32+) and its import directory, the sections
    let header = number(0x3c, 4)?;
    if bytes.get(header..header + 4)? != b"PE\0\0" {
        return None;
    }
    let section_count = number(header + 6, 2)?;
    let optional = header + 24;
    let directories = match number(optional, 2)? {
        0x10b => optional + 96,
        0x20b => optional + 112,
        _ => return None,
    };
    let imports = number(directories + 8, 4)?;
    let sections = optional + number(header + 20, 2)?;

    // An address once loaded to its place in the file
    let file_offset = |address: usize| -> Option<usize> {
        (0..section_count).find_map(|index| {
            let section = sections + index * 40;
            let start = number(section + 12, 4)?;
            let size = number(section + 8, 4)?.max(number(section + 16, 4)?);
            if !(start..start + size).contains(&address) {
                return None;
            }
            Some(address - start + number(section + 20, 4)?)
        })
    };

    // One 20-byte descriptor per DLL, up to an empty one
    let mut names = Vec::new();
    let mut descriptor = file_offset(imports)?;
    while let Some(name) = number(descriptor + 12, 4).filter(|name| *name != 0) {
        let start = file_offset(name)?;
        let length = bytes.get(start..)?.iter().position(|byte| *byte == 0)?;
        names.push(String::from_utf8_lossy(&bytes[start..start + length]).into_owned());
        descriptor += 20;
    }

    Some(names)
}

/// Whether a build links the packages' debug libraries: with the MSVC ABI's debug runtime, which they must match.
pub fn uses_debug_libraries(toolchain: &Toolchain, profile: &Profile) -> bool {
    profile.debug_runtime && toolchain.is_msvc_abi()
}

/// What a project needs from one vcpkg dependency (and everything it depends on).
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
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
    /// A static library among the libraries may hold C++ code (its package says so, or says nothing), so programs
    /// linking it link as C++.
    pub cxx_libraries: bool,
}

/// How CMake users find and link an installed port: `find_package(<name> [CONFIG])`, then its targets.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct CmakePackage {
    pub name: String,
    /// find_package needs CONFIG (a config file only), else CMake's own Find module finds it too.
    pub config: bool,
    pub targets: Vec<String>,
    /// For a `cmake` dependency: the components it asks for, and the folder of the config file the build used.
    pub components: Vec<String>,
    pub hint: Option<PathBuf>,
}

/// The build a package's generator expressions are evaluated for, in CMake's names (configuration, compiler and
/// platform), and while evaluating, the language of the file compiled and whether it is for linking.
#[derive(Debug, Clone)]
pub struct BuildContext {
    family: Family,
    os: Os,
    cpu: Cpu,
    msvc_abi: bool,
    configuration: String,
    compiler_id: &'static str,
    frontend: &'static str,
    language: Option<&'static str>,
    link: bool,

    /// The OpenMP runtime DLLs beside the compiler (clang and clang-cl with the MSVC ABI, MinGW gcc), copied beside
    /// programs; or why there are none for the target's processor.
    openmp_runtime: Result<Vec<PathBuf>, String>,
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

        // The configuration CMake builds for the profile: Debug exactly when the packages' debug libraries are linked,
        // else the first built-in release one along its chain
        let configuration = if uses_debug_libraries(toolchain, profile) {
            "debug"
        } else {
            profile
                .chain
                .iter()
                .map(String::as_str)
                .find(|name| matches!(*name, "release" | "relwithdebinfo" | "minsizerel"))
                .unwrap_or("release")
        };

        BuildContext {
            family: toolchain.family,
            os,
            cpu: toolchain.platform.cpu,
            msvc_abi: toolchain.is_msvc_abi(),
            configuration: crate::targets::config_name(configuration),
            compiler_id,
            frontend: if toolchain.family.is_msvc_style() { "MSVC" } else { "GNU" },
            language: None,
            link: false,
            openmp_runtime: openmp_runtime(toolchain),
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

    /// The configuration packages' files are picked for, in CMake's spelling (`Debug`, `Release`).
    pub(crate) fn configuration(&self) -> &str {
        &self.configuration
    }

    pub(crate) fn os(&self) -> Os {
        self.os
    }

    pub(crate) fn is_msvc_abi(&self) -> bool {
        self.msvc_abi
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
                        requirements.add_link(target, &known, Some(self));
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

            // Its static libraries are C++ unless its CMake files say they are only C
            if libs.iter().any(|lib| is_static_library(lib)) {
                let languages: Vec<&String> = imported
                    .iter()
                    .flat_map(|target| &target.files)
                    .filter_map(|files| files.languages.as_ref())
                    .flatten()
                    .collect();
                usage.cxx_libraries |= languages.is_empty() || languages.iter().any(|language| *language == "CXX");
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
                requirements.add_link(&module, &known, Some(self));
                push_new(&mut usage.runtime, &module.runtime);
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
            ..Default::default()
        })
    }

    /// The imported targets a package's CMake files define, with what each gives its users for `build`.
    fn imported_targets(&self, package: &Package, build: &BuildContext) -> Vec<ImportedTarget> {
        let prefix = ImportPrefix::Fixed(slash(&self.root.join(&self.triplet)));
        let share = format!("{}/share/", self.triplet);
        let cmake_files: Vec<PathBuf> = package
            .files
            .iter()
            .filter(|file| file.starts_with(&share) && file.ends_with(".cmake"))
            .map(|file| self.root.join(file))
            .collect();

        read_imported_targets(&cmake_files, &prefix, build)
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

    Some(CmakePackage {
        name,
        config,
        targets,
        ..Default::default()
    })
}

/// What the imported targets used so far require of their users, and the targets they name.
#[derive(Default)]
pub(crate) struct Requirements {
    /// Targets named for compiling (publicly), and for linking (under $<LINK_ONLY:...> too).
    pub(crate) visible: HashSet<String>,
    pub(crate) linked: HashSet<String>,

    /// Include folders, definitions and compile options for each language of LANGUAGES, each once (options as
    /// written, `SHELL:` ones whole).
    includes: [Vec<PathBuf>; 4],
    defines: [Vec<String>; 4],
    options: [Vec<String>; 4],
    link_options: Vec<String>,
    system_libs: Vec<PathBuf>,
}

impl Requirements {
    /// Adds what users compile with from `target`, and the targets it names publicly.
    pub(crate) fn add_compile(&mut self, target: &ImportedTarget, build: &BuildContext) {
        for (index, (_, language)) in LANGUAGES.iter().enumerate() {
            let build = build.for_language(language);
            for value in &target.include_values {
                let folders: Vec<PathBuf> = evaluate_list(value, &build)
                    .into_iter()
                    .filter(|entry| !entry.contains("${"))
                    .map(|entry| util::normalize(Path::new(&entry)))
                    .filter(|folder| folder.is_dir())
                    .collect();
                push_new(&mut self.includes[index], &folders);
            }
            for value in &target.define_values {
                let defines: Vec<String> = evaluate_list(value, &build).into_iter().filter(|define| !define.contains("${")).collect();
                push_new(&mut self.defines[index], &defines);
            }
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
    pub(crate) fn add_link(&mut self, target: &ImportedTarget, known: &HashSet<&str>, tree: Option<&InstalledTree>) {
        push_new(&mut self.link_options, &target.link_options);

        for item in &target.link_items {
            let path = Path::new(item);
            if item.starts_with('-') {
                push_new(&mut self.link_options, std::slice::from_ref(item));
            } else if item.contains("::") || known.contains(item.as_str()) {
                self.linked.insert(item.clone());
            } else if item.contains('/') || item.contains('\\') {
                // A library file elsewhere (the system's): linked when it exists here
                let outside_tree = tree.is_none_or(|tree| util::strip_prefix_ci(path, &tree.root).is_none());
                if path.is_absolute() && path.is_file() && outside_tree {
                    push_new(&mut self.system_libs, &[path.to_path_buf()]);
                }
            } else if !tree.is_some_and(|tree| tree.has_library_named(item)) {
                push_new(&mut self.system_libs, &[path.to_path_buf()]);
            }
        }
    }

    /// Fills `usage` in: include folders, definitions and options every language's files get, then those of one
    /// language only, as flags.
    pub(crate) fn fill(self, usage: &mut Usage, build: &BuildContext) {
        let (common_includes, language_includes) = split_common(&self.includes);
        push_new(&mut usage.includes, &common_includes);
        for (language, folder) in language_includes {
            usage.language_flags.push((language, format!("-I{}", folder.display())));
        }

        let (common_defines, language_defines) = split_common(&self.defines);
        usage.defines = common_defines;
        for (language, define) in language_defines {
            usage.language_flags.push((language, format!("-D{define}")));
        }

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

/// What every language's list holds, and the rest by language.
fn split_common<T: Clone + PartialEq>(lists: &[Vec<T>; 4]) -> (Vec<T>, Vec<(Language, T)>) {
    let [first, others @ ..] = lists;
    let common: Vec<T> = first
        .iter()
        .filter(|value| others.iter().all(|list| list.contains(value)))
        .cloned()
        .collect();

    let mut by_language = Vec::new();
    for ((language, _), list) in LANGUAGES.iter().zip(lists) {
        for value in list.iter().filter(|value| !common.contains(value)) {
            by_language.push((*language, value.clone()));
        }
    }

    (common, by_language)
}

/// Appends the values `list` does not hold yet.
pub(crate) fn push_new<T: Clone + PartialEq>(list: &mut Vec<T>, values: &[T]) {
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

/// What a built-in system dependency gives its users for `build` (`[deps] openmp = "system"`, likewise `threads`,
/// `opengl` and `vulkan`): the same as the CMake find module targets a package links. None for other names.
pub fn system_dependency(name: &str, build: &BuildContext) -> Option<Usage> {
    let modules: &[&str] = match name {
        "openmp" => &["OpenMP::OpenMP_CXX", "OpenMP::OpenMP_C"],
        "threads" => &["Threads::Threads"],
        "opengl" => &["OpenGL::GL"],
        "vulkan" => &["Vulkan::Vulkan"],
        _ => return None,
    };

    let mut usage = Usage::default();
    let mut requirements = Requirements::default();
    for module in modules.iter().filter_map(|module_name| module_target(module_name, build)) {
        requirements.add_compile(&module, build);
        requirements.add_link(&module, &HashSet::new(), None);
        push_new(&mut usage.runtime, &module.runtime);
    }
    requirements.fill(&mut usage, build);

    Some(usage)
}

/// An imported target of a CMake find module, with what the module gives it for `build`: FindOpenMP's flag (cl's
/// -openmp brings its runtime; clang-cl and clang on Windows link LLVM's libomp, which Visual Studio ships, clang's
/// driver through -fopenmp=libomp), FindThreads' -pthread outside Windows, FindOpenGL's system library, FindVulkan's
/// SDK or system loader.
pub(crate) fn module_target(name: &str, build: &BuildContext) -> Option<ImportedTarget> {
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
                (Family::Clang, Os::Windows) if build.msvc_abi => ("-fopenmp=libomp", None),
                (Family::Clang, Os::Macos) => ("-Xclang -fopenmp", Some("omp")),
                (Family::Clang, _) => ("-fopenmp=libomp", None),
                (Family::Gcc, _) => ("-fopenmp", None),
            };
            target.compile_options.push(format!("$<$<COMPILE_LANGUAGE:{language}>:SHELL:{flag}>"));
            match library {
                // Apple clang has no OpenMP runtime: Homebrew's libomp (keg-only, so in no search path) where it is
                // installed (unverified: no Mac to test on)
                Some("omp") => match ["/opt/homebrew/opt/libomp", "/usr/local/opt/libomp"]
                    .iter()
                    .map(Path::new)
                    .find(|prefix| prefix.join("include/omp.h").is_file())
                {
                    Some(prefix) => {
                        target.include_values.push(slash(&prefix.join("include")));
                        target.link_items.push(slash(&prefix.join("lib/libomp.dylib")));
                    }
                    None => target.link_items.push("omp".to_string()),
                },
                Some(library) => target.link_items.push(library.to_string()),
                None if !flag.contains(' ') && build.frontend == "GNU" => target.link_options.push(flag.to_string()),
                None => {}
            }
            match &build.openmp_runtime {
                Ok(files) => target.runtime.extend(files.iter().cloned()),
                Err(message) => warn_once(message),
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
        "Vulkan::Vulkan" => {
            // FindVulkan's: the SDK VULKAN_SDK names (its library for the target's processor), else the system's loader
            let sdk = std::env::var_os("VULKAN_SDK").filter(|sdk| !sdk.is_empty()).map(PathBuf::from);
            match (sdk, build.os) {
                (Some(sdk), Os::Windows) => {
                    target.include_values.push(slash(&sdk.join("Include")));
                    target.link_items.push(slash(&vulkan_sdk_library(&sdk, build.cpu)));
                }
                (Some(sdk), os) => {
                    let library = if os == Os::Macos { "libvulkan.dylib" } else { "libvulkan.so" };
                    target.include_values.push(slash(&sdk.join("include")));
                    target.link_items.push(slash(&sdk.join("lib").join(library)));
                }
                (None, Os::Windows) => {}
                (None, _) => target.link_items.push("vulkan".into()),
            }
        }
        _ => return None,
    }

    Some(target)
}

/// The Windows Vulkan SDK's loader library for programs of `cpu`: Lib for x64, Lib32 for 32-bit, Lib-ARM64 for ARM64.
pub(crate) fn vulkan_sdk_library(sdk: &Path, cpu: Cpu) -> PathBuf {
    let folder = match cpu {
        Cpu::X64 => "Lib",
        Cpu::X86 | Cpu::Arm => "Lib32",
        Cpu::Arm64 => "Lib-ARM64",
    };

    sdk.join(folder).join("vulkan-1.lib")
}

/// A warning about the OpenMP runtime, once per run however many packages and targets use OpenMP.
fn warn_once(message: &str) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        crate::ui::warn(message);
    }
}

/// The imported targets CMake package files define, with what each gives its users for `build`. One target's
/// properties may be spread over several files (its configurations' files hold the libraries).
pub(crate) fn read_imported_targets(files: &[PathBuf], prefix: &ImportPrefix, build: &BuildContext) -> Vec<ImportedTarget> {
    let link = build.for_link();
    let mut targets: Vec<ImportedTarget> = Vec::new();

    for path in files {
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        let list_dir = slash(path.parent().unwrap());
        let prefix = match prefix {
            ImportPrefix::Fixed(prefix) => prefix.clone(),
            ImportPrefix::PerFile { fallback } => file_import_prefix(&text, path).map_or_else(|| fallback.clone(), |found| slash(&found)),
        };

        // Declarations: add_library(<name> <kind> IMPORTED), and aliases, which pass on what they name
        for (name, kind, aliased) in imported_declarations(&text) {
            let target = target_named(&mut targets, &name);
            target.kind = kind;
            if let Some(aliased) = aliased {
                target.links.push(aliased.clone());
                target.link_items.push(aliased);
            }
        }

        for (name, properties) in target_property_blocks(&text) {
            if name.contains('$') {
                continue;
            }
            let target = target_named(&mut targets, &name);
            target.has_library |= properties.contains("IMPORTED_LOCATION") || properties.contains("IMPORTED_IMPLIB");

            // Include folders and definitions may depend on the file's language too: evaluated for each
            if let Some(value) = property_value(&properties, "INTERFACE_INCLUDE_DIRECTORIES") {
                target.include_values.push(expand_import_prefixes(&value, &prefix, &list_dir));
            }
            target.define_values.extend(property_value(&properties, "INTERFACE_COMPILE_DEFINITIONS"));
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

            // Its files, per configuration
            for (property, value) in property_pairs(&properties) {
                let value = expand_import_prefixes(&value, &prefix, &list_dir);
                if value.contains("${") || value.contains("$<") {
                    continue;
                }
                let found = [
                    "IMPORTED_LOCATION",
                    "IMPORTED_IMPLIB",
                    "IMPORTED_LINK_DEPENDENT_LIBRARIES",
                    "IMPORTED_LINK_INTERFACE_LANGUAGES",
                ]
                .into_iter()
                .find_map(|key| Some((key, configuration_suffix(&property, key)?)));
                let Some((key, configuration)) = found else { continue };
                let files = match target.files.iter().position(|files| files.configuration == configuration) {
                    Some(index) => &mut target.files[index],
                    None => {
                        target.files.push(ImportedFiles {
                            configuration,
                            ..Default::default()
                        });
                        target.files.last_mut().unwrap()
                    }
                };
                match key {
                    "IMPORTED_LOCATION" => files.location = Some(util::normalize(Path::new(&value))),
                    "IMPORTED_IMPLIB" => files.import_library = Some(util::normalize(Path::new(&value))),
                    "IMPORTED_LINK_INTERFACE_LANGUAGES" => {
                        files.languages = Some(value.split(';').filter(|item| !item.is_empty()).map(String::from).collect())
                    }
                    _ => files.dependent_libraries = value.split(';').filter(|item| !item.is_empty()).map(String::from).collect(),
                }
            }
        }
    }

    targets
}

/// The target of this name in `targets`, added when it is not there yet.
fn target_named<'a>(targets: &'a mut Vec<ImportedTarget>, name: &str) -> &'a mut ImportedTarget {
    let index = match targets.iter().position(|target| target.name == name) {
        Some(index) => index,
        None => {
            targets.push(ImportedTarget {
                name: name.to_string(),
                ..Default::default()
            });
            targets.len() - 1
        }
    };

    &mut targets[index]
}

/// A static library file (libz.a, z.lib), not a shared one or a MinGW import library (libz.dll.a).
pub(crate) fn is_static_library(file: &Path) -> bool {
    let name = file.file_name().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
    (name.ends_with(".a") && !name.ends_with(".dll.a")) || name.ends_with(".lib")
}

/// `IMPORTED_LOCATION_RELEASE` for `IMPORTED_LOCATION`: "RELEASE"; the property itself: "".
fn configuration_suffix(property: &str, key: &str) -> Option<String> {
    let rest = property.strip_prefix(key)?;
    match rest.strip_prefix('_') {
        Some(configuration) => Some(configuration.to_string()),
        None if rest.is_empty() => Some(String::new()),
        None => None,
    }
}

/// Each `add_library(<name> <kind> IMPORTED)` (kind SHARED, STATIC, ...) and `add_library(<name> ALIAS <target>)` of a
/// package file: the name, the kind, and the target an alias names.
fn imported_declarations(text: &str) -> Vec<(String, String, Option<String>)> {
    static IMPORTED: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"add_library\(\s*([^\s)$]+)\s+(SHARED|STATIC|MODULE|UNKNOWN|INTERFACE|OBJECT)\s+IMPORTED").unwrap()
    });
    static ALIAS: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"add_library\(\s*([^\s)$]+)\s+ALIAS\s+([^\s)$]+)\s*\)").unwrap());

    let mut declarations: Vec<(String, String, Option<String>)> = IMPORTED
        .captures_iter(text)
        .map(|captures| (captures[1].to_string(), captures[2].to_string(), None))
        .collect();
    declarations.extend(
        ALIAS
            .captures_iter(text)
            .map(|captures| (captures[1].to_string(), "INTERFACE".to_string(), Some(captures[2].to_string()))),
    );

    declarations
}

/// The install prefix a package file computes for `${_IMPORT_PREFIX}` or `${PACKAGE_PREFIX_DIR}`: its own folder, up as
/// many levels as it climbs (`get_filename_component(_IMPORT_PREFIX "${_IMPORT_PREFIX}" PATH)` once per level, or
/// `"${CMAKE_CURRENT_LIST_DIR}/../../.." ABSOLUTE`). None when the file computes none.
pub(crate) fn file_import_prefix(text: &str, file: &Path) -> Option<PathBuf> {
    static RELATIVE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r#"get_filename_component\(\s*(?:_IMPORT_PREFIX|PACKAGE_PREFIX_DIR)\s+"\$\{CMAKE_CURRENT_LIST_DIR\}((?:/\.\.)*)/?"\s+ABSOLUTE\s*\)"#,
        )
        .unwrap()
    });
    static FROM_FILE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"get_filename_component\(\s*_IMPORT_PREFIX\s+"\$\{CMAKE_CURRENT_LIST_FILE\}"\s+PATH\s*\)"#).unwrap()
    });
    static UP: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"get_filename_component\(\s*_IMPORT_PREFIX\s+"\$\{_IMPORT_PREFIX\}"\s+PATH\s*\)"#).unwrap());

    let folder = util::normalize(file.parent()?);
    let levels = match RELATIVE.captures(text) {
        Some(captures) => captures[1].matches("..").count(),
        None if FROM_FILE.is_match(text) => UP.find_iter(text).count(),
        None => return None,
    };

    folder.ancestors().nth(levels).map(Path::to_path_buf)
}

/// A set_target_properties body's `PROPERTY value` pairs after `PROPERTIES`, values unquoted.
fn property_pairs(properties: &str) -> Vec<(String, String)> {
    let Some(start) = properties.find("PROPERTIES") else { return Vec::new() };
    let mut words = Vec::new();
    let mut characters = properties[start + "PROPERTIES".len()..].chars().peekable();

    // Words: quoted (with \" and \$ escapes) or bare
    while let Some(character) = characters.next() {
        if character.is_whitespace() {
            continue;
        }
        let mut word = String::new();
        if character == '"' {
            while let Some(character) = characters.next() {
                match character {
                    '\\' => word.extend(characters.next()),
                    '"' => break,
                    _ => word.push(character),
                }
            }
        } else {
            word.push(character);
            while let Some(character) = characters.next_if(|character| !character.is_whitespace()) {
                word.push(character);
            }
        }
        words.push(word);
    }

    words
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[property, value]| (property.clone(), value.clone()))
        .collect()
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
            cpu: Cpu::X64,
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
            openmp_runtime: Ok(Vec::new()),
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

        // clang's driver links LLVM's libomp itself, from -fopenmp=libomp (a bare libomp.lib is a file to it)
        let openmp = module_target("OpenMP::OpenMP_CXX", &build(Family::Clang, Os::Windows, "Debug")).unwrap();
        assert!(openmp.link_items.is_empty());
        assert_eq!(openmp.link_options, vec!["-fopenmp=libomp"]);
    }

    #[test]
    fn system_dependencies_are_find_modules() {
        let gcc = build(Family::Gcc, Os::Linux, "Release");
        let openmp = system_dependency("openmp", &gcc).unwrap();
        let language_flag = |language: Language, flag: &str| (language, flag.to_string());
        assert_eq!(
            openmp.language_flags,
            vec![language_flag(Language::Cxx, "-fopenmp"), language_flag(Language::C, "-fopenmp")]
        );
        assert_eq!(openmp.link_flags, vec!["-fopenmp"]);

        let threads = system_dependency("threads", &gcc).unwrap();
        assert_eq!(
            (threads.flags, threads.link_flags),
            (vec!["-pthread".to_string()], vec!["-pthread".to_string()])
        );

        let msvc = build(Family::Msvc, Os::Windows, "Debug");
        let opengl = system_dependency("opengl", &msvc).unwrap();
        assert_eq!(opengl.libs, vec![PathBuf::from("opengl32")]);
        assert_eq!(system_dependency("openmp", &msvc).unwrap().language_flags.len(), 2);
        assert!(system_dependency("user32", &msvc).is_none());
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
    #[cfg(windows)]
    fn reads_dll_imports() {
        // This test program imports kernel32.dll; a file that is not a program imports nothing
        let imports = imported_dlls(&std::env::current_exe().unwrap());
        assert!(imports.iter().any(|name| name.eq_ignore_ascii_case("kernel32.dll")), "{imports:?}");
        assert!(imported_dlls(Path::new(file!())).is_empty());
        assert_eq!(pe_imports(b"MZ"), None);
    }

    #[test]
    #[cfg(windows)]
    fn reads_the_processor_a_dll_is_for() {
        // This test program is built for the processor it runs on; a file that is not a program is for none
        let host = crate::platform::Platform::host().cpu;
        assert_eq!(file_machine(&std::env::current_exe().unwrap()), Some(pe_machine_of(host)));
        assert_eq!(file_machine(Path::new(file!())), None);
        assert_eq!(pe_machine(b"MZ"), None);
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
  INTERFACE_COMPILE_DEFINITIONS "SDL_ALL;\$<\$<COMPILE_LANGUAGE:CXX>:SDL_CXX>"
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
        assert_eq!(usage.defines, vec!["SDL_ALL"]);
        assert_eq!(
            usage.language_flags,
            vec![(Language::Cxx, "-DSDL_CXX".to_string()), (Language::Cxx, "-openmp".to_string())]
        );
        assert_eq!(usage.link_flags, vec!["/LTCG", "-ignore:4221"]);
        assert!(tree.usage("missing", false, None, &build(Family::Msvc, Os::Windows, "Release")).is_err());

        std::fs::remove_dir_all(&root).ok();
    }
}
