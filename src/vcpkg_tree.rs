//! Reads a vcpkg install tree: the `status` database (which packages and features are installed, and what each
//! depends on) and the per-package file lists, to give each package's headers, libraries and DLLs without running
//! CMake.

use crate::util::slash;
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

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
}

/// What a project needs from one vcpkg dependency (and everything it depends on).
#[derive(Debug, Default, Clone)]
pub struct Usage {
    /// The triplet's include folder, then the subfolders packages export (include/vma, include/libpng16).
    pub includes: Vec<PathBuf>,
    /// Definitions the packages' libraries require of their users (FMT_SHARED for fmt's DLL).
    pub defines: Vec<String>,
    pub libs: Vec<PathBuf>,
    pub runtime: Vec<PathBuf>,
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

    /// Headers, libraries and DLLs of `name` and its dependencies, dependents before dependencies (link order).
    /// `debug` picks the debug libraries where the package has them. `only_libs` keeps libraries whose file stem
    /// matches one of the names.
    pub fn usage(&self, name: &str, debug: bool, only_libs: Option<&[String]>) -> Result<Usage> {
        if !self.contains(name) {
            bail!("vcpkg package `{name}` is not installed for {} (is it in the manifest?)", self.triplet);
        }

        let mut usage = Usage {
            includes: vec![self.root.join(&self.triplet).join("include")],
            ..Default::default()
        };

        // Imported targets users see: the package's own, then those they name publicly, as CMake propagates them
        let configuration = if debug { "Debug" } else { "Release" };
        let mut visible: HashSet<String> = HashSet::new();

        for package_name in self.closure(name) {
            // Host-only tools (vcpkg-cmake) are not installed for the target triplet
            let Some(package) = self.packages.get(&package_name) else { continue };
            let (mut libs, runtime) = self.package_binaries(package, debug);

            // The requested package: its library targets (all of them when it is header-only). Dependencies: the
            // targets something used names publicly
            let imported = self.imported_targets(package, configuration);
            let has_library = imported.iter().any(|target| target.has_library);
            let used: Vec<&ImportedTarget> = imported
                .iter()
                .filter(|target| {
                    if package_name == name {
                        target.has_library || !has_library
                    } else {
                        visible.contains(&target.name)
                    }
                })
                .collect();

            for target in used {
                for folder in &target.includes {
                    if !usage.includes.contains(folder) {
                        usage.includes.push(folder.clone());
                    }
                }
                for define in &target.defines {
                    if !usage.defines.contains(define) {
                        usage.defines.push(define.clone());
                    }
                }
                visible.extend(target.links.iter().cloned());
            }

            // The requested package's own libraries can be narrowed by name
            if package_name == name {
                if let Some(wanted) = only_libs {
                    libs.retain(|lib| wanted.iter().any(|wanted_name| lib_matches(lib, wanted_name)));
                }
            }

            usage.libs.extend(libs);
            usage.runtime.extend(runtime);
        }

        Ok(usage)
    }

    /// The imported targets a package's CMake files define, with what each gives its users for `configuration`.
    fn imported_targets(&self, package: &Package, configuration: &str) -> Vec<ImportedTarget> {
        let prefix = slash(&self.root.join(&self.triplet));
        let share = format!("{}/share/", self.triplet);
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

                for entry in property_values(&properties, "INTERFACE_INCLUDE_DIRECTORIES", configuration) {
                    let entry = expand_import_prefixes(&entry, &prefix, &list_dir);
                    if entry.contains("${") {
                        continue;
                    }
                    let folder = crate::util::normalize(Path::new(&entry));
                    if folder.is_dir() && !target.includes.contains(&folder) {
                        target.includes.push(folder);
                    }
                }
                for define in property_values(&properties, "INTERFACE_COMPILE_DEFINITIONS", configuration) {
                    if !define.contains("${") && !target.defines.contains(&define) {
                        target.defines.push(define);
                    }
                }
                target
                    .links
                    .extend(property_values(&properties, "INTERFACE_LINK_LIBRARIES", configuration));
            }
        }

        targets
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
            let Some((name, _version)) = stem.rsplit_once('_') else { continue };
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
/// `configuration` (ones that cannot be are left out).
fn property_values(properties: &str, property: &str, configuration: &str) -> Vec<String> {
    let found = properties
        .match_indices(property)
        .find(|(index, _)| properties[index + property.len()..].starts_with(char::is_whitespace));
    let Some((start, _)) = found else { return Vec::new() };
    let Some(value) = properties[start + property.len()..].split('"').nth(1) else {
        return Vec::new();
    };

    // Generator expressions written inside the file's own quoted strings come escaped (\$<...>)
    evaluate_list(&value.replace("\\$", "$"), configuration)
}

/// A ;-list whose items may be generator expressions: $<$<CONFIG:..>:..>, $<$<NOT:$<CONFIG:..>>:..>,
/// $<INSTALL_INTERFACE:..> and $<LINK_ONLY:..> (left out: it does not reach users' compiles).
fn evaluate_list(value: &str, configuration: &str) -> Vec<String> {
    let mut items = Vec::new();

    for item in split_top_level(value) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let Some(inner) = item.strip_prefix("$<").and_then(|inner| inner.strip_suffix('>')) else {
            items.push(item.to_string());
            continue;
        };

        // $<condition:value>, the condition itself a generator expression or a keyword
        let (condition, content) = split_condition(inner);
        let holds = match condition {
            "INSTALL_INTERFACE" => Some(true),
            "BUILD_INTERFACE" | "LINK_ONLY" => Some(false),
            _ => condition_holds(condition, configuration),
        };
        if holds == Some(true) {
            items.extend(evaluate_list(content, configuration));
        }
    }

    items
}

/// `$<CONFIG:A,B>`, `$<BOOL:ON>` or `$<NOT:...>` of them for a configuration; None for anything else.
fn condition_holds(condition: &str, configuration: &str) -> Option<bool> {
    let inner = condition.strip_prefix("$<")?.strip_suffix('>')?;
    if let Some(negated) = inner.strip_prefix("NOT:") {
        return condition_holds(negated, configuration).map(|holds| !holds);
    }
    if let Some(value) = inner.strip_prefix("BOOL:") {
        let upper = value.to_ascii_uppercase();
        let false_constant = matches!(upper.as_str(), "" | "0" | "OFF" | "NO" | "N" | "FALSE" | "IGNORE" | "NOTFOUND");
        return Some(!false_constant && !upper.ends_with("-NOTFOUND"));
    }

    let names = inner.strip_prefix("CONFIG:")?;
    Some(names.split(',').any(|name| name.eq_ignore_ascii_case(configuration)))
}

/// `condition:content` at the first colon outside nested generator expressions.
fn split_condition(text: &str) -> (&str, &str) {
    let mut depth = 0;
    let bytes = text.as_bytes();
    for (index, character) in text.char_indices() {
        match character {
            '<' if index > 0 && bytes[index - 1] == b'$' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            ':' if depth == 0 => return (&text[..index], &text[index + 1..]),
            _ => {}
        }
    }
    (text, "")
}

/// Splits at semicolons outside generator expressions.
fn split_top_level(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    let bytes = text.as_bytes();

    for (index, character) in text.char_indices() {
        match character {
            '<' if index > 0 && bytes[index - 1] == b'$' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            ';' if depth == 0 => {
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

    #[test]
    fn evaluates_exported_values() {
        let value = "FMT_SHARED;$<$<CONFIG:Debug>:DEBUG;_DEBUG>;$<$<NOT:$<CONFIG:Debug>>:NDEBUG>;API=__declspec(dllimport)";
        assert_eq!(
            evaluate_list(value, "Debug"),
            vec!["FMT_SHARED", "DEBUG", "_DEBUG", "API=__declspec(dllimport)"]
        );
        assert_eq!(evaluate_list(value, "Release"), vec!["FMT_SHARED", "NDEBUG", "API=__declspec(dllimport)"]);
        assert_eq!(evaluate_list("zlib::zlib;$<LINK_ONLY:brotli::dec>", "Debug"), vec!["zlib::zlib"]);
        assert_eq!(
            evaluate_list("$<$<NOT:$<BOOL:ON>>:A=0>;$<$<BOOL:OFF>:B=1>;$<$<BOOL:ON>:C>", "Debug"),
            vec!["C"]
        );

        let blocks = target_property_blocks("set_target_properties(a::b PROPERTIES\n  INTERFACE_COMPILE_DEFINITIONS \"X=f(1)\"\n)\nset_target_properties(a::c PROPERTIES IMPORTED_IMPLIB \"x.lib\")");
        assert_eq!(blocks.len(), 2);
        assert_eq!(property_values(&blocks[0].1, "INTERFACE_COMPILE_DEFINITIONS", "Debug"), vec!["X=f(1)"]);
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
        std::fs::write(info.join("sdl3_3.2_x64-windows.list"), "x64-windows/lib/\nx64-windows/lib/SDL3.lib\nx64-windows/lib/manual-link/SDL3main.lib\nx64-windows/debug/lib/SDL3.lib\nx64-windows/bin/SDL3.dll\n").unwrap();
        std::fs::write(info.join("volk_1.4_x64-windows.list"), "x64-windows/lib/volk.lib\n").unwrap();

        let tree = InstalledTree::load(&root, "x64-windows").unwrap();
        let usage = tree.usage("sdl3", false, None).unwrap();
        let names: Vec<String> = usage
            .libs
            .iter()
            .map(|lib| lib.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert_eq!(names, vec!["SDL3.lib", "SDL3main.lib", "volk.lib"]);
        assert_eq!(usage.runtime.len(), 1);
        assert!(tree.usage("missing", false, None).is_err());

        std::fs::remove_dir_all(&root).ok();
    }
}
