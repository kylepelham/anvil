//! Creating and editing projects: `anvil new`, `anvil init`, `anvil add`, `anvil remove`.

use crate::manifest::VcpkgSpec;
use crate::project::{self, MANIFEST_NAME};
use crate::targets::{self, Kind};
use crate::ui;
use crate::util::{self, slash};
use crate::vcpkg::{self, Instance};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

// Folders `anvil init` does not take sources from: tests, samples, vendored code, build output
const NOT_SOURCE_FOLDERS: &[&str] = &[
    "tests",
    "test",
    "examples",
    "example",
    "samples",
    "sample",
    "benchmarks",
    "bench",
    "docs",
    "doc",
    "third_party",
    "thirdparty",
    "external",
    "extern",
    "vendor",
    "deps",
    "build",
    "out",
    "cmake",
];

#[derive(clap::Args)]
pub struct AddArgs {
    /// The dependency's name (the vcpkg port, unless another source is given).
    pub name: String,

    /// A local folder (an Anvil project, or headers/sources).
    #[arg(long, value_name = "FOLDER", group = "source")]
    pub path: Option<String>,

    /// A git repository.
    #[arg(long, value_name = "URL", group = "source")]
    pub git: Option<String>,

    /// The git tag to check out.
    #[arg(long, requires = "git", conflicts_with_all = ["branch", "rev"])]
    pub tag: Option<String>,

    /// The git branch to follow (anvil.lock pins its commit).
    #[arg(long, requires = "git", conflicts_with = "rev")]
    pub branch: Option<String>,

    /// The git commit to check out.
    #[arg(long, requires = "git")]
    pub rev: Option<String>,

    /// Found through pkg-config.
    #[arg(long, group = "source")]
    pub pkg_config: bool,

    /// A system library (user32, pthread, ...).
    #[arg(long, group = "source")]
    pub system: bool,

    /// vcpkg features, comma separated.
    #[arg(long, value_delimiter = ',', conflicts_with_all = ["path", "git", "pkg_config", "system"])]
    pub features: Vec<String>,

    /// Targets that use it (adds it to their deps).
    #[arg(long, value_name = "TARGET")]
    pub to: Vec<String>,

    /// A dependency of the tests only ([dev-deps]).
    #[arg(long)]
    pub dev: bool,
}

/// `anvil new`; `shell_dir` is the folder Anvil was started in (before -C), which the hint to `cd` is relative to.
pub fn new_project(start_dir: &Path, path: &str, library: bool, shell_dir: &Path) -> Result<()> {
    let folder = util::absolute(&std::env::current_dir()?, &start_dir.join(path));
    if folder.exists() && std::fs::read_dir(&folder)?.next().is_some() {
        bail!("{} already exists and is not empty", slash(&folder));
    }

    // Named after its folder (`anvil new tools/gen` makes `gen`)
    let name = folder
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .with_context(|| format!("`{path}` does not name a folder"))?;
    let identifier = targets::sanitize_name(&name).replace(['-', '.'], "_");
    write_new(&folder.join(MANIFEST_NAME), &project_document(&name).to_string())?;
    write_new(&folder.join(".gitignore"), &gitignore(&name))?;

    if library {
        let header = format!("{}#pragma once\n\n#include <string>\n\nnamespace {identifier} {{\n\nstd::string greeting(const std::string& name);\n\n}} // namespace {identifier}\n", header_block(&format!("{identifier}.h"), &format!("The {name} library.")));
        write_new(&folder.join("include").join(&identifier).join(format!("{identifier}.h")), &header)?;

        let source = format!("#include \"{identifier}/{identifier}.h\"\n\nnamespace {identifier} {{\n\nstd::string greeting(const std::string& name)\n{{\n    return \"Hello, \" + name + \"!\";\n}}\n\n}} // namespace {identifier}\n");
        write_new(&folder.join("src").join(format!("{identifier}.cpp")), &source)?;

        let test = format!("#include \"{identifier}/{identifier}.h\"\n\n#include <cstdio>\n\nint main()\n{{\n    // A test passes by returning 0\n    if ({identifier}::greeting(\"Anvil\") != \"Hello, Anvil!\") {{\n        std::puts(\"greeting is wrong\");\n        return 1;\n    }}\n\n    return 0;\n}}\n");
        write_new(&folder.join("tests").join(format!("test_{identifier}.cpp")), &test)?;
    } else {
        let main = "#include <iostream>\n\nint main()\n{\n    std::cout << \"Hello from Anvil!\\n\";\n\n    return 0;\n}\n";
        write_new(&folder.join("src").join("main.cpp"), main)?;
    }

    let kind = if library { "library" } else { "program" };
    ui::status("Created", &format!("{kind} `{name}` in {}", slash(&folder)));
    let next = if library { "anvil test" } else { "anvil run" };
    let mut relative = util::rel(&folder, shell_dir);
    if relative.contains(' ') {
        relative = format!("\"{relative}\"");
    }
    ui::note(&format!("next: cd {relative} && {next}"));

    Ok(())
}

/// What `anvil new` and `anvil init` keep out of git: the build output, and what is written for this machine (the
/// compilation database, `anvil ide`'s files, Visual Studio's solution and its settings).
fn gitignore(name: &str) -> String {
    format!("/out/\n/compile_commands.json\n/.anvil/\n/.vs/\n/{name}.sln\n")
}

pub fn init(start_dir: &Path) -> Result<()> {
    let folder = util::absolute(&std::env::current_dir()?, start_dir);
    let manifest_path = folder.join(MANIFEST_NAME);
    if manifest_path.exists() {
        bail!("{} already exists", slash(&manifest_path));
    }

    // A CMake project converts
    if folder.join("CMakeLists.txt").is_file() {
        ui::note("found CMakeLists.txt: converting it");
        return crate::commands::convert(&folder, None, false, false);
    }

    let name = folder
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "app".into());
    let mut document = project_document(&name);

    // Sources the conventions find (in src/, or files here), else targets over the folders that hold some
    let conventional = has_sources_in(&folder.join("src"), 8) || has_sources_in(&folder, 1);
    if !conventional {
        let source_folders = source_folders(&folder);
        if source_folders.is_empty() {
            bail!(
                "no C or C++ sources in {}: put them under src/ (a main file there makes a program), or start with `anvil new <name>`",
                slash(&folder)
            );
        }
        document["target"] = toml_edit::table();
        document["target"].as_table_mut().unwrap().set_implicit(true);
        for (target_name, table) in init_targets(&folder, &name, &source_folders)? {
            document["target"][target_name.as_str()] = table;
        }
    }

    std::fs::write(&manifest_path, document.to_string()).with_context(|| format!("writing {}", slash(&manifest_path)))?;
    ui::status("Created", &slash(&manifest_path));
    if !folder.join(".gitignore").exists() {
        std::fs::write(folder.join(".gitignore"), gitignore(&name))?;
    }

    // Say what the conventions found
    let model = crate::commands::load_model_for_listing(&folder, &crate::BuildArgs::default())
        .with_context(|| format!("{} is written, but the project does not load yet", slash(&manifest_path)))?;
    for target in model.targets.iter().filter(|target| target.kind != Kind::External) {
        ui::note(&format!(
            "{} `{}` from {} source files",
            target.kind.name(),
            target.name,
            target.sources.len()
        ));
    }

    Ok(())
}

pub fn add(start_dir: &Path, args: &AddArgs) -> Result<()> {
    // The package the user is in gets the entry; vcpkg settings and vcpkg.json are the project root's
    let (root, manifest) = project::locate(start_dir)?;
    let manifest_path = package_manifest_path(start_dir, &root)?;
    let text = match std::fs::read_to_string(&manifest_path) {
        Ok(text) => text,
        Err(_) => project_document(
            &root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "app".into()),
        )
        .to_string(),
    };
    let mut document: toml_edit::DocumentMut = text.parse().with_context(|| format!("parsing {}", slash(&manifest_path)))?;
    let vcpkg_spec = manifest.vcpkg.clone().unwrap_or_default();

    // Where it goes and who uses it, checked before anything is written
    let (table_name, other_table) = if args.dev { ("dev-deps", "deps") } else { ("deps", "dev-deps") };
    let in_other = document
        .get(other_table)
        .and_then(|item| item.as_table_like())
        .is_some_and(|table| table.contains_key(&args.name));
    if in_other {
        bail!(
            "`{}` is already under [{other_table}]; a dependency is in one of [deps] and [dev-deps] (`anvil remove {}` first to move it)",
            args.name,
            args.name
        );
    }
    let users = choose_users(&document, args)?;

    // The entry: a new one, or features added to the one there
    let deps = document
        .entry(table_name)
        .or_insert(toml_edit::table())
        .as_table_like_mut()
        .with_context(|| format!("[{table_name}] is not a table"))?;
    let added = !deps.contains_key(&args.name);
    let mut changed = added;
    match deps.get_mut(&args.name) {
        Some(existing) => {
            let port = vcpkg_port_of(existing, &args.name);
            if port.is_none() && !args.features.is_empty() {
                bail!("`{}` in [{table_name}] is not a vcpkg port; features belong to vcpkg ports", args.name);
            }

            // A port the project's vcpkg.json lacks, or lacks features of, gets them there (the entry's and the new ones)
            if let Some(port) = port {
                let mut features = listed_features(existing);
                for feature in &args.features {
                    if !features.contains(feature) {
                        features.push(feature.clone());
                    }
                }
                add_to_vcpkg_json(&root, &vcpkg_spec, &port, &features)?;
            }
            if !args.features.is_empty() {
                changed = add_features(existing, &args.name, &args.features)?;
            }
        }
        None => {
            deps.insert(&args.name, dependency_entry(args, &root, &vcpkg_spec)?);
        }
    }

    // Targets that use it
    let has_targets = document
        .get("target")
        .and_then(|item| item.as_table())
        .is_some_and(|table| !table.is_empty());
    for user in &users {
        let target = document["target"][user.as_str()]
            .as_table_like_mut()
            .with_context(|| format!("[target.{user}] is not a table"))?;
        let deps = target.entry("deps").or_insert(toml_edit::value(toml_edit::Array::new()));
        let list = deps.as_array_mut().with_context(|| format!("[target.{user}] deps is not a list"))?;
        if !list.iter().any(|dep| dep.as_str() == Some(args.name.as_str())) {
            list.push(args.name.as_str());
            changed = true;
        }
    }

    if !changed {
        ui::note(&format!("`{}` is already in [{table_name}]", args.name));
        return Ok(());
    }
    std::fs::write(&manifest_path, document.to_string()).with_context(|| format!("writing {}", slash(&manifest_path)))?;
    if added {
        ui::status("Added", &format!("{} to {}", args.name, slash(&manifest_path)));
    } else {
        ui::status("Updated", &format!("{} in {}", args.name, slash(&manifest_path)));
    }

    if has_targets && users.is_empty() {
        ui::note(&format!(
            "add \"{}\" to the deps of the targets that use it (or run again with --to <target>)",
            args.name
        ));
    }

    Ok(())
}

/// `anvil remove <name>`: the [deps] or [dev-deps] entry, the targets' uses of it, and its port in the project's
/// vcpkg.json when no other entry needs it.
pub fn remove(start_dir: &Path, name: &str) -> Result<()> {
    // The package the user is in loses the entry; vcpkg settings and vcpkg.json are the project root's
    let (root, workspace_manifest) = project::locate(start_dir)?;
    let manifest_path = package_manifest_path(start_dir, &root)?;
    let text = std::fs::read_to_string(&manifest_path).with_context(|| format!("reading {}", slash(&manifest_path)))?;
    let manifest = crate::manifest::parse_str(&text).with_context(|| format!("in {}", slash(&manifest_path)))?;
    let mut document: toml_edit::DocumentMut = text.parse().with_context(|| format!("parsing {}", slash(&manifest_path)))?;

    // The entry
    let mut removed = None;
    for table_name in ["deps", "dev-deps"] {
        let table = document.get_mut(table_name).and_then(|item| item.as_table_like_mut());
        if let Some(item) = table.and_then(|table| table.remove(name)) {
            removed = Some((table_name, vcpkg_port_of(&item, name)));
        }
    }
    let Some((table_name, port)) = removed else {
        let declared: Vec<&String> = manifest.deps.keys().chain(manifest.dev_deps.keys()).collect();
        let hint = util::did_you_mean(name, declared.iter().map(|name| name.as_str()));
        bail!("`{name}`{hint} is not in [deps] or [dev-deps] of {}", slash(&manifest_path));
    };

    // Its uses: targets (and their when blocks), [defaults] and presets
    let mut users = Vec::new();
    for section in ["target", "preset"] {
        let Some(tables) = document.get_mut(section).and_then(|item| item.as_table_like_mut()) else {
            continue;
        };
        for (table_key, table) in tables.iter_mut() {
            if remove_uses(table, name) {
                users.push(format!("[{section}.{}]", table_key.get()));
            }
        }
    }
    if let Some(defaults) = document.get_mut("defaults") {
        if remove_uses(defaults, name) {
            users.push("[defaults]".to_string());
        }
    }

    std::fs::write(&manifest_path, document.to_string()).with_context(|| format!("writing {}", slash(&manifest_path)))?;
    ui::status("Removed", &format!("{name} from [{table_name}] in {}", slash(&manifest_path)));
    if !users.is_empty() {
        ui::note(&format!("and from the deps of {}", users.join(", ")));
    }

    // The port in vcpkg.json, unless another entry (of this package or another of the workspace) still uses it
    let Some(port) = port else { return Ok(()) };
    let mut manifests = vec![crate::manifest::parse_str(&document.to_string())?];
    let mut packages = vec![root.clone()];
    packages.extend(project::workspace_members(&root, &workspace_manifest)?);
    for folder in packages {
        let file = folder.join(MANIFEST_NAME);
        if file.is_file() && util::path_key(&file) != util::path_key(&manifest_path) {
            manifests.push(crate::manifest::load(&file)?);
        }
    }
    let still_used = manifests.iter().any(|other| {
        other
            .deps
            .iter()
            .chain(&other.dev_deps)
            .any(|(other, value)| crate::manifest::parse_dep(other, value).ok().and_then(|spec| spec.vcpkg).as_deref() == Some(port.as_str()))
    });
    if !still_used {
        remove_from_vcpkg_json(&root, &workspace_manifest.vcpkg.clone().unwrap_or_default(), &port)?;
    }

    Ok(())
}

/// The anvil.toml of the package the user is in: the nearest one up from `start_dir` (a workspace member's own, not
/// the workspace's), else the project root's (which may not exist yet).
fn package_manifest_path(start_dir: &Path, project_root: &Path) -> Result<PathBuf> {
    let start = util::absolute(&std::env::current_dir()?, start_dir);
    let nearest = start.ancestors().find(|folder| folder.join(MANIFEST_NAME).is_file());

    Ok(nearest.unwrap_or(project_root).join(MANIFEST_NAME))
}

/// A new project's anvil.toml: `[project]` with its name, a version and the default standard.
fn project_document(name: &str) -> toml_edit::DocumentMut {
    let mut project = toml_edit::Table::new();
    project.insert("name", toml_edit::value(name));
    project.insert("version", toml_edit::value("0.1.0"));
    project.insert("std", toml_edit::value("c++20"));

    let mut document = toml_edit::DocumentMut::new();
    document.insert("project", toml_edit::Item::Table(project));
    document
}

/// `[target]` tables over folders of sources the conventions do not find: a program per folder with a main file
/// (named after the project when there is one, else after its folder), and a library of the other folders, which the
/// programs use. Header-only `include/` and `src/` folders are the library's (or the programs') include folders.
fn init_targets(root: &Path, name: &str, folders: &[String]) -> Result<Vec<(String, toml_edit::Item)>> {
    // Folders with a main file; two in one folder would be two programs in one target
    let mut program_folders = Vec::new();
    for folder in folders {
        let mains: Vec<String> = walkdir::WalkDir::new(root.join(folder))
            .sort_by_file_name()
            .into_iter()
            .flatten()
            .filter(|entry| {
                entry.file_type().is_file() && is_source_file(entry.path()) && entry.path().file_stem().is_some_and(|stem| stem == "main")
            })
            .map(|entry| util::rel(entry.path(), root))
            .collect();
        if mains.len() > 1 {
            bail!(
                "{folder}/ holds several main files ({}), which `anvil init` cannot split into programs: write [target] tables for them (kind = \"exe\", sources = [...]), or start with `anvil new <name>`",
                mains.join(", ")
            );
        }
        if mains.len() == 1 {
            program_folders.push(folder.clone());
        }
    }
    let library_folders: Vec<&String> = folders.iter().filter(|folder| !program_folders.contains(folder)).collect();
    let header_folders: Vec<&str> = ["include", "src"]
        .into_iter()
        .filter(|folder| root.join(folder).is_dir() && !has_sources_in(&root.join(folder), 8))
        .collect();
    let globs = |folders: &mut dyn Iterator<Item = &String>| -> toml_edit::Array {
        folders.map(|folder| format!("{folder}/{}", targets::SOURCE_GLOB)).collect()
    };

    // The programs: named after the project when there is one
    let mut programs = Vec::new();
    for folder in &program_folders {
        let program_name = if program_folders.len() == 1 {
            name.to_string()
        } else {
            targets::sanitize_name(folder)
        };
        let mut include: toml_edit::Array = std::iter::once(folder.as_str()).collect();
        if library_folders.is_empty() {
            include.extend(header_folders.iter().copied());
        }

        let mut table = toml_edit::Table::new();
        table.insert("kind", toml_edit::value("exe"));
        table.insert("sources", toml_edit::value(globs(&mut std::iter::once(folder))));
        table.insert("include", toml_edit::value(include));
        programs.push((program_name, table));
    }

    // The library of the rest, named after the project unless a program is
    let mut targets = Vec::new();
    if !library_folders.is_empty() {
        let library_name = if programs.iter().any(|(program, _)| program == name) {
            format!("{name}_lib")
        } else {
            name.to_string()
        };
        let mut public = toml_edit::InlineTable::new();
        let public_include: toml_edit::Array = library_folders
            .iter()
            .map(|folder| folder.as_str())
            .chain(header_folders.iter().copied())
            .collect();
        public.insert("include", public_include.into());

        let mut table = toml_edit::Table::new();
        table.insert("kind", toml_edit::value("lib"));
        table.insert("sources", toml_edit::value(globs(&mut library_folders.iter().copied())));
        table.insert("public", toml_edit::value(public));
        for (_, program) in &mut programs {
            program.insert(
                "deps",
                toml_edit::value(std::iter::once(library_name.as_str()).collect::<toml_edit::Array>()),
            );
        }
        targets.push((library_name, toml_edit::Item::Table(table)));
    }
    targets.extend(programs.into_iter().map(|(program, table)| (program, toml_edit::Item::Table(table))));

    Ok(targets)
}

/// Folders under `root` holding C or C++ sources, outside tests, samples, vendored code and build output.
fn source_folders(root: &Path) -> Vec<String> {
    let mut folders = Vec::new();

    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let skipped = name.starts_with('.') || NOT_SOURCE_FOLDERS.contains(&name.to_ascii_lowercase().as_str());
        if entry.path().is_dir() && !skipped && has_sources_in(&entry.path(), 8) {
            folders.push(name);
        }
    }

    folders.sort();
    folders
}

/// C or C++ files in `folder`, `depth` levels deep.
fn has_sources_in(folder: &Path, depth: usize) -> bool {
    walkdir::WalkDir::new(folder)
        .max_depth(depth)
        .into_iter()
        .flatten()
        .any(|entry| entry.file_type().is_file() && is_source_file(entry.path()))
}

/// A C or C++ source file (not a header).
fn is_source_file(path: &Path) -> bool {
    let extension = path.extension().map(|extension| extension.to_string_lossy().to_ascii_lowercase());
    matches!(extension.as_deref(), Some("c" | "cc" | "cpp" | "cxx" | "c++"))
}

/// The [deps] value for the arguments, checking the vcpkg port exists.
fn dependency_entry(args: &AddArgs, root: &Path, vcpkg_spec: &VcpkgSpec) -> Result<toml_edit::Item> {
    let mut table = toml_edit::InlineTable::new();

    if let Some(path) = &args.path {
        table.insert("path", path.as_str().into());
    } else if let Some(url) = &args.git {
        table.insert("git", url.as_str().into());
        for (key, value) in [("tag", &args.tag), ("branch", &args.branch), ("rev", &args.rev)] {
            if let Some(value) = value {
                table.insert(key, value.as_str().into());
            }
        }
    } else if args.pkg_config {
        return Ok(toml_edit::value("pkg-config"));
    } else if args.system {
        return Ok(toml_edit::value("system"));
    } else {
        check_vcpkg_port(root, vcpkg_spec, &args.name)?;
        add_to_vcpkg_json(root, vcpkg_spec, &args.name, &args.features)?;

        if args.features.is_empty() {
            return Ok(toml_edit::value("vcpkg"));
        }
        table.insert("vcpkg", args.name.as_str().into());
        let mut features = toml_edit::Array::new();
        for feature in &args.features {
            features.push(feature.as_str());
        }
        table.insert("features", features.into());
    }

    Ok(toml_edit::value(table))
}

/// The vcpkg port a [deps] entry names, if it is one.
fn vcpkg_port_of(item: &toml_edit::Item, name: &str) -> Option<String> {
    if item.as_str() == Some("vcpkg") {
        return Some(name.to_string());
    }
    item.as_table_like()?.get("vcpkg")?.as_str().map(|port| port.to_string())
}

/// The features a [deps] entry lists.
fn listed_features(item: &toml_edit::Item) -> Vec<String> {
    let listed = item
        .as_table_like()
        .and_then(|table| table.get("features"))
        .and_then(|features| features.as_array());

    listed
        .into_iter()
        .flatten()
        .filter_map(|feature| feature.as_str().map(str::to_string))
        .collect()
}

/// Adds features to an existing vcpkg entry (the short form becomes a table); says whether anything changed.
fn add_features(item: &mut toml_edit::Item, name: &str, features: &[String]) -> Result<bool> {
    if item.as_str() == Some("vcpkg") {
        let mut table = toml_edit::InlineTable::new();
        table.insert("vcpkg", name.into());
        *item = toml_edit::value(table);
    }

    let table = item.as_table_like_mut().with_context(|| format!("`{name}` is not a table"))?;
    let list = table
        .entry("features")
        .or_insert(toml_edit::value(toml_edit::Array::new()))
        .as_array_mut()
        .with_context(|| format!("`{name}`: features is not a list"))?;

    let mut changed = false;
    for feature in features {
        if !list.iter().any(|listed| listed.as_str() == Some(feature.as_str())) {
            list.push(feature.as_str());
            changed = true;
        }
    }

    Ok(changed)
}

/// Takes `name` and its `name/<target>` uses out of the dependency lists of a target-like table, its `public` table
/// and its when blocks; lists, tables and blocks that end up empty go too. Says whether it was there.
fn remove_uses(item: &mut toml_edit::Item, name: &str) -> bool {
    let Some(table) = item.as_table_like_mut() else { return false };
    let mut removed = false;

    for key in ["deps", "header-deps", "needs", "whole-archive"] {
        let Some(list) = table.get_mut(key).and_then(|item| item.as_array_mut()) else {
            continue;
        };
        if remove_from_list(list, name) {
            removed = true;
            if list.is_empty() {
                table.remove(key);
            }
        }
    }

    if let Some(public) = table.get_mut("public") {
        if remove_uses(public, name) {
            removed = true;
            if public.as_table_like().is_some_and(|public| public.is_empty()) {
                table.remove("public");
            }
        }
    }

    // when blocks: each, then the `when` table itself
    let mut when_emptied = false;
    if let Some(when) = table.get_mut("when").and_then(|item| item.as_table_like_mut()) {
        let mut emptied = Vec::new();
        for (condition, block) in when.iter_mut() {
            if remove_uses(block, name) {
                removed = true;
                if block.as_table_like().is_some_and(|block| block.is_empty()) {
                    emptied.push(condition.get().to_string());
                }
            }
        }
        for condition in &emptied {
            when.remove(condition);
        }
        when_emptied = !emptied.is_empty() && when.is_empty();
    }
    if when_emptied {
        table.remove("when");
    }

    // `{ kind = "exe", deps = [...] }` without deps is `{ kind = "exe" }`
    if let Some(inline) = item.as_inline_table_mut().filter(|_| removed) {
        inline.fmt();
    }

    removed
}

/// Takes `name` and `name/<target>` out of a list, with the comment on the removed item's line; keeps the comment of
/// the item before it. Says whether anything went.
fn remove_from_list(list: &mut toml_edit::Array, name: &str) -> bool {
    let is_use = |value: &toml_edit::Value| {
        value
            .as_str()
            .is_some_and(|dep| dep == name || dep.strip_prefix(name).is_some_and(|rest| rest.starts_with('/')))
    };
    let mut removed = false;

    let mut index = 0;
    while index < list.len() {
        if !list.get(index).is_some_and(is_use) {
            index += 1;
            continue;
        }

        let item = list.remove(index);
        removed = true;

        // What follows it: the next item's prefix, or the list's end
        let before = item.decor().prefix().and_then(|prefix| prefix.as_str()).unwrap_or_default().to_string();
        let after = match list.get(index) {
            Some(next) => next.decor().prefix().and_then(|prefix| prefix.as_str()).unwrap_or_default(),
            None => list.trailing().as_str().unwrap_or_default(),
        };
        let joined = join_around_removed(&before, after);
        match list.get_mut(index) {
            Some(next) => next.decor_mut().set_prefix(joined),
            None => list.set_trailing(joined),
        }
    }

    // `[a, b]` without `a` is `[b]`, not `[ b]`
    if let Some(first) = list.get_mut(0) {
        let inline = first
            .decor()
            .prefix()
            .and_then(|prefix| prefix.as_str())
            .is_some_and(|prefix| !prefix.contains('\n'));
        if inline {
            first.decor_mut().set_prefix("");
        }
    }

    removed
}

/// The whitespace and comments left where an item was: `before` is what preceded it (its first line ends the line
/// before: the previous item's comment), `after` what followed it (its first line ends the item's own line: its
/// comment, which goes with it).
fn join_around_removed(before: &str, after: &str) -> String {
    let after = match after.split_once('\n') {
        Some((own_line, rest)) if own_line.contains('#') => format!("\n{rest}"),
        _ => after.to_string(),
    };

    match before.split_once('\n') {
        Some((previous_line, _)) if after.starts_with('\n') => format!("{previous_line}{after}"),
        Some(_) => before.to_string(),
        None => after,
    }
}

/// The targets to add the dependency to: --to, else the only non-test target (the only test for --dev).
fn choose_users(document: &toml_edit::DocumentMut, args: &AddArgs) -> Result<Vec<String>> {
    let targets = document.get("target").and_then(|item| item.as_table_like());

    if !args.to.is_empty() {
        let Some(targets) = targets.filter(|targets| !targets.is_empty()) else {
            bail!("--to names a target, but anvil.toml has no [target] tables: with the layout conventions every [deps] entry is used by the main target (and [dev-deps] by the tests)");
        };
        for name in &args.to {
            if !targets.contains_key(name) {
                let names: Vec<&str> = targets.iter().map(|(name, _)| name).collect();
                let hint = util::did_you_mean(name, names.iter().copied());
                bail!("--to {name}: there is no [target.{name}]{hint}. Targets: {}", names.join(", "));
            }
            let is_test = targets.get(name).and_then(|target| target.get("kind")).and_then(|kind| kind.as_str()) == Some("test");
            if args.dev && !is_test {
                bail!("--dev --to {name}: [dev-deps] are for tests, and [target.{name}] is not kind = \"test\"");
            }
        }
        return Ok(args.to.clone());
    }

    let Some(targets) = targets else { return Ok(Vec::new()) };
    let candidates: Vec<String> = targets
        .iter()
        .filter(|(_, item)| (item.get("kind").and_then(|kind| kind.as_str()) == Some("test")) == args.dev)
        .map(|(name, _)| name.to_string())
        .collect();

    Ok(if candidates.len() == 1 { candidates } else { Vec::new() })
}

/// Fails, with the closest name, for a port the project's vcpkg does not have (its ports tree and overlays); only
/// warns when vcpkg-configuration.json adds registries Anvil cannot search.
fn check_vcpkg_port(root: &Path, spec: &VcpkgSpec, name: &str) -> Result<()> {
    let resolve = |text: &String| -> Result<PathBuf> { Ok(util::absolute(root, Path::new(&util::expand_env(text)?))) };
    let root_override = spec.root.as_ref().map(resolve).transpose()?;
    let Ok(instance) = Instance::select(root_override.as_deref()) else {
        return Ok(());
    };

    let overlay_ports: Vec<PathBuf> = spec.overlay_ports.iter().map(resolve).collect::<Result<_>>()?;
    let configuration_folder = own_vcpkg_json(root, spec)?.and_then(|file| file.parent().map(Path::to_path_buf));

    match instance.knows_port(name, &overlay_ports, configuration_folder.as_deref()) {
        Some(false) => Err(instance.unknown_port_error(name, &overlay_ports)),
        None if instance.has_ports => {
            ui::warn(&format!(
                "vcpkg-configuration.json adds registries, so Anvil cannot check that `{name}` is a port in them"
            ));
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The project's own vcpkg.json, if it has one.
fn own_vcpkg_json(root: &Path, spec: &VcpkgSpec) -> Result<Option<PathBuf>> {
    let configured = spec
        .manifest
        .as_ref()
        .map(|text| -> Result<PathBuf> { Ok(util::absolute(root, Path::new(&util::expand_env(text)?))) })
        .transpose()?;
    vcpkg::own_manifest(root, configured.as_deref())
}

/// A project with its own vcpkg.json gets the port added there too, with its features.
fn add_to_vcpkg_json(root: &Path, spec: &VcpkgSpec, name: &str, features: &[String]) -> Result<()> {
    let Some(manifest) = own_vcpkg_json(root, spec)? else { return Ok(()) };

    let text = std::fs::read_to_string(&manifest)?;
    let mut json: serde_json::Value = serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", slash(&manifest)))?;
    let dependencies = json
        .as_object_mut()
        .context("vcpkg.json is not an object")?
        .entry("dependencies")
        .or_insert_with(|| serde_json::json!([]));
    let list = dependencies.as_array_mut().context("vcpkg.json dependencies is not a list")?;

    // Listed already: only features to add
    let position = list
        .iter()
        .position(|dependency| dependency.as_str().or_else(|| dependency.get("name").and_then(|name| name.as_str())) == Some(name));
    if let Some(position) = position {
        let entry = &mut list[position];
        if entry.is_string() && !features.is_empty() {
            *entry = serde_json::json!({ "name": name });
        }
        let missing: Vec<&String> = features
            .iter()
            .filter(|feature| !crate::vcpkg::dependency_names(entry.get("features")).contains(feature))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let entry_features = entry.as_object_mut().unwrap().entry("features").or_insert_with(|| serde_json::json!([]));
        entry_features
            .as_array_mut()
            .context("vcpkg.json: features is not a list")?
            .extend(missing.iter().map(|feature| serde_json::Value::String(feature.to_string())));
        write_json(&manifest, &text, &json)?;
        ui::status("Added", &format!("features to {name} in {}", slash(&manifest)));
        return Ok(());
    }

    // Listed in a manifest feature already: Anvil turns that feature on when the port is used
    let in_feature = json.get("features").and_then(|features| features.as_object()).and_then(|features| {
        features
            .iter()
            .find(|(_, feature)| {
                crate::vcpkg::dependency_names(feature.get("dependencies"))
                    .iter()
                    .any(|listed| listed == name)
            })
            .map(|(feature_name, _)| feature_name.clone())
    });
    if let Some(feature) = in_feature {
        ui::note(&format!(
            "{name} is in vcpkg.json's \"{feature}\" feature; Anvil enables that feature when it is used"
        ));
        return Ok(());
    }

    let list = json["dependencies"].as_array_mut().context("vcpkg.json dependencies is not a list")?;
    if features.is_empty() {
        list.push(serde_json::Value::String(name.to_string()));
    } else {
        list.push(serde_json::json!({ "name": name, "features": features }));
    }

    write_json(&manifest, &text, &json)?;
    ui::status("Added", &format!("{name} to {}", slash(&manifest)));

    Ok(())
}

/// Takes a port out of the "dependencies" of the project's own vcpkg.json.
fn remove_from_vcpkg_json(root: &Path, spec: &VcpkgSpec, name: &str) -> Result<()> {
    let Some(manifest) = own_vcpkg_json(root, spec)? else { return Ok(()) };

    let text = std::fs::read_to_string(&manifest)?;
    let mut json: serde_json::Value = serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", slash(&manifest)))?;
    let Some(list) = json.get_mut("dependencies").and_then(|list| list.as_array_mut()) else {
        return Ok(());
    };

    let before = list.len();
    list.retain(|dependency| dependency.as_str().or_else(|| dependency.get("name").and_then(|name| name.as_str())) != Some(name));
    if list.len() == before {
        return Ok(());
    }

    write_json(&manifest, &text, &json)?;
    ui::status("Removed", &format!("{name} from {}", slash(&manifest)));
    Ok(())
}

/// Writes JSON back with the indent and line endings `original` used.
fn write_json(path: &Path, original: &str, json: &serde_json::Value) -> Result<()> {
    use serde::Serialize;

    // The first indented line's indent (two spaces for a one-line file)
    let indent = original
        .lines()
        .map(|line| &line[..line.len() - line.trim_start_matches([' ', '\t']).len()])
        .find(|indent| !indent.is_empty())
        .unwrap_or("  ");

    let mut buffer = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut buffer, serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes()));
    json.serialize(&mut serializer)?;

    let mut text = String::from_utf8(buffer)? + "\n";
    if original.contains("\r\n") {
        text = text.replace('\n', "\r\n");
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", slash(path)))
}

fn write_new(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content).with_context(|| format!("writing {}", slash(path)))
}

/// The Doxygen block new headers open with, signed with git's user when there is one.
fn header_block(file: &str, brief: &str) -> String {
    let git_config = |key: &str| -> Option<String> {
        let output = Command::new("git").args(["config", "--get", key]).output().ok()?;
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!value.is_empty()).then_some(value)
    };

    let author = match (git_config("user.name"), git_config("user.email")) {
        (Some(name), Some(email)) => format!(" * @author {name} ({email})\n"),
        (Some(name), None) => format!(" * @author {name}\n"),
        _ => String::new(),
    };
    let year = current_year();

    format!("/**\n * @file {file}\n{author} * @brief {brief}\n *\n * @copyright Copyright (c) {year}\n*/\n")
}

fn current_year() -> i64 {
    // Days since 1970 to a civil year (Howard Hinnant's algorithm)
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64 / 86_400)
        .unwrap_or(0);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month = (5 * day_of_year + 2) / 153;

    year_of_era + era * 400 + i64::from(month >= 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The document after taking `name` out of every target, preset and [defaults] use.
    fn without(text: &str, name: &str) -> String {
        let mut document: toml_edit::DocumentMut = text.parse().unwrap();
        for section in ["target", "preset"] {
            if let Some(tables) = document.get_mut(section).and_then(|item| item.as_table_like_mut()) {
                for (_, table) in tables.iter_mut() {
                    remove_uses(table, name);
                }
            }
        }
        document.to_string()
    }

    #[test]
    fn remove_takes_uses_with_their_comments() {
        let text = "[target.app]\ndeps = [\n    \"a\", # about a\n    \"fmt\", # printing\n    \"mylib/core\",\n    \"zlib\", # last\n]\n";

        // The removed item's comment goes, the one before stays
        assert_eq!(
            without(text, "fmt"),
            "[target.app]\ndeps = [\n    \"a\", # about a\n    \"mylib/core\",\n    \"zlib\", # last\n]\n"
        );

        // `name/target` uses go with the name; so does a comment at the end of the list
        assert_eq!(
            without(&without(text, "mylib"), "zlib"),
            "[target.app]\ndeps = [\n    \"a\", # about a\n    \"fmt\", # printing\n]\n"
        );
        assert_eq!(
            without("[target.app]\ndeps = [\"fmt\", \"b\"]\n", "fmt"),
            "[target.app]\ndeps = [\"b\"]\n"
        );
    }

    #[test]
    fn remove_drops_lists_and_blocks_left_empty() {
        let text = "[target.app]\nkind = \"exe\"\nsources = []\ndeps = [\"fmt\"]\nheader-deps = [\"fmt\"]\n\n\
                    [target.app.public]\ndeps = [\"fmt\"]\n\n[target.app.when.windows]\ndeps = [\"fmt\"]\n\n\
                    [target.app.when.linux]\ndeps = [\"fmt\", \"x\"]\n\n[preset.p]\ndeps = [\"fmt\"]\n";
        let result = without(text, "fmt");

        assert!(
            !result.contains("header-deps") && !result.contains("public") && !result.contains("when.windows"),
            "{result}"
        );
        assert!(
            result.contains("sources = []") && result.contains("deps = [\"x\"]") && result.contains("[preset.p]"),
            "{result}"
        );

        let inline = "[target]\napp = { kind = \"exe\", deps = [\"fmt\"], when = { windows = { deps = [\"fmt\"] } } }\n";
        assert_eq!(without(inline, "fmt"), "[target]\napp = { kind = \"exe\" }\n");
    }
}
