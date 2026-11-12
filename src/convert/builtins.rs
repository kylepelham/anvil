//! CMake's built-in commands for variables, lists, strings, paths and files, plus the dispatcher that routes every
//! command to its handler. Target, test and dependency commands live in `target_commands.rs`.

use super::interp::{Flow, Interpreter};
use super::model::{self, Truth};
use super::parse::Command;
use super::target_commands::TargetList;
use crate::util::{self, slash};
use anyhow::Result;
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
    "check_ipo_supported",
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

impl Interpreter {
    /// Runs one command: a user function, a built-in, or (when unknown) records it.
    pub(super) fn run_command(&mut self, command: &Command) -> Result<Flow> {
        let name = command.name.as_str();

        // User functions and macros, unless the converter handles the name itself
        if !INTERCEPTED_COMMANDS.contains(&name) {
            if let Some(function) = self.functions.get(name).cloned() {
                let args = self.evaluate_args(&command.args);
                self.call_function(function, args)?;
                return Ok(Flow::Normal);
            }
        }
        if IGNORED_COMMANDS.contains(&name) {
            return Ok(Flow::Normal);
        }

        let args = self.evaluate_args(&command.args);
        match name {
            // Control flow
            "return" => return Ok(Flow::Return(Truth::Known(true))),
            "break" => return Ok(Flow::Break),
            "continue" => return Ok(Flow::Continue),
            "block" => self.scopes.push(HashMap::new()),
            "endblock" => {
                if self.scopes.len() > 1 {
                    self.scopes.pop();
                }
            }

            // Variables and values
            "set" => self.command_set(&args),
            "unset" => self.command_unset(&args),
            "option" | "cmake_dependent_option" => self.command_option(&args),
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
            "get_directory_property" | "get_cmake_property" | "get_source_file_property" => {
                if let Some(variable) = args.first() {
                    self.set_variable(variable, "");
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
            "externalproject_add" => self.warn(command, "ExternalProject_Add is not converted; add the dependency to [deps] by hand"),

            _ => {
                self.unknown_commands.insert(command.name.clone());
            }
        }

        Ok(Flow::Normal)
    }

    // Variables

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
            let force = rest.iter().any(|word| word == "FORCE");

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
                [] => self.unset_in(self.scopes.len().saturating_sub(2), name),
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

    // Lists

    fn command_list(&mut self, command: &Command, args: &[String]) {
        let [operation, variable, rest @ ..] = args else { return };
        let mut items = model::split_list(&self.get_variable(variable).unwrap_or_default());

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
                        self.set_match_variables(&captures);
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

    fn set_match_variables(&mut self, captures: &regex::Captures) {
        for index in 0..10 {
            let value = captures.get(index).map(|found| found.as_str().to_string()).unwrap_or_default();
            self.set_variable(&format!("CMAKE_MATCH_{index}"), &value);
        }
        self.set_variable("CMAKE_MATCH_COUNT", &(captures.len() - 1).to_string());
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
        match regex::Regex::new(pattern) {
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
                let [file, output, ..] = rest else { return };
                let text = std::fs::read_to_string(self.resolve_path(file)).unwrap_or_default();
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
                self.record_write(command, path, content.filter(|_| known), false);
            }
            "DOWNLOAD" | "UPLOAD" => self.warn(command, &format!("file({operation}) is not converted")),

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
            "IS_ABSOLUTE" | "HAS_FILENAME" | "HAS_EXTENSION" | "HAS_PARENT_PATH" => {
                let Some(result) = rest.first() else { return };
                let answer = match operation.as_str() {
                    "IS_ABSOLUTE" => path.is_absolute(),
                    "HAS_FILENAME" => path.file_name().is_some(),
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
            (&args[0], &args[1..4], args[4..].to_vec())
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

    /// find_package(): remembered, so its imported targets map to dependencies; always found.
    fn command_find_package(&mut self, args: &[String]) {
        let Some(name) = args.first() else { return };
        if !self.packages.contains(name) {
            self.packages.push(name.clone());
        }

        self.set_variable(&format!("{name}_FOUND"), "ON");
        self.set_variable(&format!("{}_FOUND", name.to_uppercase()), "ON");

        // Variables some packages are known for
        match name.as_str() {
            "Python3" | "Python" | "PythonInterp" => {
                self.set_variable(&format!("{name}_EXECUTABLE"), "python");
                self.set_variable("PYTHON_EXECUTABLE", "python");
            }
            "Threads" => self.set_variable("CMAKE_THREAD_LIBS_INIT", "Threads::Threads"),
            "Git" => self.set_variable("GIT_EXECUTABLE", "git"),
            _ => {}
        }
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
            "find_program" | "find_library" => self.set_variable(variable, &first_name),
            _ => self.set_variable(variable, &format!("{{found-header:{first_name}}}")),
        }
    }

    fn command_configure_file(&mut self, command: &Command, args: &[String]) {
        let [input, output, ..] = args else { return };
        self.warn(
            command,
            &format!("configure_file({input} -> {output}) is not converted; check in the generated file or generate it with a run step"),
        );
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

    /// check_*(): assumed to pass; the compiler Anvil picks supports modern flags and headers.
    fn command_check(&mut self, command: &Command, args: &[String]) {
        let result_variable = match command.name.as_str() {
            "check_ipo_supported" => args.windows(2).find(|pair| pair[0] == "RESULT").map(|pair| pair[1].clone()),
            "check_include_file" | "check_include_file_cxx" | "check_include_files" | "check_cxx_compiler_flag" | "check_c_compiler_flag" => {
                args.get(1).cloned()
            }
            "check_compiler_flag" | "check_linker_flag" | "check_symbol_exists" | "check_cxx_symbol_exists" | "check_library_exists" => {
                args.get(2).cloned()
            }
            "check_function_exists" | "check_type_size" | "check_cxx_source_compiles" | "check_c_source_compiles" => args.get(1).cloned(),
            "check_source_compiles" => args.get(2).cloned(),
            _ => {
                self.unknown_commands.insert(command.name.clone());
                return;
            }
        };

        if let Some(variable) = result_variable {
            if self.get_variable(&variable).is_none() {
                self.set_variable(&variable, "1");
            }
        }
    }

    /// get_property(var TARGET t PROPERTY p); other scopes read as empty.
    fn command_get_property(&mut self, args: &[String]) {
        let Some(variable) = args.first() else { return };

        if let [_, scope, target, property_keyword, property, ..] = args {
            if scope == "TARGET" && property_keyword == "PROPERTY" {
                let value = self.target_property(target, property);
                self.set_variable(variable, &value.unwrap_or_default());
                return;
            }
        }
        self.set_variable(variable, "");
    }
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
