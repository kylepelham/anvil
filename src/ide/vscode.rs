//! VS Code: `.vscode/tasks.json` (the build as the default build task, a build per program, the tests),
//! `.vscode/launch.json` (a debug configuration per program, which builds it first) and `.vscode/settings.json`
//! (cpptools reads the root compile_commands.json; Even Better TOML gets Anvil's schemas). Anvil's entries are those
//! it wrote (named `anvil: ...`, listed in .anvil/ide.toml); every other entry, and every comment, is the user's and
//! stays.

use super::jsonc::{self, Node, NodeKind};
use super::{entry_names, owned_entry, project_path, Debugger, Project, Record, Writer, OWNED_PREFIX};
use crate::util::{self, slash};
use anyhow::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const WORKSPACE: &str = "${workspaceFolder}";

// The settings Anvil sets: cpptools' compilation database (when the user has none), the compilers clangd may ask for
// their system headers (GCC and Clang; when the user has no arguments of their own), the schemas' associations
const COMPILE_COMMANDS_SETTING: &str = "C_Cpp.default.compileCommands";
const CLANGD_SETTING: &str = "clangd.arguments";
const QUERY_DRIVER: &str = "--query-driver=";
const SCHEMA_SETTING: &str = "evenBetterToml.schema.associations";
const MANIFEST_PATTERN: &str = ".*/anvil\\.toml$";
const TESTS_PATTERN: &str = ".*/tests\\.toml$";

pub fn outputs(root: &Path) -> Vec<PathBuf> {
    ["tasks.json", "launch.json", "settings.json"]
        .iter()
        .map(|file| root.join(".vscode").join(file))
        .collect()
}

pub fn generate(project: &Project, record: &Record, writer: &mut Writer) -> Result<()> {
    let [tasks_file, launch_file, settings_file] = <[PathBuf; 3]>::try_from(outputs(&project.root)).unwrap();

    let lists = [
        (&tasks_file, "tasks", "label", "2.0.0", tasks(project, record)),
        (&launch_file, "configurations", "name", "0.2.0", launches(project)),
    ];
    for (file, list, name_key, version, entries) in lists {
        let previous = record.entries_of(&util::rel(file, &project.root));
        merge_list(file, list, name_key, version, &entries, previous, &record.anvil, writer)?;
    }

    // Settings: the compilation database unless the user names one, the schemas always
    let (manifest_url, tests_url) = crate::schema::install_files()?;
    let query_driver = project
        .toolchain
        .compiler
        .parent()
        .map(|folder| format!("{QUERY_DRIVER}{}/*", slash(folder)));
    let merge = |text: &str| -> Result<String> {
        let mut text = text.to_string();
        let settings = jsonc::parse(&text)?;
        if settings.member(COMPILE_COMMANDS_SETTING).is_none() {
            let value = json!(format!("{WORKSPACE}/compile_commands.json"));
            text = jsonc::set_member(&text, &[], COMPILE_COMMANDS_SETTING, &value)?;
        }
        if let (false, Some(query_driver)) = (project.toolchain.msvc_abi, &query_driver) {
            if settings.member(CLANGD_SETTING).is_none_or(is_query_driver) {
                text = jsonc::set_member(&text, &[], CLANGD_SETTING, &json!([query_driver]))?;
            }
        }
        for (pattern, url) in [(MANIFEST_PATTERN, &manifest_url), (TESTS_PATTERN, &tests_url)] {
            text = jsonc::set_member(&text, &[SCHEMA_SETTING], pattern, &json!(url))?;
        }
        Ok(text)
    };
    let empty = || "{}\n".to_string();
    writer.merge(&settings_file, None, || merge(&empty()).unwrap_or_else(|_| empty()), merge)
}

/// The tasks: building everything (the default build task), building each program (what its launch runs first), and
/// the tests (the default test task). Compiler messages become problems, cl's and GCC's alike.
fn tasks(project: &Project, record: &Record) -> Vec<Value> {
    let profile = Some(record.profile.as_str());
    let matchers = json!([
        { "base": "$msCompile", "fileLocation": ["autoDetect", WORKSPACE] },
        { "base": "$gcc", "fileLocation": ["autoDetect", WORKSPACE] },
    ]);
    let task = |label: String, args: Vec<String>, group: Value| {
        json!({
            "label": label,
            "type": "process",
            "command": record.anvil,
            "args": args,
            "options": { "cwd": WORKSPACE },
            "group": group,
            "problemMatcher": matchers,
        })
    };

    let mut tasks = vec![task(
        format!("{OWNED_PREFIX}build"),
        record.args(&["build"], profile),
        json!({ "kind": "build", "isDefault": true }),
    )];
    for (target, _) in project.runnable() {
        tasks.push(task(
            build_label(&target.qualified),
            record.args(&["build", &target.qualified], profile),
            json!("build"),
        ));
    }
    tasks.push(task(
        format!("{OWNED_PREFIX}test"),
        record.args(&["test"], profile),
        json!({ "kind": "test", "isDefault": true }),
    ));

    tasks
}

/// A debug configuration per program, as `anvil run` starts it: Visual Studio's debugger for the MSVC ABI, else gdb
/// or lldb through cpptools.
fn launches(project: &Project) -> Vec<Value> {
    let root = &project.root;
    let toolchain = &project.toolchain;

    let mut launches = Vec::new();
    for (target, run) in project.runnable() {
        let Some(output) = &target.output else { continue };
        let environment: Vec<Value> = run.env.iter().map(|(name, value)| json!({ "name": name, "value": value })).collect();
        let mut launch = json!({
            "name": format!("{OWNED_PREFIX}{}", target.qualified),
            "type": if toolchain.debugger == Debugger::Msvc { "cppvsdbg" } else { "cppdbg" },
            "request": "launch",
            "program": project_path(output, root, WORKSPACE),
            "args": run.args,
            "cwd": project_path(&run.workdir, root, WORKSPACE),
            "environment": environment,
            "preLaunchTask": build_label(&target.qualified),
        });

        let settings = launch.as_object_mut().unwrap();
        match toolchain.debugger {
            Debugger::Msvc if !target.gui => {
                settings.insert("console".into(), json!("integratedTerminal"));
            }
            Debugger::Msvc => {}
            Debugger::Gdb | Debugger::Lldb => {
                settings.insert("externalConsole".into(), json!(false));
                settings.insert("MIMode".into(), json!(toolchain.debugger.name()));
                if let Some(path) = &toolchain.debugger_path {
                    settings.insert("miDebuggerPath".into(), json!(slash(path)));
                }
                if toolchain.debugger == Debugger::Gdb {
                    let printing = json!({ "description": "Pretty-print values", "text": "-enable-pretty-printing", "ignoreFailures": true });
                    settings.insert("setupCommands".into(), json!([printing]));
                }
            }
        }
        launches.push(launch);
    }

    launches
}

/// clangd's arguments as Anvil sets them: one `--query-driver`.
fn is_query_driver(arguments: &Node) -> bool {
    match &arguments.kind {
        NodeKind::Array(elements) => elements.len() == 1 && elements[0].0.text().is_some_and(|argument| argument.starts_with(QUERY_DRIVER)),
        _ => false,
    }
}

fn build_label(target: &str) -> String {
    format!("{OWNED_PREFIX}build {target}")
}

/// Replaces Anvil's entries of the file's list (those it wrote last time, `previous`, and those of the same names), or
/// writes the file.
#[allow(clippy::too_many_arguments)]
fn merge_list(
    file: &Path,
    list: &str,
    name_key: &str,
    version: &str,
    entries: &[Value],
    previous: Option<&[String]>,
    anvil: &str,
    writer: &mut Writer,
) -> Result<()> {
    let names = entry_names(entries, name_key);
    let owned = |node: &Node| owned_entry(node, name_key, previous, &names, anvil);

    writer.merge(
        file,
        Some(names.clone()),
        || jsonc::document(&json!({ "version": version, list: entries })),
        |text| jsonc::replace_entries(text, &[list], &owned, entries),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ide::sample_project;

    fn record() -> Record {
        Record {
            ides: vec![super::super::Ide::Vscode],
            anvil: "anvil".into(),
            profile: "debug".into(),
            ..Default::default()
        }
    }

    #[test]
    fn tasks_build_everything_by_default_and_each_program() {
        let project = sample_project(Path::new("C:/p"), Debugger::Msvc);
        let tasks = super::tasks(&project, &record());
        let labels: Vec<&str> = tasks.iter().map(|task| task["label"].as_str().unwrap()).collect();

        assert_eq!(labels, ["anvil: build", "anvil: build game", "anvil: build unit", "anvil: test"]);
        assert_eq!(tasks[0]["group"]["isDefault"], json!(true));
        assert_eq!(tasks[1]["args"], json!(["build", "game", "-p", "debug"]));
        assert_eq!(tasks[0]["problemMatcher"][0]["base"], json!("$msCompile"));
    }

    #[test]
    fn only_the_entries_anvil_wrote_are_replaced() {
        let folder = std::env::temp_dir().join(format!("anvil_vscode_merge_{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join("tasks.json");
        std::fs::write(
            &file,
            "{\n    \"tasks\": [\n        { \"label\": \"anvil: mine\" },\n        { \"label\": \"anvil: old\" },\n        { \"label\": \"anvil: build\" }\n    ]\n}\n",
        )
        .unwrap();

        // The user's `anvil: mine` stays; what Anvil wrote last time (`anvil: old`) and `anvil: build` are replaced
        let mut writer = Writer::default();
        let entries = [json!({ "label": "anvil: build", "command": "anvil" })];
        let previous = ["anvil: old".to_string()];
        merge_list(&file, "tasks", "label", "2.0.0", &entries, Some(&previous), "anvil", &mut writer).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("anvil: mine"));
        assert!(!text.contains("anvil: old"));
        assert!(text.contains("\"command\": \"anvil\""));
        assert_eq!(writer.entries, [(file.clone(), vec!["anvil: build".to_string()])]);

        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn launches_start_programs_as_anvil_run_does() {
        let project = sample_project(Path::new("C:/p"), Debugger::Msvc);
        let launches = launches(&project);
        assert_eq!(launches[0]["type"], json!("cppvsdbg"));
        assert_eq!(launches[0]["program"], json!("${workspaceFolder}/out/debug/game.exe"));
        assert_eq!(launches[0]["cwd"], json!("${workspaceFolder}"));
        assert_eq!(launches[0]["preLaunchTask"], json!("anvil: build game"));
        assert_eq!(launches[1]["args"], json!(["--fast"]));
        assert_eq!(launches[1]["environment"], json!([{ "name": "MODE", "value": "test" }]));

        let gdb = super::launches(&sample_project(Path::new("C:/p"), Debugger::Gdb));
        assert_eq!(gdb[0]["type"], json!("cppdbg"));
        assert_eq!(gdb[0]["MIMode"], json!("gdb"));
        assert_eq!(gdb[0]["miDebuggerPath"], json!("C:/mingw/bin/gdb.exe"));
    }
}
