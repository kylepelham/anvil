//! Zed: `.zed/tasks.json` (build, test) and `.zed/debug.json` (a debug configuration per program, built first). Both
//! are lists; Anvil's entries are those labelled `anvil: ...`, and every other entry and comment stays.

use super::jsonc::{self, Node};
use super::{project_path, Debugger, Project, Record, Writer, OWNED_PREFIX};
use anyhow::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const WORKTREE: &str = "$ZED_WORKTREE_ROOT";

pub fn outputs(root: &Path) -> Vec<PathBuf> {
    ["tasks.json", "debug.json"].iter().map(|file| root.join(".zed").join(file)).collect()
}

pub fn generate(project: &Project, record: &Record, writer: &mut Writer) -> Result<()> {
    let [tasks_file, debug_file] = <[PathBuf; 2]>::try_from(outputs(&project.root)).unwrap();

    merge_list(&tasks_file, &tasks(record), writer)?;
    merge_list(&debug_file, &debug_configurations(project, record), writer)?;

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

/// Replaces Anvil's entries of a list file, or writes it.
fn merge_list(file: &Path, entries: &[Value], writer: &mut Writer) -> Result<()> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => {
            let owned = |node: &Node| {
                node.member("label")
                    .and_then(Node::text)
                    .is_some_and(|label| label.starts_with(OWNED_PREFIX))
            };
            jsonc::replace_entries(&text, &[], &owned, entries)
        }
        Err(_) => Ok(jsonc::document(&Value::Array(entries.to_vec()))),
    };

    writer.merge(file, text)
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
