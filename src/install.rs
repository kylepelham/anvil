//! `anvil install`: builds, then copies what the project ships into a prefix (programs and DLLs to bin/, libraries to
//! lib/, public headers to include/), and writes the CMake package and pkg-config files other builds use its libraries
//! through.

use crate::commands;
use crate::plan::Plan;
use crate::platform::Cpu;
use crate::project::Model;
use crate::targets::{self, Kind, Target};
use crate::ui;
use crate::util::{self, slash};
use crate::vcpkg_tree::{CmakePackage, InstalledTree};
use crate::BuildArgs;
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

// What a public include folder that holds other files too (the package root, sources, generated files) installs
const HEADER_EXTENSIONS: &[&str] = &["h", "hh", "hpp", "hxx", "h++", "inl", "inc", "ipp", "tpp"];

// What builds a library rather than uses it: a public include folder of its own installs everything else
const SOURCE_EXTENSIONS: &[&str] = &[
    "c", "cc", "cpp", "cxx", "c++", "cp", "m", "mm", "cu", "rc", "asm", "s", "def", "manifest", "ixx", "cppm", "ccm", "cxxm", "c++m", "mpp",
];

// Source files a library's C++ users link it through, for IMPORTED_LINK_INTERFACE_LANGUAGES
const CXX_EXTENSIONS: &[&str] = &["cpp", "cc", "cxx", "c++", "cp", "cu", "mm"];

/// One file of the install: a copy of a built or source file, or text Anvil writes.
enum Content {
    Copy(PathBuf),
    Text(String),
}

struct InstallFile {
    /// Relative to the prefix, with forward slashes.
    destination: String,
    content: Content,
    /// Who installs it, for messages: "target `core`".
    owner: String,
}

/// Where one installed library's files went, relative to the prefix.
#[derive(Default)]
struct InstalledLibrary {
    /// The file itself (a DLL in bin/), and the one users link (its import library) when that is another.
    location: Option<String>,
    import_library: Option<String>,
    has_headers: bool,
    /// Public include folders outside the package (an SDK's), which stay where they are.
    external_includes: Vec<PathBuf>,
}

/// What users of the installed libraries get, gathered for the CMake package and the pkg-config files.
#[derive(Default)]
struct Export {
    /// The CMake packages the libraries use (vcpkg ports), each once.
    dependencies: Vec<CmakePackage>,
    /// One `add_library` block per library, with what does not depend on the configuration.
    blocks: Vec<String>,
    /// The configuration's files of each library that has some (a `<project>Targets-<profile>.cmake` file).
    locations: Vec<String>,
    /// The imported targets with files of their own, which a configuration is chosen for.
    built_targets: Vec<String>,
    /// One pkg-config file per library.
    pkg_configs: Vec<PkgConfig>,
    /// The package refers to files in the build's output folder (vcpkg's packages), which installing does not copy.
    uses_build_folder: bool,
}

/// One library's pkg-config file, `<project>-<library>.pc`.
struct PkgConfig {
    library: String,
    requires: Vec<String>,
    cflags: Vec<String>,
    libs: Vec<String>,
}

struct Installer<'a> {
    model: &'a Model,
    plan: &'a Plan,
    prefix: PathBuf,
    /// Where the programs and libraries go in the prefix: "", or "debug/" for a build with the debug runtime, as vcpkg
    /// lays them out, so a debug and a release install share a prefix (headers and the CMake package are shared).
    root: &'static str,

    // Every file once, by destination
    files: Vec<InstallFile>,
    destinations: HashMap<String, usize>,
}

/// `anvil install [targets]`: builds the targets to install (with the release profile unless `-p` names another),
/// then copies their files into the prefix (default `out/install`), each only when it changed, and removes what the
/// last install of this configuration copied that is no longer installed.
pub fn install(start_dir: &Path, build: &BuildArgs, names: &[String], prefix: Option<&Path>) -> Result<()> {
    let mut build = build.clone();
    if !build.release && build.profile.is_none() {
        build.release = true;
    }

    let model = commands::load_model(start_dir, &build)?;
    let chosen = chosen_targets(&model, names)?;
    let installed = with_needed(&model, &chosen)?;

    let (model, plan) = commands::build_roots(model, &build, &installed)?;
    let prefix = match prefix {
        Some(prefix) => util::absolute(&std::env::current_dir()?, prefix),
        None => model.out_root.join("install"),
    };

    // The targets' files, then the package files describing the libraries
    let mut installer = Installer {
        model: &model,
        plan: &plan,
        prefix,
        root: if model.profile.debug_runtime { "debug/" } else { "" },
        files: Vec::new(),
        destinations: HashMap::new(),
    };
    let mut libraries = HashMap::new();
    for index in &installed {
        let library = installer.add_target(*index)?;
        if model.targets[*index].kind.is_library() {
            libraries.insert(*index, library);
        }
    }

    let project = model.packages[0].manifest.project.clone().unwrap_or_default();
    if project.export.unwrap_or(true) && !libraries.is_empty() {
        installer.add_package_files(&installed, &libraries)?;
    }

    installer.write()
}

/// The targets asked for, or every program and library of the project that builds by default (or says
/// `install = true`) and does not say `install = false`.
fn chosen_targets(model: &Model, names: &[String]) -> Result<Vec<usize>> {
    let members = || model.targets.iter().filter(|target| model.packages[target.package].member);
    for target in members().filter(|target| target.install == Some(true)) {
        check_installable(target).with_context(|| format!("[target.{}] install = true", target.name))?;
    }

    if !names.is_empty() {
        let chosen = names.iter().map(|name| model.find_target(name)).collect::<Result<Vec<_>>>()?;
        for index in &chosen {
            check_installable(&model.targets[*index])?;
        }
        return Ok(chosen);
    }

    let chosen: Vec<usize> = (0..model.targets.len())
        .filter(|index| {
            let target = &model.targets[*index];
            let wanted = target.install.unwrap_or(target.default);
            model.packages[target.package].member && installable(target.kind) && wanted
        })
        .collect();
    if chosen.is_empty() {
        if members().any(|target| installable(target.kind) && target.install == Some(false)) {
            bail!(
                "nothing to install: every program and library of the project says `install = false` or does not build by default; name the targets (`anvil install <target>`) or let one install"
            );
        }
        bail!("nothing to install: the project has no program or library that builds by default (`install = true` takes one that does not)");
    }

    Ok(chosen)
}

fn installable(kind: Kind) -> bool {
    matches!(kind, Kind::Exe | Kind::Static | Kind::Shared | Kind::Headers)
}

fn check_installable(target: &Target) -> Result<()> {
    match target.kind {
        kind if installable(kind) => Ok(()),
        Kind::Test => bail!("`{}` is a test, and tests are not installed", target.name),
        kind => bail!(
            "`{}` is a {} target; `anvil install` takes programs and static, shared and header-only libraries",
            target.name,
            kind.name()
        ),
    }
}

/// The chosen targets and the libraries they need installed with them, in model order: what users of an installed
/// library link (its public dependencies; all of a static or header-only library's) and every shared library an
/// installed target loads.
fn with_needed(model: &Model, chosen: &[usize]) -> Result<Vec<usize>> {
    let mut installed = vec![false; model.targets.len()];
    let mut pending: Vec<(usize, Option<usize>)> = chosen.iter().map(|index| (*index, None)).collect();

    while let Some((index, needed_by)) = pending.pop() {
        if std::mem::replace(&mut installed[index], true) {
            continue;
        }
        let target = &model.targets[index];

        // Needed, but kept out
        if let Some(user) = needed_by.map(|user| &model.targets[user].name) {
            if target.install == Some(false) && model.packages[target.package].member {
                bail!(
                    "`{user}` is installed and needs `{}`, which says install = false; let it install, or leave `{user}` out",
                    target.name
                );
            }
            if target.kind == Kind::Object {
                bail!(
                    "`{user}` is installed, and its users would link the objects of object library `{}`, which cannot be installed; make `{}` kind = \"lib\"",
                    target.name,
                    target.name
                );
            }
        }

        // What a library's users link: its public dependencies, all of them for a static or header-only one
        if target.kind.is_library() {
            for (dependency, public) in &target.deps {
                let kind = model.targets[*dependency].kind;
                let passed_on = *public || target.kind != Kind::Shared;
                if passed_on && matches!(kind, Kind::Static | Kind::Shared | Kind::Headers | Kind::Object) {
                    pending.push((*dependency, Some(index)));
                }
            }
        }

        // The shared libraries it loads, through whatever it links
        for shared in loaded_libraries(model, index) {
            pending.push((shared, Some(index)));
        }
    }

    Ok((0..model.targets.len()).filter(|index| installed[*index]).collect())
}

/// Every shared library in a target's dependencies, directly or through others.
fn loaded_libraries(model: &Model, index: usize) -> Vec<usize> {
    let mut visited = HashSet::new();
    let mut pending = vec![index];
    let mut shared = Vec::new();

    while let Some(current) = pending.pop() {
        for (dependency, _) in &model.targets[current].deps {
            if visited.insert(*dependency) {
                if model.targets[*dependency].kind == Kind::Shared {
                    shared.push(*dependency);
                }
                pending.push(*dependency);
            }
        }
    }

    shared
}

impl Installer<'_> {
    /// A target's files: programs (with their DLLs, debug information and data) and DLLs in bin/, other libraries in
    /// lib/, a library's public headers in include/.
    fn add_target(&mut self, index: usize) -> Result<InstalledLibrary> {
        let model = self.model;
        let target = &model.targets[index];
        let toolchain = &model.toolchain;
        let owner = format!("target `{}`", target.name);
        let mut library = InstalledLibrary::default();

        if let Some(file) = self.plan.artifacts.get(&index) {
            let windows = toolchain.targets_windows();
            let bin = format!("{}bin", self.root);
            let lib = format!("{}lib", self.root);
            let folder = match target.kind {
                Kind::Exe => &bin,
                Kind::Shared if windows => &bin,
                _ => &lib,
            };
            let location = format!("{folder}/{}", file_name(file));
            self.add(&location, Content::Copy(file.clone()), &owner)?;

            // The PDB the link wrote beside a program or DLL
            let pdb = file.with_extension("pdb");
            if folder == &bin && toolchain.is_msvc_abi() && model.profile.debug && pdb.is_file() {
                self.add(&format!("{bin}/{}", file_name(&pdb)), Content::Copy(pdb), &owner)?;
            }

            // The import library users of a DLL link
            if let Some(import_library) = self.plan.import_libs.get(&index).filter(|_| target.kind == Kind::Shared) {
                let destination = format!("{lib}/{}", file_name(import_library));
                self.add(&destination, Content::Copy(import_library.clone()), &owner)?;
                library.import_library = Some(destination);
            }

            // The files Anvil copies beside it (its dependencies' DLLs, the sanitizer's), and its data files. Off
            // Windows, shared libraries go to lib/, where programs in bin/ find them (rpath $ORIGIN/../lib)
            for runtime in self.plan.runtime_files.get(&index).into_iter().flatten() {
                let runtime_folder = if !windows && is_shared_library(runtime) { &lib } else { folder };
                self.add(
                    &format!("{runtime_folder}/{}", file_name(runtime)),
                    Content::Copy(runtime.clone()),
                    &owner,
                )?;
            }
            for (data, relative) in &target.data {
                self.add(&format!("{folder}/{}", slash(relative)), Content::Copy(data.clone()), &owner)?;
            }

            library.location = Some(location);
        }

        if target.kind.is_library() {
            self.add_headers(index, &mut library)?;
        }

        Ok(library)
    }

    /// A library's public headers into include/, at their paths under each public include folder; the headers its
    /// build steps and export-header write too. Folders outside the project (an SDK's, vcpkg's) stay where they are.
    fn add_headers(&mut self, index: usize, library: &mut InstalledLibrary) -> Result<()> {
        let model = self.model;
        let target = &model.targets[index];
        let package = &model.packages[target.package];
        let owner = format!("target `{}`", target.name);

        let mut folders: Vec<PathBuf> = target
            .include
            .iter()
            .chain(&target.public_system_include)
            .chain(&target.interface_include)
            .cloned()
            .collect();
        if !target.runs.is_empty() {
            folders.push(targets::gen_dir(&model.package_out_dir(target.package), &target.name));
        }

        // What builds the package (source folders, private include folders, tests/), and what a walk never enters
        let build_folders = build_folders(model, target.package);
        let prefix_key = util::path_key(&self.prefix);

        let mut seen = HashSet::new();
        for folder in folders.into_iter().filter(|folder| seen.insert(util::path_key(folder))) {
            // The project's own folders install (a workspace member's may lie beside it), the build's generated ones
            // too; the rest (an SDK's, vcpkg's in the output folder) stays where it is
            let generated = util::strip_prefix_ci(&folder, &model.out_dir).is_some();
            let in_project = util::strip_prefix_ci(&folder, &model.out_root).is_none()
                && (util::strip_prefix_ci(&folder, &package.root).is_some()
                    || (package.member && util::strip_prefix_ci(&folder, &model.root).is_some()));
            if !generated && !in_project {
                library.external_includes.push(folder);
                continue;
            }

            // A folder holding the rest of the package too (its root, the target's folder) gives its headers, also
            // extensionless ones (Eigen/Core), but nothing of the folders that build the package
            let key = util::path_key(&folder);
            let mixed = !generated && [&package.root, &target.dir, &model.root].iter().any(|other| util::path_key(other) == key);
            let walker = walkdir::WalkDir::new(&folder).sort_by_file_name().into_iter().filter_entry(|entry| {
                let inner_folder = entry.depth() > 0 && entry.file_type().is_dir();
                let entry_key = util::path_key(entry.path());
                let skipped = entry_key == prefix_key || (mixed && build_folders.contains(&entry_key));
                !targets::is_skipped_dir(entry, &model.out_root) && !(inner_folder && skipped)
            });
            for entry in walker.flatten().filter(|entry| entry.file_type().is_file()) {
                if entry.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                let extension = entry.path().extension().map(|extension| extension.to_string_lossy().to_lowercase());
                let is_header = extension.as_deref().is_some_and(|extension| HEADER_EXTENSIONS.contains(&extension));

                // Only headers beside sources (src/ as a public include folder)
                let parent_key = entry.path().parent().map(util::path_key).unwrap_or_default();
                let wanted = if generated || build_folders.contains(&parent_key) {
                    is_header
                } else if mixed {
                    is_header || extension.is_none()
                } else {
                    !extension.as_deref().is_some_and(|extension| SOURCE_EXTENSIONS.contains(&extension))
                };
                let Ok(relative) = entry.path().strip_prefix(&folder) else { continue };
                if wanted {
                    self.add(&format!("include/{}", slash(relative)), Content::Copy(entry.path().to_path_buf()), &owner)?;
                    library.has_headers = true;
                }
            }
        }

        Ok(())
    }

    /// `lib/cmake/<project>/<project>Config.cmake` (with a version file when the project has a version) and
    /// `lib/pkgconfig/<project>.pc`, describing the installed libraries.
    fn add_package_files(&mut self, installed: &[usize], libraries: &HashMap<usize, InstalledLibrary>) -> Result<()> {
        let model = self.model;
        let package = &model.packages[0];
        let project = package.name.clone();
        if !project
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_.+-".contains(character))
        {
            bail!(
                "the project's name `{project}` cannot name a CMake package; set [project] name (letters, digits, `_`, `-`, `.`), or [project] export = false"
            );
        }

        // One CMake target per library, so names must differ across packages
        let mut names: HashMap<&str, usize> = HashMap::new();
        for index in installed.iter().filter(|index| libraries.contains_key(index)) {
            let target = &model.targets[*index];
            if let Some(other) = names.insert(&target.name, *index) {
                bail!(
                    "two installed libraries are named `{}` (packages `{}` and `{}`), and both would be `{project}::{}`; rename one",
                    target.name,
                    model.packages[model.targets[other].package].name,
                    model.packages[target.package].name,
                    target.name
                );
            }
        }

        // vcpkg's installed tree says which CMake packages its ports are
        let vcpkg_tree = match &model.vcpkg {
            Some(_) => Some(InstalledTree::load(model.vcpkg_dir.parent().unwrap(), &model.vcpkg_triplet)?),
            None => None,
        };

        // Users before the libraries they use
        let mut export = Export::default();
        for index in model.closure(installed).into_iter().rev() {
            if let Some(library) = libraries.get(&index) {
                export.add_library(model, &project, index, library, vcpkg_tree.as_ref());
            }
        }

        // The CMake package: what every configuration shares, then this configuration's files
        let owner = "the CMake package";
        let folder = format!("lib/cmake/{project}");
        let configuration = targets::sanitize_name(&model.profile.name);
        self.add(
            &format!("{folder}/{project}Config.cmake"),
            Content::Text(export.cmake_config(model, &project)),
            owner,
        )?;
        self.add(
            &format!("{folder}/{project}Targets-{configuration}.cmake"),
            Content::Text(export.cmake_locations(model, &project)),
            owner,
        )?;
        if let Some(version) = package.manifest.project.as_ref().and_then(|project| project.version.as_deref()) {
            let text = cmake_version_file(&project, version, model.toolchain.platform.cpu);
            self.add(&format!("{folder}/{project}ConfigVersion.cmake"), Content::Text(text), owner)?;
        }

        // pkg-config: one file per library, and one for them all
        let pkg_config_folder = format!("{}lib/pkgconfig", self.root);
        for pkg_config in &export.pkg_configs {
            let text = export.pkg_config(model, &project, pkg_config, self.root);
            self.add(
                &format!("{pkg_config_folder}/{project}-{}.pc", pkg_config.library),
                Content::Text(text),
                "the pkg-config files",
            )?;
        }
        self.add(
            &format!("{pkg_config_folder}/{project}.pc"),
            Content::Text(export.pkg_config_all(model, &project)),
            "the pkg-config files",
        )?;

        // What the package names in the build's output folder stays there
        if export.uses_build_folder {
            ui::warn(&format!(
                "the installed package refers to vcpkg's packages in {}, which are not copied into the prefix: keep that folder, or install them where the package's users find them",
                util::rel(&model.vcpkg_dir, &model.root)
            ));
        }

        Ok(())
    }

    /// Adds a file to the install; two different files for one destination are an error naming both.
    fn add(&mut self, destination: &str, content: Content, owner: &str) -> Result<()> {
        let key = util::path_key(Path::new(destination));
        let Some(existing) = self.destinations.get(&key).map(|index| &self.files[*index]) else {
            self.destinations.insert(key, self.files.len());
            self.files.push(InstallFile {
                destination: destination.to_string(),
                content,
                owner: owner.to_string(),
            });
            return Ok(());
        };

        if same_content(&existing.content, &content) {
            return Ok(());
        }
        bail!(
            "{} and {owner} would both install {destination}: {} and {}; rename one of them",
            existing.owner,
            describe(&existing.content),
            describe(&content)
        );
    }

    /// Writes every file that is missing from the prefix or differs, removes the files the last install of this
    /// configuration wrote that are no longer installed (as its manifest lists them), then says how many.
    fn write(&self) -> Result<()> {
        let prefix = &self.prefix;
        let mut copied = 0;
        for file in &self.files {
            let destination = prefix.join(&file.destination);
            let changed = match &file.content {
                Content::Copy(source) => copy_if_changed(source, &destination)?,
                Content::Text(text) => util::write_if_changed(&destination, text.as_bytes())?,
            };
            copied += changed as usize;
        }

        // The manifest of each configuration's install: the files a debug install shares with a release one stay
        let project = targets::sanitize_name(&self.model.packages[0].name);
        let manifest_path = |root: &str| prefix.join(format!("{root}share/{project}/install_manifest.txt"));
        let other_root = if self.root.is_empty() { "debug/" } else { "" };
        let installed: Vec<&str> = self.files.iter().map(|file| file.destination.as_str()).collect();
        let kept: HashSet<String> = installed
            .iter()
            .map(|destination| destination.to_string())
            .chain(read_manifest(&manifest_path(other_root)))
            .map(|destination| util::path_key(Path::new(&destination)))
            .collect();
        let mut removed = 0;
        for stale in read_manifest(&manifest_path(self.root)) {
            if !kept.contains(&util::path_key(Path::new(&stale))) {
                removed += remove_installed(prefix, &stale)? as usize;
            }
        }
        util::write_if_changed(&manifest_path(self.root), format!("{}\n", installed.join("\n")).as_bytes())?;

        let shown = util::rel(prefix, &std::env::current_dir()?);
        let unchanged = self.files.len() - copied;
        let removed = if removed > 0 { format!(", {removed} removed") } else { String::new() };
        ui::status("Installed", &format!("{shown}: {copied} files copied, {unchanged} up to date{removed}"));

        Ok(())
    }
}

/// The files an install manifest lists, relative to the prefix; only plain relative paths, so removing them never
/// leaves the prefix.
fn read_manifest(path: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter(|line| {
            let path = Path::new(line);
            !line.is_empty() && path.components().all(|component| matches!(component, std::path::Component::Normal(_)))
        })
        .map(str::to_string)
        .collect()
}

/// Removes a file an earlier install wrote, and the folders it leaves empty up to the prefix.
fn remove_installed(prefix: &Path, relative: &str) -> Result<bool> {
    let path = prefix.join(relative);
    if !path.is_file() {
        return Ok(false);
    }
    std::fs::remove_file(&path).with_context(|| format!("removing {} (no longer installed)", slash(&path)))?;

    let mut folder = path.parent();
    while let Some(current) = folder.filter(|current| *current != prefix) {
        if std::fs::remove_dir(current).is_err() {
            break;
        }
        folder = current.parent();
    }

    Ok(true)
}

impl Export {
    /// One library's CMake target, its files in this configuration, and its pkg-config file.
    fn add_library(&mut self, model: &Model, project: &str, index: usize, library: &InstalledLibrary, vcpkg_tree: Option<&InstalledTree>) {
        let target = &model.targets[index];
        let name = format!("{project}::{}", target.name);
        let mut notes = Vec::new();
        let mut pkg_config = PkgConfig {
            library: target.name.clone(),
            requires: Vec::new(),
            cflags: Vec::new(),
            libs: Vec::new(),
        };

        // Its own usage: headers, definitions, flags
        let mut includes = Vec::new();
        if library.has_headers {
            includes.push("${_IMPORT_PREFIX}/include".to_string());
            push_once(&mut pkg_config.cflags, "-I${includedir}".to_string());
        }
        for folder in &library.external_includes {
            self.uses_build_folder |= util::strip_prefix_ci(folder, &model.out_root).is_some();
            includes.push(cmake_escape(&slash(folder)));
            push_once(&mut pkg_config.cflags, pkg_config_word(&format!("-I{}", slash(folder))));
        }
        let defines: Vec<&String> = target.public_defines.iter().chain(&target.interface_defines).collect();
        let options: Vec<&String> = target.public_flags.iter().chain(&target.interface_flags).collect();
        for define in &defines {
            push_once(&mut pkg_config.cflags, pkg_config_word(&format!("-D{define}")));
        }
        for option in &options {
            push_once(&mut pkg_config.cflags, pkg_config_word(option));
        }

        // Its file
        let linked_file = library.import_library.as_ref().or(library.location.as_ref());
        if let Some(file) = linked_file {
            push_once(&mut pkg_config.libs, format!("${{prefix}}/{file}"));
        }

        // What its users link: a static or header-only library passes on all its dependencies (private ones linked
        // only), a shared one its public ones; and a static one its own system libraries and link flags
        let mut links = Vec::new();
        let mut link_options = Vec::new();
        let mut link_dirs = Vec::new();
        let shared = target.kind == Kind::Shared;
        for (dependency_index, public) in &target.deps {
            if shared && !public {
                continue;
            }
            let link_only = !public && target.kind == Kind::Static;
            let item = |text: String| if link_only { format!("$<LINK_ONLY:{text}>") } else { text };
            let dependency = &model.targets[*dependency_index];

            match dependency.kind {
                Kind::Static | Kind::Shared | Kind::Headers => {
                    links.push(item(format!("{project}::{}", dependency.name)));
                    push_once(&mut pkg_config.requires, format!("{project}-{}", dependency.name));
                }
                Kind::External => {
                    // A vcpkg port CMake can find: its package and targets
                    let port = dependency.vcpkg.as_ref().map(|(port, _)| port.as_str());
                    let build = crate::vcpkg_tree::BuildContext::new(&model.toolchain, &model.profile);
                    let package = port.and_then(|port| vcpkg_tree.and_then(|tree| tree.cmake_package(port, &build)));
                    self.uses_build_folder |= port.is_some();
                    if let Some(package) = package {
                        links.extend(package.targets.iter().map(|target| item(target.clone())));
                        if !self.dependencies.contains(&package) {
                            self.dependencies.push(package);
                        }
                    } else {
                        if let Some(port) = port {
                            notes.push(format!(
                                "vcpkg port `{port}` has no CMake package Anvil can name: its files are linked by path"
                            ));
                        }
                        links.extend(dependency.libs.iter().map(|lib| item(cmake_escape(lib))));
                        link_options.extend(dependency.link_flags.iter().map(|flag| cmake_escape(flag)));
                        link_dirs.extend(dependency.lib_dirs.iter().map(|folder| cmake_escape(&slash(folder))));
                        if *public {
                            includes.extend(dependency.public_system_include.iter().map(|folder| cmake_escape(&slash(folder))));
                        }
                    }

                    // pkg-config gets the files, and the include folders and definitions of public ones
                    for lib in &dependency.libs {
                        push_once(&mut pkg_config.libs, pkg_config_lib(lib));
                    }
                    if *public {
                        for folder in &dependency.public_system_include {
                            push_once(&mut pkg_config.cflags, pkg_config_word(&format!("-I{}", slash(folder))));
                        }
                        for define in &dependency.public_defines {
                            push_once(&mut pkg_config.cflags, pkg_config_word(&format!("-D{define}")));
                        }
                    }
                }
                _ => {}
            }
        }
        if target.kind == Kind::Static {
            links.extend(target.libs.iter().map(|lib| cmake_escape(lib)));
            link_options.extend(target.link_flags.iter().map(|flag| cmake_escape(flag)));
            link_dirs.extend(target.lib_dirs.iter().map(|folder| cmake_escape(&slash(folder))));
            for lib in &target.libs {
                push_once(&mut pkg_config.libs, pkg_config_lib(lib));
            }
        }
        self.pkg_configs.push(pkg_config);

        // The block every configuration shares
        let kind = match target.kind {
            Kind::Static => "STATIC",
            Kind::Shared => "SHARED",
            _ => "INTERFACE",
        };
        let lists = [
            ("INTERFACE_INCLUDE_DIRECTORIES", includes),
            (
                "INTERFACE_COMPILE_DEFINITIONS",
                defines.iter().map(|define| cmake_escape(define)).collect(),
            ),
            ("INTERFACE_COMPILE_OPTIONS", options.iter().map(|option| cmake_escape(option)).collect()),
            ("INTERFACE_LINK_LIBRARIES", links),
            ("INTERFACE_LINK_OPTIONS", link_options),
            ("INTERFACE_LINK_DIRECTORIES", link_dirs),
        ];
        let mut properties = Vec::new();
        for (property, mut values) in lists {
            dedup(&mut values);
            if !values.is_empty() {
                properties.push((property.to_string(), values.join(";")));
            }
        }

        let mut block = String::new();
        for note in notes {
            block.push_str(&format!("# {note}\n"));
        }
        block.push_str(&format!("if(NOT TARGET {name})\n  add_library({name} {kind} IMPORTED)\n"));
        block.push_str(&target_properties(&name, &properties, "  "));
        block.push_str("endif()\n");
        self.blocks.push(block);

        // This configuration's files
        let Some(location) = &library.location else { return };
        let configuration = configuration_name(&model.profile.name);
        let mut properties = vec![(format!("IMPORTED_LOCATION_{configuration}"), format!("${{_IMPORT_PREFIX}}/{location}"))];
        if let Some(import_library) = &library.import_library {
            properties.push((
                format!("IMPORTED_IMPLIB_{configuration}"),
                format!("${{_IMPORT_PREFIX}}/{import_library}"),
            ));
        }
        if target.kind == Kind::Static {
            properties.push((
                format!("IMPORTED_LINK_INTERFACE_LANGUAGES_{configuration}"),
                link_language(target).to_string(),
            ));
        }
        let mut block = format!("set_property(TARGET {name} APPEND PROPERTY IMPORTED_CONFIGURATIONS {configuration})\n");
        block.push_str(&target_properties(&name, &properties, ""));
        self.locations.push(block);
        self.built_targets.push(name);
    }

    /// `<project>Config.cmake`: the dependencies found, one imported target per library, then the files of each
    /// installed configuration, from the `<project>Targets-<profile>.cmake` files beside it.
    fn cmake_config(&self, model: &Model, project: &str) -> String {
        let toolchain = &model.toolchain;
        let mut text = format!(
            "# The {project} CMake package, written by `anvil install` ({} {}); each installed configuration's files are in a {project}Targets-<profile>.cmake beside it.\n\
             # Use: find_package({project} CONFIG REQUIRED), then target_link_libraries(app PRIVATE {project}::<library>).\n\n\
             if(CMAKE_VERSION VERSION_LESS 3.13)\n  message(FATAL_ERROR \"{project} needs CMake 3.13 or newer\")\nendif()\n\n",
            toolchain.family.name(),
            toolchain.version
        );

        // Dependencies, also from the vcpkg packages the build used when the user's paths do not find them; the
        // search path is restored before a missing one ends the file
        if !self.dependencies.is_empty() {
            let vcpkg = cmake_escape(&slash(&model.vcpkg_dir));
            text.push_str(&format!(
                "set(_{project}_prefix_path \"${{CMAKE_PREFIX_PATH}}\")\n\
                 list(APPEND CMAKE_PREFIX_PATH \"{vcpkg}\")\n\
                 set(_{project}_quiet \"\")\n\
                 if({project}_FIND_QUIETLY)\n  set(_{project}_quiet QUIET)\nendif()\n\
                 set(_{project}_missing \"\")\n"
            ));
            for package in &self.dependencies {
                let mode = if package.config { " CONFIG" } else { "" };
                text.push_str(&format!(
                    "if(NOT _{project}_missing)\n  find_package({name}{mode} ${{_{project}_quiet}})\n  if(NOT {name}_FOUND)\n    set(_{project}_missing {name})\n  endif()\nendif()\n",
                    name = package.name
                ));
            }
            text.push_str(&format!(
                "set(CMAKE_PREFIX_PATH \"${{_{project}_prefix_path}}\")\n\
                 unset(_{project}_prefix_path)\n\
                 unset(_{project}_quiet)\n\
                 if(_{project}_missing)\n\
                 \x20 set({project}_NOT_FOUND_MESSAGE \"{project} needs ${{_{project}_missing}}, which was not found (its build used the one in {vcpkg})\")\n\
                 \x20 set({project}_FOUND FALSE)\n\
                 \x20 unset(_{project}_missing)\n\
                 \x20 return()\n\
                 endif()\n\
                 unset(_{project}_missing)\n\n"
            ));
        }

        // The prefix: this file is in <prefix>/lib/cmake/<project>
        text.push_str("get_filename_component(_IMPORT_PREFIX \"${CMAKE_CURRENT_LIST_DIR}/../../..\" ABSOLUTE)\n\n");
        for block in &self.blocks {
            text.push_str(block);
            text.push('\n');
        }

        // Each installed configuration's files
        text.push_str(&format!(
            "file(GLOB _{project}_configurations \"${{CMAKE_CURRENT_LIST_DIR}}/{project}Targets-*.cmake\")\n\
             foreach(_{project}_file IN LISTS _{project}_configurations)\n  include(\"${{_{project}_file}}\")\nendforeach()\n\
             unset(_{project}_configurations)\nunset(_{project}_file)\n\n"
        ));

        // MSVC: a Debug build takes the libraries built with the debug runtime, the others those built with the
        // release runtime; a build that would mix the two (which corrupts the heap) finds no package
        if toolchain.is_msvc_abi() && !self.built_targets.is_empty() {
            let targets = self.built_targets.join(" ");
            text.push_str(&format!(
                r#"if(MSVC)
  set(_{project}_debug_map ${{_{project}_debug_runtime}} NOT_INSTALLED)
  set(_{project}_release_map ${{_{project}_release_runtime}} NOT_INSTALLED)
  set_target_properties({targets} PROPERTIES
    MAP_IMPORTED_CONFIG_DEBUG "${{_{project}_debug_map}}"
    MAP_IMPORTED_CONFIG_RELEASE "${{_{project}_release_map}}"
    MAP_IMPORTED_CONFIG_RELWITHDEBINFO "${{_{project}_release_map}}"
    MAP_IMPORTED_CONFIG_MINSIZEREL "${{_{project}_release_map}}"
  )
  if(NOT CMAKE_CONFIGURATION_TYPES)
    string(TOUPPER "${{CMAKE_BUILD_TYPE}}" _{project}_build_type)
    if(_{project}_build_type STREQUAL "DEBUG" AND NOT _{project}_debug_runtime)
      set({project}_NOT_FOUND_MESSAGE "this Debug build uses the debug C runtime, and ${{_IMPORT_PREFIX}} holds the {project} libraries built with the release runtime only (mixing the two corrupts the heap): install a debug build there too (anvil install -p debug), or build this project in Release")
      set({project}_FOUND FALSE)
    elseif(NOT _{project}_build_type STREQUAL "DEBUG" AND NOT _{project}_release_runtime)
      set({project}_NOT_FOUND_MESSAGE "this ${{CMAKE_BUILD_TYPE}} build uses the release C runtime, and ${{_IMPORT_PREFIX}} holds the {project} libraries built with the debug runtime only (mixing the two corrupts the heap): install a release build there too (anvil install), or build this project in Debug")
      set({project}_FOUND FALSE)
    endif()
    unset(_{project}_build_type)
  endif()
  unset(_{project}_debug_map)
  unset(_{project}_release_map)
endif()
unset(_{project}_debug_runtime)
unset(_{project}_release_runtime)

"#
            ));
        }
        text.push_str("unset(_IMPORT_PREFIX)\n");

        text
    }

    /// `<project>Targets-<profile>.cmake`: this configuration's files of each library, and (with the MSVC ABI) which C
    /// runtime they use.
    fn cmake_locations(&self, model: &Model, project: &str) -> String {
        let toolchain = &model.toolchain;
        let mut text = format!(
            "# The {project} libraries built with profile {} ({} {}, {}), written by `anvil install`; {project}Config.cmake reads it.\n\n",
            model.profile.name,
            toolchain.family.name(),
            toolchain.version,
            toolchain.platform.name()
        );
        for block in &self.locations {
            text.push_str(block);
            text.push('\n');
        }
        if toolchain.is_msvc_abi() && !self.built_targets.is_empty() {
            let runtime = if model.profile.debug_runtime { "debug" } else { "release" };
            let configuration = configuration_name(&model.profile.name);
            text.push_str(&format!("list(APPEND _{project}_{runtime}_runtime {configuration})\n"));
        }

        text
    }

    /// `<project>-<library>.pc`, its paths relative to its own folder (`root` is "" or "debug/").
    fn pkg_config(&self, model: &Model, project: &str, pkg_config: &PkgConfig, root: &str) -> String {
        let up = if root.is_empty() { "../.." } else { "../../.." };
        let library = &pkg_config.library;
        let libs: Vec<String> = pkg_config
            .libs
            .iter()
            .map(|lib| lib.replace(&format!("${{prefix}}/{root}lib/"), "${libdir}/"))
            .collect();
        let requires = match pkg_config.requires.is_empty() {
            true => String::new(),
            false => format!("Requires: {}\n", pkg_config.requires.join(" ")),
        };

        format!(
            "# The {library} library of {project}, written by `anvil install`.\n\
             prefix=${{pcfiledir}}/{up}\nincludedir=${{prefix}}/include\nlibdir=${{prefix}}/{root}lib\n\n\
             Name: {project}-{library}\nDescription: The {library} library of {project}\nVersion: {}\n{requires}Cflags: {}\nLibs: {}\n",
            model.packages[0].version(),
            pkg_config.cflags.join(" "),
            libs.join(" ")
        )
    }

    /// `<project>.pc`: every library's pkg-config file.
    fn pkg_config_all(&self, model: &Model, project: &str) -> String {
        let package = &model.packages[0];
        let description = package
            .manifest
            .project
            .as_ref()
            .and_then(|project| project.description.clone())
            .unwrap_or_else(|| format!("The {project} libraries"));
        let requires: Vec<String> = self
            .pkg_configs
            .iter()
            .map(|pkg_config| format!("{project}-{}", pkg_config.library))
            .collect();

        format!(
            "# The {project} pkg-config file, written by `anvil install`: all its libraries ({project}-<library> for one).\n\
             Name: {project}\nDescription: {description}\nVersion: {}\nRequires: {}\n",
            package.version(),
            requires.join(" ")
        )
    }
}

/// `<project>ConfigVersion.cmake`: compatible with requests for the same major version, unsuitable for another
/// pointer size (a 32-bit build finding a 64-bit install).
fn cmake_version_file(project: &str, version: &str, cpu: Cpu) -> String {
    let major = version.split(['.', '-', '+']).next().unwrap_or(version);
    let pointer_size = match cpu {
        Cpu::X64 | Cpu::Arm64 => 8,
        Cpu::X86 | Cpu::Arm => 4,
    };

    format!(
        r#"# The version of the {project} CMake package, written by `anvil install`: requests for the same major version match.
set(PACKAGE_VERSION "{version}")

if("${{PACKAGE_FIND_VERSION}}" STREQUAL "")
  set(PACKAGE_VERSION_COMPATIBLE TRUE)
elseif(PACKAGE_VERSION VERSION_LESS PACKAGE_FIND_VERSION)
  set(PACKAGE_VERSION_COMPATIBLE FALSE)
elseif("${{PACKAGE_FIND_VERSION_MAJOR}}" STREQUAL "{major}")
  set(PACKAGE_VERSION_COMPATIBLE TRUE)
  if("${{PACKAGE_FIND_VERSION}}" STREQUAL "${{PACKAGE_VERSION}}")
    set(PACKAGE_VERSION_EXACT TRUE)
  endif()
else()
  set(PACKAGE_VERSION_COMPATIBLE FALSE)
endif()

if(NOT "${{CMAKE_SIZEOF_VOID_P}}" STREQUAL "" AND NOT "${{CMAKE_SIZEOF_VOID_P}}" STREQUAL "{pointer_size}")
  math(EXPR _installed_bits "{pointer_size} * 8")
  set(PACKAGE_VERSION "${{PACKAGE_VERSION}} (${{_installed_bits}}bit)")
  set(PACKAGE_VERSION_UNSUITABLE TRUE)
  unset(_installed_bits)
endif()
"#
    )
}

/// A profile's CMake configuration as imported-target properties name it: RELEASE, DEBUG, REL_FAST.
fn configuration_name(profile: &str) -> String {
    targets::config_name(profile)
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// `set_target_properties` of a target, one property per line; nothing without properties.
fn target_properties(name: &str, properties: &[(String, String)], indent: &str) -> String {
    if properties.is_empty() {
        return String::new();
    }

    let mut text = format!("{indent}set_target_properties({name} PROPERTIES\n");
    for (property, value) in properties {
        text.push_str(&format!("{indent}  {property} \"{value}\"\n"));
    }
    text.push_str(&format!("{indent})\n"));

    text
}

/// A shared library's file off Windows: `libz.so`, `libz.so.1`, `libz.dylib`.
fn is_shared_library(path: &Path) -> bool {
    let name = file_name(path);
    name.ends_with(".so") || name.contains(".so.") || name.ends_with(".dylib")
}

/// The folders that build a package's targets: those holding their sources, their private include folders, and the
/// package's tests/.
fn build_folders(model: &Model, package: usize) -> HashSet<String> {
    let mut folders = HashSet::new();
    for target in model.targets.iter().filter(|target| target.package == package) {
        folders.extend(target.sources.iter().filter_map(|source| source.parent()).map(util::path_key));
        folders.extend(
            target
                .private_include
                .iter()
                .chain(&target.system_include)
                .map(|folder| util::path_key(folder)),
        );
    }
    folders.insert(util::path_key(&model.packages[package].root.join("tests")));

    folders
}

/// C++ when a library has C++ sources, else C: what its static library needs linked in.
fn link_language(target: &Target) -> &'static str {
    let has_cxx = target.sources.iter().any(|source| {
        let extension = source
            .extension()
            .map(|extension| extension.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        CXX_EXTENSIONS.contains(&extension.as_str())
    });

    if has_cxx {
        "CXX"
    } else {
        "C"
    }
}

/// Text inside a CMake quoted argument, as one list item: `\`, `"`, `$` and `;` escaped.
fn cmake_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(character, '\\' | '"' | '$' | ';') {
            escaped.push('\\');
        }
        escaped.push(character);
    }

    escaped
}

/// A word of a pkg-config field: quoted when it holds a space or a quote.
fn pkg_config_word(word: &str) -> String {
    if !word.contains([' ', '\t', '"']) {
        return word.to_string();
    }
    format!("\"{}\"", word.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A library for pkg-config's Libs: a file as is, a system library's name as `-l<name>`, a flag as is.
fn pkg_config_lib(lib: &str) -> String {
    let is_file = lib.contains(['/', '\\']) || Path::new(lib).extension().is_some();
    if is_file || lib.starts_with('-') {
        pkg_config_word(lib)
    } else {
        format!("-l{lib}")
    }
}

fn push_once(list: &mut Vec<String>, item: String) {
    if !list.contains(&item) {
        list.push(item);
    }
}

fn dedup(list: &mut Vec<String>) {
    let mut seen = HashSet::new();
    list.retain(|item| seen.insert(item.clone()));
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().into_owned()
}

/// Two contents for one destination are the same file: one path, or equal bytes.
fn same_content(left: &Content, right: &Content) -> bool {
    match (left, right) {
        (Content::Copy(left), Content::Copy(right)) => {
            util::path_key(left) == util::path_key(right) || std::fs::read(left).ok().is_some_and(|bytes| std::fs::read(right).ok() == Some(bytes))
        }
        (Content::Text(left), Content::Text(right)) => left == right,
        _ => false,
    }
}

fn describe(content: &Content) -> String {
    match content {
        Content::Copy(source) => slash(source),
        Content::Text(_) => "a file Anvil writes".to_string(),
    }
}

/// Copies a file unless the destination holds the same bytes already (copying keeps a program's permissions).
fn copy_if_changed(source: &Path, destination: &Path) -> Result<bool> {
    let bytes = std::fs::read(source).with_context(|| format!("reading {}", slash(source)))?;
    if std::fs::read(destination).is_ok_and(|existing| existing == bytes) {
        return Ok(false);
    }

    if let Some(folder) = destination.parent() {
        std::fs::create_dir_all(folder).with_context(|| format!("creating {}", slash(folder)))?;
    }
    std::fs::copy(source, destination).with_context(|| format!("copying {} to {}", slash(source), slash(destination)))?;

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_cmake_and_pkg_config_values() {
        assert_eq!(cmake_escape(r#"DATA="C:/a b";X=$1"#), r#"DATA=\"C:/a b\"\;X=\$1"#);
        assert_eq!(pkg_config_word("-DNAME=\"a b\""), r#""-DNAME=\"a b\"""#);
        assert_eq!(pkg_config_lib("user32"), "-luser32");
        assert_eq!(pkg_config_lib("C:/vcpkg/lib/fmt.lib"), "C:/vcpkg/lib/fmt.lib");
        assert_eq!(pkg_config_lib("-pthread"), "-pthread");
    }

    #[test]
    fn version_file_matches_the_major_version() {
        let text = cmake_version_file("game", "2.4.1", Cpu::X64);
        assert!(text.contains("set(PACKAGE_VERSION \"2.4.1\")"));
        assert!(text.contains("\"${PACKAGE_FIND_VERSION_MAJOR}\" STREQUAL \"2\""));
        assert!(text.contains("NOT \"${CMAKE_SIZEOF_VOID_P}\" STREQUAL \"8\""));
        assert!(cmake_version_file("game", "3", Cpu::X86).contains("STREQUAL \"4\""));
    }

    #[test]
    fn names_configurations_and_shared_libraries() {
        assert_eq!(configuration_name("release"), "RELEASE");
        assert_eq!(configuration_name("relwithdebinfo"), "RELWITHDEBINFO");
        assert_eq!(configuration_name("rel-fast"), "REL_FAST");
        assert!(is_shared_library(Path::new("lib/libz.so.1.3")));
        assert!(is_shared_library(Path::new("libz.dylib")));
        assert!(!is_shared_library(Path::new("zlib1.dll")));
        assert!(!is_shared_library(Path::new("assets/song.sound")));
    }

    #[test]
    fn a_manifest_names_only_files_in_the_prefix() {
        let folder = std::env::temp_dir().join(format!("anvil-install-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let manifest = folder.join("install_manifest.txt");
        std::fs::write(&manifest, "include/a.h\n../outside.txt\n/etc/passwd\n\nbin/c.exe\n").unwrap();

        assert_eq!(read_manifest(&manifest), ["include/a.h", "bin/c.exe"]);
        assert!(read_manifest(&folder.join("missing.txt")).is_empty());
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
