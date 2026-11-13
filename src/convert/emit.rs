//! Turns what the interpreter collected into anvil.toml: conventional paths left out, file lists turned back into
//! patterns, flags mapped to Anvil settings, imported targets mapped to vcpkg ports, settings every target shares
//! moved to [defaults], and TODO comments for whatever has no equivalent.

use super::interp::Interpreter;
use super::model::{CustomCommand, Item, Target, TargetKind, TestProperties};
use super::test_groups;
use super::Conversion;
use crate::util::{self, slash};
use anyhow::Result;
use indexmap::{IndexMap, IndexSet};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};

// Compile flags Anvil passes already, or that belong to profiles
const DROPPED_FLAGS: &[&str] = &[
    "/permissive-",
    "/utf-8",
    "/EHsc",
    "/EHs",
    "/MP",
    "/nologo",
    "/Zc:__cplusplus",
    "/bigobj",
    "-fPIC",
    "-fpic",
    "/Zi",
    "/Z7",
    "/ZI",
    "/Od",
    "/O1",
    "/O2",
    "/Ox",
    "/Ob1",
    "/Ob2",
    "/RTC1",
    "/DNDEBUG",
    "-DNDEBUG",
    "-O0",
    "-O1",
    "-O2",
    "-O3",
    "-Os",
    "-g",
    "/GR",
    "/W1",
    "/W2",
    "/W3",
    "/DWIN32",
    "/D_WINDOWS",
    "-fdiagnostics-color=always",
    "-fcolor-diagnostics",
];

// Link flags Anvil passes already
const DROPPED_LINK_FLAGS: &[&str] = &[
    "/INCREMENTAL",
    "/INCREMENTAL:NO",
    "/DEBUG",
    "/DEBUG:FULL",
    "/DEBUG:FASTLINK",
    "/NOLOGO",
    "/machine:x64",
    "/MACHINE:X64",
    "/SUBSYSTEM:CONSOLE",
];

// GCC/Clang flags that Anvil's arch = "avx2" passes already
const AVX2_IMPLIED_FLAGS: &[&str] = &["-mavx", "-mavx2", "-mfma", "-mbmi", "-mbmi2", "-mf16c", "-mlzcnt", "-mpopcnt"];

// Triplets Anvil chooses by itself for the toolchain and platform
const STANDARD_TRIPLETS: &[&str] = &[
    "x64-windows",
    "arm64-windows",
    "x64-linux",
    "arm64-linux",
    "x64-osx",
    "arm64-osx",
    "x64-mingw-dynamic",
];

// Imported targets whose vcpkg port is not their namespace ("ns::" matches the whole namespace)
const KNOWN_PORTS: &[(&str, &str)] = &[
    ("gpuopen::vulkanmemoryallocator", "vulkan-memory-allocator"),
    ("curl::", "curl"),
    ("jolt::", "joltphysics"),
    ("tracy::", "tracy"),
    ("enkits::", "enkits"),
    ("sqlite3::", "sqlite3"),
    ("sqlite::", "sqlite3"),
    ("gtest::", "gtest"),
    ("gmock::", "gtest"),
    ("catch2::", "catch2"),
    ("openssl::", "openssl"),
    ("zlib::", "zlib"),
    ("png::", "libpng"),
    ("jpeg::", "libjpeg-turbo"),
    ("freetype::", "freetype"),
    ("freetype", "freetype"),
    ("eigen3::", "eigen3"),
    ("tbb::", "tbb"),
    ("glfw", "glfw3"),
    ("absl::", "abseil"),
    ("protobuf::", "protobuf"),
    ("magic_enum::", "magic-enum"),
    ("tl::expected", "tl-expected"),
    ("vulkan::headers", "vulkan-headers"),
    ("vulkan::", "vulkan"),
    ("yaml-cpp", "yaml-cpp"),
    ("yaml-cpp::", "yaml-cpp"),
    ("assimp", "assimp"),
    ("lz4::", "lz4"),
    ("zstd::", "zstd"),
    ("libzstd_static", "zstd"),
    ("libzstd_shared", "zstd"),
    ("glad::", "glad"),
    ("stb::", "stb"),
    ("spirv-tools", "spirv-tools"),
    ("fmt::", "fmt"),
    ("spdlog::", "spdlog"),
    ("nlohmann_json::", "nlohmann-json"),
    ("httplib::", "cpp-httplib"),
];

// Profiles every manifest knows; with the platform and compiler words (crate::cond::BUILTIN_WORDS), any other word
// in a condition is an option or a profile
const KNOWN_PROFILES: &[&str] = &["debug", "release"];

// The order settings appear in within a table
const KEY_ORDER: &[&str] = &[
    "sources",
    "include",
    "system-include",
    "defines",
    "flags",
    "link-flags",
    "libs",
    "lib-dirs",
    "deps",
    "whole-archive",
    "header-deps",
    "needs",
    "std",
    "c-std",
    "warnings",
    "werror",
    "arch",
    "cuda-arch",
    "gui",
];

// A group of tests is written only when it takes at most this share (in percent) of the tests written one by one
const GROUP_SIZE_PERCENT: usize = 80;

// A command up to this long is written as one line
const COMMAND_LINE_WIDTH: usize = 100;

// A group of tests whose entries do not fit lines this wide is written test by test
const GROUP_LINE_WIDTH: usize = 160;

// Lists too long for one line are packed into lines this wide
const LINE_WIDTH: usize = 110;

// Keys whose empty list turns Anvil's layout convention off (sources from src/, include/ and src/ folders)
const CONVENTION_KEYS: &[&str] = &["sources", "include", "public.include"];

// Keys of the [public] and [interface] sub-tables are written `public.<key>` until rendered
const SUB_TABLES: &[&str] = &["public", "interface"];

// Settings [defaults] may hold: private ones, which do not leak into dependents
const FACTORED_KEYS: &[&str] = &[
    "defines",
    "flags",
    "link-flags",
    "std",
    "warnings",
    "werror",
    "arch",
    "include",
    "public.include",
    "deps",
    "public.deps",
    "header-deps",
    "needs",
    "whole-archive",
    "libs",
    "lib-dirs",
    "output-prefix",
    "output-suffix",
    "exports",
];

// Words a build step's command hands to the shell (redirections, pipes, chaining)
const SHELL_OPERATORS: &[&str] = &[">", ">>", "<", "|", "&&", "||", "2>", "2>&1", "1>"];

// Folders never searched for sources
const SKIPPED_FOLDERS: &[&str] = &["out", "build", "target", "node_modules", "_deps", "vcpkg_installed"];

// The condition of a setting that never applies; such settings are dropped
const NEVER: &str = "false";

// [preset.common]: worth it for this many targets sharing this many settings
const PRESET_NAME: &str = "common";
const MIN_PRESET_MEMBERS: usize = 3;
const MIN_PRESET_ENTRIES: usize = 3;

// Presets of targets made alike: the settings they may hold (public ones too: a preset is the targets' own), and
// how many two targets must share
const SIBLING_KEYS: &[&str] = &[
    "defines",
    "flags",
    "link-flags",
    "std",
    "warnings",
    "werror",
    "arch",
    "include",
    "system-include",
    "deps",
    "header-deps",
    "needs",
    "libs",
    "lib-dirs",
    "public.include",
    "public.system-include",
    "public.defines",
    "public.flags",
    "public.link-flags",
    "public.deps",
    "public.libs",
    "interface.defines",
];
const MIN_SIBLING_LINES_SAVED: usize = 4;

// A list longer than this goes on several lines
const INLINE_LIST_WIDTH: usize = 100;

#[derive(Debug, Clone, PartialEq)]
enum Value {
    List(Vec<String>),
    Text(String),
    Bool(bool),
}

/// One setting: its condition, key and value (one list item, or a scalar).
type Entry = (Option<String>, &'static str, Value);

/// Settings by the condition they hold under (None: always).
#[derive(Debug, Default, Clone)]
struct Blocks {
    blocks: IndexMap<Option<String>, IndexMap<&'static str, Value>>,
}

#[derive(Debug, Default, Clone)]
struct FileRuleOut {
    files: Vec<String>,
    condition: Option<String>,
    defines: Vec<String>,
    flags: Vec<String>,
    arch: Option<String>,
    warnings: Option<String>,
}

#[derive(Debug, Default, Clone)]
struct RunOut {
    for_each: Vec<String>,
    inputs: Vec<String>,
    outputs: Vec<String>,
    command: Vec<String>,
    shell: bool,
    depfile: bool,
    message: Option<String>,
    workdir: Option<String>,
    condition: Option<String>,
}

#[derive(Debug, Default)]
struct TargetOut {
    kind: &'static str,
    presets: Vec<String>,
    /// The project's function that made it.
    helper: Option<String>,
    dir: Option<String>,
    condition: Option<String>,
    output_name: Option<String>,
    pch: Option<String>,
    args: Vec<String>,
    workdir: Option<String>,
    export_header: Option<(String, String)>,
    pre_build: Vec<RunOut>,
    post_build: Vec<RunOut>,
    test: TestProperties,
    blocks: Blocks,
    files: Vec<FileRuleOut>,
    runs: Vec<RunOut>,
    notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub(super) struct TestOut {
    pub name: String,
    pub command: Vec<String>,
    pub workdir: Option<String>,
    pub condition: Option<String>,
    pub properties: TestProperties,
    pub helper: Option<super::model::HelperCall>,
    pub loops: Vec<(String, String)>,
    pub dir: PathBuf,
}

#[derive(Debug, Clone)]
enum DependencyEntry {
    Vcpkg { verified: bool, note: Option<String> },
    Git { url: String, tag: Option<String> },
    PkgConfig(String),
}

/// What a link item turned out to be.
#[derive(Debug, Clone)]
enum Link {
    Target(String),
    Dependency(String),
    /// A target or dependency used for its headers only.
    HeaderOnly(String),
    Library(String),
    Flag(String),
}

/// What a compile flag means in anvil.toml.
enum FlagMeaning {
    Drop,
    Keep,
    Warnings(&'static str),
    Werror,
    Arch(&'static str),
    Standard(String),
    Define(String),
    Note(String),
}

struct Emitter {
    cmake: Interpreter,
    root: PathBuf,
    root_text: String,

    // Every file under the project, and every target's sources, for turning file lists back into patterns
    all_files: Vec<(String, PathBuf)>,
    source_owners: Vec<(String, String)>,
    /// Folders where several targets would take the convention's sources: those targets list theirs.
    crowded_folders: HashSet<String>,

    // Dependencies: the project's vcpkg.json, a vcpkg checkout to check port names, and what was found
    manifest_ports: Vec<String>,
    vcpkg: Option<crate::vcpkg::Instance>,
    dependencies: IndexMap<String, DependencyEntry>,
    link_cache: HashMap<String, Vec<(Option<String>, Link)>>,

    // The result
    targets: IndexMap<String, TargetOut>,
    defaults: Blocks,
    /// [preset.common], then one per helper function that made several targets.
    presets: IndexMap<String, Blocks>,
    project_std: Option<String>,
    tests: Vec<TestOut>,
    files: Vec<(PathBuf, String)>,
    warnings: Vec<String>,

    // Steps that run CMake scripts (cmake -P)
    cmake_scripts: usize,
}

pub fn emit(interpreter: Interpreter) -> Result<Conversion> {
    let mut emitter = Emitter::new(interpreter);

    emitter.convert_export_headers();
    emitter.convert_targets();
    emitter.convert_custom_commands();
    emitter.convert_tests();
    emitter.drop_empty_custom_targets();
    emitter.factor_shared_settings();

    Ok(emitter.render())
}

impl Blocks {
    fn push(&mut self, condition: Option<String>, key: &'static str, item: String) {
        if condition.as_deref() == Some(NEVER) {
            return;
        }
        let block = self.blocks.entry(condition).or_default();
        let Value::List(list) = block.entry(key).or_insert_with(|| Value::List(Vec::new())) else {
            return;
        };

        if !list.contains(&item) {
            list.push(item);
        }
    }

    fn set(&mut self, condition: Option<String>, key: &'static str, value: Value) {
        if condition.as_deref() == Some(NEVER) {
            return;
        }
        self.blocks.entry(condition).or_default().insert(key, value);
    }

    fn add(&mut self, (condition, key, value): Entry) {
        match value {
            Value::List(items) => {
                for item in items {
                    self.push(condition.clone(), key, item);
                }
            }
            scalar => self.set(condition, key, scalar),
        }
    }

    fn get(&self, condition: &Option<String>, key: &str) -> Option<&Value> {
        self.blocks.get(condition).and_then(|block| block.get(key))
    }

    fn remove(&mut self, condition: &Option<String>, key: &str) {
        if let Some(block) = self.blocks.get_mut(condition) {
            block.shift_remove(key);
        }
    }

    /// Every condition at its shortest given `assumption` (the target's own condition), merging blocks that turn out
    /// the same and dropping those that never hold.
    fn simplify_conditions(&mut self, assumption: Option<&str>) {
        let blocks = std::mem::take(&mut self.blocks);
        for (condition, block) in blocks {
            let simplified = condition.and_then(|condition| condition_text(&Some(condition), assumption));
            for (key, value) in block {
                match value {
                    // (an empty list says something too: no sources, no include folders)
                    Value::List(items) if items.is_empty() && simplified.as_deref() != Some(NEVER) => {
                        let block = self.blocks.entry(simplified.clone()).or_default();
                        block.entry(key).or_insert(Value::List(items));
                    }
                    value => self.add((simplified.clone(), key, value)),
                }
            }
        }
    }

    /// Drops empty lists and empty blocks; merges a scalar set the same way under `c` and `!c` into one.
    fn tidy(&mut self) {
        // With arch = "avx2" everywhere, the GCC flags it implies are noise
        let wide_arch = matches!(self.get(&None, "arch"), Some(Value::Text(arch)) if arch == "avx2" || arch == "avx512");
        for block in self.blocks.values_mut() {
            if wide_arch {
                if let Some(Value::List(flags)) = block.get_mut("flags") {
                    flags.retain(|flag| !AVX2_IMPLIED_FLAGS.contains(&flag.as_str()));
                }
            }
            // An empty list of these replaces Anvil's layout convention, so it says something
            block.retain(|key, value| CONVENTION_KEYS.contains(key) || !matches!(value, Value::List(list) if list.is_empty()));
        }

        // warnings = "high" under msvc and under !msvc is just warnings = "high"
        let conditions: Vec<String> = self.blocks.keys().flatten().cloned().collect();
        for condition in conditions {
            let negated = Some(super::model::negate(&condition));
            let keys: Vec<&'static str> = self
                .blocks
                .get(&Some(condition.clone()))
                .map(|block| block.keys().copied().collect())
                .unwrap_or_default();

            for key in keys {
                let here = self.get(&Some(condition.clone()), key).cloned();
                let there = self.get(&negated, key).cloned();
                if here.is_some() && here == there && !matches!(here, Some(Value::List(_))) {
                    self.remove(&Some(condition.clone()), key);
                    self.remove(&negated, key);
                    self.set(None, key, here.clone().unwrap());
                }

                // List items under both hold everywhere too (a list this empties goes: an empty one says something)
                if let (Some(Value::List(here)), Some(Value::List(there))) = (here, there) {
                    let both: Vec<String> = here.into_iter().filter(|item| there.contains(item)).collect();
                    for condition in [Some(condition.clone()), negated.clone()] {
                        let Some(block) = self.blocks.get_mut(&condition) else { continue };
                        if let Some(Value::List(items)) = block.get_mut(key) {
                            items.retain(|item| !both.contains(item));

                            // (emptied here: the items still hold, so it is no `sources = []`)
                            if items.is_empty() && !both.is_empty() {
                                block.shift_remove(key);
                            }
                        }
                    }
                    for item in both {
                        self.push(None, key, item);
                    }
                }
            }
        }

        self.blocks.retain(|_, block| !block.is_empty());
    }
}

impl Emitter {
    fn new(cmake: Interpreter) -> Emitter {
        let root = cmake.root.clone();
        let root_text = slash(&root);

        // The project's vcpkg.json ports
        let manifest_ports = std::fs::read_to_string(root.join("vcpkg.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|manifest| manifest.get("dependencies").and_then(|dependencies| dependencies.as_array()).cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|dependency| {
                dependency
                    .as_str()
                    .or_else(|| dependency.get("name").and_then(|name| name.as_str()))
                    .map(String::from)
            })
            .collect();

        let source_owners = cmake
            .targets
            .values()
            .flat_map(|target| target.sources.iter().map(|item| (util::path_key(&item.value), target.name.clone())))
            .collect();

        Emitter {
            cmake_scripts: 0,
            all_files: collect_files(&root),
            source_owners,
            crowded_folders: HashSet::new(),
            vcpkg: crate::vcpkg::Instance::select(None).ok(),
            cmake,
            root,
            root_text,
            manifest_ports,
            dependencies: IndexMap::new(),
            link_cache: HashMap::new(),
            targets: IndexMap::new(),
            defaults: Blocks::default(),
            presets: IndexMap::new(),
            project_std: None,
            tests: Vec::new(),
            files: Vec::new(),
            warnings: Vec::new(),
        }
    }

    // Targets

    fn convert_targets(&mut self) {
        let targets: Vec<Target> = self.cmake.targets.values().cloned().collect();

        // Anvil gives a folder's sources to one target only, so where several would take them all, they list them
        let mut conventional_per_folder: HashMap<String, usize> = HashMap::new();
        for target in &targets {
            let compiles = !matches!(target.kind, TargetKind::Interface | TargetKind::Custom);
            let files: Vec<PathBuf> = target
                .sources
                .iter()
                .filter(|item| is_compiled(&item.value))
                .map(|item| item.value.clone())
                .collect();
            if compiles && self.matches_convention(&target.dir, &files) {
                *conventional_per_folder.entry(util::path_key(&target.dir)).or_default() += 1;
            }
        }
        self.crowded_folders = conventional_per_folder
            .into_iter()
            .filter(|(_, count)| *count > 1)
            .map(|(folder, _)| folder)
            .collect();

        for target in &targets {
            let converted = self.convert_target(target);
            self.targets.insert(target.name.clone(), converted);
        }
    }

    fn convert_target(&mut self, target: &Target) -> TargetOut {
        let dir = target.dir.clone();
        let base = condition_text(&target.condition, None);
        let relative_condition = |condition: &Option<String>| condition_text(condition, base.as_deref());

        let mut converted = TargetOut {
            helper: target.helper.clone(),
            kind: match target.kind {
                TargetKind::Exe => "exe",
                TargetKind::Static => "lib",
                TargetKind::Shared => "shared",
                TargetKind::Object => "object",
                TargetKind::Interface => "headers",
                TargetKind::Custom => "custom",
            },
            dir: (util::path_key(&dir) != util::path_key(&self.root)).then(|| relative_path(&dir, &self.root)),
            condition: base.clone(),
            output_name: target.output_name.clone().filter(|name| *name != target.name),
            pch: target.pch.as_ref().map(|pch| self.path_text(pch, &dir)),
            export_header: target.export_header.clone(),
            notes: target.notes.clone(),
            ..Default::default()
        };

        // Keys properties set: the file name, exports, files that relink, GPUs
        let has_cuda = target
            .sources
            .iter()
            .any(|item| item.value.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("cu")));
        for item in &target.settings {
            let (key, value) = &item.value;
            let condition = relative_condition(&item.condition);
            match *key {
                "cuda-arch" if !has_cuda => {}
                "link-depends" => converted.blocks.push(condition, key, self.path_text(Path::new(value), &dir)),
                "output-dir" if value == "{out}" => {}
                "output-dir" | "pch" => converted.blocks.set(condition, key, Value::Text(self.path_text(Path::new(value), &dir))),
                "exports" => {
                    if super::model::constant_truth(value) == Some(true) {
                        converted.blocks.set(condition, key, Value::Bool(true));
                    }
                }
                "gui" => converted.blocks.set(condition, key, Value::Bool(value == "true")),
                _ => converted.blocks.set(condition, key, Value::Text(value.clone())),
            }
        }

        // Shared when BUILD_SHARED_LIBS is
        if let Some(condition) = &target.shared_condition {
            converted
                .blocks
                .set(relative_condition(&Some(condition.clone())), "kind", Value::Text("shared".into()));
        }

        // Sources
        self.convert_sources(target, &mut converted, &relative_condition);

        // Include folders; another target's include folders are a header-only dependency on it
        for item in &target.includes {
            let (path, system) = &item.value;
            if let Some(name) = header_dep_name(&slash(path)) {
                for (extra_condition, link) in self.classify_link(&format!("{{header-dep:{name}}}"), &dir) {
                    if let Link::HeaderOnly(name) = link {
                        let condition = super::model::and_optional(&relative_condition(&item.condition), &extra_condition);
                        converted.blocks.push(condition, "header-deps", name);
                    }
                }
                continue;
            }

            // A folder find_path() looked for: the package of that header
            if let Some(header) = found_header(&slash(path)) {
                let stem = Path::new(&header)
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let port = self
                    .manifest_ports
                    .iter()
                    .find(|port| normalize_name(port) == normalize_name(&stem))
                    .cloned();
                match port {
                    Some(port) => {
                        self.dependencies
                            .entry(port.clone())
                            .or_insert(DependencyEntry::Vcpkg { verified: true, note: None });
                        let key = if item.public { "public.deps" } else { "deps" };
                        converted.blocks.push(relative_condition(&item.condition), key, port);
                    }
                    None => converted.notes.push(format!("the folder of {header} (from find_path) is not converted")),
                }
                continue;
            }

            let path = &self.generated_folder(path).unwrap_or_else(|| path.clone());
            let key = match (item.public, system) {
                (true, _) => "public.include",
                (false, true) => "system-include",
                (false, false) => "include",
            };
            converted
                .blocks
                .push(relative_condition(&item.condition), key, self.path_text(path, &dir));
        }

        // Definitions and flags
        for item in &target.defines {
            if let Some(note) = unconverted_note(&item.value) {
                converted.notes.push(note);
                continue;
            }
            let key = if item.public { "public.defines" } else { "defines" };
            converted
                .blocks
                .push(relative_condition(&item.condition), key, self.define_text(&item.value));
        }
        for item in &target.interface_defines {
            converted
                .blocks
                .push(relative_condition(&item.condition), "interface.defines", self.define_text(&item.value));
        }
        for item in &target.interface_options {
            converted
                .blocks
                .push(relative_condition(&item.condition), "interface.flags", item.value.clone());
        }
        for item in &target.interface_includes {
            let path = self.generated_folder(&item.value.0).unwrap_or_else(|| item.value.0.clone());
            converted
                .blocks
                .push(relative_condition(&item.condition), "interface.include", self.path_text(&path, &dir));
        }

        for item in &target.options {
            self.convert_compile_flag(item, &relative_condition(&item.condition), &mut converted);
        }

        // Link options; libraries between --whole-archive and --no-whole-archive are whole-archive
        let options = &target.link_options;
        let mut index = 0;
        while index < options.len() {
            let condition = relative_condition(&options[index].condition);
            let run = options[index..].iter().position(|option| option.value.ends_with("--no-whole-archive"));
            if let (true, Some(run)) = (options[index].value.ends_with("--whole-archive"), run) {
                let names: Option<Vec<String>> = options[index + 1..index + run]
                    .iter()
                    .map(|option| self.whole_archive_name(&option.value, &dir))
                    .collect();
                if let Some(names) = names {
                    for name in names {
                        converted.blocks.push(condition.clone(), "whole-archive", name);
                    }
                    index += run + 1;
                    continue;
                }
            }

            match options[index]
                .value
                .strip_prefix("/WHOLEARCHIVE:")
                .and_then(|library| self.whole_archive_name(library, &dir))
            {
                Some(name) => converted.blocks.push(condition, "whole-archive", name),
                None => self.convert_link_flag(&options[index].value, condition, &mut converted.blocks),
            }
            index += 1;
        }

        for item in &target.lib_dirs {
            converted.blocks.push(
                relative_condition(&item.condition),
                "lib-dirs",
                self.path_text(Path::new(&item.value), &dir),
            );
        }

        // Libraries: project targets, packages, system libraries
        for item in &target.links {
            let condition = relative_condition(&item.condition);
            for (extra_condition, link) in self.classify_link(&item.value, &dir) {
                let condition = super::model::and_optional(&condition, &extra_condition);
                match link {
                    Link::Target(name) | Link::Dependency(name) => {
                        let key = if item.public { "public.deps" } else { "deps" };
                        converted.blocks.push(condition, key, name);
                    }
                    Link::HeaderOnly(name) => converted.blocks.push(condition, "header-deps", name),
                    Link::Library(library) => converted.blocks.push(condition, "libs", library),
                    Link::Flag(flag) => self.convert_link_flag(&flag, condition, &mut converted.blocks),
                }
            }
        }

        // add_dependencies: built first, nothing linked (unless it is linked anyway)
        for depend in &target.depends {
            let name = self.cmake.resolve_alias(depend);
            let linked = ["deps", "public.deps"]
                .iter()
                .any(|key| has_value(&converted.blocks, &None, key, &Value::List(vec![name.clone()])));
            if self.cmake.targets.contains_key(&name) && !linked {
                converted.blocks.push(None, "needs", name);
            }
        }

        // Single settings
        if let Some(standard) = self.effective_standard(&target.name, &mut Vec::new()) {
            converted.blocks.set(None, "std", Value::Text(format!("c++{standard}")));
        }
        if let Some(standard) = target.c_std {
            converted.blocks.set(None, "c-std", Value::Text(format!("c{standard}")));
        }
        if target.gui && converted.blocks.get(&None, "gui").is_none() {
            converted.blocks.set(None, "gui", Value::Bool(true));
        }

        // A windows-subsystem program entered through main() (/ENTRY:mainCRTStartup), not WinMain
        let options = target.link_options.iter().chain(&target.links);
        if options.map(|item| item.value.to_lowercase()).any(|option| is_main_entry(&option)) {
            for block in converted.blocks.blocks.values_mut() {
                if let Some(gui @ Value::Bool(true)) = block.get_mut("gui") {
                    *gui = Value::Text("main".into());
                }
            }
        }
        if target.imported {
            converted.notes.push("an IMPORTED target in CMake: check its include and libs".into());
        }

        // Per-file settings
        self.convert_file_rules(target, &mut converted);

        // {target:pkg::lib} of a package names the dependency it became
        let mut renamed: HashMap<String, String> = HashMap::new();
        let referenced: Vec<String> = converted
            .blocks
            .blocks
            .values()
            .flat_map(|block| block.values())
            .filter_map(|value| match value {
                Value::List(items) => Some(crate::plan::target_references(items)),
                _ => None,
            })
            .flatten()
            .filter(|name| !self.cmake.targets.contains_key(name))
            .collect();
        for name in referenced {
            let links = self.classify_link(&name, &dir);
            if let [(None, Link::Dependency(dependency))] = links.as_slice() {
                renamed.insert(name, dependency.clone());
            }
        }

        // Generator expressions with no Anvil equivalent, and files of targets Anvil does not build, are left for the user
        let notes = &mut converted.notes;
        let known_targets = &self.cmake.targets;
        for block in converted.blocks.blocks.values_mut() {
            for value in block.values_mut() {
                let Value::List(items) = value else { continue };
                for item in items.iter_mut() {
                    for (name, dependency) in &renamed {
                        *item = item.replace(&format!("{{target:{name}}}"), &format!("{{target:{dependency}}}"));
                    }
                }
                let known_dependencies: Vec<&String> = renamed.values().collect();
                items.retain(|item| {
                    let unknown_target = crate::plan::target_references(std::slice::from_ref(item))
                        .iter()
                        .any(|name| !known_targets.contains_key(name) && !known_dependencies.contains(&name));
                    let unconverted = item.contains("$<") || item.contains("{found-header:") || unknown_target;
                    if unconverted {
                        notes.push(format!("not converted: {item}"));
                    }
                    !unconverted
                });
            }
        }

        // Each note once
        let mut seen_notes = HashSet::new();
        converted.notes.retain(|note| seen_notes.insert(note.clone()));

        // Custom targets only order the build: they keep nothing but their dependencies
        if converted.kind == "custom" {
            converted
                .blocks
                .blocks
                .values_mut()
                .for_each(|block| block.retain(|key, _| matches!(*key, "deps" | "public.deps" | "needs")));
        }

        // A program's settings are its own when nothing links it; then the include folders the convention gives anyway
        let exports = target.settings.iter().any(|item| item.value.0 == "exports");
        let is_program = converted.kind == "exe" && !exports && !self.is_linked(&target.name);
        if is_program {
            make_private(&mut converted.blocks);
        }
        self.omit_conventional_includes(&dir, converted.kind, &mut converted.blocks);

        // Conditions combined above (`(unix & !macos) & unix`) at their shortest
        converted.blocks.simplify_conditions(converted.condition.as_deref());
        converted.blocks.tidy();
        converted
    }

    /// Does a target of the project link this one?
    fn is_linked(&self, name: &str) -> bool {
        self.cmake
            .targets
            .values()
            .any(|target| target.links.iter().any(|item| self.cmake.resolve_alias(&item.value) == name))
    }

    /// The target's standard, raised by the public standards of the project targets it links, as CMake does.
    fn effective_standard(&self, name: &str, visiting: &mut Vec<String>) -> Option<u32> {
        let target = self.cmake.targets.get(name)?;
        if visiting.iter().any(|visited| visited == name) {
            return target.std;
        }
        visiting.push(name.to_string());

        let mut standard = target.std;
        for item in &target.links {
            let linked = self.cmake.resolve_alias(&item.value);
            let Some(linked_target) = self.cmake.targets.get(&linked) else {
                continue;
            };

            let inherited = linked_target.public_std.max(self.effective_public_standard(&linked, visiting));
            standard = standard.max(inherited);
        }

        visiting.pop();
        standard
    }

    /// The standard a target passes on: its own public one and what its links pass on.
    fn effective_public_standard(&self, name: &str, visiting: &mut Vec<String>) -> Option<u32> {
        let target = self.cmake.targets.get(name)?;
        if visiting.iter().any(|visited| visited == name) {
            return target.public_std;
        }
        visiting.push(name.to_string());

        let mut standard = target.public_std;
        for item in target.links.iter().filter(|item| item.public) {
            standard = standard.max(self.effective_public_standard(&self.cmake.resolve_alias(&item.value), visiting));
        }

        visiting.pop();
        standard
    }

    fn convert_sources(&mut self, target: &Target, converted: &mut TargetOut, relative_condition: &dyn Fn(&Option<String>) -> Option<String>) {
        let dir = &target.dir;
        let compiled: Vec<&Item<PathBuf>> = target.sources.iter().filter(|item| is_compiled(&item.value)).collect();

        let always: Vec<PathBuf> = compiled
            .iter()
            .filter(|item| relative_condition(&item.condition).is_none())
            .map(|item| item.value.clone())
            .collect();
        let mut sometimes: IndexMap<String, Vec<PathBuf>> = IndexMap::new();
        for item in &compiled {
            if let Some(condition) = relative_condition(&item.condition) {
                sometimes.entry(condition).or_default().push(item.value.clone());
            }
        }

        // The convention covers every file: leave the target's own sources out. Files under a condition leave the
        // convention on the platforms where it does not hold, so they need only their when blocks
        let compiles = !matches!(target.kind, TargetKind::Interface | TargetKind::Custom);
        let every_file: Vec<PathBuf> = always.iter().chain(sometimes.values().flatten()).cloned().collect();
        let crowded = self.crowded_folders.contains(&util::path_key(dir));
        let conventional = compiles && !crowded && self.matches_convention(dir, &every_file);

        // Otherwise the files themselves, conditional ones excluded from the patterns and added under their condition
        if !conventional {
            let conditional_keys: HashSet<String> = sometimes.values().flatten().map(|path| util::path_key(path)).collect();
            for pattern in self.source_patterns(dir, &always, &conditional_keys, Some(&target.name)) {
                converted.blocks.push(None, "sources", pattern);
            }
            if compiles && always.is_empty() && !sometimes.is_empty() {
                converted.blocks.set(None, "sources", Value::List(Vec::new()));
            }
        }
        for (condition, files) in sometimes {
            for file in files {
                converted.blocks.push(Some(condition.clone()), "sources", self.path_text(&file, dir));
            }
        }
        if conventional {
            return;
        }

        // A target with no compiled sources under any condition still needs `sources = []` so the convention adds none
        let has_sources = converted.blocks.blocks.values().any(|block| block.contains_key("sources"));
        if compiles && !has_sources {
            converted.blocks.set(None, "sources", Value::List(Vec::new()));
            converted.notes.push("no source files: check this target".into());
        }
    }

    /// Would Anvil's convention for a target in `dir` find exactly these files?
    fn matches_convention(&self, dir: &Path, files: &[PathBuf]) -> bool {
        let source_dir = dir.join("src");
        let source_key = format!("{}/", util::path_key(&source_dir));
        let dir_key = util::path_key(dir);

        let conventional: HashSet<&String> = if source_dir.is_dir() {
            self.all_files
                .iter()
                .filter(|(key, path)| key.starts_with(&source_key) && is_compiled(path) && !is_resource(path))
                .map(|(key, _)| key)
                .collect()
        } else {
            self.all_files
                .iter()
                .filter(|(_, path)| path.parent().is_some_and(|parent| util::path_key(parent) == dir_key) && is_default_extension(path))
                .map(|(key, _)| key)
                .collect()
        };
        let given: HashSet<String> = files.iter().map(|file| util::path_key(file)).collect();

        !given.is_empty() && conventional.len() == given.len() && conventional.iter().all(|key| given.contains(*key))
    }

    /// File lists as patterns: a folder whose files are all listed becomes `folder/**/*.cpp`; generated files and
    /// stragglers stay as they are. For a target's sources (`owner`), only folders no other target compiles files
    /// from, since a new file there would land in the wrong target.
    fn source_patterns(&self, dir: &Path, files: &[PathBuf], excluded_keys: &HashSet<String>, owner: Option<&str>) -> Vec<String> {
        let mut remaining: IndexMap<String, PathBuf> = files.iter().map(|file| (util::path_key(file), file.clone())).collect();
        let mut patterns = Vec::new();
        let root_key = util::path_key(&self.root);

        // Generated files keep their {out} path
        remaining.retain(|_, file| {
            let text = slash(file);
            if text.starts_with("{out}") {
                patterns.push(text);
                return false;
            }
            true
        });

        // Whole folder trees, shallowest first
        let mut candidates: Vec<PathBuf> = remaining
            .values()
            .flat_map(|file| {
                file.ancestors()
                    .skip(1)
                    .take_while(|folder| util::path_key(folder) != root_key)
                    .map(Path::to_path_buf)
                    .collect::<Vec<_>>()
            })
            .collect();
        candidates.sort_by_key(|folder| (folder.components().count(), util::path_key(folder)));
        candidates.dedup_by_key(|folder| util::path_key(folder));

        for folder in candidates {
            let prefix = format!("{}/", util::path_key(&folder));
            let listed: Vec<String> = remaining.keys().filter(|key| key.starts_with(&prefix)).cloned().collect();
            if listed.len() < 2 {
                continue;
            }

            let shared = owner.is_some_and(|owner| self.source_owners.iter().any(|(key, name)| name != owner && key.starts_with(&prefix)));
            if shared {
                continue;
            }

            // Every file of those extensions there, minus a few exclusions at most
            let extensions = extensions_of(listed.iter().map(|key| &remaining[key]));
            let present: Vec<&(String, PathBuf)> = self
                .all_files
                .iter()
                .filter(|(key, path)| key.starts_with(&prefix) && extensions.contains(&extension_of(path)))
                .collect();
            let missing: Vec<&PathBuf> = present
                .iter()
                .filter(|(key, _)| !remaining.contains_key(key))
                .map(|(_, path)| path)
                .collect();

            let allowed_exclusions = listed.len() / 4;
            let unexpected = missing.iter().filter(|path| !excluded_keys.contains(&util::path_key(path))).count();
            if missing.len() > allowed_exclusions.max(excluded_keys.len()) || unexpected > allowed_exclusions {
                continue;
            }

            let folder_text = self.path_text(&folder, dir);
            let folder_prefix = if folder_text == "." { String::new() } else { format!("{folder_text}/") };
            patterns.push(format!("{folder_prefix}**/{}", extension_glob(&extensions)));
            for path in missing {
                patterns.push(format!("!{}", self.path_text(path, dir)));
            }
            for key in listed {
                remaining.shift_remove(&key);
            }
        }

        // The rest, one file each
        for file in remaining.values() {
            patterns.push(self.path_text(file, dir));
        }

        patterns
    }

    /// Leaves include folders out when they are what Anvil assumes anyway (include/ public, src/ the target's own),
    /// or when the folder the convention would add is one the target has anyway.
    fn omit_conventional_includes(&self, dir: &Path, kind: &str, blocks: &mut Blocks) {
        let conventional = |folder: &str| {
            if dir.join(folder).is_dir() {
                vec![folder.to_string()]
            } else {
                Vec::new()
            }
        };
        let given = |blocks: &Blocks, key: &str| match blocks.get(&None, key) {
            Some(Value::List(list)) => list.clone(),
            _ => Vec::new(),
        };
        if kind == "custom" {
            return;
        }

        // Public: what dependents see. A program's include/ (public by convention) may also be one of its own folders
        let public_convention = conventional("include");
        let mut public = given(blocks, "public.include");
        let mut private = given(blocks, "include");
        let program = kind == "exe" && public.is_empty();
        if program && !public_convention.is_empty() && public_convention.iter().all(|folder| private.contains(folder)) {
            private.retain(|folder| !public_convention.contains(folder));
            public = public_convention.clone();
            blocks.set(None, "include", Value::List(private.clone()));
        }
        if public == public_convention {
            blocks.remove(&None, "public.include");
        } else if public.is_empty() {
            blocks.set(None, "public.include", Value::List(Vec::new()));
        }

        // Private: the target's own; a headers target compiles nothing, and a folder it sees publicly needs no saying
        let private_convention = conventional("src");
        let seen_anyway = private_convention.iter().all(|folder| public.contains(folder));
        if private == private_convention || (private.is_empty() && (kind == "headers" || seen_anyway)) {
            blocks.remove(&None, "include");
        } else if private.is_empty() {
            blocks.set(None, "include", Value::List(Vec::new()));
        }
    }

    fn convert_compile_flag(&mut self, item: &Item<String>, condition: &Option<String>, converted: &mut TargetOut) {
        let key = if item.public { "public.flags" } else { "flags" };
        if let Some(note) = unconverted_note(&item.value) {
            converted.notes.push(note);
            return;
        }

        match compile_flag_meaning(&item.value) {
            FlagMeaning::Drop => {}
            FlagMeaning::Keep => converted.blocks.push(condition.clone(), key, item.value.clone()),
            FlagMeaning::Warnings(level) => converted.blocks.set(condition.clone(), "warnings", Value::Text(level.into())),
            FlagMeaning::Werror => converted.blocks.set(condition.clone(), "werror", Value::Bool(true)),
            FlagMeaning::Arch(arch) => converted.blocks.set(condition.clone(), "arch", Value::Text(arch.into())),
            FlagMeaning::Standard(standard) => {
                let key = if standard.starts_with("c++") { "std" } else { "c-std" };
                converted.blocks.set(condition.clone(), key, Value::Text(standard))
            }
            FlagMeaning::Define(define) => converted.blocks.push(
                condition.clone(),
                if item.public { "public.defines" } else { "defines" },
                self.define_text(&define),
            ),
            FlagMeaning::Note(note) => {
                if !converted.notes.contains(&note) {
                    converted.notes.push(note);
                }
            }
        }
    }

    /// The library a whole-archive link option names (`$<TARGET_FILE:x>`, or a target's name), as a dependency:
    /// a static library target, or a package.
    fn whole_archive_name(&mut self, option: &str, dir: &Path) -> Option<String> {
        // {target:x}: a static library target, or a package's library
        if let Some(name) = option.strip_prefix("{target:").and_then(|rest| rest.strip_suffix('}')) {
            if let Some(target) = self.cmake.targets.get(name) {
                return (target.kind == TargetKind::Static).then(|| name.to_string());
            }
            return match self.classify_link(name, dir).as_slice() {
                [(None, Link::Dependency(dependency))] => Some(dependency.clone()),
                _ => None,
            };
        }

        let name = option
            .strip_prefix("$<TARGET_FILE:")
            .and_then(|rest| rest.strip_suffix('>'))
            .unwrap_or(option);
        let resolved = self.cmake.resolve_alias(name);
        let is_static = self.cmake.targets.get(&resolved).is_some_and(|target| target.kind == TargetKind::Static);
        match self.classify_link(name, dir).as_slice() {
            [(None, Link::Target(name))] if is_static => Some(name.clone()),
            [(None, Link::Dependency(name))] => Some(name.clone()),
            _ => None,
        }
    }

    fn convert_link_flag(&self, flag: &str, condition: Option<String>, blocks: &mut Blocks) {
        let upper = flag.to_uppercase();

        // A GUI program (an entry through main() makes it gui = "main" once the target is converted)
        if upper == "/SUBSYSTEM:WINDOWS" || flag == "-mwindows" || flag == "-Wl,--subsystem,windows" {
            if blocks.get(&condition, "gui").is_none() {
                blocks.set(condition, "gui", Value::Bool(true));
            }
        } else if upper.starts_with("/ENTRY:") || DROPPED_LINK_FLAGS.iter().any(|dropped| dropped.eq_ignore_ascii_case(flag)) {
        } else if let Some(library) = flag.strip_prefix("-l") {
            blocks.push(condition, "libs", library.to_string());
        } else if let Some(folder) = flag.strip_prefix("-L") {
            blocks.push(condition, "lib-dirs", folder.to_string());
        } else {
            blocks.push(condition, "link-flags", flag.to_string());
        }
    }

    fn convert_file_rules(&mut self, target: &Target, converted: &mut TargetOut) {
        let source_keys: HashSet<String> = target.sources.iter().map(|item| util::path_key(&item.value)).collect();
        let properties = self.cmake.source_properties.clone();

        for property in properties {
            let files: Vec<&PathBuf> = property.files.iter().filter(|file| source_keys.contains(&util::path_key(file))).collect();
            if files.is_empty() {
                continue;
            }

            let mut rule = FileRuleOut {
                files: files.iter().map(|file| self.path_text(file, &target.dir)).collect(),
                condition: condition_text(&property.condition, condition_text(&target.condition, None).as_deref()),
                defines: property.defines.clone(),
                ..Default::default()
            };
            for flag in &property.options {
                match compile_flag_meaning(flag) {
                    FlagMeaning::Arch(arch) => rule.arch = Some(arch.into()),
                    FlagMeaning::Warnings(level) => rule.warnings = Some(level.into()),
                    FlagMeaning::Define(define) => rule.defines.push(define),
                    FlagMeaning::Drop => {}
                    _ => rule.flags.push(flag.clone()),
                }
            }
            if matches!(rule.arch.as_deref(), Some("avx2" | "avx512")) {
                rule.flags.retain(|flag| !AVX2_IMPLIED_FLAGS.contains(&flag.as_str()));
            }

            converted.files.push(rule);
        }

        // Flags and definitions for the files of one language
        let target_condition = condition_text(&target.condition, None);
        for language_rule in &target.language_rules {
            let folder = self.path_text(&language_rule.folder, &target.dir);
            let prefix = if folder == "." { String::new() } else { format!("{folder}/") };
            let mut rule = FileRuleOut {
                files: vec![format!("{prefix}**/{}", language_rule.pattern)],
                condition: condition_text(&language_rule.condition, target_condition.as_deref()),
                defines: language_rule.defines.iter().map(|define| self.define_text(define)).collect(),
                ..Default::default()
            };
            for flag in &language_rule.options {
                match compile_flag_meaning(flag) {
                    FlagMeaning::Define(define) => rule.defines.push(self.define_text(&define)),
                    FlagMeaning::Drop => {}
                    _ => rule.flags.push(flag.clone()),
                }
            }
            if !rule.defines.is_empty() || !rule.flags.is_empty() {
                converted.files.push(rule);
            }
        }

        converted.files = merge_file_rules(std::mem::take(&mut converted.files));
    }

    // Generated headers

    /// generate_export_header: the header is written into the project, where CMake put it in the build folder.
    /// generate_export_header becomes `export-header` (Anvil writes it and sets its defines); a header with the
    /// project's own content is written into the project instead, with the defines spelled out.
    fn convert_export_headers(&mut self) {
        for header in self.cmake.export_headers.clone() {
            let Some(target) = self.cmake.targets.get_mut(&header.target) else {
                continue;
            };
            let base = crate::export_header::base(&header.macro_name).to_string();

            // CMake's static switch is Anvil's default
            let static_define = format!("{}_STATIC_DEFINE", header.base.to_uppercase());
            target.defines.retain(|item| !item.value.eq_ignore_ascii_case(&static_define));

            // The include folder in the build folder that holds it, and the path below that folder
            let holder = target.includes.iter().position(|item| {
                let folder = format!("{}/", slash(&item.value.0).trim_end_matches('/'));
                header.generated_path.starts_with(&folder)
            });
            if let (Some(index), true) = (holder, header.custom_content.is_empty()) {
                let folder = slash(&target.includes[index].value.0);
                let relative = header.generated_path[folder.trim_end_matches('/').len() + 1..].to_string();
                target.includes.remove(index);
                target.export_header = Some((relative, header.macro_name.clone()));
                continue;
            }

            // Written into the project: exporting while built shared, also when BUILD_SHARED_LIBS decides
            let shared = match (&target.kind, &target.shared_condition) {
                (TargetKind::Shared, _) => Some(None),
                (_, Some(condition)) => Some(Some(condition.clone())),
                _ => None,
            };
            if let Some(condition) = shared {
                for (public, suffix) in [(true, "_SHARED"), (false, "_BUILDING")] {
                    target.defines.push(Item {
                        condition: condition.clone(),
                        public,
                        value: format!("{base}{suffix}"),
                    });
                }
            }

            let path = self.project_location(&header.generated_path);
            self.files
                .push((path, crate::export_header::text(&header.macro_name, &header.custom_content)));
        }
    }

    /// An include folder in the build folder that holds a generated header: the matching folder in the project.
    fn generated_folder(&self, folder: &Path) -> Option<PathBuf> {
        let text = slash(folder);
        if !text.starts_with("{out}") {
            return None;
        }

        let prefix = format!("{}/", text.trim_end_matches('/'));
        self.cmake
            .export_headers
            .iter()
            .any(|header| header.generated_path.starts_with(&prefix))
            .then(|| self.project_location(&text))
    }

    /// Where a file CMake generates into `{out}/...` lives in the project instead.
    fn project_location(&self, generated: &str) -> PathBuf {
        match generated.strip_prefix("{out}") {
            Some(rest) => self.root.join(rest.trim_start_matches('/')),
            None => PathBuf::from(generated),
        }
    }

    // Links and dependencies

    /// What a target_link_libraries item is, with any condition it adds (Threads only matters on unix).
    fn classify_link(&mut self, item: &str, dir: &Path) -> Vec<(Option<String>, Link)> {
        if let Some(cached) = self.link_cache.get(item) {
            return cached.clone();
        }

        // $<COMPILE_ONLY:x>: what x links as, for its headers only
        if let Some(inner) = header_dep_name(item) {
            return self
                .classify_link(&inner, dir)
                .into_iter()
                .filter_map(|(condition, link)| match link {
                    Link::Target(name) | Link::Dependency(name) => Some((condition, Link::HeaderOnly(name))),
                    _ => None,
                })
                .collect();
        }

        // `$<$<cond>:a b>` leaves two items in one
        if item.contains(' ') && !Path::new(item).exists() {
            return item.split_whitespace().flat_map(|part| self.classify_link(part, dir)).collect();
        }

        let resolved = self.cmake.resolve_alias(item);
        let lower = item.to_lowercase();
        let has_library_extension = [".lib", ".a", ".so", ".dylib"].iter().any(|extension| lower.ends_with(extension));

        let links = if let Some(target) = self.cmake.targets.get(&resolved) {
            // A target of this project (custom targets only order the build)
            if target.kind == TargetKind::Custom {
                Vec::new()
            } else {
                vec![(None, Link::Target(resolved))]
            }
        } else if let Some(library) = item.strip_prefix("-l") {
            vec![(None, Link::Library(library.to_string()))]
        } else if item.starts_with('-') || (item.starts_with('/') && !has_library_extension) {
            vec![(None, Link::Flag(item.to_string()))]
        } else if has_library_extension || item.contains(['/', '\\']) {
            vec![(None, Link::Library(self.path_text(Path::new(item), dir)))]
        } else if item.contains("::") || self.is_package_name(item) {
            self.resolve_package(item)
        } else {
            // A system library; the unix-only ones get a condition
            match item {
                "dl" | "pthread" | "m" | "rt" => vec![(Some("unix".to_string()), Link::Library(item.to_string()))],
                _ => vec![(None, Link::Library(item.to_string()))],
            }
        };

        self.link_cache.insert(item.to_string(), links.clone());
        links
    }

    fn is_package_name(&self, item: &str) -> bool {
        let wanted = normalize_name(item);
        let lower = item.to_lowercase();

        KNOWN_PORTS.iter().any(|(pattern, _)| *pattern == lower)
            || self.cmake.packages.iter().any(|package| normalize_name(package) == wanted)
            || self.cmake.fetched.keys().any(|name| normalize_name(name) == wanted)
    }

    /// An imported target (`ns::name`) or package name as a [deps] entry.
    fn resolve_package(&mut self, item: &str) -> Vec<(Option<String>, Link)> {
        let lower = item.to_lowercase();

        // System packages
        match lower.as_str() {
            "threads::threads" => return vec![(Some("unix".into()), Link::Library("pthread".into()))],
            // The CUDA runtime links into whatever has .cu files; the toolkit's other libraries are in its lib folder
            "cuda::cudart" | "cuda::cudart_static" | "cuda::toolkit" => return Vec::new(),
            "opengl::gl" | "opengl::opengl" => {
                return vec![
                    (Some("windows".into()), Link::Library("opengl32".into())),
                    (Some("linux".into()), Link::Library("GL".into())),
                ];
            }
            _ => {}
        }

        if let Some(library) = lower.strip_prefix("cuda::") {
            return vec![(None, Link::Library(library.trim_end_matches("_static").to_string()))];
        }

        // pkg_check_modules(X ... IMPORTED_TARGET): PkgConfig::X
        let stripped = lower.strip_prefix("unofficial::").unwrap_or(&lower).to_string();
        let (namespace, short) = stripped
            .split_once("::")
            .map(|(namespace, short)| (namespace.to_string(), short.to_string()))
            .unwrap_or((stripped.clone(), stripped.clone()));
        if namespace == "pkgconfig" {
            let original_short = item.rsplit("::").next().unwrap_or_default();
            let modules = self.cmake.pkg_config.get(original_short).cloned().unwrap_or_default();
            return modules
                .into_iter()
                .map(|module| {
                    let name = module.split(['>', '<', '=']).next().unwrap_or_default().trim().to_string();
                    self.dependencies.insert(name.clone(), DependencyEntry::PkgConfig(name.clone()));
                    (None, Link::Dependency(name))
                })
                .collect();
        }

        let (port, entry) = self.port_for(&lower, &namespace, &short);
        self.dependencies.entry(port.clone()).or_insert(entry);
        vec![(None, Link::Dependency(port))]
    }

    /// The vcpkg port (or git dependency) behind a package: known names, the project's vcpkg.json, FetchContent, the
    /// ports in the vcpkg checkout, and last a guess marked for review.
    fn port_for(&self, full: &str, namespace: &str, short: &str) -> (String, DependencyEntry) {
        let verified = |note: Option<String>| DependencyEntry::Vcpkg { verified: true, note };

        // Known names
        for (pattern, port) in KNOWN_PORTS {
            let matches = match pattern.strip_suffix("::") {
                Some(known_namespace) => namespace == known_namespace,
                None => full == *pattern,
            };
            if matches {
                return (port.to_string(), verified(None));
            }
        }

        // Boost components are separate ports
        if namespace == "boost" {
            let component = if short == "boost" || short == "headers" { "headers" } else { short };
            return (format!("boost-{component}"), verified(None));
        }

        // The project's vcpkg.json
        for candidate in [namespace, short] {
            if let Some(port) = self.manifest_ports.iter().find(|port| normalize_name(port) == normalize_name(candidate)) {
                return (port.clone(), verified(None));
            }
        }

        // Fetched at configure time: vcpkg when it has the port, else git
        let fetched = self
            .cmake
            .fetched
            .iter()
            .find(|(name, _)| normalize_name(name) == normalize_name(namespace) || normalize_name(name) == normalize_name(short));
        if let Some((name, (url, tag))) = fetched {
            let port = name.to_lowercase().replace('_', "-");
            if self.has_port(&port) == Some(true) {
                return (port, verified(Some(format!("fetched with FetchContent/CPM from {url} in CMake"))));
            }
            return (
                name.to_lowercase(),
                DependencyEntry::Git {
                    url: url.clone(),
                    tag: tag.clone(),
                },
            );
        }

        // The ports in the vcpkg checkout
        for candidate in [
            namespace.to_string(),
            namespace.replace('_', "-"),
            short.to_string(),
            short.replace('_', "-"),
        ] {
            if self.has_port(&candidate) == Some(true) {
                return (candidate, verified(None));
            }
        }

        (namespace.replace('_', "-"), DependencyEntry::Vcpkg { verified: false, note: None })
    }

    fn has_port(&self, name: &str) -> Option<bool> {
        self.vcpkg.as_ref().and_then(|instance| instance.has_port(name))
    }

    // Build steps

    /// add_custom_command(OUTPUT ...) as [[run]] steps of the target that uses the output; steps that differ only in
    /// their input file become one step with `for-each`.
    fn convert_custom_commands(&mut self) {
        let commands = self.cmake.custom_commands.clone();
        let mut runs: IndexMap<String, Vec<(RunOut, Option<PathBuf>)>> = IndexMap::new();

        for command in &commands {
            // Pre-build and post-build commands (PRE_LINK runs before compiling in Anvil, which a check does not mind)
            if let (Some(target), Some(stage)) = (&command.target, &command.stage) {
                if !self.targets.contains_key(target) {
                    continue;
                }
                let (step, _) = self.convert_run(command, target);
                let converted = self.targets.get_mut(target).unwrap();
                match stage.as_str() {
                    "POST_BUILD" => converted.post_build.push(step),
                    _ => converted.pre_build.push(step),
                }
                continue;
            }

            let Some(owner) = self.owner_of(command) else {
                self.warnings.push(format!(
                    "a build step making {} is used by no target; not converted",
                    command.outputs.first().map(|output| slash(output)).unwrap_or_default()
                ));
                continue;
            };

            // `cmake -E echo text > file`: a file whose text is known now
            if let Some(text) = echoed_text(command) {
                self.cmake.configure_writes.push(super::model::ConfigureWrite {
                    location: String::new(),
                    path: command.outputs[0].clone(),
                    text: Some(text),
                    condition: condition_text(&command.condition, None),
                });
                continue;
            }

            // A step without outputs cannot be a [[run]] step
            if command.outputs.is_empty() {
                let note = format!("command not converted (a run step needs outputs): {}", command_line(&command.commands));
                self.targets.get_mut(&owner).unwrap().notes.push(note);
                continue;
            }

            let (run, input) = self.convert_run(command, &owner);
            runs.entry(owner).or_default().push((run, input));
        }

        for (owner, owner_runs) in runs {
            let merged = self.merge_runs(&owner, owner_runs);
            self.targets.get_mut(&owner).unwrap().runs.extend(merged);
        }
    }

    /// The target a build step belongs to: the one compiling its output, a custom target depending on it, or the
    /// custom target that owns it.
    fn owner_of(&self, command: &CustomCommand) -> Option<String> {
        if let Some(target) = &command.target {
            return Some(target.clone());
        }

        let output_keys: HashSet<String> = command.outputs.iter().map(|output| util::path_key(output)).collect();
        let by_source = self
            .cmake
            .targets
            .values()
            .find(|target| target.sources.iter().any(|item| output_keys.contains(&util::path_key(&item.value))));

        let by_depends = || {
            self.cmake.targets.values().find(|target| {
                target
                    .depends
                    .iter()
                    .any(|depend| output_keys.contains(&util::path_key(Path::new(depend))))
            })
        };

        // A target the step runs or waits for cannot own it
        let used_by_step = |name: &str| {
            command.depends.iter().any(|depend| self.cmake.resolve_alias(depend) == name)
                || command
                    .commands
                    .iter()
                    .flatten()
                    .any(|word| word.contains(&format!("$<TARGET_FILE:{name}>")) || self.cmake.resolve_alias(word) == name)
        };
        let by_folder = || {
            self.cmake
                .targets
                .values()
                .find(|target| util::path_key(&target.dir) == util::path_key(&command.dir) && !used_by_step(&target.name))
        };

        by_source.or_else(by_depends).or_else(by_folder).map(|target| target.name.clone())
    }

    fn convert_run(&mut self, command: &CustomCommand, owner: &str) -> (RunOut, Option<PathBuf>) {
        let dir = self.cmake.targets[owner].dir.clone();
        let mut run = RunOut {
            message: command.comment.clone(),
            condition: condition_text(&command.condition, self.targets[owner].condition.as_deref()),
            ..Default::default()
        };

        // Input files (not targets) the step reads; headers find_file() looked for are vcpkg's
        let input_files: Vec<PathBuf> = command
            .depends
            .iter()
            .filter(|depend| !self.cmake.target_exists(depend) && !depend.contains("{found-header:"))
            .map(PathBuf::from)
            .collect();
        run.inputs = self.source_patterns(&dir, &input_files, &HashSet::new(), None);
        for depend in command.depends.iter().filter(|depend| depend.contains("{found-header:")) {
            if let Some(header) = found_header(depend).filter(|_| self.uses_vcpkg()) {
                run.inputs.push(format!("{{vcpkg}}/include/{header}"));
            }
        }
        run.outputs = command.outputs.iter().map(|output| self.path_text(output, &dir)).collect();

        // The command: one runs directly, several through the shell
        let mut lines: Vec<Vec<String>> = command
            .commands
            .iter()
            .map(|words| words.iter().map(|word| self.command_word(word)).collect())
            .collect();

        // Anvil creates output folders itself
        let output_texts: Vec<String> = command.outputs.iter().map(|output| self.command_word(&slash(output))).collect();
        lines.retain(|words| !is_output_folder_creation(words, &output_texts));

        if let Some(depfile) = &command.depfile {
            run.depfile = true;
            for line in &mut lines {
                for word in line.iter_mut() {
                    *word = word.replace(depfile.as_str(), "{depfile}");
                }
            }
        }

        // Several lines run through the shell, joined with &&, and so does a line using the shell's operators
        let uses_shell = lines.iter().flatten().any(|word| SHELL_OPERATORS.contains(&word.as_str()));
        if lines.len() == 1 && !uses_shell {
            run.command = lines.remove(0);
        } else {
            run.shell = true;
            run.command = vec![lines
                .iter()
                .map(|words| words.iter().map(|word| shell_quote(word)).collect::<Vec<_>>().join(" "))
                .collect::<Vec<_>>()
                .join(" && ")];
        }

        // Targets the command runs become {target:name}
        for word in &mut run.command {
            if let Some(target) = self.cmake.targets.get(&self.cmake.resolve_alias(word)) {
                if target.kind == TargetKind::Exe {
                    *word = format!("{{target:{}}}", target.name);
                }
            }
        }

        // CMake scripts still run, through cmake on PATH; counted for one warning
        let runs_script = command
            .commands
            .iter()
            .any(|words| words.first().map(String::as_str) == Some("cmake") && words.iter().any(|word| word == "-P"));
        if runs_script {
            self.cmake_scripts += 1;
        }
        if let Some(workdir) = &command.workdir {
            run.workdir = Some(self.command_word(workdir));
        }

        (run, input_files.into_iter().next())
    }

    /// Runs that differ only in their first input become one `for-each` run with {input} and {stem}.
    fn merge_runs(&self, owner: &str, runs: Vec<(RunOut, Option<PathBuf>)>) -> Vec<RunOut> {
        let dir = self.cmake.targets[owner].dir.clone();
        let mut groups: IndexMap<String, (RunOut, Vec<PathBuf>)> = IndexMap::new();
        let mut single = Vec::new();

        for (run, input) in runs {
            let Some(input) = input else {
                single.push(run);
                continue;
            };

            // The run with its input written as {input} and {stem}
            let input_text = self.path_text(&input, &dir);
            let stem = input.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
            let template_word = |word: &String| -> String {
                let replaced = word
                    .replace(&self.command_word(&slash(&input)), "{input}")
                    .replace(&input_text, "{input}");

                // The stem only as a whole file name part (`shade` must not touch `shaders`)
                let with_stem = replaced.replace(&format!("/{stem}."), "/{stem}.");
                if with_stem == stem {
                    "{stem}".to_string()
                } else {
                    with_stem
                }
            };
            let mut template = run.clone();
            template.command = run.command.iter().map(template_word).collect();
            template.outputs = run.outputs.iter().map(template_word).collect();
            template.inputs = run.inputs.iter().skip(1).map(template_word).collect();
            template.message = None;

            let key = format!(
                "{:?}",
                (
                    &template.command,
                    &template.outputs,
                    &template.inputs,
                    &template.condition,
                    template.shell,
                    template.depfile
                )
            );
            groups.entry(key).or_insert_with(|| (template, Vec::new())).1.push(input);
            single.push(run);
        }

        // Groups of two or more replace their single runs
        let mut merged = Vec::new();
        let mut merged_outputs: HashSet<String> = HashSet::new();
        for (_, (mut template, inputs)) in groups {
            if inputs.len() < 2 {
                continue;
            }
            // The file(GLOB) that found them, else the exact files (a pattern of our own could take in unrelated ones)
            let keys: HashSet<String> = inputs.iter().map(|input| util::path_key(input)).collect();
            let glob = self
                .cmake
                .glob_results
                .iter()
                .find(|(_, files)| files.len() == keys.len() && files.iter().all(|file| keys.contains(&util::path_key(Path::new(file)))));
            template.for_each = match glob {
                Some((pattern, _)) => vec![self.path_text(Path::new(pattern), &dir)],
                None => inputs.iter().map(|input| self.path_text(input, &dir)).collect(),
            };
            merged_outputs.extend(inputs.iter().map(|input| util::path_key(input)));
            merged.push(template);
        }

        let dir_for_inputs = dir.clone();
        merged.extend(single.into_iter().filter(|run| {
            let first_input = run
                .inputs
                .first()
                .map(|input| util::path_key(&util::absolute(&dir_for_inputs, Path::new(input))));
            !first_input.is_some_and(|key| merged_outputs.contains(&key))
        }));

        merged
    }

    // Tests

    /// add_test: a program run once becomes a `kind = "test"` target; anything else a [[test]] table.
    fn convert_tests(&mut self) {
        let tests = self.cmake.tests.clone();

        // How often each program is used by a test
        let program_of = |command: &[String]| -> Option<String> {
            let first = command.first()?;
            let name = first.strip_prefix("{target:").and_then(|name| name.strip_suffix('}')).unwrap_or(first);
            let resolved = self.cmake.resolve_alias(name);
            self.cmake
                .targets
                .get(&resolved)
                .filter(|target| target.kind == TargetKind::Exe)
                .map(|target| target.name.clone())
        };
        let mut uses: HashMap<String, usize> = HashMap::new();
        for test in &tests {
            if let Some(program) = program_of(&test.command) {
                *uses.entry(program).or_default() += 1;
            }
        }

        // A program becomes the test itself only when it is a test program: named or kept as one (`foo_tests`,
        // `tests/`), and no other test runs or mentions it. Other programs stay programs, built by `anvil build`.
        let mut mentions: HashMap<String, usize> = HashMap::new();
        for program in uses.keys() {
            let reference = format!("$<TARGET_FILE:{program}>");
            let mentioning = tests
                .iter()
                .filter(|test| test.command.iter().any(|word| word.contains(&reference) || word == program))
                .count();
            mentions.insert(program.clone(), mentioning);
        }
        let is_test_program = |program: &str| {
            let target = &self.cmake.targets[program];
            let dir = slash(&target.dir).to_lowercase();
            program.to_lowercase().contains("test") || dir.split('/').any(|part| part.starts_with("test"))
        };

        // Fixtures: the tests that set each one up
        let mut fixture_setups: HashMap<String, Vec<String>> = HashMap::new();
        for test in &tests {
            for fixture in &test.properties.fixtures_setup {
                fixture_setups.entry(fixture.clone()).or_default().push(test.name.clone());
            }
        }

        // CMake test names to Anvil's (a test target is named after its program)
        let mut anvil_names: HashMap<String, String> = HashMap::new();
        for test in tests {
            if test.properties.disabled {
                self.warnings.push(format!("test `{}` is DISABLED in CMake; not converted", test.name));
                continue;
            }

            let program = program_of(&test.command);
            let workdir = test
                .workdir
                .as_ref()
                .map(|dir| self.test_workdir(&dir.replace("$<CONFIG>", "{config}")))
                .filter(|dir| dir != ".");

            // Run after DEPENDS and after the setup of every fixture it requires; the environment as commands see it,
            // a variable set twice taking its last value as in CTest
            let mut properties = test.properties.clone();
            for (_, value) in &mut properties.env {
                *value = self.command_word(&value.replace("$<CONFIG>", "{config}"));
            }
            let environment: IndexMap<String, String> = properties.env.drain(..).collect();
            properties.env = environment.into_iter().collect();
            for pattern in properties.expect.iter_mut().chain(&mut properties.reject) {
                *pattern = self.command_word(&pattern.replace("$<CONFIG>", "{config}"));
            }
            for fixture in &properties.fixtures_required {
                let setups = fixture_setups.get(fixture).into_iter().flatten();
                properties.after.extend(setups.filter(|setup| **setup != test.name).cloned());
            }

            // The test's own program, run once
            if let Some(program) = &program {
                let converted = self.targets.get_mut(program).unwrap();
                let only_test = uses[program] == 1 && mentions[program] == 1 && is_test_program(program);
                if only_test && condition_text(&test.condition, converted.condition.as_deref()).is_none() {
                    converted.kind = "test";
                    converted.args = test.command[1..].iter().map(|word| word.replace(&self.root_text, "{root}")).collect();
                    converted.workdir = workdir;
                    converted.test = properties;
                    anvil_names.insert(test.name.clone(), program.clone());
                    continue;
                }
            }

            let mut command: Vec<String> = test.command.iter().map(|word| self.command_word(word)).collect();
            if let Some(program) = &program {
                command[0] = format!("{{target:{program}}}");
            }
            anvil_names.insert(test.name.clone(), test.name.clone());
            self.tests.push(TestOut {
                name: test.name.clone(),
                command,
                workdir,
                condition: condition_text(&test.condition, None),
                properties,
                helper: test.helper.clone(),
                loops: test.loops.clone(),
                dir: test.dir.clone(),
            });
        }

        // `after` in Anvil's names; tests that were not converted drop out
        let mut dropped = Vec::new();
        let mut rename = |after: &mut Vec<String>| {
            let mut renamed = Vec::new();
            for name in after.drain(..) {
                match anvil_names.get(&name) {
                    Some(anvil_name) if !renamed.contains(anvil_name) => renamed.push(anvil_name.clone()),
                    Some(_) => {}
                    None => dropped.push(name),
                }
            }
            *after = renamed;
        };
        for target in self.targets.values_mut() {
            rename(&mut target.test.after);
        }
        for test in &mut self.tests {
            rename(&mut test.properties.after);
        }
        for name in dropped.into_iter().collect::<IndexSet<_>>() {
            self.warnings
                .push(format!("tests wait on `{name}`, which was not converted; that ordering is dropped"));
        }
    }

    /// A test's working folder: `{out}` folders as they are, project folders relative to the root.
    fn test_workdir(&self, dir: &str) -> String {
        if dir.starts_with("{out}") {
            return dir.to_string();
        }

        relative_path(Path::new(dir), &self.root)
    }

    /// Custom targets that ended up with no run steps do nothing in Anvil.
    fn drop_empty_custom_targets(&mut self) {
        let empty: HashSet<String> = self
            .targets
            .iter()
            .filter(|(_, target)| target.kind == "custom" && target.runs.is_empty())
            .map(|(name, _)| name.clone())
            .collect();
        if empty.is_empty() {
            return;
        }

        for (name, target) in &self.targets {
            if empty.contains(name) && !target.notes.is_empty() {
                let notes = target.notes.join("; ");
                self.warnings.push(format!("custom target `{name}` was dropped: {notes}"));
            }
        }
        self.targets.retain(|name, _| !empty.contains(name));

        // And references to them
        for target in self.targets.values_mut() {
            for block in target.blocks.blocks.values_mut() {
                for key in ["deps", "public.deps", "needs"] {
                    if let Some(Value::List(list)) = block.get_mut(key) {
                        list.retain(|name| !empty.contains(name));
                    }
                }
            }
            target.blocks.tidy();
        }
    }

    // [defaults] and [project]

    /// Settings every compiled target shares move to [defaults]; settings most of them share (a CMake helper like
    /// `my_target_defaults()`) move to [preset.common], which those targets name.
    fn factor_shared_settings(&mut self) {
        let compiled: Vec<String> = self
            .targets
            .iter()
            .filter(|(_, target)| matches!(target.kind, "exe" | "lib" | "shared" | "object" | "test"))
            .map(|(name, _)| name.clone())
            .collect();
        if compiled.is_empty() {
            return;
        }

        // Shared by all of them; a lone target keeps its settings, except its standard
        let shared: Vec<Entry> = self
            .shared_entries(&compiled)
            .into_iter()
            .filter(|(_, key, _)| compiled.len() >= 2 || *key == "std")
            .collect();
        for entry in self.move_entries(&compiled, shared) {
            self.defaults.add(entry);
        }

        // The shared standard goes to [project], where targets with that standard need not say it again
        if let Some(Value::Text(standard)) = self.defaults.get(&None, "std").cloned() {
            self.defaults.remove(&None, "std");
            for target in self.targets.values_mut() {
                if target.blocks.get(&None, "std") == Some(&Value::Text(standard.clone())) {
                    target.blocks.remove(&None, "std");
                }
            }
            self.project_std = Some(standard);
        }

        self.defaults.tidy();

        // Shared by most of them: named after the helper function that made them all, if one did
        let members = self.preset_members(&compiled);
        let helper = members.first().and_then(|first| self.targets[first].helper.clone());
        let one_helper = helper
            .as_ref()
            .filter(|helper| members.iter().all(|name| self.targets[name].helper.as_ref() == Some(*helper)));
        let name = one_helper.map(|helper| preset_name(helper)).unwrap_or_else(|| PRESET_NAME.to_string());
        self.add_preset(&name, &members);

        // Shared by the targets one helper function made (`my_plugin(...)`): what is left of them
        let mut helpers: IndexMap<String, Vec<String>> = IndexMap::new();
        for name in &compiled {
            if let Some(helper) = &self.targets[name].helper {
                helpers.entry(helper.clone()).or_default().push(name.clone());
            }
        }
        for (helper, members) in helpers {
            let preset = preset_name(&helper);
            if !self.presets.contains_key(&preset) {
                self.add_preset(&preset, &members);
            }
        }

        // Shared by targets made alike (a library built shared and static): what is left of them, public settings
        // and when blocks included
        self.add_sibling_presets();
    }

    /// Presets for targets that still repeat enough of each other's settings: each target with the one sharing most
    /// with it, and every other target having all of that.
    fn add_sibling_presets(&mut self) {
        let names: Vec<String> = self
            .targets
            .iter()
            .filter(|(_, target)| target.kind != "custom")
            .map(|(name, _)| name.clone())
            .collect();
        let mut taken: HashSet<String> = HashSet::new();

        for (index, name) in names.iter().enumerate() {
            if taken.contains(name) {
                continue;
            }

            // The target sharing most with this one
            let mut best: Vec<Entry> = Vec::new();
            for other in names[index + 1..].iter().filter(|other| !taken.contains(*other)) {
                let shared = self.shared_entries_with_keys(&[name.clone(), other.clone()], SIBLING_KEYS);
                if shared.len() > best.len() {
                    best = shared;
                }
            }
            if best.is_empty() {
                continue;
            }

            // Every target left having all of that, if a preset of it is shorter than saying it in each
            let members: Vec<String> = names[index..]
                .iter()
                .filter(|member| !taken.contains(*member))
                .filter(|member| {
                    best.iter()
                        .all(|(condition, key, value)| has_value(&self.targets[*member].blocks, condition, key, value))
                })
                .cloned()
                .collect();
            let entries = self.shared_entries_with_keys(&members, SIBLING_KEYS);
            let preset_lines = written_lines(&entries) + 2 + members.len();
            if members.len() * written_lines(&entries) < preset_lines + MIN_SIBLING_LINES_SAVED {
                continue;
            }

            let mut blocks = Blocks::default();
            for entry in self.move_entries(&members, entries) {
                blocks.add(entry);
            }
            blocks.tidy();

            let preset = self.sibling_preset_name(&members);
            self.presets.insert(preset.clone(), blocks);
            for member in &members {
                self.targets[member].presets.push(preset.clone());
                taken.insert(member.clone());
            }
        }
    }

    /// A preset named after the start its members' names share, in whole words (`pugixml-shared`, `pugixml-static`:
    /// pugixml), not taken by another preset.
    fn sibling_preset_name(&self, members: &[String]) -> String {
        let mut start = members[0].clone();
        for member in &members[1..] {
            let length = start.chars().zip(member.chars()).take_while(|(left, right)| left == right).count();
            start = start.chars().take(length).collect();
        }
        if !members.contains(&start) {
            let word_end = start.rfind(|character: char| !character.is_ascii_alphanumeric()).unwrap_or(0);
            start.truncate(word_end);
        }
        let start = preset_name(start.trim_end_matches(|character: char| !character.is_ascii_alphanumeric()));
        let base = if start.len() >= 3 { start } else { "alike".to_string() };

        let numbered = (2..).map(|number| format!("{base}-{number}"));
        std::iter::once(base.clone())
            .chain(numbered)
            .find(|name| !self.presets.contains_key(name))
            .unwrap()
    }

    /// A preset of the settings all of `members` share, when there are enough of both.
    fn add_preset(&mut self, preset: &str, members: &[String]) {
        let entries = self.shared_entries(members);
        if members.len() < MIN_PRESET_MEMBERS || entries.len() < MIN_PRESET_ENTRIES {
            return;
        }

        let mut blocks = Blocks::default();
        for entry in self.move_entries(members, entries) {
            blocks.add(entry);
        }
        blocks.tidy();
        self.presets.insert(preset.to_string(), blocks);
        for name in members {
            self.targets[name].presets.push(preset.to_string());
        }
    }

    /// The factorable settings of a target, one list item or scalar each.
    fn entries_of(&self, name: &str) -> Vec<Entry> {
        self.entries_with_keys(name, FACTORED_KEYS)
    }

    /// The settings of a target under these keys, one list item or scalar each.
    fn entries_with_keys(&self, name: &str, keys: &[&str]) -> Vec<Entry> {
        let mut entries = Vec::new();

        for (condition, block) in &self.targets[name].blocks.blocks {
            for (key, value) in block.iter().filter(|(key, _)| keys.contains(key)) {
                match value {
                    Value::List(items) => entries.extend(items.iter().map(|item| (condition.clone(), *key, Value::List(vec![item.clone()])))),
                    scalar => entries.push((condition.clone(), *key, scalar.clone())),
                }
            }
        }

        entries
    }

    /// The settings all of `names` have.
    fn shared_entries(&self, names: &[String]) -> Vec<Entry> {
        self.shared_entries_with_keys(names, FACTORED_KEYS)
    }

    /// The settings under these keys all of `names` have.
    fn shared_entries_with_keys(&self, names: &[String], keys: &[&str]) -> Vec<Entry> {
        let Some(first) = names.first() else { return Vec::new() };

        self.entries_with_keys(first, keys)
            .into_iter()
            .filter(|(condition, key, value)| names.iter().all(|name| has_value(&self.targets[name].blocks, condition, key, value)))
            .collect()
    }

    /// The largest group of targets (at least half) sharing the settings most targets have.
    fn preset_members(&self, compiled: &[String]) -> Vec<String> {
        let threshold = MIN_PRESET_MEMBERS.max(compiled.len().div_ceil(2));

        // How many targets have each setting
        let mut counts: IndexMap<String, (Entry, usize)> = IndexMap::new();
        for name in compiled {
            for entry in self.entries_of(name) {
                counts.entry(format!("{entry:?}")).or_insert((entry, 0)).1 += 1;
            }
        }
        let mut core: Vec<(Entry, usize)> = counts.into_values().filter(|(_, count)| *count >= threshold).collect();
        core.sort_by_key(|(_, count)| std::cmp::Reverse(*count));

        // Candidate groups: the targets having each common setting, and those having all of the most common ones
        // (dropping the rarest one by one)
        let holders = |entries: &[(Entry, usize)]| -> Vec<String> {
            compiled
                .iter()
                .filter(|name| {
                    entries
                        .iter()
                        .all(|((condition, key, value), _)| has_value(&self.targets[*name].blocks, condition, key, value))
                })
                .cloned()
                .collect()
        };
        let mut candidates: IndexSet<Vec<String>> = core.iter().map(|entry| holders(std::slice::from_ref(entry))).collect();
        for length in (1..=core.len()).rev() {
            candidates.insert(holders(&core[..length]));
        }

        // The group of enough targets whose preset takes the most settings out of them
        let mut best: (usize, Vec<String>) = (0, Vec::new());
        for members in candidates.into_iter().filter(|members| members.len() >= threshold) {
            let moved = (members.len() - 1) * self.shared_entries(&members).len();
            if moved > best.0 {
                best = (moved, members);
            }
        }

        best.1
    }

    /// Removes the entries from the targets, returning them.
    fn move_entries(&mut self, names: &[String], entries: Vec<Entry>) -> Vec<Entry> {
        for (condition, key, value) in &entries {
            for name in names {
                remove_value(&mut self.targets[name].blocks, condition, key, value);
            }
        }
        for name in names {
            self.targets[name].blocks.tidy();
        }

        entries
    }

    // Output

    fn render(mut self) -> Conversion {
        let mut output = String::new();
        let mut todo_count = 0;

        let _ = writeln!(
            output,
            "# Converted from CMakeLists.txt by `anvil convert`. Review the TODO comments, then delete them."
        );

        // [project]
        let name = self
            .cmake
            .project_name
            .clone()
            .unwrap_or_else(|| self.root.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default());
        let _ = writeln!(output, "\n[project]\nname = {}", quote(&name));
        if let Some(version) = &self.cmake.project_version {
            let _ = writeln!(output, "version = {}", quote(version));
        }
        if let Some(standard) = &self.project_std {
            let _ = writeln!(output, "std = {}", quote(standard));
        }

        // [options]: those a condition uses (the rest controlled what Anvil does not do: installing, fetching). An
        // option the project set() later is still one where conditions written before that use it
        let used_words = self.condition_words();
        let (options, unused): (Vec<(String, bool, String)>, Vec<_>) = self
            .cmake
            .options
            .iter()
            .map(|(name, (default, description))| (name.clone(), *default, description.clone()))
            .partition(|(name, _, _)| used_words.contains(name));
        let unused: Vec<_> = unused
            .into_iter()
            .filter(|(name, _, _)| !self.cmake.overridden_options.contains(name))
            .collect();
        if !unused.is_empty() {
            let names: Vec<&str> = unused.iter().map(|(name, _, _)| name.as_str()).collect();
            self.warnings
                .push(format!("options that control nothing in anvil.toml were left out: {}", names.join(", ")));
        }
        if !options.is_empty() {
            let _ = writeln!(output, "\n[options]");
            for (name, default, description) in &options {
                if description.is_empty() {
                    let _ = writeln!(output, "{} = {default}", key(name));
                } else {
                    let _ = writeln!(output, "{} = {{ default = {default}, description = {} }}", key(name), quote(description));
                }
            }
        }

        // [deps]
        if !self.dependencies.is_empty() {
            let _ = writeln!(output, "\n[deps]");
            for (name, entry) in &self.dependencies {
                let (value, note) = match entry {
                    DependencyEntry::Vcpkg { verified: true, note } => ("\"vcpkg\"".to_string(), note.clone()),
                    DependencyEntry::Vcpkg { verified: false, .. } => ("\"vcpkg\"".to_string(), Some("TODO: check the vcpkg port name".into())),
                    DependencyEntry::Git { url, tag } => {
                        let tag_text = tag.as_ref().map(|tag| format!(", tag = {}", quote(tag))).unwrap_or_default();
                        let value = format!("{{ git = {}{tag_text} }}", quote(url));
                        (
                            value,
                            Some("TODO: a git dependency needs an anvil.toml, or include/sources overrides".into()),
                        )
                    }
                    DependencyEntry::PkgConfig(module) => (format!("{{ pkg-config = {} }}", quote(module)), None),
                };

                // Needed only where the targets using it are: installed only there
                let value = match (self.dependency_condition(name), value.strip_suffix(" }")) {
                    (None, _) => value,
                    (Some(condition), Some(table)) => format!("{table}, if = {} }}", quote(&condition)),
                    (Some(condition), None) => format!("{{ vcpkg = {}, if = {} }}", quote(name), quote(&condition)),
                };
                if note.as_deref().is_some_and(|note| note.starts_with("TODO")) {
                    todo_count += 1;
                }
                let comment = note.map(|note| format!("  # {}", comment_text(&note))).unwrap_or_default();
                let _ = writeln!(output, "{} = {value}{comment}", key(name));

                if !self.manifest_ports.is_empty() && matches!(entry, DependencyEntry::Vcpkg { .. }) && !self.manifest_ports.contains(name) {
                    self.warnings.push(format!(
                        "vcpkg.json does not list `{name}`; add it (anvil add {name}) unless another port brings it in"
                    ));
                }
            }
        }

        // [vcpkg]
        self.render_vcpkg(&mut output);

        // [defaults], then [preset.x] (a table with only sub-tables needs no header of its own)
        let has_own_keys = |blocks: &Blocks| blocks.blocks.get(&None).is_some_and(|block| block.keys().any(|name| !name.contains('.')));
        if has_own_keys(&self.defaults) {
            let _ = writeln!(output, "\n[defaults]");
        }
        render_blocks(&mut output, "defaults", &self.defaults);

        for (name, preset) in &self.presets {
            if has_own_keys(preset) || preset.blocks.is_empty() {
                let _ = writeln!(output, "\n[preset.{}]", key(name));
            }
            render_blocks(&mut output, &format!("preset.{}", key(name)), preset);
        }

        // [profile.x] for configurations other than debug and release
        for profile in self.profiles() {
            let _ = writeln!(output, "\n[profile.{}]\ninherits = \"release\"", key(&profile));
            match profile.as_str() {
                "relwithdebinfo" => output.push_str("debug = true\n"),
                "minsizerel" => output.push_str("opt = \"s\"\n"),
                _ => {}
            }
        }

        // Targets
        for (name, target) in &self.targets {
            todo_count += render_target(&mut output, name, target);
        }

        // [[test]]: here for tests the top folder adds, in a tests.toml beside the CMake file of the others
        let mut test_files: IndexMap<PathBuf, Vec<&TestOut>> = IndexMap::new();
        for test in &self.tests {
            test_files.entry(test.dir.clone()).or_default().push(test);
        }
        let root_key = util::path_key(&self.cmake.root);
        let mut tests_files = Vec::new();
        for (dir, tests) in test_files {
            let tests: Vec<TestOut> = tests.into_iter().cloned().collect();
            if util::path_key(&dir) == root_key {
                todo_count += self.render_tests(&mut output, &tests);
                continue;
            }

            let mut text = format!("# The tests {}/ adds, converted by `anvil convert`.\n", util::rel(&dir, &self.cmake.root));
            todo_count += self.render_tests(&mut text, &tests);
            tests_files.push((dir.join(crate::project::TESTS_FILE_NAME), text));
        }

        // [[write]]: files CMake writes at configure time, when their text is known now
        let mut unconverted_writes = Vec::new();
        for write in &self.cmake.configure_writes {
            let Some(text) = write.text.as_ref().and_then(|text| self.found_header_text(text)) else {
                unconverted_writes.push(write.location.clone());
                continue;
            };

            let path = self.command_word(&slash(&write.path));
            let _ = writeln!(output, "\n[[write]]\npath = {}", quote(&path));
            if let Some(condition) = &write.condition {
                let _ = writeln!(output, "if = {}", quote(condition));
            }
            let _ = writeln!(output, "text = {}", multiline_quote(&text.replace(&self.root_text, "{root}")));
        }

        // What could not be converted
        let mut warnings = self.cmake.warnings.clone();
        warnings.append(&mut self.warnings);
        if self.cmake_scripts > 0 {
            warnings.push(format!(
                "{} build steps run CMake scripts (cmake -P), so they need cmake on PATH; port them to drop CMake entirely",
                self.cmake_scripts
            ));
        }

        if !self.cmake.unknown_commands.is_empty() {
            let names: Vec<&str> = self.cmake.unknown_commands.iter().map(String::as_str).collect();
            warnings.push(format!("commands the converter does not know were skipped: {}", names.join(", ")));
        }
        if let Some(first) = unconverted_writes.first() {
            let count = unconverted_writes.len();
            warnings.push(format!(
                "file(WRITE/GENERATE/CONFIGURE) makes {count} file(s) at configure time whose content depends on the build (first at {first}); not converted"
            ));
        }

        let unknown_genex: IndexSet<&String> = self.cmake.unknown_genex.iter().collect();
        for expression in unknown_genex {
            warnings.push(format!("generator expression kept as text: {expression}"));
        }

        // The same warning from a function called many times is shown once, with a count
        let mut counted: IndexMap<String, usize> = IndexMap::new();
        for warning in warnings {
            *counted.entry(comment_text(&warning)).or_default() += 1;
        }
        let warnings = counted
            .into_iter()
            .map(|(warning, count)| if count > 1 { format!("{warning} (x{count})") } else { warning })
            .collect();
        let warnings = aggregate_warnings(warnings);

        Conversion {
            toml: output,
            warnings,
            tests_files,
            files: self.files,
            target_count: self.targets.len(),
            dependency_count: self.dependencies.len(),
            todo_count,
        }
    }

    /// [[test]] tables: alone, or tests of one kind as one with a matrix where that is clearly shorter.
    fn render_tests(&self, output: &mut String, tests: &[TestOut]) -> usize {
        let convert_word = |word: &str| self.command_word(word);
        let entries = test_groups::group_tests(tests, &test_fields, &convert_word);

        self.render_test_entries(output, entries, true)
    }

    /// Tests alone and in groups; a group that does not read well is written split by its tests' shapes when
    /// `split` (and that is shorter), else test by test. Returns the number of TODOs.
    fn render_test_entries(&self, output: &mut String, entries: Vec<test_groups::Entry>, split: bool) -> usize {
        let convert_word = |word: &str| self.command_word(word);
        let mut todo_count = 0;

        for entry in entries {
            match entry {
                test_groups::Entry::Alone(test) => todo_count += render_test(output, test),
                test_groups::Entry::Group(group, members) => {
                    let mut grouped = String::new();
                    render_test_group(&mut grouped, &group);
                    let mut alone = String::new();
                    let alone_todo_count: usize = members.iter().map(|test| render_test(&mut alone, test)).sum();

                    // Only when it reads well: shorter, and every entry on a line of its own
                    let shorter = grouped.len() * GROUP_SIZE_PERCENT < alone.len() * 100;
                    let readable = grouped.lines().all(|line| line.len() <= GROUP_LINE_WIDTH);
                    if shorter && readable {
                        output.push_str(&grouped);
                        continue;
                    }

                    // Else its alike tests as groups of their own, if any
                    let mut parts = String::new();
                    let parts_todo_count = match split {
                        true => self.render_test_entries(&mut parts, test_groups::split_group(&members, &test_fields, &convert_word), false),
                        false => 0,
                    };
                    if split && parts.len() < alone.len() {
                        output.push_str(&parts);
                        todo_count += parts_todo_count;
                    } else {
                        output.push_str(&alone);
                        todo_count += alone_todo_count;
                    }
                }
            }
        }

        todo_count
    }

    fn render_vcpkg(&self, output: &mut String) {
        // A standard triplet is what Anvil picks anyway (and naming it would pin the platform)
        let triplet = self
            .cmake
            .get_variable("VCPKG_TARGET_TRIPLET")
            .filter(|triplet| !STANDARD_TRIPLETS.contains(&triplet.as_str()));
        let overlay_ports = crate::convert::model::split_list(&self.cmake.get_variable("VCPKG_OVERLAY_PORTS").unwrap_or_default());
        let overlay_triplets = crate::convert::model::split_list(&self.cmake.get_variable("VCPKG_OVERLAY_TRIPLETS").unwrap_or_default());
        if triplet.is_none() && overlay_ports.is_empty() && overlay_triplets.is_empty() {
            return;
        }

        let relative = |paths: &[String]| -> Vec<String> {
            paths
                .iter()
                .map(|path| relative_path(&util::absolute(&self.root, Path::new(path)), &self.root))
                .collect()
        };

        output.push_str("\n[vcpkg]\n");
        if let Some(triplet) = triplet {
            let _ = writeln!(output, "triplet = {}", quote(&triplet));
        }
        if !overlay_ports.is_empty() {
            let _ = writeln!(output, "overlay-ports = {}", list(&relative(&overlay_ports), ""));
        }
        if !overlay_triplets.is_empty() {
            let _ = writeln!(output, "overlay-triplets = {}", list(&relative(&overlay_triplets), ""));
        }
    }

    /// Where a dependency is needed: the conditions of the settings naming it, joined; None when something needs it
    /// always (or nothing names it).
    fn dependency_condition(&self, name: &str) -> Option<String> {
        const DEPENDENCY_KEYS: &[&str] = &["deps", "public.deps", "header-deps", "whole-archive"];

        let mut sets: Vec<(Option<&String>, &Blocks)> = vec![(None, &self.defaults)];
        sets.extend(self.presets.values().map(|preset| (None, preset)));
        sets.extend(self.targets.values().map(|target| (target.condition.as_ref(), &target.blocks)));

        let mut conditions: Vec<String> = Vec::new();
        for (target_condition, blocks) in sets {
            for (condition, block) in &blocks.blocks {
                let names_it = DEPENDENCY_KEYS
                    .iter()
                    .any(|key| matches!(block.get(*key), Some(Value::List(items)) if items.contains(&name.to_string())));
                if !names_it {
                    continue;
                }
                match super::model::and_optional(&target_condition.cloned(), condition) {
                    Some(condition) => conditions.push(condition),
                    None => return None,
                }
            }
        }
        if conditions.is_empty() {
            return None;
        }

        let any = conditions
            .iter()
            .map(|condition| format!("({condition})"))
            .collect::<Vec<_>>()
            .join(" | ");
        condition_text(&Some(any), None)
    }

    /// Profile names used in conditions or listed in CMAKE_CONFIGURATION_TYPES, other than debug and release.
    fn profiles(&self) -> Vec<String> {
        let option_names: HashSet<&str> = self.cmake.options.keys().map(String::as_str).collect();
        let mut profiles: IndexSet<String> = IndexSet::new();

        // CMAKE_CONFIGURATION_TYPES when the project set its own
        let types = self.cmake.get_variable("CMAKE_CONFIGURATION_TYPES").unwrap_or_default();
        if types != "Debug;Release;RelWithDebInfo;MinSizeRel" {
            profiles.extend(
                crate::convert::model::split_list(&types)
                    .iter()
                    .map(|name| super::model::profile_name(name)),
            );
        }

        // A word that is not a known atom or an option names a profile
        for word in self.condition_words() {
            let known = crate::cond::BUILTIN_WORDS.contains(&word.as_str()) || KNOWN_PROFILES.contains(&word.as_str());
            if !known && !option_names.contains(word.as_str()) {
                profiles.insert(word);
            }
        }

        profiles.retain(|profile| profile != "debug" && profile != "release");
        profiles.into_iter().collect()
    }

    /// Every word the output's conditions use.
    fn condition_words(&self) -> IndexSet<String> {
        let mut conditions: Vec<&String> = Vec::new();
        conditions.extend(self.defaults.blocks.keys().flatten());
        conditions.extend(self.presets.values().flat_map(|preset| preset.blocks.keys().flatten()));
        for target in self.targets.values() {
            conditions.extend(target.condition.iter());
            conditions.extend(target.blocks.blocks.keys().flatten());
            conditions.extend(target.files.iter().filter_map(|rule| rule.condition.as_ref()));
            let steps = target.runs.iter().chain(&target.pre_build).chain(&target.post_build);
            conditions.extend(steps.filter_map(|run| run.condition.as_ref()));
        }
        conditions.extend(self.tests.iter().filter_map(|test| test.condition.as_ref()));
        let writes = self.cmake.configure_writes.iter().filter(|write| write.text.is_some());
        conditions.extend(writes.filter_map(|write| write.condition.as_ref()));

        let mut words = IndexSet::new();
        for condition in conditions {
            let split = condition.split(|character: char| !(character.is_ascii_alphanumeric() || "_-.+".contains(character)));
            words.extend(split.filter(|word| !word.is_empty()).map(String::from));
        }

        words
    }

    // Paths

    /// A path as anvil.toml writes it: relative to the target's folder, `{out}` paths unchanged.
    fn path_text(&self, path: &Path, dir: &Path) -> String {
        let text = slash(path);
        if text.starts_with("{out}") {
            return text;
        }

        relative_path(path, dir)
    }

    /// Text with the headers find_file() looked for as files in vcpkg's include folder; None without vcpkg.
    fn found_header_text(&self, text: &str) -> Option<String> {
        let mut result = text.to_string();
        while let Some(header) = found_header(&result) {
            if !self.uses_vcpkg() {
                return None;
            }
            result = result.replace(&format!("{{found-header:{header}}}"), &format!("{{vcpkg}}/include/{header}"));
        }
        Some(result)
    }

    fn uses_vcpkg(&self) -> bool {
        !self.manifest_ports.is_empty() || self.dependencies.values().any(|entry| matches!(entry, DependencyEntry::Vcpkg { .. }))
    }

    /// A command word with the project folder written as {root}.
    fn command_word(&self, word: &str) -> String {
        // A whole path loses its `..` parts first, so equal paths read the same
        let word = match Path::new(word).is_absolute() {
            true => slash(&util::normalize(Path::new(word))),
            false => word.to_string(),
        };
        collapse_parent_segments(&word.replace(&self.root_text, "{root}"))
    }

    /// A definition with {root}, and the project's version as {version} so it follows [project] version.
    fn define_text(&self, define: &str) -> String {
        let text = self.command_word(define);
        match &self.cmake.project_version {
            Some(version) if version.contains('.') => replace_version(&text, version),
            _ => text,
        }
    }
}

/// A note for a definition or flag whose value CMake computes at configure time from something Anvil cannot repeat
/// (`file(TIMESTAMP)`): it is left out, and the note says so in anvil.toml.
fn unconverted_note(value: &str) -> Option<String> {
    let start = value.find("{unconverted:")? + "{unconverted:".len();
    let end = value[start..].find('}')? + start;
    let name = value.split('=').next().unwrap_or(value);
    Some(format!(
        "`{name}` is left out: CMake computes its value at configure time with {}, which Anvil cannot repeat",
        &value[start..end]
    ))
}

/// `a/b/../c` inside a word as `a/c`, so `-DDIR={root}/cmake/../engine` reads `-DDIR={root}/engine`. Leading `..`
/// parts (relative to a folder) stay.
fn collapse_parent_segments(word: &str) -> String {
    // A segment (not `.` or `..`, not a placeholder or drive) followed by `/../`
    let pattern = regex::Regex::new(r"(^|[/=;\s])[^/=;\s{}:.][^/=;\s{}:]*/\.\./").unwrap();
    let mut text = word.to_string();
    loop {
        let collapsed = pattern.replace(&text, "$1").into_owned();
        if collapsed == text {
            return text;
        }
        text = collapsed;
    }
}

/// The header in a `{found-header:NAME}` marker, which find_path() leaves for the folder it looked for.
fn found_header(text: &str) -> Option<String> {
    let start = text.find("{found-header:")? + "{found-header:".len();
    let end = text[start..].find('}')?;
    Some(text[start..start + end].to_string())
}

/// Renders one target: its table, when blocks, file rules and run steps. Returns the number of TODOs.
fn render_target(output: &mut String, name: &str, target: &TargetOut) -> usize {
    let table = format!("target.{}", key(name));
    let _ = writeln!(output, "\n[{table}]\nkind = {}", quote(target.kind));

    if let Some(dir) = &target.dir {
        let _ = writeln!(output, "dir = {}", quote(dir));
    }
    match target.presets.as_slice() {
        [] => {}
        [preset] => {
            let _ = writeln!(output, "preset = {}", quote(preset));
        }
        presets => {
            let _ = writeln!(output, "preset = {}", list(presets, ""));
        }
    }
    if let Some(condition) = &target.condition {
        let _ = writeln!(output, "if = {}", quote(condition));
    }
    if let Some(output_name) = &target.output_name {
        let _ = writeln!(output, "output-name = {}", quote(output_name));
    }
    if let Some((path, macro_name)) = &target.export_header {
        if *macro_name == crate::targets::default_export_macro(name) {
            let _ = writeln!(output, "export-header = {}", quote(path));
        } else {
            let _ = writeln!(output, "export-header = {{ path = {}, macro = {} }}", quote(path), quote(macro_name));
        }
    }
    if let Some(pch) = &target.pch {
        let _ = writeln!(output, "pch = {}", quote(pch));
    }
    if !target.args.is_empty() {
        let _ = writeln!(output, "args = {}", list(&target.args, ""));
    }
    if let Some(workdir) = &target.workdir {
        let _ = writeln!(output, "workdir = {}", quote(workdir));
    }
    let todo_count = render_test_properties(output, &target.test);
    for note in &target.notes {
        let _ = writeln!(output, "# TODO: {}", comment_text(note));
    }

    render_blocks(output, &table, &target.blocks);

    // Per-file rules
    for rule in &target.files {
        let _ = writeln!(output, "\n[[{table}.file]]\nmatch = {}", list(&rule.files, ""));
        if let Some(condition) = &rule.condition {
            let _ = writeln!(output, "if = {}", quote(condition));
        }
        if !rule.defines.is_empty() {
            let _ = writeln!(output, "defines = {}", list(&rule.defines, ""));
        }
        if !rule.flags.is_empty() {
            let _ = writeln!(output, "flags = {}", list(&rule.flags, ""));
        }
        if let Some(arch) = &rule.arch {
            let _ = writeln!(output, "arch = {}", quote(arch));
        }
        if let Some(warnings) = &rule.warnings {
            let _ = writeln!(output, "warnings = {}", quote(warnings));
        }
    }

    // Build steps
    for run in &target.runs {
        let _ = writeln!(output, "\n[[{table}.run]]");
        if !run.for_each.is_empty() {
            let _ = writeln!(output, "for-each = {}", list(&run.for_each, ""));
        }
        if !run.inputs.is_empty() {
            let _ = writeln!(output, "inputs = {}", list(&run.inputs, ""));
        }
        match run.outputs.as_slice() {
            [single] => {
                let _ = writeln!(output, "output = {}", quote(single));
            }
            many => {
                let _ = writeln!(output, "outputs = {}", list(many, ""));
            }
        }
        match run.command.as_slice() {
            [single] => {
                let _ = writeln!(output, "command = {}", quote(single));
            }
            words => {
                let _ = writeln!(output, "command = {}", command_text(words));
            }
        }
        if run.shell {
            output.push_str("shell = true\n");
        }
        if run.depfile {
            output.push_str("depfile = true\n");
        }
        if let Some(message) = &run.message {
            let _ = writeln!(output, "message = {}", quote(message));
        }
        if let Some(workdir) = &run.workdir {
            let _ = writeln!(output, "workdir = {}", quote(workdir));
        }
        if let Some(condition) = &run.condition {
            let _ = writeln!(output, "if = {}", quote(condition));
        }
    }

    // Pre-build and post-build commands
    for (stage, steps) in [("pre-build", &target.pre_build), ("post-build", &target.post_build)] {
        for step in steps {
            let _ = writeln!(output, "\n[[{table}.{stage}]]");
            match step.command.as_slice() {
                [single] => {
                    let _ = writeln!(output, "command = {}", quote(single));
                }
                words => {
                    let _ = writeln!(output, "command = {}", command_text(words));
                }
            }
            if step.shell {
                output.push_str("shell = true\n");
            }
            if let Some(message) = &step.message {
                let _ = writeln!(output, "message = {}", quote(message));
            }
            if let Some(workdir) = &step.workdir {
                let _ = writeln!(output, "workdir = {}", quote(workdir));
            }
            if let Some(condition) = &step.condition {
                let _ = writeln!(output, "if = {}", quote(condition));
            }
        }
    }

    todo_count + target.notes.len()
}

/// Warnings of one kind (one command at several places, custom targets dropped for one reason) as the first in full
/// and where the others are, when there are enough of them.
fn aggregate_warnings(warnings: Vec<String>) -> Vec<String> {
    const MIN_AGGREGATED: usize = 3;
    const LISTED_PLACES: usize = 10;
    let located = regex::Regex::new(r"^(\S+?:\d+): (.*)$").unwrap();
    let named = regex::Regex::new(r"`([^`]*)`").unwrap();

    // Each warning's kind and what tells it from the others of its kind: its place, or the name it is about
    let mut kinds: IndexMap<String, Vec<(String, String)>> = IndexMap::new();
    for warning in warnings {
        let (kind, place) = match (located.captures(&warning), named.captures(&warning)) {
            (Some(captures), _) => {
                let command = captures[2].split(['(', ' ']).next().unwrap_or_default().to_string();
                (format!("at {command}"), captures[1].to_string())
            }
            (None, Some(captures)) => {
                let without_name = warning.replacen(&captures[0], "``", 1);
                let reason = without_name
                    .rsplit_once(": ")
                    .map_or(without_name.as_str(), |(reason, _)| reason)
                    .to_string();
                (reason, captures[1].to_string())
            }
            (None, None) => match warning.split_once(": ") {
                Some((kind, detail)) => (kind.to_string(), detail.to_string()),
                None => (warning.clone(), String::new()),
            },
        };
        kinds.entry(kind).or_default().push((warning, place));
    }

    let mut aggregated = Vec::new();
    for mut members in kinds.into_values() {
        if members.len() < MIN_AGGREGATED {
            aggregated.extend(members.into_iter().map(|(warning, _)| warning));
            continue;
        }

        // The others' places once each, a file named once for its lines in a row (`a.cmake:4, :9`)
        let (first, _) = members.remove(0);
        let distinct: IndexSet<&String> = members.iter().map(|(_, place)| place).collect();
        let mut places: Vec<String> = Vec::new();
        let mut previous_file = "";
        for place in distinct.iter().take(LISTED_PLACES) {
            match place.rsplit_once(':') {
                Some((file, line)) if file == previous_file => places.push(format!(":{line}")),
                Some((file, _)) => {
                    previous_file = file;
                    places.push(place.to_string());
                }
                None => places.push(place.to_string()),
            }
        }
        if distinct.len() > LISTED_PLACES {
            places.push(format!("and {} more", distinct.len() - LISTED_PLACES));
        }
        aggregated.push(format!("{first}\n  {} more like it: {}", members.len(), places.join(", ")));
    }

    aggregated
}

/// `/ENTRY:mainCRTStartup` (or wmain, or through the GNU linker): main() as the entry of a windows program.
fn is_main_entry(lowercase_option: &str) -> bool {
    let option = lowercase_option.strip_prefix("-wl,").unwrap_or(lowercase_option);
    let entry = option.strip_prefix("/entry:").or_else(|| option.strip_prefix("-entry:"));

    matches!(entry, Some("maincrtstartup" | "wmaincrtstartup"))
}

/// About how many lines settings take written in a table: a line per key, and a header (with the blank line before
/// it) per when block or sub-table.
fn written_lines(entries: &[Entry]) -> usize {
    let keys: HashSet<(&Option<String>, &str)> = entries.iter().map(|(condition, key, _)| (condition, *key)).collect();
    let tables: HashSet<(&Option<String>, &str)> = entries
        .iter()
        .map(|(condition, key, _)| (condition, key.split_once('.').map_or("", |(table, _)| table)))
        .filter(|(condition, table)| condition.is_some() || !table.is_empty())
        .collect();

    keys.len() + 2 * tables.len()
}

/// A program's public settings as its own: with nothing linking it, they mean the same.
fn make_private(blocks: &mut Blocks) {
    for block in blocks.blocks.values_mut() {
        let public_keys: Vec<&'static str> = block.keys().copied().filter(|key| key.starts_with("public.")).collect();
        for public_key in public_keys {
            let private_key = &public_key["public.".len()..];
            let Some(value) = block.shift_remove(public_key) else { continue };
            match (block.get_mut(private_key), value) {
                (Some(Value::List(items)), Value::List(public_items)) => {
                    items.extend(public_items.into_iter().filter(|item| !items.contains(item)).collect::<Vec<_>>());
                }
                (None, value) => {
                    block.insert(private_key, value);
                }
                (Some(_), value) => {
                    block.insert(public_key, value);
                }
            }
        }
    }
}

/// A note as one comment line: option markers as the option's name, line breaks as spaces, other control
/// characters (which TOML comments may not hold) dropped.
fn comment_text(note: &str) -> String {
    let marker = regex::Regex::new("\u{1}option-([^\u{2}]*)\u{2}").unwrap();
    let unmarked = marker.replace_all(note, "$1");
    let spaced: String = unmarked
        .chars()
        .filter_map(|character| match character {
            '\n' | '\r' | '\t' => Some(' '),
            _ if character.is_control() => None,
            _ => Some(character),
        })
        .collect();

    spaced.split(' ').filter(|word| !word.is_empty()).collect::<Vec<_>>().join(" ")
}

/// One [[test]] table; returns the number of TODOs.
fn render_test(output: &mut String, test: &TestOut) -> usize {
    let _ = writeln!(
        output,
        "\n[[test]]\nname = {}\ncommand = {}",
        quote(&test.name),
        command_text(&test.command)
    );
    if let Some(workdir) = &test.workdir {
        let _ = writeln!(output, "workdir = {}", quote(workdir));
    }
    if let Some(condition) = &test.condition {
        let _ = writeln!(output, "if = {}", quote(condition));
    }
    render_test_properties(output, &test.properties)
}

/// A test's settings in the order they are written, for grouping (its name aside).
fn test_fields(test: &TestOut) -> Vec<(&'static str, test_groups::Field)> {
    use test_groups::Field;

    let properties = &test.properties;
    let mut fields = vec![("command", Field::List(test.command.clone()))];
    let texts = [
        ("workdir", test.workdir.clone()),
        ("if", test.condition.clone()),
        ("pass-regex", properties.pass_regex.clone()),
        ("fail-regex", properties.fail_regex.clone()),
        ("skip-regex", properties.skip_regex.clone()),
    ];
    fields.extend(texts.into_iter().filter_map(|(name, text)| Some((name, Field::Text(text?)))));

    let literals = [
        ("will-fail", properties.will_fail.then(|| "true".to_string())),
        ("serial", properties.serial.then(|| "true".to_string())),
        ("timeout", properties.timeout.map(|timeout| timeout.to_string())),
        ("skip-exit-code", properties.skip_exit_code.map(|code| code.to_string())),
        ("exit-code", properties.exit_code.map(|code| code.to_string())),
    ];
    fields.extend(literals.into_iter().filter_map(|(name, text)| Some((name, Field::Literal(text?)))));

    let lists = [
        ("expect", &properties.expect),
        ("reject", &properties.reject),
        ("after", &properties.after),
    ];
    for (name, items) in lists.into_iter().filter(|(_, items)| !items.is_empty()) {
        fields.push((name, Field::List(items.clone())));
    }
    if !properties.env.is_empty() {
        fields.push(("env", Field::Env(properties.env.clone())));
    }

    fields
}

/// One [[test]] with a matrix for a group of tests: the template, then each axis's entries.
fn render_test_group(output: &mut String, group: &test_groups::Group) {
    let origin = group.function.as_ref().map(|function| format!(" from {function}()")).unwrap_or_default();
    let _ = writeln!(
        output,
        "
# {} tests{origin}
[[test]]",
        group.count
    );
    if let Some(name) = &group.name {
        let _ = writeln!(output, "name = {}", quote(name));
    }
    for (name, field) in &group.template {
        let _ = writeln!(output, "{name} = {}", field_value(field, false));
    }

    // An entry setting only its axis's parameter is that value
    for axis in &group.axes {
        let entries: Vec<String> = axis
            .entries
            .iter()
            .map(|entry| match entry.as_slice() {
                [(name, test_groups::Field::Text(text))] if *name == axis.name => quote(text),
                _ => {
                    let keys: Vec<String> = entry
                        .iter()
                        .map(|(name, field)| format!("{} = {}", key(name), field_value(field, true)))
                        .collect();
                    format!("{{ {} }}", keys.join(", "))
                }
            })
            .collect();
        let value = if entries.iter().all(|entry| entry.starts_with('"')) {
            pack(entries, "")
        } else {
            format!(
                "[\n{}\n]",
                entries.iter().map(|entry| format!("    {entry},")).collect::<Vec<_>>().join("\n")
            )
        };
        let _ = writeln!(output, "matrix.{} = {value}", key(&axis.name));
    }
}

/// A test setting's value; lists on one line when `inline` (inside an inline table).
fn field_value(field: &test_groups::Field, inline: bool) -> String {
    use test_groups::Field;

    // Inside an inline table text stays on its line: escaped, not a multi-line string
    let one_line = |text: &str| {
        if inline && text.contains(['\n', '\r']) {
            serde_json::to_string(text).unwrap()
        } else {
            quote(text)
        }
    };
    match field {
        Field::Text(text) => one_line(text),
        Field::List(items) if inline => format!("[{}]", items.iter().map(|item| one_line(item)).collect::<Vec<_>>().join(", ")),
        Field::List(items) => list(items, ""),
        Field::Literal(text) => text.clone(),
        Field::Env(entries) => {
            let entries: Vec<String> = entries.iter().map(|(name, value)| format!("{} = {}", key(name), quote(value))).collect();
            format!("{{ {} }}", entries.join(", "))
        }
    }
}

/// A test's judging and scheduling keys, then its notes as TODOs. Returns the number of TODOs.
fn render_test_properties(output: &mut String, properties: &TestProperties) -> usize {
    if properties.will_fail {
        output.push_str("will-fail = true\n");
    }
    if properties.serial {
        output.push_str("serial = true\n");
    }
    if let Some(timeout) = properties.timeout {
        let _ = writeln!(output, "timeout = {timeout}");
    }
    if let Some(pattern) = &properties.pass_regex {
        let _ = writeln!(output, "pass-regex = {}", quote(pattern));
    }
    if let Some(pattern) = &properties.fail_regex {
        let _ = writeln!(output, "fail-regex = {}", quote(pattern));
    }
    if let Some(code) = properties.skip_exit_code {
        let _ = writeln!(output, "skip-exit-code = {code}");
    }
    if let Some(pattern) = &properties.skip_regex {
        let _ = writeln!(output, "skip-regex = {}", quote(pattern));
    }
    if !properties.expect.is_empty() {
        let _ = writeln!(output, "expect = {}", list(&properties.expect, ""));
    }
    if !properties.reject.is_empty() {
        let _ = writeln!(output, "reject = {}", list(&properties.reject, ""));
    }
    if let Some(code) = properties.exit_code {
        let _ = writeln!(output, "exit-code = {code}");
    }
    if !properties.after.is_empty() {
        let _ = writeln!(output, "after = {}", list(&properties.after, ""));
    }

    // Environment as an inline table
    if !properties.env.is_empty() {
        let entries: Vec<String> = properties
            .env
            .iter()
            .map(|(name, value)| format!("{} = {}", key(name), quote(value)))
            .collect();
        let _ = writeln!(output, "env = {{ {} }}", entries.join(", "));
    }

    for note in &properties.notes {
        let _ = writeln!(output, "# TODO: {}", comment_text(note));
    }

    properties.notes.len()
}

/// The unconditional settings under the current table, then one `[table.when."cond"]` per condition.
fn render_blocks(output: &mut String, table: &str, blocks: &Blocks) {
    if let Some(block) = blocks.blocks.get(&None) {
        render_settings(output, table, block);
    }

    for (condition, block) in &blocks.blocks {
        let Some(condition) = condition else { continue };
        let block_table = format!("{table}.when.{}", key(condition));
        let has_own_keys = block.keys().any(|name| !name.contains('.'));
        if has_own_keys {
            let _ = writeln!(output, "\n[{block_table}]");
        }
        render_settings(output, &block_table, block);
    }
}

/// A block's keys, then its [public] and [interface] sub-tables.
fn render_settings(output: &mut String, table: &str, block: &IndexMap<&'static str, Value>) {
    let render_keys = |output: &mut String, names: Vec<(&str, &Value)>| {
        let mut names = names;
        names.sort_by_key(|(name, _)| KEY_ORDER.iter().position(|ordered| ordered == name).unwrap_or(usize::MAX));
        for (name, value) in names {
            let value = match value {
                Value::List(items) => list(items, ""),
                Value::Text(text) => quote(text),
                Value::Bool(flag) => flag.to_string(),
            };
            let _ = writeln!(output, "{name} = {value}");
        }
    };

    let own = block
        .iter()
        .filter(|(name, _)| !name.contains('.'))
        .map(|(name, value)| (*name, value))
        .collect();
    render_keys(output, own);

    for sub_table in SUB_TABLES {
        let prefix = format!("{sub_table}.");
        let keys: Vec<(&str, &Value)> = block
            .iter()
            .filter_map(|(name, value)| Some((name.strip_prefix(prefix.as_str())?, value)))
            .collect();
        if !keys.is_empty() {
            let _ = writeln!(output, "\n[{table}.{sub_table}]");
            render_keys(output, keys);
        }
    }
}

/// A preset named after the CMake function that made its targets: `add_plugin` -> add-plugin.
fn preset_name(helper: &str) -> String {
    helper.to_lowercase().replace('_', "-")
}

/// A command of several words: one line when no word needs quoting and it fits, else a list.
fn command_text(words: &[String]) -> String {
    let plain =
        |word: &String| !word.is_empty() && !word.contains(|character: char| character.is_whitespace() || matches!(character, '"' | '\'' | '\\'));
    let line = words.join(" ");
    if words.len() > 1 && words.iter().all(plain) && line.len() <= COMMAND_LINE_WIDTH {
        quote(&line)
    } else {
        list(words, "")
    }
}

/// A TOML array: on one line when short, else as many items to a line as fit.
fn list(items: &[String], indent: &str) -> String {
    pack(items.iter().map(|item| quote(item)).collect(), indent)
}

/// Written list items: on one line when they fit, else as many to a line as fit.
fn pack(items: Vec<String>, indent: &str) -> String {
    let inline = format!("[{}]", items.join(", "));
    if inline.len() <= INLINE_LIST_WIDTH {
        return inline;
    }

    let mut text = String::from("[\n");
    let mut line = String::new();
    for item in items {
        if !line.is_empty() && indent.len() + 4 + line.len() + item.len() + 1 > LINE_WIDTH {
            let _ = writeln!(text, "{indent}    {}", line.trim_end());
            line.clear();
        }
        line.push_str(&item);
        line.push_str(", ");
    }
    let _ = writeln!(text, "{indent}    {}", line.trim_end().trim_end_matches(','));
    text.push_str(indent);
    text.push(']');

    text
}

fn quote(text: &str) -> String {
    toml::Value::String(text.to_string()).to_string()
}

/// Text as a literal '''...''' string when it can be, quoted otherwise; one plain line ending in a line break stays
/// on one line, the break escaped.
fn multiline_quote(text: &str) -> String {
    let literal = !text.contains("'''")
        && !text
            .chars()
            .any(|character| character.is_control() && character != '\n' && character != '\t');
    let one_plain_line = !text.trim_end_matches('\n').contains(['\n', '"', '\\']);
    if text.contains('\n') && one_plain_line && literal {
        serde_json::to_string(text).unwrap()
    } else if text.contains('\n') && literal {
        format!("'''\n{text}'''")
    } else {
        quote(text)
    }
}

/// A TOML key: bare when it can be, quoted otherwise.
fn key(text: &str) -> String {
    let bare = !text.is_empty()
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-');
    if bare {
        text.to_string()
    } else {
        quote(text)
    }
}

fn has_value(blocks: &Blocks, condition: &Option<String>, key: &str, value: &Value) -> bool {
    match (blocks.get(condition, key), value) {
        (Some(Value::List(items)), Value::List(wanted)) => wanted.iter().all(|item| items.contains(item)),
        (Some(present), wanted) => present == wanted,
        (None, _) => false,
    }
}

fn remove_value(blocks: &mut Blocks, condition: &Option<String>, key: &str, value: &Value) {
    let Some(block) = blocks.blocks.get_mut(condition) else { return };

    match (block.get_mut(key), value) {
        // (a list emptied this way goes: an empty one would say something else)
        (Some(Value::List(items)), Value::List(removed)) => {
            items.retain(|item| !removed.contains(item));
            if items.is_empty() {
                block.shift_remove(key);
            }
        }
        (Some(_), _) => {
            block.shift_remove(key);
        }
        (None, _) => {}
    }
}

/// A condition as anvil.toml writes it, simplified given `assumption` (the target's own condition): None when it
/// always holds there, NEVER when it cannot.
fn condition_text(condition: &Option<String>, assumption: Option<&str>) -> Option<String> {
    let condition = condition.as_ref()?;

    match super::model::simplify(condition, assumption) {
        super::model::Truth::Known(true) => None,
        super::model::Truth::Known(false) => Some(NEVER.into()),
        super::model::Truth::Depends(simplified) => Some(simplified),
    }
}

/// File rules merged: a file with the same settings under several conditions gets one rule under either of them
/// (under `msvc` and `!msvc` is always), then files with the same settings and condition share one rule.
fn merge_file_rules(rules: Vec<FileRuleOut>) -> Vec<FileRuleOut> {
    let settings_key = |rule: &FileRuleOut| format!("{:?}", (&rule.flags, &rule.defines, &rule.arch, &rule.warnings));

    // Per file and settings: the conditions, joined
    let mut per_file: IndexMap<(String, String), (Option<String>, FileRuleOut)> = IndexMap::new();
    for rule in rules {
        for file in &rule.files {
            let template = FileRuleOut {
                files: Vec::new(),
                condition: None,
                ..rule.clone()
            };
            let entry = per_file
                .entry((file.clone(), settings_key(&rule)))
                .or_insert((Some(NEVER.to_string()), template));
            entry.0 = match (&entry.0, &rule.condition) {
                (_, None) | (None, _) => None,
                (Some(previous), Some(condition)) => condition_text(&Some(format!("({previous}) | ({condition})")), None),
            };
        }
    }

    // Per condition and settings: the files
    let mut merged: IndexMap<(Option<String>, String), FileRuleOut> = IndexMap::new();
    for ((file, settings), (condition, template)) in per_file {
        let rule = merged
            .entry((condition.clone(), settings))
            .or_insert(FileRuleOut { condition, ..template });
        rule.files.push(file);
    }

    merged.into_values().collect()
}

/// What a compile flag becomes.
fn compile_flag_meaning(flag: &str) -> FlagMeaning {
    match flag {
        "/W4" | "-Wall" | "-Wextra" | "-Wpedantic" | "-pedantic" => FlagMeaning::Warnings("high"),
        "/Wall" | "-Weverything" => FlagMeaning::Warnings("all"),
        "/W0" | "-w" => FlagMeaning::Warnings("off"),
        "/WX" | "-Werror" => FlagMeaning::Werror,
        "/arch:AVX" | "-mavx" => FlagMeaning::Arch("avx"),
        "/arch:AVX2" | "-mavx2" => FlagMeaning::Arch("avx2"),
        "/arch:AVX512" | "-mavx512f" => FlagMeaning::Arch("avx512"),
        "-march=native" => FlagMeaning::Arch("native"),
        "/MT" | "/MTd" | "/MD" | "/MDd" => FlagMeaning::Note(format!(
            "{flag}: the runtime library comes from the profile (`static-runtime`, `debug-runtime`)"
        )),
        _ if DROPPED_FLAGS.contains(&flag) => FlagMeaning::Drop,
        _ => {
            let standard = flag.strip_prefix("/std:").or_else(|| flag.strip_prefix("-std="));
            if let Some(standard) = standard {
                let standard = standard.replace("gnu++", "c++").replace("c++latest", "c++23").replace("gnu", "c");
                return FlagMeaning::Standard(standard);
            }

            match flag.strip_prefix("-D").or_else(|| flag.strip_prefix("/D")) {
                Some(define) if !define.is_empty() => FlagMeaning::Define(define.to_string()),
                _ => FlagMeaning::Keep,
            }
        }
    }
}

/// Every file under the project, keyed by path_key, skipping build folders and hidden ones.
fn collect_files(root: &Path) -> Vec<(String, PathBuf)> {
    let skipped = |entry: &walkdir::DirEntry| {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        entry.depth() > 0
            && entry.file_type().is_dir()
            && (name.starts_with('.') || name.starts_with("cmake-build") || SKIPPED_FOLDERS.contains(&name.as_str()))
    };

    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| !skipped(entry))
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| (util::path_key(entry.path()), entry.into_path()))
        .collect()
}

fn is_compiled(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| util::is_source_ext(&extension.to_string_lossy()))
}

fn is_resource(path: &Path) -> bool {
    extension_of(path) == "rc"
}

/// The extensions Anvil's root-folder convention (`*.{c,cc,cpp,cxx}`) picks up.
fn is_default_extension(path: &Path) -> bool {
    matches!(extension_of(path).as_str(), "c" | "cc" | "cpp" | "cxx")
}

fn extension_of(path: &Path) -> String {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn extensions_of<'a>(paths: impl Iterator<Item = &'a PathBuf>) -> Vec<String> {
    let mut extensions: Vec<String> = paths.map(|path| extension_of(path)).collect();
    extensions.sort();
    extensions.dedup();

    extensions
}

/// `*.cpp`, or `*.{c,cpp}` for several extensions.
fn extension_glob(extensions: &[String]) -> String {
    match extensions {
        [single] => format!("*.{single}"),
        many => format!("*.{{{}}}", many.join(",")),
    }
}

/// Lowercase without separators, so `nlohmann_json` matches `nlohmann-json`.
fn normalize_name(name: &str) -> String {
    name.to_lowercase().replace(['-', '_', '.'], "")
}

/// `path` relative to `base`, climbing with `..` when needed; the full path when they share nothing.
fn relative_path(path: &Path, base: &Path) -> String {
    let path_text = slash(path);
    let base_text = slash(base);
    let path_parts: Vec<&str> = path_text.trim_end_matches('/').split('/').collect();
    let base_parts: Vec<&str> = base_text.trim_end_matches('/').split('/').collect();

    let same = |left: &str, right: &str| {
        if cfg!(windows) {
            left.eq_ignore_ascii_case(right)
        } else {
            left == right
        }
    };
    let common = path_parts.iter().zip(&base_parts).take_while(|(left, right)| same(left, right)).count();
    if common == 0 {
        return path_text;
    }

    let mut parts: Vec<&str> = vec![".."; base_parts.len() - common];
    parts.extend(&path_parts[common..]);
    if parts.is_empty() {
        return ".".into();
    }

    parts.join("/")
}

/// `cmake -E make_directory <folders>` where every folder holds an output.
fn is_output_folder_creation(words: &[String], outputs: &[String]) -> bool {
    let [program, mode, operation, folders @ ..] = words else { return false };
    let is_cmake = program == "cmake" || program.to_lowercase().ends_with("cmake.exe");

    is_cmake
        && mode == "-E"
        && operation == "make_directory"
        && folders.iter().all(|folder| {
            outputs
                .iter()
                .any(|output| output.starts_with(&format!("{}/", folder.trim_end_matches('/'))))
        })
}

/// The version written as {version} where it stands alone (not inside a longer number like 10.1.0.5).
fn replace_version(text: &str, version: &str) -> String {
    let mut result = String::new();
    let mut rest = text;

    while let Some(start) = rest.find(version) {
        let before = rest[..start].chars().next_back();
        let after = rest[start + version.len()..].chars().next();
        let in_number = |character: Option<char>| character.is_some_and(|character| character.is_ascii_digit() || character == '.');

        result.push_str(&rest[..start]);
        if in_number(before) || in_number(after) {
            result.push_str(version);
        } else {
            result.push_str("{version}");
        }
        rest = &rest[start + version.len()..];
    }
    result.push_str(rest);

    result
}

/// The target in a `{header-dep:NAME}` marker (from $<COMPILE_ONLY:...> or an INTERFACE_INCLUDE_DIRECTORIES
/// property), wherever it sits in the text.
fn header_dep_name(text: &str) -> Option<String> {
    let start = text.find("{header-dep:")? + "{header-dep:".len();
    let end = text[start..].find('}')? + start;

    Some(text[start..end].to_string())
}

fn shell_quote(word: &str) -> String {
    if word.contains([' ', '\t']) {
        format!("\"{word}\"")
    } else {
        word.to_string()
    }
}

/// The text `cmake -E echo <text> > <output>` writes, when that is all a step does (and the text holds no `{`, which
/// [[write]] would read as a placeholder).
fn echoed_text(command: &CustomCommand) -> Option<String> {
    let ([words], [output]) = (command.commands.as_slice(), command.outputs.as_slice()) else {
        return None;
    };
    let [program, mode, operation, rest @ ..] = words.as_slice() else {
        return None;
    };
    let [text @ .., redirect, file] = rest else { return None };

    let is_cmake = program == "cmake" || program.to_lowercase().ends_with("cmake.exe");
    let to_output = util::path_key(Path::new(file)) == util::path_key(output);
    let text = text.join(" ");
    let plain = !text.contains(['{', '}']);
    (is_cmake && mode == "-E" && operation == "echo" && redirect == ">" && to_output && plain).then(|| format!("{text}\n"))
}

fn command_line(commands: &[Vec<String>]) -> String {
    commands.iter().map(|words| words.join(" ")).collect::<Vec<_>>().join(" && ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths() {
        assert_eq!(relative_path(Path::new("C:/p/src/a.cpp"), Path::new("C:/p")), "src/a.cpp");
        assert_eq!(relative_path(Path::new("C:/p/shared/a.cpp"), Path::new("C:/p/app")), "../shared/a.cpp");
        assert_eq!(relative_path(Path::new("C:/p"), Path::new("C:/p")), ".");
    }

    #[test]
    fn flags() {
        assert!(matches!(compile_flag_meaning("/W4"), FlagMeaning::Warnings("high")));
        assert!(matches!(compile_flag_meaning("/std:c++latest"), FlagMeaning::Standard(standard) if standard == "c++23"));
        assert!(matches!(compile_flag_meaning("-DFOO=1"), FlagMeaning::Define(define) if define == "FOO=1"));
        assert!(matches!(compile_flag_meaning("/utf-8"), FlagMeaning::Drop));
        assert!(matches!(compile_flag_meaning("-Wshadow"), FlagMeaning::Keep));
    }

    #[test]
    fn blocks_merge_complementary_scalars() {
        let mut blocks = Blocks::default();
        blocks.set(Some("msvc".into()), "warnings", Value::Text("high".into()));
        blocks.set(Some("!msvc".into()), "warnings", Value::Text("high".into()));
        blocks.tidy();

        assert_eq!(blocks.get(&None, "warnings"), Some(&Value::Text("high".into())));
        assert!(blocks.blocks.get(&Some("msvc".into())).is_none());
    }

    #[test]
    fn notes_are_one_comment_line() {
        assert_eq!(comment_text("a $<BOOL:\u{1}option-OPT\u{2}>\r\n  b\u{7}"), "a $<BOOL:OPT> b");
    }

    #[test]
    fn conditions_simplify() {
        let simplified = condition_text(&Some("(unix & !macos) & unix".into()), None);
        assert_eq!(simplified.as_deref(), Some("unix & !macos"));
        assert_eq!(condition_text(&Some("!!OPT & OPT".into()), None).as_deref(), Some("OPT"));
    }

    #[test]
    fn main_entries() {
        assert!(is_main_entry("/entry:maincrtstartup"));
        assert!(is_main_entry("-wl,/entry:maincrtstartup"));
        assert!(!is_main_entry("/entry:winmaincrtstartup"));
    }

    #[test]
    fn warnings_of_one_kind_are_aggregated() {
        let warnings: Vec<String> = (1..=4)
            .map(|line| format!("a.cmake:{line}: configure_file(x{line}) is not converted"))
            .collect();
        let aggregated = aggregate_warnings(warnings);

        assert_eq!(
            aggregated,
            ["a.cmake:1: configure_file(x1) is not converted\n  3 more like it: a.cmake:2, :3, :4"]
        );
    }

    #[test]
    fn programs_settings_are_their_own() {
        let mut blocks = Blocks::default();
        blocks.set(None, "public.include", Value::List(vec!["inc".into()]));
        blocks.set(None, "include", Value::List(vec!["src".into()]));
        make_private(&mut blocks);

        assert_eq!(blocks.get(&None, "include"), Some(&Value::List(vec!["src".into(), "inc".into()])));
        assert!(blocks.get(&None, "public.include").is_none());
    }
}
