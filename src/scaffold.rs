//! Creating and editing projects: `anvil new`, `anvil init`, `anvil add`.

use crate::project::{self, MANIFEST_NAME};
use crate::targets::{self, Kind};
use crate::ui;
use crate::util::{self, slash};
use crate::vcpkg::Instance;
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

#[derive(clap::Args)]
pub struct AddArgs {
    /// The dependency's name (the vcpkg port, unless another source is given).
    pub name: String,

    /// A local folder (an Anvil project, or headers/sources).
    #[arg(long, value_name = "FOLDER")]
    pub path: Option<String>,

    /// A git repository.
    #[arg(long, value_name = "URL")]
    pub git: Option<String>,

    #[arg(long, requires = "git")]
    pub tag: Option<String>,

    #[arg(long, requires = "git")]
    pub branch: Option<String>,

    #[arg(long, requires = "git")]
    pub rev: Option<String>,

    /// Found through pkg-config.
    #[arg(long)]
    pub pkg_config: bool,

    /// A system library (user32, pthread, ...).
    #[arg(long)]
    pub system: bool,

    /// vcpkg features, comma separated.
    #[arg(long, value_delimiter = ',')]
    pub features: Vec<String>,

    /// Targets that use it (adds it to their deps).
    #[arg(long, value_name = "TARGET")]
    pub to: Vec<String>,
}

pub fn new_project(start_dir: &Path, name: &str, library: bool) -> Result<()> {
    let folder = util::absolute(&std::env::current_dir()?, &start_dir.join(name));
    if folder.exists() && std::fs::read_dir(&folder)?.next().is_some() {
        bail!("{} already exists and is not empty", slash(&folder));
    }

    let identifier = targets::sanitize_name(name).replace(['-', '.'], "_");
    write_new(
        &folder.join(MANIFEST_NAME),
        &format!("[project]\nname = \"{name}\"\nversion = \"0.1.0\"\nstd = \"c++23\"\n"),
    )?;
    write_new(&folder.join(".gitignore"), "/out/\n/compile_commands.json\n")?;

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
    ui::note(&format!("next: cd {name} && {next}"));

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
    std::fs::write(
        &manifest_path,
        format!("[project]\nname = \"{name}\"\nversion = \"0.1.0\"\nstd = \"c++20\"\n"),
    )?;
    ui::status("Created", &slash(&manifest_path));

    // Say what the conventions found
    let model = crate::commands::load_model(&folder, &crate::BuildArgs::default());
    match model {
        Ok(model) => {
            for target in model.targets.iter().filter(|target| target.kind != Kind::External) {
                ui::note(&format!(
                    "{} `{}` from {} source files",
                    target.kind.name(),
                    target.name,
                    target.sources.len()
                ));
            }
            if model.vcpkg.is_some() {
                ui::note("vcpkg packages installed");
            }
        }
        Err(error) => ui::warn(&format!("the project does not load yet: {error:#}")),
    }

    Ok(())
}

pub fn add(start_dir: &Path, args: &AddArgs) -> Result<()> {
    let (root, _) = project::locate(start_dir)?;
    let manifest_path = root.join(MANIFEST_NAME);
    if !manifest_path.exists() {
        let name = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "app".into());
        std::fs::write(&manifest_path, format!("[project]\nname = \"{name}\"\n"))?;
    }

    let text = std::fs::read_to_string(&manifest_path)?;
    let mut document: toml_edit::DocumentMut = text.parse().with_context(|| format!("parsing {}", slash(&manifest_path)))?;

    // The entry
    let entry = dependency_entry(args, &root)?;
    let deps = document
        .entry("deps")
        .or_insert(toml_edit::table())
        .as_table_mut()
        .context("[deps] is not a table")?;
    if deps.contains_key(&args.name) {
        // Already declared; dependency_entry has added it to vcpkg.json if it was missing there
        ui::note(&format!("`{}` is already in [deps]", args.name));
        return Ok(());
    }
    deps.insert(&args.name, entry);

    // Targets that use it
    let has_targets = document
        .get("target")
        .and_then(|item| item.as_table())
        .is_some_and(|table| !table.is_empty());
    let users = choose_users(&document, args)?;
    for user in &users {
        let target = document["target"][user.as_str()]
            .as_table_mut()
            .with_context(|| format!("no [target.{user}]"))?;
        let deps = target.entry("deps").or_insert(toml_edit::value(toml_edit::Array::new()));
        deps.as_array_mut().context("deps is not a list")?.push(args.name.as_str());
    }

    std::fs::write(&manifest_path, document.to_string())?;
    ui::status("Added", &format!("{} to {}", args.name, slash(&manifest_path)));

    if has_targets && users.is_empty() {
        ui::note(&format!(
            "add \"{}\" to the deps of the targets that use it (or run again with --to <target>)",
            args.name
        ));
    }

    Ok(())
}

/// The [deps] value for the arguments, checking the vcpkg port exists.
fn dependency_entry(args: &AddArgs, root: &Path) -> Result<toml_edit::Item> {
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
        check_vcpkg_port(&args.name)?;
        add_to_vcpkg_json(root, &args.name, &args.features)?;

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

/// The targets to add the dependency to: --to, else the only non-test target.
fn choose_users(document: &toml_edit::DocumentMut, args: &AddArgs) -> Result<Vec<String>> {
    let Some(targets) = document.get("target").and_then(|item| item.as_table()) else {
        return Ok(Vec::new());
    };

    if !args.to.is_empty() {
        for name in &args.to {
            if !targets.contains_key(name) {
                let names: Vec<&str> = targets.iter().map(|(name, _)| name).collect();
                bail!("no [target.{name}]; targets: {}", names.join(", "));
            }
        }
        return Ok(args.to.clone());
    }

    let candidates: Vec<String> = targets
        .iter()
        .filter(|(_, item)| item.get("kind").and_then(|kind| kind.as_str()) != Some("test"))
        .map(|(name, _)| name.to_string())
        .collect();

    Ok(if candidates.len() == 1 { candidates } else { Vec::new() })
}

fn check_vcpkg_port(name: &str) -> Result<()> {
    let Ok(instance) = Instance::select(None) else { return Ok(()) };

    if instance.has_port(name) == Some(false) {
        let ports = instance.root.join("ports");
        let port_names: Vec<String> = std::fs::read_dir(&ports)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        let hint = util::suggest(name, port_names.iter().map(|port| port.as_str()))
            .map(|close| format!(" Did you mean `{close}`?"))
            .unwrap_or_default();
        bail!(
            "vcpkg has no port `{name}` (in {}).{hint} Search with `anvil vcpkg search {name}`.",
            slash(&instance.root)
        );
    }

    Ok(())
}

/// A project with its own vcpkg.json gets the port added there too.
fn add_to_vcpkg_json(root: &Path, name: &str, features: &[String]) -> Result<()> {
    let manifest = root.join("vcpkg.json");
    if !manifest.is_file() {
        return Ok(());
    }

    let mut json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&manifest)?).context("vcpkg.json is not valid JSON")?;
    let dependencies = json
        .as_object_mut()
        .context("vcpkg.json is not an object")?
        .entry("dependencies")
        .or_insert_with(|| serde_json::json!([]));
    if !dependencies.is_array() {
        bail!("vcpkg.json dependencies is not a list");
    }

    if crate::vcpkg::dependency_names(Some(dependencies)).iter().any(|listed| listed == name) {
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

    std::fs::write(&manifest, serde_json::to_string_pretty(&json)? + "\n")?;
    ui::status("Added", &format!("{name} to {}", slash(&manifest)));

    Ok(())
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
