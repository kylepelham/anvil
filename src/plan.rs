//! Turns the resolved targets into concrete actions: compiles (with precompiled headers and unity batches),
//! resource scripts, archives, links, build steps and DLL copies, plus compile_commands.json.

use crate::flags::{path_text, CompileSpec, DepsFormat, Language, LinkSpec};
use crate::manifest::{RunSpec, StrOrList};
use crate::profile::Opt;
use crate::project::Model;
use crate::source_hash::{self, HashInputs, HASH_START};
use crate::targets::{self, Kind, PackageScope, Target};
use crate::toolchain::{Family, Toolchain};
use crate::util::{self, slash};
use anyhow::{bail, Context, Result};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// The longest object file path, as CMake's CMAKE_OBJECT_PATH_MAX on Windows: tools fail past MAX_PATH (260)
const OBJECT_PATH_MAX: usize = 250;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Compile,
    Precompile,
    Resource,
    Assemble,
    Archive,
    Link,
    Run,
    Copy,
}

/// How a long command line goes into a response file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseStyle {
    Never,
    Msvc,
    Gnu,
}

#[derive(Debug, Clone)]
pub struct Action {
    pub kind: ActionKind,
    pub description: String,

    // The process
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Arc<Vec<(String, String)>>,
    /// A compiler cache the program runs through (`sccache cl ...`): left out of the hash and compile_commands.json,
    /// so turning it on or off rebuilds nothing.
    pub launcher: Option<PathBuf>,
    pub workdir: Option<PathBuf>,
    pub response: ResponseStyle,

    // Files: what it reads (besides discovered headers) and writes; the first output keys the database record
    pub inputs: Vec<PathBuf>,
    pub outputs: Vec<PathBuf>,
    pub deps: DepsFormat,

    /// Actions that must finish first.
    pub after: Vec<usize>,

    /// Delete outputs before running (ar appends to an existing archive).
    pub remove_outputs_first: bool,

    /// A line of output to drop (cl echoes the source file's name).
    pub echo_line: Option<String>,

    /// Copy actions: the file to copy (no process).
    pub copy_from: Option<PathBuf>,

    /// Files the action must create, with the error to show when it does not.
    pub must_create: Vec<(PathBuf, String)>,

    /// The outputs are stamps the action does not make itself: written once it succeeds.
    pub stamp: bool,

    /// An incremental link's state file (link.exe's .ilk).
    pub incremental_state: Option<PathBuf>,

    /// The arguments go on the command line as written, unquoted (cmd /c's line, on Windows).
    pub raw_args: bool,

    /// gcc and clang color their diagnostics: added when the process starts, so the hash and compile_commands.json
    /// do not depend on the terminal.
    pub color_diagnostics: bool,

    /// Everything about how the action runs (kind, program, arguments, folder, declared inputs); a change means the
    /// output is stale. Set once the action is planned.
    pub command_hash: u64,
}

pub struct Plan {
    pub actions: Vec<Action>,

    /// Each planned target's program or library file.
    pub artifacts: HashMap<usize, PathBuf>,

    /// Each linked DLL's (or exporting program's) import library, and the runtime files copied beside each program
    /// and shared library (DLLs of its dependencies, the sanitizer's).
    pub import_libs: HashMap<usize, PathBuf>,
    pub runtime_files: HashMap<usize, Vec<PathBuf>>,
}

/// Include folders, defines and flags a target compiles with.
#[derive(Default, Clone)]
struct CompileInputs {
    includes: Vec<PathBuf>,
    system_includes: Vec<PathBuf>,
    defines: Vec<String>,
    flags: Vec<String>,
    /// Flags for one language's files only.
    language_flags: Vec<(Language, String)>,
}

/// What linking a binary needs from its dependencies.
#[derive(Default)]
struct LinkInputs {
    objects: Vec<PathBuf>,
    libs: Vec<String>,
    /// Static libraries linked whole.
    whole: Vec<String>,
    lib_dirs: Vec<PathBuf>,
    flags: Vec<String>,
    files: Vec<PathBuf>,
    after: Vec<usize>,
    runtime_files: Vec<PathBuf>,
    /// It links against a program's exports (a module the program loads).
    program_exports: bool,
}

/// A target's precompiled header: the arguments users add, the file they read, and the action that makes it.
struct PrecompiledHeader {
    use_args: Vec<String>,
    file: PathBuf,
    action: usize,
}

struct Planner<'a> {
    model: &'a Model,
    toolchain: &'a Toolchain,
    env: Arc<Vec<(String, String)>>,
    color: bool,

    actions: Vec<Action>,
    compile_commands: Vec<CompileEntry>,

    // Per target: build-step actions, object files (with their actions), the file dependents link and its action
    generation: HashMap<usize, Vec<usize>>,
    objects: HashMap<usize, Vec<(PathBuf, usize)>>,
    link_artifact: HashMap<usize, (PathBuf, usize)>,
    artifacts: HashMap<usize, PathBuf>,
    import_libs: HashMap<usize, PathBuf>,
    runtime_files: HashMap<usize, Vec<PathBuf>>,

    // Runtime and data files already copied into the output folder: by destination, the file and whether it is data
    copies: HashMap<String, (PathBuf, bool)>,

    /// A cross build's programs that build steps run, built for this machine: name -> (program, its action).
    host_tools: HashMap<String, (PathBuf, usize)>,

    /// Files of the package build steps name in their commands, by step action: inputs once planning ends (see
    /// `add_named_files`).
    named_files: Vec<(usize, Vec<PathBuf>)>,
}

impl Action {
    /// Hashes everything about how the action runs, streamed (no command line text is built).
    fn compute_command_hash(&self) -> u64 {
        let mut hasher = xxhash_rust::xxh3::Xxh3::new();
        let mut field = |bytes: &[u8]| {
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };

        field(&[self.kind as u8]);
        field(self.program.as_os_str().as_encoded_bytes());
        for argument in &self.args {
            field(argument.as_bytes());
        }
        field(self.workdir.as_deref().unwrap_or(Path::new("")).as_os_str().as_encoded_bytes());
        field(self.copy_from.as_deref().unwrap_or(Path::new("")).as_os_str().as_encoded_bytes());
        field(&[self.raw_args as u8]);

        // Declared inputs as a set: a file a pattern newly matches makes the action stale
        let mut inputs: Vec<&[u8]> = self.inputs.iter().map(|input| input.as_os_str().as_encoded_bytes()).collect();
        inputs.sort_unstable();
        field(b"inputs");
        for input in inputs {
            field(input);
        }

        hasher.digest()
    }

    fn new(kind: ActionKind, description: String, program: PathBuf, env: Arc<Vec<(String, String)>>) -> Action {
        Action {
            kind,
            description,
            program,
            args: Vec::new(),
            env,
            launcher: None,
            workdir: None,
            response: ResponseStyle::Never,
            inputs: Vec::new(),
            outputs: Vec::new(),
            deps: DepsFormat::None,
            after: Vec::new(),
            remove_outputs_first: false,
            echo_line: None,
            copy_from: None,
            must_create: Vec::new(),
            stamp: false,
            incremental_state: None,
            raw_args: false,
            color_diagnostics: false,
            command_hash: 0,
        }
    }
}

/// Plans every action `roots` need (their dependencies, and programs their build steps run). In a cross build,
/// `host` is the project loaded for this machine: the programs build steps run come from it.
pub fn plan(model: &Model, host: Option<&Model>, roots: &[usize], color: bool) -> Result<Plan> {
    let mut planner = Planner::new(model, color);

    // Programs build steps run, built for this machine first
    if let Some(host) = host {
        let tools = step_programs(model, roots)?;
        let tool_roots = tools.iter().map(|name| host.find_target(name)).collect::<Result<Vec<_>>>()?;

        let mut host_planner = Planner::new(host, color);
        for target in host_planner.order(&tool_roots)? {
            host_planner
                .plan_target(target)
                .with_context(|| format!("planning target `{}` for this machine", host.targets[target].name))?;
        }
        for (name, index) in tools.iter().zip(tool_roots) {
            let program = host_planner.artifacts[&index].clone();
            let action = host_planner.link_artifact[&index].1;
            planner.host_tools.insert(name.clone(), (program, action));
        }
        planner.actions = host_planner.actions;
        planner.named_files = host_planner.named_files;
    }

    let order = planner.order(roots)?;
    for target in &order {
        planner
            .plan_target(*target)
            .with_context(|| format!("planning target `{}`", model.targets[*target].name))?;
    }
    planner.add_named_files();

    // compile_commands.json covers the whole project, not just what this build makes
    planner.write_compile_commands(&order)?;
    Ok(Plan {
        actions: planner.actions,
        artifacts: planner.artifacts,
        import_libs: planner.import_libs,
        runtime_files: planner.runtime_files,
    })
}

/// The settings `{hash:...}` may appear in: `hash_fields!(target)` borrows them, `hash_fields!(target, mut)` mutably.
macro_rules! hash_fields {
    ($target:expr $(, $mutable:tt)?) => {{
        let target = $target;
        let mut fields = vec![
            &$($mutable)? target.defines,
            &$($mutable)? target.public_defines,
            &$($mutable)? target.interface_defines,
            &$($mutable)? target.flags,
            &$($mutable)? target.public_flags,
            &$($mutable)? target.interface_flags,
            &$($mutable)? target.cuda_flags,
            &$($mutable)? target.link_flags,
        ];
        for rule in &$($mutable)? target.file_rules {
            fields.extend([&$($mutable)? rule.defines, &$($mutable)? rule.flags]);
        }
        fields
    }};
}

/// Replaces `{hash:...}` in every target's definitions and flags (crate::source_hash), once the model is complete.
pub fn expand_hashes(model: &mut Model) -> Result<()> {
    let toolchain = &model.toolchain;
    let compiler = format!("{:?} {} {}", toolchain.family, toolchain.version, toolchain.platform.name());

    // Each target with hashes, expanded in a copy while the planner reads the model
    let mut expanded = Vec::new();
    let planner = Planner::new(model, false);
    for (index, target) in model.targets.iter().enumerate() {
        if !hash_fields!(target)
            .iter()
            .any(|field| field.iter().any(|text| text.contains(HASH_START)))
        {
            continue;
        }

        let flags = std::cell::OnceCell::new();
        let flags_text = || flags.get_or_init(|| planner.hash_flags(index)).clone();
        let inputs = HashInputs {
            compiler: &compiler,
            flags: &flags_text,
            dir: &target.dir,
            root: &model.packages[target.package].root,
            out_root: &model.out_root,
        };

        let mut copy = target.clone();
        for field in hash_fields!(&mut copy, mut) {
            for text in field.iter_mut().filter(|text| text.contains(HASH_START)) {
                *text = source_hash::expand(text, &inputs).with_context(|| format!("target `{}`", target.name))?;
            }
        }
        expanded.push((index, copy));
    }

    for (index, target) in expanded {
        model.targets[index] = target;
    }
    Ok(())
}

/// Programs the build steps of `roots` and everything they need run (`{target:NAME}` of an executable in a step's
/// command): what a cross build must also build for this machine.
pub fn step_programs(model: &Model, roots: &[usize]) -> Result<Vec<String>> {
    let mut programs: Vec<String> = Vec::new();
    let mut visited = vec![false; model.targets.len()];
    let mut pending: Vec<usize> = roots.to_vec();

    while let Some(index) = pending.pop() {
        if std::mem::replace(&mut visited[index], true) {
            continue;
        }

        let target = &model.targets[index];
        pending.extend(target.deps.iter().map(|(dependency, _)| *dependency));
        pending.extend(target.header_deps.iter().chain(&target.needs).copied());

        // Executables a step runs; anything else a step names (a library it inspects) is the target machine's
        for words in step_commands(target) {
            for name in target_references(&words) {
                let referenced = model.find_target(&name)?;
                if model.targets[referenced].kind.is_executable() {
                    if !programs.contains(&name) {
                        programs.push(name);
                    }
                } else {
                    pending.push(referenced);
                }
            }
        }
    }

    Ok(programs)
}

/// The commands of a target's build steps: [[run]], pre-build and post-build.
fn step_commands(target: &Target) -> Vec<Vec<String>> {
    let steps = target.pre_build.iter().chain(&target.post_build).map(|step| step.command.to_command());
    target.runs.iter().map(|run| run.command.to_command()).chain(steps).collect()
}

/// The file a target produces, without planning anything (for `anvil run` and tests).
pub fn artifact_path(model: &Model, target: &Target) -> Option<PathBuf> {
    let toolchain = &model.toolchain;
    let (default_folder, (prefix, suffix)) = match target.kind {
        Kind::Exe | Kind::Test => (model.out_dir.clone(), toolchain.executable_affixes()),
        Kind::Shared => (model.out_dir.clone(), toolchain.shared_lib_affixes()),
        Kind::Static => (model.package_out_dir(target.package).join("lib"), toolchain.static_lib_affixes()),
        _ => return None,
    };
    let folder = target.output_dir.clone().unwrap_or(default_folder);

    // output-prefix and output-suffix replace the platform's
    let prefix = target.output_prefix.as_deref().unwrap_or(prefix);
    let suffix = target.output_suffix.as_deref().unwrap_or(suffix);

    Some(folder.join(format!("{prefix}{}{suffix}", target.output_name)))
}

/// `{target:NAME}` references in a command.
pub fn target_references(words: &[String]) -> Vec<String> {
    let mut names = Vec::new();

    for word in words {
        let mut rest = word.as_str();
        while let Some(start) = rest.find("{target:") {
            let after = &rest[start + 8..];
            let Some(end) = after.find('}') else { break };
            names.push(after[..end].to_string());
            rest = &after[end + 1..];
        }
    }

    names
}

/// What a listed source file is to the build.
enum SourceKind {
    C,
    Cxx,
    /// Objective-C and Objective-C++: compiled alone, never batched or given the C++ precompiled header.
    ObjC,
    Resource,
    Assembly,
    Cuda,
    /// Listed for IDEs (CMake lists headers with sources); not compiled.
    Header,
    /// A module-definition file: the exports of the DLL or program it links into.
    Definition,
    /// A Windows application manifest, embedded by the link.
    Manifest,
}

fn source_kind(source: &Path, family: crate::toolchain::Family) -> Result<SourceKind> {
    let extension = source
        .extension()
        .map(|extension| extension.to_string_lossy().into_owned())
        .unwrap_or_default();

    let kind = match extension.to_lowercase().as_str() {
        "c" => SourceKind::C,
        "cpp" | "cc" | "cxx" | "c++" | "cp" => SourceKind::Cxx,
        "m" | "mm" if !family.is_msvc_style() => SourceKind::ObjC,
        "m" | "mm" => bail!("{}: Objective-C needs clang or gcc (--toolchain clang)", slash(source)),
        "rc" => SourceKind::Resource,
        "asm" if family.is_msvc_style() => SourceKind::Assembly,
        "s" if !family.is_msvc_style() => SourceKind::Assembly,
        "cu" => SourceKind::Cuda,
        "h" | "hh" | "hpp" | "hxx" | "h++" | "inl" | "inc" | "ipp" | "tpp" => SourceKind::Header,
        "def" => SourceKind::Definition,
        "manifest" => SourceKind::Manifest,
        "cppm" | "ixx" | "ccm" | "cxxm" | "c++m" | "mpp" => bail!(
            "{}: C++20 modules are not supported yet; take it out of `sources` (its code can go in an ordinary header and source)",
            slash(source)
        ),
        _ => bail!(
            "{}: Anvil does not build .{extension} files (it builds C, C++, Objective-C, CUDA, .rc and assembly: .asm with MSVC, .s/.S otherwise; .def and .manifest files go to the link)",
            slash(source)
        ),
    };
    Ok(kind)
}

/// Replaces `{target:NAME}` with each target's artifact path.
pub fn substitute_targets(word: &str, model: &Model) -> Result<String> {
    let mut result = word.to_string();

    for name in target_references(&[word.to_string()]) {
        let index = model.find_target(&name)?;
        let path = referenced_file(model, &model.targets[index]).with_context(|| format!("{{target:{name}}}: `{name}` produces no file"))?;
        result = result.replace(&format!("{{target:{name}}}"), &path_text(&path));
    }

    Ok(result)
}

/// The file {target:NAME} stands for: what the target builds, or a package's library when it has exactly one.
fn referenced_file(model: &Model, target: &Target) -> Option<PathBuf> {
    if target.kind != Kind::External {
        return artifact_path(model, target);
    }

    match target.libs.as_slice() {
        [library] if Path::new(library).is_absolute() => Some(PathBuf::from(library)),
        _ => None,
    }
}

impl<'a> Planner<'a> {
    fn new(model: &'a Model, color: bool) -> Planner<'a> {
        Planner {
            model,
            toolchain: &model.toolchain,
            env: Arc::new(model.toolchain.env.clone()),
            color,
            actions: Vec::new(),
            compile_commands: Vec::new(),
            generation: HashMap::new(),
            objects: HashMap::new(),
            link_artifact: HashMap::new(),
            artifacts: HashMap::new(),
            import_libs: HashMap::new(),
            runtime_files: HashMap::new(),
            copies: HashMap::new(),
            host_tools: HashMap::new(),
            named_files: Vec::new(),
        }
    }

    /// Targets in build order: dependencies, and programs used by build steps, before their users.
    fn order(&self, roots: &[usize]) -> Result<Vec<usize>> {
        let model = self.model;
        let host_tools = &self.host_tools;
        let mut order = Vec::new();
        let mut state = vec![0u8; model.targets.len()];

        fn visit(
            model: &Model,
            host_tools: &HashMap<String, (PathBuf, usize)>,
            index: usize,
            state: &mut Vec<u8>,
            order: &mut Vec<usize>,
        ) -> Result<()> {
            match state[index] {
                2 => return Ok(()),
                1 => bail!(
                    "target `{}` needs itself to build (through a build step's {{target:...}})",
                    model.targets[index].name
                ),
                _ => {}
            }
            state[index] = 1;

            // Dependencies (header-only ones too), then targets its build steps run or its link flags name
            let target = &model.targets[index];
            let mut before: Vec<usize> = target.deps.iter().map(|(dependency, _)| *dependency).collect();
            before.extend(target.header_deps.iter().copied());
            before.extend(target.needs.iter().copied());
            for words in step_commands(target) {
                for name in target_references(&words).into_iter().filter(|name| !host_tools.contains_key(name)) {
                    before.push(model.find_target(&name)?);
                }
            }
            for name in target_references(&target.link_flags) {
                before.push(model.find_target(&name)?);
            }
            for dependency in before {
                visit(model, host_tools, dependency, state, order)?;
            }

            state[index] = 2;
            order.push(index);
            Ok(())
        }

        for root in roots {
            visit(model, host_tools, *root, &mut state, &mut order)?;
        }

        Ok(order)
    }

    fn plan_target(&mut self, index: usize) -> Result<()> {
        let target = &self.model.targets[index];

        // The export header, written only when it changes so nothing rebuilds for it
        if let Some((path, macro_name)) = &target.export_header {
            let file = targets::gen_dir(&self.model.package_out_dir(target.package), &target.name).join(path);
            util::write_if_changed(&file, crate::export_header::text(macro_name, "").as_bytes())?;
        }

        // Build steps first: what they generate may be compiled or included; pre-build commands gate the compiles
        let generated_sources = self.plan_runs(index)?;
        let pre_build = self.plan_steps(index, Stage::PreBuild, &[])?;
        self.generation.entry(index).or_default().extend(pre_build);
        if matches!(target.kind, Kind::External | Kind::Headers | Kind::Custom) {
            return Ok(());
        }

        // Compiles
        let mut sources = target.sources.clone();
        sources.extend(generated_sources);
        self.plan_compiles(index, &sources)?;

        // The binary
        match target.kind {
            Kind::Static => self.plan_archive(index)?,
            Kind::Exe | Kind::Test | Kind::Shared => self.plan_link(index)?,
            _ => {}
        }

        // Post-build commands run once the file is made; dependents wait for them
        let Some((_, link_action)) = self.link_artifact.get(&index).cloned() else {
            return Ok(());
        };
        let post_build = self.plan_steps(index, Stage::PostBuild, &[link_action])?;
        if let Some(last) = post_build.last() {
            self.link_artifact.get_mut(&index).unwrap().1 = *last;
        }

        Ok(())
    }

    /// Pre-build or post-build commands, in order. Each reruns when the target's sources (pre) or its file (post)
    /// change, or its command does; a stamp records that it ran.
    fn plan_steps(&mut self, index: usize, stage: Stage, first_after: &[usize]) -> Result<Vec<usize>> {
        let target = &self.model.targets[index];
        let (steps, stage_name, inputs) = match stage {
            Stage::PreBuild => (&target.pre_build, "pre-build", target.sources.clone()),
            Stage::PostBuild => (
                &target.post_build,
                "post-build",
                self.artifacts.get(&index).cloned().into_iter().collect(),
            ),
        };
        let scope = self.package_scope(index);

        let mut planned = Vec::new();
        let mut after = first_after.to_vec();
        for (number, step) in steps.iter().enumerate() {
            if let Some(condition) = &step.cond {
                if !self.model.condition.test(condition)? {
                    continue;
                }
            }

            // A run step whose output is the stamp
            let words = step
                .command
                .to_command()
                .iter()
                .map(|word| targets::expand_path_text(word, &scope, &target.name))
                .collect::<Result<Vec<_>>>()?;
            let run = RunSpec {
                command: step.command.clone(),
                shell: step.shell,
                message: step.message.clone(),
                workdir: step.workdir.clone(),
                ..Default::default()
            };
            let invocation = RunInvocation {
                input: None,
                inputs: inputs.clone(),
                outputs: vec![self.object_dir(target).join("steps").join(format!("{stage_name}-{number}.stamp"))],
                words,
            };

            let action = self
                .plan_run_invocation(index, &run, invocation)
                .with_context(|| format!("in a {stage_name} step of `{}`", target.name))?;
            self.actions[action].stamp = true;
            self.actions[action].after.extend(after.iter().copied());
            after = vec![action];
            planned.push(action);
        }

        Ok(planned)
    }

    /// What a target's placeholders resolve against.
    fn package_scope(&self, index: usize) -> PackageScope<'a> {
        let model = self.model;
        let package = &model.packages[model.targets[index].package];

        PackageScope {
            index: model.targets[index].package,
            name: &package.name,
            root: &package.root,
            out_dir: &model.out_dir,
            out_root: &model.out_root,
            std: "",
            c_std: "",
            version: package.version(),
            vcpkg_dir: &model.vcpkg_dir,
            cxx: &model.toolchain.cxx,
            cc: &model.toolchain.cc,
            condition: &model.condition,
        }
    }

    fn object_dir(&self, target: &Target) -> PathBuf {
        self.model
            .package_out_dir(target.package)
            .join("obj")
            .join(targets::sanitize_name(&target.name))
    }

    /// Generation actions of the target and everything it depends on (headers they write may be included), and the
    /// finished files of the targets they need built first.
    fn generation_barrier(&self, index: usize) -> Vec<usize> {
        let closure = self.model.closure(&[index]);
        let generation = closure
            .iter()
            .flat_map(|dependency| self.generation.get(dependency).cloned().unwrap_or_default());
        let needed = closure
            .iter()
            .flat_map(|dependency| &self.model.targets[*dependency].needs)
            .filter_map(|need| self.link_artifact.get(need).map(|(_, action)| *action));

        generation.chain(needed).collect()
    }

    fn plan_compiles(&mut self, index: usize, sources: &[PathBuf]) -> Result<()> {
        let target = &self.model.targets[index];
        let object_dir = self.object_dir(target);
        let inputs = self.compile_inputs(index);
        let barrier = self.generation_barrier(index);

        // Split sources: resources, assembly, files with their own settings, and the rest (which may batch and use the PCH)
        let has_rule = |source: &Path| target.file_rules.iter().any(|rule| rule.matcher.is_match(slash(source)));
        let mut resources = Vec::new();
        let mut assembly = Vec::new();
        let mut cuda = Vec::new();
        let mut plain_cxx = Vec::new();
        let mut others = Vec::new();
        for source in sources {
            match source_kind(source, self.toolchain.family)? {
                SourceKind::Header | SourceKind::Definition | SourceKind::Manifest => {}
                SourceKind::Resource => resources.push(source.clone()),
                SourceKind::Assembly => assembly.push(source.clone()),
                SourceKind::Cuda => cuda.push(source.clone()),
                SourceKind::Cxx if !has_rule(source) => plain_cxx.push(source.clone()),
                SourceKind::Cxx | SourceKind::C | SourceKind::ObjC => others.push(source.clone()),
            }
        }

        // Precompiled header for the plain C++ files
        let pch = match &target.pch {
            Some(header) if !plain_cxx.is_empty() => Some(self.plan_pch(index, header, &inputs, &barrier)?),
            _ => None,
        };

        // Unity batches replace the plain files they include
        let mut compile_list: Vec<(PathBuf, bool)> = Vec::new();
        match target.unity {
            Some(batch_size) if plain_cxx.len() > 1 => {
                for source in &plain_cxx {
                    self.add_compile_command(index, source, &inputs, pch.as_ref());
                }

                for (batch, chunk) in plain_cxx.chunks(batch_size).enumerate() {
                    let unity_file = object_dir.join("unity").join(format!("unity_{batch}.cpp"));
                    let content: String = chunk.iter().map(|source| format!("#include \"{}\"\n", slash(source))).collect();
                    util::write_if_changed(&unity_file, content.as_bytes())?;
                    compile_list.push((unity_file, true));
                }
            }
            _ => compile_list.extend(plain_cxx.iter().map(|source| (source.clone(), true))),
        }
        compile_list.extend(others.iter().map(|source| (source.clone(), false)));

        // One action per translation unit
        let mut objects: Vec<(PathBuf, usize)> = Vec::new();
        if let Some(pch) = &pch {
            if self.toolchain.family.is_msvc_style() {
                objects.push((self.actions[pch.action].outputs[0].clone(), pch.action));
            }
        }

        for (source, may_use_pch) in compile_list {
            let pch_for_file = if may_use_pch { pch.as_ref() } else { None };
            let action = self.plan_compile(index, &source, &inputs, pch_for_file, &barrier)?;
            objects.push((self.actions[action].outputs[0].clone(), action));
        }

        for source in resources {
            let action = self.plan_resource(index, &source, &inputs, &barrier)?;
            objects.push((self.actions[action].outputs[0].clone(), action));
        }

        for source in assembly {
            let action = self.plan_assemble(index, &source, &inputs, &barrier)?;
            objects.push((self.actions[action].outputs[0].clone(), action));
        }

        for source in cuda {
            let action = self.plan_cuda(index, &source, &inputs, &barrier)?;
            objects.push((self.actions[action].outputs[0].clone(), action));
        }

        self.objects.insert(index, objects);
        Ok(())
    }

    /// The settings one source file compiles with: the target's, with matching file rules applied.
    fn file_settings<'t>(&self, target: &'t Target, source: &Path, inputs: &'t CompileInputs) -> FileSettings<'t> {
        let profile = &self.model.profile;
        let extension = source
            .extension()
            .map(|extension| extension.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let language = match extension.as_str() {
            "c" => Language::C,
            "m" => Language::ObjC,
            "mm" => Language::ObjCxx,
            _ => Language::Cxx,
        };

        // The target's settings, borrowed until a file rule adds to them
        let mut settings = FileSettings {
            language,
            standard: if matches!(language, Language::C | Language::ObjC) {
                &target.c_std
            } else {
                &target.std
            },
            opt: profile.opt,
            warnings: target.warnings,
            werror: target.werror,
            arch: target.arch,
            defines: Cow::Borrowed(&inputs.defines),
            includes: &inputs.includes,
            system_includes: &inputs.system_includes,
            flags: Cow::Borrowed(&inputs.flags),
        };

        // Flags for this language's files only
        let mut language_flags = inputs
            .language_flags
            .iter()
            .filter(|(flag_language, _)| *flag_language == language)
            .peekable();
        if language_flags.peek().is_some() {
            settings.flags.to_mut().extend(language_flags.map(|(_, flag)| flag.clone()));
        }

        // File rules
        if target.file_rules.is_empty() {
            return settings;
        }
        let source_text = slash(source);
        for rule in target.file_rules.iter().filter(|rule| rule.matcher.is_match(&source_text)) {
            apply_file_rule(&mut settings, rule);
        }

        settings
    }

    fn compile_spec<'b>(&'b self, settings: &'b FileSettings, position_independent: bool) -> CompileSpec<'b> {
        CompileSpec {
            language: settings.language,
            standard: settings.standard,
            profile: &self.model.profile,
            opt: settings.opt,
            warnings: settings.warnings,
            werror: settings.werror,
            arch: settings.arch,
            defines: &settings.defines,
            includes: settings.includes,
            system_includes: settings.system_includes,
            flags: &settings.flags,
            position_independent,
        }
    }

    fn position_independent(&self, target: &Target) -> bool {
        matches!(target.kind, Kind::Static | Kind::Shared | Kind::Object)
    }

    /// Where a source's object goes: its path inside the target (or package, or output folder), mirrored. A path
    /// too long for Windows tools becomes `<hash>_<file name>` in the target's folder, as CMake's
    /// CMAKE_OBJECT_PATH_MAX does.
    fn object_path(&self, target: &Target, source: &Path, extension: &str) -> PathBuf {
        let package_root = &self.model.packages[target.package].root;
        let name = source.file_name().unwrap_or_default().to_string_lossy();
        let hashed_name = format!("{:08x}_{name}", util::hash_bytes(slash(source).as_bytes()) as u32);

        let relative = util::strip_prefix_ci(source, &target.dir)
            .or_else(|| util::strip_prefix_ci(source, package_root))
            .or_else(|| util::strip_prefix_ci(source, &self.model.out_dir))
            .map(|relative| slash(&relative).replace("../", "__/"))
            .unwrap_or_else(|| format!("external/{hashed_name}"));

        // Room for the dependency file's extension after it (.json)
        let object = self.object_dir(target).join(format!("{relative}.{extension}"));
        if path_text(&object).len() + ".json".len() <= OBJECT_PATH_MAX {
            return object;
        }
        self.object_dir(target).join(format!("{hashed_name}.{extension}"))
    }

    fn plan_compile(
        &mut self,
        index: usize,
        source: &Path,
        inputs: &CompileInputs,
        pch: Option<&PrecompiledHeader>,
        barrier: &[usize],
    ) -> Result<usize> {
        let target = &self.model.targets[index];
        let settings = self.file_settings(target, source, inputs);
        let spec = self.compile_spec(&settings, self.position_independent(target));

        // Object and header-list files
        let object = self.object_path(target, source, self.toolchain.object_extension());
        let deps_extension = if self.toolchain.family == Family::Msvc { "json" } else { "d" };
        let deps_file = PathBuf::from(format!("{}.{deps_extension}", object.display()));
        let (mut args, deps) = self.toolchain.compile_args(&spec, source, &object, &deps_file);

        // Precompiled header (C++ only)
        let mut after = barrier.to_vec();
        let mut inputs_read = vec![source.to_path_buf()];
        let pch = pch.filter(|_| settings.language == Language::Cxx);
        if let Some(pch) = pch {
            insert_before_source(&mut args, &pch.use_args, self.toolchain.family.is_msvc_style());
            after.push(pch.action);
            inputs_read.push(pch.file.clone());
        }

        let description = format!("Compiling {}", self.display_path(source));
        let program = self.toolchain.compiler(settings.language).to_path_buf();
        let mut action = Action::new(ActionKind::Compile, description, program, self.env.clone());
        action.args = args;
        action.inputs = inputs_read;
        action.outputs = vec![object];
        action.deps = deps;
        action.after = after;
        action.response = self.response_style();
        action.echo_line = self.compiler_echo(source);
        action.color_diagnostics = self.color_diagnostics();
        action.launcher = self.launcher(pch.is_some());

        self.compile_commands.push(CompileEntry {
            source: source.to_path_buf(),
            command: CompileCommand::Planned(self.actions.len()),
        });
        Ok(self.push(action))
    }

    /// The compiler cache a compile runs through: none for sccache with cl's precompiled headers (/Yc, /Yu), which
    /// it never caches.
    fn launcher(&self, uses_pch: bool) -> Option<PathBuf> {
        let launcher = self.model.launcher.as_ref()?;
        let sccache = launcher.file_stem().is_some_and(|stem| stem.eq_ignore_ascii_case("sccache"));
        if uses_pch && sccache && self.toolchain.family.is_msvc_style() {
            return None;
        }

        Some(launcher.clone())
    }

    /// A compile_commands.json entry for a file compiled inside a unity batch, as if compiled alone.
    fn add_compile_command(&mut self, index: usize, source: &Path, inputs: &CompileInputs, pch: Option<&PrecompiledHeader>) {
        let action = self.alone_compile_action(index, source, inputs, pch);
        self.compile_commands.push(CompileEntry {
            source: source.to_path_buf(),
            command: CompileCommand::Alone(Box::new(action)),
        });
    }

    /// The command compiling one file by itself, for compile_commands.json only.
    fn alone_compile_action(&self, index: usize, source: &Path, inputs: &CompileInputs, pch: Option<&PrecompiledHeader>) -> Action {
        let target = &self.model.targets[index];
        let settings = self.file_settings(target, source, inputs);
        let spec = self.compile_spec(&settings, self.position_independent(target));

        let object = self.object_path(target, source, self.toolchain.object_extension());
        let (mut args, _) = self.toolchain.compile_args(&spec, source, &object, &object.with_extension("d"));
        if let Some(pch) = pch {
            insert_before_source(&mut args, &pch.use_args, self.toolchain.family.is_msvc_style());
        }

        let program = self.toolchain.compiler(settings.language).to_path_buf();
        let mut action = Action::new(ActionKind::Compile, String::new(), program, self.env.clone());
        action.args = args;
        action.outputs = vec![object];
        action.command_hash = action.compute_command_hash();

        action
    }

    fn plan_pch(&mut self, index: usize, header: &Path, inputs: &CompileInputs, barrier: &[usize]) -> Result<PrecompiledHeader> {
        let target = &self.model.targets[index];
        let pch_dir = self.object_dir(target).join("pch");
        let header_name = header.file_name().unwrap().to_string_lossy().into_owned();

        let settings = self.file_settings(target, &PathBuf::from("pch.cpp"), inputs);
        let spec = self.compile_spec(&settings, self.position_independent(target));
        let description = format!("Precompiling {}", self.display_path(header));
        let mut action = Action::new(ActionKind::Precompile, description, self.toolchain.cxx.clone(), self.env.clone());
        action.after = barrier.to_vec();
        action.response = self.response_style();
        action.color_diagnostics = self.color_diagnostics();
        action.launcher = self.launcher(true);

        let (file, use_args) = match self.toolchain.family {
            // cl: compile a stub source with /Yc; its object links into the target
            Family::Msvc | Family::ClangCl => {
                let stub = pch_dir.join(format!("{header_name}.cpp"));
                util::write_if_changed(&stub, b"// Generated by Anvil: builds the precompiled header (forced include).\n")?;

                let pch_file = pch_dir.join(format!("{header_name}.pch"));
                let object = pch_dir.join(format!("{header_name}.{}", self.toolchain.object_extension()));
                let deps_file = pch_dir.join(format!("{header_name}.json"));
                let (mut args, deps) = self.toolchain.compile_args(&spec, &stub, &object, &deps_file);
                insert_before_source(&mut args, &self.toolchain.pch_args(header, &pch_file, true), true);

                action.args = args;
                action.deps = deps;
                action.inputs = vec![stub.clone(), header.to_path_buf()];
                action.outputs = vec![object, pch_file.clone()];
                action.echo_line = self.compiler_echo(&stub);
                let use_args = self.toolchain.pch_args(header, &pch_file, false);
                (pch_file, use_args)
            }

            // gcc: compile a stub header including the real one; users -include the stub and gcc picks its .gch
            Family::Gcc => {
                let stub = pch_dir.join(&header_name);
                util::write_if_changed(&stub, format!("#include \"{}\"\n", slash(header)).as_bytes())?;

                let gch = pch_dir.join(format!("{header_name}.gch"));
                let deps_file = pch_dir.join(format!("{header_name}.d"));
                action.args = self.toolchain.pch_header_args(&spec, &stub, &gch, &deps_file);
                action.deps = DepsFormat::Make(deps_file);
                action.inputs = vec![stub, header.to_path_buf()];
                action.outputs = vec![gch.clone()];
                let use_args = self.toolchain.pch_args(header, &gch, false);
                (gch, use_args)
            }

            Family::Clang => {
                let pch_file = pch_dir.join(format!("{header_name}.pch"));
                let deps_file = pch_dir.join(format!("{header_name}.d"));
                action.args = self.toolchain.pch_header_args(&spec, header, &pch_file, &deps_file);
                action.deps = DepsFormat::Make(deps_file);
                action.inputs = vec![header.to_path_buf()];
                action.outputs = vec![pch_file.clone()];
                let use_args = self.toolchain.pch_args(header, &pch_file, false);
                (pch_file, use_args)
            }
        };

        let action = self.push(action);
        Ok(PrecompiledHeader { use_args, file, action })
    }

    fn plan_resource(&mut self, index: usize, source: &Path, inputs: &CompileInputs, barrier: &[usize]) -> Result<usize> {
        let mut action = self.resource_action(index, source, inputs)?;
        action.after = barrier.to_vec();

        self.compile_commands.push(CompileEntry {
            source: source.to_path_buf(),
            command: CompileCommand::Planned(self.actions.len()),
        });
        Ok(self.push(action))
    }

    /// Compiling one resource script, without its ordering.
    fn resource_action(&self, index: usize, source: &Path, inputs: &CompileInputs) -> Result<Action> {
        let target = &self.model.targets[index];
        let compiler = self
            .toolchain
            .resource_compiler
            .clone()
            .context("this toolchain has no resource compiler (rc.exe, llvm-rc or windres) for .rc files")?;

        let output = self.object_path(target, source, self.toolchain.resource_output_extension());
        let description = format!("Compiling {}", self.display_path(source));
        let mut action = Action::new(ActionKind::Resource, description, compiler, self.env.clone());
        action.args = self.toolchain.resource_args(source, &output, &inputs.includes, &inputs.defines);
        action.inputs = vec![source.to_path_buf()];
        action.outputs = vec![output];
        action.workdir = source.parent().map(|folder| folder.to_path_buf());

        Ok(action)
    }

    /// Assembly: ml64 (armasm64) for .asm with MSVC-style toolchains, the compiler driver for .s and .S otherwise.
    fn plan_assemble(&mut self, index: usize, source: &Path, inputs: &CompileInputs, barrier: &[usize]) -> Result<usize> {
        let target = &self.model.targets[index];
        let assembler = self
            .toolchain
            .assembler()
            .context("no assembler found for this toolchain (ml64.exe beside the compiler, or on its PATH)")?;

        let output = self.object_path(target, source, self.toolchain.object_extension());
        let description = format!("Assembling {}", self.display_path(source));
        let mut action = Action::new(ActionKind::Assemble, description, assembler, self.env.clone());
        action.args = self.toolchain.assemble_args(source, &output, &inputs.includes, &inputs.defines);
        action.inputs = vec![source.to_path_buf()];
        action.outputs = vec![output];
        action.after = barrier.to_vec();

        Ok(self.push(action))
    }

    /// One .cu file through nvcc.
    fn plan_cuda(&mut self, index: usize, source: &Path, inputs: &CompileInputs, barrier: &[usize]) -> Result<usize> {
        let target = &self.model.targets[index];
        let Some(cuda) = self.model.cuda.as_ref() else {
            bail!("{}", self.model.cuda_error.as_deref().unwrap_or("the CUDA toolkit was not loaded"));
        };
        let settings = self.file_settings(target, source, inputs);
        let spec = self.compile_spec(&settings, self.position_independent(target));

        // Host-compiler flags are the C++ compiler's, not nvcc's: only defines, includes and cuda-flags go through
        let object = self.object_path(target, source, self.toolchain.object_extension());
        let depfile = PathBuf::from(format!("{}.d", object.display()));
        let mut args = cuda.compile_args(self.toolchain, &spec, &target.cuda_arch, &target.cuda_flags);
        args.extend(crate::cuda::Cuda::file_args(source, &object, &depfile));

        let description = format!("Compiling {}", self.display_path(source));
        let mut action = Action::new(ActionKind::Compile, description, cuda.nvcc.clone(), Arc::new(cuda.env.clone()));
        action.args = args;
        action.inputs = vec![source.to_path_buf()];
        action.outputs = vec![object];
        action.deps = DepsFormat::Make(depfile);
        action.after = barrier.to_vec();
        action.echo_line = Some(source.file_name().unwrap_or_default().to_string_lossy().into_owned());

        self.compile_commands.push(CompileEntry {
            source: source.to_path_buf(),
            command: CompileCommand::Planned(self.actions.len()),
        });
        Ok(self.push(action))
    }

    fn plan_archive(&mut self, index: usize) -> Result<()> {
        let target = &self.model.targets[index];
        let objects = self.objects.get(&index).cloned().unwrap_or_default();
        let library = artifact_path(self.model, target).unwrap();
        let object_paths: Vec<PathBuf> = objects.iter().map(|(object, _)| object.clone()).collect();

        let description = format!("Archiving {}", library.file_name().unwrap().to_string_lossy());
        let mut action = Action::new(ActionKind::Archive, description, self.toolchain.archiver.clone(), self.env.clone());
        action.args = self.toolchain.archive_args(&object_paths, &library, &self.model.profile);
        action.inputs = object_paths;
        action.outputs = vec![library.clone()];
        action.after = objects.iter().map(|(_, action)| *action).collect();
        action.response = self.response_style();
        action.remove_outputs_first = true;

        let action = self.push(action);
        self.link_artifact.insert(index, (library.clone(), action));
        self.artifacts.insert(index, library);

        Ok(())
    }

    fn plan_link(&mut self, index: usize) -> Result<()> {
        let target = &self.model.targets[index];
        let output = artifact_path(self.model, target).unwrap();
        let link = self.link_inputs(index);

        // Shared libraries on Windows produce an import library dependents link against, and so do programs that
        // export symbols to the modules they load
        let shared = target.kind == Kind::Shared;
        let import_lib = match shared || (target.exports && self.toolchain.targets_windows()) {
            true => self
                .toolchain
                .shared_lib_names(&targets::sanitize_name(&target.name))
                .1
                .map(|name| self.model.package_out_dir(target.package).join("lib").join(name)),
            false => None,
        };

        // Objects: the target's own, then object libraries'
        let own_objects = self.objects.get(&index).cloned().unwrap_or_default();
        let mut objects: Vec<PathBuf> = own_objects.iter().map(|(object, _)| object.clone()).collect();
        objects.extend(link.objects.iter().cloned());

        // Link flags may name other targets' files: {target:NAME}, which then link first
        let mut flags = Vec::new();
        let mut referenced_files = Vec::new();
        let mut referenced_actions = Vec::new();
        let profile_link_flags = self.profile_settings(index, &self.model.profile.link_flags);
        for flag in link.flags.iter().chain(&profile_link_flags) {
            for name in target_references(std::slice::from_ref(flag)) {
                let referenced = self.model.find_target(&name)?;
                referenced_files.extend(referenced_file(self.model, &self.model.targets[referenced]));
                referenced_actions.extend(self.link_artifact.get(&referenced).map(|(_, action)| *action));
            }
            flags.push(substitute_targets(flag, self.model)?);
        }

        // Module-definition files and manifests listed with the sources
        let (source_args, source_files) = self.source_link_args(target)?;
        flags.extend(source_args);

        let spec = LinkSpec {
            profile: &self.model.profile,
            shared,
            gui: target.gui,
            exports: target.exports,
            uses_program_exports: link.program_exports,
            objects: &objects,
            libs: &link.libs,
            whole: &link.whole,
            lib_dirs: &link.lib_dirs,
            flags: &flags,
            output: &output,
            import_lib: import_lib.as_deref(),
        };

        let description = format!("Linking {}", output.file_name().unwrap().to_string_lossy());
        let mut action = Action::new(ActionKind::Link, description, self.toolchain.linker.clone(), self.env.clone());
        action.args = self.toolchain.link_args(&spec);
        action.inputs = objects.clone();
        action.inputs.extend(link.files.iter().cloned());
        action.inputs.extend(target.link_depends.iter().cloned());
        action.inputs.extend(referenced_files);
        action.inputs.extend(source_files);
        action.outputs = vec![output.clone()];
        action.after = own_objects.iter().map(|(_, action)| *action).chain(link.after.iter().copied()).collect();
        action.after.extend(referenced_actions);
        action.response = self.response_style();
        if action.args.iter().any(|argument| argument == "/INCREMENTAL") {
            action.incremental_state = Some(output.with_extension("ilk"));
        }

        // A DLL (or exporting program) that exports nothing makes no import library: say why instead of failing later
        if let Some(import_lib) = &import_lib {
            self.import_libs.insert(index, import_lib.clone());
            action.outputs.push(import_lib.clone());
            let remedy = if shared { "or make it kind = \"lib\"" } else { "or drop exports = true" };
            let message = format!(
                "`{}` exports no symbols, so there is nothing to link against: mark its API __declspec(dllexport), {remedy}",
                target.name
            );
            action.must_create.push((import_lib.clone(), message));
        }

        let action = self.push(action);
        self.link_artifact.insert(index, (import_lib.unwrap_or_else(|| output.clone()), action));
        self.artifacts.insert(index, output.clone());

        // Runtime files (DLLs, the sanitizer's) beside every program
        if target.kind.is_executable() || shared {
            let folder = output.parent().unwrap().to_path_buf();
            let sanitizer_runtime = self.toolchain.sanitizer_runtime(&self.model.profile)?;
            for file in link.runtime_files.iter().chain(&sanitizer_runtime) {
                self.plan_copy_to(file, &folder.join(file.file_name().unwrap()), false)?;
            }
            let runtime_files = link.runtime_files.iter().chain(&sanitizer_runtime).cloned().collect();
            self.runtime_files.insert(index, runtime_files);

            // Data files, at their path under the target's folder
            for (file, relative) in &target.data {
                self.plan_copy_to(file, &folder.join(relative), true)?;
            }
        }

        Ok(())
    }

    /// Link arguments for the module-definition files and manifests among a target's sources, and those files (their
    /// change relinks). Windows only; GNU toolchains have no manifest step, so a manifest is left out there.
    fn source_link_args(&self, target: &Target) -> Result<(Vec<String>, Vec<PathBuf>)> {
        let mut args = Vec::new();
        let mut files = Vec::new();
        if !self.toolchain.targets_windows() {
            return Ok((args, files));
        }

        let msvc_style = self.toolchain.family.is_msvc_style();
        let msvc_abi = self.toolchain.is_msvc_abi();
        for source in &target.sources {
            let text = path_text(source);
            match source_kind(source, self.toolchain.family)? {
                SourceKind::Definition if msvc_style => args.push(format!("/DEF:{text}")),
                SourceKind::Definition if msvc_abi => args.push(format!("-Wl,/DEF:{text}")),
                SourceKind::Definition => args.push(text),
                SourceKind::Manifest if msvc_style => args.push(format!("/MANIFESTINPUT:{text}")),
                SourceKind::Manifest if msvc_abi => args.push(format!("-Wl,/MANIFESTINPUT:{text}")),
                _ => continue,
            }
            files.push(source.clone());
        }

        // Manifests are merged into the one embedded in the binary
        let has_manifest = args.iter().any(|argument| argument.contains("/MANIFESTINPUT:"));
        if has_manifest {
            args.push(if msvc_style { "/MANIFEST:EMBED" } else { "-Wl,/MANIFEST:EMBED" }.to_string());
        }

        Ok((args, files))
    }

    /// Copies a runtime or data file into the output folder once; a data file and another file for one destination
    /// are an error.
    fn plan_copy_to(&mut self, file: &Path, destination: &Path, data: bool) -> Result<()> {
        let destination = destination.to_path_buf();
        if let Some((other, other_data)) = self.copies.get(&util::path_key(&destination)) {
            if (data || *other_data) && util::path_key(other) != util::path_key(file) {
                let root = &self.model.root;
                let describe = |path: &Path, data: bool| format!("{} {}", if data { "data file" } else { "runtime file" }, util::rel(path, root));
                bail!(
                    "the {} and the {} would both be copied to {}; rename the data file, or move it to a subfolder",
                    describe(other, *other_data),
                    describe(file, data),
                    util::rel(&destination, root)
                );
            }
            return Ok(());
        }
        self.copies.insert(util::path_key(&destination), (file.to_path_buf(), data));

        let shown = util::strip_prefix_ci(&destination, &self.model.out_dir)
            .map(|relative| slash(&relative))
            .unwrap_or_else(|| destination.file_name().unwrap().to_string_lossy().into_owned());
        let description = format!("Copying {shown}");
        let mut action = Action::new(ActionKind::Copy, description, PathBuf::new(), self.env.clone());
        action.copy_from = Some(file.to_path_buf());
        action.inputs = vec![file.to_path_buf()];
        action.outputs = vec![destination];
        self.push(action);

        Ok(())
    }

    /// Build steps: returns the generated files that should be compiled.
    fn plan_runs(&mut self, index: usize) -> Result<Vec<PathBuf>> {
        let target = &self.model.targets[index];
        let scope = self.package_scope(index);

        let mut invocations = Vec::new();
        for run in &target.runs {
            if let Some(condition) = &run.cond {
                if !self.model.condition.test(condition)? {
                    continue;
                }
            }

            for invocation in self.expand_run(target, run, &scope)? {
                invocations.push((run.clone(), invocation));
            }
        }

        // Generated C and C++ files compile into the target
        let mut generated_sources = Vec::new();
        let mut actions = Vec::new();
        for (run, invocation) in invocations {
            for output in &invocation.outputs {
                let extension = output
                    .extension()
                    .map(|extension| extension.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if util::is_source_ext(&extension) {
                    generated_sources.push(output.clone());
                }
            }
            actions.push(self.plan_run_invocation(index, &run, invocation)?);
        }

        self.generation.insert(index, actions);
        Ok(generated_sources)
    }

    /// One run spec into its invocations: one per `for-each` match, or a single one.
    fn expand_run(&self, target: &Target, run: &RunSpec, scope: &PackageScope) -> Result<Vec<RunInvocation>> {
        let expand = |text: &str| -> Result<String> { targets::expand_path_text(text, scope, &target.name) };

        let output_templates: Vec<String> = run.output.iter().chain(run.outputs.iter()).cloned().collect();
        if output_templates.is_empty() {
            bail!("a [[run]] step needs `output` (or `outputs`) so Anvil knows when it is up to date");
        }

        // Extra inputs
        let extra_patterns: Vec<String> = run.inputs.iter().map(|input| expand(input)).collect::<Result<_>>()?;
        let extra_inputs = targets::expand_patterns(&target.dir, &extra_patterns, &self.model.out_root, true)?;

        // The files it runs over
        let each: Vec<Option<PathBuf>> = match &run.for_each {
            Some(patterns) => {
                let patterns: Vec<String> = patterns.to_vec().iter().map(|pattern| expand(pattern)).collect::<Result<_>>()?;
                let files = targets::expand_patterns(&target.dir, &patterns, &self.model.out_root, true)?;
                files.into_iter().map(Some).collect()
            }
            None => vec![None],
        };

        let mut invocations = Vec::new();
        for input in each {
            let substitute = |text: &str| -> Result<String> {
                let mut text = text.to_string();
                if let Some(input) = &input {
                    text = text
                        .replace("{input}", &path_text(input))
                        .replace("{stem}", &input.file_stem().unwrap_or_default().to_string_lossy())
                        .replace("{filename}", &input.file_name().unwrap_or_default().to_string_lossy());
                }
                expand(&text)
            };

            let mut outputs = Vec::new();
            for template in &output_templates {
                outputs.push(util::absolute(&target.dir, Path::new(&substitute(template)?)));
            }

            let mut inputs: Vec<PathBuf> = input.iter().cloned().collect();
            inputs.extend(extra_inputs.iter().cloned());

            let words = run.command.to_command().iter().map(|word| substitute(word)).collect::<Result<_>>()?;
            invocations.push(RunInvocation {
                input,
                inputs,
                outputs,
                words,
            });
        }

        Ok(invocations)
    }

    fn plan_run_invocation(&mut self, index: usize, run: &RunSpec, invocation: RunInvocation) -> Result<usize> {
        let target = &self.model.targets[index];
        let depfile_name = format!("{:016x}.d", util::hash_bytes(slash(&invocation.outputs[0]).as_bytes()));
        let depfile = self.object_dir(target).join("run").join(depfile_name);
        let workdir = match &run.workdir {
            Some(workdir) => {
                let workdir = targets::expand_path_text(workdir, &self.package_scope(index), &target.name)?;
                util::absolute(&target.dir, Path::new(&workdir))
            }
            None => target.dir.clone(),
        };

        // {inputs}, {output}, {outputs}, {depfile}; {target:X} becomes a path and an ordering edge
        let mut words = Vec::new();
        let mut after = Vec::new();
        let mut extra_inputs = Vec::new();
        let mut named_files = Vec::new();
        for word in &invocation.words {
            match word.as_str() {
                "{inputs}" => words.extend(invocation.inputs.iter().map(|input| path_text(input))),
                "{outputs}" => words.extend(invocation.outputs.iter().map(|output| path_text(output))),
                _ => {
                    let names_build_files = ["{input}", "{output}", "{depfile}", "{target:"]
                        .iter()
                        .any(|placeholder| word.contains(placeholder));
                    let mut word = word
                        .replace("{output}", &path_text(&invocation.outputs[0]))
                        .replace("{depfile}", &path_text(&depfile));
                    if let Some(first) = invocation.inputs.first() {
                        word = word.replace("{input}", &path_text(first));
                    }

                    for name in target_references(std::slice::from_ref(&word)) {
                        if let Some((program, action)) = self.host_tools.get(&name) {
                            after.push(*action);
                            extra_inputs.push(program.clone());
                            word = word.replace(&format!("{{target:{name}}}"), &path_text(program));
                            continue;
                        }

                        let referenced = self.model.find_target(&name)?;
                        let artifact =
                            artifact_path(self.model, &self.model.targets[referenced]).context("{target:...} names a target without a file")?;
                        if let Some((_, action)) = self.link_artifact.get(&referenced) {
                            after.push(*action);
                        }
                        extra_inputs.push(artifact);
                    }

                    // A file of the package the command names (a script it runs) re-runs it when it changes
                    if !names_build_files {
                        named_files.extend(self.package_file(index, &workdir, &word, &invocation.outputs));
                    }
                    words.push(substitute_targets(&word, self.model)?);
                }
            }
        }

        if words.is_empty() {
            bail!("a [[run]] step has an empty command");
        }

        // The program: through the shell, Anvil itself for `cmake -E`, a path, or a name on PATH
        let anvil = std::env::current_exe().context("finding Anvil's own program")?;
        let (program, args) = if run.shell {
            // A command line runs as written, placeholders filled in; a list's words are quoted as needed
            let text = match &run.command {
                StrOrList::Str(line) => self.shell_line(index, line, &invocation, &depfile)?,
                StrOrList::List(_) => words.iter().map(|word| shell_quote(word)).collect::<Vec<_>>().join(" "),
            };
            let text = crate::cmake_tool::rewrite_shell(&text, &anvil);

            // cmd /s /c "...": cmd drops the outer quotes and keeps the rest as is (the arguments go unquoted)
            if cfg!(windows) {
                let args = vec!["/d".to_string(), "/s".to_string(), "/c".to_string(), format!("\"{text}\"")];
                (PathBuf::from("cmd.exe"), args)
            } else {
                (PathBuf::from("sh"), vec!["-c".to_string(), text])
            }
        } else if let Some(cmake_args) = crate::cmake_tool::handled(&words) {
            let mut args = vec![crate::cmake_tool::SUBCOMMAND.to_string()];
            args.extend(cmake_args.iter().cloned());
            (anvil, args)
        } else {
            let first = &words[0];
            let program = if first.contains('/') || first.contains('\\') {
                util::absolute(&target.dir, Path::new(first))
            } else {
                util::which(first).with_context(|| format!("program `{first}` (used by a [[run]] step) is not on PATH"))?
            };
            (program, words[1..].to_vec())
        };

        let program_name = match crate::cmake_tool::handled(&words) {
            Some(_) => "cmake".to_string(),
            None => program.file_stem().unwrap_or_default().to_string_lossy().into_owned(),
        };
        let description = match (&run.message, &invocation.input) {
            (Some(message), _) => message.clone(),
            (None, Some(input)) => format!("Running {program_name} {}", input.file_name().unwrap_or_default().to_string_lossy()),
            (None, None) => format!("Running {program_name}"),
        };

        let mut action = Action::new(ActionKind::Run, description, program, self.env.clone());
        action.args = args;
        action.inputs = invocation.inputs;
        action.inputs.extend(extra_inputs);
        action.outputs = invocation.outputs;
        action.after = after;
        action.workdir = Some(workdir);
        action.raw_args = run.shell && cfg!(windows);
        if run.depfile {
            action.deps = DepsFormat::Make(depfile);
        }

        // A step that succeeds without writing an output would run again every build
        for output in &action.outputs {
            let message = format!(
                "{}: the step succeeded but did not write {}: make its command write every file its `output` or `outputs` names",
                action.description,
                util::rel(output, &self.model.root)
            );
            action.must_create.push((output.clone(), message));
        }

        // Named files are left out of the command hash: they are stamped like any input once they are added
        let action = self.push(action);
        if !named_files.is_empty() {
            self.named_files.push((action, named_files));
        }

        Ok(action)
    }

    /// Adds the files build steps name to their inputs, now that every action is planned, and orders each step after
    /// the actions that write its inputs (a step's output in the package). A named file nothing writes must exist.
    fn add_named_files(&mut self) {
        let has_step_inputs = self
            .actions
            .iter()
            .any(|action| action.kind == ActionKind::Run && !action.inputs.is_empty());
        if self.named_files.is_empty() && !has_step_inputs {
            return;
        }

        let mut producers: HashMap<String, usize> = HashMap::new();
        for (index, action) in self.actions.iter().enumerate() {
            for output in &action.outputs {
                producers.insert(util::path_key(output), index);
            }
        }

        // Named files (one written by a step that waits for this one would be read before it is made)
        for (action, files) in std::mem::take(&mut self.named_files) {
            for file in files {
                let usable = match producers.get(&util::path_key(&file)) {
                    Some(producer) => *producer != action && !self.depends_on(*producer, action),
                    None => file.is_file(),
                };
                if usable && !self.actions[action].inputs.contains(&file) {
                    self.actions[action].inputs.push(file);
                }
            }
        }

        // Steps after what makes their inputs (never into a cycle)
        for action in 0..self.actions.len() {
            if self.actions[action].kind != ActionKind::Run {
                continue;
            }

            for position in 0..self.actions[action].inputs.len() {
                let Some(producer) = producers.get(&util::path_key(&self.actions[action].inputs[position])).copied() else {
                    continue;
                };
                let ordered = self.actions[action].after.contains(&producer);
                if producer != action && !ordered && !self.depends_on(producer, action) {
                    self.actions[action].after.push(producer);
                }
            }
        }
    }

    /// Whether an action waits, directly or not, for another.
    fn depends_on(&self, action: usize, other: usize) -> bool {
        let mut visited = HashSet::new();
        let mut pending = vec![action];

        while let Some(index) = pending.pop() {
            if index == other {
                return true;
            }
            if visited.insert(index) {
                pending.extend(self.actions[index].after.iter().copied());
            }
        }

        false
    }

    /// The file a word of a step's command names, when it is one of the package's own (not the build's, not one the
    /// step writes): a script the step runs, a table it reads. Whether it exists is checked once planning ends.
    fn package_file(&self, index: usize, workdir: &Path, word: &str, outputs: &[PathBuf]) -> Option<PathBuf> {
        if !word.contains(['/', '\\']) || word.starts_with('-') {
            return None;
        }

        let file = util::absolute(workdir, Path::new(word));
        let root = &self.model.packages[self.model.targets[index].package].root;
        let in_package = util::strip_prefix_ci(&file, root).is_some() && util::strip_prefix_ci(&file, &self.model.out_root).is_none();
        let written = outputs.iter().any(|output| util::path_key(output) == util::path_key(&file));
        if !in_package || written {
            return None;
        }

        Some(file)
    }

    /// A `shell = true` command line as written, with its placeholders filled in (see `fill_shell_line`).
    fn shell_line(&self, index: usize, line: &str, invocation: &RunInvocation, depfile: &Path) -> Result<String> {
        let target = &self.model.targets[index];
        let scope = self.package_scope(index);
        let line = util::expand_env(line)?;

        // The package's placeholders
        let mut values = Vec::new();
        for (placeholder, is_path) in SCOPE_PLACEHOLDERS {
            let value = targets::expand_placeholders(placeholder, &scope, &target.name);
            values.push(ShellValue::new(placeholder, vec![value], *is_path));
        }

        // The step's files
        let paths = |files: &[PathBuf]| files.iter().map(|file| path_text(file)).collect::<Vec<_>>();
        values.push(ShellValue::new("{inputs}", paths(&invocation.inputs), true));
        values.push(ShellValue::new("{outputs}", paths(&invocation.outputs), true));
        values.push(ShellValue::new("{output}", paths(&invocation.outputs[..1]), true));
        values.push(ShellValue::new("{depfile}", vec![path_text(depfile)], true));
        if let Some(first) = invocation.inputs.first() {
            values.push(ShellValue::new("{input}", vec![path_text(first)], true));
        }
        if let Some(input) = &invocation.input {
            let stem = input.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            let filename = input.file_name().unwrap_or_default().to_string_lossy().into_owned();
            values.push(ShellValue::new("{stem}", vec![stem], false));
            values.push(ShellValue::new("{filename}", vec![filename], false));
        }

        // Other targets' files: this machine's build of a program a cross build runs, or the target's file
        for name in target_references(&[line.clone()]) {
            let placeholder = format!("{{target:{name}}}");
            let value = match self.host_tools.get(&name) {
                Some((program, _)) => path_text(program),
                None => substitute_targets(&placeholder, self.model)?,
            };
            values.push(ShellValue::new(&placeholder, vec![value], true));
        }

        Ok(fill_shell_line(&line, &values, cfg!(windows)))
    }

    /// What `flags` in a target's `{hash:...}` stands for: the arguments its C++ files compile with, without include
    /// folders, file names and the hashes themselves, and with the package root as `{root}`.
    fn hash_flags(&self, index: usize) -> String {
        let target = &self.model.targets[index];
        let mut inputs = self.compile_inputs(index);
        inputs.includes.clear();
        inputs.system_includes.clear();
        inputs.defines.retain(|define| !define.contains(HASH_START));
        inputs.flags.retain(|flag| !flag.contains(HASH_START));

        // The target's files, then those of each file rule (what it matches, and how they compile)
        let source = Path::new("hash.cpp");
        let settings = self.file_settings(target, source, &inputs);
        let arguments = |settings: &FileSettings| {
            let spec = self.compile_spec(settings, self.position_independent(target));
            let (args, _) = self.toolchain.compile_args(&spec, source, Path::new("hash.obj"), Path::new("hash.d"));
            args.join("\n")
        };
        let mut text = arguments(&settings);
        for rule in &target.file_rules {
            let mut rule_settings = settings.clone();
            apply_file_rule(&mut rule_settings, rule);
            rule_settings.defines.to_mut().retain(|define| !define.contains(HASH_START));
            rule_settings.flags.to_mut().retain(|flag| !flag.contains(HASH_START));
            text.push_str(&format!("\nfiles {}\n{}", rule.patterns.join(","), arguments(&rule_settings)));
        }

        let root = slash(&self.model.packages[target.package].root);
        text.replace(&root, "{root}")
    }

    /// Include folders, defines and flags: the target's own, plus what its dependencies export.
    fn compile_inputs(&self, index: usize) -> CompileInputs {
        let target = &self.model.targets[index];
        let mut inputs = CompileInputs::default();

        // Own settings
        inputs.includes.extend(target.private_include.iter().cloned());
        inputs.includes.extend(target.include.iter().cloned());
        if !target.runs.is_empty() {
            inputs
                .includes
                .push(targets::gen_dir(&self.model.package_out_dir(target.package), &target.name));
        }
        inputs.system_includes.extend(target.system_include.iter().cloned());
        inputs.system_includes.extend(target.public_system_include.iter().cloned());
        inputs.defines.extend(self.profile_settings(index, &self.model.profile.defines));
        inputs.defines.extend(target.defines.iter().cloned());
        inputs.defines.extend(target.public_defines.iter().cloned());
        inputs.flags.extend(target.flags.iter().cloned());
        inputs.flags.extend(target.public_flags.iter().cloned());
        inputs.language_flags.extend(target.public_language_flags.iter().cloned());

        // Every direct dependency's exports (and through its public dependencies, theirs), header-only uses too
        let mut visited = HashSet::new();
        for (dependency, _) in &target.deps {
            self.add_exports(*dependency, &mut inputs, &mut visited);
        }
        for dependency in &target.header_deps {
            self.add_exports(*dependency, &mut inputs, &mut visited);
        }
        inputs.flags.extend(self.profile_settings(index, &self.model.profile.flags));

        dedup_in_order(&mut inputs.includes, |path| util::path_key(path));
        dedup_in_order(&mut inputs.system_includes, |path| util::path_key(path));
        dedup_in_order(&mut inputs.defines, |define| define.clone());

        inputs
    }

    /// A profile's definitions or flags with a target's placeholders filled in (`{root}` is its package's).
    fn profile_settings(&self, index: usize, list: &[String]) -> Vec<String> {
        let scope = self.package_scope(index);
        let name = &self.model.targets[index].name;

        list.iter().map(|text| targets::expand_placeholders(text, &scope, name)).collect()
    }

    fn add_exports(&self, index: usize, inputs: &mut CompileInputs, visited: &mut HashSet<usize>) {
        if !visited.insert(index) {
            return;
        }

        let target = &self.model.targets[index];
        inputs.includes.extend(target.include.iter().cloned());
        inputs.includes.extend(target.interface_include.iter().cloned());
        if !target.runs.is_empty() {
            inputs
                .includes
                .push(targets::gen_dir(&self.model.package_out_dir(target.package), &target.name));
        }
        inputs.system_includes.extend(target.public_system_include.iter().cloned());
        inputs.defines.extend(target.public_defines.iter().cloned());
        inputs.defines.extend(target.interface_defines.iter().cloned());
        inputs.flags.extend(target.public_flags.iter().cloned());
        inputs.flags.extend(target.interface_flags.iter().cloned());
        inputs.language_flags.extend(target.public_language_flags.iter().cloned());

        for (dependency, public) in &target.deps {
            if *public {
                self.add_exports(*dependency, inputs, visited);
            }
        }
    }

    /// Everything a binary links: dependencies in link order (users before what they use), each library once.
    fn link_inputs(&self, index: usize) -> LinkInputs {
        let mut order = Vec::new();
        let mut visited = HashSet::new();
        self.visit_link(index, index, &mut visited, &mut order);
        order.reverse();

        let mut link = LinkInputs::default();
        for dependency in order {
            let target = &self.model.targets[dependency];

            // The dependency's own file
            if dependency != index {
                match target.kind {
                    Kind::Static | Kind::Shared => {
                        if let Some((file, action)) = self.link_artifact.get(&dependency) {
                            let whole = self.model.targets[index].whole_archive.contains(&dependency);
                            if whole { &mut link.whole } else { &mut link.libs }.push(path_text(file));
                            link.files.push(file.clone());
                            link.after.push(*action);
                        }
                    }
                    Kind::Object => {
                        for (object, action) in self.objects.get(&dependency).into_iter().flatten() {
                            link.objects.push(object.clone());
                            link.after.push(*action);
                        }
                    }
                    // A program that exports symbols is linked through its import library (Windows); others only order
                    Kind::Exe | Kind::Test => {
                        if let Some((file, action)) = self.link_artifact.get(&dependency) {
                            if target.exports && self.toolchain.targets_windows() {
                                link.libs.push(path_text(file));
                                link.files.push(file.clone());
                            }
                            link.program_exports |= target.exports;
                            link.after.push(*action);
                        }
                    }
                    _ => {}
                }
            }

            // Its system libraries (a package's, linked whole when asked), folders, flags and DLLs
            let whole = target.kind == Kind::External && self.model.targets[index].whole_archive.contains(&dependency);
            if whole { &mut link.whole } else { &mut link.libs }.extend(target.libs.iter().cloned());

            // Prebuilt library files (and packages') relink when they change
            link.files.extend(target.libs.iter().map(PathBuf::from).filter(|lib| lib.is_absolute()));
            link.lib_dirs.extend(target.lib_dirs.iter().cloned());
            link.flags.extend(target.link_flags.iter().cloned());
            link.runtime_files.extend(target.runtime_files.iter().cloned());
        }

        // Generation steps of anything linked must finish too
        link.after.extend(self.generation_barrier(index));

        dedup_libs(&mut link.libs);
        dedup_in_order(&mut link.lib_dirs, |path| util::path_key(path));
        dedup_in_order(&mut link.runtime_files, |path| util::path_key(path));

        link
    }

    fn visit_link(&self, index: usize, root: usize, visited: &mut HashSet<usize>, order: &mut Vec<usize>) {
        if !visited.insert(index) {
            return;
        }

        // A shared library or a program (other than the one being linked) already contains its private dependencies
        let target = &self.model.targets[index];
        let closed = matches!(target.kind, Kind::Shared | Kind::Exe | Kind::Test) && index != root;
        for (dependency, public) in &target.deps {
            if *public || !closed {
                self.visit_link(*dependency, root, visited, order);
            }
        }

        order.push(index);
    }

    fn push(&mut self, mut action: Action) -> usize {
        action.command_hash = action.compute_command_hash();
        self.actions.push(action);
        self.actions.len() - 1
    }

    /// gcc and clang color diagnostics when Anvil's own output is colored (cl-style compilers never do).
    fn color_diagnostics(&self) -> bool {
        self.color && !self.toolchain.family.is_msvc_style()
    }

    fn response_style(&self) -> ResponseStyle {
        if self.toolchain.family.is_msvc_style() {
            ResponseStyle::Msvc
        } else {
            ResponseStyle::Gnu
        }
    }

    /// cl and clang-cl print the source file's name; that line is noise.
    fn compiler_echo(&self, source: &Path) -> Option<String> {
        if !self.toolchain.family.is_msvc_style() {
            return None;
        }
        Some(source.file_name().unwrap_or_default().to_string_lossy().into_owned())
    }

    fn display_path(&self, path: &Path) -> String {
        if util::strip_prefix_ci(path, &self.model.out_dir).is_some() {
            return path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        }
        util::rel(path, &self.model.root)
    }

    /// The project's own targets this build does not make (tests, targets not asked for): compile_commands.json
    /// covers their files too.
    fn unplanned_targets(&self, planned: &[usize]) -> Vec<usize> {
        let model = self.model;
        let planned: HashSet<usize> = planned.iter().copied().collect();

        (0..model.targets.len())
            .filter(|index| {
                let target = &model.targets[*index];
                let compiles = !matches!(target.kind, Kind::External | Kind::Headers | Kind::Custom);
                compiles && !planned.contains(index) && model.packages[target.package].member
            })
            .collect()
    }

    /// The command of one file of a target this build does not make, when the file is compiled.
    fn unplanned_action(&self, index: usize, source: &Path, inputs: &CompileInputs) -> Option<Action> {
        match source_kind(source, self.toolchain.family) {
            Ok(SourceKind::C | SourceKind::Cxx | SourceKind::ObjC) => Some(self.alone_compile_action(index, source, inputs, None)),
            Ok(SourceKind::Resource) => self.resource_action(index, source, inputs).ok(),
            _ => None,
        }
    }

    /// Stands for an unplanned target's entries without making each one: its files, and the command of one file of
    /// each kind (extension and the file rules that apply), from which the others differ only by their own paths.
    fn unplanned_fingerprint(&self, index: usize, fingerprint: &mut Vec<u8>) {
        let target = &self.model.targets[index];
        let inputs = self.compile_inputs(index);

        let mut kinds_seen = HashSet::new();
        for source in &target.sources {
            let mut kind = source
                .extension()
                .map(|extension| extension.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if !target.file_rules.is_empty() {
                let source_text = slash(source);
                for (number, rule) in target.file_rules.iter().enumerate() {
                    if rule.matcher.is_match(&source_text) {
                        kind.push_str(&format!(",{number}"));
                    }
                }
            }
            fingerprint.extend(util::hash_bytes(source.as_os_str().as_encoded_bytes()).to_le_bytes());
            fingerprint.extend(util::hash_bytes(kind.as_bytes()).to_le_bytes());

            // The first file of its kind
            if !kinds_seen.insert(kind) {
                continue;
            }
            if let Some(action) = self.unplanned_action(index, source, &inputs) {
                fingerprint.extend(entry_hash(source, &action).to_le_bytes());
            }
        }
    }

    /// Entries for every file of the targets this build does not make.
    fn add_unplanned_compile_commands(&mut self, unplanned: &[usize]) {
        let model = self.model;

        for index in unplanned {
            let inputs = self.compile_inputs(*index);
            for source in &model.targets[*index].sources {
                if let Some(action) = self.unplanned_action(*index, source, &inputs) {
                    self.compile_commands.push(CompileEntry {
                        source: source.clone(),
                        command: CompileCommand::Alone(Box::new(action)),
                    });
                }
            }
        }
    }

    fn compile_action<'s>(&'s self, entry: &'s CompileEntry) -> &'s Action {
        match &entry.command {
            CompileCommand::Planned(index) => &self.actions[*index],
            CompileCommand::Alone(action) => action,
        }
    }

    /// compile_commands.json in the profile's folder and the output root (for clangd and editors), covering the targets
    /// this build does not make too. It is only rewritten when a command changed: a fingerprint of the commands, kept
    /// beside the build database, says whether one did; the unplanned targets' entries are made only then.
    fn write_compile_commands(&mut self, planned: &[usize]) -> Result<()> {
        let unplanned = self.unplanned_targets(planned);

        // Unchanged since the last write: nothing to do
        let files = [
            self.model.out_dir.join("compile_commands.json"),
            self.model.out_root.join("compile_commands.json"),
        ];
        let mut fingerprint = Vec::with_capacity(self.compile_commands.len() * 8);
        for entry in &self.compile_commands {
            fingerprint.extend(entry_hash(&entry.source, self.compile_action(entry)).to_le_bytes());
        }
        for index in &unplanned {
            self.unplanned_fingerprint(*index, &mut fingerprint);
        }
        let fingerprint = format!("{:016x}", util::hash_bytes(&fingerprint));
        let stamp_file = self.model.out_dir.join(".anvil").join("compile_commands.stamp");
        let unchanged = std::fs::read_to_string(&stamp_file).ok().as_deref() == Some(fingerprint.as_str());
        if unchanged && files.iter().all(|file| file.is_file()) {
            return Ok(());
        }

        self.add_unplanned_compile_commands(&unplanned);
        let entries: Vec<serde_json::Value> = self
            .compile_commands
            .iter()
            .map(|entry| {
                let action = self.compile_action(entry);
                let mut arguments = vec![path_text(&action.program)];
                arguments.extend(action.args.iter().cloned());
                serde_json::json!({
                    "directory": path_text(action.workdir.as_deref().unwrap_or(&self.model.root)),
                    "file": path_text(&entry.source),
                    "arguments": arguments,
                    "output": path_text(&action.outputs[0]),
                })
            })
            .collect();
        let text = serde_json::to_string_pretty(&entries)? + "\n";
        for file in &files {
            util::write_if_changed(file, text.as_bytes())?;
        }
        util::write_atomic(&stamp_file, fingerprint.as_bytes())?;

        Ok(())
    }
}

/// A compile_commands.json entry: a planned compile, or a file compiled inside a unity batch as if alone.
struct CompileEntry {
    source: PathBuf,
    command: CompileCommand,
}

/// What a compile_commands.json entry writes, hashed.
fn entry_hash(source: &Path, action: &Action) -> u64 {
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    let workdir = action.workdir.as_deref().unwrap_or(Path::new(""));

    for path in [source, &action.program, &action.outputs[0], workdir] {
        hasher.update(path.as_os_str().as_encoded_bytes());
        hasher.update(b"\0");
    }
    for argument in &action.args {
        hasher.update(argument.as_bytes());
        hasher.update(b"\0");
    }

    hasher.digest()
}

enum CompileCommand {
    Planned(usize),
    Alone(Box<Action>),
}

/// A file rule's settings on top of the target's.
fn apply_file_rule(settings: &mut FileSettings, rule: &targets::ResolvedFileRule) {
    if !rule.defines.is_empty() {
        settings.defines.to_mut().extend(rule.defines.iter().cloned());
    }
    if !rule.flags.is_empty() {
        settings.flags.to_mut().extend(rule.flags.iter().cloned());
    }
    settings.warnings = rule.warnings.unwrap_or(settings.warnings);
    settings.arch = rule.arch.or(settings.arch);
    settings.opt = rule.opt.unwrap_or(settings.opt);
}

/// Settings of one source file (CompileSpec borrows from it): the target's, copied only where file rules add to them.
#[derive(Clone)]
struct FileSettings<'a> {
    language: Language,
    standard: &'a str,
    opt: Opt,
    warnings: crate::flags::Warnings,
    werror: bool,
    arch: Option<crate::flags::Arch>,
    defines: Cow<'a, [String]>,
    includes: &'a [PathBuf],
    system_includes: &'a [PathBuf],
    flags: Cow<'a, [String]>,
}

/// When a target's own commands run.
#[derive(Clone, Copy)]
enum Stage {
    PreBuild,
    PostBuild,
}

/// One command of a build step, with placeholders for its input already filled in.
struct RunInvocation {
    input: Option<PathBuf>,
    inputs: Vec<PathBuf>,
    outputs: Vec<PathBuf>,
    words: Vec<String>,
}

// A package's placeholders a shell command line may hold, and whether each is a path
const SCOPE_PLACEHOLDERS: &[(&str, bool)] = &[
    ("{root}", true),
    ("{out}", true),
    ("{gen}", true),
    ("{vcpkg}", true),
    ("{cxx}", true),
    ("{cc}", true),
    ("{name}", false),
    ("{profile}", false),
    ("{config}", false),
    ("{version}", false),
];

/// What one placeholder of a shell command line stands for: one value, or a list ({inputs}).
struct ShellValue {
    placeholder: String,
    items: Vec<String>,
    is_path: bool,
}

impl ShellValue {
    fn new(placeholder: &str, items: Vec<String>, is_path: bool) -> ShellValue {
        ShellValue {
            placeholder: placeholder.to_string(),
            items,
            is_path,
        }
    }
}

/// Fills placeholders into a shell command line and keeps the rest as written. A value with a space or a character
/// the shell reads (`&`, `>`, ...) is quoted, from its start to the end of its word or the next redirection or pipe
/// (only the value itself when the word has quotes of its own); on Windows a path and the rest of its word take
/// backslashes, since cmd's built-ins read `/` as the start of a switch.
fn fill_shell_line(line: &str, values: &[ShellValue], windows: bool) -> String {
    let quote_characters: &[char] = if windows { &['"'] } else { &['"', '\''] };
    let mut filled = String::with_capacity(line.len());
    let mut word = String::new();
    let mut quote: Option<char> = None;

    // Split into words at spaces outside quotes; whitespace stays as written
    for character in line.chars() {
        match quote {
            Some(open) if character == open => quote = None,
            None if quote_characters.contains(&character) => quote = Some(character),
            None if character.is_whitespace() => {
                filled.push_str(&fill_shell_word(&std::mem::take(&mut word), values, windows, quote_characters));
                filled.push(character);
                continue;
            }
            _ => {}
        }
        word.push(character);
    }
    filled.push_str(&fill_shell_word(&word, values, windows, quote_characters));

    filled
}

fn fill_shell_word(word: &str, values: &[ShellValue], windows: bool, quote_characters: &[char]) -> String {
    let has_quotes = word.contains(quote_characters);
    let mut filled = String::with_capacity(word.len());
    let mut quote: Option<char> = None;
    let mut backslashes = false;
    let mut rest = word;

    // A quote opened before a value, closed at the end of the word or before a redirection or pipe
    let mut opened = false;

    while let Some(character) = rest.chars().next() {
        // A placeholder: its items, each quoted when it needs it and is not inside quotes already
        let value = values
            .iter()
            .find(|value| character == '{' && rest.starts_with(value.placeholder.as_str()));
        if let Some(value) = value {
            for (position, item) in value.items.iter().enumerate() {
                let item = if windows && value.is_path {
                    item.replace('/', "\\")
                } else {
                    item.clone()
                };
                if position > 0 {
                    filled.push(' ');
                }

                let needs_quotes = quote.is_none() && !opened && needs_shell_quotes(&item);
                if needs_quotes && (has_quotes || value.items.len() > 1) {
                    filled.push_str(&format!("\"{item}\""));
                } else {
                    if needs_quotes {
                        filled.push('"');
                        opened = true;
                    }
                    filled.push_str(&item);
                }
            }
            backslashes |= windows && value.is_path;
            rest = &rest[value.placeholder.len()..];
            continue;
        }

        match quote {
            Some(open) if character == open => quote = None,
            None if quote_characters.contains(&character) => quote = Some(character),
            _ => {}
        }
        if opened && SHELL_OPERATORS.contains(&character) {
            filled.push('"');
            opened = false;
        }

        // A backslash before a quote would escape it, so that slash stays
        let before_quote = rest[character.len_utf8()..].starts_with(quote_characters);
        filled.push(if backslashes && character == '/' && !before_quote {
            '\\'
        } else {
            character
        });
        rest = &rest[character.len_utf8()..];
    }

    if opened {
        filled.push('"');
    }
    filled
}

/// One word of a command list for the shell's line: quoted when it holds a space or a character the shell reads,
/// unless it is a redirection or pipe by itself (`>`, `2>&1`, `&&`).
fn shell_quote(word: &str) -> String {
    let is_operator = !word.is_empty()
        && word
            .chars()
            .all(|character| SHELL_OPERATORS.contains(&character) || character.is_ascii_digit());
    if word.is_empty() || (needs_shell_quotes(word) && !is_operator) {
        format!("\"{word}\"")
    } else {
        word.to_string()
    }
}

// Characters that end a quoted value in a shell line: redirections and pipes
const SHELL_OPERATORS: &[char] = &['<', '>', '|', '&'];

// Characters cmd (or sh) reads specially outside quotes, besides whitespace
const SHELL_SPECIALS: &[char] = &['&', '|', '<', '>', '(', ')', '^', '%', '!', ',', ';', '=', '$', '`'];

/// A value holds whitespace or a character the shell would act on.
fn needs_shell_quotes(text: &str) -> bool {
    text.contains(char::is_whitespace) || text.contains(SHELL_SPECIALS)
}

/// Puts extra arguments before the source-and-output tail of a compile command (cl: before /c).
fn insert_before_source(args: &mut Vec<String>, extra: &[String], msvc_style: bool) {
    let marker = if msvc_style { "/c" } else { "-c" };
    let position = args.iter().position(|argument| argument == marker).unwrap_or(args.len());

    for (offset, argument) in extra.iter().enumerate() {
        args.insert(position + offset, argument.clone());
    }
}

fn dedup_in_order<T, K: std::hash::Hash + Eq>(items: &mut Vec<T>, key: impl Fn(&T) -> K) {
    let mut seen = HashSet::new();
    items.retain(|item| seen.insert(key(item)));
}

/// Each library once, in order; flags and their values ("-framework", "Cocoa") stay as they are.
fn dedup_libs(libs: &mut Vec<String>) {
    let mut seen = HashSet::new();
    let mut flag_value = false;
    libs.retain(|lib| {
        let keep = flag_value || lib.starts_with('-') || seen.insert(lib.to_lowercase());
        flag_value = crate::flags::takes_value(lib);
        keep
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_is_not_part_of_the_command() {
        let mut action = Action::new(ActionKind::Compile, String::new(), PathBuf::from("cl"), Arc::new(Vec::new()));
        action.args = vec!["/c".into(), "a.cpp".into()];
        let without = action.compute_command_hash();

        action.launcher = Some(PathBuf::from("sccache"));
        assert_eq!(action.compute_command_hash(), without);
        assert_eq!(crate::exec::command_line(&action), "sccache cl /c a.cpp");
    }

    #[test]
    fn long_compiles_run_without_their_launcher() {
        let mut action = Action::new(ActionKind::Compile, String::new(), PathBuf::from("cl"), Arc::new(Vec::new()));
        action.launcher = Some(PathBuf::from("sccache"));
        action.args = (0..500)
            .map(|number| format!("/DLONG_DEFINE_{number}_XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX={number}"))
            .collect();

        assert_eq!(crate::exec::command_line(&action).starts_with("sccache "), !cfg!(windows));
    }

    fn values() -> Vec<ShellValue> {
        vec![
            ShellValue::new("{root}", vec!["C:/my project".into()], true),
            ShellValue::new("{out}", vec!["C:/p/out".into()], true),
            ShellValue::new("{inputs}", vec!["C:/a b/x.txt".into(), "C:/c/y.txt".into()], true),
            ShellValue::new("{name}", vec!["my app".into()], false),
        ]
    }

    #[test]
    fn fills_shell_lines() {
        // Windows: quotes kept, values with spaces quoted, paths and the rest of their word with backslashes
        let line = r#"copy {root}/data/a.txt "{out}/b c.txt" && echo "x  y" {name}/z"#;
        assert_eq!(
            fill_shell_line(line, &values(), true),
            r#"copy "C:\my project\data\a.txt" "C:\p\out\b c.txt" && echo "x  y" "my app/z""#
        );
        assert_eq!(fill_shell_line("tool {inputs}", &values(), true), r#"tool "C:\a b\x.txt" C:\c\y.txt"#);
        assert_eq!(
            fill_shell_line(r#"tool --in={root}/"q r""#, &values(), true),
            r#"tool --in="C:\my project"/"q r""#
        );

        // Elsewhere: slashes stay, single quotes quote too
        assert_eq!(
            fill_shell_line("cat '{root}/a' {root}/b", &values(), false),
            r#"cat 'C:/my project/a' "C:/my project/b""#
        );

        // A redirection stays outside the quotes; a value with a character cmd reads is quoted too
        assert_eq!(
            fill_shell_line("echo hi>{root}/a.txt", &values(), true),
            r#"echo hi>"C:\my project\a.txt""#
        );
        assert_eq!(
            fill_shell_line("tool {root}/x>{out}/log 2>&1", &values(), true),
            r#"tool "C:\my project\x">C:\p\out\log 2>&1"#
        );
        let values = vec![ShellValue::new("{root}", vec!["C:/s9&x".into()], true)];
        assert_eq!(fill_shell_line("python {root}/w.py", &values, true), r#"python "C:\s9&x\w.py""#);
    }

    #[test]
    fn quotes_shell_words() {
        assert_eq!(shell_quote("plain"), "plain");
        assert_eq!(shell_quote("a b"), "\"a b\"");
        assert_eq!(shell_quote(""), "\"\"");
        assert_eq!(shell_quote("C:/s9&x/a.txt"), "\"C:/s9&x/a.txt\"");
        assert_eq!(shell_quote(">"), ">");
        assert_eq!(shell_quote("2>&1"), "2>&1");
    }

    #[test]
    fn kinds_of_listed_files() {
        let kind = |name: &str| source_kind(Path::new(name), Family::Msvc);
        assert!(matches!(kind("x.def"), Ok(SourceKind::Definition)));
        assert!(matches!(kind("app.manifest"), Ok(SourceKind::Manifest)));
        assert!(matches!(kind("x.inl"), Ok(SourceKind::Header)));
        assert!(kind("m.ixx").err().unwrap().to_string().contains("C++20 modules"));
        assert!(kind("notes.txt").is_err());
    }
}
