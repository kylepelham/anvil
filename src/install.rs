//! `anvil install`: builds, then copies what the project ships into a prefix (programs and DLLs to bin/, libraries to
//! lib/, public headers to include/), and writes the CMake package and pkg-config file other builds use its libraries
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

// What a public include folder that holds other files too (the package root, generated files) installs
const HEADER_EXTENSIONS: &[&str] = &["h", "hh", "hpp", "hxx", "h++", "inl", "inc", "ipp", "tpp"];

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

/// What users of the installed libraries get, gathered for the CMake package and the pkg-config file.
#[derive(Default)]
struct Export {
    /// `find_dependency` lines, each once.
    dependencies: Vec<String>,
    /// One `add_library` block per library.
    blocks: Vec<String>,

    // The pkg-config file's flags, each once: the libraries' files (users before what they use), then what they link
    cflags: Vec<String>,
    libs: Vec<String>,
    system_libs: Vec<String>,
}

struct Installer<'a> {
    model: &'a Model,
    plan: &'a Plan,

    // Every file once, by destination
    files: Vec<InstallFile>,
    destinations: HashMap<String, usize>,
}

/// `anvil install [targets]`: builds the targets to install, then copies their files into the prefix (default
/// `out/install`), each only when it changed.
pub fn install(start_dir: &Path, build: &BuildArgs, names: &[String], prefix: Option<&Path>) -> Result<()> {
    let model = commands::load_model(start_dir, build)?;
    let chosen = chosen_targets(&model, names)?;
    let installed = with_needed(&model, &chosen)?;

    let (model, plan) = commands::build_roots(model, build, &installed)?;
    let prefix = match prefix {
        Some(prefix) => util::absolute(&std::env::current_dir()?, prefix),
        None => model.out_root.join("install"),
    };

    // The targets' files, then the package files describing the libraries
    let mut installer = Installer {
        model: &model,
        plan: &plan,
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

    installer.write(&prefix)
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
            let folder = match target.kind {
                Kind::Exe => "bin",
                Kind::Shared if toolchain.targets_windows() => "bin",
                _ => "lib",
            };
            let location = format!("{folder}/{}", file_name(file));
            self.add(&location, Content::Copy(file.clone()), &owner)?;

            // The PDB the link wrote beside a program or DLL
            let pdb = file.with_extension("pdb");
            if folder == "bin" && toolchain.is_msvc_abi() && model.profile.debug && pdb.is_file() {
                self.add(&format!("bin/{}", file_name(&pdb)), Content::Copy(pdb), &owner)?;
            }

            // The import library users of a DLL link
            if let Some(import_library) = self.plan.import_libs.get(&index).filter(|_| target.kind == Kind::Shared) {
                let destination = format!("lib/{}", file_name(import_library));
                self.add(&destination, Content::Copy(import_library.clone()), &owner)?;
                library.import_library = Some(destination);
            }

            // The DLLs Anvil copies beside it (its dependencies', the sanitizer's), and its data files
            for runtime in self.plan.runtime_files.get(&index).into_iter().flatten() {
                self.add(&format!("bin/{}", file_name(runtime)), Content::Copy(runtime.clone()), &owner)?;
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
    /// build steps and export-header write too. Folders outside the package (an SDK's) stay where they are.
    fn add_headers(&mut self, index: usize, library: &mut InstalledLibrary) -> Result<()> {
        let model = self.model;
        let target = &model.targets[index];
        let package_root = &model.packages[target.package].root;
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

        let mut seen = HashSet::new();
        for folder in folders.into_iter().filter(|folder| seen.insert(util::path_key(folder))) {
            let generated = util::strip_prefix_ci(&folder, &model.out_root).is_some();
            if !generated && util::strip_prefix_ci(&folder, package_root).is_none() {
                library.external_includes.push(folder);
                continue;
            }

            // Every file but sources, or only headers from a folder that holds the rest of the package too
            let key = util::path_key(&folder);
            let headers_only = generated || key == util::path_key(package_root) || key == util::path_key(&target.dir);
            let walker = walkdir::WalkDir::new(&folder)
                .sort_by_file_name()
                .into_iter()
                .filter_entry(|entry| !targets::is_skipped_dir(entry, &model.out_root));
            for entry in walker.flatten().filter(|entry| entry.file_type().is_file()) {
                let extension = entry
                    .path()
                    .extension()
                    .map(|extension| extension.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                let wanted = match headers_only {
                    true => HEADER_EXTENSIONS.contains(&extension.as_str()),
                    false => !util::is_source_ext(&extension),
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

        // Users before the libraries they use, the order a GNU link of the pkg-config file's libraries needs
        let mut export = Export::default();
        for index in model.closure(installed).into_iter().rev() {
            if let Some(library) = libraries.get(&index) {
                export.add_library(model, &project, index, library, vcpkg_tree.as_ref());
            }
        }

        let owner = "the CMake package";
        let folder = format!("lib/cmake/{project}");
        self.add(
            &format!("{folder}/{project}Config.cmake"),
            Content::Text(export.cmake_config(model, &project)),
            owner,
        )?;
        if let Some(version) = package.manifest.project.as_ref().and_then(|project| project.version.as_deref()) {
            let text = cmake_version_file(&project, version, model.toolchain.platform.cpu);
            self.add(&format!("{folder}/{project}ConfigVersion.cmake"), Content::Text(text), owner)?;
        }
        self.add(
            &format!("lib/pkgconfig/{project}.pc"),
            Content::Text(export.pkg_config(model, &project)),
            "the pkg-config file",
        )?;

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

    /// Writes every file that is missing from the prefix or differs, then says how many.
    fn write(&self, prefix: &Path) -> Result<()> {
        let mut copied = 0;
        for file in &self.files {
            let destination = prefix.join(&file.destination);
            let changed = match &file.content {
                Content::Copy(source) => copy_if_changed(source, &destination)?,
                Content::Text(text) => util::write_if_changed(&destination, text.as_bytes())?,
            };
            copied += changed as usize;
        }

        let shown = util::rel(prefix, &std::env::current_dir()?);
        let unchanged = self.files.len() - copied;
        ui::status("Installed", &format!("{shown}: {copied} files copied, {unchanged} up to date"));

        Ok(())
    }
}

impl Export {
    /// One library's CMake target, and its part of the pkg-config file.
    fn add_library(&mut self, model: &Model, project: &str, index: usize, library: &InstalledLibrary, vcpkg_tree: Option<&InstalledTree>) {
        let target = &model.targets[index];
        let name = format!("{project}::{}", target.name);
        let mut notes = Vec::new();

        // Its own usage: headers, definitions, flags
        let mut includes = Vec::new();
        if library.has_headers {
            includes.push("${_IMPORT_PREFIX}/include".to_string());
            push_once(&mut self.cflags, "-I${includedir}".to_string());
        }
        for folder in &library.external_includes {
            includes.push(cmake_escape(&slash(folder)));
            push_once(&mut self.cflags, pkg_config_word(&format!("-I{}", slash(folder))));
        }
        let defines: Vec<&String> = target.public_defines.iter().chain(&target.interface_defines).collect();
        let options: Vec<&String> = target.public_flags.iter().chain(&target.interface_flags).collect();
        for define in &defines {
            push_once(&mut self.cflags, pkg_config_word(&format!("-D{define}")));
        }
        for option in &options {
            push_once(&mut self.cflags, pkg_config_word(option));
        }

        // Its file
        let linked_file = library.import_library.as_ref().or(library.location.as_ref());
        if let Some(file) = linked_file {
            push_once(&mut self.libs, format!("${{prefix}}/{file}"));
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
                Kind::Static | Kind::Shared | Kind::Headers => links.push(item(format!("{project}::{}", dependency.name))),
                Kind::External => {
                    // A vcpkg port CMake can find: its package and targets
                    let port = dependency.vcpkg.as_ref().map(|(port, _)| port.as_str());
                    let package = port.and_then(|port| vcpkg_tree.and_then(|tree| tree.cmake_package(port)));
                    if let Some(package) = package {
                        push_once(&mut self.dependencies, find_dependency_line(&package));
                        links.extend(package.targets.iter().map(|target| item(target.clone())));
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
                        push_once(&mut self.system_libs, pkg_config_lib(lib));
                    }
                    if *public {
                        for folder in &dependency.public_system_include {
                            push_once(&mut self.cflags, pkg_config_word(&format!("-I{}", slash(folder))));
                        }
                        for define in &dependency.public_defines {
                            push_once(&mut self.cflags, pkg_config_word(&format!("-D{define}")));
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
                push_once(&mut self.system_libs, pkg_config_lib(lib));
            }
        }

        // The block
        let kind = match target.kind {
            Kind::Static => "STATIC",
            Kind::Shared => "SHARED",
            _ => "INTERFACE",
        };
        let mut properties = Vec::new();
        if let Some(location) = &library.location {
            properties.push(("IMPORTED_LOCATION", format!("${{_IMPORT_PREFIX}}/{location}")));
        }
        if let Some(import_library) = &library.import_library {
            properties.push(("IMPORTED_IMPLIB", format!("${{_IMPORT_PREFIX}}/{import_library}")));
        }
        if target.kind == Kind::Static {
            properties.push(("IMPORTED_LINK_INTERFACE_LANGUAGES", link_language(target).to_string()));
        }
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
        for (property, mut values) in lists {
            dedup(&mut values);
            if !values.is_empty() {
                properties.push((property, values.join(";")));
            }
        }

        let mut block = String::new();
        for note in notes {
            block.push_str(&format!("# {note}\n"));
        }
        block.push_str(&format!("if(NOT TARGET {name})\n  add_library({name} {kind} IMPORTED)\n"));
        if !properties.is_empty() {
            block.push_str(&format!("  set_target_properties({name} PROPERTIES\n"));
            for (property, value) in properties {
                block.push_str(&format!("    {property} \"{value}\"\n"));
            }
            block.push_str("  )\n");
        }
        block.push_str("endif()\n");
        self.blocks.push(block);
    }

    /// `<project>Config.cmake`: the dependencies found, then one imported target per library.
    fn cmake_config(&self, model: &Model, project: &str) -> String {
        let toolchain = &model.toolchain;
        let mut text = format!(
            "# The {project} CMake package, written by `anvil install` (profile {}, {} {}).\n\
             # Use: find_package({project} CONFIG REQUIRED), then target_link_libraries(app PRIVATE {project}::<library>).\n\n\
             if(CMAKE_VERSION VERSION_LESS 3.13)\n  message(FATAL_ERROR \"{project} needs CMake 3.13 or newer\")\nendif()\n\n",
            model.profile.name,
            toolchain.family.name(),
            toolchain.version
        );

        // Dependencies, also from the vcpkg packages the build used when the user's paths do not find them
        if !self.dependencies.is_empty() {
            text.push_str("include(CMakeFindDependencyMacro)\n");
            text.push_str(&format!(
                "set(_anvil_prefix_path \"${{CMAKE_PREFIX_PATH}}\")\nlist(APPEND CMAKE_PREFIX_PATH \"{}\")\n",
                cmake_escape(&slash(&model.vcpkg_dir))
            ));
            for line in &self.dependencies {
                text.push_str(&format!("{line}\n"));
            }
            text.push_str("set(CMAKE_PREFIX_PATH \"${_anvil_prefix_path}\")\nunset(_anvil_prefix_path)\n\n");
        }

        // The prefix: this file is in <prefix>/lib/cmake/<project>
        text.push_str("get_filename_component(_IMPORT_PREFIX \"${CMAKE_CURRENT_LIST_DIR}/../../..\" ABSOLUTE)\n\n");
        for block in &self.blocks {
            text.push_str(block);
            text.push('\n');
        }
        text.push_str("unset(_IMPORT_PREFIX)\n");

        text
    }

    /// `<project>.pc`, its paths relative to its own folder.
    fn pkg_config(&self, model: &Model, project: &str) -> String {
        let package = &model.packages[0];
        let description = package
            .manifest
            .project
            .as_ref()
            .and_then(|project| project.description.clone())
            .unwrap_or_else(|| format!("The {project} libraries"));

        let mut libs: Vec<String> = self.libs.iter().map(|lib| lib.replace("${prefix}/lib/", "${libdir}/")).collect();
        libs.extend(self.system_libs.iter().filter(|lib| !self.libs.contains(lib)).cloned());

        format!(
            "# The {project} pkg-config file, written by `anvil install`.\n\
             prefix=${{pcfiledir}}/../..\nincludedir=${{prefix}}/include\nlibdir=${{prefix}}/lib\n\n\
             Name: {project}\nDescription: {description}\nVersion: {}\nCflags: {}\nLibs: {}\n",
            package.version(),
            self.cflags.join(" "),
            libs.join(" ")
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

if(PACKAGE_FIND_VERSION STREQUAL "")
  set(PACKAGE_VERSION_COMPATIBLE TRUE)
elseif(PACKAGE_VERSION VERSION_LESS PACKAGE_FIND_VERSION)
  set(PACKAGE_VERSION_COMPATIBLE FALSE)
elseif(PACKAGE_FIND_VERSION_MAJOR STREQUAL "{major}")
  set(PACKAGE_VERSION_COMPATIBLE TRUE)
  if(PACKAGE_FIND_VERSION STREQUAL PACKAGE_VERSION)
    set(PACKAGE_VERSION_EXACT TRUE)
  endif()
else()
  set(PACKAGE_VERSION_COMPATIBLE FALSE)
endif()

if(NOT CMAKE_SIZEOF_VOID_P STREQUAL "" AND NOT CMAKE_SIZEOF_VOID_P STREQUAL "{pointer_size}")
  math(EXPR _installed_bits "{pointer_size} * 8")
  set(PACKAGE_VERSION "${{PACKAGE_VERSION}} (${{_installed_bits}}bit)")
  set(PACKAGE_VERSION_UNSUITABLE TRUE)
  unset(_installed_bits)
endif()
"#
    )
}

/// `find_dependency(SDL3 CONFIG)`, or without CONFIG when CMake's own Find module finds it too (ZLIB).
fn find_dependency_line(package: &CmakePackage) -> String {
    match package.config {
        true => format!("find_dependency({} CONFIG)", package.name),
        false => format!("find_dependency({})", package.name),
    }
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
        assert!(text.contains("PACKAGE_FIND_VERSION_MAJOR STREQUAL \"2\""));
        assert!(text.contains("STREQUAL \"8\""));
        assert!(cmake_version_file("game", "3", Cpu::X86).contains("STREQUAL \"4\""));
    }
}
