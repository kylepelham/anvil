//! Zed: `.zed/tasks.json` (build, test) and `.zed/debug.json` (a debug configuration per program, built first). Both
//! are lists; Anvil's entries are those it wrote (labelled `anvil: ...`, listed in .anvil/ide.toml), and every other
//! entry and comment stays.

use super::jsonc::{self, Node};
use super::{entry_names, owned_entry, project_path, Debugger, Project, Record, Writer, OWNED_PREFIX};
use crate::util;
use anyhow::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const WORKTREE: &str = "$ZED_WORKTREE_ROOT";

pub fn outputs(root: &Path) -> Vec<PathBuf> {
    ["tasks.json", "debug.json"].iter().map(|file| root.join(".zed").join(file)).collect()
}

pub fn generate(project: &Project, record: &Record, writer: &mut Writer) -> Result<()> {
    let [tasks_file, debug_file] = <[PathBuf; 2]>::try_from(outputs(&project.root)).unwrap();

    for (file, entries) in [(&tasks_file, tasks(record)), (&debug_file, debug_configurations(project, record))] {
        let previous = record.entries_of(&util::rel(file, &project.root));
        merge_list(file, &entries, previous, &record.anvil, writer)?;
    }

    Ok(())
}

fn tasks(record: &Record) -> Vec<Value> {
    let profile = Some(record.profile.as_str());
    let task = |label: &str, words: &[&str]| {
        json!({
            "label": format!("{OWNED_PREFIX}{label}"),
            "command": record.anvil,
            "args": record.args(words, profile),
            "cwd": WORKTREE,
        })
    };

    vec![task("build", &["build"]), task("test", &["test"])]
}

/// A configuration per program, as `anvil run` starts it, built first: GDB's adapter for gdb, else CodeLLDB.
fn debug_configurations(project: &Project, record: &Record) -> Vec<Value> {
    let root = &project.root;
    let adapter = match project.toolchain.debugger {
        Debugger::Gdb => "GDB",
        Debugger::Lldb | Debugger::Msvc => "CodeLLDB",
    };

    let mut configurations = Vec::new();
    for (target, run) in project.runnable() {
        let Some(output) = &target.output else { continue };
        let env: serde_json::Map<String, Value> = run.env.iter().map(|(name, value)| (name.clone(), json!(value))).collect();
        configurations.push(json!({
            "label": format!("{OWNED_PREFIX}{}", target.qualified),
            "adapter": adapter,
            "request": "launch",
            "program": project_path(output, root, WORKTREE),
            "args": run.args,
            "cwd": project_path(&run.workdir, root, WORKTREE),
            "env": env,
            "build": {
                "command": record.anvil,
                "args": record.args(&["build", &target.qualified], Some(&record.profile)),
                "cwd": WORKTREE,
            },
        }));
    }

    configurations
}

/// Replaces Anvil's entries of a list file (those it wrote last time, `previous`, and those of the same labels), or
/// writes it.
fn merge_list(file: &Path, entries: &[Value], previous: Option<&[String]>, anvil: &str, writer: &mut Writer) -> Result<()> {
    let labels = entry_names(entries, "label");
    let owned = |node: &Node| owned_entry(node, "label", previous, &labels, anvil);

    writer.merge(
        file,
        Some(labels.clone()),
        || jsonc::document(&Value::Array(entries.to_vec())),
        |text| jsonc::replace_entries(text, &[], &owned, entries),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ide::sample_project;

    #[test]
    fn debug_configurations_build_then_start_each_program() {
        let project = sample_project(Path::new("/p"), Debugger::Gdb);
        let record = Record {
            anvil: "anvil".into(),
            profile: "debug".into(),
            ..Default::default()
        };
        let configurations = debug_configurations(&project, &record);

        assert_eq!(configurations.len(), 2);
        assert_eq!(configurations[0]["adapter"], json!("GDB"));
        assert_eq!(configurations[0]["program"], json!("$ZED_WORKTREE_ROOT/out/debug/game.exe"));
        assert_eq!(configurations[0]["build"]["args"], json!(["build", "game", "-p", "debug"]));
        assert_eq!(configurations[1]["env"], json!({ "MODE": "test" }));
        assert_eq!(tasks(&record)[0]["label"], json!("anvil: build"));
    }
}
