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

// What `anvil new` and `anvil init` keep out of git
const GITIGNORE: &str = "/out/\n/compile_commands.json\n";

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

    #[arg(long, requires = "git", conflicts_with_all = ["branch", "rev"])]
    pub tag: Option<String>,

    #[arg(long, requires = "git", conflicts_with = "rev")]
    pub branch: Option<String>,

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

pub fn new_project(start_dir: &Path, path: &str, library: bool) -> Result<()> {
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
    write_new(&folder.join(".gitignore"), GITIGNORE)?;

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
    ui::note(&format!("next: cd {path} && {next}"));

    Ok(())
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

    // Sources the conventions find (src/, or files here), else a target over the folders that hold some
    let conventional = folder.join("src").is_dir() || has_sources_in(&folder, 1);
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
        document["target"][name.as_str()] = target_table(&folder, &source_folders);
    }

    std::fs::write(&manifest_path, document.to_string()).with_context(|| format!("writing {}", slash(&manifest_path)))?;
    ui::status("Created", &slash(&manifest_path));
    if !folder.join(".gitignore").exists() {
        std::fs::write(folder.join(".gitignore"), GITIGNORE)?;
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
    let (root, manifest) = project::locate(start_dir)?;
    let manifest_path = root.join(MANIFEST_NAME);
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
        Some(existing) if !args.features.is_empty() => {
            let port = vcpkg_port_of(existing, &args.name)
                .with_context(|| format!("`{}` in [{table_name}] is not a vcpkg port; features belong to vcpkg ports", args.name))?;
            add_to_vcpkg_json(&root, &vcpkg_spec, &port, &args.features)?;
            changed = add_features(existing, &args.name, &args.features)?;
        }
        Some(_) => {}
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
    let (root, manifest) = project::locate(start_dir)?;
    let manifest_path = root.join(MANIFEST_NAME);
    let text = std::fs::read_to_string(&manifest_path).with_context(|| format!("reading {}", slash(&manifest_path)))?;
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
        let hint = util::suggest(name, declared.iter().map(|name| name.as_str()))
            .map(|close| format!(" Did you mean `{close}`?"))
            .unwrap_or_default();
        bail!("`{name}` is not in [deps] or [dev-deps] of {}.{hint}", slash(&manifest_path));
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

    // The port in vcpkg.json, unless another entry still uses it
    let Some(port) = port else { return Ok(()) };
    let still_used = manifest
        .deps
        .iter()
        .chain(&manifest.dev_deps)
        .filter(|(other, _)| other.as_str() != name)
        .any(|(other, value)| crate::manifest::parse_dep(other, value).ok().and_then(|spec| spec.vcpkg).as_deref() == Some(port.as_str()));
    if !still_used {
        remove_from_vcpkg_json(&root, &manifest.vcpkg.clone().unwrap_or_default(), &port)?;
    }

    Ok(())
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

/// `[target.<name>]` over folders of sources the conventions do not find: a program when one has a main file.
fn target_table(root: &Path, folders: &[String]) -> toml_edit::Item {
    let has_main = folders.iter().any(|folder| {
        walkdir::WalkDir::new(root.join(folder))
            .into_iter()
            .flatten()
            .any(|entry| entry.file_type().is_file() && entry.path().file_stem().is_some_and(|stem| stem == "main"))
    });

    let mut sources = toml_edit::Array::new();
    let mut include = toml_edit::Array::new();
    for folder in folders {
        sources.push(format!("{folder}/{}", targets::SOURCE_GLOB));
        include.push(folder.as_str());
    }

    let mut table = toml_edit::Table::new();
    table.insert("kind", toml_edit::value(if has_main { "exe" } else { "lib" }));
    table.insert("sources", toml_edit::value(sources));
    table.insert("include", toml_edit::value(include));
    toml_edit::Item::Table(table)
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
    walkdir::WalkDir::new(folder).max_depth(depth).into_iter().flatten().any(|entry| {
        let extension = entry.path().extension().map(|extension| extension.to_string_lossy().to_ascii_lowercase());
        entry.file_type().is_file() && matches!(extension.as_deref(), Some("c" | "cc" | "cpp" | "cxx" | "c++"))
    })
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

/// Takes `name` out of the dependency lists of a target-like table and its when blocks; says whether it was there.
fn remove_uses(table: &mut toml_edit::Item, name: &str) -> bool {
    let Some(table) = table.as_table_like_mut() else { return false };
    let mut removed = false;

    for key in ["deps", "header-deps", "needs", "whole-archive"] {
        if let Some(list) = table.get_mut(key).and_then(|item| item.as_array_mut()) {
            let before = list.len();
            list.retain(|dep| dep.as_str() != Some(name));
            removed |= list.len() != before;

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
        }
    }
    if let Some(public) = table.get_mut("public") {
        removed |= remove_uses(public, name);
    }
    if let Some(when) = table.get_mut("when").and_then(|item| item.as_table_like_mut()) {
        for (_, block) in when.iter_mut() {
            removed |= remove_uses(block, name);
        }
    }

    removed
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
                let hint = util::suggest(name, names.iter().copied())
                    .map(|close| format!(" Did you mean `{close}`?"))
                    .unwrap_or_default();
                bail!("--to {name}: there is no [target.{name}].{hint} Targets: {}", names.join(", "));
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
