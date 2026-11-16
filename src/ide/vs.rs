//! Visual Studio: a solution at the project root (`<name>.sln`, to double-click) and a Makefile project per target in
//! `.anvil/vs/`, with a configuration per profile. Building runs anvil; IntelliSense gets each target's include
//! folders, definitions and standard from the profile's compile_commands.json; the debugger starts the program as
//! `anvil run` does.

use super::{Project, Record, TargetInfo, Writer, STATE_FOLDER};
use crate::commands::RunSetup;
use crate::platform::{Cpu, Os, Platform};
use crate::profile::Profile;
use crate::project::Model;
use crate::targets::Kind;
use crate::util::{self, slash};
use anyhow::{bail, Result};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

// Where the projects go, in Anvil's folder
const PROJECTS_FOLDER: &str = "vs";

// Visual Studio's project kinds: C++, and solution folders
const CPP_PROJECT: &str = "{8BC9CEB8-8B4A-11D0-8D11-00A0C91BC942}";
const SOLUTION_FOLDER: &str = "{2150E333-8FDC-42A3-9474-1A3956D46DE8}";

// Header files shown in a project, from its include folders
const HEADER_PATTERNS: &[&str] = &["h", "hh", "hpp", "hxx", "inl", "ipp"];

/// A solution configuration: a profile, for the Windows machine it builds for.
struct Configuration {
    profile: String,
    name: String,
    platform: &'static str,
    out_dir: PathBuf,
}

/// What IntelliSense reads for a target in one configuration.
#[derive(Default, Clone)]
struct IntelliSense {
    includes: Vec<String>,
    defines: Vec<String>,
    forced_includes: Vec<String>,
    options: Vec<String>,
}

/// A compile_commands.json entry, as far as IntelliSense needs it.
struct CompileEntry {
    file: String,
    output: String,
    directory: PathBuf,
    arguments: Vec<String>,
}

pub fn solution_path(root: &Path, project_name: &str) -> PathBuf {
    root.join(format!("{project_name}.sln"))
}

/// The files `generate` writes.
pub fn outputs(project: &Project) -> Vec<PathBuf> {
    let mut files = vec![solution_path(&project.root, &project.name)];
    files.extend(projects(project).iter().map(|(target, _)| project_file(project, target)));
    files
}

/// The output folders of the configurations, whose compile commands IntelliSense reads.
pub fn configuration_out_dirs(model: &Model, project: &Project, record: &Record) -> Vec<PathBuf> {
    configurations(model, project, record)
        .map(|configurations| configurations.into_iter().map(|configuration| configuration.out_dir).collect())
        .unwrap_or_default()
}

pub fn generate(model: &Model, project: &Project, record: &Record, writer: &mut Writer) -> Result<()> {
    let configurations = configurations(model, project, record)?;
    if configurations.is_empty() {
        bail!("no profile builds for Windows, so Visual Studio has nothing to build");
    }

    // Each configuration's compile commands; a profile not built yet borrows this one's
    let entries: Vec<(PathBuf, Vec<CompileEntry>)> = configurations
        .iter()
        .map(|configuration| {
            let own = read_compile_commands(&configuration.out_dir);
            match own {
                Some(entries) => (configuration.out_dir.clone(), entries),
                None => (project.out_dir.clone(), read_compile_commands(&project.out_dir).unwrap_or_default()),
            }
        })
        .collect();

    // A project per target
    let projects = projects(project);
    for (target, display_name) in &projects {
        let intellisense: Vec<IntelliSense> = entries
            .iter()
            .map(|(out_dir, entries)| target_intellisense(project, target, out_dir, entries))
            .collect();
        let text = project_text(model, project, record, target, display_name, &configurations, &intellisense);
        writer.write(&project_file(project, target), &text)?;
    }

    // The solution, then the projects of targets that are gone
    let text = solution_text(project, &projects, &configurations);
    writer.write(&solution_path(&project.root, &project.name), &text)?;
    let folder = project.root.join(STATE_FOLDER).join(PROJECTS_FOLDER);
    let current: Vec<PathBuf> = projects.iter().map(|(target, _)| project_file(project, target)).collect();
    for entry in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
        let path = entry.path();
        let stale = path.extension().is_some_and(|extension| extension == "vcxproj") && !current.contains(&path);
        if stale {
            writer.remove(&path)?;
        }
    }

    Ok(())
}

/// The targets that get a project, the startup one first (Visual Studio starts the first project of a solution it has
/// no settings for), with their names in the solution: the target's, or `package.target` where two packages share one.
fn projects(project: &Project) -> Vec<(&TargetInfo, String)> {
    let mut targets: Vec<&TargetInfo> = project
        .targets
        .iter()
        .filter(|target| !matches!(target.kind, Kind::Headers | Kind::External))
        .collect();

    let startup = project
        .default_run
        .as_deref()
        .and_then(|name| targets.iter().position(|target| target.qualified == name))
        .or_else(|| targets.iter().position(|target| target.kind == Kind::Exe && target.runnable));
    if let Some(startup) = startup {
        let first = targets.remove(startup);
        targets.insert(0, first);
    }

    let shared = |name: &str| project.targets.iter().filter(|target| target.name == name).count() > 1;
    targets
        .into_iter()
        .map(|target| {
            let name = match shared(&target.name) {
                true => format!("{}.{}", target.package, target.name),
                false => target.name.clone(),
            };
            (target, name)
        })
        .collect()
}

fn project_file(project: &Project, target: &TargetInfo) -> PathBuf {
    let file = format!("{}.vcxproj", target.qualified.replace('/', "."));
    project.root.join(STATE_FOLDER).join(PROJECTS_FOLDER).join(file)
}

/// A configuration per profile that builds for Windows, named like the profile (`Debug`, `Release`, `Shipping`), on
/// the platform it builds for, with the output folder its builds use (a toolchain or machine `anvil ide` was given
/// gets its own, as on the command line).
fn configurations(model: &Model, project: &Project, record: &Record) -> Result<Vec<Configuration>> {
    let manifest = &model.packages[0].manifest;
    let mut configurations = Vec::new();

    for profile in &project.profiles {
        let platform = match record.target.as_deref() {
            Some(target) => Platform::parse(target)?,
            None => match Profile::resolve(profile, &manifest.profile)?.target {
                Some(target) => Platform::parse(&target)?,
                None => Platform::host(),
            },
        };
        if platform.os != Os::Windows {
            continue;
        }

        let out_dir = match *profile == project.profile {
            true => project.out_dir.clone(),
            false => {
                let mut folder = profile.clone();
                if record.toolchain.is_some() {
                    folder.push_str(&format!("-{}", model.toolchain.family.name()));
                }
                if record.target.is_some() && platform.is_cross() {
                    folder.push_str(&format!("-{}", platform.name()));
                }
                project.out_root.join(folder)
            }
        };

        let mut name: Vec<char> = profile.chars().collect();
        name[0] = name[0].to_ascii_uppercase();
        configurations.push(Configuration {
            profile: profile.clone(),
            name: name.into_iter().collect(),
            platform: match platform.cpu {
                Cpu::X64 => "x64",
                Cpu::X86 => "Win32",
                Cpu::Arm64 => "ARM64",
                Cpu::Arm => "ARM",
            },
            out_dir,
        });
    }

    Ok(configurations)
}

/// A target's Makefile project: anvil builds it, IntelliSense reads the compile commands, the debugger starts it.
fn project_text(
    model: &Model,
    project: &Project,
    record: &Record,
    target: &TargetInfo,
    display_name: &str,
    configurations: &[Configuration],
    intellisense: &[IntelliSense],
) -> String {
    let mut text = String::new();
    let root = &project.root;
    let anvil_words = |words: &[&str], profile: &str| {
        let mut command = vec![record.anvil.clone(), "-C".to_string(), slash(root)];
        command.extend(record.args(words, Some(profile)));
        property(&command_line(&command))
    };

    text.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\r\n");
    text.push_str("<!-- Written by `anvil ide vs`; every anvil build rewrites it. -->\r\n");
    text.push_str("<Project DefaultTargets=\"Build\" ToolsVersion=\"Current\" xmlns=\"http://schemas.microsoft.com/developer/msbuild/2003\">\r\n");

    // Configurations
    text.push_str("  <ItemGroup Label=\"ProjectConfigurations\">\r\n");
    for configuration in configurations {
        let _ = write!(
            text,
            "    <ProjectConfiguration Include=\"{0}|{1}\">\r\n      <Configuration>{0}</Configuration>\r\n      <Platform>{1}</Platform>\r\n    </ProjectConfiguration>\r\n",
            configuration.name, configuration.platform
        );
    }
    text.push_str("  </ItemGroup>\r\n");

    // Globals
    let _ = write!(
        text,
        "  <PropertyGroup Label=\"Globals\">\r\n    <VCProjectVersion>17.0</VCProjectVersion>\r\n    <ProjectGuid>{}</ProjectGuid>\r\n    <Keyword>MakeFileProj</Keyword>\r\n    <ProjectName>{}</ProjectName>\r\n  </PropertyGroup>\r\n",
        guid(project, &target.qualified),
        property(display_name)
    );
    text.push_str("  <Import Project=\"$(VCTargetsPath)\\Microsoft.Cpp.Default.props\" />\r\n");
    for configuration in configurations {
        let _ = write!(
            text,
            "  <PropertyGroup Condition=\"{}\" Label=\"Configuration\">\r\n    <ConfigurationType>Makefile</ConfigurationType>\r\n    <UseDebugLibraries>{}</UseDebugLibraries>\r\n  </PropertyGroup>\r\n",
            condition(configuration),
            configuration.profile == "debug"
        );
    }
    text.push_str("  <Import Project=\"$(VCTargetsPath)\\Microsoft.Cpp.props\" />\r\n");

    // Per configuration: the build commands, IntelliSense, the debugger
    for (configuration, intellisense) in configurations.iter().zip(intellisense) {
        let profile = configuration.profile.as_str();
        let relocate = |text: &str| relocate(text, &project.out_dir, &configuration.out_dir);
        let _ = write!(text, "  <PropertyGroup Condition=\"{}\">\r\n", condition(configuration));
        let build = anvil_words(&["build", &target.qualified], profile);
        let _ = write!(text, "    <NMakeBuildCommandLine>{build}</NMakeBuildCommandLine>\r\n");
        let _ = write!(text, "    <NMakeReBuildCommandLine>{build}</NMakeReBuildCommandLine>\r\n");
        let clean = anvil_words(&["clean"], profile);
        let _ = write!(text, "    <NMakeCleanCommandLine>{clean}</NMakeCleanCommandLine>\r\n");
        if let Some(output) = &target.output {
            let _ = write!(
                text,
                "    <NMakeOutput>{}</NMakeOutput>\r\n",
                property(&windows_path(&relocate(&slash(output))))
            );
        }

        let lists = [
            ("NMakeIncludeSearchPath", &intellisense.includes),
            ("NMakePreprocessorDefinitions", &intellisense.defines),
            ("NMakeForcedIncludes", &intellisense.forced_includes),
        ];
        for (name, values) in lists {
            let values: Vec<String> = values.iter().map(|value| property(value)).collect();
            let _ = write!(text, "    <{name}>{}</{name}>\r\n", values.join(";"));
        }
        let options: Vec<String> = intellisense.options.iter().map(|option| property(option)).collect();
        let _ = write!(text, "    <AdditionalOptions>{}</AdditionalOptions>\r\n", options.join(" "));

        // Intermediate files of MSBuild's own beside the project, not in the project root
        text.push_str("    <OutDir>$(MSBuildProjectDirectory)\\bin\\$(ProjectName)\\$(Configuration)\\</OutDir>\r\n");
        text.push_str("    <IntDir>$(MSBuildProjectDirectory)\\obj\\$(ProjectName)\\$(Configuration)\\</IntDir>\r\n");

        if let (Some(run), Some(output), true) = (&target.run, &target.output, target.runnable) {
            let (args, env) = debug_arguments(model, project, target, run, configuration);
            let environment: Vec<String> = env.iter().map(|(name, value)| format!("{name}={value}")).collect();
            let _ = write!(
                text,
                "    <LocalDebuggerCommand>{}</LocalDebuggerCommand>\r\n",
                property(&windows_path(&relocate(&slash(output))))
            );
            let _ = write!(
                text,
                "    <LocalDebuggerCommandArguments>{}</LocalDebuggerCommandArguments>\r\n",
                property(&command_line(&args))
            );
            let _ = write!(
                text,
                "    <LocalDebuggerWorkingDirectory>{}</LocalDebuggerWorkingDirectory>\r\n",
                property(&windows_path(&relocate(&slash(&run.workdir))))
            );
            let _ = write!(
                text,
                "    <LocalDebuggerEnvironment>{}</LocalDebuggerEnvironment>\r\n",
                property(&environment.join("\n"))
            );
            text.push_str("    <DebuggerFlavor>WindowsLocalDebugger</DebuggerFlavor>\r\n");
        }
        text.push_str("  </PropertyGroup>\r\n");
    }

    // Files: the sources, the headers of its include folders, and its package's manifest
    text.push_str("  <ItemGroup>\r\n");
    for source in &target.sources {
        let item = source_item(source);
        let _ = write!(text, "    <{item} Include=\"{}\" />\r\n", property(&windows_path(&slash(source))));
    }
    let exclude = format!("{}\\**", property(&windows_path(&slash(&project.out_root))));
    for folder in &target.header_folders {
        let folder = property(&windows_path(&slash(folder)));
        let patterns: Vec<String> = HEADER_PATTERNS.iter().map(|extension| format!("{folder}\\**\\*.{extension}")).collect();
        let _ = write!(text, "    <ClInclude Include=\"{}\" Exclude=\"{exclude}\" />\r\n", patterns.join(";"));
    }
    text.push_str("  </ItemGroup>\r\n");

    text.push_str("  <Import Project=\"$(VCTargetsPath)\\Microsoft.Cpp.targets\" />\r\n");
    text.push_str("</Project>\r\n");
    text
}

/// The arguments and environment the debugger starts a program with in a configuration: a test target's, its own
/// with their placeholders filled for that profile (`{profile}`, `{out}`); another program's, those of `anvil run`.
fn debug_arguments(
    model: &Model,
    project: &Project,
    target: &TargetInfo,
    run: &RunSetup,
    configuration: &Configuration,
) -> (Vec<String>, Vec<(String, String)>) {
    let relocate = |text: &str| relocate(text, &project.out_dir, &configuration.out_dir);
    if target.kind != Kind::Test {
        let env = run.env.iter().map(|(name, value)| (name.clone(), relocate(value))).collect();
        return (run.args.iter().map(|arg| relocate(arg)).collect(), env);
    }

    let spec = &model.targets[target.index];
    let package = &model.packages[spec.package];
    let fill = |text: &str| {
        let filled = crate::project::fill_test_text(text, package, &configuration.out_dir, &model.vcpkg_dir, &configuration.profile);
        relocate(&super::substitute_targets(&filled, model).unwrap_or(filled))
    };
    let args = spec.args.iter().map(|arg| fill(arg)).collect();
    let env = spec.env.iter().map(|(name, value)| (name.clone(), fill(value))).collect();

    (args, env)
}

/// The solution: the projects (startup first), a folder per package in a workspace, every configuration; each
/// configuration builds the targets a plain `anvil build --all` builds (non-default ones only when asked).
fn solution_text(project: &Project, projects: &[(&TargetInfo, String)], configurations: &[Configuration]) -> String {
    let mut text = String::from("\u{feff}\r\nMicrosoft Visual Studio Solution File, Format Version 12.00\r\n");
    text.push_str("# Visual Studio Version 17\r\n");
    text.push_str("VisualStudioVersion = 17.0.31903.59\r\n");
    text.push_str("MinimumVisualStudioVersion = 10.0.40219.1\r\n");

    for (target, name) in projects {
        let path = format!("{STATE_FOLDER}\\{PROJECTS_FOLDER}\\{}.vcxproj", target.qualified.replace('/', "."));
        let _ = write!(
            text,
            "Project(\"{CPP_PROJECT}\") = \"{name}\", \"{path}\", \"{}\"\r\nEndProject\r\n",
            guid(project, &target.qualified)
        );
    }

    // A folder per package, holding its manifest too, in a workspace; else the manifest alone
    let folders = project.packages.len() > 1;
    let package_folders: Vec<(String, String)> = match folders {
        true => project
            .packages
            .iter()
            .map(|package| (package.clone(), guid(project, &format!("folder:{package}"))))
            .collect(),
        false => Vec::new(),
    };
    for (package, folder_guid) in &package_folders {
        let _ = write!(
            text,
            "Project(\"{SOLUTION_FOLDER}\") = \"{package}\", \"{package}\", \"{folder_guid}\"\r\nEndProject\r\n"
        );
    }
    if !folders {
        let _ = write!(
            text,
            "Project(\"{SOLUTION_FOLDER}\") = \"Solution Items\", \"Solution Items\", \"{}\"\r\n\tProjectSection(SolutionItems) = preProject\r\n\t\tanvil.toml = anvil.toml\r\n\tEndProjectSection\r\nEndProject\r\n",
            guid(project, "folder:items")
        );
    }

    text.push_str("Global\r\n\tGlobalSection(SolutionConfigurationPlatforms) = preSolution\r\n");
    for configuration in configurations {
        let _ = write!(text, "\t\t{0}|{1} = {0}|{1}\r\n", configuration.name, configuration.platform);
    }
    text.push_str("\tEndGlobalSection\r\n\tGlobalSection(ProjectConfigurationPlatforms) = postSolution\r\n");
    for (target, _) in projects {
        let project_guid = guid(project, &target.qualified);
        for configuration in configurations {
            let pair = format!("{}|{}", configuration.name, configuration.platform);
            let _ = write!(text, "\t\t{project_guid}.{pair}.ActiveCfg = {pair}\r\n");
            if target.default || target.kind == Kind::Test {
                let _ = write!(text, "\t\t{project_guid}.{pair}.Build.0 = {pair}\r\n");
            }
        }
    }
    text.push_str(
        "\tEndGlobalSection\r\n\tGlobalSection(SolutionProperties) = preSolution\r\n\t\tHideSolutionNode = FALSE\r\n\tEndGlobalSection\r\n",
    );
    if folders {
        text.push_str("\tGlobalSection(NestedProjects) = preSolution\r\n");
        for (target, _) in projects {
            if let Some((_, folder_guid)) = package_folders.iter().find(|(package, _)| *package == target.package) {
                let _ = write!(text, "\t\t{} = {folder_guid}\r\n", guid(project, &target.qualified));
            }
        }
        text.push_str("\tEndGlobalSection\r\n");
    }
    let _ = write!(
        text,
        "\tGlobalSection(ExtensionGlobals) = postSolution\r\n\t\tSolutionGuid = {}\r\n\tEndGlobalSection\r\nEndGlobal\r\n",
        guid(project, "solution")
    );

    text
}

/// The include folders, definitions, forced includes and standard of a target's first C++ file (else its first
/// file) in a configuration's compile commands.
fn target_intellisense(project: &Project, target: &TargetInfo, out_dir: &Path, entries: &[CompileEntry]) -> IntelliSense {
    let object_dir = match util::strip_prefix_ci(&target.object_dir, &project.out_dir) {
        Some(relative) => out_dir.join(relative),
        None => target.object_dir.clone(),
    };
    let prefix = format!("{}/", slash(&object_dir).to_lowercase());

    let mut own = entries.iter().filter(|entry| entry.output.to_lowercase().starts_with(&prefix));
    let first = own.clone().next();
    let chosen = own.find(|entry| is_cxx(&entry.file)).or(first);
    match chosen {
        Some(entry) => read_arguments(&entry.arguments, &entry.directory),
        None => IntelliSense::default(),
    }
}

fn read_compile_commands(out_dir: &Path) -> Option<Vec<CompileEntry>> {
    let text = std::fs::read_to_string(out_dir.join("compile_commands.json")).ok()?;
    let values: Vec<serde_json::Value> = serde_json::from_str(&text).ok()?;

    let entries = values
        .iter()
        .map(|value| {
            let text = |key: &str| value[key].as_str().unwrap_or_default().to_string();
            let arguments = value["arguments"]
                .as_array()
                .map(|arguments| arguments.iter().filter_map(|argument| argument.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            CompileEntry {
                file: text("file"),
                output: text("output").replace('\\', "/"),
                directory: PathBuf::from(text("directory")),
                arguments,
            }
        })
        .collect();
    Some(entries)
}

/// What IntelliSense needs from a compile command, cl's and GCC's options alike.
fn read_arguments(arguments: &[String], directory: &Path) -> IntelliSense {
    let mut intellisense = IntelliSense::default();
    let absolute = |path: &str| windows_path(&slash(&util::absolute(directory, Path::new(path))));

    let mut words = arguments.iter().skip(1);
    while let Some(word) = words.next() {
        // An option and its value, joined (`/Iinclude`, `-DX`) or as the next word (`-I include`)
        let mut value_of = |names: &[&str]| -> Option<String> {
            let name = names.iter().find(|name| word.starts_with(**name))?;
            match &word[name.len()..] {
                "" => words.next().cloned(),
                rest => Some(rest.to_string()),
            }
        };

        if let Some(folder) = value_of(&["/external:I", "-external:I", "-isystem", "-imsvc", "-iquote", "/I", "-I"]) {
            intellisense.includes.push(absolute(&folder));
        } else if let Some(define) = value_of(&["/D", "-D"]) {
            intellisense.defines.push(define);
        } else if let Some(file) = value_of(&["/FI", "-include"]) {
            intellisense.forced_includes.push(absolute(&file));
        } else if let Some(standard) = word.strip_prefix("/std:").or_else(|| word.strip_prefix("-std:")) {
            intellisense.options.push(format!("/std:{standard}"));
        } else if let Some(standard) = word.strip_prefix("-std=") {
            intellisense.options.push(format!("/std:{}", cl_standard(standard)));
        }
    }

    intellisense
}

/// GCC's `-std=` as cl's `/std:` (IntelliSense reads cl's options): GNU dialects as the standard, the newest as latest.
fn cl_standard(standard: &str) -> String {
    let standard = standard.replace("gnu", "c");
    match standard.as_str() {
        "c++2b" | "c++23" | "c++2c" | "c++26" => "c++latest".into(),
        "c++2a" => "c++20".into(),
        "c++1z" => "c++17".into(),
        "c2x" | "c23" => "clatest".into(),
        "c18" => "c17".into(),
        _ => standard,
    }
}

/// The MSBuild item a project lists a file as.
fn source_item(source: &Path) -> &'static str {
    let extension = source
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "c" | "cc" | "cpp" | "cxx" | "c++" | "cp" | "cu" | "m" | "mm" => "ClCompile",
        "h" | "hh" | "hpp" | "hxx" | "h++" | "inl" | "inc" | "ipp" | "tpp" => "ClInclude",
        "rc" => "ResourceCompile",
        _ => "None",
    }
}

fn is_cxx(file: &str) -> bool {
    let extension = Path::new(file).extension().map(|extension| extension.to_string_lossy().to_lowercase());
    matches!(extension.as_deref(), Some("cpp" | "cc" | "cxx" | "c++" | "cp"))
}

/// A path or argument of this profile moved to another's output folder: `out/debug/x` becomes `out/release/x`.
fn relocate(text: &str, from: &Path, to: &Path) -> String {
    if from == to {
        return text.to_string();
    }
    text.replace(&slash(from), &slash(to))
}

/// `'$(Configuration)|$(Platform)'=='Debug|x64'`.
fn condition(configuration: &Configuration) -> String {
    format!("'$(Configuration)|$(Platform)'=='{}|{}'", configuration.name, configuration.platform)
}

/// Words as one command line: quoted when they hold spaces or quotes.
fn command_line(words: &[String]) -> String {
    let quoted: Vec<String> = words
        .iter()
        .map(|word| match word.is_empty() || word.contains([' ', '\t', '"']) {
            true => format!("\"{}\"", word.replace('"', "\\\"")),
            false => word.clone(),
        })
        .collect();
    quoted.join(" ")
}

fn windows_path(text: &str) -> String {
    text.replace('/', "\\")
}

/// Text for a property or item: MSBuild's special characters escaped (it would read `%`, `$`, `@` and `'`), then XML's.
fn property(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '%' => escaped.push_str("%25"),
            '$' => escaped.push_str("%24"),
            '@' => escaped.push_str("%40"),
            '\'' => escaped.push_str("%27"),
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

/// A GUID for a name, the same every time, so Visual Studio keeps its settings for the project.
fn guid(project: &Project, name: &str) -> String {
    let seed = format!("{}|{name}", slash(&project.root).to_lowercase());
    let high = util::hash_bytes(format!("{seed}|high").as_bytes());
    let low = util::hash_bytes(format!("{seed}|low").as_bytes());
    let hex = format!("{high:016X}{low:016X}");
    format!("{{{}-{}-{}-{}-{}}}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn reads_includes_definitions_and_standards() {
        let words = |text: &str| text.split(' ').map(str::to_string).collect::<Vec<_>>();
        let directory = Path::new("C:/p");

        let cl = read_arguments(
            &words("cl.exe /nologo /Iinclude /I C:/x /external:IC:/v/include /DA=1 /D B /std:c++20 /FIpch.h /c src/a.cpp"),
            directory,
        );
        assert_eq!(cl.includes, ["C:\\p\\include", "C:\\x", "C:\\v\\include"]);
        assert_eq!(cl.defines, ["A=1", "B"]);
        assert_eq!(cl.options, ["/std:c++20"]);
        assert_eq!(cl.forced_includes, ["C:\\p\\pch.h"]);

        let gcc = read_arguments(&words("g++ -Iinclude -isystem C:/v -DX -std=gnu++23 -c a.cpp"), directory);
        assert_eq!(gcc.includes, ["C:\\p\\include", "C:\\v"]);
        assert_eq!(gcc.defines, ["X"]);
        assert_eq!(gcc.options, ["/std:c++latest"]);
        assert_eq!(cl_standard("gnu17"), "c17");
    }

    #[test]
    fn escapes_what_msbuild_and_xml_read() {
        assert_eq!(property("A=\"x\" & 50% $(y) @z 'q'"), "A=&quot;x&quot; &amp; 50%25 %24(y) %40z %27q%27");
        assert_eq!(
            command_line(&["C:/a b/anvil.exe".into(), "build".into(), String::new()]),
            "\"C:/a b/anvil.exe\" build \"\""
        );
        assert_eq!(
            relocate("C:/p/out/debug/x.exe", Path::new("C:/p/out/debug"), Path::new("C:/p/out/release")),
            "C:/p/out/release/x.exe"
        );
    }
}
