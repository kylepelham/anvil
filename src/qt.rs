//! Qt's code generators, run as CMake's AUTOMOC, AUTOUIC and AUTORCC run them, for every target that can include Qt
//! (a `cmake = "Qt6"` or Qt vcpkg dependency it uses): moc over the headers and sources with Q_OBJECT and its kin, uic
//! over `.ui` forms, rcc over `.qrc` resources. Each run is a planned action with its inputs, outputs and dependency
//! file (crate::plan); what they write goes to `<out>/qt/<target>/`. Programs built for Windows also get the Qt
//! plugins they load copied beside them.

use crate::cmake_package::{Found, Request};
use crate::flags::path_text;
use crate::platform::Os;
use crate::profile::Profile;
use crate::project::Model;
use crate::targets::{self, Kind, Target};
use crate::toolchain::{Family, Toolchain};
use crate::util::{self, slash};
use crate::vcpkg_tree::{self, BuildContext, CmakePackage, ImportPrefix, Usage};
use anyhow::{bail, Context, Result};
use regex::Regex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, OnceLock};

// The macros that make moc write code, as AUTOMOC looks for them (at a line's start, after a `{` too)
const MOC_MACROS: &str = r"(?m)^[ \t]*\{?[ \t]*(?:Q_OBJECT|Q_GADGET|Q_GADGET_EXPORT|Q_NAMESPACE|Q_NAMESPACE_EXPORT)\b";

// Header extensions AUTOMOC tries beside a source, and the C++ sources moc reads
const HEADER_EXTENSIONS: &[&str] = &["h", "hh", "h++", "hm", "hpp", "hxx"];
const SOURCE_EXTENSIONS: &[&str] = &["cpp", "cc", "cxx", "c++", "cp", "mm"];

// The first line of a target's scan cache: a file without it (another version's) is read again
const SCAN_CACHE_HEADER: &str = "# anvil qt scans 2: time, moc macro, generated includes, path";

// Folders under a Qt prefix that hold its tools ({major}: 5 or 6), most specific first: distributions' lib/qt6/libexec,
// vcpkg's tools/Qt6/bin, Homebrew's share/qt/libexec, then bin/ (Qt's own installs; on Linux /usr/bin/moc may be
// another Qt's)
const TOOL_FOLDERS: &[&str] = &[
    "lib/qt{major}/libexec",
    "lib/qt{major}/bin",
    "libexec",
    "tools/Qt{major}/bin",
    "tools/qt{major}/bin",
    "share/qt/libexec",
    "bin",
];

/// A Qt installation as a dependency found it: its CMake packages' folder (`lib/cmake`), its name and version.
#[derive(Debug)]
pub struct Qt {
    /// `Qt6` or `Qt5`: the prefix of its packages and targets.
    pub name: String,
    pub version: Vec<u64>,
    cmake_dir: PathBuf,

    // Its tools, found once
    moc: OnceLock<std::result::Result<PathBuf, String>>,
    uic: OnceLock<std::result::Result<PathBuf, String>>,
    rcc: OnceLock<std::result::Result<PathBuf, String>>,
}

/// What a target compiles with, as moc must see it.
pub struct CompileSettings<'a> {
    pub includes: &'a [PathBuf],
    pub system_includes: &'a [PathBuf],
    pub defines: &'a [String],
}

/// One run of a Qt tool (or of the compiler, for the macros it predefines, which moc reads).
#[derive(Debug)]
pub struct Step {
    pub description: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub inputs: Vec<PathBuf>,
    pub outputs: Vec<PathBuf>,
    /// A make-style file of what it read.
    pub depfile: Option<PathBuf>,
    /// Runs after the first step (moc after the predefined macros are written).
    pub after_first: bool,
}

/// A target's Qt steps, and the files they write that compile into it.
#[derive(Debug, Default)]
pub struct Generated {
    pub steps: Vec<Step>,
    pub sources: Vec<PathBuf>,
}

/// What a file holds for Qt's tools: a moc macro, and the generated files it includes (`x.moc`, `moc_x.cpp`, `ui_x.h`).
#[derive(Debug, Clone, Default, PartialEq)]
struct Scan {
    has_macro: bool,
    includes: Vec<String>,
}

/// Scans of the files a target's moc may read, kept beside its generated files with each file's time, so a build
/// reads only the files that changed.
struct ScanCache {
    file: PathBuf,
    entries: HashMap<String, (i64, Scan)>,
    /// The times of each folder's files, from one listing (Windows; elsewhere a file's own stat is cheap).
    folder_times: HashMap<PathBuf, Option<HashMap<String, i64>>>,
    /// This build's files' lines, by path.
    lines: BTreeMap<String, String>,
}

impl Qt {
    /// The Qt a dependency is: an installed `Qt6`/`Qt5` CMake package (or one of its modules), or a Qt port of vcpkg.
    fn of_dependency(model: &Model, external: &Target) -> Option<Qt> {
        if let Some(package) = &external.cmake_package {
            let major = qt_major(&package.name)?;
            let cmake_dir = package.hint.as_ref()?.parent()?.to_path_buf();
            return Some(Qt::new(&format!("Qt{major}"), cmake_dir));
        }

        let (port, _) = external.vcpkg.as_ref()?;
        let port = port.to_lowercase();
        if !port.starts_with("qt") {
            return None;
        }
        let name = if port.starts_with("qt5") { "Qt5" } else { "Qt6" };
        let cmake_dir = model.vcpkg_dir.join("share");
        cmake_dir.join(format!("{name}Core")).is_dir().then(|| Qt::new(name, cmake_dir))
    }

    fn new(name: &str, cmake_dir: PathBuf) -> Qt {
        let version = read_version(&cmake_dir, name).unwrap_or_default();

        Qt {
            name: name.to_string(),
            version,
            cmake_dir,
            moc: OnceLock::new(),
            uic: OnceLock::new(),
            rcc: OnceLock::new(),
        }
    }

    /// moc writes a make-style dependency file from Qt 5.15 on.
    fn moc_writes_depfile(&self) -> bool {
        self.version.as_slice() >= [5, 15].as_slice()
    }

    /// A tool's program: as the package declares it (Qt6::moc in Qt6CoreTools, Qt5::moc in Qt5Core's extras), else
    /// where Qt installs keep it.
    fn tool(&self, toolchain: &Toolchain, profile: &Profile, tool: &str) -> Result<PathBuf> {
        let cell = match tool {
            "moc" => &self.moc,
            "uic" => &self.uic,
            _ => &self.rcc,
        };
        let found = cell.get_or_init(|| self.find_tool(toolchain, profile, tool));

        found.clone().map_err(|message| anyhow::anyhow!("{message}"))
    }

    fn find_tool(&self, toolchain: &Toolchain, profile: &Profile, tool: &str) -> std::result::Result<PathBuf, String> {
        let name = &self.name;
        let module = if tool == "uic" { "Widgets" } else { "Core" };

        // Declared by the package
        let declared = match name.as_str() {
            "Qt5" => self.qt5_tool(module, tool),
            _ => self.declared_tool(toolchain, profile, &format!("{name}{module}Tools"), tool),
        };
        if let Some(program) = declared.filter(|program| program.is_file()) {
            return Ok(program);
        }

        // Where installs keep them, under the prefix and its parents
        let file_name = if cfg!(windows) { format!("{tool}.exe") } else { tool.to_string() };
        let major = &name[2..];
        for folder in TOOL_FOLDERS {
            for base in self.cmake_dir.ancestors().skip(1).take(3) {
                let candidate = base.join(folder.replace("{major}", major)).join(&file_name);
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }

        let package = if tool == "uic" { "Widgets" } else { "Core" };
        Err(format!(
            "Qt's {tool} was not found: {} declares no {name}::{tool}, and no {file_name} is in the Qt installation's bin/ or libexec/ \
             (looked under {}); install Qt's development tools (Debian and Ubuntu: {}), or set `qt = false` on the target and run {tool} \
             in a build step",
            slash(
                &self
                    .cmake_dir
                    .join(format!("{name}{package}{}", if name == "Qt5" { "" } else { "Tools" }))
            ),
            slash(self.cmake_dir.ancestors().nth(3).unwrap_or(&self.cmake_dir)),
            if name == "Qt5" { "qtbase5-dev-tools" } else { "qt6-base-dev-tools" }
        ))
    }

    /// Qt 6's imported executable `<name>::<tool>` from a tools package's target files.
    fn declared_tool(&self, toolchain: &Toolchain, profile: &Profile, package: &str, tool: &str) -> Option<PathBuf> {
        let folder = self.cmake_dir.join(package);
        let files = files_named(&folder, |file| file.starts_with(&format!("{package}Targets")) && file.ends_with(".cmake"));

        // The prefix the main targets file computes, which its configurations' files use
        let main = folder.join(format!("{package}Targets.cmake"));
        let prefix = vcpkg_tree::file_import_prefix(&std::fs::read_to_string(&main).ok()?, &main)?;
        let fallback = ImportPrefix::PerFile { fallback: slash(&prefix) };
        let build = BuildContext::new(toolchain, profile);
        let wanted = format!("{}::{tool}", self.name);

        let targets = vcpkg_tree::read_imported_targets(&files, &fallback, &build);
        let target = targets.iter().find(|target| target.name == wanted)?;
        target
            .files
            .iter()
            .filter_map(|files| files.location.clone())
            .find(|location| location.is_file())
    }

    /// Qt 5's tools: `add_executable(Qt5::moc IMPORTED)` then `set(imported_location "${_qt5Core_install_prefix}/...")`
    /// in Qt5CoreConfigExtras.cmake, the prefix computed in Qt5CoreConfig.cmake from its own folder.
    fn qt5_tool(&self, module: &str, tool: &str) -> Option<PathBuf> {
        static PREFIX: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"get_filename_component\(\s*_qt5\w+_install_prefix\s+"\$\{CMAKE_CURRENT_LIST_DIR\}((?:/\.\.)*)/?"\s+ABSOLUTE\s*\)"#).unwrap()
        });
        let folder = self.cmake_dir.join(format!("Qt5{module}"));
        let extras = std::fs::read_to_string(folder.join(format!("Qt5{module}ConfigExtras.cmake"))).ok()?;
        let config = std::fs::read_to_string(folder.join(format!("Qt5{module}Config.cmake"))).ok()?;

        let location = Regex::new(&format!(
            r#"add_executable\(\s*Qt5::{tool}\s+IMPORTED\s*\)\s*set\(\s*imported_location\s+"\$\{{_qt5\w+_install_prefix\}}/([^"]+)"\s*\)"#
        ))
        .ok()?;
        let relative = location.captures(&extras)?[1].to_string();
        let levels = PREFIX.captures(&config)?[1].matches("..").count();
        let prefix = folder.ancestors().nth(levels)?;

        Some(prefix.join(relative))
    }
}

/// The Qt each target that compiles uses (through a dependency it can include, as its compiles see them), unless the
/// target says `qt = false`.
pub fn installations(model: &Model) -> HashMap<usize, Arc<Qt>> {
    let mut found = HashMap::new();
    let dependencies: HashMap<usize, Arc<Qt>> = model
        .targets
        .iter()
        .enumerate()
        .filter(|(_, target)| target.kind == Kind::External)
        .filter_map(|(index, target)| Qt::of_dependency(model, target).map(|qt| (index, Arc::new(qt))))
        .collect();
    if dependencies.is_empty() {
        return found;
    }

    for (index, target) in model.targets.iter().enumerate() {
        let compiles = !matches!(target.kind, Kind::External | Kind::Headers | Kind::Custom);
        if !compiles || !target.qt {
            continue;
        }
        let used = compile_closure(model, index)
            .into_iter()
            .find_map(|dependency| dependencies.get(&dependency));
        if let Some(qt) = used {
            found.insert(index, qt.clone());
        }
    }

    found
}

/// A Qt 5 package (`Qt5`, `Qt5Widgets`), which `read_qt5` reads.
pub fn is_qt5_package(package: &str) -> bool {
    qt_major(package) == Some(5)
}

/// What a Qt 5 package gives its users. Its files follow one template (Qt5BasicConfig.cmake.in) that sets variables
/// running CMake would fill in: the install prefix computed from the package's folder, each module's include folders,
/// definitions, library files per configuration and the modules it links (`_Qt5Widgets_LIB_DEPENDENCIES`).
pub fn read_qt5(request: &Request, folder: &Path, toolchain: &Toolchain, profile: &Profile) -> Result<Found> {
    let dependency = request.dependency;
    let cmake_dir = folder.parent().context("a Qt 5 package folder has a parent")?;
    let debug = vcpkg_tree::uses_debug_libraries(toolchain, profile);

    // The modules asked for: components of `Qt5`, the module a `Qt5Widgets` package is, or targets named
    let mut pending: Vec<String> = match (request.targets, request.package.len() > 3) {
        (Some(targets), _) => targets
            .iter()
            .map(|target| target.strip_prefix("Qt5::").unwrap_or(target).to_string())
            .collect(),
        (None, true) => vec![request.package[3..].to_string()],
        (None, false) => request.components.to_vec(),
    };
    if pending.is_empty() {
        bail!("dependency `{dependency}`: Qt 5 needs the modules it uses: `{dependency} = {{ cmake = \"Qt5\", components = [\"Widgets\"] }}`");
    }
    let wanted: Vec<String> = pending.iter().map(|module| format!("Qt5::{module}")).collect();

    // Each module, then the modules it links, each after those that link it
    let mut modules: Vec<Qt5Module> = Vec::new();
    while let Some(name) = pending.pop() {
        if let Some(position) = modules.iter().position(|module| module.name == name) {
            let module = modules.remove(position);
            modules.push(module);
            continue;
        }
        let module = Qt5Module::read(cmake_dir, &name, debug).with_context(|| format!("dependency `{dependency}`"))?;
        pending.extend(module.links.iter().rev().cloned());
        modules.push(module);
    }

    // What they give: include folders, definitions (QT_NO_DEBUG unless Qt's debug libraries are linked), options,
    // libraries, and the DLLs (Windows) or rpaths (elsewhere) to run with them
    let mut usage = Usage::default();
    let windows = toolchain.platform.os == Os::Windows;
    for module in &modules {
        vcpkg_tree::push_new(&mut usage.includes, &module.includes);
        vcpkg_tree::push_new(&mut usage.defines, &module.defines);
        vcpkg_tree::push_new(&mut usage.flags, &module.flags);
        match windows {
            true => {
                vcpkg_tree::push_new(&mut usage.libs, module.import_library.as_slice());
                vcpkg_tree::push_new(&mut usage.runtime, module.library.as_slice());
            }
            false => {
                vcpkg_tree::push_new(&mut usage.libs, module.library.as_slice());
                let folder = module.library.as_ref().and_then(|library| library.parent()).map(slash);
                let system = folder.as_deref().is_some_and(|folder| {
                    ["/lib", "/usr/lib"]
                        .iter()
                        .any(|system| folder == *system || folder.starts_with(&format!("{system}/")))
                });
                if let Some(folder) = folder.filter(|_| !system) {
                    vcpkg_tree::push_new(&mut usage.link_flags, &[format!("-Wl,-rpath,{folder}")]);
                }
            }
        }
    }
    if !debug {
        usage.defines.push("QT_NO_DEBUG".to_string());
    }
    usage
        .includes
        .retain(|folder| !matches!(slash(folder).trim_end_matches('/'), "/usr/include" | "/usr/local/include"));

    Ok(Found {
        package: CmakePackage {
            name: request.package.to_string(),
            config: true,
            targets: wanted,
            components: request.components.to_vec(),
            hint: Some(folder.to_path_buf()),
        },
        usage,
    })
}

/// One Qt 5 module's package (`Qt5Widgets/Qt5WidgetsConfig.cmake`, read with its extras).
struct Qt5Module {
    name: String,
    includes: Vec<PathBuf>,
    defines: Vec<String>,
    flags: Vec<String>,
    /// The modules it links (Gui, Core).
    links: Vec<String>,
    /// The library (a DLL on Windows) and its import library, in the configuration the build links.
    library: Option<PathBuf>,
    import_library: Option<PathBuf>,
}

impl Qt5Module {
    fn read(cmake_dir: &Path, name: &str, debug: bool) -> Result<Qt5Module> {
        static PREFIX: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"get_filename_component\(\s*_qt5\w+_install_prefix\s+"\$\{CMAKE_CURRENT_LIST_DIR\}((?:/\.\.)*)/?"\s+ABSOLUTE\s*\)"#).unwrap()
        });
        static PREFIX_VARIABLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\{_qt5\w+_install_prefix\}").unwrap());
        static QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""([^"]*)""#).unwrap());

        // Its config file, and its install prefix
        let folder = cmake_dir.join(format!("Qt5{name}"));
        let config_file = folder.join(format!("Qt5{name}Config.cmake"));
        let Ok(text) = std::fs::read_to_string(&config_file) else {
            let mut available: Vec<String> = files_named(cmake_dir, |file| file.starts_with("Qt5") && file.len() > 3)
                .iter()
                .map(|folder| file_name(folder)[3..].to_string())
                .collect();
            available.dedup();
            bail!(
                "Qt 5 has no module `{name}`: no {} (it has {})",
                slash(&config_file),
                available.join(", ")
            );
        };
        let levels = PREFIX
            .captures(&text)
            .map(|captures| captures[1].matches("..").count())
            .with_context(|| format!("{} computes no install prefix Anvil can read", slash(&config_file)))?;
        let prefix = slash(folder.ancestors().nth(levels).unwrap_or(&folder));
        let expand = |value: &str| PREFIX_VARIABLE.replace_all(value, prefix.as_str()).into_owned();
        let setting = |variable: &str| -> Option<String> {
            let pattern = Regex::new(&format!(r"set\(\s*{}\s+([^)]*)\)", regex::escape(variable))).ok()?;
            pattern.captures(&text).map(|captures| captures[1].trim().to_string())
        };

        // Include folders, definitions, the modules it links
        let mut includes: Vec<PathBuf> = setting(&format!("_Qt5{name}_OWN_INCLUDE_DIRS"))
            .map(|value| {
                QUOTED
                    .captures_iter(&value)
                    .map(|captures| util::normalize(Path::new(&expand(&captures[1]))))
                    .collect()
            })
            .unwrap_or_default();
        let defines: Vec<String> = setting(&format!("Qt5{name}_COMPILE_DEFINITIONS"))
            .map(|value| value.split_whitespace().map(String::from).collect())
            .unwrap_or_default();
        let links: Vec<String> = setting(&format!("_Qt5{name}_LIB_DEPENDENCIES"))
            .map(|value| {
                value
                    .trim_matches('"')
                    .split(';')
                    .filter_map(|item| item.strip_prefix("Qt5::"))
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();

        // Core's extras: the mkspecs folder its headers include, and -fPIC where Qt was built with reduced relocations
        let mut flags = Vec::new();
        if name == "Core" {
            let mkspecs = std::fs::read_to_string(folder.join("Qt5CoreConfigExtrasMkspecDir.cmake")).unwrap_or_default();
            if let Some(captures) = QUOTED.captures(&mkspecs) {
                includes.push(util::normalize(Path::new(&expand(&captures[1]))));
            }
            let extras = std::fs::read_to_string(folder.join("Qt5CoreConfigExtras.cmake")).unwrap_or_default();
            if extras.contains("INTERFACE_COMPILE_OPTIONS -fPIC") {
                flags.push("-fPIC".to_string());
            }
        }

        // Its files: `_populate_Widgets_target_properties(RELEASE "<library>" "<import library>" ...)` joined to the
        // folders `imported_location` and `imported_implib` name
        let location = Regex::new(r#"set\(\s*imported_location\s+"([^"]*)\$\{LIB_LOCATION\}"\s*\)"#).unwrap();
        let implib = Regex::new(r#"set\(\s*imported_implib\s+"([^"]*)\$\{IMPLIB_LOCATION\}"\s*\)"#).unwrap();
        let populate = Regex::new(&format!(r#"_populate_{name}_target_properties\(\s*(\w+)\s+"([^"]*)"\s+"([^"]*)""#)).unwrap();
        let location_folder = location.captures(&text).map(|captures| expand(&captures[1]));
        let implib_folder = implib.captures(&text).map(|captures| expand(&captures[1]));
        let configurations: Vec<(String, String, String)> = populate
            .captures_iter(&text)
            .map(|captures| (captures[1].to_string(), captures[2].to_string(), captures[3].to_string()))
            .collect();
        let wanted = if debug { "DEBUG" } else { "RELEASE" };
        let chosen = configurations
            .iter()
            .find(|(configuration, _, _)| configuration == wanted)
            .or(configurations.first());
        let join = |folder: &Option<String>, file: &str| -> Option<PathBuf> {
            let folder = folder.as_ref().filter(|_| !file.is_empty())?;
            Some(util::normalize(Path::new(&format!("{folder}{file}"))))
        };
        let library = chosen.and_then(|(_, library, _)| join(&location_folder, library));
        let import_library = chosen.and_then(|(_, _, import_library)| join(&implib_folder, import_library));

        Ok(Qt5Module {
            name: name.to_string(),
            includes,
            defines,
            flags,
            links,
            library,
            import_library,
        })
    }
}

/// Where a target's generated Qt files go; `include/` in it is on its include path.
pub fn output_dir(model: &Model, target: &Target) -> PathBuf {
    model
        .package_out_dir(target.package)
        .join("qt")
        .join(targets::sanitize_name(&target.name))
}

/// `.ui` and `.qrc` files in the sources of a target that does not use Qt: an error saying what it needs.
pub fn check_sources(target: &Target) -> Result<()> {
    let Some(file) = target.sources.iter().find(|source| matches!(extension(source).as_str(), "ui" | "qrc")) else {
        return Ok(());
    };

    match target.qt {
        true => bail!(
            "{}: .{} files are built by Qt's tools, and `{}` uses no Qt: add a Qt dependency to its deps \
             (`qt = {{ cmake = \"Qt6\", components = [\"Widgets\"] }}` in [deps])",
            slash(file),
            extension(file),
            target.name
        ),
        false => bail!(
            "{}: .{} files are built by Qt's tools, which `qt = false` turns off for `{}`; take it out of `sources` \
             (a build step can run the tool), or drop `qt = false`",
            slash(file),
            extension(file),
            target.name
        ),
    }
}

/// The moc, uic and rcc runs a target needs, and the sources they write that compile into it.
pub fn plan(model: &Model, index: usize, qt: &Qt, settings: &CompileSettings) -> Result<Generated> {
    let target = &model.targets[index];
    let folder = output_dir(model, target);
    let include = folder.join("include");
    let mut cache = ScanCache::load(folder.join("scan.txt"));
    let mut generated = Generated::default();

    // The target's files by kind: forms, resources, headers it lists, and C++ sources
    let mut forms = Vec::new();
    let mut resources = Vec::new();
    let mut headers = Vec::new();
    let mut sources = Vec::new();
    for file in target.sources.iter().chain(&conventional_files(model, target)?) {
        let extension = extension(file);
        match extension.as_str() {
            "ui" => forms.push(file.clone()),
            "qrc" => resources.push(file.clone()),
            _ if HEADER_EXTENSIONS.contains(&extension.as_str()) => headers.push(file.clone()),
            _ if SOURCE_EXTENSIONS.contains(&extension.as_str()) => sources.push(file.clone()),
            _ => {}
        }
    }

    // What the sources include and hold; a form a source includes the header of is found beside it, as AUTOUIC does
    let mut source_scans = Vec::new();
    for source in &sources {
        let scan = cache.scan(source);
        for name in &scan.includes {
            let Some(form) = name.strip_prefix("ui_").and_then(|rest| rest.strip_suffix(".h")) else {
                continue;
            };
            let beside = source.with_file_name(format!("{form}.ui"));
            if !forms.iter().any(|known| stem(known) == form) && beside.is_file() {
                forms.push(beside);
            }
        }
        source_scans.push((source, scan));
    }

    // Headers moc may read: those listed, those beside a source with its name (`x.h`, `x_p.h`), and those of the
    // target's own include folders with a source's name; with the layout convention, any other header of its own
    // folders with a moc macro, unless the convention dropped the sources beside it
    let (named, others) = candidate_headers(model, target, &sources)?;
    headers.extend(named);
    let mut seen = HashSet::new();
    headers.retain(|header| seen.insert(util::path_key(header)));
    let mut moc_headers: Vec<PathBuf> = headers.into_iter().filter(|header| cache.scan(header).has_macro).collect();
    let compiled: HashSet<String> = target.sources.iter().map(|source| util::path_key(source)).collect();
    let mut dropped_folders: HashMap<PathBuf, bool> = HashMap::new();
    for header in others {
        if !seen.insert(util::path_key(&header)) || !cache.scan(&header).has_macro {
            continue;
        }
        let folder = header.parent().unwrap_or(Path::new("")).to_path_buf();
        let dropped = *dropped_folders
            .entry(folder)
            .or_insert_with_key(|folder| has_dropped_sources(folder, &compiled));
        if !dropped {
            moc_headers.push(header);
        }
    }
    cache.save()?;

    // Sources: one with a moc macro includes `<name>.moc`, which moc writes from it (into include/, or the include's
    // folder there: "sub/<name>.moc"); `moc_<name>.cpp` a source includes is a header's, written beside the forms'
    // headers instead of compiled alone
    let mut source_mocs = Vec::new();
    let mut moc_writers: HashMap<String, &Path> = HashMap::new();
    let mut included_header_mocs = HashSet::new();
    for (source, scan) in &source_scans {
        let own = format!("{}.moc", stem(source));
        let mut own_include = None;
        for name in &scan.includes {
            if name.ends_with(".moc") {
                if file_name(Path::new(name)) != own {
                    bail!(
                        "{}: includes \"{name}\", which would be moc's output for another file; a source includes only its own, \"{own}\"",
                        slash(source)
                    );
                }
                own_include.get_or_insert(name);
            }
            if let Some(header) = name.strip_prefix("moc_").and_then(|rest| rest.strip_suffix(".cpp")) {
                if !moc_headers.iter().any(|known| stem(known) == header) {
                    bail!(
                        "{}: includes \"{name}\", and no header {header}.h with Q_OBJECT (or Q_GADGET, Q_NAMESPACE) was found beside a source \
                         or in the target's include folders; list the header in `sources`",
                        slash(source)
                    );
                }
                included_header_mocs.insert(header.to_string());
            }
        }

        match (scan.has_macro, own_include) {
            (_, Some(name)) => {
                let output = source_moc_output(source, name, &include, &mut moc_writers)?;
                source_mocs.push((source.to_path_buf(), output));
            }
            (true, None) => bail!(
                "{}: a class with Q_OBJECT (or Q_GADGET, Q_NAMESPACE) in a source file needs `#include \"{own}\"` at the end of the file, \
                 which moc writes; or move the class to a header",
                slash(source)
            ),
            (false, None) => {}
        }
    }

    // The compiler's predefined macros, which moc reads with GNU compilers (cl's come from the definitions)
    let has_moc = !moc_headers.is_empty() || !source_mocs.is_empty();
    let predefs = folder.join("moc_predefs.h");
    let reads_predefs = has_moc && matches!(model.toolchain.family, Family::Gcc | Family::Clang);
    if reads_predefs {
        let empty = folder.join("moc_predefs.cpp");
        util::write_if_changed(&empty, b"")?;
        let mut args = vec![format!("-std={}", target.std)];
        args.extend(["-dM", "-E", "-x", "c++"].map(String::from));
        args.extend([path_text(&empty), "-o".to_string(), path_text(&predefs)]);

        generated.steps.push(Step {
            description: format!("Reading the compiler's predefined macros for `{}`", target.name),
            program: model.toolchain.cxx.clone(),
            args,
            inputs: vec![empty],
            outputs: vec![predefs.clone()],
            depfile: None,
            after_first: false,
        });
    }

    // moc: headers into moc_<name>.cpp (compiled, or included by a source), sources into <name>.moc
    let moc = match has_moc {
        true => qt.tool(&model.toolchain, &model.profile, "moc")?,
        false => PathBuf::new(),
    };
    let header_outputs = moc_output_names(&moc_headers);
    for (header, name) in moc_headers.iter().zip(header_outputs) {
        let included = included_header_mocs.contains(&stem(header));
        let output = match included {
            true => include.join(format!("moc_{}.cpp", stem(header))),
            false => folder.join(name),
        };
        let predefs = reads_predefs.then_some(predefs.as_path());
        generated.steps.push(moc_step(qt, &moc, settings, predefs, header, &output, &folder));
        if !included {
            generated.sources.push(output);
        }
    }
    for (source, output) in &source_mocs {
        let predefs = reads_predefs.then_some(predefs.as_path());
        generated.steps.push(moc_step(qt, &moc, settings, predefs, source, output, &folder));
    }

    // uic: forms into ui_<name>.h, which the sources include
    check_unique_names(&forms, "form")?;
    if !forms.is_empty() {
        let uic = qt.tool(&model.toolchain, &model.profile, "uic")?;
        for form in &forms {
            generated.steps.push(uic_step(&uic, form, &include));
        }
    }

    // rcc: resources into qrc_<name>.cpp, with the files each lists (Qt 6 also writes what it read)
    check_unique_names(&resources, "resource file")?;
    if !resources.is_empty() {
        let rcc = qt.tool(&model.toolchain, &model.profile, "rcc")?;
        for resource in &resources {
            let step = rcc_step(qt, &rcc, resource, &folder);
            generated.sources.extend(step.outputs.iter().cloned());
            generated.steps.push(step);
        }
    }

    Ok(generated)
}

/// The plugins a program built for Windows loads from folders beside it (platforms/qwindows.dll, styles/, imageformats/):
/// the default plugins of the Qt modules whose DLLs it has, in the configuration of those DLLs, each with the folder it
/// goes to. Elsewhere Qt finds its plugins where it is installed.
pub fn windows_plugins(model: &Model, index: usize, runtime_files: &[PathBuf]) -> Vec<(PathBuf, PathBuf)> {
    let target = &model.targets[index];
    if !model.toolchain.targets_windows() || !target.kind.is_executable() {
        return Vec::new();
    }
    let Some(qt) = link_closure(model, index)
        .into_iter()
        .find_map(|dependency| Qt::of_dependency(model, &model.targets[dependency]))
    else {
        return Vec::new();
    };

    // The Qt DLLs it has, and the modules they are (Qt6Guid.dll is Gui's debug DLL)
    let present: HashSet<String> = runtime_files.iter().map(|file| file_name(file).to_lowercase()).collect();
    let packages: HashMap<String, String> = std::fs::read_dir(&qt.cmake_dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .map(|name| (name.to_lowercase(), name))
                .collect()
        })
        .unwrap_or_default();
    let prefix = qt.name.to_lowercase();
    let mut modules = Vec::new();
    for name in &present {
        let Some(base) = name.strip_suffix(".dll").filter(|base| base.starts_with(&prefix)) else {
            continue;
        };
        let package = [base, base.strip_suffix('d').unwrap_or(base)]
            .into_iter()
            .find_map(|base| packages.get(base));
        if let Some(package) = package.filter(|package| !modules.contains(*package)) {
            modules.push(package.clone());
        }
    }
    modules.sort();

    // Each module's default plugins, in the configuration whose Qt DLLs the program has
    let mut plugins = Vec::new();
    for module in modules {
        for plugin in default_plugins(&qt.cmake_dir.join(&module)) {
            let usable = plugin.files.into_iter().find(|file| {
                imported_dlls(file).is_some_and(|imports| {
                    imports
                        .iter()
                        .map(|import| import.to_lowercase())
                        .filter(|import| import.starts_with(&prefix))
                        .all(|import| present.contains(&import))
                })
            });
            if let Some(file) = usable {
                let destination = Path::new(&plugin.kind).join(file_name(&file));
                plugins.push((file, destination));
            }
        }
    }

    plugins
}

/// The `qt.conf` a program built for Windows that uses Qt Quick needs beside it: QML's modules are not copied (only
/// windeployqt knows which a program imports), so it points at the Qt installation's qml/ folder. None when the
/// program does not load Qt's Qml DLL, or its own `data` has a qt.conf.
pub fn windows_qt_conf(model: &Model, index: usize, runtime_files: &[PathBuf]) -> Option<String> {
    let target = &model.targets[index];
    if !model.toolchain.targets_windows() || !target.kind.is_executable() {
        return None;
    }
    if target
        .data
        .iter()
        .any(|(_, relative)| relative.as_os_str().eq_ignore_ascii_case("qt.conf"))
    {
        return None;
    }
    let qt = link_closure(model, index)
        .into_iter()
        .find_map(|dependency| Qt::of_dependency(model, &model.targets[dependency]))?;

    // Qt6Qml.dll (Qt6Qmld.dll) among its DLLs
    let qml_dll = format!("{}qml", qt.name.to_lowercase());
    let uses_qml = runtime_files.iter().any(|file| {
        let name = file_name(file).to_lowercase();
        name == format!("{qml_dll}.dll") || name == format!("{qml_dll}d.dll")
    });
    if !uses_qml {
        return None;
    }

    // The installation's qml/ folder: under its prefix (lib/cmake/../..), or vcpkg's Qt6/qml
    let prefix = qt.cmake_dir.ancestors().nth(2)?;
    let folder = [prefix.join("qml"), prefix.join(&qt.name).join("qml")]
        .into_iter()
        .find(|folder| folder.is_dir())?;
    let folder = slash(&folder);

    Some(format!("[Paths]\nQmlImports = {folder}\nQml2Imports = {folder}\n"))
}

/// A Qt plugin a module's package declares: the folder type it loads from, and its files per configuration.
struct Plugin {
    kind: String,
    files: Vec<PathBuf>,
}

/// The plugins of a module's package folder (`Qt6Gui/Qt6QWindowsIntegrationPluginTargets.cmake`) that Qt loads by
/// default (QT_DEFAULT_PLUGIN "1").
fn default_plugins(folder: &Path) -> Vec<Plugin> {
    static KIND: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"QT_PLUGIN_TYPE\s+"([^"]+)""#).unwrap());
    static DEFAULT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"QT_DEFAULT_PLUGIN\s+"1""#).unwrap());
    static LOCATION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"IMPORTED_LOCATION(?:_\w+)?\s+"\$\{_IMPORT_PREFIX\}/([^"]+)""#).unwrap());

    let mut plugins = Vec::new();
    for main in files_named(folder, |file| file.ends_with("PluginTargets.cmake")) {
        let Ok(text) = std::fs::read_to_string(&main) else { continue };
        let Some(kind) = KIND.captures(&text).map(|captures| captures[1].to_string()) else {
            continue;
        };
        let Some(prefix) = vcpkg_tree::file_import_prefix(&text, &main) else {
            continue;
        };
        if !DEFAULT.is_match(&text) {
            continue;
        }

        // Its configurations' files
        let stem = file_name(&main).trim_end_matches(".cmake").to_string();
        let mut files = Vec::new();
        for configuration in files_named(folder, |file| file.starts_with(&format!("{stem}-"))) {
            let text = std::fs::read_to_string(&configuration).unwrap_or_default();
            files.extend(LOCATION.captures_iter(&text).map(|captures| prefix.join(&captures[1])));
        }
        files.retain(|file| file.is_file());
        plugins.push(Plugin { kind, files });
    }

    plugins
}

/// moc over one header or source, with the target's include folders and definitions.
fn moc_step(qt: &Qt, moc: &Path, settings: &CompileSettings, predefs: Option<&Path>, input: &Path, output: &Path, folder: &Path) -> Step {
    let mut args: Vec<String> = settings.defines.iter().map(|define| format!("-D{define}")).collect();
    for include in settings.includes.iter().chain(settings.system_includes) {
        args.push(format!("-I{}", path_text(include)));
    }
    if let Some(predefs) = predefs {
        args.extend(["--include".to_string(), path_text(predefs)]);
    }

    // What it read, from Qt 5.15 on (named for the output's place, as two outputs may share a name)
    let depfile = qt.moc_writes_depfile().then(|| {
        let relative = util::strip_prefix_ci(output, folder).map_or_else(|| file_name(output), |relative| slash(&relative));
        folder.join("deps").join(format!("{relative}.d"))
    });
    if let Some(depfile) = &depfile {
        args.extend(["--output-dep-file".to_string(), "--dep-file-path".to_string(), path_text(depfile)]);
    }
    args.extend(["-o".to_string(), path_text(output), path_text(input)]);

    let mut inputs = vec![input.to_path_buf()];
    inputs.extend(predefs.map(Path::to_path_buf));
    Step {
        description: format!("Running moc {}", file_name(input)),
        program: moc.to_path_buf(),
        args,
        inputs,
        outputs: vec![output.to_path_buf()],
        depfile,
        after_first: predefs.is_some(),
    }
}

/// uic over one form, into `ui_<name>.h` in the generated include folder.
fn uic_step(uic: &Path, form: &Path, include: &Path) -> Step {
    let output = include.join(format!("ui_{}.h", stem(form)));

    Step {
        description: format!("Running uic {}", file_name(form)),
        program: uic.to_path_buf(),
        args: vec!["-o".to_string(), path_text(&output), path_text(form)],
        inputs: vec![form.to_path_buf()],
        outputs: vec![output],
        depfile: None,
        after_first: false,
    }
}

/// rcc over one resource file, into `qrc_<name>.cpp`, with the files it lists as inputs (Qt 6 also writes what it
/// read). Without zstd in Qt's Core, rcc must not compress with it (what Qt's CMake passes too).
fn rcc_step(qt: &Qt, rcc: &Path, resource: &Path, folder: &Path) -> Step {
    let name = stem(resource);
    let output = folder.join(format!("qrc_{name}.cpp"));
    let qt6 = qt.version.first() >= Some(&6);

    let mut args = vec!["--name".to_string(), resource_name(&name), "-o".to_string(), path_text(&output)];
    let depfile = qt6.then(|| folder.join("deps").join(format!("qrc_{name}.cpp.d")));
    if let Some(depfile) = &depfile {
        args.extend(["--depfile".to_string(), path_text(depfile)]);
    }
    if qt6 && zstd_disabled(&qt.cmake_dir, &qt.name) {
        args.push("--no-zstd".to_string());
    }
    args.push(path_text(resource));

    let mut inputs = vec![resource.to_path_buf()];
    inputs.extend(resource_files(resource));
    Step {
        description: format!("Running rcc {}", file_name(resource)),
        program: rcc.to_path_buf(),
        args,
        inputs,
        outputs: vec![output],
        depfile,
        after_first: false,
    }
}

/// Each header's moc file: `moc_<name>.cpp`, in a folder of its own when two headers share a name.
fn moc_output_names(headers: &[PathBuf]) -> Vec<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for header in headers {
        *counts.entry(stem(header)).or_default() += 1;
    }

    headers
        .iter()
        .map(|header| {
            let name = format!("moc_{}.cpp", stem(header));
            match counts[&stem(header)] {
                1 => name,
                _ => format!("{:08x}/{name}", util::hash_bytes(slash(header).as_bytes()) as u32),
            }
        })
        .collect()
}

/// Forms and resources write `ui_<name>.h` and `qrc_<name>.cpp`: two of one name would write one file.
fn check_unique_names(files: &[PathBuf], what: &str) -> Result<()> {
    let mut seen: HashMap<String, &PathBuf> = HashMap::new();
    for file in files {
        if let Some(other) = seen.insert(stem(file).to_lowercase(), file) {
            bail!(
                "the {what}s {} and {} have one name, and Qt's tools would write one file for both; rename one",
                slash(other),
                slash(file)
            );
        }
    }

    Ok(())
}

/// Where moc writes a source's `.moc`: in the generated include folder, as the source includes it ("x.moc",
/// "sub/x.moc"). Two sources whose includes name one file are an error, as in CMake: one include must say more.
fn source_moc_output<'a>(source: &'a Path, name: &str, include: &Path, writers: &mut HashMap<String, &'a Path>) -> Result<PathBuf> {
    let relative = Path::new(name);
    let file = file_name(relative);
    if !relative.components().all(|component| matches!(component, Component::Normal(_))) {
        bail!(
            "{}: includes \"{name}\"; moc writes it on the include path, so include it as \"{file}\" or \"<folder>/{file}\"",
            slash(source)
        );
    }

    let output = include.join(relative);
    if let Some(other) = writers.insert(util::path_key(&output), source) {
        let folder = |path: &Path| path.parent().map(file_name).unwrap_or_default();
        bail!(
            "{} and {} both include \"{name}\", and moc would write that one file from both; tell them apart with a folder \
             (`#include \"{}/{file}\"` in one, `#include \"{}/{file}\"` in the other), or rename one",
            slash(other),
            slash(source),
            folder(other),
            folder(source)
        );
    }

    Ok(output)
}

/// The headers AUTOMOC would read for these sources: `<name>.h` and `<name>_p.h` beside each (any header extension),
/// and the headers of the target's own include folders (in its package, not generated) with a source's name. With
/// the layout convention, where no headers are listed, also the other headers of those folders and of the folder its
/// sources come from (src/, else the target's folder itself): (headers named for a source, others).
fn candidate_headers(model: &Model, target: &Target, sources: &[PathBuf]) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let names: HashSet<String> = sources.iter().map(|source| stem(source).to_lowercase()).collect();
    let mut headers = Vec::new();
    let mut others = Vec::new();

    // Beside the sources
    for source in sources {
        for extension in HEADER_EXTENSIONS {
            for suffix in ["", "_p"] {
                let header = source.with_file_name(format!("{}{suffix}.{extension}", stem(source)));
                if header.is_file() {
                    headers.push(header);
                }
            }
        }
    }

    // Its own folders: include folders, and the convention's source folder
    let root = &model.packages[target.package].root;
    let extensions = HEADER_EXTENSIONS.join(",");
    let below = format!("**/*.{{{extensions}}}");
    let mut folders: Vec<(&Path, String)> = Vec::new();
    for folder in target.include.iter().chain(&target.private_include) {
        let own = util::strip_prefix_ci(folder, root).is_some() && util::strip_prefix_ci(folder, &model.out_root).is_none();
        if own && folder.is_dir() {
            folders.push((folder, below.clone()));
        }
    }
    let source_folder = target.dir.join("src");
    if target.conventional_sources {
        match source_folder.is_dir() {
            true => folders.push((&source_folder, below.clone())),
            false => folders.push((&target.dir, format!("*.{{{extensions}}}"))),
        }
    }

    // Their headers, each folder once
    let mut walked = HashSet::new();
    for (folder, pattern) in folders {
        if !walked.insert((util::path_key(folder), pattern.clone())) {
            continue;
        }
        for header in targets::expand_pattern_list(folder, &[pattern], &model.out_root, false, false)? {
            let name = stem(&header).to_lowercase();
            let base = name.strip_suffix("_p").unwrap_or(&name);
            if names.contains(base) {
                headers.push(header);
            } else if target.conventional_sources {
                others.push(header);
            }
        }
    }

    Ok((headers, others))
}

/// Whether a folder holds C or C++ files the target does not compile: the convention's files a `when` block or `!`
/// pattern took out (src/win/ elsewhere than on Windows), whose headers are not the target's either.
fn has_dropped_sources(folder: &Path, compiled: &HashSet<String>) -> bool {
    std::fs::read_dir(folder).into_iter().flatten().flatten().any(|entry| {
        let path = entry.path();
        let source = matches!(extension(&path).as_str(), "c" | "cc" | "cpp" | "cxx" | "c++");

        source && !compiled.contains(&util::path_key(&path))
    })
}

/// With the layout convention, the `.ui` and `.qrc` files where the convention finds sources.
fn conventional_files(model: &Model, target: &Target) -> Result<Vec<PathBuf>> {
    if !target.conventional_sources {
        return Ok(Vec::new());
    }

    let pattern = match target.dir.join("src").is_dir() {
        true => "src/**/*.{ui,qrc}",
        false => "*.{ui,qrc}",
    };
    let found = targets::expand_pattern_list(&target.dir, &[pattern.to_string()], &model.out_root, false, false)?;
    Ok(found
        .into_iter()
        .filter(|file| !target.sources.iter().any(|source| util::path_key(source) == util::path_key(file)))
        .collect())
}

/// The files a `.qrc` lists (`<file>data/a.png</file>`, relative to it; a folder stands for the files in it).
fn resource_files(resource: &Path) -> Vec<PathBuf> {
    static FILE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<file\b[^>]*>\s*([^<]*?)\s*</file>").unwrap());
    let Ok(text) = std::fs::read_to_string(resource) else {
        return Vec::new();
    };
    let folder = resource.parent().unwrap_or(Path::new(""));

    let mut files = Vec::new();
    for captures in FILE.captures_iter(&text) {
        let written = captures[1]
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&");
        let path = util::absolute(folder, Path::new(&written));
        if path.is_dir() {
            files.extend(files_below(&path));
        } else {
            files.push(path);
        }
    }

    files
}

/// Every file in a folder and its subfolders, sorted.
fn files_below(folder: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![folder.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();

    files
}

/// rcc's `--name`, which `Q_INIT_RESOURCE(<name>)` calls: the resource file's name as an identifier.
fn resource_name(stem: &str) -> String {
    stem.chars()
        .map(|character| if character.is_ascii_alphanumeric() { character } else { '_' })
        .collect()
}

/// Whether Qt 6's Core was built without zstd, so rcc must not compress with it (what Qt's CMake passes too).
fn zstd_disabled(cmake_dir: &Path, name: &str) -> bool {
    static DISABLED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"QT_DISABLED_PRIVATE_FEATURES\s+"([^"]*)""#).unwrap());
    let file = cmake_dir.join(format!("{name}Core")).join(format!("{name}CoreTargets.cmake"));
    let text = std::fs::read_to_string(file).unwrap_or_default();

    DISABLED
        .captures(&text)
        .is_some_and(|captures| captures[1].split(';').any(|feature| feature == "zstd"))
}

/// Qt's version, from its Core package's version file.
fn read_version(cmake_dir: &Path, name: &str) -> Option<Vec<u64>> {
    static VERSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"set\(\s*PACKAGE_VERSION\s+"?([0-9.]+)"?\s*\)"#).unwrap());
    let folder = cmake_dir.join(format!("{name}Core"));

    for file in [format!("{name}CoreConfigVersionImpl.cmake"), format!("{name}CoreConfigVersion.cmake")] {
        let Ok(text) = std::fs::read_to_string(folder.join(file)) else {
            continue;
        };
        if let Some(captures) = VERSION.captures(&text) {
            return crate::cmake_package::parse_version(&captures[1]);
        }
    }

    None
}

/// 6 for `Qt6`, `Qt6Widgets`; None for a package that is not Qt's.
fn qt_major(package: &str) -> Option<u64> {
    let rest = package.get(..2).filter(|start| start.eq_ignore_ascii_case("qt")).map(|_| &package[2..])?;
    match rest.chars().next()? {
        '5' => Some(5),
        '6' => Some(6),
        _ => None,
    }
}

/// The targets whose include folders and definitions a target's compiles see: its dependencies, those it uses only
/// for their headers, and theirs passed on publicly.
fn compile_closure(model: &Model, index: usize) -> Vec<usize> {
    let target = &model.targets[index];
    let mut pending: Vec<usize> = target.deps.iter().map(|(dependency, _)| *dependency).collect();
    pending.extend(target.header_deps.iter().copied());
    let mut visited = HashSet::new();
    let mut order = Vec::new();

    while let Some(next) = pending.pop() {
        if !visited.insert(next) {
            continue;
        }
        order.push(next);
        pending.extend(
            model.targets[next]
                .deps
                .iter()
                .filter(|(_, public)| *public)
                .map(|(dependency, _)| *dependency),
        );
    }

    order
}

/// Everything a target links, through every dependency.
fn link_closure(model: &Model, index: usize) -> Vec<usize> {
    let mut pending: Vec<usize> = model.targets[index].deps.iter().map(|(dependency, _)| *dependency).collect();
    let mut visited = HashSet::new();
    let mut order = Vec::new();

    while let Some(next) = pending.pop() {
        if visited.insert(next) {
            order.push(next);
            pending.extend(model.targets[next].deps.iter().map(|(dependency, _)| *dependency));
        }
    }

    order
}

/// The files in a folder whose names pass `wanted`, sorted.
fn files_named(folder: &Path, wanted: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| wanted(&entry.file_name().to_string_lossy()))
        .map(|entry| entry.path())
        .collect();
    files.sort();

    files
}

/// The DLLs a Windows program or DLL imports, read from its import table without reading the whole file.
fn imported_dlls(file: &Path) -> Option<Vec<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut handle = std::fs::File::open(file).ok()?;
    let mut read_at = |offset: usize, length: usize| -> Option<Vec<u8>> {
        let mut bytes = Vec::with_capacity(length);
        handle.seek(SeekFrom::Start(offset as u64)).ok()?;
        (&mut handle).take(length as u64).read_to_end(&mut bytes).ok()?;
        Some(bytes)
    };
    let number = |bytes: &[u8], offset: usize, size: usize| -> Option<usize> {
        let field = bytes.get(offset..offset + size)?;
        Some(field.iter().rev().fold(0, |value, byte| (value << 8) | *byte as usize))
    };

    // The headers: the PE signature, the optional header (PE32 or PE32+) and its import directory, the sections
    let headers = read_at(0, 8192)?;
    let header = number(&headers, 0x3c, 4)?;
    if headers.get(header..header + 4)? != b"PE\0\0" {
        return None;
    }
    let section_count = number(&headers, header + 6, 2)?;
    let optional = header + 24;
    let directories = match number(&headers, optional, 2)? {
        0x10b => optional + 96,
        0x20b => optional + 112,
        _ => return None,
    };
    let imports = number(&headers, directories + 8, 4)?;
    let sections = optional + number(&headers, header + 20, 2)?;

    // An address once loaded to its place in the file
    let file_offset = |address: usize| -> Option<usize> {
        (0..section_count).find_map(|index| {
            let section = sections + index * 40;
            let start = number(&headers, section + 12, 4)?;
            let size = number(&headers, section + 8, 4)?.max(number(&headers, section + 16, 4)?);
            if !(start..start + size).contains(&address) {
                return None;
            }
            Some(address - start + number(&headers, section + 20, 4)?)
        })
    };

    // One 20-byte descriptor per DLL, up to an empty one
    let mut names = Vec::new();
    let mut descriptor = file_offset(imports)?;
    loop {
        let bytes = read_at(descriptor, 20)?;
        let Some(name) = number(&bytes, 12, 4).filter(|name| *name != 0) else {
            break;
        };
        let text = read_at(file_offset(name)?, 260)?;
        let length = text.iter().position(|byte| *byte == 0)?;
        names.push(String::from_utf8_lossy(&text[..length]).into_owned());
        descriptor += 20;
    }

    Some(names)
}

impl ScanCache {
    fn load(file: PathBuf) -> ScanCache {
        // Another version's scans are read again
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let text = text.strip_prefix(&format!("{SCAN_CACHE_HEADER}\n")).unwrap_or_default();
        let mut entries = HashMap::new();
        for line in text.lines() {
            let fields: Vec<&str> = line.splitn(4, '\t').collect();
            let [time, has_macro, includes, path] = fields[..] else { continue };
            let Ok(time) = time.parse() else { continue };
            let scan = Scan {
                has_macro: has_macro == "1",
                includes: includes.split('|').filter(|name| !name.is_empty()).map(String::from).collect(),
            };
            entries.insert(path.to_string(), (time, scan));
        }

        ScanCache {
            file,
            entries,
            folder_times: HashMap::new(),
            lines: BTreeMap::new(),
        }
    }

    /// A file's scan: the cached one while its time stays, else read now.
    fn scan(&mut self, path: &Path) -> Scan {
        let key = slash(path);
        let time = self.modified(path);

        let scan = match self.entries.get(&key) {
            Some((cached_time, scan)) if *cached_time == time => scan.clone(),
            _ => scan_text(&String::from_utf8_lossy(&std::fs::read(path).unwrap_or_default())),
        };
        let line = format!("{time}\t{}\t{}\t{key}", scan.has_macro as u8, scan.includes.join("|"));
        self.lines.insert(key, line);

        scan
    }

    /// A file's modification time: from its folder's listing where there is one (a stat per file costs on Windows).
    fn modified(&mut self, path: &Path) -> i64 {
        let folder = path.parent().unwrap_or(Path::new(""));
        let times = self
            .folder_times
            .entry(folder.to_path_buf())
            .or_insert_with(|| util::folder_mtimes(folder));
        let listed = times.as_ref().and_then(|times| times.get(&util::path_key(Path::new(&file_name(path)))));

        listed.copied().or_else(|| util::mtime(path)).unwrap_or(0)
    }

    /// Keeps the scans of this build's files, written only when they changed.
    fn save(&self) -> Result<()> {
        let mut text = format!("{SCAN_CACHE_HEADER}\n");
        for line in self.lines.values() {
            text.push_str(line);
            text.push('\n');
        }

        util::write_if_changed(&self.file, text.as_bytes()).with_context(|| format!("writing {}", slash(&self.file)))?;
        Ok(())
    }
}

/// Whether a file's text has a moc macro at the start of a line, and the generated Qt files it includes.
fn scan_text(text: &str) -> Scan {
    static MACRO: LazyLock<Regex> = LazyLock::new(|| Regex::new(MOC_MACROS).unwrap());
    static INCLUDE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?m)^[ \t]*#[ \t]*include[ \t]*["<]([^">]+)[">]"#).unwrap());

    let mut includes = Vec::new();
    for captures in INCLUDE.captures_iter(text) {
        let written = captures[1].replace('\\', "/");
        let name = written.rsplit('/').next().unwrap_or_default().to_string();
        let generated =
            name.ends_with(".moc") || (name.starts_with("moc_") && name.ends_with(".cpp")) || (name.starts_with("ui_") && name.ends_with(".h"));

        // A `.moc` keeps its folder ("sub/x.moc"), where moc writes it
        let name = match name.ends_with(".moc") {
            true => written.trim_start_matches("./").to_string(),
            false => name,
        };
        if generated && !includes.contains(&name) {
            includes.push(name);
        }
    }

    Scan {
        has_macro: MACRO.is_match(text),
        includes,
    }
}

fn stem(path: &Path) -> String {
    path.file_stem().unwrap_or_default().to_string_lossy().into_owned()
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().into_owned()
}

fn extension(path: &Path) -> String {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{Cpu, Platform};

    /// A fresh folder for one test.
    fn scratch(test: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("anvil-qt-{test}-{}", std::process::id()));
        std::fs::remove_dir_all(&folder).ok();
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn toolchain(os: Os) -> Toolchain {
        let platform = Platform {
            cpu: Cpu::X64,
            os,
            triple: None,
        };
        match os {
            Os::Windows => Toolchain::for_tests(Family::Msvc, platform, true),
            _ => Toolchain::for_tests(Family::Gcc, platform, false),
        }
    }

    fn profile(name: &str) -> Profile {
        Profile::resolve(name, &Default::default()).unwrap()
    }

    /// A Qt 6 installation as its packages describe it: tools declared in Qt6CoreTools and Qt6WidgetsTools under
    /// libexec/, Core built without zstd.
    fn fake_qt6(prefix: &Path) -> Qt {
        let cmake_dir = prefix.join("lib/cmake");
        write(
            &cmake_dir.join("Qt6Core/Qt6CoreConfigVersionImpl.cmake"),
            "set(PACKAGE_VERSION \"6.8.0\")\n",
        );
        write(
            &cmake_dir.join("Qt6Core/Qt6CoreTargets.cmake"),
            "set_target_properties(Qt6::Core PROPERTIES\n  QT_DISABLED_PRIVATE_FEATURES \"backtrace;zstd\"\n)\n",
        );
        for (package, tools) in [("Qt6CoreTools", &["moc", "rcc"][..]), ("Qt6WidgetsTools", &["uic"][..])] {
            let folder = cmake_dir.join(package);
            let mut main = String::from(
                "get_filename_component(_IMPORT_PREFIX \"${CMAKE_CURRENT_LIST_FILE}\" PATH)\n\
                 get_filename_component(_IMPORT_PREFIX \"${_IMPORT_PREFIX}\" PATH)\n\
                 get_filename_component(_IMPORT_PREFIX \"${_IMPORT_PREFIX}\" PATH)\n\
                 get_filename_component(_IMPORT_PREFIX \"${_IMPORT_PREFIX}\" PATH)\n",
            );
            let mut release = String::new();
            for tool in tools {
                main.push_str(&format!("add_executable(Qt6::{tool} IMPORTED)\n"));
                release.push_str(&format!(
                    "set_property(TARGET Qt6::{tool} APPEND PROPERTY IMPORTED_CONFIGURATIONS RELEASE)\n\
                     set_target_properties(Qt6::{tool} PROPERTIES\n  IMPORTED_LOCATION_RELEASE \"${{_IMPORT_PREFIX}}/libexec/{tool}\"\n  )\n"
                ));
                write(&prefix.join("libexec").join(tool), "");
            }
            write(&folder.join(format!("{package}Targets.cmake")), &main);
            write(&folder.join(format!("{package}Targets-release.cmake")), &release);
        }

        Qt::new("Qt6", cmake_dir)
    }

    #[test]
    fn scans_moc_macros_and_generated_includes() {
        let text = "#include \"ui_form.h\"\n#  include <moc_window.cpp>\n#include \"sub/main.moc\"\n#include \"plain.h\"\n\
                    class A : public QObject {\n    Q_OBJECT\n};\n";
        let scan = scan_text(text);
        assert!(scan.has_macro);
        assert_eq!(scan.includes, ["ui_form.h", "moc_window.cpp", "sub/main.moc"]);

        // Only a macro that starts a line counts: not one in a comment or a longer name
        for text in ["// Q_OBJECT\n", "#define X Q_OBJECT\n", "  Q_OBJECTS\n"] {
            assert!(!scan_text(text).has_macro, "{text}");
        }
        for text in [
            "Q_GADGET\n",
            "\tQ_NAMESPACE_EXPORT(API)\n",
            "  Q_GADGET_EXPORT(API)\n",
            "Q_NAMESPACE\n",
            "{ Q_OBJECT\n",
            "  {Q_OBJECT\n",
        ] {
            assert!(scan_text(text).has_macro, "{text}");
        }
    }

    #[test]
    fn writes_each_source_moc_where_its_include_finds_it() {
        let include = PathBuf::from("/out/qt/app/include");
        let (first, second) = (PathBuf::from("/p/src/a/item.cpp"), PathBuf::from("/p/src/b/item.cpp"));
        let mut writers = HashMap::new();

        // "item.moc" in include/, "b/item.moc" in include/b/
        let output = source_moc_output(&first, "item.moc", &include, &mut writers).unwrap();
        assert_eq!(output, include.join("item.moc"));
        let output = source_moc_output(&second, "b/item.moc", &include, &mut writers).unwrap();
        assert_eq!(output, include.join("b/item.moc"));

        // Two sources whose includes name one file, and an include that leaves the include folder
        let error = source_moc_output(&second, "item.moc", &include, &mut writers).unwrap_err().to_string();
        assert!(
            error.contains("/p/src/a/item.cpp and /p/src/b/item.cpp both include \"item.moc\"")
                && error.contains("`#include \"a/item.moc\"` in one, `#include \"b/item.moc\"` in the other"),
            "{error}"
        );
        let error = source_moc_output(&second, "../item.moc", &include, &mut writers).unwrap_err().to_string();
        assert!(error.contains("include it as \"item.moc\" or \"<folder>/item.moc\""), "{error}");
    }

    #[test]
    fn scan_cache_keeps_scans_until_a_file_changes() {
        let folder = scratch("scancache");
        let header = folder.join("window.h");
        write(&header, "class Window {\n    Q_OBJECT\n};\n");
        let file = folder.join("scan.txt");

        // Scanned, kept, and read back
        let mut cache = ScanCache::load(file.clone());
        assert!(cache.scan(&header).has_macro);
        cache.save().unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with(&format!("{SCAN_CACHE_HEADER}\n")), "{text}");
        assert_eq!(ScanCache::load(file.clone()).entries.len(), 1);

        // Another version's cache is read again
        write(&file, text.trim_start_matches(&format!("{SCAN_CACHE_HEADER}\n")));
        assert!(ScanCache::load(file).entries.is_empty());

        std::fs::remove_dir_all(&folder).ok();
    }

    #[test]
    fn folders_with_dropped_sources() {
        let folder = scratch("dropped");
        write(&folder.join("win/native.cpp"), "");
        write(&folder.join("win/native.h"), "");
        write(&folder.join("extra/orphan.h"), "");
        write(&folder.join("main.cpp"), "");
        let compiled: HashSet<String> = [util::path_key(&folder.join("main.cpp"))].into_iter().collect();

        assert!(has_dropped_sources(&folder.join("win"), &compiled));
        assert!(!has_dropped_sources(&folder.join("extra"), &compiled));
        assert!(!has_dropped_sources(&folder, &compiled));

        std::fs::remove_dir_all(&folder).ok();
    }

    #[test]
    fn names_generated_files() {
        // Two headers of one name: each in a folder of its own
        let headers = [PathBuf::from("/p/a/x.h"), PathBuf::from("/p/b/x.h"), PathBuf::from("/p/y.hpp")];
        let names = moc_output_names(&headers);
        assert_eq!(names[2], "moc_y.cpp");
        assert!(names[0].ends_with("/moc_x.cpp") && names[1].ends_with("/moc_x.cpp"), "{names:?}");
        assert_ne!(names[0], names[1]);

        // Forms and resources of one name write one file
        let forms = [PathBuf::from("/p/a/Form.ui"), PathBuf::from("/p/b/form.ui")];
        let error = check_unique_names(&forms, "form").unwrap_err().to_string();
        assert!(error.contains("the forms /p/a/Form.ui and /p/b/form.ui have one name"), "{error}");
        assert!(check_unique_names(&forms[..1], "form").is_ok());

        assert_eq!(resource_name("my-icons.v2"), "my_icons_v2");
        assert_eq!(qt_major("Qt6"), Some(6));
        assert_eq!(qt_major("qt5widgets"), Some(5));
        assert_eq!(qt_major("Qt"), None);
        assert_eq!(qt_major("Qwt6"), None);
    }

    #[test]
    fn resources_list_their_files() {
        let folder = scratch("resources");
        write(&folder.join("data/a.txt"), "a");
        write(&folder.join("icons/x.png"), "x");
        write(&folder.join("icons/more/y.png"), "y");
        let resource = folder.join("app.qrc");
        write(
            &resource,
            "<RCC>\n  <qresource prefix=\"/\">\n    <file alias=\"a\">data/a.txt</file>\n    <file> icons </file>\n    \
             <file>b&amp;c.txt</file>\n  </qresource>\n</RCC>\n",
        );

        let files: Vec<String> = resource_files(&resource)
            .iter()
            .map(|file| slash(&util::strip_prefix_ci(file, &folder).unwrap()))
            .collect();
        assert_eq!(files, ["data/a.txt", "icons/more/y.png", "icons/x.png", "b&c.txt"]);

        std::fs::remove_dir_all(&folder).ok();
    }

    #[test]
    fn plans_tool_runs_of_a_qt6_installation() {
        let folder = scratch("qt6");
        let qt = fake_qt6(&folder);
        assert_eq!(qt.version, [6, 8, 0]);
        assert!(qt.moc_writes_depfile());

        // The tools its packages declare
        let (toolchain, profile) = (toolchain(Os::Linux), profile("release"));
        for tool in ["moc", "uic", "rcc"] {
            assert_eq!(qt.tool(&toolchain, &profile, tool).unwrap(), folder.join("libexec").join(tool), "{tool}");
        }

        // moc: definitions, include folders, the compiler's macros, its dependency file, then output and input
        let out = folder.join("out/qt/app");
        let includes = [folder.join("src")];
        let system_includes = [folder.join("include/QtCore")];
        let defines = ["QT_CORE_LIB".to_string(), "NAME=\"x\"".to_string()];
        let settings = CompileSettings {
            includes: &includes,
            system_includes: &system_includes,
            defines: &defines,
        };
        let moc = folder.join("libexec/moc");
        let header = folder.join("src/window.h");
        let predefs = out.join("moc_predefs.h");
        let step = moc_step(&qt, &moc, &settings, Some(&predefs), &header, &out.join("moc_window.cpp"), &out);
        let text = |path: &Path| path_text(path);
        assert_eq!(
            step.args,
            [
                "-DQT_CORE_LIB".to_string(),
                "-DNAME=\"x\"".to_string(),
                format!("-I{}", text(&folder.join("src"))),
                format!("-I{}", text(&folder.join("include/QtCore"))),
                "--include".to_string(),
                text(&predefs),
                "--output-dep-file".to_string(),
                "--dep-file-path".to_string(),
                text(&out.join("deps").join("moc_window.cpp.d")),
                "-o".to_string(),
                text(&out.join("moc_window.cpp")),
                text(&header),
            ]
        );
        assert_eq!(step.inputs, [header.clone(), predefs.clone()]);
        assert_eq!(step.depfile, Some(out.join("deps").join("moc_window.cpp.d")));
        assert!(step.after_first);

        // uic: into the generated include folder
        let form = folder.join("src/window.ui");
        let step = uic_step(&folder.join("libexec/uic"), &form, &out.join("include"));
        assert_eq!(step.args, ["-o".to_string(), text(&out.join("include").join("ui_window.h")), text(&form)]);
        assert_eq!(step.outputs, [out.join("include").join("ui_window.h")]);

        // rcc: named for Q_INIT_RESOURCE, its dependency file, no zstd as Core was built without it; the listed
        // files are inputs
        let resource = folder.join("src/my-icons.qrc");
        write(&resource, "<RCC><qresource><file>a.png</file></qresource></RCC>\n");
        let step = rcc_step(&qt, &folder.join("libexec/rcc"), &resource, &out);
        assert_eq!(
            step.args,
            [
                "--name".to_string(),
                "my_icons".to_string(),
                "-o".to_string(),
                text(&out.join("qrc_my-icons.cpp")),
                "--depfile".to_string(),
                text(&out.join("deps").join("qrc_my-icons.cpp.d")),
                "--no-zstd".to_string(),
                text(&resource),
            ]
        );
        assert_eq!(step.inputs, [resource.clone(), folder.join("src/a.png")]);

        std::fs::remove_dir_all(&folder).ok();
    }

    #[test]
    fn finds_tools_where_installs_keep_them() {
        // No tools package: Debian's lib/qt6/libexec before bin/, which may hold another Qt's moc
        let folder = scratch("toolfolders");
        let cmake_dir = folder.join("lib/x86_64-linux-gnu/cmake");
        write(&cmake_dir.join("Qt6Core/Qt6CoreConfigVersion.cmake"), "set(PACKAGE_VERSION \"6.4.2\")\n");
        let name = if cfg!(windows) { "moc.exe" } else { "moc" };
        write(&folder.join("lib/qt6/libexec").join(name), "");
        write(&folder.join("bin").join(name), "");
        let qt = Qt::new("Qt6", cmake_dir);

        let (toolchain, profile) = (toolchain(Os::Linux), profile("release"));
        assert_eq!(qt.tool(&toolchain, &profile, "moc").unwrap(), folder.join("lib/qt6/libexec").join(name));
        let error = qt.tool(&toolchain, &profile, "uic").unwrap_err().to_string();
        assert!(
            error.contains("Qt's uic was not found") && error.contains("qt6-base-dev-tools"),
            "{error}"
        );

        std::fs::remove_dir_all(&folder).ok();
    }

    /// A Qt 5 module's package as Qt5BasicConfig.cmake.in writes it, for Linux or Windows.
    fn fake_qt5_module(cmake_dir: &Path, name: &str, links: &str, windows: bool) {
        let folder = cmake_dir.join(format!("Qt5{name}"));
        let lower = name.to_lowercase();
        let populate = match windows {
            true => format!(
                "    set(imported_location \"${{_qt5{name}_install_prefix}}/bin/${{LIB_LOCATION}}\")\n\
                     set(imported_implib \"${{_qt5{name}_install_prefix}}/lib/${{IMPLIB_LOCATION}}\")\n\
                 _populate_{name}_target_properties(RELEASE \"Qt5{name}.dll\" \"Qt5{name}.lib\" FALSE)\n\
                 _populate_{name}_target_properties(DEBUG \"Qt5{name}d.dll\" \"Qt5{name}d.lib\" FALSE)\n"
            ),
            false => format!(
                "    set(imported_location \"${{_qt5{name}_install_prefix}}/lib/${{LIB_LOCATION}}\")\n\
                 _populate_{name}_target_properties(RELEASE \"libQt5{name}.so.5.15.2\" \"\" FALSE)\n"
            ),
        };
        write(
            &folder.join(format!("Qt5{name}Config.cmake")),
            &format!(
                "get_filename_component(_qt5{name}_install_prefix \"${{CMAKE_CURRENT_LIST_DIR}}/../../../\" ABSOLUTE)\n\
                 set(_Qt5{name}_OWN_INCLUDE_DIRS \"${{_qt5{name}_install_prefix}}/include/\" \"${{_qt5{name}_install_prefix}}/include/Qt{name}\")\n\
                 set(Qt5{name}_COMPILE_DEFINITIONS QT_{}_LIB)\n\
                 set(_Qt5{name}_LIB_DEPENDENCIES \"{links}\")\n{populate}",
                lower.to_uppercase()
            ),
        );
        if name == "Core" {
            let moc = if windows { "bin/moc.exe" } else { "bin/moc" };
            write(
                &folder.join("Qt5CoreConfigExtras.cmake"),
                &format!(
                    "if (NOT TARGET Qt5::moc)\n    add_executable(Qt5::moc IMPORTED)\n\n    set(imported_location \"${{_qt5Core_install_prefix}}/{moc}\")\n\
                     endif()\nset_property(TARGET Qt5::Core APPEND PROPERTY INTERFACE_COMPILE_OPTIONS -fPIC)\n"
                ),
            );
            write(
                &folder.join("Qt5CoreConfigExtrasMkspecDir.cmake"),
                "set(_qt5_corelib_extra_includes \"${_qt5Core_install_prefix}/mkspecs/linux-g++\")\n",
            );
            write(&folder.join("Qt5CoreConfigVersion.cmake"), "set(PACKAGE_VERSION 5.15.2)\n");
        }
    }

    #[test]
    fn reads_qt5_packages() {
        let folder = scratch("qt5");
        let cmake_dir = folder.join("lib/cmake");
        fake_qt5_module(&cmake_dir, "Core", "", false);
        fake_qt5_module(&cmake_dir, "Gui", "Qt5::Core", false);
        fake_qt5_module(&cmake_dir, "Widgets", "Qt5::Gui;Qt5::Core", false);
        let components = ["Widgets".to_string()];
        let request = Request {
            dependency: "qt",
            package: "Qt5",
            components: &components,
            version: None,
            targets: None,
            prefixes: &[],
        };

        // Each module after those that link it, Core's mkspecs folder and -fPIC, QT_NO_DEBUG for release libraries
        let found = read_qt5(&request, &cmake_dir.join("Qt5"), &toolchain(Os::Linux), &profile("release")).unwrap();
        let usage = &found.usage;
        let relative =
            |paths: &[PathBuf]| -> Vec<String> { paths.iter().map(|path| slash(&util::strip_prefix_ci(path, &folder).unwrap())).collect() };
        assert_eq!(
            relative(&usage.includes),
            ["include", "include/QtWidgets", "include/QtGui", "include/QtCore", "mkspecs/linux-g++"]
        );
        assert_eq!(usage.defines, ["QT_WIDGETS_LIB", "QT_GUI_LIB", "QT_CORE_LIB", "QT_NO_DEBUG"]);
        assert_eq!(usage.flags, ["-fPIC"]);
        assert_eq!(
            relative(&usage.libs),
            ["lib/libQt5Widgets.so.5.15.2", "lib/libQt5Gui.so.5.15.2", "lib/libQt5Core.so.5.15.2"]
        );
        assert_eq!(usage.link_flags, [format!("-Wl,-rpath,{}", slash(&folder.join("lib")))]);
        assert_eq!(found.package.targets, ["Qt5::Widgets"]);

        // Its moc, as Core's extras declare it
        let qt = Qt::new("Qt5", cmake_dir.clone());
        assert_eq!(qt.version, [5, 15, 2]);
        assert_eq!(qt.qt5_tool("Core", "moc"), Some(folder.join("bin/moc")));

        // A module Qt does not have
        let components = ["Wigdets".to_string()];
        let request = Request {
            components: &components,
            ..request
        };
        let Err(error) = read_qt5(&request, &cmake_dir.join("Qt5"), &toolchain(Os::Linux), &profile("release")) else {
            panic!("a module Qt does not have was read");
        };
        assert!(format!("{error:#}").contains("Qt 5 has no module `Wigdets`"), "{error:#}");

        std::fs::remove_dir_all(&folder).ok();
    }

    #[test]
    fn reads_qt5_packages_for_windows() {
        let folder = scratch("qt5windows");
        let cmake_dir = folder.join("lib/cmake");
        fake_qt5_module(&cmake_dir, "Core", "", true);
        let request = Request {
            dependency: "qt",
            package: "Qt5Core",
            components: &[],
            version: None,
            targets: None,
            prefixes: &[],
        };

        // Debug DLLs with the debug runtime: import libraries linked, DLLs to copy, no QT_NO_DEBUG
        let found = read_qt5(&request, &cmake_dir.join("Qt5Core"), &toolchain(Os::Windows), &profile("debug")).unwrap();
        assert_eq!(found.usage.libs, [folder.join("lib/Qt5Cored.lib")]);
        assert_eq!(found.usage.runtime, [folder.join("bin/Qt5Cored.dll")]);
        assert!(found.usage.link_flags.is_empty());
        assert_eq!(found.usage.defines, ["QT_CORE_LIB"]);

        let found = read_qt5(&request, &cmake_dir.join("Qt5Core"), &toolchain(Os::Windows), &profile("release")).unwrap();
        assert_eq!(found.usage.libs, [folder.join("lib/Qt5Core.lib")]);
        assert!(found.usage.defines.contains(&"QT_NO_DEBUG".to_string()));

        std::fs::remove_dir_all(&folder).ok();
    }

    #[test]
    fn finds_default_plugins() {
        let folder = scratch("plugins");
        let gui = folder.join("lib/cmake/Qt6Gui");
        let main = "get_filename_component(_IMPORT_PREFIX \"${CMAKE_CURRENT_LIST_FILE}\" PATH)\n\
                    get_filename_component(_IMPORT_PREFIX \"${_IMPORT_PREFIX}\" PATH)\n\
                    get_filename_component(_IMPORT_PREFIX \"${_IMPORT_PREFIX}\" PATH)\n\
                    get_filename_component(_IMPORT_PREFIX \"${_IMPORT_PREFIX}\" PATH)\n\
                    add_library(Qt6::QWindowsIntegrationPlugin MODULE IMPORTED)\n\
                    set_target_properties(Qt6::QWindowsIntegrationPlugin PROPERTIES\n  QT_DEFAULT_PLUGIN \"1\"\n  QT_PLUGIN_TYPE \"platforms\"\n)\n";
        write(&gui.join("Qt6QWindowsIntegrationPluginTargets.cmake"), main);
        write(
            &gui.join("Qt6QWindowsIntegrationPluginTargets-debug.cmake"),
            "set_target_properties(Qt6::QWindowsIntegrationPlugin PROPERTIES\n  IMPORTED_LOCATION_DEBUG \"${_IMPORT_PREFIX}/plugins/platforms/qwindowsd.dll\"\n)\n",
        );
        write(&folder.join("plugins/platforms/qwindowsd.dll"), "");
        write(
            &gui.join("Qt6QOffscreenIntegrationPluginTargets.cmake"),
            &main
                .replace("QWindowsIntegrationPlugin", "QOffscreenIntegrationPlugin")
                .replace("QT_DEFAULT_PLUGIN \"1\"", "QT_DEFAULT_PLUGIN \"0\""),
        );

        let plugins = default_plugins(&gui);
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].kind, "platforms");
        assert_eq!(plugins[0].files, [folder.join("plugins/platforms/qwindowsd.dll")]);

        std::fs::remove_dir_all(&folder).ok();
    }
}
