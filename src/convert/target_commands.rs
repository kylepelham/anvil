//! CMake's target, test and dependency commands: add_library, target_*, properties, add_test, add_custom_command,
//! FetchContent and CPM. Every setting keeps the condition it was made under.

use super::builtins::Keywords;
use super::interp::{DirState, Interpreter, SourceProperties};
use super::model::{self, CustomCommand, ExportHeader, Item, LanguageRule, Target, TargetKind, Test, TestProperties, Truth};
use super::parse::Command;
use crate::util::{self, slash};
use indexmap::IndexMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

// Languages enable_language may name that Anvil builds
const SUPPORTED_LANGUAGES: &[&str] = &["C", "CXX", "RC", "ASM", "ASM_MASM", "CUDA"];

// The suffixes of per-configuration variables (CMAKE_CXX_FLAGS_DEBUG), and the profile each is for
const CONFIGURATION_SUFFIXES: &[(&str, Option<&str>)] = &[
    ("", None),
    ("_DEBUG", Some("debug")),
    ("_RELEASE", Some("release")),
    ("_RELWITHDEBINFO", Some("relwithdebinfo")),
    ("_MINSIZEREL", Some("minsizerel")),
];

// Test properties that only label or weigh tests for CTest's scheduler
const IGNORED_TEST_PROPERTIES: &[&str] = &[
    "LABELS",
    "PROCESSORS",
    "COST",
    "TIMEOUT_AFTER_MATCH",
    "TIMEOUT_SIGNAL_NAME",
    "TIMEOUT_SIGNAL_GRACE_PERIOD",
];

// Target properties that change nothing Anvil builds: these names (with any _<CONFIG> suffix), an entry ending in `_`
// is a prefix, one starting with `_` a suffix
const IGNORED_PROPERTIES: &[&str] = &[
    "_OUTPUT_DIRECTORY",
    "VS_",
    "XCODE_",
    "_POSTFIX",
    "MACOSX_",
    "MACHO_",
    "INTERPROCEDURAL_OPTIMIZATION",
    "MAP_IMPORTED_CONFIG_",
    "IMPORTED_CONFIGURATIONS",
    "_VISIBILITY_PRESET",
    "VISIBILITY_INLINES_HIDDEN",
    "FOLDER",
    "_STANDARD_REQUIRED",
    "_EXTENSIONS",
    "LINKER_LANGUAGE",
    "VERSION",
    "SOVERSION",
    "EXPORT_",
    "POSITION_INDEPENDENT_CODE",
    "INSTALL_",
    "PDB_NAME",
    "COMPILE_PDB_NAME",
    "IMPORTED_NO_SONAME",
    "IMPORTED_LINK_INTERFACE_",
    "BUILD_RPATH",
    "SKIP_BUILD_RPATH",
    "BUILD_WITH_INSTALL_RPATH",
    "PUBLIC_HEADER",
    "PRIVATE_HEADER",
    "RESOURCE",
    "LABELS",
    "CXX_SCAN_FOR_MODULES",
    "EXCLUDE_FROM_ALL",
    "FRAMEWORK",
    "FRAMEWORK_VERSION",
    "UNITY_BUILD_",
    "OPTIMIZE_DEPENDENCIES",
    "SYSTEM",
    "ADDITIONAL_CLEAN_FILES",
    "CLEAN_DIRECT_OUTPUT",
];

/// The list-valued settings shared by targets and directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetList {
    Defines,
    Options,
    Links,
    LinkOptions,
    LibDirs,
}

impl TargetList {
    fn of_target(self, target: &mut Target) -> &mut Vec<Item<String>> {
        match self {
            TargetList::Defines => &mut target.defines,
            TargetList::Options => &mut target.options,
            TargetList::Links => &mut target.links,
            TargetList::LinkOptions => &mut target.link_options,
            TargetList::LibDirs => &mut target.lib_dirs,
        }
    }

    /// The target's list, or for definitions and options only its users get, their own list.
    fn of_users(self, target: &mut Target, only_users: bool) -> &mut Vec<Item<String>> {
        match (self, only_users) {
            (TargetList::Defines, true) => &mut target.interface_defines,
            (TargetList::Options, true) => &mut target.interface_options,
            _ => self.of_target(target),
        }
    }

    fn of_dir(self, dir: &mut DirState) -> &mut Vec<Item<String>> {
        match self {
            TargetList::Defines => &mut dir.defines,
            TargetList::Options => &mut dir.options,
            TargetList::Links => &mut dir.links,
            TargetList::LinkOptions => &mut dir.link_options,
            TargetList::LibDirs => &mut dir.lib_dirs,
        }
    }
}

/// Who a setting is for: `public` passes it on to dependents, `only_users` keeps it from the target itself
/// (INTERFACE on a target that compiles).
#[derive(Debug, Clone, Copy, Default)]
struct Scope {
    public: bool,
    only_users: bool,
}

/// target_sources(... FILE_SET name [TYPE type] [BASE_DIRS ...] [FILES ...]).
#[derive(Debug, Default)]
struct FileSet {
    scope: Scope,
    name: String,
    kind: Option<String>,
    base_dirs: Vec<String>,
    files: Vec<String>,
}

impl Interpreter {
    // Creating targets

    pub(super) fn command_add_executable(&mut self, args: &[String]) {
        let Some((name, rest)) = args.split_first() else { return };

        if let [keyword, real, ..] = rest {
            if keyword == "ALIAS" {
                self.aliases.insert(name.clone(), real.clone());
                return;
            }
        }
        if rest.iter().any(|word| word == "IMPORTED") {
            self.create_imported_target(name);
            return;
        }

        let sources: Vec<String> = rest
            .iter()
            .filter(|word| !matches!(word.as_str(), "WIN32" | "MACOSX_BUNDLE" | "EXCLUDE_FROM_ALL"))
            .cloned()
            .collect();
        self.create_target(name, TargetKind::Exe, &sources);
        if rest.iter().any(|word| word == "WIN32") {
            self.targets.get_mut(name.as_str()).unwrap().gui = true;
        }
    }

    pub(super) fn command_add_library(&mut self, args: &[String]) {
        let Some((name, rest)) = args.split_first() else { return };

        if let [keyword, real, ..] = rest {
            if keyword == "ALIAS" {
                self.aliases.insert(name.clone(), real.clone());
                return;
            }
        }
        if rest.iter().any(|word| word == "IMPORTED") {
            self.create_imported_target(name);
            return;
        }

        // The kind: given, or BUILD_SHARED_LIBS (an option leaves it to a condition)
        let shared_by_default = match (self.option_truth("BUILD_SHARED_LIBS"), self.get_variable("BUILD_SHARED_LIBS")) {
            (Some(truth), _) => truth,
            (None, Some(value)) => self.truthiness(&value, false),
            (None, None) => Truth::Known(false),
        };
        let mut kind = if shared_by_default == Truth::Known(true) {
            TargetKind::Shared
        } else {
            TargetKind::Static
        };
        let mut explicit_kind = false;
        let mut sources = Vec::new();
        for word in rest {
            match word.as_str() {
                "STATIC" => kind = TargetKind::Static,
                "SHARED" | "MODULE" => kind = TargetKind::Shared,
                "OBJECT" => kind = TargetKind::Object,
                "INTERFACE" => kind = TargetKind::Interface,
                "UNKNOWN" | "GLOBAL" | "EXCLUDE_FROM_ALL" => {}
                _ => {
                    sources.push(word.clone());
                    continue;
                }
            }
            explicit_kind |= !matches!(word.as_str(), "UNKNOWN" | "GLOBAL" | "EXCLUDE_FROM_ALL");
        }

        self.create_target(name, kind, &sources);
        if let (false, Truth::Depends(condition)) = (explicit_kind, shared_by_default) {
            if let Some(target) = self.targets.get_mut(name.as_str()) {
                target.shared_condition = Some(condition);
            }
        }
    }

    /// A target CMake builds: a second definition under another condition merges into the first.
    fn create_target(&mut self, name: &str, kind: TargetKind, sources: &[String]) {
        let condition = self.current_condition();
        let dir_id = self.current_dir_id();

        // Defined again (if(WIN32) add_library(x ...) else() add_library(x ...)): one target for both
        if let Some(existing) = self.targets.get_mut(name) {
            if dir_id != existing.dir_id && !existing.other_dir_ids.contains(&dir_id) {
                existing.other_dir_ids.push(dir_id);
            }
            existing.condition = match (existing.condition.take(), condition) {
                (Some(first), Some(second)) => match Truth::Depends(first).or(Truth::Depends(second)) {
                    Truth::Depends(either) => Some(either),
                    Truth::Known(_) => None,
                },
                _ => None,
            };

            let previous = self.target_prefixes.get(name).cloned().unwrap_or_default();
            let common: Vec<String> = previous
                .iter()
                .zip(&self.conditions)
                .take_while(|(left, right)| left == right)
                .map(|(left, _)| left.clone())
                .collect();

            // What the first definition set now holds under the conditions only it was under
            let only_first = previous[common.len()..]
                .iter()
                .fold(None, |combined, condition| model::and_optional(&combined, &Some(condition.clone())));
            if only_first.is_some() {
                self.targets.get_mut(name).unwrap().qualify_items(&only_first);
            }

            // Another kind: the second's under the conditions only it is under
            let only_second = self.conditions[common.len()..]
                .iter()
                .fold(None, |combined, condition| model::and_optional(&combined, &Some(condition.clone())));
            self.merge_kind(name, kind, only_first, only_second);

            self.target_prefixes.insert(name.to_string(), common);
            self.add_sources(name, sources);
            return;
        }

        let mut target = Target::new(name, kind, self.current_dir(), self.current_dir_id(), condition);
        target.helper = self.call_stack.last().map(|call| call.function.clone());
        target.std = self.get_variable("CMAKE_CXX_STANDARD").and_then(|standard| standard.parse().ok());
        target.c_std = self.get_variable("CMAKE_C_STANDARD").and_then(|standard| standard.parse().ok());
        if let Some(gpus) = self.get_variable("CMAKE_CUDA_ARCHITECTURES").filter(|gpus| !gpus.is_empty()) {
            target.settings.push(Item {
                condition: None,
                public: false,
                value: ("cuda-arch", gpus),
            });
        }

        // The folder's compile options, libraries, link options and library folders so far (later ones are not
        // the target's, unlike its definitions and include folders, which come at the folder's end)
        if kind != TargetKind::Custom && kind != TargetKind::Interface {
            let folder = &self.dirs[self.current_dir_id()];
            target.options = folder.options.clone();
            target.links = folder.links.clone();
            target.link_options = folder.link_options.clone();
            target.lib_dirs = folder.lib_dirs.clone();
        }
        self.target_prefixes.insert(name.to_string(), self.conditions.clone());
        self.targets.insert(name.to_string(), target);

        self.add_sources(name, sources);
    }

    /// A second definition of another kind: the kind becomes conditional. The target keeps the kind this machine
    /// builds by default (a compiled one over headers); the other is a `when` setting under its own conditions.
    fn merge_kind(&mut self, name: &str, kind: TargetKind, only_first: Option<String>, only_second: Option<String>) {
        let options = self.option_defaults();
        let target = self.targets.get_mut(name).unwrap();
        let previous = target.kind;
        if kind == previous {
            return;
        }

        let holds = |condition: &Option<String>| {
            condition
                .as_ref()
                .map_or(true, |condition| model::holds_on_host(condition, &|name| options.get(name).copied()))
        };
        let compiles = |kind: TargetKind| kind != TargetKind::Interface;
        let second_wins = (compiles(kind) && !compiles(previous)) || (holds(&only_second) && (compiles(kind) || !compiles(previous)));

        let (other_condition, other_kind) = if second_wins {
            target.kind = kind;
            (only_first, previous)
        } else {
            (only_second, kind)
        };
        if other_condition.is_some() {
            target.settings.push(Item {
                condition: other_condition,
                public: false,
                value: ("kind", other_kind.text().to_string()),
            });
        }
    }

    /// add_library(x IMPORTED): a prebuilt library, described by its properties later.
    fn create_imported_target(&mut self, name: &str) {
        let mut target = Target::new(
            name,
            TargetKind::Interface,
            self.current_dir(),
            self.current_dir_id(),
            self.current_condition(),
        );
        target.imported = true;

        self.target_prefixes.insert(name.to_string(), self.conditions.clone());
        self.targets.insert(name.to_string(), target);
    }

    /// Remembers the condition words are added to a flag variable under: every branch of an if() runs, so the
    /// variable's final value holds the words of all of them.
    pub(super) fn note_flag_words(&mut self, name: &str, value: &str) {
        if !is_flag_variable(name) {
            return;
        }

        // The words it gains (one already there counts as added again when it now appears more often). A value
        // may be a list for a moment (separate_arguments), so `;` separates words too
        let condition = self.current_condition();
        let words = |text: &str| util::split_command(&text.replace(';', " "));
        let before = words(&self.get_variable(name).unwrap_or_default());
        let after = words(value);
        let count = |words: &[String], word: &String| words.iter().filter(|other| *other == word).count();
        let mut added: Vec<String> = after.iter().filter(|word| count(&after, word) > count(&before, word)).cloned().collect();
        added.sort();
        added.dedup();

        for word in added {
            let key = (name.to_string(), word);
            match &condition {
                // Added in another branch too: under either
                Some(condition) => {
                    let combined = match self.flag_word_conditions.get(&key) {
                        Some(previous) => match Truth::Depends(previous.clone()).or(Truth::Depends(condition.clone())) {
                            Truth::Depends(either) => Some(either),
                            Truth::Known(_) => None,
                        },
                        None => Some(condition.clone()),
                    };
                    match combined {
                        Some(combined) => self.flag_word_conditions.insert(key, combined),
                        None => self.flag_word_conditions.remove(&key),
                    };
                }
                None => {
                    self.flag_word_conditions.remove(&key);
                }
            }
        }
    }

    /// CMAKE_<LANG>_FLAGS[_<CONFIG>] and the linker flag variables, as the folder's targets' own settings. CMake
    /// reads them at the folder's end, also for targets made before they were set; C flags are for C files only, C++
    /// flags for C++ files.
    pub(super) fn apply_flag_variables(&mut self, dir_id: usize) {
        let flag_items = |variable: &str, extra_condition: Option<&str>| -> Vec<Item<String>> {
            let mut items = Vec::new();
            for (suffix, profile) in CONFIGURATION_SUFFIXES {
                let name = format!("{variable}{suffix}");
                let condition = model::and_optional(&extra_condition.map(String::from), &profile.map(String::from));
                let flags = util::split_command(&self.get_variable(&name).unwrap_or_default());
                items.extend(flags.into_iter().map(|flag| {
                    // The branch the word was added in, or else the compilers that take it
                    let word_condition = match self.flag_word_conditions.get(&(name.clone(), flag.clone())) {
                        Some(branch) => Some(branch.clone()),
                        None => compiler_family_of_flag(&flag).map(String::from),
                    };
                    Item {
                        condition: model::and_optional(&condition, &word_condition),
                        public: false,
                        value: flag,
                    }
                }));
            }
            items
        };

        // Compile flags per language, link flags per kind
        let mut compile_flags = flag_items("CMAKE_C_FLAGS", Some(model::LANGUAGE_C));
        compile_flags.extend(flag_items("CMAKE_CXX_FLAGS", Some(model::LANGUAGE_CXX)));
        let program_link_flags = flag_items("CMAKE_EXE_LINKER_FLAGS", None);
        let library_link_flags = flag_items("CMAKE_SHARED_LINKER_FLAGS", None);

        // rc's include folders (CMAKE_RC_FLAGS /I folder): Anvil gives rc the target's own, so the targets with
        // resource files get them
        let rc_words = util::split_command(&self.get_variable("CMAKE_RC_FLAGS").unwrap_or_default());
        let mut rc_includes = Vec::new();
        let mut words = rc_words.iter();
        while let Some(word) = words.next() {
            let folder = match word.as_str() {
                "/I" | "-I" => words.next().cloned(),
                _ => word.strip_prefix("/I").or_else(|| word.strip_prefix("-I")).map(String::from),
            };
            if let Some(folder) = folder.filter(|folder| !folder.is_empty()) {
                rc_includes.push(Item {
                    condition: None,
                    public: false,
                    value: (self.resolve_path(&folder), false),
                });
            }
        }

        // They come before the target's own, as on CMake's command lines
        for target in self.targets.values_mut().filter(|target| target.dir_id == dir_id && !target.imported) {
            let link_flags = match target.kind {
                TargetKind::Interface | TargetKind::Custom => continue,
                TargetKind::Exe => &program_link_flags[..],
                TargetKind::Shared => &library_link_flags[..],
                TargetKind::Static | TargetKind::Object => &[],
            };
            target.options.splice(0..0, compile_flags.iter().cloned());
            target.link_options.splice(0..0, link_flags.iter().cloned());

            let has_resources = target
                .sources
                .iter()
                .any(|source| source.value.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("rc")));
            if has_resources {
                target.includes.extend(rc_includes.iter().cloned());
            }
        }
    }

    fn add_sources(&mut self, name: &str, values: &[String]) {
        for (condition, value) in self.conditional_values(Some(name), values) {
            // $<TARGET_OBJECTS:x>: x's objects, which a dependency on the object library brings
            if let Some(library) = value.strip_prefix(model::OBJECTS_PREFIX).and_then(|rest| rest.strip_suffix('}')) {
                self.targets.get_mut(name).unwrap().links.push(Item {
                    condition,
                    public: false,
                    value: library.to_string(),
                });
                continue;
            }

            let path = self.resolve_path(&value);
            if self.header_only_sources.contains(&util::path_key(&path)) {
                continue;
            }
            self.targets.get_mut(name).unwrap().sources.push(Item {
                condition,
                public: false,
                value: path,
            });
        }
    }

    // Target settings

    pub(super) fn command_target_sources(&mut self, command: &Command, args: &[String]) {
        let Some((name, rest)) = args.split_first() else { return };
        let Some(target) = self.find_target(command, name) else { return };
        let interface = self.targets[&target].kind == TargetKind::Interface;
        let users_only = self.interface_is_users_only(&target);

        // Plain files, and file sets (FILE_SET name [TYPE type] [BASE_DIRS ...] [FILES ...])
        let mut scope = Scope {
            public: true,
            only_users: false,
        };
        let mut mode = "FILES";
        let mut files = Vec::new();
        let mut file_set: Option<FileSet> = None;
        for word in rest {
            match word.as_str() {
                "PUBLIC" | "INTERFACE" | "PRIVATE" => {
                    self.add_file_set(&target, file_set.take());
                    scope = Scope {
                        public: word != "PRIVATE" || interface,
                        only_users: word == "INTERFACE" && users_only,
                    };
                    mode = "FILES";
                }
                "FILE_SET" => {
                    self.add_file_set(&target, file_set.take());
                    file_set = Some(FileSet { scope, ..Default::default() });
                    mode = "FILE_SET";
                }
                "TYPE" | "BASE_DIRS" | "FILES" => mode = word.as_str(),
                _ => match (mode, file_set.as_mut()) {
                    ("FILE_SET", Some(set)) => set.name = word.clone(),
                    ("TYPE", Some(set)) => set.kind = Some(word.clone()),
                    ("BASE_DIRS", Some(set)) => set.base_dirs.push(word.clone()),
                    ("FILES", Some(set)) => set.files.push(word.clone()),
                    ("FILES", None) => files.push(word.clone()),
                    _ => {}
                },
            }
        }
        self.add_file_set(&target, file_set);

        self.add_sources(&target, &files);
    }

    /// A file set: HEADERS ones put their base folders (by default the current one) on the include path; C++
    /// modules Anvil does not build.
    fn add_file_set(&mut self, target: &str, file_set: Option<FileSet>) {
        let Some(file_set) = file_set else { return };

        match file_set.kind.as_deref().unwrap_or(&file_set.name) {
            "HEADERS" => {
                let base_dirs = match file_set.base_dirs.is_empty() {
                    true => vec![slash(&self.current_dir())],
                    false => file_set.base_dirs,
                };
                for (condition, value) in self.conditional_values(Some(target), &base_dirs) {
                    let path = self.resolve_path(&value);
                    self.add_include(target, file_set.scope, condition, (path, false));
                }
                self.add_sources(target, &file_set.files);
            }
            "CXX_MODULES" => {
                // Not built, and not by default where an option decides it
                let converted = self.targets.get_mut(target).unwrap();
                converted.notes.push(
                    "C++ modules (FILE_SET CXX_MODULES) are not supported: Anvil does not build module interface files; \
                     build the code as ordinary sources and headers, or leave this target out"
                        .into(),
                );
                if let Some((default, _)) = converted.condition.as_ref().and_then(|condition| self.options.get_mut(condition)) {
                    *default = false;
                }
            }
            _ => self.add_sources(target, &file_set.files),
        }
    }

    /// An include folder: the target's own, its and its users' (public), or its users' only.
    fn add_include(&mut self, target: &str, scope: Scope, condition: Option<String>, value: (PathBuf, bool)) {
        let converted = self.targets.get_mut(target).unwrap();
        let item = Item {
            condition,
            public: scope.public,
            value,
        };

        if scope.only_users {
            converted.interface_includes.push(item);
        } else {
            converted.includes.push(item);
        }
    }

    pub(super) fn command_target_include_directories(&mut self, command: &Command, args: &[String]) {
        let Some((name, rest)) = args.split_first() else { return };
        let Some(target) = self.find_target(command, name) else { return };
        let system = rest.iter().any(|word| word == "SYSTEM");

        for (public, raw, only_users) in self.split_scopes(&target, rest) {
            for (condition, value) in self.conditional_values(Some(&target), &[raw]) {
                let path = self.resolve_path(&value);
                self.add_include(&target, Scope { public, only_users }, condition, (path, system));
            }
        }
    }

    /// target_compile_definitions, target_compile_options, target_link_options, target_link_directories.
    pub(super) fn command_target_list(&mut self, command: &Command, args: &[String], list: TargetList) {
        let Some((name, rest)) = args.split_first() else { return };
        let Some(target) = self.find_target(command, name) else { return };

        for (public, raw, interface_only) in self.split_scopes(&target, rest) {
            for (condition, value) in self.conditional_values(Some(&target), &[raw]) {
                let value = self.clean_list_value(list, &value);
                if value.is_empty() {
                    continue;
                }

                // A compiled target's INTERFACE definitions and flags are not its own (FOO_API dllexport inside,
                // dllimport outside)
                let converted = self.targets.get_mut(&target).unwrap();
                list.of_users(converted, interface_only).push(Item { condition, public, value });
            }
        }
    }

    /// target_link_libraries, with the old `debug`/`optimized` prefixes.
    pub(super) fn command_target_link_libraries(&mut self, command: &Command, args: &[String]) {
        let Some((name, rest)) = args.split_first() else { return };
        let Some(target) = self.find_target(command, name) else { return };
        let interface = self.targets[&target].kind == TargetKind::Interface;

        let mut public = true;
        let mut configuration: Option<String> = None;
        for word in rest {
            match word.as_str() {
                "PUBLIC" | "INTERFACE" | "LINK_PUBLIC" | "LINK_INTERFACE_LIBRARIES" => public = true,
                "PRIVATE" | "LINK_PRIVATE" => public = interface,
                "debug" => configuration = Some("debug".into()),
                "optimized" => configuration = Some("!debug".into()),
                "general" => configuration = None,
                _ => {
                    for (condition, value) in self.conditional_values(Some(&target), std::slice::from_ref(word)) {
                        let condition = model::and_optional(&condition, &configuration);
                        let value = match value.strip_prefix(model::OBJECTS_PREFIX).and_then(|rest| rest.strip_suffix('}')) {
                            Some(library) => library.to_string(),
                            None => value,
                        };
                        self.targets.get_mut(&target).unwrap().links.push(Item { condition, public, value });
                    }
                    configuration = None;
                }
            }
        }
    }

    pub(super) fn command_target_compile_features(&mut self, command: &Command, args: &[String]) {
        let Some((name, rest)) = args.split_first() else { return };
        let Some(target) = self.find_target(command, name) else { return };

        for (public, raw) in self.split_visibility(&target, rest) {
            for (condition, feature) in self.conditional_values(Some(&target), &[raw]) {
                self.apply_compile_feature(&target, &feature, public, condition);
            }
        }
    }

    /// cxx_std_NN and c_std_NN: the target's standard (the highest asked for), for C++ its dependents' too when
    /// public. One asked for under a condition holds there, when it is higher.
    fn apply_compile_feature(&mut self, target: &str, feature: &str, public: bool, condition: Option<String>) {
        let (key, number) = match (feature.strip_prefix("cxx_std_"), feature.strip_prefix("c_std_")) {
            (Some(number), _) => ("std", number),
            (_, Some(number)) => ("c-std", number),
            _ => return,
        };
        let Ok(standard) = number.parse::<u32>() else { return };
        if key == "std" && standard < 11 {
            return;
        }
        let converted = self.targets.get_mut(target).unwrap();
        let current = if key == "std" { converted.std } else { converted.c_std };

        if condition.is_some() {
            if current.map_or(true, |current| standard > current) {
                let text = if key == "std" {
                    format!("c++{standard}")
                } else {
                    format!("c{standard}")
                };
                converted.settings.push(Item {
                    condition,
                    public: false,
                    value: (key, text),
                });
            }
            return;
        }

        let raised = Some(current.map_or(standard, |current| current.max(standard)));
        if key == "std" {
            converted.std = raised;
        } else {
            converted.c_std = raised;
        }
        if public && key == "std" {
            converted.public_std = Some(converted.public_std.map_or(standard, |current| current.max(standard)));
        }
    }

    pub(super) fn command_target_precompile_headers(&mut self, command: &Command, args: &[String]) {
        let Some((name, rest)) = args.split_first() else { return };
        let Some(target) = self.find_target(command, name) else { return };

        // REUSE_FROM another target: the same header
        if let [keyword, other, ..] = rest {
            if keyword == "REUSE_FROM" {
                let header = self.find_target(command, other).and_then(|other| self.targets[&other].pch.clone());
                self.targets.get_mut(&target).unwrap().pch = header;
                return;
            }
        }

        // Anvil has one header per target: the first under each condition
        let headers: Vec<String> = self.split_visibility(&target, rest).into_iter().map(|(_, header)| header).collect();
        let mut seen_conditions = Vec::new();
        for (condition, header) in self.conditional_values(Some(&target), &headers) {
            if !seen_conditions.contains(&condition) {
                seen_conditions.push(condition.clone());
                self.set_precompiled_header(&target, &header, condition);
            }
        }
    }

    fn set_precompiled_header(&mut self, target: &str, header: &str, condition: Option<String>) {
        // <vector>-style headers have no file of their own
        if header.starts_with('<') {
            let note = format!("precompiled header {header}: Anvil's pch is a header file in the project; make one that includes it");
            self.targets.get_mut(target).unwrap().notes.push(note);
            return;
        }

        let path = self.resolve_path(header.trim_matches('"'));
        if self.set_single_value(target, "pch", condition, slash(&path)) {
            self.targets.get_mut(target).unwrap().pch = Some(path);
        }
    }

    /// A property with one value (pch, std, output name, ...) set under `condition`: true when it holds always, for the
    /// caller to set the target's own field (replacing the conditional values set before, as in CMake); otherwise a
    /// setting that overrides the field where the condition holds.
    fn set_single_value(&mut self, target: &str, key: &'static str, condition: Option<String>, value: String) -> bool {
        let settings = &mut self.targets.get_mut(target).unwrap().settings;
        if condition.is_none() {
            settings.retain(|item| item.value.0 != key);
            return true;
        }

        settings.push(Item {
            condition,
            public: false,
            value: (key, value),
        });
        false
    }

    // Properties

    pub(super) fn command_set_target_properties(&mut self, command: &Command, args: &[String]) {
        let Some(properties_index) = args.iter().position(|word| word == "PROPERTIES") else {
            return;
        };
        let pairs: Vec<(String, String)> = args[properties_index + 1..]
            .chunks(2)
            .filter(|pair| pair.len() == 2)
            .map(|pair| (pair[0].clone(), pair[1].clone()))
            .collect();

        for name in &args[..properties_index] {
            let Some(target) = self.find_target(command, name) else { continue };
            for (property, value) in &pairs {
                self.set_target_property(&target, property, std::slice::from_ref(value), false);
            }
        }
    }

    /// set_tests_properties(test... PROPERTIES name value ...).
    pub(super) fn command_set_tests_properties(&mut self, command: &Command, args: &[String]) {
        let Some(properties_index) = args.iter().position(|word| word == "PROPERTIES") else {
            return;
        };
        let pairs: Vec<(String, String)> = args[properties_index + 1..]
            .chunks(2)
            .filter(|pair| pair.len() == 2)
            .map(|pair| (pair[0].clone(), pair[1].clone()))
            .collect();

        for name in &args[..properties_index] {
            for (property, value) in &pairs {
                self.set_test_property(command, name, property, std::slice::from_ref(value), false);
            }
        }
    }

    /// aux_source_directory(dir variable): the folder's source files, appended to the variable.
    pub(super) fn command_aux_source_directory(&mut self, args: &[String]) {
        let [dir, variable, ..] = args else { return };
        let folder = self.resolve_path(dir);

        let mut files: Vec<String> = std::fs::read_dir(&folder)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_file() && is_aux_source(path))
            .map(|path| slash(&path))
            .collect();
        files.sort();

        let mut list = model::split_list(&self.get_variable(variable).unwrap_or_default());
        list.extend(files);
        self.set_variable(variable, &list.join(";"));
    }

    /// enable_language: C, C++, resource files and assembly build; anything else is reported.
    pub(super) fn command_enable_language(&mut self, command: &Command, args: &[String]) {
        for language in args.iter().filter(|word| word.as_str() != "OPTIONAL") {
            if !SUPPORTED_LANGUAGES.contains(&language.as_str()) {
                self.warn(
                    command,
                    &format!("enable_language({language}): Anvil does not compile {language}; its sources are skipped"),
                );
            }
        }
    }

    /// set_property(TARGET|SOURCE|DIRECTORY|CACHE ... [APPEND] PROPERTY name values...).
    pub(super) fn command_set_property(&mut self, command: &Command, args: &[String]) {
        let Some(property_index) = args.iter().position(|word| word == "PROPERTY") else {
            return;
        };
        let Some((property, values)) = args[property_index + 1..].split_first() else {
            return;
        };
        let Some((scope, head)) = args[..property_index].split_first() else {
            return;
        };

        let append = head.iter().any(|word| word == "APPEND" || word == "APPEND_STRING");
        let names: Vec<String> = head
            .iter()
            .filter(|word| word.as_str() != "APPEND" && word.as_str() != "APPEND_STRING")
            .cloned()
            .collect();

        match scope.as_str() {
            "TARGET" => {
                for name in &names {
                    if let Some(target) = self.find_target(command, name) {
                        self.set_target_property(&target, property, values, append);
                    }
                }
            }
            "SOURCE" => {
                let keywords = Keywords::parse(&names, &[], &[], &["DIRECTORY", "TARGET_DIRECTORY"]);
                self.add_source_properties(&keywords.unparsed, property, values);
            }
            "DIRECTORY" => {
                let list = match property.as_str() {
                    "COMPILE_OPTIONS" => TargetList::Options,
                    "COMPILE_DEFINITIONS" => TargetList::Defines,
                    "LINK_OPTIONS" => TargetList::LinkOptions,
                    "LINK_DIRECTORIES" => TargetList::LibDirs,
                    "INCLUDE_DIRECTORIES" => {
                        self.add_directory_includes(values, false);
                        return;
                    }
                    _ => return,
                };
                self.command_directory_list(values, list);
            }
            "CACHE" => {
                for name in &names {
                    if property == "VALUE" {
                        self.cache.insert(name.clone(), values.join(";"));
                    }
                }
            }
            "TEST" => {
                for name in &names {
                    self.set_test_property(command, name, property, values, append);
                }
            }
            _ => {}
        }
    }

    /// One test property, on the test of that name defined so far.
    fn set_test_property(&mut self, command: &Command, name: &str, property: &str, values: &[String], append: bool) {
        // The working folder belongs to the test itself
        let workdir = (property == "WORKING_DIRECTORY").then(|| self.resolve_text_path(&values.join(";")));

        let Some(test) = self.tests.iter_mut().rev().find(|test| test.name == name) else {
            self.warn(command, &format!("set_tests_properties: no test named `{name}` (yet)"));
            return;
        };
        if workdir.is_some() {
            test.workdir = workdir;
            return;
        }

        let items: Vec<String> = values.iter().flat_map(|value| model::split_list(value)).collect();
        apply_test_property(&mut test.properties, property, &items, append);
    }

    /// One target property; list properties append when `append`, else replace.
    fn set_target_property(&mut self, target: &str, property: &str, values: &[String], append: bool) {
        let items: Vec<String> = values.iter().flat_map(|value| model::split_list(value)).collect();
        let public = property.starts_with("INTERFACE_");
        let base_property = property.trim_start_matches("INTERFACE_");
        let only_users = public && self.interface_is_users_only(target);

        // List properties: the same lists the target_* commands fill
        let list = match base_property {
            "COMPILE_DEFINITIONS" => Some(TargetList::Defines),
            "COMPILE_OPTIONS" | "COMPILE_FLAGS" => Some(TargetList::Options),
            "LINK_LIBRARIES" => Some(TargetList::Links),
            "LINK_OPTIONS" | "LINK_FLAGS" => Some(TargetList::LinkOptions),
            "LINK_DIRECTORIES" => Some(TargetList::LibDirs),
            _ => None,
        };
        if let Some(list) = list {
            if !append {
                list.of_users(self.targets.get_mut(target).unwrap(), only_users)
                    .retain(|item| item.public != public);
            }

            // COMPILE_FLAGS and LINK_FLAGS are command-line strings, not lists
            let items = if base_property.ends_with("_FLAGS") {
                crate::util::split_command(&items.join(" "))
            } else {
                items
            };
            for (condition, value) in self.conditional_values(Some(target), &items) {
                let value = self.clean_list_value(list, &value);
                list.of_users(self.targets.get_mut(target).unwrap(), only_users)
                    .push(Item { condition, public, value });
            }
            return;
        }

        match base_property {
            "INCLUDE_DIRECTORIES" | "SYSTEM_INCLUDE_DIRECTORIES" => {
                let system = base_property.starts_with("SYSTEM");
                if !append {
                    let converted = self.targets.get_mut(target).unwrap();
                    converted.includes.retain(|item| item.public != public);
                    converted.interface_includes.retain(|item| item.public != public);
                }
                for (condition, value) in self.conditional_values(Some(target), &items) {
                    let path = self.resolve_path(&value);
                    self.add_include(target, Scope { public, only_users }, condition, (path, system));
                }
            }
            "SOURCES" => self.add_sources(target, &items),
            "PRECOMPILE_HEADERS" => {
                if let Some((condition, header)) = self.conditional_values(Some(target), &items).into_iter().next() {
                    self.set_precompiled_header(target, &header, condition);
                }
            }
            "COMPILE_FEATURES" => {
                for (condition, feature) in self.conditional_values(Some(target), &items) {
                    self.apply_compile_feature(target, &feature, public, condition);
                }
            }
            "CXX_STANDARD" | "C_STANDARD" => {
                for (condition, value) in self.conditional_values(Some(target), &items) {
                    let Ok(standard) = value.parse::<u32>() else { continue };
                    let cxx = base_property == "CXX_STANDARD";
                    if cxx && standard < 11 {
                        continue;
                    }

                    let (key, text) = if cxx {
                        ("std", format!("c++{standard}"))
                    } else {
                        ("c-std", format!("c{standard}"))
                    };
                    if self.set_single_value(target, key, condition, text) {
                        let converted = self.targets.get_mut(target).unwrap();
                        if cxx {
                            converted.std = Some(standard);
                        } else {
                            converted.c_std = Some(standard);
                        }
                    }
                }
            }
            "OUTPUT_NAME" | "RUNTIME_OUTPUT_NAME" | "ARCHIVE_OUTPUT_NAME" | "LIBRARY_OUTPUT_NAME" => {
                for (condition, name) in self.conditional_values(Some(target), &items) {
                    if self.set_single_value(target, "output-name", condition, name.clone()) {
                        self.targets.get_mut(target).unwrap().output_name = Some(name);
                    }
                }
            }
            "WIN32_EXECUTABLE" => {
                for (condition, value) in self.conditional_values(Some(target), &items) {
                    let gui = model::constant_truth(&value) == Some(true);
                    if self.set_single_value(target, "gui", condition, gui.to_string()) {
                        self.targets.get_mut(target).unwrap().gui = gui;
                    }
                }
            }

            // What a shared build defines in place of <target>_EXPORTS ("" for nothing)
            "DEFINE_SYMBOL" => self.targets.get_mut(target).unwrap().define_symbol = Some(items.join(";")),
            "WINDOWS_EXPORT_ALL_SYMBOLS" => {
                if model::constant_truth(&items.join(";")) == Some(true) {
                    let note = "WINDOWS_EXPORT_ALL_SYMBOLS: Anvil does not export every symbol of a DLL; mark the API with an \
                                export macro (`export-header`) or list a .def file in `link-flags`";
                    self.targets.get_mut(target).unwrap().notes.push(note.into());
                }
            }
            "MSVC_RUNTIME_LIBRARY" => {
                let note = format!(
                    "MSVC_RUNTIME_LIBRARY {}: use `static-runtime = true` in a profile for the static runtime",
                    model::without_option_markers(&items.join(";"))
                );
                self.targets.get_mut(target).unwrap().notes.push(note);
            }
            "UNITY_BUILD" => {
                let note = "UNITY_BUILD: add `unity = true` to the target for unity builds".to_string();
                self.targets.get_mut(target).unwrap().notes.push(note);
            }
            "CUDA_ARCHITECTURES" => {
                let settings = &mut self.targets.get_mut(target).unwrap().settings;
                settings.retain(|item| item.value.0 != "cuda-arch");
                if !items.is_empty() && model::constant_truth(&items.join(";")) != Some(false) {
                    settings.push(Item {
                        condition: None,
                        public: false,
                        value: ("cuda-arch", items.join(";")),
                    });
                }
            }
            "CUDA_SEPARABLE_COMPILATION" | "CUDA_RESOLVE_DEVICE_SYMBOLS" if items.iter().any(|item| model::constant_truth(item) == Some(true)) => {
                let note = format!("{base_property}: Anvil compiles each .cu file whole (no device linking across files)");
                self.targets.get_mut(target).unwrap().notes.push(note);
            }
            // The file name, exports to loaded modules, files that relink
            "PREFIX" | "SUFFIX" | "ENABLE_EXPORTS" | "LINK_DEPENDS" | "RUNTIME_OUTPUT_DIRECTORY" | "LIBRARY_OUTPUT_DIRECTORY" => {
                let key = match base_property {
                    "PREFIX" => "output-prefix",
                    "SUFFIX" => "output-suffix",
                    "ENABLE_EXPORTS" => "exports",
                    "LINK_DEPENDS" => "link-depends",
                    _ => "output-dir",
                };
                // PREFIX "" is a value too: no prefix
                if items.is_empty() {
                    let condition = self.current_condition();
                    self.targets.get_mut(target).unwrap().settings.push(Item {
                        condition,
                        public: false,
                        value: (key, String::new()),
                    });
                }
                for (condition, value) in self.conditional_values(Some(target), &items) {
                    let value = match key {
                        "link-depends" => slash(&self.resolve_path(&value)),
                        "output-dir" => self.output_dir_text(&value),
                        _ => value,
                    };
                    self.targets.get_mut(target).unwrap().settings.push(Item {
                        condition,
                        public: false,
                        value: (key, value),
                    });
                }
            }
            "AUTOMOC" | "AUTOUIC" | "AUTORCC" => {
                let note = format!("{property}: Qt's code generators are not supported; add run steps for moc/uic/rcc");
                self.targets.get_mut(target).unwrap().notes.push(note);
            }
            imported if imported.starts_with("IMPORTED_LOCATION") || imported.starts_with("IMPORTED_IMPLIB") => {
                self.set_imported_location(target, imported, &items);
            }
            other if is_ignored_property(other) => {}
            _ => {
                let note = format!(
                    "property {property} = \"{}\" is not converted",
                    model::without_option_markers(&items.join(";"))
                );
                self.targets.get_mut(target).unwrap().notes.push(note);
            }
        }
    }

    /// An output folder in Anvil's terms: under the project's default output folder (and its configuration
    /// folder) is under {out}, which already holds the profile.
    fn output_dir_text(&self, value: &str) -> String {
        let folder = slash(&self.resolve_path(value));
        let default = self
            .get_variable("CMAKE_RUNTIME_OUTPUT_DIRECTORY")
            .map(|default| slash(&self.resolve_path(&default)))
            .unwrap_or_else(|| "{out}".to_string());

        let rest = folder.strip_prefix(&default).map(|rest| rest.strip_prefix("/{config}").unwrap_or(rest));
        match rest {
            Some(rest) => format!("{{out}}{rest}"),
            None => folder,
        }
    }

    /// IMPORTED_LOCATION[_<CONFIG>] / IMPORTED_IMPLIB[_<CONFIG>]: the prebuilt file to link.
    fn set_imported_location(&mut self, target: &str, property: &str, items: &[String]) {
        let configuration = match property.rsplit_once('_').map(|(_, suffix)| suffix) {
            Some("DEBUG") => Some("debug".to_string()),
            Some("RELEASE" | "RELWITHDEBINFO" | "MINSIZEREL") => Some("!debug".to_string()),
            _ => None,
        };
        let base = self.condition_for_target(target);
        let condition = model::and_optional(&base, &configuration);

        for item in items {
            let path = slash(&self.resolve_path(item));

            // A DLL runs beside the program but is not linked
            let is_runtime = [".dll", ".so", ".dylib"].iter().any(|extension| path.to_lowercase().ends_with(extension));
            let target = self.targets.get_mut(target).unwrap();
            if is_runtime {
                target.notes.push(format!("runtime library {path}: list it under `runtime` in [deps]"));
                continue;
            }
            target.links.push(Item {
                condition: condition.clone(),
                public: true,
                value: path,
            });
        }
    }

    pub(super) fn command_get_target_property(&mut self, args: &[String]) {
        let [variable, target, property, ..] = args else { return };
        let value = self.target_property(target, property).unwrap_or_else(|| format!("{variable}-NOTFOUND"));

        self.set_variable(variable, &value);
    }

    /// The properties the converter knows the value of.
    pub(super) fn target_property(&self, name: &str, property: &str) -> Option<String> {
        if property == "ALIASED_TARGET" {
            return self.aliases.get(name).cloned();
        }
        let target = self.targets.get(&self.resolve_alias(name))?;
        let unconditional_paths = |paths: Vec<&PathBuf>| paths.iter().map(|path| slash(path)).collect::<Vec<_>>().join(";");

        match property {
            "TYPE" => Some(
                match target.kind {
                    TargetKind::Exe => "EXECUTABLE",
                    TargetKind::Static => "STATIC_LIBRARY",
                    TargetKind::Shared => "SHARED_LIBRARY",
                    TargetKind::Object => "OBJECT_LIBRARY",
                    TargetKind::Interface => "INTERFACE_LIBRARY",
                    TargetKind::Custom => "UTILITY",
                }
                .into(),
            ),
            "NAME" => Some(target.name.clone()),
            "SOURCE_DIR" => Some(slash(&target.dir)),
            "BINARY_DIR" => Some("{out}".into()),
            "IMPORTED" => Some(if target.imported { "TRUE" } else { "FALSE" }.into()),
            "OUTPUT_NAME" => target.output_name.clone(),
            "CXX_STANDARD" => target.std.map(|standard| standard.to_string()),
            "SOURCES" => Some(unconditional_paths(
                target
                    .sources
                    .iter()
                    .filter(|item| item.condition.is_none())
                    .map(|item| &item.value)
                    .collect(),
            )),
            "INCLUDE_DIRECTORIES" => Some(unconditional_paths(target.includes.iter().map(|item| &item.value.0).collect())),
            "INTERFACE_INCLUDE_DIRECTORIES" => Some(unconditional_paths(
                target.includes.iter().filter(|item| item.public).map(|item| &item.value.0).collect(),
            )),
            "LINK_LIBRARIES" => Some(target.links.iter().map(|item| item.value.clone()).collect::<Vec<_>>().join(";")),
            _ => None,
        }
    }

    /// set_source_files_properties(files... [DIRECTORY ...] [TARGET_DIRECTORY ...] PROPERTIES name value ...).
    pub(super) fn command_set_source_files_properties(&mut self, args: &[String]) {
        let Some(properties_index) = args.iter().position(|word| word == "PROPERTIES") else {
            return;
        };
        let keywords = Keywords::parse(&args[..properties_index], &[], &[], &["DIRECTORY", "TARGET_DIRECTORY"]);

        for pair in args[properties_index + 1..].chunks(2) {
            if let [property, value] = pair {
                self.add_source_properties(&keywords.unparsed, property, std::slice::from_ref(value));
            }
        }
    }

    /// Per-file options and defines; matched to the targets compiling those files at the end.
    fn add_source_properties(&mut self, files: &[String], property: &str, values: &[String]) {
        let items: Vec<String> = values.iter().flat_map(|value| model::split_list(value)).collect();
        let paths: Vec<PathBuf> = files
            .iter()
            .flat_map(|file| model::split_list(file))
            .map(|file| self.resolve_path(&file))
            .collect();

        // A file that is only a header: no target compiles it
        if property == "HEADER_FILE_ONLY" {
            if model::constant_truth(&items.join(";")) == Some(true) {
                let keys: HashSet<String> = paths.iter().map(|path| util::path_key(path)).collect();
                for target in self.targets.values_mut() {
                    target.sources.retain(|item| !keys.contains(&util::path_key(&item.value)));
                }
                self.header_only_sources.extend(keys);
            }
            return;
        }

        // A C file compiled as C++, or the other way round: the compiler's switch for it
        if property == "LANGUAGE" {
            let (switches, language) = match items.join(";").as_str() {
                "CXX" => (["/TP", "-xc++"], model::LANGUAGE_CXX),
                "C" => (["/TC", "-xc"], model::LANGUAGE_C),
                _ => return,
            };

            // Only the files whose extension means another language (zstd sets LANGUAGE C on its .c files)
            let files: Vec<PathBuf> = paths.into_iter().filter(|path| source_language(path) != Some(language)).collect();
            if files.is_empty() {
                return;
            }

            let condition = self.current_condition();
            for (compiler, switch) in ["msvc", "!msvc"].iter().zip(switches) {
                self.source_properties.push(SourceProperties {
                    files: files.clone(),
                    condition: model::and_optional(&condition, &Some(compiler.to_string())),
                    options: vec![switch.to_string()],
                    defines: Vec::new(),
                });
            }
            return;
        }

        let (is_options, items) = match property {
            "COMPILE_OPTIONS" => (true, items),
            "COMPILE_FLAGS" => (true, crate::util::split_command(&items.join(" "))),
            "COMPILE_DEFINITIONS" => (false, items),
            _ => return,
        };
        let files = paths;

        // One record per condition the values apply under
        let mut by_condition: IndexMap<Option<String>, Vec<String>> = IndexMap::new();
        for (condition, value) in self.conditional_values(None, &items) {
            by_condition.entry(condition).or_default().push(value);
        }

        for (condition, values) in by_condition {
            let (options, defines) = if is_options { (values, Vec::new()) } else { (Vec::new(), values) };
            self.source_properties.push(SourceProperties {
                files: files.clone(),
                condition,
                options,
                defines,
            });
        }
    }

    // Directory settings

    pub(super) fn command_include_directories(&mut self, args: &[String]) {
        let system = args.iter().any(|word| word == "SYSTEM");
        let dirs: Vec<String> = args
            .iter()
            .filter(|word| !matches!(word.as_str(), "SYSTEM" | "AFTER" | "BEFORE"))
            .cloned()
            .collect();

        self.add_directory_includes(&dirs, system);
    }

    fn add_directory_includes(&mut self, dirs: &[String], system: bool) {
        for (condition, value) in self.conditional_values(None, dirs) {
            let path = self.resolve_path(&value);
            let dir_id = self.current_dir_id();
            self.dirs[dir_id].includes.push(Item {
                condition,
                public: false,
                value: (path, system),
            });
        }
    }

    /// add_compile_definitions, add_compile_options, link_libraries, add_link_options, link_directories.
    pub(super) fn command_directory_list(&mut self, args: &[String], list: TargetList) {
        for (condition, value) in self.conditional_values(None, args) {
            let value = self.clean_list_value(list, &value);
            let dir_id = self.current_dir_id();
            list.of_dir(&mut self.dirs[dir_id]).push(Item {
                condition,
                public: false,
                value,
            });
        }
    }

    /// add_definitions: -D flags are definitions, anything else an option.
    pub(super) fn command_add_definitions(&mut self, args: &[String]) {
        for arg in args {
            let is_define = arg.starts_with("-D") || arg.starts_with("/D");
            let list = if is_define { TargetList::Defines } else { TargetList::Options };
            self.command_directory_list(std::slice::from_ref(arg), list);
        }
    }

    // Tests and build steps

    /// add_test(NAME n COMMAND ... [WORKING_DIRECTORY d]) or the old add_test(n command ...).
    pub(super) fn command_add_test(&mut self, args: &[String]) {
        let (name, words, workdir, expand_lists) = if args.first().map(String::as_str) == Some("NAME") {
            let keywords = Keywords::parse(
                args,
                &["COMMAND_EXPAND_LISTS"],
                &["NAME", "WORKING_DIRECTORY"],
                &["COMMAND", "CONFIGURATIONS"],
            );
            let expand_lists = keywords.has("COMMAND_EXPAND_LISTS");
            (
                keywords.value("NAME"),
                keywords.list("COMMAND"),
                keywords.value("WORKING_DIRECTORY"),
                expand_lists,
            )
        } else {
            (args.first().cloned(), args.iter().skip(1).cloned().collect(), None, false)
        };
        let Some(name) = name else { return };

        // A `;` inside one argument stays in it (CTest passes "-DEXPECT=a;b" whole), unless lists are to be expanded
        let mut command = if expand_lists {
            self.plain_words(&words)
        } else {
            self.plain_words_kept(&words)
        };
        let workdir = workdir.map(|dir| self.resolve_text_path(&dir));
        let condition = self.current_condition();
        let helper = self.call_stack.last().cloned();
        let loops = self.loops.clone();

        // A CMake script that only runs a command and checks it: that command, checked by Anvil
        let mut properties = TestProperties::default();
        let read = |script: &str| std::fs::read_to_string(self.resolve_path(script)).ok();
        if let Some(checked) = super::check_script::convert(&command, &read) {
            command = checked.command;
            properties.expect = checked.expect;
            properties.reject = checked.reject;
            properties.exit_code = Some(checked.exit_code).filter(|code| *code != 0);
        }

        self.tests.push(Test {
            name,
            command,
            workdir,
            condition,
            properties,
            helper,
            loops,
            dir: self.current_dir(),
        });
    }

    /// gtest_discover_tests(target ...) and gtest_add_tests(TARGET target ...): the program is the test.
    pub(super) fn command_gtest_tests(&mut self, args: &[String]) {
        let target_index = if args.first().map(String::as_str) == Some("TARGET") { 1 } else { 0 };
        let Some(target) = args.get(target_index).cloned() else { return };
        let single = [
            "WORKING_DIRECTORY",
            "TEST_PREFIX",
            "TEST_SUFFIX",
            "TIMEOUT",
            "DISCOVERY_TIMEOUT",
            "DISCOVERY_MODE",
        ];
        let keywords = Keywords::parse(&args[target_index + 1..], &[], &single, &["EXTRA_ARGS", "PROPERTIES", "SOURCES"]);

        let mut command = vec![target.clone()];
        command.extend(keywords.list("EXTRA_ARGS"));
        let workdir = keywords.value("WORKING_DIRECTORY").map(|dir| self.resolve_text_path(&dir));
        let condition = self.current_condition();

        // PROPERTIES name value ... apply to the program's run; TIMEOUT is per discovered test, so it is kept as a floor
        let mut properties = TestProperties::default();
        let mut workdir = workdir;
        for pair in keywords.list("PROPERTIES").chunks(2).filter(|pair| pair.len() == 2) {
            if pair[0] == "WORKING_DIRECTORY" {
                workdir = Some(self.resolve_text_path(&pair[1]));
                continue;
            }
            apply_test_property(&mut properties, &pair[0], &model::split_list(&pair[1]), false);
        }

        self.tests.push(Test {
            name: target,
            command,
            workdir,
            condition,
            properties,
            helper: None,
            loops: Vec::new(),
            dir: self.current_dir(),
        });
    }

    /// add_custom_command(OUTPUT ...) is a build step; add_custom_command(TARGET ...) a pre/post-build step.
    pub(super) fn command_add_custom_command(&mut self, command: &Command, args: &[String]) {
        let flags = [
            "VERBATIM",
            "APPEND",
            "USES_TERMINAL",
            "COMMAND_EXPAND_LISTS",
            "CODEGEN",
            "DEPENDS_EXPLICIT_ONLY",
            "PRE_BUILD",
            "PRE_LINK",
            "POST_BUILD",
        ];
        let single = [
            "MAIN_DEPENDENCY",
            "WORKING_DIRECTORY",
            "COMMENT",
            "DEPFILE",
            "JOB_POOL",
            "JOB_SERVER_AWARE",
            "TARGET",
        ];
        let multi = ["OUTPUT", "COMMAND", "DEPENDS", "BYPRODUCTS", "IMPLICIT_DEPENDS"];
        let keywords = Keywords::parse(args, &flags, &single, &multi);

        let commands: Vec<Vec<String>> = keywords
            .groups("COMMAND")
            .iter()
            .map(|words| self.plain_words(words))
            .filter(|words| !words.is_empty())
            .collect();
        let outputs: Vec<PathBuf> = keywords.list("OUTPUT").iter().map(|output| self.resolve_output(output)).collect();

        // APPEND adds commands to the step that makes the same output
        if keywords.has("APPEND") {
            if let Some(existing) = self
                .custom_commands
                .iter_mut()
                .rev()
                .find(|existing| existing.outputs.first() == outputs.first())
            {
                existing.commands.extend(commands);
            }
            return;
        }

        // A step attached to a target
        let target = match keywords.value("TARGET") {
            Some(name) => match self.find_target(command, &name) {
                Some(target) => Some(target),
                None => return,
            },
            None => None,
        };
        let stage = ["PRE_BUILD", "PRE_LINK", "POST_BUILD"]
            .iter()
            .find(|stage| keywords.has(stage))
            .map(|stage| stage.to_string());

        let mut depends: Vec<String> = keywords.value("MAIN_DEPENDENCY").into_iter().collect();
        depends.extend(keywords.list("DEPENDS"));
        let depends = self.plain_words(&depends).iter().map(|depend| self.resolve_dependency(depend)).collect();

        let custom_command = CustomCommand {
            outputs,
            commands,
            depends,
            depfile: keywords.value("DEPFILE").map(|depfile| slash(&self.resolve_output(&depfile))),
            workdir: keywords.value("WORKING_DIRECTORY").map(|dir| self.resolve_text_path(&dir)),
            comment: keywords.value("COMMENT"),
            condition: self.current_condition(),
            dir: self.current_dir(),
            target,
            stage,
        };
        self.custom_commands.push(custom_command);
    }

    /// add_custom_target(name [ALL] [command] [COMMAND ...] [DEPENDS ...] ...).
    pub(super) fn command_add_custom_target(&mut self, args: &[String]) {
        let Some((name, rest)) = args.split_first() else { return };
        let keywords = Keywords::parse(
            rest,
            &["ALL", "VERBATIM", "USES_TERMINAL", "COMMAND_EXPAND_LISTS"],
            &["WORKING_DIRECTORY", "COMMENT", "JOB_POOL"],
            &["COMMAND", "DEPENDS", "BYPRODUCTS", "SOURCES"],
        );

        // Words before the first keyword are a command too
        let mut command_groups = Vec::new();
        if !keywords.unparsed.is_empty() {
            command_groups.push(keywords.unparsed.clone());
        }
        command_groups.extend(keywords.groups("COMMAND"));
        let commands: Vec<Vec<String>> = command_groups
            .iter()
            .map(|words| self.plain_words(words))
            .filter(|words| !words.is_empty())
            .collect();

        self.create_target(name, TargetKind::Custom, &[]);
        let depends: Vec<String> = self
            .plain_words(&keywords.list("DEPENDS"))
            .iter()
            .map(|depend| self.resolve_dependency(depend))
            .collect();
        self.targets.get_mut(name.as_str()).unwrap().depends.extend(depends);

        if !commands.is_empty() {
            let custom_command = CustomCommand {
                outputs: Vec::new(),
                commands,
                depends: Vec::new(),
                depfile: None,
                workdir: keywords.value("WORKING_DIRECTORY").map(|dir| self.resolve_text_path(&dir)),
                comment: keywords.value("COMMENT"),
                condition: self.current_condition(),
                dir: self.current_dir(),
                target: Some(name.clone()),
                stage: None,
            };
            self.custom_commands.push(custom_command);
        }
    }

    pub(super) fn command_add_dependencies(&mut self, command: &Command, args: &[String]) {
        let Some((name, rest)) = args.split_first() else { return };
        let Some(target) = self.find_target(command, name) else { return };

        self.targets.get_mut(&target).unwrap().depends.extend(rest.iter().cloned());
    }

    /// generate_export_header: remembered, so the converter can write the header into the project.
    pub(super) fn command_generate_export_header(&mut self, args: &[String]) {
        let Some((name, rest)) = args.split_first() else { return };
        let single = [
            "BASE_NAME",
            "EXPORT_MACRO_NAME",
            "EXPORT_FILE_NAME",
            "DEPRECATED_MACRO_NAME",
            "NO_EXPORT_MACRO_NAME",
            "STATIC_DEFINE",
            "NO_DEPRECATED_MACRO_NAME",
            "CUSTOM_CONTENT_FROM_VARIABLE",
            "PREFIX_NAME",
            "INCLUDE_GUARD_NAME",
        ];
        let keywords = Keywords::parse(rest, &["DEFINE_NO_DEPRECATED"], &single, &[]);
        let target = self.resolve_alias(name);
        if !self.targets.contains_key(&target) {
            return;
        }

        let base = keywords.value("BASE_NAME").unwrap_or_else(|| target.clone());
        let macro_name = keywords
            .value("EXPORT_MACRO_NAME")
            .unwrap_or_else(|| format!("{}_EXPORT", base.to_uppercase()));
        let file = keywords
            .value("EXPORT_FILE_NAME")
            .unwrap_or_else(|| format!("{}_export.h", base.to_lowercase()));
        let custom_content = keywords
            .value("CUSTOM_CONTENT_FROM_VARIABLE")
            .and_then(|variable| self.get_variable(&variable))
            .unwrap_or_default();

        let generated_path = slash(&self.resolve_output(&file));
        self.export_headers.push(ExportHeader {
            target,
            base,
            macro_name,
            generated_path,
            custom_content,
        });
    }

    // Dependencies fetched at configure time

    pub(super) fn command_fetchcontent_declare(&mut self, args: &[String]) {
        let Some((name, rest)) = args.split_first() else { return };
        let single = [
            "GIT_REPOSITORY",
            "GIT_TAG",
            "URL",
            "URL_HASH",
            "URL_MD5",
            "SOURCE_DIR",
            "GIT_SHALLOW",
            "GIT_PROGRESS",
            "SOURCE_SUBDIR",
            "DOWNLOAD_EXTRACT_TIMESTAMP",
        ];
        let keywords = Keywords::parse(
            rest,
            &["SYSTEM", "EXCLUDE_FROM_ALL", "OVERRIDE_FIND_PACKAGE"],
            &single,
            &["FIND_PACKAGE_ARGS", "PATCH_COMMAND"],
        );

        let url = keywords.value("GIT_REPOSITORY").or_else(|| keywords.value("URL")).unwrap_or_default();
        self.fetched.insert(name.clone(), (url, keywords.value("GIT_TAG")));
    }

    /// FetchContent_MakeAvailable and friends: the variables a populated dependency sets.
    pub(super) fn command_fetchcontent_available(&mut self, args: &[String]) {
        for name in args.iter().filter(|name| self.fetched.contains_key(*name)).cloned().collect::<Vec<_>>() {
            let lower = name.to_lowercase();
            self.set_variable(&format!("{lower}_POPULATED"), "ON");
            self.set_variable(&format!("{lower}_SOURCE_DIR"), &format!("{{out}}/_deps/{lower}-src"));
            self.set_variable(&format!("{lower}_BINARY_DIR"), &format!("{{out}}/_deps/{lower}-build"));
        }
    }

    /// CPMAddPackage("gh:owner/repo@1.2") or CPMAddPackage(NAME x GITHUB_REPOSITORY owner/repo VERSION 1.2 ...).
    pub(super) fn command_cpm_add_package(&mut self, args: &[String]) {
        let single = [
            "NAME",
            "VERSION",
            "GIT_TAG",
            "GITHUB_REPOSITORY",
            "GITLAB_REPOSITORY",
            "GIT_REPOSITORY",
            "URL",
            "DOWNLOAD_ONLY",
            "SOURCE_DIR",
            "GIT_SHALLOW",
            "EXCLUDE_FROM_ALL",
            "SYSTEM",
        ];
        let keywords = Keywords::parse(args, &[], &single, &["OPTIONS", "PATCHES"]);

        let (name, url, tag) = match keywords.unparsed.first() {
            // The shorthand form
            Some(shorthand) if keywords.values.is_empty() => {
                let (location, tag) = match shorthand.split_once(['@', '#']) {
                    Some((location, version)) => (location.to_string(), Some(version.to_string())),
                    None => (shorthand.clone(), None),
                };
                let url = if let Some(repository) = location.strip_prefix("gh:") {
                    format!("https://github.com/{repository}.git")
                } else if let Some(repository) = location.strip_prefix("gl:") {
                    format!("https://gitlab.com/{repository}.git")
                } else {
                    location.clone()
                };
                let name = location.rsplit('/').next().unwrap_or_default().trim_end_matches(".git").to_string();
                let tag = tag.map(|version| if shorthand.contains('@') { format!("v{version}") } else { version });
                (name, url, tag)
            }
            _ => {
                let url = if let Some(repository) = keywords.value("GITHUB_REPOSITORY") {
                    format!("https://github.com/{repository}.git")
                } else if let Some(repository) = keywords.value("GITLAB_REPOSITORY") {
                    format!("https://gitlab.com/{repository}.git")
                } else {
                    keywords.value("GIT_REPOSITORY").or_else(|| keywords.value("URL")).unwrap_or_default()
                };
                let tag = keywords
                    .value("GIT_TAG")
                    .or_else(|| keywords.value("VERSION").map(|version| format!("v{version}")));
                let Some(name) = keywords.value("NAME") else { return };
                (name, url, tag)
            }
        };

        self.set_variable(&format!("{name}_ADDED"), "ON");
        self.set_variable(&format!("{name}_SOURCE_DIR"), &format!("{{out}}/_deps/{}-src", name.to_lowercase()));
        self.fetched.insert(name, (url, tag));
    }

    // After the whole project ran

    /// What needs every target's sources known: the language COMPILE_LANGUAGE tests mean, C standards only C files
    /// use, and the symbol a shared build defines.
    pub fn finish_targets(&mut self) {
        // C++ unless the project compiles only C, for targets with no sources of their own
        let compiles = |language: &str| {
            self.targets
                .values()
                .any(|target| target.sources.iter().any(|item| source_language(&item.value) == Some(language)))
        };
        let project_language = if compiles(model::LANGUAGE_C) && !compiles(model::LANGUAGE_CXX) {
            model::LANGUAGE_C
        } else {
            model::LANGUAGE_CXX
        };

        for target in self.targets.values_mut() {
            resolve_languages(target, project_language);
            define_export_symbol(target);
        }

        // Per-file settings: decided for each file's language
        for property in std::mem::take(&mut self.source_properties) {
            let Some(condition) = property.condition.as_ref().filter(|_| model::mentions_language(&property.condition)) else {
                self.source_properties.push(property);
                continue;
            };

            for language in [model::LANGUAGE_C, model::LANGUAGE_CXX] {
                let files: Vec<PathBuf> = property
                    .files
                    .iter()
                    .filter(|file| source_language(file) == Some(language))
                    .cloned()
                    .collect();
                let condition = match decide_language(condition, language) {
                    Truth::Known(false) => continue,
                    Truth::Known(true) => None,
                    Truth::Depends(condition) => Some(condition),
                };
                if !files.is_empty() {
                    self.source_properties.push(SourceProperties {
                        files,
                        condition,
                        options: property.options.clone(),
                        defines: property.defines.clone(),
                    });
                }
            }
        }
    }

    // Helpers

    /// The real target a name refers to (following aliases); warns when it is not one the project builds.
    fn find_target(&mut self, command: &Command, name: &str) -> Option<String> {
        let resolved = self.resolve_alias(name);
        if self.targets.contains_key(&resolved) {
            return Some(resolved);
        }

        if !self.target_exists(name) {
            self.warn(command, &format!("{}: unknown target `{name}`", command.name));
        }
        None
    }

    pub(super) fn resolve_alias(&self, name: &str) -> String {
        let mut resolved = name.to_string();
        while let Some(real) = self.aliases.get(&resolved) {
            if *real == resolved {
                break;
            }
            resolved = real.clone();
        }

        resolved
    }

    /// Arguments after PUBLIC/PRIVATE/INTERFACE, with whether each passes on to dependents.
    fn split_visibility(&self, target: &str, args: &[String]) -> Vec<(bool, String)> {
        self.split_scopes(target, args)
            .into_iter()
            .map(|(public, arg, _)| (public, arg))
            .collect()
    }

    /// Arguments after PUBLIC/PRIVATE/INTERFACE: whether each passes on to dependents, and whether only they get it
    /// (INTERFACE on a target that compiles).
    fn split_scopes(&self, target: &str, args: &[String]) -> Vec<(bool, String, bool)> {
        let interface = self.targets[target].kind == TargetKind::Interface;
        let users_only = self.interface_is_users_only(target);
        let mut public = true;
        let mut only_users = false;
        let mut items = Vec::new();

        for arg in args {
            match arg.as_str() {
                "PUBLIC" => (public, only_users) = (true, false),
                "INTERFACE" => (public, only_users) = (true, users_only),
                "PRIVATE" => (public, only_users) = (interface, false),
                "SYSTEM" | "BEFORE" | "AFTER" => {}
                _ => items.push((public, arg.clone(), only_users)),
            }
        }

        items
    }

    /// Do a target's INTERFACE settings stay away from its own compile? Only when it compiles under every condition
    /// (one header-only somewhere uses them as its public settings).
    fn interface_is_users_only(&self, target: &str) -> bool {
        let target = &self.targets[target];
        let headers = TargetKind::Interface.text();

        target.kind != TargetKind::Interface && !target.settings.iter().any(|item| item.value == ("kind", headers.to_string()))
    }

    /// Values with their generator expressions evaluated, each with the condition it applies under: the target's
    /// own (when given) or the current one, and the expression's.
    pub(super) fn conditional_values(&mut self, target: Option<&str>, values: &[String]) -> Vec<(Option<String>, String)> {
        let base = match target {
            Some(target) => self.condition_for_target(target),
            None => self.current_condition(),
        };
        let mut results = Vec::new();
        let mut unknown = std::mem::take(&mut self.unknown_genex);

        for value in values {
            for (truth, text) in model::expand_genex(value, &mut unknown, Some(&|name| self.target_exists(name))) {
                let condition = match truth {
                    Truth::Known(false) => continue,
                    Truth::Known(true) => base.clone(),
                    Truth::Depends(condition) => model::and_optional(&base, &Some(condition)),
                };

                // An expression can produce a list
                for item in model::split_list(&text) {
                    results.push((condition.clone(), item));
                }
            }
        }
        self.unknown_genex = unknown;

        results
    }

    /// Command words with generator expressions decided as on this machine (commands cannot hold conditions).
    fn plain_words(&mut self, words: &[String]) -> Vec<String> {
        self.plain_words_kept(words).iter().flat_map(|word| model::split_list(word)).collect()
    }

    /// Generator expressions chosen as plain_words does, each word kept whole.
    fn plain_words_kept(&mut self, words: &[String]) -> Vec<String> {
        let options = self.option_defaults();
        let mut plain = Vec::new();

        for word in words {
            let mut unknown = std::mem::take(&mut self.unknown_genex);
            let alternatives = model::expand_genex(word, &mut unknown, Some(&|name| self.target_exists(name)));
            self.unknown_genex = unknown;
            let chosen = alternatives
                .iter()
                .find(|(truth, _)| match truth {
                    Truth::Known(value) => *value,
                    Truth::Depends(condition) => model::holds_on_host(condition, &|name| options.get(name).copied()),
                })
                .or(alternatives.first());

            if let Some((_, text)) = chosen.filter(|(_, text)| !text.is_empty()) {
                plain.push(text.clone());
            }
        }

        plain
    }

    /// Strips what CMake allows around list values: -D on definitions, SHELL: on options, and resolves folders.
    fn clean_list_value(&self, list: TargetList, value: &str) -> String {
        match list {
            TargetList::Defines => value.strip_prefix("-D").or_else(|| value.strip_prefix("/D")).unwrap_or(value).to_string(),
            TargetList::Options => value.strip_prefix("SHELL:").unwrap_or(value).to_string(),
            TargetList::LinkOptions => match value.strip_prefix("LINKER:") {
                Some(linker_flag) => format!("-Wl,{linker_flag}"),
                None => value.strip_prefix("SHELL:").unwrap_or(value).to_string(),
            },
            TargetList::LibDirs => slash(&self.resolve_path(value)),
            TargetList::Links => value.to_string(),
        }
    }

    /// A custom command's output: relative paths are in the current build folder.
    pub(super) fn resolve_output(&self, text: &str) -> PathBuf {
        if text.starts_with("{out}") || PathBuf::from(text).is_absolute() {
            return util::normalize(Path::new(text));
        }

        let binary_dir = self.get_variable("CMAKE_CURRENT_BINARY_DIR").unwrap_or_else(|| "{out}".into());
        util::normalize(Path::new(&format!("{binary_dir}/{text}")))
    }

    /// A DEPENDS entry: a target name stays a name, a file becomes a full path.
    fn resolve_dependency(&self, text: &str) -> String {
        if self.target_exists(text) || text.starts_with("{out}") {
            return text.to_string();
        }

        slash(&self.resolve_path(text))
    }

    /// A folder or file as text: `{out}` paths unchanged, others made absolute.
    fn resolve_text_path(&self, text: &str) -> String {
        slash(&self.resolve_path(text))
    }
}

/// A test property in Anvil's terms; one Anvil has no key for becomes a note on the test.
fn apply_test_property(properties: &mut TestProperties, property: &str, items: &[String], append: bool) {
    let joined = items.join(";");
    let truthy = model::constant_truth(&joined).unwrap_or(false);

    match property {
        "WILL_FAIL" => properties.will_fail = truthy,
        "RUN_SERIAL" => properties.serial = truthy,
        "DISABLED" => properties.disabled = truthy,

        // A shared resource: running alone is the safe reading
        "RESOURCE_LOCK" | "RESOURCE_GROUPS" => properties.serial = true,

        "SKIP_RETURN_CODE" => properties.skip_exit_code = joined.parse().ok(),
        "SKIP_REGULAR_EXPRESSION" => properties.skip_regex = Some(items.join("|")),

        "TIMEOUT" => properties.timeout = joined.parse::<f64>().ok().map(|seconds| seconds.ceil() as u64),
        "PASS_REGULAR_EXPRESSION" => properties.pass_regex = Some(items.join("|")),
        "FAIL_REGULAR_EXPRESSION" => properties.fail_regex = Some(items.join("|")),
        "DEPENDS" => extend_or_replace(&mut properties.after, items, append),
        "FIXTURES_SETUP" => extend_or_replace(&mut properties.fixtures_setup, items, append),
        "FIXTURES_REQUIRED" => extend_or_replace(&mut properties.fixtures_required, items, append),
        "ENVIRONMENT" => {
            if !append {
                properties.env.clear();
            }
            for item in items {
                if let Some((name, value)) = item.split_once('=') {
                    properties.env.push((name.to_string(), value.to_string()));
                }
            }
        }
        _ if IGNORED_TEST_PROPERTIES.contains(&property) => {}
        _ => properties.notes.push(format!("test property {property} = \"{joined}\" is not converted")),
    }
}

fn extend_or_replace(list: &mut Vec<String>, items: &[String], append: bool) {
    if !append {
        list.clear();
    }
    list.extend(items.iter().cloned());
}

/// Is this a target property that changes nothing Anvil builds?
fn is_ignored_property(property: &str) -> bool {
    let name = CONFIGURATION_SUFFIXES
        .iter()
        .filter(|(suffix, _)| !suffix.is_empty())
        .find_map(|(suffix, _)| property.strip_suffix(suffix))
        .unwrap_or(property);

    IGNORED_PROPERTIES.iter().any(|entry| {
        if entry.starts_with('_') {
            name.ends_with(entry)
        } else if entry.ends_with('_') {
            name.starts_with(entry)
        } else {
            name == *entry
        }
    })
}

/// The compilers a word of a flag variable is for when only one family takes it: `/W4` is cl's; `-Wshadow`,
/// `-fno-rtti` and `-march=native` are GCC's and Clang's. cl also takes its options with `-` (`-W4`, `-GS`).
fn compiler_family_of_flag(flag: &str) -> Option<&'static str> {
    if flag.len() > 1 && flag.starts_with('/') {
        return Some("msvc");
    }

    let gnu_warning = flag
        .strip_prefix("-W")
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_lowercase()) && rest != "all");
    let gnu_feature = flag.strip_prefix("-f").is_some_and(|rest| !rest.is_empty() && !rest.contains(':'));
    let gnu_machine = flag.strip_prefix("-m").is_some_and(|rest| !rest.is_empty());
    let gnu_other = matches!(flag, "-pedantic" | "-pedantic-errors" | "-pthread" | "-ansi");
    (gnu_warning || gnu_feature || gnu_machine || gnu_other).then_some("!msvc")
}

/// Is this a CMAKE_<LANG>_FLAGS or linker flag variable, which Anvil reads at the folder's end?
fn is_flag_variable(name: &str) -> bool {
    let base = CONFIGURATION_SUFFIXES
        .iter()
        .filter(|(suffix, _)| !suffix.is_empty())
        .find_map(|(suffix, _)| name.strip_suffix(suffix))
        .unwrap_or(name);
    matches!(
        base,
        "CMAKE_C_FLAGS" | "CMAKE_CXX_FLAGS" | "CMAKE_EXE_LINKER_FLAGS" | "CMAKE_SHARED_LINKER_FLAGS"
    )
}
fn is_aux_source(path: &std::path::Path) -> bool {
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    matches!(extension.as_str(), "c" | "cc" | "cpp" | "cxx" | "c++")
}

/// The language condition word of a C or C++ source file.
fn source_language(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_string_lossy().to_lowercase();
    match extension.as_str() {
        "c" => Some(model::LANGUAGE_C),
        "cc" | "cpp" | "cxx" | "c++" | "cp" => Some(model::LANGUAGE_CXX),
        _ => None,
    }
}

/// A condition with the language decided, as short as it gets.
fn decide_language(condition: &str, language: &str) -> Truth {
    match model::for_language(condition, language) {
        Truth::Depends(decided) => model::simplify(&decided, None),
        known => known,
    }
}

/// Puts an item under a decided condition; false when it never holds.
fn set_decided<T>(item: &mut Item<T>, truth: Truth) -> bool {
    match truth {
        Truth::Known(holds) => {
            item.condition = None;
            holds
        }
        Truth::Depends(condition) => {
            item.condition = Some(condition);
            true
        }
    }
}

/// Decides the COMPILE_LANGUAGE tests in a target's settings. In a target with C and C++ files, a private flag or
/// definition for one language goes to a rule for that language's files; everything else is decided for the
/// target's main language (C++ when it has C++ files), as its link and its users are.
fn resolve_languages(target: &mut Target, project_language: &'static str) {
    let compiles = |language: &str| target.sources.iter().any(|item| source_language(&item.value) == Some(language));
    let (has_c, has_cxx) = (compiles(model::LANGUAGE_C), compiles(model::LANGUAGE_CXX));
    let main = match (has_c, has_cxx) {
        (_, true) => model::LANGUAGE_CXX,
        (true, false) => model::LANGUAGE_C,
        (false, false) => project_language,
    };

    // C standards only matter to C files
    if !has_c {
        target.c_std = None;
        target.settings.retain(|item| item.value.0 != "c-std");
    }

    // Per-language rules for a target compiling both
    if has_c && has_cxx {
        let mut rules: IndexMap<(&'static str, Option<String>), LanguageRule> = IndexMap::new();
        for (is_option, list) in [(true, &mut target.options), (false, &mut target.defines)] {
            for (language, condition, value) in split_by_language(list) {
                let rule = rules.entry((language, condition.clone())).or_insert_with(|| LanguageRule {
                    folder: PathBuf::new(),
                    pattern: String::new(),
                    condition,
                    options: Vec::new(),
                    defines: Vec::new(),
                });
                if is_option {
                    rule.options.push(value);
                } else {
                    rule.defines.push(value);
                }
            }
        }

        for ((language, _), mut rule) in rules {
            (rule.folder, rule.pattern) = language_files(target, language);
            target.language_rules.push(rule);
        }
    }

    // The rest: the main language's
    fn decide_all<T>(items: &mut Vec<Item<T>>, language: &str) {
        items.retain_mut(|item| match &item.condition {
            Some(condition) if model::mentions_language(&item.condition) => {
                let truth = decide_language(condition, language);
                set_decided(item, truth)
            }
            _ => true,
        });
    }
    decide_all(&mut target.sources, main);
    decide_all(&mut target.includes, main);
    decide_all(&mut target.defines, main);
    decide_all(&mut target.options, main);
    decide_all(&mut target.interface_defines, main);
    decide_all(&mut target.interface_includes, main);
    decide_all(&mut target.interface_options, main);
    decide_all(&mut target.links, main);
    decide_all(&mut target.link_options, main);
    decide_all(&mut target.lib_dirs, main);
    decide_all(&mut target.settings, main);
}

/// Takes a target's private settings that differ between C and C++ files out of the list: each value once, with the
/// condition it holds under for each language. Values the same for both stay, with the language decided.
fn split_by_language(items: &mut Vec<Item<String>>) -> Vec<(&'static str, Option<String>, String)> {
    let splits = |item: &Item<String>| !item.public && model::mentions_language(&item.condition);

    // A value given several times holds under any of its conditions
    let mut conditions: IndexMap<String, Truth> = IndexMap::new();
    for item in items.iter().filter(|item| splits(item)) {
        let condition = Truth::Depends(item.condition.clone().unwrap_or_default());
        let combined = conditions.entry(item.value.clone()).or_insert(Truth::Known(false));
        *combined = combined.clone().or(condition);
    }

    // The first of each stays in its place when both languages agree
    let mut moved = Vec::new();
    let mut seen = HashSet::new();
    items.retain_mut(|item| {
        if !splits(item) {
            return true;
        }
        let Some(Truth::Depends(combined)) = conditions.get(&item.value).filter(|_| seen.insert(item.value.clone())) else {
            return false;
        };

        let for_c = decide_language(combined, model::LANGUAGE_C);
        let for_cxx = decide_language(combined, model::LANGUAGE_CXX);
        if for_c == for_cxx {
            return set_decided(item, for_c);
        }
        for (language, truth) in [(model::LANGUAGE_C, for_c), (model::LANGUAGE_CXX, for_cxx)] {
            match truth {
                Truth::Known(false) => {}
                Truth::Known(true) => moved.push((language, None, item.value.clone())),
                Truth::Depends(condition) => moved.push((language, Some(condition), item.value.clone())),
            }
        }
        false
    });

    moved
}

/// Where a target's files of one language are, as a folder and a pattern under it (`*.c`, `*.{cc,cpp}`): the
/// target's folder, or the nearest one above it that holds them all.
fn language_files(target: &Target, language: &str) -> (PathBuf, String) {
    let files: Vec<&PathBuf> = target
        .sources
        .iter()
        .map(|item| &item.value)
        .filter(|path| source_language(path) == Some(language) && !slash(path).starts_with("{out}"))
        .collect();

    let mut extensions: Vec<String> = files
        .iter()
        .filter_map(|file| file.extension().map(|extension| extension.to_string_lossy().to_lowercase()))
        .collect();
    extensions.sort();
    extensions.dedup();
    let pattern = match extensions.as_slice() {
        [single] => format!("*.{single}"),
        many => format!("*.{{{}}}", many.join(",")),
    };

    let mut folder = target.dir.clone();
    let holds_all = |folder: &Path| {
        let prefix = format!("{}/", util::path_key(folder).trim_end_matches('/'));
        files.iter().all(|file| util::path_key(file).starts_with(&prefix))
    };
    while !holds_all(&folder) {
        match folder.parent() {
            Some(parent) => folder = parent.to_path_buf(),
            None => break,
        }
    }

    (folder, pattern)
}

/// A shared library's own definition: DEFINE_SYMBOL, or CMake's `<target>_EXPORTS` when its code uses it, under the
/// condition it builds shared.
fn define_export_symbol(target: &mut Target) {
    let shared = shared_truth(target);
    if shared == Truth::Known(false) {
        return;
    }

    let symbol = match &target.define_symbol {
        Some(symbol) => symbol.clone(),
        None => {
            let default = format!("{}_EXPORTS", c_identifier(&target.name));
            if !mentions_symbol(target, &default) {
                return;
            }
            default
        }
    };
    if symbol.is_empty() {
        return;
    }

    let condition = match shared {
        Truth::Depends(condition) => Some(condition),
        Truth::Known(_) => None,
    };
    target.defines.push(Item {
        condition,
        public: false,
        value: symbol,
    });
}

/// Where a target builds as a shared library: its kind, BUILD_SHARED_LIBS, and kinds set under conditions.
fn shared_truth(target: &Target) -> Truth {
    let mut truth = Truth::Known(target.kind == TargetKind::Shared);
    if let Some(condition) = &target.shared_condition {
        truth = truth.or(Truth::Depends(condition.clone()));
    }

    for item in target.settings.iter().filter(|item| item.value.0 == "kind") {
        let here = Truth::Known(item.value.1 == TargetKind::Shared.text());
        let condition = item.condition.clone().map_or(Truth::Known(true), Truth::Depends);
        truth = condition.clone().and(here).or(condition.not().and(truth));
    }

    truth
}

// Folders the search for a symbol skips, and how many headers it reads at most per target
const SKIPPED_SEARCH_FOLDERS: &[&str] = &["out", "build", "_deps", "vcpkg_installed", "node_modules", "target"];
const MAX_SEARCHED_HEADERS: usize = 2000;

/// Does a target's code (its sources, and the headers in its include folders) use this name?
fn mentions_symbol(target: &Target, symbol: &str) -> bool {
    let mut files: Vec<PathBuf> = target.sources.iter().map(|item| item.value.clone()).collect();

    for item in target.includes.iter().chain(&target.interface_includes) {
        if slash(&item.value.0).starts_with("{out}") {
            continue;
        }
        let headers = walkdir::WalkDir::new(&item.value.0)
            .into_iter()
            .filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                entry.depth() == 0 || !(name.starts_with('.') || SKIPPED_SEARCH_FOLDERS.contains(&name.as_ref()))
            })
            .flatten()
            .filter(|entry| entry.file_type().is_file() && is_header(entry.path()))
            .map(|entry| entry.into_path())
            .take(MAX_SEARCHED_HEADERS);
        files.extend(headers);
    }

    files
        .iter()
        .any(|file| std::fs::read_to_string(file).is_ok_and(|text| text.contains(symbol)))
}

fn is_header(path: &Path) -> bool {
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    matches!(extension.as_str(), "h" | "hh" | "hpp" | "hxx" | "h++" | "inl" | "ipp")
}

/// A name as a C identifier, as CMake makes `<target>_EXPORTS`: other characters become `_`.
fn c_identifier(name: &str) -> String {
    let identifier: String = name
        .chars()
        .map(|character| if character.is_ascii_alphanumeric() { character } else { '_' })
        .collect();
    if identifier.starts_with(|character: char| character.is_ascii_digit()) {
        format!("_{identifier}")
    } else {
        identifier
    }
}

#[cfg(test)]
mod tests {
    /// The anvil.toml of a project made of the given files, in a fresh temporary folder.
    fn convert_files(name: &str, files: &[(&str, &str)]) -> String {
        let folder = std::env::temp_dir().join(format!("anvil-target-commands-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        for (path, text) in files {
            let file = folder.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        }

        let conversion = super::super::convert(&folder).unwrap();
        let _ = std::fs::remove_dir_all(&folder);
        conversion.toml
    }

    #[test]
    fn folder_settings_apply_as_in_cmake() {
        let cmake = r#"
cmake_minimum_required(VERSION 3.20)
project(late C CXX)
add_compile_options(-DEARLY_OPTION)
add_executable(app main.cpp)
add_compile_options(-DLATE_OPTION)
link_libraries(late_library)
add_definitions(-DLATE_DEFINITION)
include_directories(late)
set(CMAKE_CXX_FLAGS "${CMAKE_CXX_FLAGS} -DLATE_CXX_FLAGS")
"#;
        let toml = convert_files("late", &[("CMakeLists.txt", cmake), ("main.cpp", "int main() {}\n")]);

        // Options and libraries only from before the target; definitions, include folders and flag variables from
        // the whole folder
        assert!(
            toml.contains(r#"defines = ["LATE_DEFINITION", "LATE_CXX_FLAGS", "EARLY_OPTION"]"#),
            "{toml}"
        );
        assert!(toml.contains(r#"include = ["late"]"#), "{toml}");
        assert!(!toml.contains("LATE_OPTION") && !toml.contains("late_library"), "{toml}");
    }

    #[test]
    fn language_flags_go_to_their_files() {
        let cmake = r#"
cmake_minimum_required(VERSION 3.20)
project(mixed C CXX)
set(CMAKE_C_STANDARD 11)
add_executable(app main.cpp util.c)
add_library(clib STATIC util.c)
target_compile_options(app PRIVATE $<$<COMPILE_LANGUAGE:C>:-Wstrict-prototypes> $<$<COMPILE_LANG_AND_ID:CXX,MSVC>:/wd4251>)
set(CMAKE_C_FLAGS "${CMAKE_C_FLAGS} -DC_ONLY")
set(CMAKE_C_FLAGS_DEBUG "${CMAKE_C_FLAGS_DEBUG} -DC_DEBUG")
"#;
        let files = [("CMakeLists.txt", cmake), ("main.cpp", "int main() {}\n"), ("util.c", "int util;\n")];
        let toml = convert_files("mixed", &files);

        // A mixed target: rules for its C files; a C target: plain settings and its C standard
        assert!(
            toml.contains("[[target.app.file]]\nmatch = [\"**/*.c\"]\ndefines = [\"C_ONLY\"]\nflags = [\"-Wstrict-prototypes\"]"),
            "{toml}"
        );
        assert!(
            toml.contains("[[target.app.file]]\nmatch = [\"**/*.cpp\"]\nif = \"cl\"\nflags = [\"/wd4251\"]"),
            "{toml}"
        );
        assert!(
            toml.contains("[[target.app.file]]\nmatch = [\"**/*.c\"]\nif = \"debug\"\ndefines = [\"C_DEBUG\"]"),
            "{toml}"
        );
        assert!(
            toml.contains("[target.clib]\nkind = \"lib\"\nsources = [\"util.c\"]\ndefines = [\"C_ONLY\"]\nc-std = \"c11\""),
            "{toml}"
        );
        assert!(!toml.contains("language."), "{toml}");
    }

    #[test]
    fn flag_variables_keep_their_branches() {
        let cmake = r#"
cmake_minimum_required(VERSION 3.20)
project(branches C)
option(ENABLE_SAN "sanitizers" OFF)
if(MSVC)
  set(CMAKE_C_FLAGS "${CMAKE_C_FLAGS} /W4 -DBOTH")
else()
  set(CMAKE_C_FLAGS "${CMAKE_C_FLAGS} -Wshadow -DBOTH")
endif()
if(ENABLE_SAN)
  set(CMAKE_C_FLAGS "${CMAKE_C_FLAGS} -fsanitize=address")
endif()
set(flags -Wstrict-prototypes /sdl)
foreach(flag ${flags})
  set(CMAKE_C_FLAGS "${CMAKE_C_FLAGS} ${flag}")
endforeach()
separate_arguments(CMAKE_C_FLAGS)
string(REPLACE ";" " " CMAKE_C_FLAGS "${CMAKE_C_FLAGS}")
set(CMAKE_RC_FLAGS "${CMAKE_RC_FLAGS} /I \"${CMAKE_CURRENT_SOURCE_DIR}/res\"")
set_source_files_properties(main.c PROPERTIES LANGUAGE C)
add_executable(app main.c app.rc)
"#;
        let files = [("CMakeLists.txt", cmake), ("main.c", "int main(void) { return 0; }\n"), ("app.rc", "\n")];
        let toml = convert_files("branches", &files);

        // The branch a word was added in, also after the variable was a list for a moment; without one, the
        // compilers that take it
        assert!(toml.contains("include = [\"res\"]\ndefines = [\"BOTH\"]\n"), "{toml}");
        assert!(
            toml.contains("[target.app.when.\"!msvc\"]\nflags = [\"-Wshadow\", \"-Wstrict-prototypes\"]"),
            "{toml}"
        );
        assert!(toml.contains("[target.app.when.msvc]\nflags = [\"/sdl\"]\nwarnings = \"high\""), "{toml}");
        assert!(toml.contains("[target.app.when.ENABLE_SAN]\nflags = [\"-fsanitize=address\"]"), "{toml}");
        assert!(!toml.contains("/TC") && !toml.contains("-xc"), "{toml}");
    }

    #[test]
    fn single_values_keep_their_conditions() {
        let cmake = r#"
cmake_minimum_required(VERSION 3.20)
project(kinds CXX)
option(USE_PCH "pch" OFF)
option(BUILD_DLL "dll" OFF)
option(HEADER_ONLY "headers" OFF)
if(BUILD_DLL)
  add_library(lib SHARED lib.cpp)
else()
  add_library(lib STATIC lib.cpp)
endif()
if(USE_PCH)
  target_precompile_headers(lib PRIVATE pch.h)
  set_target_properties(lib PROPERTIES OUTPUT_NAME lib_pch)
endif()
if(HEADER_ONLY)
  add_library(api INTERFACE)
else()
  add_library(api STATIC api.cpp)
endif()
"#;
        let files = [
            ("CMakeLists.txt", cmake),
            ("lib.cpp", "#ifdef lib_EXPORTS\n#endif\n"),
            ("api.cpp", "int api;\n"),
            ("pch.h", "\n"),
        ];
        let toml = convert_files("kinds", &files);

        // The kind this machine builds by default, a compiled one over headers; the rest under their options
        assert!(toml.contains("[target.lib]\nkind = \"lib\""), "{toml}");
        assert!(
            toml.contains("[target.lib.when.BUILD_DLL]\ndefines = [\"lib_EXPORTS\"]\nkind = \"shared\""),
            "{toml}"
        );
        assert!(
            toml.contains("[target.lib.when.USE_PCH]\npch = \"pch.h\"\noutput-name = \"lib_pch\""),
            "{toml}"
        );
        assert!(toml.contains("[target.api]\nkind = \"lib\""), "{toml}");
        assert!(toml.contains("[target.api.when.HEADER_ONLY]\nkind = \"headers\""), "{toml}");
        assert!(toml.contains("[target.api.when.\"!HEADER_ONLY\"]\nsources = [\"api.cpp\"]"), "{toml}");
    }

    #[test]
    fn interface_settings_and_file_sets() {
        let cmake = r#"
cmake_minimum_required(VERSION 3.25)
project(sets CXX)
add_library(lib SHARED lib.cpp)
target_compile_options(lib INTERFACE -fuser-only)
target_include_directories(lib INTERFACE inc)
target_sources(lib PUBLIC FILE_SET HEADERS FILES hdr/lib.h)
set_target_properties(lib PROPERTIES DEFINE_SYMBOL LIB_BUILDING WINDOWS_EXPORT_ALL_SYMBOLS ON)
add_library(objects OBJECT part.cpp)
add_executable(app main.cpp $<TARGET_OBJECTS:objects>)
"#;
        let files = [
            ("CMakeLists.txt", cmake),
            ("lib.cpp", "\n"),
            ("part.cpp", "\n"),
            ("main.cpp", "int main() {}\n"),
            ("hdr/lib.h", "\n"),
        ];
        let toml = convert_files("sets", &files);

        assert!(
            toml.contains("[target.lib.interface]\ninclude = [\"inc\"]\nflags = [\"-fuser-only\"]"),
            "{toml}"
        );
        assert!(toml.contains("[target.lib.public]\ninclude = [\".\"]"), "{toml}");
        assert!(toml.contains("defines = [\"LIB_BUILDING\"]"), "{toml}");
        assert!(toml.contains("# TODO: WINDOWS_EXPORT_ALL_SYMBOLS"), "{toml}");
        assert!(toml.contains("deps = [\"objects\"]"), "{toml}");
    }
}
