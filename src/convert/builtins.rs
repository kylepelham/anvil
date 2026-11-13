//! CMake's built-in commands for variables, lists, strings, paths and files, plus the dispatcher that routes every
//! command to its handler. Target, test and dependency commands live in `target_commands.rs`.

use super::interp::{Flow, Interpreter};
use super::model::{self, Truth};
use super::parse::{Arg, ArgKind, Command};
use super::target_commands::TargetList;
use crate::util::{self, slash};
use anyhow::Result;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

// Commands with no effect on what gets built
const IGNORED_COMMANDS: &[&str] = &[
    "cmake_minimum_required",
    "cmake_policy",
    "enable_testing",
    "install",
    "export",
    "mark_as_advanced",
    "source_group",
    "include_guard",
    "set_directory_properties",
    "define_property",
    "configure_package_config_file",
    "write_basic_package_version_file",
    "cpack_add_component",
    "variable_watch",
    "site_name",
    "cmake_host_system_information",
    "include_regular_expression",
    "set_package_properties",
    "feature_summary",
    "add_feature_info",
    "build_command",
    "create_test_sourcelist",
    "install_files",
    "install_programs",
    "install_targets",
];

// Commands whose arguments CMake joins into one list before it reads generator expressions
const LIST_COMMANDS: &[&str] = &[
    "add_executable",
    "add_library",
    "set_property",
    "include_directories",
    "add_compile_definitions",
    "add_definitions",
    "add_compile_options",
    "link_libraries",
    "add_link_options",
    "link_directories",
];

// Commands the converter handles itself, even when the project defines a function of the same name (CPM.cmake does)
const INTERCEPTED_COMMANDS: &[&str] = &[
    "cpmaddpackage",
    "cpmfindpackage",
    "cpmusepackagelock",
    "cpmdeclarepackage",
    "fetchcontent_declare",
    "fetchcontent_makeavailable",
    "fetchcontent_populate",
    "fetchcontent_getproperties",
];

// Commands with a handler below or in target_commands.rs
const HANDLED_COMMANDS: &[&str] = &[
    "return",
    "break",
    "continue",
    "block",
    "endblock",
    "set",
    "unset",
    "option",
    "cmake_dependent_option",
    "list",
    "string",
    "math",
    "file",
    "get_filename_component",
    "cmake_path",
    "cmake_parse_arguments",
    "separate_arguments",
    "message",
    "include",
    "add_subdirectory",
    "project",
    "find_package",
    "find_program",
    "find_library",
    "find_path",
    "find_file",
    "configure_file",
    "execute_process",
    "try_compile",
    "try_run",
    "check_cxx_compiler_flag",
    "check_c_compiler_flag",
    "check_compiler_flag",
    "check_linker_flag",
    "check_include_file",
    "check_include_file_cxx",
    "check_include_files",
    "check_symbol_exists",
    "check_cxx_symbol_exists",
    "check_function_exists",
    "check_library_exists",
    "check_type_size",
    "check_cxx_source_compiles",
    "check_c_source_compiles",
    "check_source_compiles",
    "check_c_source_runs",
    "check_cxx_source_runs",
    "check_source_runs",
    "check_struct_has_member",
    "check_ipo_supported",
    "check_language",
    "test_big_endian",
    "get_property",
    "get_directory_property",
    "get_cmake_property",
    "get_source_file_property",
    "add_executable",
    "add_library",
    "target_sources",
    "target_include_directories",
    "target_compile_definitions",
    "target_compile_options",
    "target_link_libraries",
    "target_link_options",
    "target_link_directories",
    "target_compile_features",
    "target_precompile_headers",
    "set_target_properties",
    "set_tests_properties",
    "set_property",
    "aux_source_directory",
    "enable_language",
    "get_target_property",
    "set_source_files_properties",
    "include_directories",
    "add_compile_definitions",
    "add_definitions",
    "add_compile_options",
    "link_libraries",
    "add_link_options",
    "link_directories",
    "add_test",
    "gtest_discover_tests",
    "gtest_add_tests",
    "add_custom_command",
    "add_custom_target",
    "add_dependencies",
    "generate_export_header",
    "externalproject_add",
    "pkg_check_modules",
    "pkg_search_module",
];

// message() modes; anything else is part of the text
const MESSAGE_MODES: &[&str] = &[
    "FATAL_ERROR",
    "SEND_ERROR",
    "WARNING",
    "AUTHOR_WARNING",
    "DEPRECATION",
    "NOTICE",
    "STATUS",
    "VERBOSE",
    "DEBUG",
    "TRACE",
    "CHECK_START",
    "CHECK_PASS",
    "CHECK_FAIL",
];

/// Does CMake (or the converter) define this command? For if(COMMAND name).
pub fn is_builtin(name: &str) -> bool {
    HANDLED_COMMANDS.contains(&name) || IGNORED_COMMANDS.contains(&name) || INTERCEPTED_COMMANDS.contains(&name)
}

/// A command's arguments sorted by keyword, as cmake_parse_arguments does it.
#[derive(Debug, Default)]
pub struct Keywords {
    pub flags: HashSet<String>,
    pub values: HashMap<String, String>,
    pub lists: HashMap<String, Vec<Vec<String>>>,
    pub unparsed: Vec<String>,
    pub missing_values: Vec<String>,
}

impl Keywords {
    /// Sorts `args` by keyword: `flags` take no value, `single` one, `multi` the words up to the next keyword.
    pub fn parse(args: &[String], flags: &[&str], single: &[&str], multi: &[&str]) -> Keywords {
        let mut keywords = Keywords::default();
        let mut current: Option<(&str, bool)> = None;

        for arg in args {
            let word = arg.as_str();

            // A keyword starts a new group
            if flags.contains(&word) {
                keywords.flags.insert(arg.clone());
                current = None;
                continue;
            }

            if single.contains(&word) || multi.contains(&word) {
                if let Some((previous, _)) = current {
                    keywords.note_missing(previous);
                }
                let is_multi = multi.contains(&word);
                if is_multi {
                    keywords.lists.entry(arg.clone()).or_default().push(Vec::new());
                }
                current = Some((multi.iter().chain(single).find(|keyword| **keyword == word).unwrap(), is_multi));
                continue;
            }

            // A value for the current keyword
            match current {
                Some((keyword, true)) => keywords.lists.get_mut(keyword).unwrap().last_mut().unwrap().push(arg.clone()),
                Some((keyword, false)) => {
                    keywords.values.insert(keyword.to_string(), arg.clone());
                    current = None;
                }
                None => keywords.unparsed.push(arg.clone()),
            }
        }

        if let Some((keyword, _)) = current {
            keywords.note_missing(keyword);
        }
        keywords
    }

    pub fn has(&self, flag: &str) -> bool {
        self.flags.contains(flag)
    }

    pub fn value(&self, keyword: &str) -> Option<String> {
        self.values.get(keyword).cloned()
    }

    /// All the words given after `keyword`, over every occurrence.
    pub fn list(&self, keyword: &str) -> Vec<String> {
        self.lists.get(keyword).map(|groups| groups.concat()).unwrap_or_default()
    }

    /// Each occurrence of `keyword` on its own (add_custom_command's COMMANDs).
    pub fn groups(&self, keyword: &str) -> Vec<Vec<String>> {
        self.lists.get(keyword).cloned().unwrap_or_default()
    }

    fn note_missing(&mut self, keyword: &str) {
        let empty_list = self
            .lists
            .get(keyword)
            .is_some_and(|groups| groups.last().is_some_and(|group| group.is_empty()));
        if !self.values.contains_key(keyword) && (empty_list || !self.lists.contains_key(keyword)) {
            self.missing_values.push(keyword.to_string());
        }
    }
}

/// What the built-in commands keep besides variables.
#[derive(Debug, Default)]
pub struct BuiltinState {
    /// cmake_dependent_option()s whose dependencies hold only on some platforms: the option reads as `OPT & (these)`.
    pub option_conditions: HashMap<String, String>,
    /// set_property(GLOBAL ...): each property's value.
    pub global_properties: HashMap<String, String>,
    /// The PROPAGATE variables of each block() running now, innermost last.
    pub block_propagates: Vec<Vec<String>>,
    /// ExternalProject_Add names.
    pub external_projects: Vec<String>,
    /// Files configure_file() writes only for installing: kept when a test or build step reads them after all.
    pub install_writes: Vec<model::ConfigureWrite>,
    /// The ports in the project's vcpkg.json, read when a find_package() first needs them.
    pub manifest_ports: Option<Vec<String>>,
}

impl Interpreter {
    /// Runs one command: a user function, a built-in, or (when unknown) records it.
    pub(super) fn run_command(&mut self, command: &Command) -> Result<Flow> {
        let name = command.name.as_str();

        // User functions and macros, unless the converter handles the name itself
        if !INTERCEPTED_COMMANDS.contains(&name) {
            if let Some(function) = self.functions.get(name).cloned() {
                let args = self.evaluate_args(&command.args);
                return self.call_function(function, args);
            }
        }
        if IGNORED_COMMANDS.contains(&name) {
            return Ok(Flow::Normal);
        }

        let mut args = self.evaluate_args(&command.args);

        // Commands that read their arguments as one list: a generator expression the parser cut at whitespace is one
        // item again
        if name.starts_with("target_") || LIST_COMMANDS.contains(&name) {
            args = model::join_genex_fragments(args);
        }

        match name {
            // Control flow
            "return" => return Ok(Flow::Return(Truth::Known(true))),
            "break" => return Ok(Flow::Break),
            "continue" => return Ok(Flow::Continue),
            "block" => self.command_block(&args),
            "endblock" => self.command_endblock(),

            // Variables and values
            "set" => self.command_set(&args),
            "unset" => self.command_unset(&args),
            "option" => self.command_option(&args),
            "cmake_dependent_option" => self.command_dependent_option(&args),
            "list" => self.command_list(command, &args),
            "string" => self.command_string(command, &args),
            "math" => self.command_math(command, &args),
            "file" => self.command_file(command, &args),
            "get_filename_component" => self.command_get_filename_component(&args),
            "cmake_path" => self.command_cmake_path(command, &args),
            "cmake_parse_arguments" => self.command_parse_arguments(&args),
            "separate_arguments" => self.command_separate_arguments(&args),
            "message" => self.command_message(command, &args),

            // Files and projects
            "include" => self.command_include(command, &args)?,
            "add_subdirectory" => self.command_add_subdirectory(command, &args)?,
            "project" => self.command_project(&args),
            "find_package" => self.command_find_package(&args),
            "pkg_check_modules" | "pkg_search_module" => self.command_pkg_check_modules(&args),
            "find_program" | "find_library" | "find_path" | "find_file" => self.command_find(command, &args),
            "configure_file" => self.command_configure_file(command, &args),
            "execute_process" => self.command_execute_process(command, &args),
            "try_compile" | "try_run" => {
                self.warn(command, &format!("{name}() cannot run during conversion; assumed to fail"));
                if let Some(variable) = args.first() {
                    self.set_variable(variable, "OFF");
                }
            }
            "get_property" => self.command_get_property(&args),
            "get_cmake_property" => self.command_get_cmake_property(&args),
            "get_directory_property" | "get_source_file_property" => {
                if let Some(variable) = args.first() {
                    self.set_variable(variable, "");
                }
            }
            "test_big_endian" => {
                // Every platform Anvil builds for is little-endian
                if let Some(variable) = args.first() {
                    self.set_variable(variable, "0");
                }
            }
            "check_language" => {
                // Only the languages Anvil always has; an optional one (CUDA, Fortran) is not found
                if let Some(language) = args.first() {
                    let variable = format!("CMAKE_{language}_COMPILER");
                    if self.get_variable(&variable).is_none() {
                        self.set_variable(&variable, &format!("{variable}-NOTFOUND"));
                    }
                }
            }
            check if check.starts_with("check_") => self.command_check(command, &args),

            // Targets, tests and dependencies
            "add_executable" => self.command_add_executable(&args),
            "add_library" => self.command_add_library(&args),
            "target_sources" => self.command_target_sources(command, &args),
            "target_include_directories" => self.command_target_include_directories(command, &args),
            "target_compile_definitions" => self.command_target_list(command, &args, TargetList::Defines),
            "target_compile_options" => self.command_target_list(command, &args, TargetList::Options),
            "target_link_libraries" => self.command_target_link_libraries(command, &args),
            "target_link_options" => self.command_target_list(command, &args, TargetList::LinkOptions),
            "target_link_directories" => self.command_target_list(command, &args, TargetList::LibDirs),
            "target_compile_features" => self.command_target_compile_features(command, &args),
            "target_precompile_headers" => self.command_target_precompile_headers(command, &args),
            "set_target_properties" => self.command_set_target_properties(command, &args),
            "set_tests_properties" => self.command_set_tests_properties(command, &args),
            "set_property" if args.first().map(String::as_str) == Some("GLOBAL") => self.command_set_global_property(&args),
            "set_property" => self.command_set_property(command, &args),
            "aux_source_directory" => self.command_aux_source_directory(&args),
            "enable_language" => self.command_enable_language(command, &args),
            "get_target_property" => self.command_get_target_property(&args),
            "set_source_files_properties" => self.command_set_source_files_properties(&args),
            "include_directories" => self.command_include_directories(&args),
            "add_compile_definitions" => self.command_directory_list(&args, TargetList::Defines),
            "add_definitions" => self.command_add_definitions(&args),
            "add_compile_options" => self.command_directory_list(&args, TargetList::Options),
            "link_libraries" => self.command_directory_list(&args, TargetList::Links),
            "add_link_options" => self.command_directory_list(&args, TargetList::LinkOptions),
            "link_directories" => self.command_directory_list(&args, TargetList::LibDirs),
            "add_test" => self.command_add_test(&args),
            "gtest_discover_tests" | "gtest_add_tests" => self.command_gtest_tests(&args),
            "add_custom_command" => self.command_add_custom_command(command, &args),
            "add_custom_target" => self.command_add_custom_target(&args),
            "add_dependencies" => self.command_add_dependencies(command, &args),
            "generate_export_header" => self.command_generate_export_header(&args),
            "fetchcontent_declare" => self.command_fetchcontent_declare(&args),
            "fetchcontent_makeavailable" | "fetchcontent_populate" | "fetchcontent_getproperties" => self.command_fetchcontent_available(&args),
            "cpmaddpackage" | "cpmfindpackage" | "cpmdeclarepackage" => self.command_cpm_add_package(&args),
            "cpmusepackagelock" => {}
            "externalproject_add" => {
                let name = args.first().cloned().unwrap_or_default();
                self.warn(
                    command,
                    &format!("ExternalProject_Add({name}) is not converted: it downloads and builds another project; add what it provides to [deps] by hand"),
                );
                self.builtin.external_projects.push(name);
            }

            _ => {
                self.unknown_commands.insert(command.name.clone());
            }
        }

        Ok(Flow::Normal)
    }

    // Variables

    /// block([SCOPE_FOR ...] [PROPAGATE vars...]): a variable scope; endblock() hands the PROPAGATE ones out.
    fn command_block(&mut self, args: &[String]) {
        let propagate = args.iter().skip_while(|word| *word != "PROPAGATE").skip(1).cloned().collect();
        self.builtin.block_propagates.push(propagate);
        self.scopes.push(HashMap::new());
    }

    fn command_endblock(&mut self) {
        if self.scopes.len() <= 1 {
            return;
        }

        // The propagated variables' values in the block, set (or unset) where the block is
        let propagate = self.builtin.block_propagates.pop().unwrap_or_default();
        let values: Vec<(String, Option<String>)> = propagate.into_iter().map(|name| (name.clone(), self.get_variable(&name))).collect();
        self.scopes.pop();
        for (name, value) in values {
            match value {
                Some(value) => self.set_variable(&name, &value),
                None => self.unset_variable(&name),
            }
        }
    }

    fn command_set(&mut self, args: &[String]) {
        let Some((name, values)) = args.split_first() else { return };

        // set(ENV{X} ...) changes nothing the converter reads back
        if name.starts_with("ENV{") {
            return;
        }

        // set(X value CACHE TYPE "doc" [FORCE]): a BOOL cache entry is an option
        if let Some(cache_index) = values.iter().position(|value| value == "CACHE") {
            let value = values[..cache_index].join(";");
            let rest = &values[cache_index + 1..];
            let kind = rest.first().map(String::as_str).unwrap_or("STRING");
            let force = rest.iter().any(|word| word == "FORCE") || kind == "INTERNAL";

            if kind == "BOOL" && self.get_variable(name).is_none() {
                let description = rest.get(1).cloned().unwrap_or_default();
                self.options
                    .insert(name.clone(), (model::constant_truth(&value).unwrap_or(false), description));
            }
            if force && self.options.contains_key(name) {
                self.overridden_options.insert(name.clone());
            }
            if force || !self.cache.contains_key(name) {
                self.cache.insert(name.clone(), value);
            }
            return;
        }

        if values.last().map(String::as_str) == Some("PARENT_SCOPE") {
            match &values[..values.len() - 1] {
                [] if self.scopes.len() >= 2 => self.unset_in(self.scopes.len() - 2, name),
                [] => {}
                values => self.set_parent_variable(name, &values.join(";")),
            }
            return;
        }

        if values.is_empty() {
            self.unset_variable(name);
        } else {
            self.set_variable(name, &values.join(";"));
        }
    }

    fn command_unset(&mut self, args: &[String]) {
        let Some(name) = args.first() else { return };

        match args.get(1).map(String::as_str) {
            Some("CACHE") => {
                self.cache.remove(name);
            }
            Some("PARENT_SCOPE") => {
                let count = self.scopes.len();
                if count >= 2 {
                    self.unset_in(count - 2, name);
                }
            }
            _ => self.unset_variable(name),
        }
    }

    /// option(NAME "doc" default): becomes an Anvil option, and tests of it become conditions.
    fn command_option(&mut self, args: &[String]) {
        let Some(name) = args.first() else { return };
        if self.get_variable(name).is_some() || self.options.contains_key(name) {
            return;
        }

        let default = args.get(2).map(|value| model::constant_truth(value).unwrap_or(false)).unwrap_or(false);
        let description = args.get(1).cloned().unwrap_or_default();
        self.options.insert(name.clone(), (default, description));
        self.cache.insert(name.clone(), if default { "ON" } else { "OFF" }.into());
    }

    /// cmake_dependent_option(NAME "doc" default "dependencies" force): an option where its `;`-separated
    /// dependencies hold, else the variable `force`. Dependencies that hold on some platforms only stay a condition of
    /// the option: it reads as `NAME & (dependencies)`.
    fn command_dependent_option(&mut self, args: &[String]) {
        let [name, _, _, depends, force, ..] = args else {
            self.command_option(args);
            return;
        };
        if self.get_variable(name).is_some() || self.options.contains_key(name) {
            return;
        }

        // Each dependency is an if() condition, its words separated by spaces
        let truth = model::split_list(depends).iter().fold(Truth::Known(true), |truth, dependency| {
            let words: Vec<Arg> = dependency
                .split_whitespace()
                .map(|word| Arg {
                    text: word.to_string(),
                    kind: ArgKind::Unquoted,
                })
                .collect();
            truth.and(self.evaluate_condition(&words))
        });

        match truth {
            Truth::Known(true) => self.command_option(args),
            Truth::Known(false) => self.set_variable(name, force),
            Truth::Depends(condition) => {
                self.command_option(args);
                self.builtin.option_conditions.insert(name.clone(), condition);
            }
        }
    }

    // Lists

    fn command_list(&mut self, command: &Command, args: &[String]) {
        let [operation, variable, rest @ ..] = args else { return };
        let mut items = model::list_elements(&self.get_variable(variable).unwrap_or_default());

        match operation.as_str() {
            "LENGTH" => {
                if let Some(output) = rest.first() {
                    self.set_variable(output, &items.len().to_string());
                }
            }
            "GET" => {
                let Some((output, indexes)) = rest.split_last() else { return };
                let picked: Vec<String> = indexes
                    .iter()
                    .filter_map(|index| list_index(index, items.len()))
                    .map(|index| items[index].clone())
                    .collect();
                self.set_variable(output, &picked.join(";"));
            }
            "JOIN" => {
                let [glue, output, ..] = rest else { return };
                self.set_variable(output, &items.join(glue));
            }
            "SUBLIST" => {
                let [begin, length, output, ..] = rest else { return };
                let begin: usize = begin.parse().unwrap_or(0).min(items.len());
                let length: i64 = length.parse().unwrap_or(-1);
                let end = if length < 0 {
                    items.len()
                } else {
                    (begin + length as usize).min(items.len())
                };
                self.set_variable(output, &items[begin..end].join(";"));
            }
            "FIND" => {
                let [value, output, ..] = rest else { return };
                let index = items.iter().position(|item| item == value).map(|index| index as i64).unwrap_or(-1);
                self.set_variable(output, &index.to_string());
            }
            "APPEND" => {
                items.extend(rest.iter().cloned());
                self.set_variable(variable, &items.join(";"));
            }
            "PREPEND" => {
                let mut prepended = rest.to_vec();
                prepended.append(&mut items);
                self.set_variable(variable, &prepended.join(";"));
            }
            "INSERT" => {
                let Some((index, values)) = rest.split_first() else { return };
                let index = list_index(index, items.len() + 1).unwrap_or(items.len());
                items.splice(index..index, values.iter().cloned());
                self.set_variable(variable, &items.join(";"));
            }
            "POP_BACK" | "POP_FRONT" => {
                let count = rest.len().max(1);
                let mut popped = Vec::new();
                for _ in 0..count.min(items.len()) {
                    popped.push(if operation == "POP_BACK" {
                        items.pop().unwrap()
                    } else {
                        items.remove(0)
                    });
                }
                for (output, value) in rest.iter().zip(&popped) {
                    self.set_variable(output, value);
                }
                self.set_variable(variable, &items.join(";"));
            }
            "REMOVE_ITEM" => {
                items.retain(|item| !rest.contains(item));
                self.set_variable(variable, &items.join(";"));
            }
            "REMOVE_AT" => {
                let length = items.len();
                let removed: HashSet<usize> = rest.iter().filter_map(|index| list_index(index, length)).collect();
                let kept: Vec<String> = items
                    .into_iter()
                    .enumerate()
                    .filter(|(index, _)| !removed.contains(index))
                    .map(|(_, item)| item)
                    .collect();
                self.set_variable(variable, &kept.join(";"));
            }
            "REMOVE_DUPLICATES" => {
                let mut seen = HashSet::new();
                items.retain(|item| seen.insert(item.clone()));
                self.set_variable(variable, &items.join(";"));
            }
            "REVERSE" => {
                items.reverse();
                self.set_variable(variable, &items.join(";"));
            }
            "SORT" => {
                let insensitive = rest.windows(2).any(|pair| pair[0] == "CASE" && pair[1] == "INSENSITIVE");
                if insensitive {
                    items.sort_by_key(|item| item.to_lowercase());
                } else {
                    items.sort();
                }
                if rest.windows(2).any(|pair| pair[0] == "ORDER" && pair[1] == "DESCENDING") {
                    items.reverse();
                }
                self.set_variable(variable, &items.join(";"));
            }
            "FILTER" => {
                let [mode, _, pattern, ..] = rest else { return };
                let Some(pattern) = self.compile_regex(command, pattern) else { return };
                items.retain(|item| pattern.is_match(item) == (mode == "INCLUDE"));
                self.set_variable(variable, &items.join(";"));
            }
            "TRANSFORM" => self.list_transform(command, variable, items, rest),
            _ => self.warn(command, &format!("list({operation}) is not supported")),
        }
    }

    /// list(TRANSFORM var ACTION [args] [AT i... | FOR a b [step] | REGEX r] [OUTPUT_VARIABLE out]).
    fn list_transform(&mut self, command: &Command, variable: &str, mut items: Vec<String>, rest: &[String]) {
        let Some((action, rest)) = rest.split_first() else { return };
        let argument_count = match action.as_str() {
            "APPEND" | "PREPEND" => 1,
            "REPLACE" => 2,
            _ => 0,
        };
        if rest.len() < argument_count {
            return;
        }
        let (action_args, rest) = rest.split_at(argument_count);

        // Output variable and selector
        let mut output = variable.to_string();
        let mut selected: Option<HashSet<usize>> = None;
        let mut selector_regex = None;
        let mut position = 0;
        while position < rest.len() {
            match rest[position].as_str() {
                "OUTPUT_VARIABLE" => {
                    output = rest.get(position + 1).cloned().unwrap_or(output);
                    position += 2;
                }
                "AT" => {
                    let mut indexes = HashSet::new();
                    position += 1;
                    while let Some(index) = rest.get(position).and_then(|index| list_index(index, items.len())) {
                        indexes.insert(index);
                        position += 1;
                    }
                    selected = Some(indexes);
                }
                "FOR" => {
                    let numbers: Vec<usize> = rest[position + 1..].iter().map_while(|number| number.parse().ok()).collect();
                    let (start, stop, step) = match numbers.as_slice() {
                        [start, stop] => (*start, *stop, 1),
                        [start, stop, step, ..] => (*start, *stop, (*step).max(1)),
                        _ => (0, 0, 1),
                    };
                    selected = Some((start..=stop).step_by(step).collect());
                    position += 1 + numbers.len().min(3);
                }
                "REGEX" => {
                    selector_regex = rest.get(position + 1).and_then(|pattern| self.compile_regex(command, pattern));
                    position += 2;
                }
                _ => position += 1,
            }
        }

        // Transform the selected items
        let replace_regex = if action == "REPLACE" {
            self.compile_regex(command, &action_args[0])
        } else {
            None
        };
        for (index, item) in items.iter_mut().enumerate() {
            let chosen = selected.as_ref().map_or(true, |indexes| indexes.contains(&index))
                && selector_regex.as_ref().map_or(true, |pattern| pattern.is_match(item));
            if !chosen {
                continue;
            }

            *item = match action.as_str() {
                "APPEND" => format!("{item}{}", action_args[0]),
                "PREPEND" => format!("{}{item}", action_args[0]),
                "TOLOWER" => item.to_lowercase(),
                "TOUPPER" => item.to_uppercase(),
                "STRIP" => item.trim().to_string(),
                "GENEX_STRIP" => strip_genex(item),
                "REPLACE" => match &replace_regex {
                    Some(pattern) => pattern.replace_all(item, cmake_replacement(&action_args[1]).as_str()).into_owned(),
                    None => item.clone(),
                },
                _ => item.clone(),
            };
        }

        self.set_variable(&output, &items.join(";"));
    }

    // Strings

    fn command_string(&mut self, command: &Command, args: &[String]) {
        let Some((operation, rest)) = args.split_first() else { return };

        match operation.as_str() {
            "APPEND" | "PREPEND" => {
                let Some((variable, values)) = rest.split_first() else { return };
                let current = self.get_variable(variable).unwrap_or_default();
                let added = values.concat();
                let value = if operation == "APPEND" {
                    format!("{current}{added}")
                } else {
                    format!("{added}{current}")
                };
                self.set_variable(variable, &value);
            }
            "CONCAT" => {
                let Some((output, values)) = rest.split_first() else { return };
                self.set_variable(output, &values.concat());
            }
            "JOIN" => {
                let [glue, output, values @ ..] = rest else { return };
                self.set_variable(output, &values.join(glue));
            }
            "TOLOWER" | "TOUPPER" => {
                let [input, output, ..] = rest else { return };
                let value = if operation == "TOLOWER" {
                    input.to_lowercase()
                } else {
                    input.to_uppercase()
                };
                self.set_variable(output, &value);
            }
            "LENGTH" => {
                let [input, output, ..] = rest else { return };
                self.set_variable(output, &input.len().to_string());
            }
            "SUBSTRING" => {
                let [input, begin, length, output, ..] = rest else { return };

                // Part of a hash is still the hash: Anvil's {hash:...} is already short
                if input.starts_with("{hash:") && input.ends_with('}') {
                    self.set_variable(output, input);
                    return;
                }

                let begin: usize = begin.parse().unwrap_or(0).min(input.len());
                let length: i64 = length.parse().unwrap_or(-1);
                let end = if length < 0 {
                    input.len()
                } else {
                    (begin + length as usize).min(input.len())
                };
                self.set_variable(output, input.get(begin..end).unwrap_or(""));
            }
            "STRIP" => {
                let [input, output, ..] = rest else { return };
                self.set_variable(output, input.trim());
            }
            "GENEX_STRIP" => {
                let [input, output, ..] = rest else { return };
                self.set_variable(output, &strip_genex(input));
            }
            "REPEAT" => {
                let [input, count, output, ..] = rest else { return };
                self.set_variable(output, &input.repeat(count.parse().unwrap_or(0)));
            }
            "REPLACE" => {
                let [search, replacement, output, inputs @ ..] = rest else { return };
                let input = inputs.concat();
                let value = if search.is_empty() {
                    input
                } else {
                    input.replace(search.as_str(), replacement)
                };
                self.set_variable(output, &value);
            }
            "FIND" => {
                let [input, search, output, flags @ ..] = rest else { return };
                let found = if flags.iter().any(|flag| flag == "REVERSE") {
                    input.rfind(search.as_str())
                } else {
                    input.find(search.as_str())
                };
                self.set_variable(output, &found.map(|index| index as i64).unwrap_or(-1).to_string());
            }
            "COMPARE" => {
                let [mode, left, right, output, ..] = rest else { return };
                let result = match mode.as_str() {
                    "EQUAL" => left == right,
                    "NOTEQUAL" => left != right,
                    "LESS" => left < right,
                    "GREATER" => left > right,
                    "LESS_EQUAL" => left <= right,
                    "GREATER_EQUAL" => left >= right,
                    _ => false,
                };
                self.set_variable(output, if result { "1" } else { "0" });
            }
            "MAKE_C_IDENTIFIER" => {
                let [input, output, ..] = rest else { return };
                let mut identifier: String = input
                    .chars()
                    .map(|character| if character.is_ascii_alphanumeric() { character } else { '_' })
                    .collect();
                if identifier.starts_with(|character: char| character.is_ascii_digit()) {
                    identifier.insert(0, '_');
                }
                self.set_variable(output, &identifier);
            }
            "REGEX" => self.string_regex(command, rest),
            "JSON" => self.string_json(command, rest),
            "CONFIGURE" => {
                let [input, output, flags @ ..] = rest else { return };
                let only_at = flags.iter().any(|flag| flag == "@ONLY");
                let value = self.configure_text(input, only_at);
                self.set_variable(output, &value);
            }
            "ASCII" => {
                let Some((output, codes)) = rest.split_last() else { return };
                let text: String = codes.iter().filter_map(|code| code.parse::<u8>().ok()).map(char::from).collect();
                self.set_variable(output, &text);
            }
            "HEX" => {
                let [input, output, ..] = rest else { return };
                let hex: String = input.bytes().map(|byte| format!("{byte:02x}")).collect();
                self.set_variable(output, &hex);
            }
            "MD5" | "SHA1" | "SHA224" | "SHA256" | "SHA384" | "SHA512" | "SHA3_224" | "SHA3_256" | "SHA3_384" | "SHA3_512" => {
                let [output, input, ..] = rest else { return };

                // A digest of file digests (and the compiler's name) is Anvil's {hash:...}, computed on every build;
                // of plain text, a stand-in that is only good for making names unique
                let mut items = hashed_files(input);
                if input.split([' ', ';', '-', '_']).any(|word| COMPILER_IDS.contains(&word)) {
                    items.insert(0, "compiler".to_string());
                }
                let value = if items.is_empty() {
                    format!("{:016x}", util::hash_bytes(input.as_bytes()))
                } else {
                    format!("{{hash:{}}}", self.globbed(items).join(","))
                };
                self.set_variable(output, &value);
            }
            "TIMESTAMP" | "RANDOM" | "UUID" => {
                if let Some(output) = rest.first() {
                    self.set_variable(output, &format!("{{unconverted:string({operation})}}"));
                }
            }
            _ => self.warn(command, &format!("string({operation}) is not supported")),
        }
    }

    /// string(REGEX MATCH|MATCHALL|REPLACE ...); sets CMAKE_MATCH_<n> as CMake does.
    fn string_regex(&mut self, command: &Command, args: &[String]) {
        let Some((mode, rest)) = args.split_first() else { return };

        match mode.as_str() {
            "MATCH" | "MATCHALL" => {
                let [pattern, output, inputs @ ..] = rest else { return };
                let Some(pattern) = self.compile_regex(command, pattern) else { return };
                let input = inputs.concat();

                if mode == "MATCH" {
                    let captures = pattern.captures(&input);
                    let matched = captures
                        .as_ref()
                        .and_then(|captures| captures.get(0))
                        .map(|found| found.as_str().to_string())
                        .unwrap_or_default();
                    self.set_variable(output, &matched);
                    if let Some(captures) = captures {
                        let groups: Vec<String> = (0..captures.len())
                            .map(|index| captures.get(index).map(|found| found.as_str().to_string()).unwrap_or_default())
                            .collect();
                        self.set_match_variables(&groups);
                    }
                } else {
                    let matches: Vec<&str> = pattern.find_iter(&input).map(|found| found.as_str()).collect();
                    self.set_variable(output, &matches.join(";"));
                }
            }
            "REPLACE" => {
                let [pattern, replacement, output, inputs @ ..] = rest else { return };
                let Some(pattern) = self.compile_regex(command, pattern) else { return };
                let input = inputs.concat();
                let value = pattern.replace_all(&input, cmake_replacement(replacement).as_str()).into_owned();
                self.set_variable(output, &value);
            }
            _ => self.warn(command, &format!("string(REGEX {mode}) is not supported")),
        }
    }

    /// CMAKE_MATCH_<n> from a match's groups (none when it did not match), as string(REGEX) and if(MATCHES) do.
    pub(super) fn set_match_variables(&mut self, groups: &[String]) {
        for index in 0..10 {
            let value = groups.get(index).cloned().unwrap_or_default();
            self.set_variable(&format!("CMAKE_MATCH_{index}"), &value);
        }
        self.set_variable("CMAKE_MATCH_COUNT", &groups.len().saturating_sub(1).to_string());
    }

    /// string(JSON out [ERROR_VARIABLE err] GET|LENGTH|TYPE|MEMBER json path...).
    fn string_json(&mut self, command: &Command, args: &[String]) {
        let Some((output, mut rest)) = args.split_first() else { return };

        let mut error_variable = None;
        if rest.first().map(String::as_str) == Some("ERROR_VARIABLE") && rest.len() >= 2 {
            error_variable = Some(rest[1].clone());
            rest = &rest[2..];
        }
        let [mode, json, path @ ..] = rest else { return };

        let Ok(document) = serde_json::from_str::<serde_json::Value>(json) else {
            match error_variable {
                Some(variable) => self.set_variable(&variable, "invalid JSON"),
                None => self.warn(command, "string(JSON): invalid JSON"),
            }
            self.set_variable(output, &format!("{output}-NOTFOUND"));
            return;
        };

        // Walk the path (MEMBER's last word is an index, not a step)
        let steps = if mode == "MEMBER" && !path.is_empty() {
            &path[..path.len() - 1]
        } else {
            path
        };
        let mut node = Some(&document);
        for step in steps {
            node = node.and_then(|value| match value {
                serde_json::Value::Array(array) => step.parse::<usize>().ok().and_then(|index| array.get(index)),
                serde_json::Value::Object(object) => object.get(step),
                _ => None,
            });
        }
        let Some(node) = node else {
            if let Some(variable) = error_variable {
                self.set_variable(&variable, "member not found");
            }
            self.set_variable(output, &format!("{output}-NOTFOUND"));
            return;
        };

        let value = match mode.as_str() {
            "GET" => match node {
                serde_json::Value::String(text) => text.clone(),
                serde_json::Value::Bool(flag) => if *flag { "ON" } else { "OFF" }.into(),
                serde_json::Value::Null => String::new(),
                other => other.to_string(),
            },
            "LENGTH" => match node {
                serde_json::Value::Array(array) => array.len().to_string(),
                serde_json::Value::Object(object) => object.len().to_string(),
                _ => "0".into(),
            },
            "TYPE" => match node {
                serde_json::Value::Null => "NULL",
                serde_json::Value::Bool(_) => "BOOLEAN",
                serde_json::Value::Number(_) => "NUMBER",
                serde_json::Value::String(_) => "STRING",
                serde_json::Value::Array(_) => "ARRAY",
                serde_json::Value::Object(_) => "OBJECT",
            }
            .into(),
            "MEMBER" => {
                let index: usize = path.last().and_then(|index| index.parse().ok()).unwrap_or(0);
                node.as_object().and_then(|object| object.keys().nth(index)).cloned().unwrap_or_default()
            }
            _ => {
                self.warn(command, &format!("string(JSON {mode}) is not supported"));
                String::new()
            }
        };

        if let Some(variable) = error_variable {
            self.set_variable(&variable, "NOTFOUND");
        }
        self.set_variable(output, &value);
    }

    /// Replaces @VAR@ (and ${VAR} unless `only_at`), as configure_file and string(CONFIGURE) do.
    pub(super) fn configure_text(&self, text: &str, only_at: bool) -> String {
        let at_pattern = regex::Regex::new(r"@([A-Za-z_][A-Za-z0-9_]*)@").unwrap();
        let replaced = at_pattern
            .replace_all(text, |captures: &regex::Captures| self.get_variable(&captures[1]).unwrap_or_default())
            .into_owned();

        if only_at {
            replaced
        } else {
            self.expand(&replaced)
        }
    }

    fn compile_regex(&mut self, command: &Command, pattern: &str) -> Option<regex::Regex> {
        match regex::Regex::new(&super::check_script::anvil_regex(pattern)) {
            Ok(compiled) => Some(compiled),
            Err(_) => {
                self.warn(command, &format!("regular expression `{pattern}` is not supported"));
                None
            }
        }
    }

    // Math

    fn command_math(&mut self, command: &Command, args: &[String]) {
        let [mode, output, expression, rest @ ..] = args else { return };
        if mode != "EXPR" {
            return;
        }

        let Some(value) = evaluate_math(expression) else {
            self.warn(command, &format!("math(EXPR): cannot evaluate `{expression}`"));
            return;
        };

        let hexadecimal = rest.windows(2).any(|pair| pair[0] == "OUTPUT_FORMAT" && pair[1] == "HEXADECIMAL");
        let text = if hexadecimal { format!("0x{value:x}") } else { value.to_string() };
        self.set_variable(output, &text);
    }

    // Files

    fn command_file(&mut self, command: &Command, args: &[String]) {
        let Some((operation, rest)) = args.split_first() else { return };

        match operation.as_str() {
            "GLOB" | "GLOB_RECURSE" => self.file_glob(command, operation == "GLOB_RECURSE", rest),
            "READ" => {
                let [file, output, options @ ..] = rest else { return };
                // As CMake on Windows reads it (text mode): line ends as \n, which OFFSET and LIMIT count in bytes
                let text = std::fs::read_to_string(self.resolve_path(file)).unwrap_or_default().replace("\r\n", "\n");

                let number = |name: &str| {
                    let position = options.iter().position(|word| word == name)?;
                    options.get(position + 1)?.parse::<usize>().ok()
                };
                let start = number("OFFSET").unwrap_or(0).min(text.len());
                let end = number("LIMIT").map_or(text.len(), |limit| (start + limit).min(text.len()));
                let text = String::from_utf8_lossy(&text.as_bytes()[start..end]);
                self.set_variable(output, &text);
            }
            "STRINGS" => {
                let [file, output, options @ ..] = rest else { return };
                let text = std::fs::read_to_string(self.resolve_path(file)).unwrap_or_default();
                let filter = options
                    .windows(2)
                    .find(|pair| pair[0] == "REGEX")
                    .and_then(|pair| self.compile_regex(command, &pair[1]));
                let lines: Vec<String> = text
                    .lines()
                    .filter(|line| filter.as_ref().map_or(true, |pattern| pattern.is_match(line)))
                    .map(|line| line.replace(';', "\\;"))
                    .collect();
                self.set_variable(output, &lines.join(";"));
            }
            "TO_CMAKE_PATH" | "TO_NATIVE_PATH" => {
                let [input, output, ..] = rest else { return };
                self.set_variable(output, &input.replace('\\', "/"));
            }
            "REAL_PATH" => {
                let [input, output, ..] = rest else { return };
                self.set_variable(output, &slash(&self.resolve_path(input)));
            }
            "RELATIVE_PATH" => {
                let [output, directory, file, ..] = rest else { return };
                let relative = util::rel(&self.resolve_path(file), &self.resolve_path(directory));
                self.set_variable(output, &relative);
            }
            "SHA224" | "SHA256" | "SHA384" | "SHA512" | "MD5" | "SHA1" | "SHA3_256" | "SHA3_512" => {
                // Anvil hashes the file on every build: {hash:file}. A CMake file stands for the compile flags it set,
                // which now live in anvil.toml: {hash:flags} is the target's flags
                let [file, output, ..] = rest else { return };
                let path = slash(&util::normalize(&self.resolve_path(file)));
                let is_cmake_file = path.ends_with("/CMakeLists.txt") || path.to_lowercase().ends_with(".cmake");
                let item = if is_cmake_file { "flags" } else { &path };
                self.set_variable(output, &format!("{{hash:{item}}}"));
            }
            "SIZE" | "TIMESTAMP" | "READ_SYMLINK" => {
                // Values made from it are left out of anvil.toml with a TODO where they were used
                if let Some(output) = rest.get(1) {
                    self.set_variable(output, &format!("{{unconverted:file({operation})}}"));
                }
            }
            "WRITE" | "APPEND" => {
                let [file, content @ ..] = rest else { return };
                let path = self.resolve_path(file);
                self.record_write(command, path, Some(content.concat()), operation == "APPEND");
            }
            "GENERATE" | "CONFIGURE" => {
                let keywords = Keywords::parse(
                    rest,
                    &["@ONLY", "ESCAPE_QUOTES", "NO_SOURCE_PERMISSIONS", "USE_SOURCE_PERMISSIONS"],
                    &["OUTPUT", "CONTENT", "INPUT", "CONDITION", "TARGET", "NEWLINE_STYLE"],
                    &["FILE_PERMISSIONS"],
                );
                // $<CONFIG> is the profile; other generator expressions, an input file or a condition are not known now
                let in_profile = |text: String| text.replace("$<CONFIG>", "{config}").replace("$<CONFIGURATION>", "{config}");
                let Some(output) = keywords.value("OUTPUT").map(in_profile) else {
                    return;
                };
                let content = keywords
                    .value("CONTENT")
                    .map(in_profile)
                    .filter(|content| !content.contains("$<") && !output.contains("$<"));
                let known = keywords.value("INPUT").is_none() && keywords.value("CONDITION").map_or(true, |condition| condition == "1");
                let path = self.resolve_output(&output);
                match content.filter(|_| known) {
                    Some(content) if operation == "CONFIGURE" => {
                        let (only_at, escape_quotes) = (keywords.has("@ONLY"), keywords.has("ESCAPE_QUOTES"));
                        let variants = self.configure_variants(command, "CONTENT", &content, only_at, escape_quotes, is_c_family(&path));
                        self.record_variants(command, &path, variants);
                    }
                    content => self.record_write(command, path, content, false),
                }
            }
            "COPY" => {
                let files: Vec<&String> = rest.iter().take_while(|word| *word != "DESTINATION").collect();
                let destination = rest.iter().skip_while(|word| *word != "DESTINATION").nth(1).cloned().unwrap_or_default();
                self.warn(
                    command,
                    &format!(
                        "file(COPY) of {} file(s) or folder(s) to {destination} at configure time is not converted; copy them with a build step, or read them where they are",
                        files.len()
                    ),
                );
            }
            "DOWNLOAD" | "UPLOAD" => self.warn(command, &format!("file({operation}) is not converted")),
            "COPY_FILE" => {
                let [from, to, ..] = rest else { return };
                self.warn(
                    command,
                    &format!("file(COPY_FILE {from} {to}) at configure time is not converted; copy it with a build step, or read it where it is"),
                );
            }

            // Changes to the file system that a configure does but conversion must not
            _ => {}
        }
    }

    /// A file written at configure time; APPEND adds to an earlier write of the same file.
    fn record_write(&mut self, command: &Command, path: PathBuf, text: Option<String>, append: bool) {
        // Text made from a process's output, or from something not found, is not known now
        let unknown = self.calls_ran_process.iter().any(|ran| *ran);
        let text = text.filter(|text| !unknown && !text.contains("-NOTFOUND"));

        let key = util::path_key(&path);
        let earlier = self.configure_writes.iter_mut().rfind(|write| util::path_key(&write.path) == key);
        if let (true, Some(earlier)) = (append, earlier) {
            earlier.text = earlier.text.take().zip(text).map(|(before, added)| before + &added);
            return;
        }

        let location = self.location(command);
        let condition = self.current_condition();
        self.configure_writes.push(model::ConfigureWrite {
            location,
            path,
            text,
            condition,
        });
    }

    /// file(GLOB[_RECURSE] var [LIST_DIRECTORIES b] [RELATIVE dir] [CONFIGURE_DEPENDS] patterns...).
    fn file_glob(&mut self, command: &Command, recursive: bool, args: &[String]) {
        let Some((output, rest)) = args.split_first() else { return };
        let keywords = Keywords::parse(rest, &["CONFIGURE_DEPENDS", "FOLLOW_SYMLINKS"], &["LIST_DIRECTORIES", "RELATIVE"], &[]);
        let list_directories = keywords
            .value("LIST_DIRECTORIES")
            .map_or(!recursive, |value| model::constant_truth(&value).unwrap_or(false));
        let relative_to = keywords.value("RELATIVE").map(|dir| self.resolve_path(&dir));

        let mut found = Vec::new();
        for pattern in &keywords.unparsed {
            let absolute = slash(&self.resolve_path(pattern));
            let full_pattern = glob_pattern(&absolute, recursive);
            let Ok(matcher) = crate::targets::compile_glob(&full_pattern) else {
                self.warn(command, &format!("file(GLOB): cannot read the pattern `{pattern}`"));
                continue;
            };
            let matcher = matcher.compile_matcher();

            // Walk from the deepest folder without wildcards
            let base = glob_base(&absolute);
            let depth = if recursive || full_pattern.contains("**") {
                usize::MAX
            } else {
                full_pattern[base.len()..].matches('/').count()
            };
            let mut matched: Vec<PathBuf> = walkdir::WalkDir::new(&base)
                .min_depth(1)
                .max_depth(depth)
                .into_iter()
                .filter_map(|entry| entry.ok())
                .filter(|entry| list_directories || entry.file_type().is_file())
                .map(|entry| entry.into_path())
                .filter(|path| matcher.is_match(slash(path)))
                .collect();
            matched.sort();

            let normalized = |path: &Path| slash(&util::normalize(path));
            let files = matched.iter().map(|path| normalized(path)).collect();
            self.glob_results.push((normalized(Path::new(&full_pattern)), files));
            found.extend(matched);
        }

        let values: Vec<String> = found
            .iter()
            .map(|path| match &relative_to {
                Some(dir) => util::rel(path, dir),
                None => slash(path),
            })
            .collect();
        self.set_variable(output, &values.join(";"));
    }

    /// Hashed files with every file(GLOB) result they hold whole as its pattern, so files added later count too.
    fn globbed(&self, mut items: Vec<String>) -> Vec<String> {
        for (pattern, files) in &self.glob_results {
            let positions: Option<Vec<usize>> = files.iter().map(|file| items.iter().position(|item| item == file)).collect();
            let Some(positions) = positions.filter(|positions| !positions.is_empty()) else {
                continue;
            };

            let first = *positions.iter().min().unwrap();
            items[first] = pattern.clone();
            let mut index = 0;
            items.retain(|_| {
                index += 1;
                index - 1 == first || !positions.contains(&(index - 1))
            });
        }
        items
    }

    fn command_get_filename_component(&mut self, args: &[String]) {
        let [output, file, mode, rest @ ..] = args else { return };
        let path = Path::new(file);
        let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();

        let value = match mode.as_str() {
            "DIRECTORY" | "PATH" => path.parent().map(slash).unwrap_or_default(),
            "NAME" => name,
            "EXT" => name.find('.').map(|index| name[index..].to_string()).unwrap_or_default(),
            "NAME_WE" => name.split('.').next().unwrap_or_default().to_string(),
            "LAST_EXT" => name.rfind('.').map(|index| name[index..].to_string()).unwrap_or_default(),
            "NAME_WLE" => name.rfind('.').map(|index| name[..index].to_string()).unwrap_or(name),
            "ABSOLUTE" | "REALPATH" => {
                let base = rest.windows(2).find(|pair| pair[0] == "BASE_DIR").map(|pair| pair[1].clone());
                match base {
                    Some(base) => slash(&util::absolute(&self.resolve_path(&base), path)),
                    None => slash(&self.resolve_path(file)),
                }
            }
            "PROGRAM" => file.clone(),
            _ => String::new(),
        };

        self.set_variable(output, &value);
    }

    /// cmake_path(): the common operations.
    fn command_cmake_path(&mut self, command: &Command, args: &[String]) {
        let [operation, variable, rest @ ..] = args else { return };
        let current = self.get_variable(variable).unwrap_or_default();
        let path = PathBuf::from(&current);
        let output_after = |keyword: &str| rest.windows(2).find(|pair| pair[0] == keyword).map(|pair| pair[1].clone());
        let output = output_after("OUTPUT_VARIABLE").unwrap_or_else(|| variable.clone());

        match operation.as_str() {
            "GET" => {
                let Some(component) = rest.first() else { return };
                let Some(result) = rest.last() else { return };
                let last_only = rest.iter().any(|word| word == "LAST_ONLY");
                let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();

                let value = match component.as_str() {
                    "FILENAME" => name,
                    "EXTENSION" if last_only => path
                        .extension()
                        .map(|extension| format!(".{}", extension.to_string_lossy()))
                        .unwrap_or_default(),
                    "EXTENSION" => name.find('.').map(|index| name[index..].to_string()).unwrap_or_default(),
                    "STEM" if last_only => path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default(),
                    "STEM" => name.split('.').next().unwrap_or_default().to_string(),
                    "PARENT_PATH" => path.parent().map(slash).unwrap_or_default(),
                    "ROOT_PATH" | "ROOT_NAME" | "ROOT_DIRECTORY" => path.ancestors().last().map(slash).unwrap_or_default(),
                    "RELATIVE_PART" => slash(
                        &path
                            .components()
                            .skip_while(|part| !matches!(part, std::path::Component::Normal(_)))
                            .collect::<PathBuf>(),
                    ),
                    _ => String::new(),
                };
                self.set_variable(result, &value);
            }
            "SET" => {
                let Some(input) = rest.first() else { return };
                let normalize = rest.iter().any(|word| word == "NORMALIZE");
                let value = if normalize {
                    slash(&util::normalize(Path::new(input)))
                } else {
                    input.replace('\\', "/")
                };
                self.set_variable(variable, &value);
            }
            "APPEND" => {
                let parts: Vec<&String> = rest.iter().take_while(|word| word.as_str() != "OUTPUT_VARIABLE").collect();
                let joined = parts.iter().fold(path, |joined, part| joined.join(part.as_str()));
                self.set_variable(&output, &slash(&joined));
            }
            "ABSOLUTE_PATH" => {
                let base = output_after("BASE_DIRECTORY")
                    .map(|base| self.resolve_path(&base))
                    .unwrap_or_else(|| self.current_dir());
                self.set_variable(&output, &slash(&util::absolute(&base, &path)));
            }
            "NORMAL_PATH" => self.set_variable(&output, &slash(&util::normalize(&path))),
            "RELATIVE_PATH" => {
                let base = output_after("BASE_DIRECTORY")
                    .map(|base| self.resolve_path(&base))
                    .unwrap_or_else(|| self.current_dir());
                self.set_variable(&output, &util::rel(&path, &base));
            }
            "REMOVE_FILENAME" => self.set_variable(&output, &format!("{}/", path.parent().map(slash).unwrap_or_default())),
            "REPLACE_FILENAME" => {
                let Some(name) = rest.first() else { return };
                self.set_variable(&output, &slash(&path.with_file_name(name)));
            }
            "REMOVE_EXTENSION" => self.set_variable(&output, &slash(&path.with_extension(""))),
            "REPLACE_EXTENSION" => {
                let Some(extension) = rest.first() else { return };
                self.set_variable(&output, &slash(&path.with_extension(extension.trim_start_matches('.'))));
            }
            "IS_ABSOLUTE" | "IS_RELATIVE" | "HAS_FILENAME" | "HAS_STEM" | "HAS_EXTENSION" | "HAS_PARENT_PATH" => {
                let Some(result) = rest.first() else { return };
                let answer = match operation.as_str() {
                    "IS_ABSOLUTE" => path.is_absolute(),
                    "IS_RELATIVE" => !path.is_absolute(),
                    "HAS_FILENAME" | "HAS_STEM" => path.file_name().is_some(),
                    "HAS_EXTENSION" => path.extension().is_some(),
                    _ => path.parent().is_some_and(|parent| !parent.as_os_str().is_empty()),
                };
                self.set_variable(result, if answer { "ON" } else { "OFF" });
            }
            "CONVERT" => {
                // cmake_path(CONVERT input TO_CMAKE_PATH_LIST|TO_NATIVE_PATH_LIST output): `variable` is the input
                let [_, result, ..] = rest else { return };
                self.set_variable(result, &variable.replace('\\', "/"));
            }
            "NATIVE_PATH" => {
                // Forward slashes, as file(TO_NATIVE_PATH) gives: Anvil and Windows both read them
                let Some(result) = rest.iter().find(|word| *word != "NORMALIZE") else {
                    return;
                };
                let value = if rest.iter().any(|word| word == "NORMALIZE") {
                    slash(&util::normalize(&path))
                } else {
                    current.replace('\\', "/")
                };
                self.set_variable(result, &value);
            }
            _ => self.warn(command, &format!("cmake_path({operation}) is not supported")),
        }
    }

    // Arguments

    /// cmake_parse_arguments(prefix options single multi args...) and the PARSE_ARGV form.
    fn command_parse_arguments(&mut self, args: &[String]) {
        let (prefix, lists, input): (&String, &[String], Vec<String>) = if args.first().map(String::as_str) == Some("PARSE_ARGV") {
            if args.len() < 6 {
                return;
            }
            let start: usize = args[1].parse().unwrap_or(0);
            let count: usize = self.get_variable("ARGC").and_then(|count| count.parse().ok()).unwrap_or(0);
            let input = (start..count)
                .map(|index| self.get_variable(&format!("ARGV{index}")).unwrap_or_default())
                .collect();
            (&args[2], &args[3..6], input)
        } else {
            if args.len() < 4 {
                return;
            }

            // The arguments are lists, as `"${ARGN}"` passes them: split each as CMake does
            let input = args[4..].iter().flat_map(|arg| model::split_list(arg)).collect();
            (&args[0], &args[1..4], input)
        };

        let [options, single, multi, ..] = lists else { return };

        let options = model::split_list(options);
        let single = model::split_list(single);
        let multi = model::split_list(multi);
        let option_words: Vec<&str> = options.iter().map(String::as_str).collect();
        let single_words: Vec<&str> = single.iter().map(String::as_str).collect();
        let multi_words: Vec<&str> = multi.iter().map(String::as_str).collect();
        let keywords = Keywords::parse(&input, &option_words, &single_words, &multi_words);

        // Options are TRUE or FALSE; absent keywords are unset
        for option in &options {
            let value = if keywords.has(option) { "TRUE" } else { "FALSE" };
            self.set_variable(&format!("{prefix}_{option}"), value);
        }
        for name in &single {
            match keywords.value(name) {
                Some(value) => self.set_variable(&format!("{prefix}_{name}"), &value),
                None => self.unset_variable(&format!("{prefix}_{name}")),
            }
        }
        for name in &multi {
            match keywords.lists.get(name) {
                Some(_) => self.set_variable(&format!("{prefix}_{name}"), &keywords.list(name).join(";")),
                None => self.unset_variable(&format!("{prefix}_{name}")),
            }
        }

        // What did not belong to a keyword
        let unparsed_name = format!("{prefix}_UNPARSED_ARGUMENTS");
        if keywords.unparsed.is_empty() {
            self.unset_variable(&unparsed_name);
        } else {
            self.set_variable(&unparsed_name, &keywords.unparsed.join(";"));
        }
        let missing_name = format!("{prefix}_KEYWORDS_MISSING_VALUES");
        if keywords.missing_values.is_empty() {
            self.unset_variable(&missing_name);
        } else {
            self.set_variable(&missing_name, &keywords.missing_values.join(";"));
        }
    }

    fn command_separate_arguments(&mut self, args: &[String]) {
        let Some(variable) = args.first() else { return };

        let text = match args.len() {
            1 => self.get_variable(variable).unwrap_or_default().replace(';', " "),
            _ => args.last().cloned().unwrap_or_default(),
        };
        let words = util::split_command(&text);
        self.set_variable(variable, &words.join(";"));
    }

    fn command_message(&mut self, command: &Command, args: &[String]) {
        let (mode, text) = match args.first() {
            Some(mode) if MESSAGE_MODES.contains(&mode.as_str()) => (mode.as_str(), args[1..].concat()),
            _ => ("NOTICE", args.concat()),
        };
        if mode != "FATAL_ERROR" && mode != "SEND_ERROR" {
            return;
        }

        // An error on some platform is worth knowing about, but not a reason to stop converting
        let message = match self.current_condition() {
            Some(condition) => format!("message({mode}) when `{condition}`: {text}"),
            None => format!("message({mode}): {text}"),
        };
        self.warn(command, &message);
    }

    // Files and projects

    /// include(): project files run; CMake's own modules are known to the converter already.
    fn command_include(&mut self, command: &Command, args: &[String]) -> Result<()> {
        let Some(name) = args.first() else { return Ok(()) };
        let optional = args.iter().any(|word| word == "OPTIONAL");
        let result_variable = args.windows(2).find(|pair| pair[0] == "RESULT_VARIABLE").map(|pair| pair[1].clone());

        // CTest turns on testing
        if name == "CTest" {
            if self.get_variable("BUILD_TESTING").is_none() {
                self.set_variable("BUILD_TESTING", "ON");
            }
            return Ok(());
        }

        // A file path, or a module on CMAKE_MODULE_PATH
        let is_path = name.contains('/') || name.contains('\\') || name.ends_with(".cmake");
        let file = if is_path {
            Some(self.resolve_path(name))
        } else {
            model::split_list(&self.get_variable("CMAKE_MODULE_PATH").unwrap_or_default())
                .iter()
                .map(|dir| self.resolve_path(dir).join(format!("{name}.cmake")))
                .find(|candidate| candidate.is_file())
        };

        // CPM.cmake downloads itself; its commands are handled directly
        let is_cpm = name.to_lowercase().contains("cpm");

        match file {
            Some(file) if file.is_file() && !is_cpm => {
                if let Some(variable) = &result_variable {
                    self.set_variable(variable, &slash(&file));
                }
                self.include_file(&file)?;
            }
            Some(_) if is_path && !optional && !is_cpm && !name.starts_with("{out}") => {
                self.warn(command, &format!("include({name}): file not found"));
            }
            _ => {
                if let Some(variable) = &result_variable {
                    self.set_variable(variable, "NOTFOUND");
                }
            }
        }

        Ok(())
    }

    fn command_add_subdirectory(&mut self, command: &Command, args: &[String]) -> Result<()> {
        let Some(source) = args.first() else { return Ok(()) };
        let dir = self.resolve_path(source);

        // A fetched dependency's folder: it becomes a [deps] entry instead
        if source.starts_with("{out}") {
            return Ok(());
        }
        if util::strip_prefix_ci(&dir, &self.root).is_none() {
            self.warn(command, &format!("add_subdirectory({source}) is outside the project; skipped"));
            return Ok(());
        }
        if !dir.join("CMakeLists.txt").is_file() {
            self.warn(command, &format!("add_subdirectory({source}): no CMakeLists.txt there; skipped"));
            return Ok(());
        }

        self.add_subdirectory(&dir)
    }

    fn command_project(&mut self, args: &[String]) {
        let Some((name, rest)) = args.split_first() else { return };
        let keywords = Keywords::parse(rest, &[], &["VERSION", "DESCRIPTION", "HOMEPAGE_URL"], &["LANGUAGES"]);
        let is_top_level = self.project_name.is_none();

        // Names and folders
        let dir = slash(&self.current_dir());
        self.set_variable("PROJECT_NAME", name);
        self.set_variable("PROJECT_SOURCE_DIR", &dir);
        self.set_variable(&format!("{name}_SOURCE_DIR"), &dir);
        self.set_variable(&format!("{name}_BINARY_DIR"), "{out}");
        self.set_variable(&format!("{name}_IS_TOP_LEVEL"), if is_top_level { "ON" } else { "OFF" });
        self.set_variable("PROJECT_IS_TOP_LEVEL", if is_top_level { "ON" } else { "OFF" });

        // Version, split into its parts
        let version = keywords.value("VERSION").unwrap_or_default();
        let parts: Vec<&str> = version.split('.').collect();
        self.set_variable("PROJECT_VERSION", &version);
        self.set_variable(&format!("{name}_VERSION"), &version);
        for (index, suffix) in ["MAJOR", "MINOR", "PATCH", "TWEAK"].iter().enumerate() {
            let part = parts.get(index).copied().unwrap_or_default();
            self.set_variable(&format!("PROJECT_VERSION_{suffix}"), part);
            self.set_variable(&format!("{name}_VERSION_{suffix}"), part);
        }

        // Languages: unparsed words also name languages (project(x CXX))
        let languages: Vec<String> = keywords.list("LANGUAGES").into_iter().chain(keywords.unparsed.iter().cloned()).collect();
        if languages.iter().any(|language| language == "C") {
            self.c_language = true;
        }

        if is_top_level {
            self.set_variable("CMAKE_PROJECT_NAME", name);
            self.project_name = Some(name.clone());
            self.project_version = (!version.is_empty()).then_some(version);
        }
    }

    /// find_package(): remembered, so its imported targets map to dependencies. A REQUIRED package is found, and so is
    /// an optional one Anvil has (Threads, Python) or can add (a known port, one in vcpkg.json, a fetched one); other
    /// optional ones are not, as on a machine without them.
    fn command_find_package(&mut self, args: &[String]) {
        let Some(name) = args.first() else { return };
        let required = args.iter().any(|word| word == "REQUIRED");
        let found = required || self.provides_package(name);

        let found_value = if found { "ON" } else { "OFF" };
        self.set_variable(&format!("{name}_FOUND"), found_value);
        self.set_variable(&format!("{}_FOUND", name.to_uppercase()), found_value);
        if !found {
            return;
        }
        if !self.packages.contains(name) {
            self.packages.push(name.clone());
        }

        // Variables some packages are known for
        match name.as_str() {
            "Python3" | "Python" | "PythonInterp" => {
                self.set_variable(&format!("{name}_EXECUTABLE"), "python");
                self.set_variable("PYTHON_EXECUTABLE", "python");
            }
            "Threads" => self.set_variable("CMAKE_THREAD_LIBS_INIT", "Threads::Threads"),
            "Git" => self.set_variable("GIT_EXECUTABLE", "git"),
            "OpenGL" => {
                for variable in ["OPENGL_LIBRARIES", "OPENGL_LIBRARY", "OPENGL_gl_LIBRARY"] {
                    self.set_variable(variable, "OpenGL::GL");
                }
            }
            "OpenMP" => {
                for language in ["CXX", "C"] {
                    self.set_variable(&format!("OpenMP_{language}_LIBRARIES"), &format!("OpenMP::OpenMP_{language}"));
                    self.set_variable(&format!("OpenMP_{language}_FOUND"), "ON");
                }
            }
            _ if !SYSTEM_PACKAGES.contains(&name.as_str()) => {
                // A find module's libraries are the package's imported target; its include folders, that
                // dependency's headers
                let library = format!("{name}::{name}");
                let headers = format!("{{header-dep:{library}}}");
                for prefix in [name.clone(), name.to_uppercase()] {
                    for (suffix, value) in [
                        ("LIBRARIES", &library),
                        ("LIBRARY", &library),
                        ("INCLUDE_DIRS", &headers),
                        ("INCLUDE_DIR", &headers),
                    ] {
                        let variable = format!("{prefix}_{suffix}");
                        if self.get_variable(&variable).is_none() {
                            self.set_variable(&variable, value);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Is an optional package one Anvil has or adds: a system package, a port it knows, one in the project's
    /// vcpkg.json, or one fetched with FetchContent or CPM?
    fn provides_package(&mut self, name: &str) -> bool {
        let normalized = |text: &str| text.to_lowercase().replace(['-', '_'], "");
        let wanted = normalized(name);

        if self.builtin.manifest_ports.is_none() {
            self.builtin.manifest_ports = Some(manifest_ports(&self.root));
        }
        let in_manifest = self.builtin.manifest_ports.iter().flatten().any(|port| normalized(port) == wanted);
        let known_port = super::emit::KNOWN_PORTS
            .iter()
            .any(|(pattern, _)| normalized(pattern.trim_end_matches("::")) == wanted);
        let fetched = self.fetched.keys().any(|fetched| normalized(fetched) == wanted);

        SYSTEM_PACKAGES.contains(&name) || in_manifest || known_port || fetched
    }

    /// pkg_check_modules(PREFIX [REQUIRED] [IMPORTED_TARGET] modules...): PkgConfig::PREFIX and PREFIX_LIBRARIES.
    fn command_pkg_check_modules(&mut self, args: &[String]) {
        let Some((prefix, rest)) = args.split_first() else { return };
        let flags = [
            "REQUIRED",
            "QUIET",
            "IMPORTED_TARGET",
            "GLOBAL",
            "NO_CMAKE_PATH",
            "NO_CMAKE_ENVIRONMENT_PATH",
        ];
        let modules: Vec<String> = rest.iter().filter(|word| !flags.contains(&word.as_str())).cloned().collect();

        let imported = format!("PkgConfig::{prefix}");
        self.set_variable(&format!("{prefix}_FOUND"), "ON");
        self.set_variable(&format!("{prefix}_LIBRARIES"), &imported);
        self.set_variable(&format!("{prefix}_LINK_LIBRARIES"), &imported);
        self.set_variable(&format!("{prefix}_INCLUDE_DIRS"), "");
        self.pkg_config.insert(prefix.clone(), modules);
    }

    /// find_program/library/path/file: programs and libraries by name; a header's folder is a marker the emitter
    /// turns into the package that has it.
    fn command_find(&mut self, command: &Command, args: &[String]) {
        let Some((variable, rest)) = args.split_first() else { return };
        let existing = self.get_variable(variable).unwrap_or_default();
        if model::constant_truth(&existing) != Some(false) {
            return;
        }

        // The first name: after NAMES, or the first word
        let names_index = rest.iter().position(|word| word == "NAMES").map_or(0, |index| index + 1);
        let Some(first_name) = rest.get(names_index).cloned() else { return };

        match command.name.as_str() {
            "find_program" => {
                // Not on PATH (or in its PATHS and HINTS) here: not found, so steps and tests that need it drop out
                let keywords = Keywords::parse(rest, &[], &[], &["NAMES", "PATHS", "HINTS", "PATH_SUFFIXES", "DOC"]);
                let names = if rest.first().map(String::as_str) == Some("NAMES") {
                    keywords.list("NAMES")
                } else {
                    vec![first_name.clone()]
                };
                let folders: Vec<PathBuf> = keywords.list("PATHS").iter().chain(&keywords.list("HINTS")).map(PathBuf::from).collect();
                let found = names.iter().any(|name| {
                    util::which(name).is_some()
                        || folders.iter().any(|folder| {
                            ["", ".exe", ".bat", ".cmd"]
                                .iter()
                                .any(|extension| folder.join(format!("{name}{extension}")).is_file())
                        })
                });
                let value = if found { first_name } else { format!("{variable}-NOTFOUND") };
                self.set_variable(variable, &value);
            }
            "find_library" => self.set_variable(variable, &first_name),
            _ => self.set_variable(variable, &format!("{{found-header:{first_name}}}")),
        }
    }

    /// configure_file(input output [COPYONLY] [@ONLY] [ESCAPE_QUOTES] ...): a [[write]] of the configured text. Lines
    /// that differ by platform become `#if` blocks in C and C++ files, other differences a write per variant; a large
    /// unchanged copy becomes a copy step. Files only an install uses (pkg-config and package config files) are left
    /// out unless a test or step reads them.
    fn command_configure_file(&mut self, command: &Command, args: &[String]) {
        let [input, output, flags @ ..] = args else { return };
        let has = |flag: &str| flags.iter().any(|word| word == flag);

        // The input in the source folder (or written earlier into the build folder), the output in the build folder
        let input_path = self.resolve_path(input);
        let mut output_path = self.resolve_output(output);
        let output_text = slash(&output_path);
        let names_folder = match output_text.strip_prefix("{out}") {
            Some(rest) => Path::new(&output_text).extension().is_none() && self.root.join(rest.trim_start_matches('/')).is_dir(),
            None => output_path.is_dir(),
        };
        if output.ends_with('/') || names_folder {
            output_path = output_path.join(input_path.file_name().unwrap_or_default());
        }

        let from_build = slash(&input_path).starts_with("{out}");
        let template = if from_build {
            let key = util::path_key(&input_path);
            self.configure_writes
                .iter()
                .rfind(|write| util::path_key(&write.path) == key)
                .and_then(|write| write.text.clone())
        } else {
            std::fs::read_to_string(&input_path).ok()
        };

        // CMake reads the template line by line, without carriage returns
        let template = template.map(|text| text.replace("\r\n", "\n"));
        let Some(template) = template else {
            if from_build {
                self.record_write(command, output_path, None, false);
            } else {
                self.warn(command, &format!("configure_file({input}): no such file; not converted"));
            }
            return;
        };

        // A large unchanged copy of a project file: a copy step, rather than the whole text in anvil.toml
        let variants = if has("COPYONLY") {
            vec![(None, template.clone())]
        } else {
            self.configure_variants(command, input, &template, has("@ONLY"), has("ESCAPE_QUOTES"), is_c_family(&output_path))
        };
        let unchanged = matches!(variants.as_slice(), [(None, text)] if *text == template);
        if unchanged && !from_build && template.lines().count() > LARGE_WRITE_LINES {
            self.add_copy_step(&input_path, &output_path);
            return;
        }

        // A write per variant; an install's files aside
        let first = self.configure_writes.len();
        self.record_variants(command, &output_path, variants);
        if is_install_only(&output_path) {
            let writes: Vec<model::ConfigureWrite> = self.configure_writes.drain(first..).collect();
            self.builtin.install_writes.extend(writes);
        }
    }

    /// A [[write]] of each variant of a configured file, under its condition.
    fn record_variants(&mut self, command: &Command, path: &Path, variants: Vec<(Option<String>, String)>) {
        for (condition, text) in variants {
            self.conditions.extend(condition.clone());
            self.record_write(command, path.to_path_buf(), Some(text), false);
            if condition.is_some() {
                self.conditions.pop();
            }
        }
    }

    /// A configured text under every choice of the conditions it depends on: (the choice's condition, the text).
    /// Conditions a C or C++ file can test with `#if` stay inside the text; past MAX_VARIANT_CONDITIONS others, the
    /// text is this machine's.
    fn configure_variants(
        &mut self,
        command: &Command,
        input: &str,
        template: &str,
        only_at: bool,
        escape_quotes: bool,
        c_family: bool,
    ) -> Vec<(Option<String>, String)> {
        let options = self.option_defaults();
        let on_host = |condition: &str| model::holds_on_host(condition, &|name| options.get(name).copied());

        // The conditions it reads; those on lines #if cannot wrap (a macro's continued lines) vary the whole text
        let mut conditions: Vec<String> = Vec::new();
        let mut varying: Vec<String> = Vec::new();
        let mut continued = false;
        for line in template.split_inclusive('\n') {
            let read = RefCell::new(Vec::new());
            self.configure_line(line, only_at, escape_quotes, &|condition| {
                read.borrow_mut().push(condition.to_string());
                on_host(condition)
            });
            let joined = continued || line.trim_end().ends_with('\\');
            continued = line.trim_end().ends_with('\\');

            for condition in read.into_inner() {
                let inline = c_family && !joined && preprocessor_condition(&condition).is_some();
                if !inline && !varying.contains(&condition) {
                    varying.push(condition.clone());
                }
                if !conditions.contains(&condition) {
                    conditions.push(condition);
                }
            }
        }
        let inline: Vec<String> = conditions.into_iter().filter(|condition| !varying.contains(condition)).collect();

        if varying.len() > MAX_VARIANT_CONDITIONS {
            let mut named: Vec<&str> = varying.iter().take(5).map(String::as_str).collect();
            if varying.len() > named.len() {
                named.push("...");
            }
            self.warn(
                command,
                &format!(
                    "configure_file({input}): the text depends on {} options and platform tests ({}); written as on this machine, with the options' defaults",
                    varying.len(),
                    named.join(", ")
                ),
            );
            varying.clear();
        }

        let texts = texts_by_choice(&varying, &|chosen| {
            self.configured_text(template, only_at, escape_quotes, &inline, &|condition| {
                chosen(condition).unwrap_or_else(|| on_host(condition))
            })
        });
        texts
            .into_iter()
            .filter_map(|(truth, text)| match truth {
                Truth::Known(true) => Some((None, text)),
                Truth::Depends(condition) => Some((Some(condition), text)),
                Truth::Known(false) => None,
            })
            .collect()
    }

    /// A template's text with `choice` deciding each condition, except `inline` ones: lines reading those become
    /// `#if` blocks.
    fn configured_text(&self, template: &str, only_at: bool, escape_quotes: bool, inline: &[String], choice: &dyn Fn(&str) -> bool) -> String {
        let mut text = String::new();

        for line in template.split_inclusive('\n') {
            let read = RefCell::new(Vec::<String>::new());
            let plain = self.configure_line(line, only_at, escape_quotes, &|condition| {
                if inline.iter().any(|known| known == condition) && !read.borrow().iter().any(|known| known == condition) {
                    read.borrow_mut().push(condition.to_string());
                }
                choice(condition)
            });
            let read = read.into_inner();
            if read.is_empty() || read.len() > MAX_VARIANT_CONDITIONS {
                text.push_str(&plain);
                continue;
            }

            let versions = texts_by_choice(&read, &|chosen| {
                self.configure_line(line, only_at, escape_quotes, &|condition| {
                    chosen(condition).unwrap_or_else(|| choice(condition))
                })
            });
            text.push_str(&preprocessor_block(&versions, line).unwrap_or(plain));
        }

        text
    }

    /// One line of a template: `#cmakedefine VAR ...` and `#cmakedefine01 VAR`, then `@VAR@` and `${VAR}` (unless
    /// @ONLY), as CMake configures them.
    fn configure_line(&self, line: &str, only_at: bool, escape_quotes: bool, choice: &dyn Fn(&str) -> bool) -> String {
        let value = |name: &str| self.configure_value(name, choice);
        let body = line.trim_end_matches(['\r', '\n']);
        let ending = &line[body.len()..];

        // #cmakedefine VAR rest: `#define VAR rest`, or `/* #undef VAR */`; #cmakedefine01 VAR: `#define VAR 1` or 0
        let define = regex::Regex::new(r"#([ \t]*)cmakedefine[ \t]+([A-Za-z0-9_]*)").unwrap();
        let define01 = regex::Regex::new(r"#([ \t]*)cmakedefine01[ \t]+([A-Za-z0-9_]*)").unwrap();
        let body = if let Some(captures) = define.captures(body) {
            let indent = &captures[1];
            match value(&captures[2]).0 {
                true => body.replacen(&format!("#{indent}cmakedefine"), &format!("#{indent}define"), 1),
                false => format!("/* #undef {} */", &captures[2]),
            }
        } else if let Some(captures) = define01.captures(body) {
            let indent = &captures[1];
            let replaced = body.replacen(&format!("#{indent}cmakedefine01"), &format!("#{indent}define"), 1);
            format!("{replaced} {}", if value(&captures[2]).0 { 1 } else { 0 })
        } else {
            body.to_string()
        };

        // @VAR@ and ${VAR}
        let substitute = |captures: &regex::Captures| {
            let text = value(&captures[1]).1;
            if escape_quotes {
                text.replace('"', "\\\"")
            } else {
                text
            }
        };
        let at_pattern = regex::Regex::new(r"@([A-Za-z0-9_./+-]+)@").unwrap();
        let mut body = at_pattern.replace_all(&body, substitute).into_owned();
        if !only_at {
            let dollar_pattern = regex::Regex::new(r"\$\{([A-Za-z0-9_./+-]+)\}").unwrap();
            body = dollar_pattern.replace_all(&body, substitute).into_owned();
            if body.contains("$ENV{") || body.contains("${") {
                body = self.expand(&body);
            }
        }

        body + ending
    }

    /// A variable as configure_file reads it: whether it is on, and its text. One that depends on a condition (an
    /// option, a platform, a check's result) takes `choice`'s answer for it.
    fn configure_value(&self, name: &str, choice: &dyn Fn(&str) -> bool) -> (bool, String) {
        let value = self.get_variable(name);
        let truth = match &value {
            Some(_) => self.truthiness(name, false),
            None => self.option_truth(name).unwrap_or(Truth::Known(false)),
        };

        match truth {
            Truth::Depends(condition) => {
                let on = choice(&condition);
                let text = match (self.options.contains_key(name), on) {
                    (true, true) => "ON",
                    (true, false) => "OFF",
                    (false, true) => "1",
                    (false, false) => "",
                };
                (on, text.to_string())
            }
            Truth::Known(on) => (on, self.host_text(&value.unwrap_or_default())),
        }
    }

    /// A build step copying a project file into the build folder (Anvil runs `cmake -E copy` itself).
    fn add_copy_step(&mut self, input: &Path, output: &Path) {
        let condition = self.current_condition();
        let dir = self.current_dir();
        self.custom_commands.push(model::CustomCommand {
            outputs: vec![output.to_path_buf()],
            commands: vec![vec!["cmake".into(), "-E".into(), "copy".into(), slash(input), slash(output)]],
            depends: vec![slash(input)],
            depfile: None,
            workdir: None,
            comment: None,
            condition,
            dir,
            target: None,
            stage: None,
        });
    }

    fn command_execute_process(&mut self, command: &Command, args: &[String]) {
        let keywords = Keywords::parse(
            args,
            &[],
            &["RESULT_VARIABLE", "OUTPUT_VARIABLE", "ERROR_VARIABLE", "WORKING_DIRECTORY"],
            &["COMMAND"],
        );
        let first_command = keywords.groups("COMMAND").first().map(|words| words.join(" ")).unwrap_or_default();
        self.warn(
            command,
            &format!("execute_process({first_command}) does not run during conversion; assumed to fail"),
        );

        if let Some(ran) = self.calls_ran_process.last_mut() {
            *ran = true;
        }
        if let Some(variable) = keywords.value("RESULT_VARIABLE") {
            self.set_variable(&variable, "1");
        }
        for keyword in ["OUTPUT_VARIABLE", "ERROR_VARIABLE"] {
            if let Some(variable) = keywords.value(keyword) {
                self.set_variable(&variable, "");
            }
        }
    }

    /// check_*(): passes where what it tests exists. Flags of another compiler's syntax, POSIX headers and functions,
    /// and GCC extensions pass only where they exist, as a condition (`!msvc`, `!windows`) the result's tests become;
    /// the rest passes everywhere.
    fn command_check(&mut self, command: &Command, args: &[String]) {
        let arg = |index: usize| args.get(index).cloned().unwrap_or_default();
        let names = |index: usize| model::split_list(&arg(index));

        // The result variable, and what decides it
        let (result_variable, truth) = match command.name.as_str() {
            "check_ipo_supported" => (
                args.windows(2).find(|pair| pair[0] == "RESULT").map(|pair| pair[1].clone()),
                Truth::Known(true),
            ),
            "check_c_compiler_flag" | "check_cxx_compiler_flag" => (args.get(1).cloned(), flags_truth(&arg(0))),
            "check_compiler_flag" | "check_linker_flag" => (args.get(2).cloned(), flags_truth(&arg(1))),
            "check_include_file" | "check_include_file_cxx" | "check_include_files" => (args.get(1).cloned(), names_truth(&names(0))),
            "check_symbol_exists" | "check_cxx_symbol_exists" => {
                let mut checked = names(1);
                checked.push(arg(0));
                (args.get(2).cloned(), names_truth(&checked))
            }
            "check_function_exists" => (args.get(1).cloned(), names_truth(&names(0))),
            "check_library_exists" => (args.get(3).cloned(), library_truth(&arg(0)).and(names_truth(&names(1)))),
            "check_struct_has_member" => (args.get(3).cloned(), names_truth(&names(2))),
            "check_type_size" => {
                // check_type_size(type VAR): VAR is its size, HAVE_VAR whether it exists
                let size = TYPE_SIZES.iter().find(|(name, _)| *name == arg(0)).map(|(_, size)| size.to_string());
                if let (Some(variable), Some(size)) = (args.get(1), size) {
                    if self.get_variable(variable).is_none() {
                        self.set_variable(variable, &size);
                    }
                }
                (args.get(1).map(|variable| format!("HAVE_{variable}")), names_truth(&names(0)))
            }
            "check_c_source_compiles" | "check_cxx_source_compiles" | "check_c_source_runs" | "check_cxx_source_runs" => {
                (args.get(1).cloned(), source_truth(&arg(0)))
            }
            "check_source_compiles" | "check_source_runs" => (args.get(2).cloned(), source_truth(&arg(1))),
            _ => {
                self.unknown_commands.insert(command.name.clone());
                return;
            }
        };

        // Flags and libraries the check is made with count too
        let required_flags = self.get_variable("CMAKE_REQUIRED_FLAGS").unwrap_or_default();
        let required_libraries = model::split_list(&self.get_variable("CMAKE_REQUIRED_LIBRARIES").unwrap_or_default());
        let truth = required_libraries.iter().fold(truth.and(flags_truth(&required_flags)), |truth, library| {
            truth.and(library_truth(library))
        });

        let Some(variable) = result_variable else { return };
        if self.get_variable(&variable).is_some() {
            return;
        }
        let value = match truth {
            Truth::Depends(condition) => match model::simplify(&condition, None) {
                Truth::Depends(condition) => model::option_marker(&condition),
                Truth::Known(found) => if found { "1" } else { "" }.to_string(),
            },
            Truth::Known(found) => if found { "1" } else { "" }.to_string(),
        };
        self.set_variable(&variable, &value);
    }

    /// get_property(var TARGET t PROPERTY p) and GLOBAL properties; other scopes read as empty.
    fn command_get_property(&mut self, args: &[String]) {
        let Some(variable) = args.first() else { return };

        if let [_, scope, target, property_keyword, property, ..] = args {
            if scope == "TARGET" && property_keyword == "PROPERTY" {
                let value = self.target_property(target, property);
                self.set_variable(variable, &value.unwrap_or_default());
                return;
            }
        }

        // get_property(var GLOBAL PROPERTY p [SET | DEFINED])
        if let [_, scope, property_keyword, property, rest @ ..] = args {
            if scope == "GLOBAL" && property_keyword == "PROPERTY" {
                let value = self.global_property(property);
                let value = match rest.first().map(String::as_str) {
                    Some("SET" | "DEFINED") => if value.is_some() { "1" } else { "0" }.to_string(),
                    _ => value.unwrap_or_default(),
                };
                self.set_variable(variable, &value);
                return;
            }
        }
        self.set_variable(variable, "");
    }

    /// get_cmake_property(var name): the GLOBAL properties; VARIABLES and the like read as empty.
    fn command_get_cmake_property(&mut self, args: &[String]) {
        let [variable, property, ..] = args else { return };
        let value = self.global_property(property).unwrap_or_default();
        self.set_variable(variable, &value);
    }

    /// set_property(GLOBAL [APPEND | APPEND_STRING] PROPERTY p values...).
    fn command_set_global_property(&mut self, args: &[String]) {
        let keywords = Keywords::parse(&args[1..], &["APPEND", "APPEND_STRING"], &[], &["PROPERTY"]);
        let Some((property, values)) = keywords
            .list("PROPERTY")
            .split_first()
            .map(|(property, values)| (property.clone(), values.to_vec()))
        else {
            return;
        };

        let current = self.builtin.global_properties.get(&property).cloned().unwrap_or_default();
        let value = if keywords.has("APPEND") {
            let mut items = model::split_list(&current);
            items.extend(values);
            items.join(";")
        } else if keywords.has("APPEND_STRING") {
            current + &values.concat()
        } else {
            values.join(";")
        };
        self.builtin.global_properties.insert(property, value);
    }

    /// A GLOBAL property: one the project set, or what CMake's own are in an Anvil build.
    fn global_property(&self, property: &str) -> Option<String> {
        if let Some(value) = self.builtin.global_properties.get(property) {
            return Some(value.clone());
        }

        match property {
            "ENABLED_LANGUAGES" => Some(if self.c_language { "C;CXX" } else { "CXX" }.to_string()),
            "GENERATOR_IS_MULTI_CONFIG" => Some("0".into()),
            "TARGET_SUPPORTS_SHARED_LIBS" => Some("1".into()),
            _ => None,
        }
    }

    /// After the run: tests that build an ExternalProject (`cmake --build ... --target x`) get a TODO, and files only an
    /// install used are written after all when a test or build step reads them.
    pub(super) fn finish_builtins(&mut self) {
        // Targets that stand for an external project: it, and custom targets depending on it
        let mut external = self.builtin.external_projects.clone();
        for command in &self.custom_commands {
            if let Some(target) = &command.target {
                if command.depends.iter().any(|depend| external.contains(depend)) {
                    external.push(target.clone());
                }
            }
        }
        for target in self.targets.values() {
            if target.depends.iter().any(|depend| external.contains(depend)) {
                external.push(target.name.clone());
            }
        }

        let mut flagged = Vec::new();
        for test in &mut self.tests {
            let built = test
                .command
                .windows(2)
                .find(|pair| pair[0] == "--target" && external.contains(&pair[1]))
                .filter(|_| test.command.iter().any(|word| word == "--build"));
            if let Some(pair) = built {
                let note = format!(
                    "builds `{}`, which comes from ExternalProject_Add (not converted): provide it another way",
                    pair[1]
                );
                test.properties.notes.push(note);
                flagged.push(test.name.clone());
            }
        }
        if let Some(first) = flagged.first() {
            self.warnings.push(format!(
                "{} test(s) build an ExternalProject_Add target, which is not converted (first: `{first}`); see the TODO there",
                flagged.len()
            ));
        }

        // Install-only files a test or step reads after all
        let install_writes = std::mem::take(&mut self.builtin.install_writes);
        for write in install_writes {
            let path = slash(&write.path);
            let in_tests = self.tests.iter().any(|test| test.command.iter().any(|word| word.contains(&path)));
            let in_steps = self
                .custom_commands
                .iter()
                .any(|command| command.commands.iter().flatten().chain(&command.depends).any(|word| word.contains(&path)));
            if in_tests || in_steps {
                self.configure_writes.push(write);
            }
        }
    }
}

// configure_file outputs up to this many lines are written into anvil.toml; a larger unchanged copy is a copy step
const LARGE_WRITE_LINES: usize = 200;

// A configured file depending on more conditions than this (outside `#if`) is written as on this machine
const MAX_VARIANT_CONDITIONS: usize = 3;

// Packages Anvil has without adding a dependency
const SYSTEM_PACKAGES: &[&str] = &[
    "Threads",
    "PkgConfig",
    "Python",
    "Python3",
    "PythonInterp",
    "Git",
    "OpenGL",
    "OpenMP",
    "CUDAToolkit",
];

// Condition words as C preprocessor tests
const PREPROCESSOR_TESTS: &[(&str, &str)] = &[
    ("windows", "defined(_WIN32)"),
    ("linux", "defined(__linux__)"),
    ("macos", "defined(__APPLE__)"),
    ("unix", "(defined(__unix__) || defined(__APPLE__))"),
    // rc.exe defines neither _MSC_VER nor __GNUC__ (windres runs GCC's preprocessor)
    ("msvc", "(defined(_MSC_VER) || (defined(RC_INVOKED) && !defined(__GNUC__)))"),
    ("gnu", "!(defined(_MSC_VER) || (defined(RC_INVOKED) && !defined(__GNUC__)))"),
    ("cl", "(defined(_MSC_VER) && !defined(__clang__))"),
    ("clang-cl", "(defined(_MSC_VER) && defined(__clang__))"),
    ("clang", "defined(__clang__)"),
    ("gcc", "(defined(__GNUC__) && !defined(__clang__))"),
    ("mingw", "defined(__MINGW32__)"),
    ("x64", "(defined(_M_X64) || defined(__x86_64__))"),
    ("x86", "(defined(_M_IX86) || defined(__i386__))"),
    ("arm64", "(defined(_M_ARM64) || defined(__aarch64__))"),
    ("arm", "(defined(_M_ARM) || defined(__arm__))"),
];

// Headers, functions and types check_*() finds only where the condition holds; anything else is found everywhere
const PLATFORM_NAMES: &[(&str, &str)] = &[
    // POSIX headers MinGW has too
    ("unistd.h", "!msvc"),
    ("dirent.h", "!msvc"),
    ("sys/time.h", "!msvc"),
    ("sys/param.h", "!msvc"),
    ("strings.h", "!msvc"),
    ("pthread.h", "!msvc"),
    ("sched.h", "!msvc"),
    ("semaphore.h", "!msvc"),
    ("libgen.h", "!msvc"),
    ("getopt.h", "!msvc"),
    // POSIX headers
    ("dlfcn.h", "!windows"),
    ("sys/mman.h", "!windows"),
    ("sys/socket.h", "!windows"),
    ("sys/select.h", "!windows"),
    ("sys/ioctl.h", "!windows"),
    ("sys/wait.h", "!windows"),
    ("sys/resource.h", "!windows"),
    ("sys/uio.h", "!windows"),
    ("sys/un.h", "!windows"),
    ("sys/utsname.h", "!windows"),
    ("sys/random.h", "!windows"),
    ("sys/syscall.h", "!windows"),
    ("sys/file.h", "!windows"),
    ("sys/poll.h", "!windows"),
    ("sys/sysctl.h", "!windows"),
    ("sys/statvfs.h", "!windows"),
    ("netinet/in.h", "!windows"),
    ("netinet/tcp.h", "!windows"),
    ("arpa/inet.h", "!windows"),
    ("netdb.h", "!windows"),
    ("poll.h", "!windows"),
    ("termios.h", "!windows"),
    ("syslog.h", "!windows"),
    ("pwd.h", "!windows"),
    ("grp.h", "!windows"),
    ("langinfo.h", "!windows"),
    ("execinfo.h", "!windows"),
    ("ucontext.h", "!windows"),
    ("spawn.h", "!windows"),
    ("sys/epoll.h", "linux"),
    ("sys/inotify.h", "linux"),
    ("sys/eventfd.h", "linux"),
    ("sys/prctl.h", "linux"),
    ("sys/event.h", "macos"),
    ("mach/mach.h", "macos"),
    // Windows headers
    ("windows.h", "windows"),
    ("io.h", "windows"),
    ("direct.h", "windows"),
    ("process.h", "windows"),
    ("winsock2.h", "windows"),
    ("ws2tcpip.h", "windows"),
    ("intrin.h", "msvc"),
    // POSIX functions and types
    ("fseeko", "!msvc"),
    ("ftello", "!msvc"),
    ("strcasecmp", "!msvc"),
    ("strncasecmp", "!msvc"),
    ("clock_gettime", "!msvc"),
    ("gettimeofday", "!msvc"),
    ("nanosleep", "!msvc"),
    ("usleep", "!msvc"),
    ("pthread_create", "!msvc"),
    ("ssize_t", "!msvc"),
    ("__int128", "!msvc"),
    ("arc4random", "!windows"),
    ("arc4random_buf", "!windows"),
    ("getentropy", "!windows"),
    ("getpagesize", "!windows"),
    ("mmap", "!windows"),
    ("munmap", "!windows"),
    ("dlopen", "!windows"),
    ("posix_memalign", "!windows"),
    ("strerror_r", "!windows"),
    ("strtok_r", "!windows"),
    ("localtime_r", "!windows"),
    ("gmtime_r", "!windows"),
    ("sysconf", "!windows"),
    ("readlink", "!windows"),
    ("realpath", "!windows"),
    ("setenv", "!windows"),
    ("unsetenv", "!windows"),
    ("sigaction", "!windows"),
    ("strndup", "!windows"),
    ("strlcpy", "!windows"),
    ("strlcat", "!windows"),
    ("pthread_setname_np", "!windows"),
    ("u_int32_t", "!windows"),
    ("u_int64_t", "!windows"),
    ("getrandom", "linux"),
    ("SYS_getrandom", "linux"),
    ("memalign", "linux"),
    ("pipe2", "linux"),
    ("getauxval", "linux"),
    ("off64_t", "linux"),
    // Windows functions and types
    ("_fseeki64", "windows"),
    ("_ftelli64", "windows"),
    ("_aligned_malloc", "windows"),
    ("_stricmp", "windows"),
    ("_isatty", "windows"),
    ("_fileno", "windows"),
    ("__int64", "windows"),
];

// Libraries check_library_exists() and CMAKE_REQUIRED_LIBRARIES find only where the condition holds
const PLATFORM_LIBRARIES: &[(&str, &str)] = &[
    ("m", "!windows"),
    ("dl", "!windows"),
    ("rt", "linux"),
    ("pthread", "!msvc"),
    ("socket", "!windows"),
    ("nsl", "linux"),
    ("execinfo", "!windows"),
];

// What check_type_size() reports for a type of the same size everywhere (64-bit targets)
const TYPE_SIZES: &[(&str, &str)] = &[
    ("char", "1"),
    ("short", "2"),
    ("int", "4"),
    ("long long", "8"),
    ("__int64", "8"),
    ("size_t", "8"),
    ("ssize_t", "8"),
    ("ptrdiff_t", "8"),
    ("intptr_t", "8"),
    ("uintptr_t", "8"),
    ("void*", "8"),
    ("void *", "8"),
    ("off64_t", "8"),
    ("int16_t", "2"),
    ("uint16_t", "2"),
    ("int32_t", "4"),
    ("uint32_t", "4"),
    ("int64_t", "8"),
    ("uint64_t", "8"),
    ("float", "4"),
    ("double", "8"),
];

// Macros a check's source tests to pick a platform's code: then its names say nothing about where it compiles
const PLATFORM_MACROS: &[&str] = &["_WIN32", "_MSC_VER", "WIN32", "__GNUC__", "__clang__", "__MINGW32__"];

/// Where compiler or linker flags work: another compiler's syntax (`/W4` for cl, `-Wextra` for the rest) only there.
fn flags_truth(flags: &str) -> Truth {
    flags.split_whitespace().fold(Truth::Known(true), |truth, flag| {
        let flag = flag.strip_prefix("LINKER:").unwrap_or(flag);
        let universal = ["-D", "-I", "-U"].iter().any(|prefix| flag.starts_with(prefix));
        let single = if flag.starts_with('/') {
            Truth::Depends("msvc".into())
        } else if flag.starts_with('-') && !universal {
            Truth::Depends("!msvc".into())
        } else {
            Truth::Known(true)
        };
        truth.and(single)
    })
}

/// Where headers, functions and types exist, from PLATFORM_NAMES.
fn names_truth(names: &[String]) -> Truth {
    names.iter().fold(Truth::Known(true), |truth, name| {
        let name = name.trim();
        let condition = PLATFORM_NAMES
            .iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(name))
            .map(|(_, condition)| *condition)
            .or_else(|| name.starts_with("linux/").then_some("linux"));
        match condition {
            Some(condition) => truth.and(Truth::Depends(condition.into())),
            None => truth,
        }
    })
}

/// Where a library exists, from PLATFORM_LIBRARIES.
fn library_truth(library: &str) -> Truth {
    let name = library.trim().trim_start_matches("-l").to_lowercase();
    match PLATFORM_LIBRARIES.iter().find(|(known, _)| *known == name) {
        Some((_, condition)) => Truth::Depends(condition.to_string()),
        None => Truth::Known(true),
    }
}

/// Where a check's source compiles: GCC extensions not with cl, and the platform headers and functions it uses
/// where they exist (unless it picks its code by platform macros).
fn source_truth(source: &str) -> Truth {
    let mut truth = Truth::Known(true);
    if ["__attribute__", "__builtin_", "__asm__", "__typeof__"]
        .iter()
        .any(|extension| source.contains(extension))
    {
        truth = Truth::Depends("!msvc".into());
    }
    if PLATFORM_MACROS.iter().any(|macro_name| source.contains(macro_name)) {
        return truth;
    }

    let include = regex::Regex::new(r#"#\s*include\s*[<"]([^>"]+)[>"]"#).unwrap();
    let word = regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").unwrap();
    let mut names: Vec<String> = include.captures_iter(source).map(|captures| captures[1].to_string()).collect();
    names.extend(word.find_iter(source).map(|found| found.as_str().to_string()));
    names.retain(|name| PLATFORM_NAMES.iter().any(|(known, _)| known == name) || name.starts_with("linux/"));
    names.sort();
    names.dedup();

    truth.and(names_truth(&names))
}

/// What `render` gives under every choice of `conditions` (each true or false), the choices with the same text
/// merged into one condition; choices that cannot happen (`windows` with `unix`) are left out.
fn texts_by_choice(conditions: &[String], render: &dyn Fn(&dyn Fn(&str) -> Option<bool>) -> String) -> Vec<(Truth, String)> {
    let simplified = |truth: Truth| match truth {
        Truth::Depends(condition) => model::simplify(&condition, None),
        known => known,
    };
    let mut texts: Vec<(Truth, String)> = Vec::new();

    for assignment in 0u32..(1 << conditions.len()) {
        let chosen = |condition: &str| {
            conditions
                .iter()
                .position(|known| known == condition)
                .map(|index| assignment & (1 << index) != 0)
        };
        let truth = conditions.iter().fold(Truth::Known(true), |truth, condition| {
            let single = Truth::Depends(condition.clone());
            truth.and(if chosen(condition) == Some(true) { single } else { single.not() })
        });
        let truth = simplified(truth);
        if truth == Truth::Known(false) {
            continue;
        }

        let text = render(&chosen);
        match texts.iter_mut().find(|(_, known)| *known == text) {
            Some((merged, _)) => *merged = merged.clone().or(truth),
            None => texts.push((truth, text)),
        }
    }

    texts.into_iter().map(|(truth, text)| (simplified(truth), text)).collect()
}

/// Versions of a template line as `#if` blocks, the last under `#else`; None when a condition has no preprocessor
/// test.
fn preprocessor_block(versions: &[(Truth, String)], line: &str) -> Option<String> {
    if let [(_, text)] = versions {
        return Some(text.clone());
    }

    let ending = if line.ends_with("\r\n") { "\r\n" } else { "\n" };
    let mut block = String::new();
    for (index, (truth, text)) in versions.iter().enumerate() {
        if index + 1 == versions.len() {
            block.push_str("#else");
        } else {
            let Truth::Depends(condition) = truth else { return None };
            let keyword = if index == 0 { "#if" } else { "#elif" };
            block.push_str(&format!("{keyword} {}", preprocessor_condition(condition)?));
        }
        block.push_str(ending);
        block.push_str(text.trim_end_matches(['\r', '\n']));
        block.push_str(ending);
    }
    block.push_str("#endif");
    if line.ends_with('\n') {
        block.push_str(ending);
    }

    Some(block)
}

/// A condition as a C preprocessor test, when it names only platforms, compilers and processors.
fn preprocessor_condition(condition: &str) -> Option<String> {
    fn translate(condition: &crate::cond::Cond) -> Option<String> {
        use crate::cond::Cond;

        match condition {
            Cond::Atom(atom) => PREPROCESSOR_TESTS.iter().find(|(name, _)| name == atom).map(|(_, test)| test.to_string()),
            Cond::Not(inner) => {
                let test = translate(inner)?;
                let single = test.starts_with("defined(") && !test.contains(' ');
                Some(if single { format!("!{test}") } else { format!("!({test})") })
            }
            Cond::And(left, right) => Some(format!("({} && {})", translate(left)?, translate(right)?)),
            Cond::Or(left, right) => Some(format!("({} || {})", translate(left)?, translate(right)?)),
        }
    }

    translate(&crate::cond::parse(condition).ok()?)
}

/// Can a file hold `#if` blocks (a C or C++ source or header)?
fn is_c_family(path: &Path) -> bool {
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    ["h", "hh", "hpp", "hxx", "inl", "ipp", "c", "cc", "cpp", "cxx"].contains(&extension.as_str())
}

/// Is a configured file one only an install or an IDE uses (pkg-config files, package config files, uninstall
/// scripts)?
fn is_install_only(path: &Path) -> bool {
    let name = path.file_name().map(|name| name.to_string_lossy().to_lowercase()).unwrap_or_default();
    let package_file = name.ends_with(".cmake") && (name.contains("config") || name.contains("targets") || name.contains("uninstall"));

    package_file || name.ends_with(".pc") || name.ends_with(".pc.gen") || name.ends_with("-config") || name.ends_with(".vcxproj.user")
}

/// The ports a project's vcpkg.json lists.
fn manifest_ports(root: &Path) -> Vec<String> {
    let manifest = std::fs::read_to_string(root.join("vcpkg.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    let dependencies = manifest
        .as_ref()
        .and_then(|manifest| manifest.get("dependencies"))
        .and_then(|dependencies| dependencies.as_array())
        .cloned()
        .unwrap_or_default();

    dependencies
        .iter()
        .filter_map(|dependency| {
            dependency
                .as_str()
                .or_else(|| dependency.get("name").and_then(|name| name.as_str()))
                .map(String::from)
        })
        .collect()
}

/// A list index, negative from the end, checked against the length.
fn list_index(text: &str, length: usize) -> Option<usize> {
    let index: i64 = text.parse().ok()?;
    let resolved = if index < 0 { length as i64 + index } else { index };

    (0..length as i64).contains(&resolved).then_some(resolved as usize)
}

/// A CMake regex replacement (`\1`) in the regex crate's syntax (`${1}`).
fn cmake_replacement(replacement: &str) -> String {
    let escaped = replacement.replace('$', "$$");
    let group = regex::Regex::new(r"\\(\d)").unwrap();

    group.replace_all(&escaped, "$${$1}").into_owned()
}

/// Removes generator expressions from text.
fn strip_genex(text: &str) -> String {
    let mut output = String::new();
    let mut depth = 0;
    let mut characters = text.chars().peekable();

    while let Some(character) = characters.next() {
        if character == '$' && characters.peek() == Some(&'<') {
            characters.next();
            depth += 1;
            continue;
        }
        if character == '>' && depth > 0 {
            depth -= 1;
            continue;
        }
        if depth == 0 {
            output.push(character);
        }
    }

    output
}

/// The folder part of a glob pattern before its first wildcard.
fn glob_base(pattern: &str) -> String {
    let wildcard = pattern.find(['*', '?', '[']).unwrap_or(pattern.len());

    match pattern[..wildcard].rfind('/') {
        Some(slash_index) => pattern[..slash_index].to_string(),
        None => ".".into(),
    }
}

/// A CMake glob as a globset pattern: GLOB_RECURSE matches the last part in any subfolder.
fn glob_pattern(absolute: &str, recursive: bool) -> String {
    if !recursive {
        return absolute.to_string();
    }

    let base = glob_base(absolute);
    let rest = absolute[base.len()..].trim_start_matches('/');
    match rest.rsplit_once('/') {
        Some((folders, name)) => format!("{base}/{folders}/**/{name}"),
        None => format!("{base}/**/{rest}"),
    }
}

/// math(EXPR): integer arithmetic with CMake's operators.
fn evaluate_math(expression: &str) -> Option<i64> {
    let tokens = math_tokens(expression)?;
    let mut position = 0;
    let value = math_binary(&tokens, &mut position, 0)?;

    (position == tokens.len()).then_some(value)
}

// Operator precedence for math(EXPR), loosest first
const MATH_PRECEDENCE: &[&[&str]] = &[&["|"], &["^"], &["&"], &["<<", ">>"], &["+", "-"], &["*", "/", "%"]];

fn math_tokens(expression: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let characters: Vec<char> = expression.chars().collect();
    let mut position = 0;

    while position < characters.len() {
        let character = characters[position];
        if character.is_whitespace() {
            position += 1;
            continue;
        }

        // Numbers, decimal or 0x hexadecimal
        if character.is_ascii_digit() {
            let start = position;
            while position < characters.len() && characters[position].is_ascii_alphanumeric() {
                position += 1;
            }
            tokens.push(characters[start..position].iter().collect());
            continue;
        }

        let pair: String = characters[position..(position + 2).min(characters.len())].iter().collect();
        if pair == "<<" || pair == ">>" {
            tokens.push(pair);
            position += 2;
            continue;
        }
        if "+-*/%&|^~()".contains(character) {
            tokens.push(character.to_string());
            position += 1;
            continue;
        }
        return None;
    }

    Some(tokens)
}

fn math_binary(tokens: &[String], position: &mut usize, level: usize) -> Option<i64> {
    if level == MATH_PRECEDENCE.len() {
        return math_unary(tokens, position);
    }

    let mut value = math_binary(tokens, position, level + 1)?;
    while let Some(operator) = tokens
        .get(*position)
        .filter(|token| MATH_PRECEDENCE[level].contains(&token.as_str()))
        .cloned()
    {
        *position += 1;
        let right = math_binary(tokens, position, level + 1)?;
        value = match operator.as_str() {
            "|" => value | right,
            "^" => value ^ right,
            "&" => value & right,
            "<<" => value.checked_shl(right as u32)?,
            ">>" => value.checked_shr(right as u32)?,
            "+" => value.wrapping_add(right),
            "-" => value.wrapping_sub(right),
            "*" => value.wrapping_mul(right),
            "/" => value.checked_div(right)?,
            "%" => value.checked_rem(right)?,
            _ => return None,
        };
    }

    Some(value)
}

fn math_unary(tokens: &[String], position: &mut usize) -> Option<i64> {
    let token = tokens.get(*position)?.clone();
    *position += 1;

    match token.as_str() {
        "-" => math_unary(tokens, position).map(|value| -value),
        "+" => math_unary(tokens, position),
        "~" => math_unary(tokens, position).map(|value| !value),
        "(" => {
            let value = math_binary(tokens, position, 0)?;
            if tokens.get(*position).map(String::as_str) != Some(")") {
                return None;
            }
            *position += 1;
            Some(value)
        }
        number => match number.strip_prefix("0x").or_else(|| number.strip_prefix("0X")) {
            Some(hex) => i64::from_str_radix(hex, 16).ok(),
            None => number.parse().ok(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn math() {
        assert_eq!(evaluate_math("1 + 2 * 3"), Some(7));
        assert_eq!(evaluate_math("(1 + 2) * 3"), Some(9));
        assert_eq!(evaluate_math("0x10 >> 2 | 1"), Some(5));
        assert_eq!(evaluate_math("-4 / 2"), Some(-2));
        assert_eq!(evaluate_math("1 +"), None);
    }

    #[test]
    fn keywords() {
        let args: Vec<String> = ["a", "STATIC", "NAME", "x", "SOURCES", "b.cpp", "c.cpp", "SOURCES", "d.cpp"]
            .iter()
            .map(|word| word.to_string())
            .collect();
        let keywords = Keywords::parse(&args, &["STATIC"], &["NAME"], &["SOURCES"]);

        assert!(keywords.has("STATIC"));
        assert_eq!(keywords.value("NAME").unwrap(), "x");
        assert_eq!(keywords.list("SOURCES"), vec!["b.cpp", "c.cpp", "d.cpp"]);
        assert_eq!(keywords.groups("SOURCES").len(), 2);
        assert_eq!(keywords.unparsed, vec!["a"]);
    }

    #[test]
    fn globs() {
        assert_eq!(glob_base("C:/p/src/*.cpp"), "C:/p/src");
        assert_eq!(glob_pattern("C:/p/src/*.cpp", true), "C:/p/src/**/*.cpp");
        assert_eq!(glob_pattern("C:/p/src/*.cpp", false), "C:/p/src/*.cpp");
    }

    #[test]
    fn replacement() {
        let pattern = regex::Regex::new(r"([a-z]+)-([0-9]+)").unwrap();
        assert_eq!(pattern.replace_all("abc-12", cmake_replacement(r"\2_\1$").as_str()), "12_abc$");
    }
}

// CMAKE_<LANG>_COMPILER_ID values: hashed text holding one hashes the compiler
const COMPILER_IDS: &[&str] = &["MSVC", "GNU", "Clang", "AppleClang", "IntelLLVM", "Intel", "NVIDIA"];

/// The files in the `{hash:a,b}` markers of a text, in order and once each. Commas, not semicolons: CMake would split
/// an unquoted value at a semicolon.
fn hashed_files(text: &str) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{hash:") {
        let after = &rest[start + "{hash:".len()..];
        let Some(end) = after.find('}') else { break };
        for file in after[..end].split(',').filter(|file| !file.is_empty()) {
            if !files.iter().any(|known| known == file) {
                files.push(file.to_string());
            }
        }
        rest = &after[end..];
    }
    files
}
