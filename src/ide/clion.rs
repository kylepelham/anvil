//! CLion, as a compilation database project (it reads the root compile_commands.json): the anvil commands as external
//! tools (`.idea/tools/Anvil.xml`), custom build targets that run them (`.idea/customTargets.xml`), and a Custom Build
//! Application run configuration per program (`.idea/runConfigurations/`). Untested: written to JetBrains' formats.

use super::{project_path, Project, Record, TargetInfo, Writer, OWNED_PREFIX};
use crate::commands::RunSetup;
use crate::util::{self, slash};
use anyhow::Result;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

const PROJECT_DIR: &str = "$PROJECT_DIR$";

// The project's folder as external tools name it
const TOOL_PROJECT_DIR: &str = "$ProjectFileDir$";

// The external tools' group, which names their actions (`Tool_Anvil_<name>`)
const TOOL_GROUP: &str = "Anvil";

// Run configuration files Anvil writes start so; it deletes those it wrote for programs that are gone
const RUN_CONFIGURATION_PREFIX: &str = "anvil_";

pub fn outputs(root: &Path) -> Vec<PathBuf> {
    let idea = root.join(".idea");
    vec![
        idea.join("tools").join(format!("{TOOL_GROUP}.xml")),
        idea.join("customTargets.xml"),
        idea.join("runConfigurations"),
    ]
}

pub fn generate(project: &Project, record: &Record, writer: &mut Writer) -> Result<()> {
    let idea = project.root.join(".idea");

    // Build targets: everything, and each program
    let mut targets: Vec<(String, Vec<String>)> = vec![("build".into(), record.args(&["build"], Some(&record.profile)))];
    for (target, _) in project.runnable() {
        let args = record.args(&["build", &target.qualified], Some(&record.profile));
        targets.push((format!("build {}", target.qualified), args));
    }
    let clean = ("clean".to_string(), record.args(&["clean"], Some(&record.profile)));
    writer.write(
        &idea.join("tools").join(format!("{TOOL_GROUP}.xml")),
        &tools_text(record, &targets, &clean),
    )?;

    // Build targets in the user's file: Anvil's (those it wrote last time, and those of the same names) replaced
    let custom_file = idea.join("customTargets.xml");
    let labels: Vec<String> = targets.iter().map(|(name, _)| format!("{OWNED_PREFIX}{name}")).collect();
    let previous = record.entries_of(&util::rel(&custom_file, &project.root));
    let owned = |name: &str, element: &str| match previous {
        _ if labels.iter().any(|owned| owned == name) => true,
        Some(previous) => previous.iter().any(|owned| owned == name),
        None => name.starts_with(OWNED_PREFIX) && element.contains(&format!("actionId=\"Tool_{TOOL_GROUP}_")),
    };
    writer.merge(
        &custom_file,
        Some(labels.clone()),
        || custom_targets_text(project, "", &targets, &clean.0, &owned),
        |text| Ok(custom_targets_text(project, text, &targets, &clean.0, &owned)),
    )?;

    // A run configuration per program, and none for those Anvil wrote for programs that are gone
    let folder = idea.join("runConfigurations");
    let mut current = Vec::new();
    for (target, run) in project.runnable() {
        let file = format!("{RUN_CONFIGURATION_PREFIX}{}.xml", file_name(&target.qualified));
        writer.write(&folder.join(&file), &run_configuration_text(project, target, run))?;
        current.push(file);
    }
    let previous: Vec<String> = match record.entries_of(&util::rel(&folder, &project.root)) {
        Some(previous) => previous.to_vec(),
        None => legacy_run_configurations(&folder),
    };
    for file in previous {
        if !current.contains(&file) {
            writer.remove(&folder.join(file))?;
        }
    }
    writer.entries.push((folder, current));

    // A compilation database project, when CLion has not made the folder its own kind of project yet
    let misc = idea.join("misc.xml");
    if !misc.exists() {
        writer.write(&misc, MISC_TEXT)?;
    }

    Ok(())
}

const MISC_TEXT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="CompDBSettings">
    <option name="linkedExternalProjectsSettings">
      <CompDBProjectSettings>
        <option name="externalProjectPath" value="$PROJECT_DIR$" />
        <option name="modules">
          <set>
            <option value="$PROJECT_DIR$" />
          </set>
        </option>
      </CompDBProjectSettings>
    </option>
  </component>
  <component name="CompDBWorkspace" PROJECT_DIR="$PROJECT_DIR$" />
  <component name="ExternalStorageConfigurationManager" enabled="true" />
</project>
"#;

/// The run configurations an anvil before the record's lists wrote: named `anvil_...`, building an `anvil: ` target.
fn legacy_run_configurations(folder: &Path) -> Vec<String> {
    let built_by_anvil =
        |path: &Path| std::fs::read_to_string(path).is_ok_and(|text| text.contains(&format!("TARGET_NAME=\"{}build ", xml(OWNED_PREFIX))));

    std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(RUN_CONFIGURATION_PREFIX) && built_by_anvil(&entry.path()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

/// The external tools: one per build target, and clean.
fn tools_text(record: &Record, targets: &[(String, Vec<String>)], clean: &(String, Vec<String>)) -> String {
    let mut text = String::from("<toolSet name=\"Anvil\">\n");

    for (name, args) in targets.iter().chain(std::iter::once(clean)) {
        let _ = write!(
            text,
            r#"  <tool name="{}" showInMainMenu="false" showInEditor="false" showInProject="false" showInSearchPopup="false" disabled="false" useConsole="true" showConsoleOnStdOut="false" showConsoleOnStdErr="false" synchronizeAfterRun="true">
    <exec>
      <option name="COMMAND" value="{}" />
      <option name="PARAMETERS" value="{}" />
      <option name="WORKING_DIRECTORY" value="{}" />
    </exec>
  </tool>
"#,
            xml(name),
            xml(&record.anvil),
            xml(&parameters(args)),
            TOOL_PROJECT_DIR
        );
    }

    text.push_str("</toolSet>\n");
    text
}

/// customTargets.xml: Anvil's targets (those `owned` picks by name) replaced, the user's kept as they are.
fn custom_targets_text(
    project: &Project,
    existing: &str,
    targets: &[(String, Vec<String>)],
    clean: &str,
    owned: &dyn Fn(&str, &str) -> bool,
) -> String {
    let mut ours = String::new();
    for (name, _) in targets {
        let label = format!("{OWNED_PREFIX}{name}");
        let _ = write!(
            ours,
            r#"    <target id="{0}" name="{1}" defaultType="TOOL">
      <configuration id="{0}" name="{1}">
        <build type="TOOL">
          <tool actionId="Tool_{TOOL_GROUP}_{2}" />
        </build>
        <clean type="TOOL">
          <tool actionId="Tool_{TOOL_GROUP}_{3}" />
        </clean>
      </configuration>
    </target>
"#,
            id(project, &label),
            xml(&label),
            xml(name),
            xml(clean)
        );
    }

    // The user's targets: every <target> element Anvil does not own
    let mut kept = String::new();
    let mut rest = existing;
    while let Some(start) = rest.find("<target ") {
        let Some(length) = rest[start..].find("</target>") else { break };
        let element = &rest[start..start + length + "</target>".len()];
        if !owned(&element_name(element), element) {
            let line_start = rest[..start].rfind('\n').map(|newline| newline + 1).unwrap_or(start);
            let _ = writeln!(kept, "{}{element}", &rest[line_start..start]);
        }
        rest = &rest[start + length + "</target>".len()..];
    }

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<project version=\"4\">\n  <component name=\"CLionExternalBuildManager\">\n{kept}{ours}  </component>\n</project>\n"
    )
}

/// A Custom Build Application: builds its target, then starts the program as `anvil run` does.
fn run_configuration_text(project: &Project, target: &TargetInfo, run: &RunSetup) -> String {
    let root = &project.root;
    let label = format!("{OWNED_PREFIX}{}", target.qualified);
    let build_label = format!("{OWNED_PREFIX}build {}", target.qualified);
    let program = target
        .output
        .as_deref()
        .map(|output| project_path(output, root, PROJECT_DIR))
        .unwrap_or_default();

    let mut envs = String::new();
    for (name, value) in &run.env {
        let _ = writeln!(envs, "      <env name=\"{}\" value=\"{}\" />", xml(name), xml(value));
    }
    let envs = match envs.is_empty() {
        true => String::new(),
        false => format!("    <envs>\n{envs}    </envs>\n"),
    };

    format!(
        r#"<component name="ProjectRunConfigurationManager">
  <configuration default="false" name="{label}" type="CLionExternalRunConfiguration" factoryName="Application" REDIRECT_INPUT="false" ELEVATE="false" USE_EXTERNAL_CONSOLE="false" EMULATE_TERMINAL="false" PASS_PARENT_ENVS_2="true" PROJECT_NAME="{project_name}" TARGET_NAME="{build_label}" CONFIG_NAME="{build_label}" RUN_PATH="{program}" PROGRAM_PARAMS="{params}" WORKING_DIR="file://{workdir}">
{envs}    <method v="2">
      <option name="CLION.EXTERNAL.BUILD" enabled="true" />
    </method>
  </configuration>
</component>
"#,
        label = xml(&label),
        project_name = xml(&project.name),
        build_label = xml(&build_label),
        program = xml(&program),
        params = xml(&parameters(&run.args)),
        workdir = xml(&project_path(&run.workdir, root, PROJECT_DIR)),
    )
}

/// The `name` attribute of an element's opening tag, its XML escapes read.
fn element_name(element: &str) -> String {
    let tag = &element[..element.find('>').unwrap_or(element.len())];
    let Some(start) = tag.find(" name=\"").map(|start| start + " name=\"".len()) else {
        return String::new();
    };
    let value = &tag[start..start + tag[start..].find('"').unwrap_or(tag.len() - start)];

    value
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Arguments as one line, quoted where they hold spaces.
fn parameters(args: &[String]) -> String {
    let quoted: Vec<String> = args
        .iter()
        .map(|arg| match arg.is_empty() || arg.contains([' ', '"']) {
            true => format!("\"{}\"", arg.replace('"', "\\\"")),
            false => arg.clone(),
        })
        .collect();
    quoted.join(" ")
}

fn file_name(name: &str) -> String {
    name.chars()
        .map(|character| if character.is_ascii_alphanumeric() { character } else { '_' })
        .collect()
}

/// A stable id for a target, as CLion writes them (a UUID).
fn id(project: &Project, name: &str) -> String {
    let high = crate::util::hash_bytes(format!("{}|{name}|1", slash(&project.root)).as_bytes());
    let low = crate::util::hash_bytes(format!("{}|{name}|2", slash(&project.root)).as_bytes());
    let hex = format!("{high:016x}{low:016x}");
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ide::{sample_project, Debugger};

    #[test]
    fn custom_targets_keep_the_users_and_replace_anvils() {
        let project = sample_project(Path::new("/p"), Debugger::Gdb);
        let existing = r#"<project version="4">
  <component name="CLionExternalBuildManager">
    <target id="1" name="mine" defaultType="TOOL">
      <configuration id="2" name="mine" />
    </target>
    <target id="3" name="anvil: old" defaultType="TOOL">
      <configuration id="4" name="anvil: old" />
    </target>
  </component>
</project>
"#;
        let targets = vec![("build".to_string(), vec!["build".to_string()])];
        let owned = |name: &str, _: &str| ["anvil: old", "anvil: build"].contains(&name);
        let text = custom_targets_text(&project, existing, &targets, "clean", &owned);

        assert!(text.contains(r#"<target id="1" name="mine" defaultType="TOOL">"#));
        assert!(!text.contains("anvil: old"));
        assert!(text.contains(r#"name="anvil: build""#));
        assert!(text.contains(r#"<tool actionId="Tool_Anvil_build" />"#));
        assert_eq!(custom_targets_text(&project, &text, &targets, "clean", &owned), text);

        // A target of the user's named like Anvil's, but not written by it, stays
        let mine = existing.replace("anvil: old", "anvil: mine");
        assert!(custom_targets_text(&project, &mine, &targets, "clean", &owned).contains(r#"name="anvil: mine""#));
        assert_eq!(element_name(r#"<target id="3" name="a &amp; b">"#), "a & b");
    }

    #[test]
    fn run_configurations_build_their_target_then_run_it() {
        let project = sample_project(Path::new("/p"), Debugger::Gdb);
        let (target, run) = project.runnable().nth(1).unwrap();
        let text = run_configuration_text(&project, target, run);

        assert!(text.contains(r#"TARGET_NAME="anvil: build unit""#));
        assert!(text.contains(r#"RUN_PATH="$PROJECT_DIR$/out/debug/unit.exe""#));
        assert!(text.contains(r#"PROGRAM_PARAMS="--fast""#));
        assert!(text.contains(r#"<env name="MODE" value="test" />"#));
    }
}
