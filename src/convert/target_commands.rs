//! CMake's target, test and dependency commands: add_library, target_*, properties, add_test, add_custom_command,
//! FetchContent and CPM. Every setting keeps the condition it was made under.

use super::builtins::Keywords;
use super::interp::{DirState, Interpreter, SourceProperties};
use super::model::{self, CustomCommand, ExportHeader, Item, Target, TargetKind, Test, TestProperties, Truth};
use super::parse::Command;
use crate::util::{self, slash};
use indexmap::IndexMap;
use std::path::{Path, PathBuf};

// Languages enable_language may name that Anvil builds
const SUPPORTED_LANGUAGES: &[&str] = &["C", "CXX", "RC", "ASM", "ASM_MASM", "CUDA"];

// Test properties that only label or weigh tests for CTest's scheduler
const IGNORED_TEST_PROPERTIES: &[&str] = &[
    "LABELS",
    "PROCESSORS",
    "COST",
    "TIMEOUT_AFTER_MATCH",
    "TIMEOUT_SIGNAL_NAME",
    "TIMEOUT_SIGNAL_GRACE_PERIOD",
];

// Target properties that change nothing Anvil builds (matched as parts of the name)
const IGNORED_PROPERTY_PARTS: &[&str] = &[
    "_OUTPUT_DIRECTORY",
    "VS_",
    "XCODE_",
    "POSTFIX",
    "MACOSX_",
    "INTERPROCEDURAL_OPTIMIZATION",
    "MAP_IMPORTED_CONFIG",
    "IMPORTED_CONFIGURATIONS",
    "VISIBILITY_PRESET",
    "VISIBILITY_INLINES_HIDDEN",
    "FOLDER",
    "STANDARD_REQUIRED",
    "_EXTENSIONS",
    "LINKER_LANGUAGE",
    "VERSION",
    "EXPORT_",
    "POSITION_INDEPENDENT_CODE",
    "INSTALL_",
    "DEBUGGER",
    "PDB_",
    "IMPORTED_NO_SONAME",
    "IMPORTED_LINK_INTERFACE",
    "C_STANDARD",
    "RPATH",
    "PUBLIC_HEADER",
    "PRIVATE_HEADER",
    "RESOURCE",
    "LABELS",
    "CXX_SCAN_FOR_MODULES",
    "COMPILE_PDB",
    "EXCLUDE_FROM_ALL",
    "FRAMEWORK",
    "UNITY_BUILD_",
    "OPTIMIZE_DEPENDENCIES",
    "SYSTEM",
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

        // Defined again (if(WIN32) add_library(x ...) else() add_library(x ...)): one target for both
        if let Some(existing) = self.targets.get_mut(name) {
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

            self.target_prefixes.insert(name.to_string(), common);
            self.add_sources(name, sources);
            return;
        }

        let mut target = Target::new(name, kind, self.current_dir(), self.current_dir_id(), condition);
        target.helper = self.call_stack.last().map(|call| call.function.clone());
        target.std = self.get_variable("CMAKE_CXX_STANDARD").and_then(|standard| standard.parse().ok());
        if let Some(gpus) = self.get_variable("CMAKE_CUDA_ARCHITECTURES").filter(|gpus| !gpus.is_empty()) {
            target.settings.push(Item {
                condition: None,
                public: false,
                value: ("cuda-arch", gpus),
            });
        }
        self.target_prefixes.insert(name.to_string(), self.conditions.clone());
        self.targets.insert(name.to_string(), target);

        self.add_sources(name, sources);
        if kind != TargetKind::Custom && kind != TargetKind::Interface {
            self.apply_flag_variables(name, kind);
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

    /// CMAKE_CXX_FLAGS[_<CONFIG>] and the linker flag variables, as the target's own settings.
    fn apply_flag_variables(&mut self, name: &str, kind: TargetKind) {
        let base = self.condition_for_target(name);

        // Compile flags, per configuration
        let compile_variables = [
            ("CMAKE_CXX_FLAGS", None),
            ("CMAKE_CXX_FLAGS_DEBUG", Some("debug")),
            ("CMAKE_CXX_FLAGS_RELEASE", Some("release")),
            ("CMAKE_CXX_FLAGS_RELWITHDEBINFO", Some("relwithdebinfo")),
            ("CMAKE_CXX_FLAGS_MINSIZEREL", Some("minsizerel")),
        ];
        for (variable, profile) in compile_variables {
            let flags = crate::util::split_command(&self.get_variable(variable).unwrap_or_default());
            let condition = model::and_optional(&base, &profile.map(String::from));
            let target = self.targets.get_mut(name).unwrap();
            target.options.extend(flags.into_iter().map(|flag| Item {
                condition: condition.clone(),
                public: false,
                value: flag,
            }));
        }

        // Link flags
        let link_variable = match kind {
            TargetKind::Exe => "CMAKE_EXE_LINKER_FLAGS",
            TargetKind::Shared => "CMAKE_SHARED_LINKER_FLAGS",
            _ => return,
        };
        let flags = crate::util::split_command(&self.get_variable(link_variable).unwrap_or_default());
        let target = self.targets.get_mut(name).unwrap();
        target.link_options.extend(flags.into_iter().map(|flag| Item {
            condition: base.clone(),
            public: false,
            value: flag,
        }));
    }

    fn add_sources(&mut self, name: &str, values: &[String]) {
        for (condition, value) in self.conditional_values(Some(name), values) {
            let path = self.resolve_path(&value);
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

        // Plain files, and FILE_SET HEADERS whose BASE_DIRS are include folders
        let mut public = true;
        let mut mode = "FILES";
        let mut files = Vec::new();
        for word in rest {
            match word.as_str() {
                "PUBLIC" | "INTERFACE" => public = true,
                "PRIVATE" => public = false,
                "FILE_SET" | "TYPE" | "BASE_DIRS" | "FILES" => mode = word.as_str(),
                _ if mode == "BASE_DIRS" => {
                    for (condition, value) in self.conditional_values(Some(&target), std::slice::from_ref(word)) {
                        let path = self.resolve_path(&value);
                        self.targets.get_mut(&target).unwrap().includes.push(Item {
                            condition,
                            public,
                            value: (path, false),
                        });
                    }
                }
                _ if mode == "FILES" => files.push(word.clone()),
                _ => {}
            }
        }

        self.add_sources(&target, &files);
    }

    pub(super) fn command_target_include_directories(&mut self, command: &Command, args: &[String]) {
        let Some((name, rest)) = args.split_first() else { return };
        let Some(target) = self.find_target(command, name) else { return };
        let system = rest.iter().any(|word| word == "SYSTEM");

        for (public, raw) in self.split_visibility(&target, rest) {
            for (condition, value) in self.conditional_values(Some(&target), &[raw]) {
                let path = self.resolve_path(&value);
                self.targets.get_mut(&target).unwrap().includes.push(Item {
                    condition,
                    public,
                    value: (path, system),
                });
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

                // A compiled target's INTERFACE definitions are not its own (FOO_API dllexport inside, dllimport outside)
                let converted = self.targets.get_mut(&target).unwrap();
                let item = Item { condition, public, value };
                match list {
                    TargetList::Defines if interface_only => converted.interface_defines.push(item),
                    _ => list.of_target(converted).push(item),
                }
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
            for (_, feature) in self.conditional_values(Some(&target), &[raw]) {
                self.apply_compile_feature(&target, &feature, public);
            }
        }
    }

    /// cxx_std_NN: the target's standard, and its dependents' too when public.
    fn apply_compile_feature(&mut self, target: &str, feature: &str, public: bool) {
        let Some(standard) = feature.strip_prefix("cxx_std_").and_then(|number| number.parse::<u32>().ok()) else {
            return;
        };
        let target = self.targets.get_mut(target).unwrap();

        target.std = Some(target.std.map_or(standard, |current| current.max(standard)));
        if public {
            target.public_std = Some(target.public_std.map_or(standard, |current| current.max(standard)));
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

        let headers: Vec<String> = self.split_visibility(&target, rest).into_iter().map(|(_, header)| header).collect();
        for (_, header) in self.conditional_values(Some(&target), &headers) {
            self.set_precompiled_header(&target, &header);
            break;
        }
    }

    fn set_precompiled_header(&mut self, target: &str, header: &str) {
        // <vector>-style headers have no file of their own
        if header.starts_with('<') {
            let note = format!("precompiled header {header}: Anvil's pch is a header file in the project; make one that includes it");
            self.targets.get_mut(target).unwrap().notes.push(note);
            return;
        }

        let path = self.resolve_path(header.trim_matches('"'));
        self.targets.get_mut(target).unwrap().pch = Some(path);
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
                list.of_target(self.targets.get_mut(target).unwrap()).retain(|item| item.public != public);
            }

            // COMPILE_FLAGS and LINK_FLAGS are command-line strings, not lists
            let items = if base_property.ends_with("_FLAGS") {
                crate::util::split_command(&items.join(" "))
            } else {
                items
            };
            for (condition, value) in self.conditional_values(Some(target), &items) {
                let value = self.clean_list_value(list, &value);
                list.of_target(self.targets.get_mut(target).unwrap())
                    .push(Item { condition, public, value });
            }
            return;
        }

        match base_property {
            "INCLUDE_DIRECTORIES" | "SYSTEM_INCLUDE_DIRECTORIES" => {
                let system = base_property.starts_with("SYSTEM");
                if !append {
                    self.targets.get_mut(target).unwrap().includes.retain(|item| item.public != public);
                }
                for (condition, value) in self.conditional_values(Some(target), &items) {
                    let path = self.resolve_path(&value);
                    self.targets.get_mut(target).unwrap().includes.push(Item {
                        condition,
                        public,
                        value: (path, system),
                    });
                }
            }
            "SOURCES" => self.add_sources(target, &items),
            "PRECOMPILE_HEADERS" => {
                if let Some(header) = items.first() {
                    self.set_precompiled_header(target, header);
                }
            }
            "COMPILE_FEATURES" => {
                for feature in &items {
                    self.apply_compile_feature(target, feature, public);
                }
            }
            "CXX_STANDARD" => self.targets.get_mut(target).unwrap().std = items.first().and_then(|standard| standard.parse().ok()),
            "OUTPUT_NAME" | "RUNTIME_OUTPUT_NAME" | "ARCHIVE_OUTPUT_NAME" | "LIBRARY_OUTPUT_NAME" => {
                self.targets.get_mut(target).unwrap().output_name = items.first().cloned();
            }
            "WIN32_EXECUTABLE" => {
                let gui = self
                    .conditional_values(Some(target), &items)
                    .iter()
                    .any(|(_, value)| model::constant_truth(value) == Some(true));
                self.targets.get_mut(target).unwrap().gui = gui;
            }
            "MSVC_RUNTIME_LIBRARY" => {
                let note = format!(
                    "MSVC_RUNTIME_LIBRARY {}: use `static-runtime = true` in a profile for the static runtime",
                    items.join(";")
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
            other if IGNORED_PROPERTY_PARTS.iter().any(|part| other.contains(part)) => {}
            _ => {
                let note = format!("property {property} = \"{}\" is not converted", items.join(";"));
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
        let (is_options, items) = match property {
            "COMPILE_OPTIONS" => (true, items),
            "COMPILE_FLAGS" => (true, crate::util::split_command(&items.join(" "))),
            "COMPILE_DEFINITIONS" => (false, items),
            _ => return,
        };
        let files: Vec<PathBuf> = files
            .iter()
            .flat_map(|file| model::split_list(file))
            .map(|file| self.resolve_path(&file))
            .collect();

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
            properties.env = checked.env;
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
        let mut public = true;
        let mut only_users = false;
        let mut items = Vec::new();

        for arg in args {
            match arg.as_str() {
                "PUBLIC" => (public, only_users) = (true, false),
                "INTERFACE" => (public, only_users) = (true, !interface),
                "PRIVATE" => (public, only_users) = (interface, false),
                "SYSTEM" | "BEFORE" | "AFTER" => {}
                _ => items.push((public, arg.clone(), only_users)),
            }
        }

        items
    }

    /// Values with their generator expressions evaluated, each with the condition it applies under: the target's
    /// own (when given) or the current one, and the expression's.
    pub(super) fn conditional_values(&mut self, target: Option<&str>, values: &[String]) -> Vec<(Option<String>, String)> {
        let base = match target {
            Some(target) => self.condition_for_target(target),
            None => self.current_condition(),
        };
        let mut results = Vec::new();

        for value in values {
            for (truth, text) in model::expand_genex(value, &mut self.unknown_genex) {
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
            let alternatives = model::expand_genex(word, &mut self.unknown_genex);
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
                    // Folders from the converting machine's own PATH ($ENV{PATH}) are not the project's
                    let value = if name.eq_ignore_ascii_case("PATH") {
                        let own = std::env::var("PATH").unwrap_or_default();
                        let own: Vec<&str> = own.split(';').filter(|folder| !folder.is_empty()).collect();
                        let folders: Vec<&str> = value.split(';').filter(|folder| !own.contains(&folder.trim_end_matches('\\'))).collect();
                        folders.join(";")
                    } else {
                        value.to_string()
                    };
                    properties.env.push((name.to_string(), value));
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

/// aux_source_directory's file kinds: C and C++ sources.
fn is_aux_source(path: &std::path::Path) -> bool {
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    matches!(extension.as_str(), "c" | "cc" | "cpp" | "cxx" | "c++")
}
