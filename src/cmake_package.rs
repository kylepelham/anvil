//! Installed CMake packages (`[deps] qt = { cmake = "Qt6", components = ["Widgets"] }`): found where
//! find_package(<name> CONFIG) finds them, then read with the reader vcpkg's packages go through (crate::vcpkg_tree):
//! their imported targets, what those give their users, and their files for the build's configuration.

use crate::find_module::{self, CxxAbi, Search};
use crate::platform::{Cpu, Os, Platform};
use crate::profile::Profile;
use crate::toolchain::Toolchain;
use crate::util::{self, slash};
use crate::vcpkg_tree::{self, BuildContext, CmakePackage, ImportPrefix, ImportedFiles, ImportedTarget, Requirements, Usage};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// Bump when what a cache entry holds, or how it is read, changes
const CACHE_VERSION: u32 = 1;

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
/// their users for this toolchain and profile. With a `cache_folder`, the result is kept there and given again while
/// nothing it came from changed: the files read, the folders searched, the environment variables asked for, Anvil.
pub fn find(request: &Request, toolchain: &Toolchain, profile: &Profile, cache_folder: Option<&Path>, reads: &Reads) -> Result<Found> {
    let build = BuildContext::new(toolchain, profile);
    let debug = vcpkg_tree::uses_debug_libraries(toolchain, profile);

    // An earlier run's result, while still valid
    let key = cache_key(request, toolchain, &build, debug);
    let cache_file = cache_folder.map(|folder| folder.join(format!("cmake-{:016x}.json", util::hash_bytes(key.as_bytes()))));
    if let Some(found) = cache_file.as_deref().and_then(|file| load_cached(file, &key)) {
        return Ok(found);
    }

    let session = Session::new(reads);
    session.variable("VULKAN_SDK");
    let (found, cacheable) = find_in(request, toolchain, profile, build, debug, &session)?;

    // A cache that cannot be written only costs the next run the search
    if let (Some(file), true) = (cache_file, cacheable) {
        let entry = session.entry(key, &found);
        let _ = util::write_atomic(&file, serde_json::to_string(&entry)?.as_bytes());
    }

    Ok(found)
}

/// `find` without the cache: the result, and whether a cache entry can stand for it (Qt 5's packages are read by
/// crate::qt, outside the session).
fn find_in(
    request: &Request,
    toolchain: &Toolchain,
    profile: &Profile,
    build: BuildContext,
    debug: bool,
    session: &Session,
) -> Result<(Found, bool)> {
    let dependency = request.dependency;
    let mut reader = Reader::new(build, debug, toolchain.platform.clone(), dependency, request.prefixes, session);

    // The package, compatible with the version asked for
    let wanted_version = match request.version {
        Some(text) => match parse_version(text) {
            Some(version) => Some(version),
            None => bail!("dependency `{dependency}`: version = \"{text}\" is not a version; write numbers such as \"1.2\""),
        },
        None => None,
    };
    let places = match request.prefixes.is_empty() {
        true => default_places(request.package, &reader.platform, session),
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
        let found = crate::qt::read_qt5(request, located.config_file.parent().unwrap(), toolchain, profile)?;
        return Ok((found, false));
    }
    let main = reader.read_package(&located);

    // The targets its users link, then what they need, which a MinGW build cannot take from MSVC's C++ libraries
    // (nor the other way round)
    let wanted = reader.wanted_targets(request, main)?;
    let usage = reader.usage(dependency, &wanted)?;
    if toolchain.platform.os == Os::Windows {
        check_abi(dependency, &usage.libs, toolchain, session)?;
    }

    let package = &reader.packages[main];
    let found = Found {
        package: CmakePackage {
            name: package.name.clone(),
            config: true,
            targets: wanted,
            components: request.components.to_vec(),
            hint: Some(package.folder.clone()),
        },
        usage,
    };

    Ok((found, true))
}

/// MinGW cannot link MSVC's C++ libraries, nor MSVC MinGW's: their C++ names differ. A library of C functions only
/// passes (both link those).
fn check_abi(dependency: &str, libraries: &[PathBuf], toolchain: &Toolchain, session: &Session) -> Result<()> {
    let wanted = if toolchain.is_msvc_abi() { CxxAbi::Msvc } else { CxxAbi::Gnu };
    let archives = libraries.iter().filter(|library| {
        let name = library.to_string_lossy().to_ascii_lowercase();
        library.is_absolute() && (name.ends_with(".lib") || name.ends_with(".a"))
    });

    for library in archives.filter(|library| session.watch(library)) {
        let Some(abi) = find_module::cxx_abi(library).filter(|abi| *abi != wanted) else {
            continue;
        };
        let family = toolchain.family.name();
        match abi {
            CxxAbi::Msvc => bail!(
                "dependency `{dependency}`: {} was built with MSVC (its C++ names are MSVC's), and {family} builds for MinGW, which cannot link \
                 MSVC's C++ libraries; build with `--toolchain msvc` or `--toolchain clang-cl`, or use a MinGW build of the package",
                slash(library)
            ),
            CxxAbi::Gnu => bail!(
                "dependency `{dependency}`: {} was built with GCC or Clang for MinGW (its C++ names are theirs), which {family} cannot link; \
                 build with `--toolchain gcc`, or use an MSVC build of the package",
                slash(library)
            ),
        }
    }

    Ok(())
}

/// What a cached result stands for: the request, the toolchain, the build its generator expressions are evaluated for
/// and the configuration's libraries, and the Anvil that read it (a newer one may read packages differently).
fn cache_key(request: &Request, toolchain: &Toolchain, build: &BuildContext, debug: bool) -> String {
    static EXECUTABLE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        let executable = std::env::current_exe().unwrap_or_default();
        format!("{}:{:?}", slash(&executable), util::stamp(&executable))
    });

    format!(
        "{CACHE_VERSION}|{}|{:016x}|{build:?}|{debug}|{}|{}|{:?}|{:?}|{:?}|{:?}",
        *EXECUTABLE,
        toolchain.identity(),
        request.dependency,
        request.package,
        request.components,
        request.version,
        request.targets,
        request.prefixes
    )
}

/// The cached result in `file` for `key`, when nothing it came from changed since: its warnings said again.
fn load_cached(file: &Path, key: &str) -> Option<Found> {
    let entry: CacheEntry = serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()?;
    let valid = entry.key == key
        && entry
            .variables
            .iter()
            .all(|(name, value)| std::env::var_os(name).map(|value| value.to_string_lossy().into_owned()) == *value)
        && entry.folders.iter().all(|(folder, time)| util::mtime(folder) == *time)
        && entry.files.iter().all(|(file, stamp)| util::stamp(file) == *stamp);
    if !valid {
        return None;
    }

    for warning in &entry.warnings {
        crate::ui::warn(warning);
    }

    Some(Found {
        package: entry.package,
        usage: entry.usage,
    })
}

/// A result kept between runs, with what it came from: the environment variables asked for, the folders listed and
/// the files read, with their time stamps (and sizes).
#[derive(Serialize, Deserialize)]
struct CacheEntry {
    key: String,
    variables: Vec<(String, Option<String>)>,
    folders: Vec<(PathBuf, Option<i64>)>,
    files: Vec<(PathBuf, Stamp)>,
    warnings: Vec<String>,
    package: CmakePackage,
    usage: Usage,
}

/// A file's modification time and size (crate::util::stamp), None when it was not there.
type Stamp = Option<(i64, u64)>;

/// The folders listed and files read for a load's CMake packages, each once however many dependencies need them,
/// with their time stamps as they were before. A Model's, so a load (an LSP session's next one) never sees an
/// earlier load's.
#[derive(Default)]
pub struct Reads {
    listings: Mutex<HashMap<String, Listing>>,
    texts: Mutex<HashMap<String, FileText>>,
}

/// What one search reads from the machine (through its load's `Reads`): the folders it lists, the files it reads,
/// the environment variables it asks for, as they were when first read (what a cache entry checks), and the warnings
/// it gave.
#[derive(Default)]
pub(crate) struct Session<'a> {
    shared: Option<&'a Reads>,
    own: Reads,
    recorded: RefCell<HashSet<String>>,
    folders: RefCell<Vec<(PathBuf, Option<i64>)>>,
    files: RefCell<Vec<(PathBuf, Stamp)>>,
    variables: RefCell<BTreeMap<String, Option<String>>>,
    warnings: RefCell<Vec<String>>,
}

/// A file's stamp before it was read, and its text (None when it could not be read).
type FileText = (Stamp, Option<Arc<str>>);

/// A folder's modification time before it was listed, and its entries.
type Listing = (Option<i64>, Arc<[FolderEntry]>);

/// One entry of a folder: its name, and whether it is a folder (links followed).
struct FolderEntry {
    name: String,
    folder: bool,
}

impl<'a> Session<'a> {
    fn new(reads: &'a Reads) -> Session<'a> {
        Session {
            shared: Some(reads),
            ..Default::default()
        }
    }

    fn reads(&self) -> &Reads {
        self.shared.unwrap_or(&self.own)
    }

    /// An environment variable's value (lossy UTF-8).
    pub(crate) fn variable(&self, name: &str) -> Option<String> {
        let mut variables = self.variables.borrow_mut();
        let value = variables
            .entry(name.to_string())
            .or_insert_with(|| std::env::var_os(name).map(|value| value.to_string_lossy().into_owned()));

        value.clone()
    }

    /// Whether `path` is a file, from its folder's listing.
    pub(crate) fn is_file(&self, path: &Path) -> bool {
        self.kind(path) == Some(false)
    }

    fn is_folder(&self, path: &Path) -> bool {
        self.kind(path) == Some(true)
    }

    /// Whether `path` is a folder (or a file), when it exists: its parent's listing says.
    fn kind(&self, path: &Path) -> Option<bool> {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return path.exists().then(|| path.is_dir());
        };
        let name = util::path_key(Path::new(name));

        self.entries(parent)
            .iter()
            .find(|entry| util::path_key(Path::new(&entry.name)) == name)
            .map(|entry| entry.folder)
    }

    /// A folder's entries (none when it cannot be listed), listed once.
    fn entries(&self, folder: &Path) -> Arc<[FolderEntry]> {
        let key = util::path_key(folder);
        let known = self.reads().listings.lock().unwrap().get(&key).cloned();
        let (time, entries) = known.unwrap_or_else(|| {
            // Its time stamp before its listing: an entry added since changes it
            let time = util::mtime(folder);
            let entries: Arc<[FolderEntry]> = std::fs::read_dir(folder)
                .map(|listing| {
                    listing
                        .flatten()
                        .filter_map(|entry| {
                            let kind = entry.file_type().ok()?;
                            let folder = match kind.is_symlink() {
                                true => std::fs::metadata(entry.path()).ok()?.is_dir(),
                                false => kind.is_dir(),
                            };
                            Some(FolderEntry {
                                name: entry.file_name().to_string_lossy().into_owned(),
                                folder,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            self.reads().listings.lock().unwrap().insert(key.clone(), (time, entries.clone()));
            (time, entries)
        });

        if self.recorded.borrow_mut().insert(format!("d{key}")) {
            self.folders.borrow_mut().push((folder.to_path_buf(), time));
        }
        entries
    }

    /// A file's text, read once.
    fn text(&self, file: &Path) -> Option<Arc<str>> {
        let key = util::path_key(file);
        let known = self.reads().texts.lock().unwrap().get(&key).cloned();
        let (stamp, text) = known.unwrap_or_else(|| {
            let stamp = util::stamp(file);
            let text: Option<Arc<str>> = std::fs::read_to_string(file).ok().map(Arc::from);
            self.reads().texts.lock().unwrap().insert(key.clone(), (stamp, text.clone()));
            (stamp, text)
        });

        if self.recorded.borrow_mut().insert(format!("f{key}")) {
            self.files.borrow_mut().push((file.to_path_buf(), stamp));
        }
        text
    }

    /// Notes that the result depends on a file's content read elsewhere (a library's): true, for use in conditions.
    pub(crate) fn watch(&self, file: &Path) -> bool {
        if self.recorded.borrow_mut().insert(format!("f{}", util::path_key(file))) {
            self.files.borrow_mut().push((file.to_path_buf(), util::stamp(file)));
        }
        true
    }

    /// Says a warning, and keeps it for a cache entry to say again.
    pub(crate) fn warn(&self, message: String) {
        crate::ui::warn(&message);
        self.warnings.borrow_mut().push(message);
    }

    /// The cache entry for a result of this session.
    fn entry(&self, key: String, found: &Found) -> CacheEntry {
        CacheEntry {
            key,
            variables: self
                .variables
                .borrow()
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            folders: self.folders.borrow().clone(),
            files: self.files.borrow().clone(),
            warnings: self.warnings.borrow().clone(),
            package: found.package.clone(),
            usage: found.usage.clone(),
        }
    }
}

/// A path's key, its links resolved: one file reached two ways is one.
fn canonical_key(path: &Path) -> String {
    util::path_key(&std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
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
struct Reader<'a> {
    build: BuildContext,
    debug: bool,
    platform: Platform,
    packages: Vec<LoadedPackage>,
    targets: Vec<ImportedTarget>,

    /// Package names looked for, each once.
    searched: HashSet<String>,

    /// The dependency, for messages; its `prefix` folders, where the packages its package names are looked for first
    /// (as CMAKE_PREFIX_PATH).
    dependency: String,
    prefixes: Vec<PathBuf>,
    session: &'a Session<'a>,
}

impl<'a> Reader<'a> {
    fn new(build: BuildContext, debug: bool, platform: Platform, dependency: &str, prefixes: &[PathBuf], session: &'a Session<'a>) -> Reader<'a> {
        Reader {
            build,
            debug,
            platform,
            packages: Vec::new(),
            targets: Vec::new(),
            searched: HashSet::new(),
            dependency: dependency.to_string(),
            prefixes: prefixes.to_vec(),
            session,
        }
    }

    /// The first config file of `name` in `places` that suits the build (and the version): else what was found and
    /// why it did not suit.
    fn locate(&self, name: &str, places: &[(Place, String)], version: Option<&[u64]>) -> std::result::Result<Located, Vec<String>> {
        let mut rejected = Vec::new();
        let mut seen = HashSet::new();

        for (place, _) in places {
            let (prefix, folders) = match place {
                Place::Prefix(prefix) => (prefix.clone(), config_folders(prefix, name, &self.platform, self.session)),
                Place::Folder(folder) => (folder.clone(), vec![folder.clone()]),
            };
            for folder in folders {
                let Some(config_file) = config_file_in(&folder, name, self.session) else {
                    continue;
                };
                // Each file once, also when two places reach it (/lib/cmake is /usr/lib/cmake where /lib links to usr/lib)
                if !seen.insert(canonical_key(&config_file)) {
                    continue;
                }

                // CMake skips a package its version file finds unsuitable
                let version_file = read_version_file(&config_file, self.session);
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
        let files = included_files(&located.config_file, self.session);
        let mut named = Vec::new();
        let mut hints = Vec::new();
        let mut prefix = located.prefix.clone();
        for file in &files {
            let Some(text) = self.session.text(file) else { continue };
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

    /// Finds and reads package `name`: in the dependency's `prefix` folders (as CMAKE_PREFIX_PATH), beside the packages
    /// read already (find_dependency's usual places), then, unless `nearby` only, in CMake's search. False when it is
    /// not there, or was looked for before.
    fn read_named(&mut self, name: &str, nearby: bool) -> bool {
        if !self.searched.insert(name.to_lowercase()) {
            return false;
        }

        let mut places: Vec<(Place, String)> = self
            .prefixes
            .iter()
            .map(|prefix| (Place::Prefix(prefix.clone()), String::new()))
            .collect();
        for package in &self.packages {
            places.extend(package.hints.iter().map(|hint| (Place::Prefix(hint.clone()), String::new())));
            if let Some(parent) = package.folder.parent() {
                places.push((Place::Prefix(parent.to_path_buf()), String::new()));
            }
            places.push((Place::Prefix(package.prefix.clone()), String::new()));
        }

        // CMake's search only when it is not near (its places, PATH among them, are then no part of the result)
        let mut located = self.locate(name, &places, None);
        if located.is_err() && !nearby {
            located = self.locate(name, &default_places(name, &self.platform, self.session), None);
        }
        match located {
            Ok(located) => {
                self.read_package(&located);
                true
            }
            Err(_) => false,
        }
    }

    /// Reads packages until one defines target `name` (`Qt6::Gui` is in package Qt6Gui, `Boost::filesystem` in
    /// boost_filesystem, `fmt::fmt` in fmt): those the packages read name first, then names made from the target's. A
    /// find module's target (ZLIB::ZLIB) is looked for only near the packages: CMake finds it with the module.
    fn read_package_of(&mut self, name: &str) -> bool {
        let Some((namespace, short)) = name.split_once("::") else { return false };
        if FIND_MODULE_NAMESPACES.contains(&namespace) {
            return false;
        }
        let nearby = find_module::find_module(name).is_some();

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
            if self.read_named(&candidate, nearby) && self.has_target(name) {
                return true;
            }
        }

        false
    }

    /// Defines target `name`: from the packages (`read_package_of`), else as the find module that defines it would.
    fn resolve(&mut self, name: &str) -> bool {
        self.has_target(name) || self.read_package_of(name) || self.read_find_module(name)
    }

    /// Defines a find module's target (ZLIB::ZLIB) from the header and library found for it: in the prefix of the
    /// package linking it, the dependency's `prefix` folders, the other packages' prefixes, CMAKE_PREFIX_PATH, then
    /// the system's. False when the module is unknown or its files were not found (an optional one is then empty).
    fn read_find_module(&mut self, name: &str) -> bool {
        let Some(module) = find_module::find_module(name) else { return false };

        let search = Search {
            session: self.session,
            prefixes: self.module_prefixes(name),
            os: self.platform.os,
            msvc_abi: self.build.is_msvc_abi(),
            debug: self.debug,
            architecture: library_architecture(&self.platform),
            pointer_size: pointer_size(self.platform.cpu),
        };
        let target = match find_module::module_target(module, &search) {
            Some((target, note)) => {
                if let Some(note) = note {
                    self.session.warn(format!("dependency `{}`: {name}: {note}", self.dependency));
                }
                target
            }
            None if module.optional => {
                let mut empty = ImportedTarget::default();
                empty.name = name.to_string();
                empty.kind = "INTERFACE".to_string();
                empty
            }
            None => return false,
        };
        self.targets.push(target);

        true
    }

    /// Where a find module's files are looked for, each once: the prefix of the package linking the target first.
    fn module_prefixes(&self, name: &str) -> Vec<PathBuf> {
        let mut prefixes: Vec<PathBuf> = self.linking_package(name).map(|package| package.prefix.clone()).into_iter().collect();
        prefixes.extend(self.prefixes.iter().cloned());
        prefixes.extend(self.packages.iter().map(|package| package.prefix.clone()));

        // CMAKE_PREFIX_PATH, then the system's (the Windows ones hold no libraries a find module would find)
        let separator = if cfg!(windows) { ';' } else { ':' };
        let listed = self.session.variable("CMAKE_PREFIX_PATH").unwrap_or_default();
        prefixes.extend(listed.split(separator).filter(|entry| !entry.trim().is_empty()).map(PathBuf::from));
        let system: &[&str] = match self.platform.os {
            Os::Windows => &[],
            Os::Linux => &["/usr/local", "/usr"],
            Os::Macos => &["/opt/homebrew", "/usr/local", "/usr"],
        };
        prefixes.extend(system.iter().map(PathBuf::from));

        let mut seen = HashSet::new();
        prefixes.retain(|prefix| seen.insert(util::path_key(prefix)));
        prefixes
    }

    /// The package whose target links target `name`.
    fn linking_package(&self, name: &str) -> Option<&LoadedPackage> {
        let linker = self.linker_of(name)?;
        self.packages.iter().find(|package| package.targets.contains(&linker))
    }

    /// A target that links target `name`.
    fn linker_of(&self, name: &str) -> Option<String> {
        self.targets
            .iter()
            .find(|target| target.link_items.iter().chain(&target.links).any(|item| item == name))
            .map(|target| target.name.clone())
    }

    /// Why target `name`, which a target of the packages links, is left out, and what to do.
    fn unresolved_message(&self, name: &str) -> String {
        let linker = match self.linker_of(name) {
            Some(linker) => match self.packages.iter().find(|package| package.targets.contains(&linker)) {
                Some(package) => format!("{linker} (package {}, {})", package.name, slash(&package.folder)),
                None => linker,
            },
            None => "a target it uses".to_string(),
        };
        let why = match find_module::find_module(name) {
            Some(module) => {
                let prefixes: Vec<String> = self.module_prefixes(name).iter().map(|prefix| slash(prefix)).collect();
                format!(
                    "CMake's find module would define it, and Anvil found no {} in {}",
                    module.wanted(),
                    prefixes.join(", ")
                )
            }
            None => {
                let namespace = name.split_once("::").map_or(name, |(namespace, _)| namespace);
                format!("Anvil found no CMake package that defines it (such as {namespace}) in the dependency's prefixes, beside its packages or in CMake's search")
            }
        };

        format!(
            "dependency `{}`: {linker} links {name}, which is left out: {why}; the build may fail to compile, link or start. \
             Add the folder it is installed in to the dependency's `prefix` list (or CMAKE_PREFIX_PATH), or give it a [deps] entry of its own",
            self.dependency
        )
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
                if !self.resolve(name) {
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
            if self.read_named(&candidate, false) {
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
    fn usage(&mut self, dependency: &str, wanted: &[String]) -> Result<Usage> {
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
                self.resolve(&name);
            }
            if (compiled.len(), linked.len(), self.targets.len()) == before {
                break;
            }
        }

        // The targets of CMake's find modules they name (Threads::Threads, OpenGL::GL); any other target still unknown is
        // said, not dropped silently
        let mut usage = Usage::default();
        let mut named: Vec<String> = requirements.visible.union(&requirements.linked).cloned().collect();
        named.sort();
        for name in &named {
            if self.has_target(name) {
                continue;
            }
            let namespace = name.split_once("::").map_or("", |(namespace, _)| namespace);
            let module = FIND_MODULE_NAMESPACES
                .contains(&namespace)
                .then(|| vcpkg_tree::module_target(name, &self.build))
                .flatten();
            let Some(module) = module else {
                // FindThreads gives Windows programs nothing
                if name.contains("::") && name != "Threads::Threads" {
                    self.session.warn(self.unresolved_message(name));
                }
                continue;
            };
            if requirements.visible.contains(name) {
                requirements.add_compile(&module, &self.build);
            }
            if requirements.linked.contains(name) {
                requirements.add_link(&module, &HashSet::new(), None);
                vcpkg_tree::push_new(&mut usage.runtime, &module.runtime);

                // Vulkan's loader is a file of the SDK, which may lack it for the target's processor
                let library = module.link_items.iter().find(|item| Path::new(item).is_absolute());
                if name == "Vulkan::Vulkan"
                    && self.platform.os == Os::Windows
                    && library.is_none_or(|library| !self.session.is_file(Path::new(library)))
                {
                    let missing = match library {
                        Some(library) => format!("the Vulkan SDK has no {library}"),
                        None => "VULKAN_SDK is not set".to_string(),
                    };
                    self.session.warn(format!(
                        "dependency `{}`: {} links Vulkan::Vulkan, but {missing}: the build will not link; install the Vulkan SDK with its {} libraries",
                        self.dependency,
                        self.linker_of(name).unwrap_or_default(),
                        self.platform.cpu.name()
                    ));
                }
            }
        }

        // Their files, dependents before dependencies, and the shared libraries those load
        let order = self.link_order(wanted, &linked);
        let mut mismatched = Vec::new();
        let mut shared = Vec::new();
        for name in &order {
            let target = self.target(name).unwrap();
            let Some(files) = pick_files(target, self.build.configuration(), self.debug, self.session) else {
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
            self.session.warn(format!(
                "dependency `{dependency}`: this build uses MSVC's {runtime} C runtime, and the package has no {wanted_configuration} configuration of {}: \
                 linking it with this runtime may fail (LNK2038) or corrupt the heap; install that configuration of the package, \
                 or build with a profile whose `debug-runtime` matches",
                mismatched.join(", ")
            ));
        }

        // What the targets require, then the rpaths
        let rpaths = std::mem::take(&mut usage.link_flags);
        requirements
            .check_resolved(&self.build)
            .map_err(|message| anyhow!("dependency `{dependency}`: {message}"))?;
        requirements.fill(&mut usage, &self.build);
        usage.link_flags.extend(rpaths);
        if self.build.os() != Os::Windows {
            usage.includes.retain(|folder| {
                let folder = slash(folder);
                let multiarch = library_architecture(&self.platform).map(|architecture| format!("/usr/include/{architecture}"));
                !IMPLICIT_INCLUDE_FOLDERS.contains(&folder.as_str()) && multiarch.as_deref() != Some(folder.as_str())
            });
        }

        Ok(usage)
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
                if target.kind != "SHARED" && self.session.watch(library) && vcpkg_tree::is_static_library(library) {
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
            if !done.insert(name.clone()) || !self.resolve(&name) {
                continue;
            }
            let target = self.target(&name).unwrap();
            let Some(files) = pick_files(target, self.build.configuration(), self.debug, self.session) else {
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
fn pick_files<'a>(target: &'a ImportedTarget, configuration: &str, debug: bool, session: &Session) -> Option<&'a ImportedFiles> {
    let present = |file: &Option<PathBuf>| file.as_ref().is_some_and(|file| session.is_file(file));
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
fn default_places(name: &str, platform: &Platform, session: &Session) -> Vec<(Place, String)> {
    let separator = if cfg!(windows) { ';' } else { ':' };
    let list = |variable: &str| -> Vec<PathBuf> {
        session
            .variable(variable)
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
    for folder in session
        .variable("PATH")
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
            .filter_map(|variable| session.variable(variable).map(PathBuf::from))
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
fn config_folders(prefix: &Path, name: &str, platform: &Platform, session: &Session) -> Vec<PathBuf> {
    let cmake = |folder: &Path| -> Vec<PathBuf> { subfolders(folder, &["cmake", "CMake"], session) };
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
        subfolders(folder, &names, session)
    };
    let mut folders = vec![prefix.to_path_buf()];

    // Windows-style: the prefix, its cmake/ folder, and the package's folder
    folders.extend(cmake(prefix));
    let named = named_folders(prefix, name, session);
    for folder in &named {
        folders.push(folder.clone());
        for cmake_folder in cmake(folder) {
            folders.push(cmake_folder.clone());
            folders.extend(named_folders(&cmake_folder, name, session));
        }
    }

    // Unix-style: the library and share folders, under the prefix, then under the package's folder
    for base in std::iter::once(prefix.to_path_buf()).chain(named) {
        for library_folder in libraries(&base) {
            for cmake_folder in subfolders(&library_folder, &["cmake"], session) {
                folders.extend(named_folders(&cmake_folder, name, session));
            }
            for folder in named_folders(&library_folder, name, session) {
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
fn subfolders(folder: &Path, names: &[&str], session: &Session) -> Vec<PathBuf> {
    names
        .iter()
        .map(|name| folder.join(name))
        .filter(|candidate| session.is_folder(candidate))
        .collect()
}

/// The subfolders of `folder` whose names start with `name`, in any case, sorted.
fn named_folders(folder: &Path, name: &str, session: &Session) -> Vec<PathBuf> {
    let lower = name.to_lowercase();
    let mut found: Vec<PathBuf> = session
        .entries(folder)
        .iter()
        .filter(|entry| entry.folder && entry.name.to_lowercase().starts_with(&lower))
        .map(|entry| folder.join(&entry.name))
        .collect();
    found.sort();

    found
}

/// The config file of package `name` in `folder`: `<name>Config.cmake` or `<lowercase name>-config.cmake`, as
/// written first, else in any case.
fn config_file_in(folder: &Path, name: &str, session: &Session) -> Option<PathBuf> {
    let wanted = [format!("{name}Config.cmake"), format!("{}-config.cmake", name.to_lowercase())];
    let entries = session.entries(folder);
    let files: Vec<&String> = entries.iter().filter(|entry| !entry.folder).map(|entry| &entry.name).collect();

    let exact = files.iter().find(|entry| wanted.contains(entry));
    let any_case = files.iter().find(|entry| wanted.iter().any(|file| file.eq_ignore_ascii_case(entry)));
    exact.or(any_case).map(|entry| folder.join(entry))
}

/// A config file and the files of its folder it includes, directly or through others: `include()` and `file(GLOB)`
/// of `${CMAKE_CURRENT_LIST_DIR}/...` (a variable in the name matching anything).
fn included_files(config_file: &Path, session: &Session) -> Vec<PathBuf> {
    static REFERENCE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"\$\{(?:CMAKE_CURRENT_LIST_DIR|_DIR|_IMPORT_DIR)\}/([^"\s)/]+)["\s)]"#).unwrap());
    static VARIABLE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"\$\{([^}]*)\}").unwrap());
    static SET: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"set\(\s*(\w+)\s+"?([^"\s)$]+)"?\s*\)"#).unwrap());

    let folder = config_file.parent().unwrap();
    let entries = session.entries(folder);
    let names: Vec<&String> = entries.iter().filter(|entry| !entry.folder).map(|entry| &entry.name).collect();
    let mut files = vec![config_file.to_path_buf()];

    let mut next = 0;
    while next < files.len() {
        // A name made of variables the file sets (`include("${CMAKE_CURRENT_LIST_DIR}/${targets_file}")`); one it
        // does not set matches anything, but not on its own
        let text = session.text(&files[next]).unwrap_or_default();
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
fn read_version_file(config_file: &Path, session: &Session) -> Option<VersionFile> {
    static VERSION: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"set\(\s*PACKAGE_VERSION\s+"?([^"\s)]+)"?\s*\)"#).unwrap());
    static POINTER_SIZE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"CMAKE_SIZEOF_VOID_P\}?"?\s+STREQUAL\s+"(\d+)""#).unwrap());

    let file_name = config_file.file_name()?.to_string_lossy().into_owned();
    let version_name = match file_name.strip_suffix(".cmake")? {
        stem if stem.ends_with("-config") => format!("{stem}-version.cmake"),
        stem => format!("{stem}Version.cmake"),
    };
    let version_file = config_file_in_any_case(config_file.parent()?, &version_name, session)?;
    let text: String = included_files(&version_file, session)
        .iter()
        .filter_map(|file| session.text(file))
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
fn config_file_in_any_case(folder: &Path, name: &str, session: &Session) -> Option<PathBuf> {
    session
        .entries(folder)
        .iter()
        .find(|entry| !entry.folder && entry.name.eq_ignore_ascii_case(name))
        .map(|entry| folder.join(&entry.name))
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

/// The package was not found: where Anvil looked, what it found and turned down, and how to point it at the package
/// (or, for a package CMake finds with its own Find module, what to write instead).
fn not_found_message(request: &Request, places: &[(Place, String)], rejected: &[String]) -> String {
    let name = request.package;
    let dependency = request.dependency;
    let lower = name.to_lowercase();
    let built_in = FIND_MODULE_NAMESPACES.iter().find(|namespace| namespace.eq_ignore_ascii_case(name));
    let module_only = find_module::module_only_package(name);
    let mut message = match (rejected.is_empty(), built_in.is_some() || module_only.is_some()) {
        (true, true) => format!(
            "dependency `{dependency}`: `{name}` has no CMake config file (no {name}Config.cmake or {lower}-config.cmake): CMake finds it with \
             its own Find{name} module, which Anvil does not run"
        ),
        (true, false) => format!("dependency `{dependency}`: CMake package `{name}` not found (no {name}Config.cmake or {lower}-config.cmake)"),
        (false, _) => format!("dependency `{dependency}`: no CMake package `{name}` that suits this build"),
    };

    // Where: each place once (one folder reached through a link too), the user's said missing, the others only when
    // they are there
    let mut looked: Vec<(String, String)> = Vec::new();
    let mut seen = HashSet::new();
    for (place, origin) in places {
        let (Place::Prefix(path) | Place::Folder(path)) = place;
        let exists = path.is_dir();
        let user_given = !matches!(origin.as_str(), "PATH" | "system");
        if !seen.insert(canonical_key(path)) || (!exists && !user_given) {
            continue;
        }
        let text = match exists {
            true => slash(path),
            false => format!("{} (not there)", slash(path)),
        };
        match looked.last_mut().filter(|(last_origin, _)| last_origin == origin) {
            Some((_, paths)) => paths.push_str(&format!(", {text}")),
            None => looked.push((origin.clone(), text)),
        }
    }
    let looked: Vec<String> = looked.iter().map(|(origin, paths)| format!("{origin}: {paths}")).collect();
    message.push_str(&format!("; looked in {}", looked.join("; ")));

    // The variables that would have pointed at it
    let variables = [format!("{name}_DIR"), format!("{name}_ROOT"), "CMAKE_PREFIX_PATH".to_string()];
    let unset: Vec<&String> = variables.iter().filter(|variable| std::env::var_os(variable).is_none()).collect();
    if request.prefixes.is_empty() && !unset.is_empty() && module_only.is_none() && built_in.is_none() {
        let names: Vec<&str> = unset.iter().map(|variable| variable.as_str()).collect();
        message.push_str(&format!(" ({} not set)", names.join(", ")));
    }

    // Found, but not suitable
    for rejection in rejected {
        message.push_str(&format!("\n  found {rejection}"));
    }

    // What to write instead
    match (built_in, module_only) {
        (Some(namespace), _) => message.push_str(&format!(
            "\nhint: Anvil has it built in, as CMake's module: `{} = \"system\"` in [deps]",
            namespace.to_lowercase()
        )),
        (None, Some((_, pkg_config, port))) => {
            let pkg_config = pkg_config.map(|module| format!("`{dependency} = {{ pkg-config = \"{module}\" }}`, or "));
            message.push_str(&format!(
                "\nhint: write {}`{dependency} = {{ vcpkg = \"{port}\" }}` in [deps]",
                pkg_config.unwrap_or_default()
            ));
        }
        (None, None) => message.push_str(&format!(
            "\nhint: point at its install folder (the one holding lib/cmake/{name} or {name}Config.cmake): \
             `{dependency} = {{ cmake = \"{name}\", prefix = \"...\" }}` in [deps], or the environment variable {name}_ROOT, or {name}_DIR \
             for the folder of its config file"
        )),
    }

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
        let version_file = read_version_file(&config, &Session::default()).unwrap();
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
        let version_file = read_version_file(&qt.join("bar-config.cmake"), &Session::default()).unwrap();
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
        let session = Session::default();
        let found: Vec<PathBuf> = config_folders(&folder, "Foo", &unix, &session)
            .iter()
            .filter_map(|folder| config_file_in(folder, "Foo", &session))
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
        assert!(config_file_in(&folder.join("Foo/cmake"), "FOO", &session).is_some());
        write(&folder.join("Foo/cmake/FooConfigVersion.cmake"), "set(PACKAGE_VERSION \"1.0\")\n");
        let session = Session::default();
        let reader = Reader::new(test_build(), false, unix.clone(), "foo", &[], &session);
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
        let session = Session::default();
        assert_eq!(pick_files(&target, "Debug", true, &session).unwrap().configuration, "DEBUG");
        assert_eq!(pick_files(&target, "MinSizeRel", false, &session).unwrap().configuration, "RELEASE");
        assert_eq!(
            pick_files(&target, "RelWithDebInfo", false, &session).unwrap().configuration,
            "RELWITHDEBINFO"
        );

        // Only a debug one: CMake's choice, the first whose files are there
        target.files = vec![missing, files("DEBUG")];
        assert_eq!(pick_files(&target, "Release", false, &session).unwrap().configuration, "DEBUG");

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

        let session = Session::default();
        let mut reader = Reader::new(test_build(), false, platform(Os::Linux), "foo", &[], &session);
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
        let usage = reader.usage("foo", &["Foo::gui".to_string()]).unwrap();
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

    /// A package `name` in `prefix` whose target `name::name` defines NAME and links `links`.
    fn write_package(prefix: &Path, name: &str, links: &str) {
        write(
            &prefix.join(format!("lib/cmake/{name}/{name}Config.cmake")),
            &format!(
                "add_library({name}::{name} INTERFACE IMPORTED)\n\
                 set_target_properties({name}::{name} PROPERTIES INTERFACE_COMPILE_DEFINITIONS \"{}\" INTERFACE_LINK_LIBRARIES \"{links}\")\n",
                name.to_uppercase()
            ),
        );
    }

    #[test]
    fn reads_dependencies_from_the_other_prefixes_and_find_modules() {
        let folder = scratch("prefixes");
        let (first, second) = (folder.join("first"), folder.join("second"));

        // Spd in the first prefix links Fmt::Fmt, in the second, and ZLIB::ZLIB, FindZLIB's, in the first's lib/
        write_package(&first, "Spd", "Fmt::Fmt;ZLIB::ZLIB;Gone::Gone");
        write_package(&second, "Fmt", "");
        write(&first.join("include/zlib.h"), "");
        write(&first.join("lib/libz.a"), "");

        let session = Session::default();
        let prefixes = vec![first.clone(), second];
        let mut reader = Reader::new(test_build(), false, platform(Os::Linux), "spd", &prefixes, &session);
        let places = vec![(Place::Prefix(first.clone()), "`prefix`".to_string())];
        let located = reader.locate("Spd", &places, None).ok().unwrap();
        reader.read_package(&located);
        let usage = reader.usage("spd", &["Spd::Spd".to_string()]).unwrap();
        assert_eq!(usage.defines, vec!["SPD", "FMT"]);
        assert_eq!(usage.includes, vec![util::normalize(&first.join("include"))]);
        assert_eq!(usage.libs, vec![util::normalize(&first.join("lib/libz.a"))]);

        // A target nothing defines is said, with the target linking it
        let warnings = session.warnings.borrow();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("Spd::Spd (package Spd") && warnings[0].contains("links Gone::Gone, which is left out"),
            "{warnings:?}"
        );

        std::fs::remove_dir_all(&folder).ok();
    }

    #[test]
    fn caches_results_until_what_they_came_from_changes() {
        let folder = scratch("cache");
        let prefix = folder.join("prefix");
        let cache = folder.join("cache");
        write_package(&prefix, "Foo", "");
        let toolchain = Toolchain::for_tests(crate::toolchain::Family::Gcc, platform(Os::Linux), false);
        let profile = Profile::resolve("release", &Default::default()).unwrap();
        let prefixes = vec![prefix.clone()];
        let request = Request {
            dependency: "foo",
            package: "Foo",
            components: &[],
            version: None,
            targets: None,
            prefixes: &prefixes,
        };
        let defines = || {
            find(&request, &toolchain, &profile, Some(&cache), &Reads::default())
                .unwrap()
                .usage
                .defines
        };
        let entry = || std::fs::read_dir(&cache).unwrap().next().unwrap().unwrap().path();

        // Written once, then read: a result changed only in the cache is what the next run gives
        assert_eq!(defines(), vec!["FOO"]);
        let text = std::fs::read_to_string(entry()).unwrap();
        std::fs::write(entry(), text.replace("\"FOO\"", "\"CACHED\"")).unwrap();
        assert_eq!(defines(), vec!["CACHED"]);

        // A package file changed (its size) is read again
        write_package(&prefix, "Foo", "m");
        assert_eq!(defines(), vec!["FOO"]);

        // So is a package added where the search looked before it found this one
        let text = std::fs::read_to_string(entry()).unwrap();
        std::fs::write(entry(), text.replace("\"FOO\"", "\"CACHED\"")).unwrap();
        assert_eq!(defines(), vec!["CACHED"]);
        std::fs::create_dir_all(prefix.join("Foo")).unwrap();
        assert_eq!(defines(), vec!["FOO"]);

        std::fs::remove_dir_all(&folder).ok();
    }

    #[test]
    fn says_what_to_write_for_find_module_packages() {
        let request = |package: &'static str| Request {
            dependency: "x",
            package,
            components: &[],
            version: None,
            targets: None,
            prefixes: &[],
        };
        let places = vec![
            (Place::Prefix(PathBuf::from("/nowhere")), "PATH".to_string()),
            (Place::Prefix(std::env::temp_dir()), "PATH".to_string()),
            (Place::Prefix(std::env::temp_dir()), "system".to_string()),
        ];

        let message = not_found_message(&request("OpenSSL"), &places, &[]);
        assert!(message.contains("`OpenSSL` has no CMake config file"), "{message}");
        assert!(
            message.contains("`x = { pkg-config = \"openssl\" }`, or `x = { vcpkg = \"openssl\" }`"),
            "{message}"
        );
        assert!(
            !message.contains("nowhere") && message.matches(&slash(&std::env::temp_dir())).count() == 1,
            "{message}"
        );

        let message = not_found_message(&request("NoSuch"), &places, &[]);
        assert!(
            message.contains("CMake package `NoSuch` not found") && message.contains("NoSuch_ROOT"),
            "{message}"
        );
    }
}
