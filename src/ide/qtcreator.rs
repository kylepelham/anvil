//! Qt Creator (experimental): `.qtcreator/project.json`, a workspace project (File > Open Workspace, Qt Creator 14 and
//! later; build configurations need 15). A build configuration per profile runs `anvil build` in the profile's output
//! folder, and a run target per program starts it from the active configuration's folder. Anvil's entries are those it
//! wrote (named `anvil: ...`, listed in .anvil/ide.toml); the user's entries and keys stay. Qt Creator gives a workspace
//! project no code model: compile_commands.json, opened as a project of its own, is that.

use super::jsonc::{self, Node};
use super::{entry_names, owned_entry, quote_word, Project, Record, Writer, OWNED_PREFIX, STATE_FOLDER};
use crate::ui;
use crate::util::{self, slash};
use anyhow::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// Keys of the file, written as they are, dots included
const CONFIGURATIONS_KEY: &str = "build.configuration";
const TARGETS_KEY: &str = "targets";
const EXCLUDE_KEY: &str = "files.exclude";
const NAME_KEY: &str = "project.name";

// What Qt Creator expands in a run target's executable: the active build configuration's folder
const BUILD_FOLDER: &str = "%{ActiveProject:BuildConfig:Path}";

// How the record lists a folder Anvil added to files.exclude, beside its entries' names
const EXCLUDE_ENTRY: &str = "files.exclude: ";

pub fn outputs(root: &Path) -> Vec<PathBuf> {
    vec![root.join(".qtcreator").join("project.json")]
}

pub fn generate(project: &Project, record: &Record, writer: &mut Writer) -> Result<()> {
    let file = &outputs(&project.root)[0];
    let configurations = build_configurations(project, record);
    let targets = run_targets(project);

    // Anvil's entries, and the folders it hid from the project tree; a folder the user takes out of the list stays out
    let previous = record.entries_of(&util::rel(file, &project.root));
    let mut names = entry_names(&configurations, "name");
    names.extend(entry_names(&targets, "name"));
    let excluded = excluded_folders(project);
    let mut entries = names.clone();
    entries.extend(excluded.iter().map(|folder| format!("{EXCLUDE_ENTRY}{folder}")));
    let owned = |node: &Node| owned_entry(node, "name", previous, &names, &record.anvil);

    let new = || {
        jsonc::document(&json!({
            NAME_KEY: project.name,
            EXCLUDE_KEY: excluded,
            CONFIGURATIONS_KEY: configurations,
            TARGETS_KEY: targets,
        }))
    };
    let merge = |text: &str| -> Result<String> {
        let mut text = jsonc::replace_entries(text, &[CONFIGURATIONS_KEY], &owned, &configurations)?;
        text = jsonc::replace_entries(&text, &[TARGETS_KEY], &owned, &targets)?;

        // The folders not yet excluded, unless Anvil added them before (the user took them out)
        let document = jsonc::parse(&text)?;
        let listed = |folder: &str| {
            let exclude = document.member(EXCLUDE_KEY);
            let items = exclude.into_iter().flat_map(|exclude| match &exclude.kind {
                jsonc::NodeKind::Array(items) => items.iter().filter_map(|(item, _)| item.text()).collect(),
                _ => Vec::new(),
            });
            items.into_iter().any(|item| item.trim_end_matches('/') == folder)
        };
        let added_before = |folder: &str| previous.is_some_and(|previous| previous.contains(&format!("{EXCLUDE_ENTRY}{folder}")));
        let missing: Vec<Value> = excluded
            .iter()
            .filter(|folder| !listed(folder) && !added_before(folder))
            .map(|folder| json!(folder))
            .collect();
        if !missing.is_empty() {
            text = jsonc::replace_entries(&text, &[EXCLUDE_KEY], &|_| false, &missing)?;
        }

        Ok(text)
    };

    writer.merge(file, Some(entries), new, merge)
}

/// A build configuration per profile: `anvil build` with that profile, from the project root, in its output folder.
fn build_configurations(project: &Project, record: &Record) -> Vec<Value> {
    let root = &project.root;
    let platform = &project.toolchain.platform;

    let mut configurations = Vec::new();
    for profile in &project.profiles {
        let out_dir = super::profile_out_dir(project, record, profile, platform);
        let arguments: Vec<String> = record.args(&["build"], Some(profile)).iter().map(|word| quote_word(word)).collect();
        configurations.push(json!({
            "name": format!("{OWNED_PREFIX}{profile}"),
            "buildDirectory": relative_path(&out_dir, root),
            "steps": [{
                "executable": record.anvil,
                "arguments": arguments,
                "workingDirectory": slash(root),
            }],
        }));
    }

    configurations
}

/// A run target per program, as `anvil run` starts it, from the active build configuration's output folder.
fn run_targets(project: &Project) -> Vec<Value> {
    let mut targets = Vec::new();
    for (target, run) in project.runnable() {
        let Some(output) = &target.output else { continue };
        let executable = match util::strip_prefix_ci(output, &project.out_dir) {
            Some(relative) => format!("{BUILD_FOLDER}/{}", slash(&relative)),
            None => slash(output),
        };
        targets.push(json!({
            "name": format!("{OWNED_PREFIX}{}", target.qualified),
            "executable": executable,
            "arguments": run.args.iter().map(|word| quote_word(word)).collect::<Vec<_>>(),
            "workingDirectory": slash(&run.workdir),
        }));
    }

    targets
}

/// The folders Qt Creator's project tree leaves out: the output folder and Anvil's own, when inside the project.
fn excluded_folders(project: &Project) -> Vec<String> {
    let folders = [project.out_root.clone(), project.root.join(STATE_FOLDER)];
    folders
        .iter()
        .filter_map(|folder| util::strip_prefix_ci(folder, &project.root))
        .filter(|relative| !relative.as_os_str().is_empty())
        .map(|relative| slash(&relative))
        .collect()
}

/// What `anvil ide qtcreator` says about the programs Qt Creator cannot start as `anvil run` does: a run target has no
/// environment variables.
pub fn note_environments(project: &Project) {
    for (target, run) in project.runnable().filter(|(_, run)| !run.env.is_empty()) {
        let names: Vec<&str> = run.env.iter().map(|(name, _)| name.as_str()).collect();
        ui::note(&format!(
            "Qt Creator's run targets have no environment: `{OWNED_PREFIX}{}` starts without {}; `anvil run` (or `anvil test`) sets them",
            target.qualified,
            names.join(", ")
        ));
    }
}

/// A path inside the project relative to it, as Qt Creator resolves a build directory; else in full.
fn relative_path(path: &Path, root: &Path) -> String {
    match util::strip_prefix_ci(path, root) {
        Some(relative) if !relative.as_os_str().is_empty() => slash(&relative),
        _ => slash(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ide::{sample_project, Debugger};

    fn record() -> Record {
        Record {
            anvil: "C:/tools/my anvil/anvil.exe".into(),
            profile: "debug".into(),
            toolchain: Some("gcc".into()),
            ..Default::default()
        }
    }

    #[test]
    fn a_configuration_per_profile_and_a_target_per_program() {
        let root = Path::new("C:/work/game");
        let project = sample_project(root, Debugger::Gdb);
        let record = record();

        // Each profile builds in its own folder (a toolchain `anvil ide` was given names it), words with spaces quoted
        let configurations = build_configurations(&project, &record);
        assert_eq!(configurations.len(), 2);
        assert_eq!(configurations[0]["name"], json!("anvil: debug"));
        assert_eq!(configurations[0]["buildDirectory"], json!("out/debug"));
        assert_eq!(configurations[1]["buildDirectory"], json!("out/release-gcc"));
        let step = &configurations[1]["steps"][0];
        assert_eq!(step["executable"], json!("C:/tools/my anvil/anvil.exe"));
        assert_eq!(step["arguments"], json!(["build", "-p", "release", "--toolchain", "gcc"]));
        assert_eq!(step["workingDirectory"], json!("C:/work/game"));

        // Programs from the active configuration's folder, with their arguments and folder
        let targets = run_targets(&project);
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0]["name"], json!("anvil: game"));
        assert_eq!(targets[0]["executable"], json!("%{ActiveProject:BuildConfig:Path}/game.exe"));
        assert_eq!(targets[1]["arguments"], json!(["--fast"]));
        assert_eq!(targets[1]["workingDirectory"], json!("C:/work/game"));
        assert_eq!(excluded_folders(&project), ["out", ".anvil"]);
    }

    #[test]
    fn the_users_entries_and_keys_stay() {
        let folder = std::env::temp_dir().join(format!("anvil_qtcreator_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        let project = sample_project(&folder, Debugger::Gdb);
        let record = record();
        let file = &outputs(&folder)[0];

        // A file of the user's: its name, excludes and targets stay; Anvil's are added
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let mine = "{\n    \"project.name\": \"Mine\",\n    \"files.exclude\": [\"build\", \"out/\"],\n    \"targets\": [\n        { \"name\": \"tool\", \"executable\": \"/bin/tool\" }\n    ]\n}\n";
        std::fs::write(file, mine).unwrap();
        let mut writer = Writer::default();
        generate(&project, &record, &mut writer).unwrap();
        let text = std::fs::read_to_string(file).unwrap();
        let written: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(written["project.name"], json!("Mine"));
        assert_eq!(written["files.exclude"], json!(["build", "out/", ".anvil"]));
        let names: Vec<&str> = written["targets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|target| target["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["tool", "anvil: game", "anvil: unit"]);
        assert_eq!(written["build.configuration"].as_array().unwrap().len(), 2);

        // Written again from what the record lists: the same text, and an exclude the user took out stays out
        let (_, entries) = writer.entries.pop().unwrap();
        let record = Record {
            entries: [(util::rel(file, &folder), entries)].into(),
            ..record
        };
        let without = text.replace(",\n    \".anvil\"", "");
        assert_ne!(without, text);
        std::fs::write(file, &without).unwrap();
        generate(&project, &record, &mut Writer::default()).unwrap();
        assert_eq!(std::fs::read_to_string(file).unwrap(), without);

        // A program gone: its target goes, the user's stays
        let mut fewer = sample_project(&folder, Debugger::Gdb);
        fewer.targets.pop();
        generate(&fewer, &record, &mut Writer::default()).unwrap();
        let written: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
        assert_eq!(written["targets"].as_array().unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(&folder);
    }
}
