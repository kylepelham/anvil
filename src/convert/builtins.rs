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
use std::sync::LazyLock;

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
    HANDLED_COMMANDS.contains(&name)
        || IGNORED_COMMANDS.contains(&name)
        || INTERCEPTED_COMMANDS.contains(&name)
        || super::qt_commands::is_qt_command(name)
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
    /// Options that exist only on some platforms or under other options: declared inside a branch, or
    /// cmake_dependent_option()s whose dependencies hold only there. The option reads as `OPT & (these)`.
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
    /// The vcpkg checkout optional find_package()s can take ports from, found when one first needs it.
    pub vcpkg: Option<Option<crate::vcpkg::Instance>>,
    /// Folders find_program() searches besides PATH (the MSVC tools'), found when a program is first not on PATH.
    pub tool_folders: Option<Vec<PathBuf>>,
    /// The `<Package>_FOUND` options optional find_package()s of vcpkg ports became.
    pub package_options: Vec<String>,
}

/// Where an optional find_package() finds its package.
enum PackageSource {
    /// Anvil or the project has it: a system package, one in vcpkg.json, one fetched.
    Project,
    /// A vcpkg port Anvil adds where the package's option is on.
    Port(String),
    /// Nowhere, and why.
    Absent(String),
}

/// A type's size by platform: `otherwise` unless one of the `cases` conditions holds (the first that does). An empty
/// size: the type does not exist there.
#[derive(Debug, Clone)]
pub struct TypeSize {
    pub cases: Vec<(String, String)>,
    pub otherwise: String,
}

/// Reads a list (list(GET), list(SUBLIST), ...): the variable it sets and its value.
type ListRead<'a> = dyn Fn(&[String]) -> Option<(String, String)> + 'a;

/// Renders a text given whether each condition is chosen (None: not one of the choices).
type ChoiceRender<'a> = dyn Fn(&dyn Fn(&str) -> Option<bool>) -> String + 'a;

impl Interpreter {
    /// Runs one command: a user function, a built-in, or (when unknown) records it.
    pub(super) fn run_command(&mut self, command: &Command) -> Result<Flow> {
        let name = command.name.as_str();

        // User functions and macros, unless the converter handles the name itself
        if !INTERCEPTED_COMMANDS.contains(&name) {
            if let Some(function) = self.functions.get(name).cloned() {
                let args = self.evaluate_call_args(&command.args);
                return self.call_function(function, args);
            }
        }
        if IGNORED_COMMANDS.contains(&name) {
            return Ok(Flow::Normal);
        }

        let mut args = self.command_args(command);

        // Commands that read their arguments as one list: a generator expression the parser cut at whitespace is one
        // item again
        if name.starts_with("target_") || LIST_COMMANDS.contains(&name) {
            args = model::join_genex_fragments(args);
        }

        // Targets named under a condition (from a list a branch added to): the command for each, under its condition
        if let Some(runs) = super::interp::conditional_target_runs(name, &args) {
            for (condition, args) in runs {
                self.conditions.extend(condition.clone());
                let literal = Command {
                    args: args
                        .into_iter()
                        .map(|text| Arg {
                            text,
                            kind: ArgKind::Bracket,
                        })
                        .collect(),
                    ..command.clone()
                };
                let flow = self.run_command(&literal);
                if condition.is_some() {
                    self.conditions.pop();
                }
                flow?;
            }
            return Ok(Flow::Normal);
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
            "find_package" => self.command_find_package(command, &args),
            "pkg_check_modules" | "pkg_search_module" => self.command_pkg_check_modules(command, &args),
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
            qt if super::qt_commands::is_qt_command(qt) => self.command_qt(command, &args),

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

            if kind == "BOOL" && (self.get_variable(name).is_none() || self.options.contains_key(name)) {
                let description = rest.get(1).cloned().unwrap_or_default();
                self.declare_option(name, model::constant_truth(&value).unwrap_or(false), description);
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

    /// option(NAME "doc" default): becomes an Anvil option, and tests of it become conditions. One declared inside a
    /// branch exists only where the branch runs: it reads as `NAME & (branch condition)`, and declared again in
    /// another branch, as `NAME & (either condition)`.
    fn command_option(&mut self, args: &[String]) {
        let Some(name) = args.first() else { return };
        let branch = self.current_condition();
        let default = args.get(2).map(|value| model::constant_truth(value).unwrap_or(false)).unwrap_or(false);
        let description = args.get(1).cloned().unwrap_or_default();
        if self.options.contains_key(name) {
            self.declare_option(name, default, description);
            self.widen_option_condition(name, branch);
            return;
        }
        if self.get_variable(name).is_some() {
            return;
        }

        self.declare_option(name, default, description);
        self.cache.insert(name.clone(), if default { "ON" } else { "OFF" }.into());
        if let Some(branch) = branch {
            self.builtin.option_conditions.insert(name.clone(), branch);
        }
    }

    /// An option declared (option() or a BOOL cache entry) under the current branch. Declared again with another
    /// default, it takes the first that holds on this machine (as CMake keeps the first value in the cache), and says
    /// so: an Anvil option has one default.
    fn declare_option(&mut self, name: &str, default: bool, description: String) {
        let branch = self.current_condition();
        let on_host = branch.as_deref().is_none_or(|branch| {
            let options = self.option_defaults();
            model::holds_on_host(branch, &|name| options.get(name).copied())
        });

        let (declarations, host_set) = self.option_declarations.entry(name.to_string()).or_default();
        declarations.push((branch, default));
        let differs = declarations.iter().any(|(_, other)| *other != default);
        let sets_default = !*host_set && on_host;
        *host_set |= on_host;
        let described = declarations
            .iter()
            .map(|(branch, default)| {
                let value = if *default { "ON" } else { "OFF" };
                match branch {
                    Some(branch) => format!("{value} where `{branch}`"),
                    None => format!("{value} elsewhere"),
                }
            })
            .collect::<Vec<_>>()
            .join(", ");

        match self.options.get_mut(name) {
            Some((current, _)) if sets_default => *current = default,
            Some(_) => {}
            None => {
                self.options.insert(name.to_string(), (default, description));
            }
        }
        if differs {
            let chosen = if self.options[name].0 { "ON" } else { "OFF" };
            self.command_warnings.borrow_mut().push(format!(
                "option {name} has a default per condition ({described}); an Anvil option has one default, so it is this machine's ({chosen})"
            ));
        }
    }

    /// An option declared again under `branch`: it exists where it did before or where the branch runs.
    fn widen_option_condition(&mut self, name: &str, branch: Option<String>) {
        let Some(earlier) = self.builtin.option_conditions.get(name).cloned() else {
            return;
        };
        let widened = match branch {
            Some(branch) => Truth::Depends(earlier).or(Truth::Depends(branch)),
            None => Truth::Known(true),
        };
        let widened = match widened {
            Truth::Depends(condition) => model::simplify(&condition, None),
            known => known,
        };
        match widened {
            Truth::Depends(condition) => {
                self.builtin.option_conditions.insert(name.to_string(), condition);
            }
            Truth::Known(_) => {
                self.builtin.option_conditions.remove(name);
            }
        }
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
                let condition = match self.builtin.option_conditions.get(name.as_str()) {
                    Some(branch) => model::and(branch, &condition),
                    None => condition,
                };
                self.builtin.option_conditions.insert(name.clone(), condition);
            }
        }
    }

    // Lists

    /// list(): items a branch this machine does not take added (conditional items) keep their place; operations that
    /// count or index the list see it as this machine does with the options' defaults, and say so when options change
    /// the answer.
    fn command_list(&mut self, command: &Command, args: &[String]) {
        let [operation, variable, rest @ ..] = args else { return };
        let mut items = model::list_elements(&self.get_variable(variable).unwrap_or_default());
        let conditional = items.iter().any(|item| model::split_conditional_item(item).is_some());
        let options = if conditional { self.option_defaults() } else { HashMap::new() };
        let visible = visible_items(&items, &options);

        match operation.as_str() {
            "LENGTH" | "GET" | "JOIN" | "SUBLIST" | "FIND" => {
                let Some((output, value)) = list_read(operation, &resolved_items(&items, &options), rest) else {
                    return;
                };
                if let Some(changed_by) = self.options_changing(&items, &|items| list_read(operation, items, rest)) {
                    self.warn(
                        command,
                        &format!(
                            "list({operation} {variable}) reads items that options add ({changed_by}); the result is the one with the options' defaults"
                        ),
                    );
                } else if let Some(changed_by) = platforms_changing(&items, &|items| list_read(operation, items, rest)) {
                    self.warn(
                        command,
                        &format!("list({operation} {variable}) reads items under platform conditions ({changed_by}); the result is this machine's: `{value}`"),
                    );
                }
                self.set_variable(&output, &value);
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
                let index = list_index(index, visible.len() + 1).unwrap_or(visible.len());
                let position = visible.get(index).copied().unwrap_or(items.len());
                items.splice(position..position, values.iter().cloned());
                self.set_variable(variable, &items.join(";"));
            }
            "POP_BACK" | "POP_FRONT" => {
                let count = rest.len().max(1).min(visible.len());
                let taken: Vec<usize> = if operation == "POP_BACK" {
                    visible.iter().rev().take(count).copied().collect()
                } else {
                    visible.iter().take(count).copied().collect()
                };
                for (output, index) in rest.iter().zip(&taken) {
                    let value = item_text(&items[*index]).to_string();
                    self.set_variable(output, &value);
                }
                let kept: Vec<String> = items
                    .into_iter()
                    .enumerate()
                    .filter(|(index, _)| !taken.contains(index))
                    .map(|(_, item)| item)
                    .collect();
                self.set_variable(variable, &kept.join(";"));
            }
            "REMOVE_ITEM" => {
                items.retain(|item| !rest.iter().any(|removed| removed == item_text(item)));
                self.set_variable(variable, &items.join(";"));
            }
            "REMOVE_AT" => {
                let removed: HashSet<usize> = rest
                    .iter()
                    .filter_map(|index| list_index(index, visible.len()))
                    .map(|index| visible[index])
                    .collect();
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
                    items.sort_by_key(|item| item_text(item).to_lowercase());
                } else {
                    items.sort_by(|left, right| item_text(left).cmp(item_text(right)));
                }
                if rest.windows(2).any(|pair| pair[0] == "ORDER" && pair[1] == "DESCENDING") {
                    items.reverse();
                }
                self.set_variable(variable, &items.join(";"));
            }
            "FILTER" => {
                let [mode, _, pattern, ..] = rest else { return };
                let Some(pattern) = self.compile_regex(command, pattern) else { return };
                items.retain(|item| pattern.is_match(item_text(item)) == (mode == "INCLUDE"));
                self.set_variable(variable, &items.join(";"));
            }
            "TRANSFORM" => self.list_transform(command, variable, items, &visible, rest),
            _ => self.warn(command, &format!("list({operation}) is not supported")),
        }
    }

    /// The options whose values change what `read` gives for a list with conditional items, or None when they change
    /// nothing.
    fn options_changing(&self, items: &[String], read: &ListRead<'_>) -> Option<String> {
        let mut names: Vec<String> = Vec::new();
        for item in items {
            let Some((condition, _)) = model::split_conditional_item(item) else {
                continue;
            };
            for atom in condition_atoms(&condition) {
                if self.options.contains_key(&atom) && !names.contains(&atom) {
                    names.push(atom);
                }
            }
        }
        if names.is_empty() {
            return None;
        }

        let defaults = self.option_defaults();
        let expected = read(&resolved_items(items, &defaults));
        let changes = names.len() > MAX_TRIED_OPTIONS
            || (1u32..(1 << names.len())).any(|flipped| {
                let mut options = defaults.clone();
                for (index, name) in names.iter().enumerate() {
                    if flipped & (1 << index) != 0 {
                        options.insert(name.clone(), !defaults.get(name).copied().unwrap_or(false));
                    }
                }
                read(&resolved_items(items, &options)) != expected
            });
        changes.then(|| names.join(", "))
    }

    /// list(TRANSFORM var ACTION [args] [AT i... | FOR a b [step] | REGEX r] [OUTPUT_VARIABLE out]). Indexes count the
    /// items this machine has (`visible`); a conditional item is transformed inside its condition.
    fn list_transform(&mut self, command: &Command, variable: &str, mut items: Vec<String>, visible: &[usize], rest: &[String]) {
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
                    while let Some(index) = rest.get(position).and_then(|index| list_index(index, visible.len())) {
                        indexes.insert(visible[index]);
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
                    let stop = stop.min(visible.len().saturating_sub(1));
                    selected = Some((start..=stop).step_by(step).filter_map(|index| visible.get(index).copied()).collect());
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
        let mut case_changed = Vec::new();
        for (index, item) in items.iter_mut().enumerate() {
            let (condition, text) = match model::split_conditional_item(item) {
                Some((condition, text)) => (Some(condition), text.to_string()),
                None => (None, item.clone()),
            };
            let chosen = selected.as_ref().is_none_or(|indexes| indexes.contains(&index))
                && selector_regex.as_ref().is_none_or(|pattern| pattern.is_match(&text));
            if !chosen {
                continue;
            }

            let transformed = match action.as_str() {
                "APPEND" => format!("{text}{}", action_args[0]),
                "PREPEND" => format!("{}{text}", action_args[0]),
                "TOLOWER" | "TOUPPER" => {
                    let (converted, host_value) = self.change_case(&text, action == "TOUPPER");
                    case_changed.extend(host_value);
                    converted
                }
                "STRIP" => text.trim().to_string(),
                "GENEX_STRIP" => strip_genex(&text),
                "REPLACE" => match &replace_regex {
                    Some(pattern) => pattern.replace_all(&text, cmake_replacement(&action_args[1]).as_str()).into_owned(),
                    None => text,
                },
                _ => text,
            };
            *item = match condition {
                Some(condition) => model::conditional_item(&condition, &transformed),
                None => transformed,
            };
        }

        if let Some(value) = case_changed.first() {
            self.warn_platform_case(command, &format!("list(TRANSFORM {variable} {action})"), value);
        }
        self.set_variable(&output, &items.join(";"));
    }

    // Strings

    fn command_string(&mut self, command: &Command, args: &[String]) {
        let Some((operation, rest)) = args.split_first() else { return };

        // Text holding values set per condition: the operation on each version, the result one value per condition
        let output = string_output(operation, rest);
        let current =
            matches!(operation.as_str(), "APPEND" | "PREPEND").then(|| output.and_then(|output| self.get_variable(output)).unwrap_or_default());
        let inputs: Vec<&str> = current.iter().map(String::as_str).chain(rest.iter().map(String::as_str)).collect();
        if let (Some(output), true) = (output, inputs.iter().any(|input| input.contains("$<$<BOOL:"))) {
            let output = output.to_string();
            let Some(versions) = super::interp::texts_versions(&inputs) else {
                let host: Vec<String> = rest.iter().map(|input| self.host_text(input)).collect();
                self.warn(
                    command,
                    &format!("string({operation}) of a value set per condition has too many versions to convert; the result is this machine's"),
                );
                if let Some(current) = &current {
                    let current = self.host_text(current);
                    self.set_variable(&output, &current);
                }
                return self.string_operation(command, operation, &host);
            };
            let mut versions: Vec<(Truth, (usize, Vec<String>))> = versions
                .into_iter()
                .enumerate()
                .map(|(index, (condition, parts))| (condition, (index, parts)))
                .collect();
            self.host_version_last(&mut versions);

            let mut results = Vec::new();
            for (condition, (index, mut parts)) in versions {
                if current.is_some() {
                    let current = parts.remove(0);
                    self.set_variable(&output, &current);
                }
                self.string_operation(command, operation, &parts);
                results.push((index, condition, self.get_variable(&output).unwrap_or_default()));
            }
            results.sort_by_key(|(index, _, _)| *index);
            let results = results.into_iter().map(|(_, condition, result)| (condition, result)).collect();
            self.set_variable(&output, &super::interp::versions_value(results));
            return;
        }

        self.string_operation(command, operation, rest);
    }

    fn string_operation(&mut self, command: &Command, operation: &str, rest: &[String]) {
        match operation {
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
                let (value, host_value) = self.change_case(input, operation == "TOUPPER");
                if let Some(host_value) = host_value {
                    self.warn_platform_case(command, &format!("string({operation})"), &host_value);
                }
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

                let (begin, end) = substring_range(begin, length, input.len());
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
                let count: usize = count.parse().unwrap_or(0);
                if input.len().saturating_mul(count) > MAX_REPEATED_LENGTH {
                    self.warn(command, &format!("string(REPEAT) of {count} copies is too long to convert; left empty"));
                    self.set_variable(output, "");
                    return;
                }
                self.set_variable(output, &input.repeat(count));
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

    /// Text in upper or lower case, and whether it held a platform value (the compiler's ID, the system's name): that
    /// cannot stay symbolic once its case changes, so it becomes this machine's value (given back for the warning).
    /// Anvil's placeholders (`{out}`, `{config}`) keep their case.
    fn change_case(&self, text: &str, upper: bool) -> (String, Option<String>) {
        let host = self.marker_free_text(text);
        let converted = change_case_keeping_placeholders(&host, upper);
        let from_platform = host != text;
        (converted.clone(), from_platform.then_some(converted))
    }

    fn warn_platform_case(&mut self, command: &Command, what: &str, value: &str) {
        let message = if value.contains("{config}") {
            format!("{what} of the build type keeps the profile's own case (`{{config}}`: Debug, Release)")
        } else {
            format!("{what} of a platform or compiler value gives this machine's (`{value}`); what tests the result holds as on this machine only")
        };
        self.warn(command, &message);
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
        let replaced = CONFIGURE_TEXT_AT
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

        // An expression holding values set per condition (a type's size): evaluated under each
        let versions = match super::interp::text_versions(expression) {
            Some(versions) => versions,
            None => {
                self.warn(
                    command,
                    "math(EXPR) of values set per condition has too many versions to convert; the result is this machine's",
                );
                vec![(Truth::Known(true), self.host_text(expression))]
            }
        };

        let hexadecimal = rest.windows(2).any(|pair| pair[0] == "OUTPUT_FORMAT" && pair[1] == "HEXADECIMAL");
        let mut results = Vec::new();
        for (condition, expression) in versions {
            let Some(value) = evaluate_math(&expression) else {
                self.warn(command, &format!("math(EXPR): cannot evaluate `{}`", model::readable(&expression)));
                return;
            };
            results.push((condition, if hexadecimal { format!("0x{value:x}") } else { value.to_string() }));
        }
        self.set_variable(output, &super::interp::versions_value(results));
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
                let end = number("LIMIT").map_or(text.len(), |limit| start.saturating_add(limit).min(text.len()));
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
                    .filter(|line| filter.as_ref().is_none_or(|pattern| pattern.is_match(line)))
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
                let known = keywords.value("INPUT").is_none() && keywords.value("CONDITION").is_none_or(|condition| condition == "1");
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
                full_pattern.strip_prefix(base.as_str()).unwrap_or(&full_pattern).matches('/').count()
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
            self.project_version = crate::project::is_version(&version).then_some(version);
        }
    }

    /// find_package(): remembered, so its imported targets map to dependencies. A REQUIRED package is found, and so is
    /// an optional one Anvil has (Threads, Python) or can add (a known port, one in vcpkg.json, a fetched one, a port
    /// in the vcpkg checkout); other optional ones are not, as on a machine without them, and the warning says so.
    fn command_find_package(&mut self, command: &Command, args: &[String]) {
        let qt_args = self.qt_find_package_names(args);
        let args = qt_args.as_deref().unwrap_or(args);
        let Some(name) = args.first() else { return };
        let required = args.iter().any(|word| word == "REQUIRED");
        let source = if required {
            PackageSource::Project
        } else {
            self.optional_package_source(name)
        };

        let when = self
            .current_condition()
            .map(|condition| format!(" (when `{condition}`)"))
            .unwrap_or_default();
        match source {
            PackageSource::Absent(reason) => {
                self.set_variable(&format!("{name}_FOUND"), "OFF");
                self.set_variable(&format!("{}_FOUND", name.to_uppercase()), "OFF");
                self.warn(
                    command,
                    &format!(
                        "find_package({name}){when} is optional and {reason}, so it is taken as not found: what {name}_FOUND turns on is left out"
                    ),
                );
                return;
            }

            // Found where the user turns it on, as CMake finds it only where it is installed
            PackageSource::Port(port) => {
                let option = format!("{name}_FOUND");
                let branch = self.current_condition();
                if self.options.contains_key(&option) {
                    self.widen_option_condition(&option, branch);
                } else {
                    let description = format!("Build with the optional package {name} (vcpkg port `{port}`)");
                    self.options.insert(option.clone(), (false, description));
                    self.cache.insert(option.clone(), "OFF".into());
                    if let Some(branch) = branch {
                        self.builtin.option_conditions.insert(option.clone(), branch);
                    }
                    self.builtin.package_options.push(option.clone());
                    self.warn(
                        command,
                        &format!("find_package({name}){when} is optional: the option {option} stands for it, off as where {name} is not installed; `-o {option}=true` builds with the port `{port}`"),
                    );
                }
                let upper = format!("{}_FOUND", name.to_uppercase());
                if upper != option {
                    self.set_variable(&upper, &model::option_marker(&option));
                }
            }
            PackageSource::Project => {
                self.set_variable(&format!("{name}_FOUND"), "ON");
                self.set_variable(&format!("{}_FOUND", name.to_uppercase()), "ON");
            }
        }
        if !self.packages.contains(name) {
            self.packages.push(name.clone());
        }
        let (components, version) = find_package_request(args);
        let request = self.package_requests.entry(name.clone()).or_default();
        for component in components {
            if !request.0.contains(&component) {
                request.0.push(component);
            }
        }
        request.1 = request.1.take().or(version);

        // Variables some packages are known for
        match name.as_str() {
            "Python3" | "Python" | "PythonInterp" => {
                self.set_variable(&format!("{name}_EXECUTABLE"), "python");
                self.set_variable("PYTHON_EXECUTABLE", "python");
            }
            "Threads" => {
                // As FindThreads sets them: pthreads wherever the compiler has them (MinGW too), Win32 threads on
                // Windows (MinGW has both)
                self.set_variable("CMAKE_THREAD_LIBS_INIT", "Threads::Threads");
                self.set_variable("CMAKE_USE_PTHREADS_INIT", &model::option_marker("!msvc"));
                self.set_variable("CMAKE_HAVE_LIBC_PTHREAD", &model::option_marker("!msvc"));
                self.set_variable("CMAKE_USE_WIN32_THREADS_INIT", &model::option_marker("windows"));
            }
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

    /// Where an optional package comes from: Anvil or the project (a system package, a port it knows, one in the
    /// project's vcpkg.json, one fetched with FetchContent or CPM), a port in the vcpkg checkout, or nowhere (and why).
    fn optional_package_source(&mut self, name: &str) -> PackageSource {
        let normalized = |text: &str| text.to_lowercase().replace(['-', '_'], "");
        let wanted = normalized(name);

        if self.builtin.manifest_ports.is_none() {
            self.builtin.manifest_ports = Some(manifest_ports(&self.root));
        }
        let in_manifest = self.builtin.manifest_ports.iter().flatten().any(|port| normalized(port) == wanted);
        let fetched = self.fetched.keys().any(|fetched| normalized(fetched) == wanted);
        if SYSTEM_PACKAGES.contains(&name) || name == "Vulkan" || in_manifest || fetched {
            return PackageSource::Project;
        }
        let known_port = super::emit::KNOWN_PORTS
            .iter()
            .find(|(pattern, _)| normalized(pattern.trim_end_matches("::")) == wanted);
        if let Some((_, port)) = known_port {
            return PackageSource::Port(port.to_string());
        }

        // A port of the package's name, as the emitter looks its imported targets up; not one that needs this project
        // (harfbuzz for freetype), which vcpkg cannot install for it
        let lower = name.to_lowercase();
        let vcpkg = self.builtin.vcpkg.get_or_insert_with(|| crate::vcpkg::Instance::select(None).ok());
        let Some(instance) = vcpkg.as_ref() else {
            return PackageSource::Absent("no vcpkg was found to take a port from".into());
        };
        let Some(port) = [lower.clone(), lower.replace('_', "-")]
            .into_iter()
            .find(|port| instance.has_port(port) == Some(true))
        else {
            return PackageSource::Absent("Anvil has no port for it".into());
        };
        let project = self.project_name.as_deref().map(normalized).unwrap_or_default();
        if port_dependencies(&instance.root, &port)
            .iter()
            .any(|dependency| normalized(dependency) == project)
        {
            return PackageSource::Absent(format!("its port `{port}` depends on this project"));
        }
        PackageSource::Port(port)
    }

    /// How a build finds a program: its name when it is on PATH or among the MSVC tools (dumpbin beside link.exe,
    /// which build steps find on the toolchain's PATH), its path when only one of `folders` has it; None when nothing
    /// has it.
    fn find_program(&mut self, name: &str, folders: &[PathBuf]) -> Option<String> {
        let in_folder = |folder: &Path| {
            ["", ".exe", ".bat", ".cmd"]
                .iter()
                .map(|extension| folder.join(format!("{name}{extension}")))
                .find(|path| path.is_file())
        };
        if util::which(name).is_some() {
            return Some(name.to_string());
        }
        if let Some(path) = folders.iter().find_map(|folder| in_folder(folder)) {
            return Some(slash(&path));
        }

        let tool_folders = self.builtin.tool_folders.get_or_insert_with(msvc_tool_folders);
        tool_folders.iter().any(|folder| in_folder(folder).is_some()).then(|| name.to_string())
    }

    /// pkg_check_modules(PREFIX [REQUIRED] [IMPORTED_TARGET] modules...): PkgConfig::PREFIX and PREFIX_LIBRARIES. An
    /// optional one finds its modules where this machine's pkg-config knows them (pkg_search_module: one of them);
    /// otherwise PREFIX_FOUND is false, as on a machine without them, and the warning says so.
    fn command_pkg_check_modules(&mut self, command: &Command, args: &[String]) {
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

        // An optional one only where pkg-config knows it
        let required = rest.iter().any(|word| word == "REQUIRED");
        let search = command.name == "pkg_search_module";
        if !required && !pkg_config_knows(&modules, search) {
            self.set_variable(&format!("{prefix}_FOUND"), "");
            let when = self
                .current_condition()
                .map(|condition| format!(" (when `{condition}`)"))
                .unwrap_or_default();
            self.warn(
                command,
                &format!(
                    "{}({prefix} {}){when} is optional and pkg-config does not know it here, so it is taken as not found: what {prefix}_FOUND turns on is left out; install it and convert again to keep it",
                    command.name,
                    modules.join(" ")
                ),
            );
            return;
        }

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
                // The first name found (as CMake stores it, but a name on PATH stays a name, so the result works on
                // other machines); not found here: steps and tests that need it drop out
                let keywords = Keywords::parse(rest, &[], &[], &["NAMES", "PATHS", "HINTS", "PATH_SUFFIXES", "DOC"]);
                let names = if rest.first().map(String::as_str) == Some("NAMES") {
                    keywords.list("NAMES")
                } else {
                    vec![first_name.clone()]
                };
                let folders: Vec<PathBuf> = keywords.list("PATHS").iter().chain(&keywords.list("HINTS")).map(PathBuf::from).collect();
                let found = names.iter().find_map(|name| self.find_program(name, &folders));
                if found.is_none() {
                    let when = self
                        .current_condition()
                        .map(|condition| format!(" (when `{condition}`)"))
                        .unwrap_or_default();
                    self.warn(
                        command,
                        &format!("find_program({variable} {}){when}: not found on this machine, so build steps and tests that run it are left out; install it and convert again to keep them", names.join(" ")),
                    );
                }
                let value = found.unwrap_or_else(|| format!("{variable}-NOTFOUND"));
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
                if !inline {
                    push_distinct(&mut varying, &condition);
                }
                push_distinct(&mut conditions, &condition);
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

        let enclosing = self.current_condition();
        let texts = texts_by_choice(&varying, enclosing.as_deref(), &|chosen| {
            self.configured_text(template, only_at, escape_quotes, &inline, enclosing.as_deref(), &|condition| {
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
    fn configured_text(
        &self,
        template: &str,
        only_at: bool,
        escape_quotes: bool,
        inline: &[String],
        enclosing: Option<&str>,
        choice: &dyn Fn(&str) -> bool,
    ) -> String {
        let mut text = String::new();

        for line in template.split_inclusive('\n') {
            let read = RefCell::new(Vec::<String>::new());
            let plain = self.configure_line(line, only_at, escape_quotes, &|condition| {
                if inline.iter().any(|known| chosen_as(known, condition).is_some()) {
                    push_distinct(&mut read.borrow_mut(), condition);
                }
                choice(condition)
            });
            let read = read.into_inner();
            if read.is_empty() || read.len() > MAX_VARIANT_CONDITIONS {
                text.push_str(&plain);
                continue;
            }

            let versions = texts_by_choice(&read, enclosing, &|chosen| {
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
        let has_define = body.contains("cmakedefine");
        let body = if let Some(captures) = has_define.then(|| CMAKEDEFINE.captures(body)).flatten() {
            let indent = &captures[1];
            match value(&captures[2]).0 {
                true => body.replacen(&format!("#{indent}cmakedefine"), &format!("#{indent}define"), 1),
                false => format!("/* #undef {} */", &captures[2]),
            }
        } else if let Some(captures) = has_define.then(|| CMAKEDEFINE01.captures(body)).flatten() {
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
        let mut body = if body.contains('@') {
            CONFIGURE_AT.replace_all(&body, substitute).into_owned()
        } else {
            body
        };
        if !only_at && body.contains('$') {
            body = CONFIGURE_DOLLAR.replace_all(&body, substitute).into_owned();
            if body.contains("$ENV{") || body.contains("${") {
                body = self.expand(&body);
            }
        }

        body + ending
    }

    /// A variable as configure_file reads it: whether it is on, and its text. One that depends on a condition (an
    /// option, a platform, a check's result) takes `choice`'s answer for it: a type's size by platform, the items
    /// a branch not taken here set where `choice` takes the branch, an option or check's result as on or off.
    fn configure_value(&self, name: &str, choice: &dyn Fn(&str) -> bool) -> (bool, String) {
        let value = self.get_variable(name);

        // An option nothing set since reads as on or off, whatever value branches gave its cache entry
        if let (Some(Truth::Depends(condition)), true) = (self.option_truth(name), value.as_deref().is_some_and(|value| value.contains("$<$<BOOL:")))
        {
            let on = choice(&condition);
            return (on, if on { "ON" } else { "OFF" }.to_string());
        }

        // Items or text under conditions (inside longer text too): those `choice` takes. Every condition is read, so
        // each choice of them sees the same ones
        if let Some(value) = value.as_deref().filter(|value| value.contains("$<$<BOOL:")) {
            let conditional = std::cell::Cell::new(false);
            let chosen = model::choose_list_items(value, &|condition| {
                conditional.set(true);
                choice(condition)
            });
            if conditional.get() {
                let text = self.marker_free_text(&chosen);
                return (model::constant_truth(&text).unwrap_or(true), text);
            }
        }

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
                (on, self.chosen_text(value.as_deref(), text, choice))
            }
            Truth::Known(on) => (on, self.marker_free_text(&value.unwrap_or_default())),
        }
    }

    /// Text as this machine has it (host_text), skipping the work for text without platform or check markers.
    fn marker_free_text(&self, text: &str) -> String {
        if text.contains('\u{1}') {
            self.host_text(text)
        } else {
            text.to_string()
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

        // Warnings made errors (`-Werror` for GCC, `-WX` for MSVC: one branch's) fail a source check that warns
        let required_flags = self.get_variable("CMAKE_REQUIRED_FLAGS").unwrap_or_default();
        let warnings_fail = required_flags.split_whitespace().any(|flag| ["-Werror", "-WX", "/WX"].contains(&flag));

        // The result variable, and what decides it
        let (result_variable, truth) = match command.name.as_str() {
            "check_ipo_supported" => (
                args.windows(2).find(|pair| pair[0] == "RESULT").map(|pair| pair[1].clone()),
                Truth::Known(true),
            ),
            "check_c_compiler_flag" => (args.get(1).cloned(), flags_truth(&arg(0), Some("C"))),
            "check_cxx_compiler_flag" => (args.get(1).cloned(), flags_truth(&arg(0), Some("CXX"))),
            "check_compiler_flag" | "check_linker_flag" => (args.get(2).cloned(), flags_truth(&arg(1), Some(&arg(0)))),
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
                if let Some(variable) = args.get(1) {
                    self.check_type_size(command, &arg(0), variable);
                }
                (args.get(1).map(|variable| format!("HAVE_{variable}")), names_truth(&names(0)))
            }
            "check_c_source_compiles" | "check_cxx_source_compiles" | "check_c_source_runs" | "check_cxx_source_runs" => {
                (args.get(1).cloned(), source_truth(&arg(0), warnings_fail))
            }
            "check_source_compiles" | "check_source_runs" => (args.get(2).cloned(), source_truth(&arg(1), warnings_fail)),
            _ => {
                self.unknown_commands.insert(command.name.clone());
                return;
            }
        };
        if command.name.ends_with("_flag") {
            let flags = if matches!(command.name.as_str(), "check_compiler_flag" | "check_linker_flag") {
                arg(1)
            } else {
                arg(0)
            };
            for flag in unknown_flags(&flags) {
                let variable = result_variable.clone().unwrap_or_default();
                self.warn(
                    command,
                    &format!(
                        "{}({flag}): a flag Anvil does not know, so {variable} is false; where the compiler takes it, set it by hand",
                        command.name
                    ),
                );
            }
        }

        // Flags and libraries the check is made with count too, unless they work on this machine: then they may be
        // the ones the branch this machine takes set (`-WX` for MSVC where GCC gets `-Werror`), and say nothing about
        // the other platforms, which set their own
        let required_libraries = model::split_list(&self.get_variable("CMAKE_REQUIRED_LIBRARIES").unwrap_or_default());
        let required = required_libraries
            .iter()
            .fold(flags_truth(&required_flags, None), |truth, library| truth.and(library_truth(library)));
        let truth = match required {
            Truth::Depends(condition) if model::holds_on_host(&condition, &|_| None) => truth,
            required => truth.and(required),
        };

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
        // A cache entry, as CMake's: seen outside the function that ran the check
        self.cache.insert(variable, value);
    }

    /// check_type_size(type VAR): VAR is the size, one per condition where it differs by platform. A type not in
    /// TYPE_SIZES has no size known without compiling.
    fn check_type_size(&mut self, command: &Command, type_name: &str, variable: &str) {
        if self.get_variable(variable).is_some() {
            return;
        }
        let Some(size) = type_size(type_name) else {
            self.warn(
                command,
                &format!("check_type_size({type_name}): its size is not known without compiling; {variable} is left empty, so set it by hand where it is used"),
            );
            return;
        };

        // Each case where no earlier one holds, then the rest
        let mut earlier = Truth::Known(false);
        let mut sizes = Vec::new();
        for (condition, case_size) in &size.cases {
            let here = Truth::Depends(condition.clone());
            sizes.push((earlier.clone().not().and(here.clone()), case_size.clone()));
            earlier = earlier.or(here);
        }
        sizes.push((earlier.not(), size.otherwise.clone()));
        let sizes = sizes
            .into_iter()
            .map(|(truth, size)| match truth {
                Truth::Depends(condition) => (model::simplify(&condition, None), size),
                known => (known, size),
            })
            .filter(|(truth, _)| *truth != Truth::Known(false))
            .collect();
        self.cache.insert(variable.to_string(), super::interp::versions_value(sizes));
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
// find_package's keywords besides COMPONENTS and REQUIRED (and the NO_... ones), which end a list of components
const FIND_PACKAGE_KEYWORDS: &[&str] = &[
    "EXACT",
    "QUIET",
    "MODULE",
    "CONFIG",
    "OPTIONAL_COMPONENTS",
    "GLOBAL",
    "NAMES",
    "CONFIGS",
    "HINTS",
    "PATHS",
    "PATH_SUFFIXES",
    "REGISTRY_VIEW",
    "BYPASS_PROVIDER",
    "CMAKE_FIND_ROOT_PATH_BOTH",
    "ONLY_CMAKE_FIND_ROOT_PATH",
];

/// A find_package call's components (after COMPONENTS, or REQUIRED) and version (the word after the name, when it is
/// one).
fn find_package_request(args: &[String]) -> (Vec<String>, Option<String>) {
    let version = args
        .get(1)
        .map(|word| word.split("...").next().unwrap_or(word).to_string())
        .filter(|word| crate::cmake_package::parse_version(word).is_some());

    let mut components = Vec::new();
    let mut listing = false;
    for word in args.iter().skip(1) {
        match word.as_str() {
            "COMPONENTS" | "REQUIRED" => listing = true,
            _ if FIND_PACKAGE_KEYWORDS.contains(&word.as_str()) || word.starts_with("NO_") => listing = false,
            _ if listing && !components.contains(word) => components.push(word.clone()),
            _ => {}
        }
    }

    (components, version)
}

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
    ("sys/file.h", "!msvc"),
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
    ("intrin.h", "windows"),
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
    ("fopen64", "linux | mingw"),
    ("pthread_setname_np", "!msvc"),
    ("getpagesize", "!msvc"),
    ("strtok_r", "!msvc"),
    ("ssize_t", "!msvc"),
    ("__int128", "!msvc"),
    ("arc4random", "!windows"),
    ("arc4random_buf", "!windows"),
    ("getentropy", "!windows"),
    ("mmap", "!windows"),
    ("munmap", "!windows"),
    ("dlopen", "!windows"),
    ("posix_memalign", "!windows"),
    ("strerror_r", "!windows"),
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
    ("u_int32_t", "!windows"),
    ("u_int64_t", "!windows"),
    ("getrandom", "linux"),
    ("SYS_getrandom", "linux"),
    ("memalign", "linux"),
    ("pipe2", "linux"),
    ("getauxval", "linux"),
    ("off64_t", "linux | mingw"),
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
    ("m", "!msvc"),
    ("dl", "!windows"),
    ("rt", "linux"),
    ("pthread", "!msvc"),
    ("socket", "!windows"),
    ("nsl", "linux"),
    ("execinfo", "!windows"),
];

// check_type_size()'s sizes: (types, sizes by condition (the first that holds), size otherwise). An empty size: the
// type does not exist there. 32-bit targets are x86 and arm; Windows is LLP64 (long is 4 bytes) and MSVC's long
// double is a double.
type TypeSizeRow = (&'static [&'static str], &'static [(&'static str, &'static str)], &'static str);
const TYPE_SIZES: &[TypeSizeRow] = &[
    (&["char", "signed char", "unsigned char", "_Bool", "bool", "int8_t", "uint8_t"], &[], "1"),
    (
        &["short", "short int", "unsigned short", "unsigned short int", "int16_t", "uint16_t"],
        &[],
        "2",
    ),
    (&["int", "unsigned int", "unsigned", "int32_t", "uint32_t", "float"], &[], "4"),
    (
        &[
            "long long",
            "long long int",
            "unsigned long long",
            "unsigned long long int",
            "int64_t",
            "uint64_t",
            "double",
        ],
        &[],
        "8",
    ),
    (
        &["long", "long int", "unsigned long", "unsigned long int"],
        &[("windows | x86 | arm", "4")],
        "8",
    ),
    (
        &["size_t", "ptrdiff_t", "intptr_t", "uintptr_t", "void*", "char*"],
        &[("x86 | arm", "4")],
        "8",
    ),
    (&["ssize_t"], &[("msvc", ""), ("x86 | arm", "4")], "8"),
    (&["wchar_t"], &[("windows", "2")], "4"),
    (&["long double"], &[("msvc | (macos & arm64)", "8"), ("x86 & !windows", "12")], "16"),
    (&["time_t"], &[("(x86 | arm) & !windows", "4")], "8"),
    (&["off_t"], &[("windows | x86 | arm", "4")], "8"),
    (&["off64_t"], &[("!(linux | mingw)", "")], "8"),
    (&["__int64", "unsigned __int64"], &[("!windows", "")], "8"),
    (&["__int128", "unsigned __int128"], &[("msvc | x86 | arm", "")], "16"),
    (&["u_int32_t"], &[("windows", "")], "4"),
    (&["u_int64_t"], &[("windows", "")], "8"),
];

/// A type's sizes from TYPE_SIZES (any pointer is a void*'s size), with this machine's.
fn type_size(type_name: &str) -> Option<TypeSize> {
    let words: Vec<&str> = type_name.split_whitespace().collect();
    let mut name = words.join(" ").replace(" *", "*");
    if name.ends_with('*') {
        name = "void*".into();
    }
    let (_, cases, otherwise) = TYPE_SIZES.iter().find(|(names, _, _)| names.contains(&name.as_str()))?;

    Some(TypeSize {
        cases: cases.iter().map(|(condition, size)| (condition.to_string(), size.to_string())).collect(),
        otherwise: otherwise.to_string(),
    })
}

// configure_file()'s patterns: #cmakedefine and #cmakedefine01 lines, @VAR@ and ${VAR}
static CMAKEDEFINE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"#([ \t]*)cmakedefine[ \t]+([A-Za-z0-9_]*)").unwrap());
static CMAKEDEFINE01: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"#([ \t]*)cmakedefine01[ \t]+([A-Za-z0-9_]*)").unwrap());
static CONFIGURE_AT: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"@([A-Za-z0-9_./+-]+)@").unwrap());
static CONFIGURE_DOLLAR: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\$\{([A-Za-z0-9_./+-]+)\}").unwrap());
// string(CONFIGURE)'s @VAR@
static CONFIGURE_TEXT_AT: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"@([A-Za-z_][A-Za-z0-9_]*)@").unwrap());
// A check's source: its #includes and words
static SOURCE_INCLUDE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r#"#\s*include\s*[<"]([^>"]+)[>"]"#).unwrap());
static SOURCE_WORD: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").unwrap());
// A CMake regex replacement's group reference (\1)
static REPLACEMENT_GROUP: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\\(\d)").unwrap());

// Macros a check's source tests to pick a platform's code: then its names say nothing about where it compiles
const PLATFORM_MACROS: &[&str] = &["_WIN32", "_MSC_VER", "WIN32", "__GNUC__", "__clang__", "__MINGW32__"];

/// Where compiler or linker flags pass check_*_flag() as CMake runs it for `language` ("C", "CXX"; None for either):
/// cl's options (`/W4`, and spelled with a dash, `-W4`) with MSVC, GCC's (`-Wextra`) with GCC and Clang, Clang's
/// (`-Wcomma`) with Clang (not clang-cl, as GCC's), and a few with both. cl's options that fail its checks (CL_FAILING_FLAGS) pass nowhere when
/// spelled with a slash, and flags Anvil does not know pass nowhere.
fn flags_truth(flags: &str, language: Option<&str>) -> Truth {
    flags.split_whitespace().fold(Truth::Known(true), |truth, flag| {
        truth.and(flag_truth(flag, language).unwrap_or(Truth::Known(false)))
    })
}

/// The flags of `flags` that flags_truth() does not know.
fn unknown_flags(flags: &str) -> Vec<&str> {
    flags.split_whitespace().filter(|flag| flag_truth(flag, None).is_none()).collect()
}

/// Where one flag passes check_*_flag() (see flags_truth); None when Anvil does not know it.
fn flag_truth(flag: &str, language: Option<&str>) -> Option<Truth> {
    let flag = flag.strip_prefix("LINKER:").unwrap_or(flag);
    let depends = |condition: &str| Some(Truth::Depends(condition.into()));

    if let Some(name) = flag.strip_prefix('/') {
        return if CL_FAILING_FLAGS.is_match(name) {
            Some(Truth::Known(false))
        } else {
            depends("msvc")
        };
    }
    if flag == "-fsanitize=address" {
        // cl has AddressSanitizer too; MinGW's GCC has no runtime for it
        return depends("!mingw");
    }
    let Some(name) = flag.strip_prefix('-') else {
        return Some(Truth::Known(true));
    };

    if ["D", "I", "U"].iter().any(|prefix| name.starts_with(prefix)) || BOTH_COMPILERS_FLAGS.is_match(name) {
        Some(Truth::Known(true))
    } else if is_cl_flag(name) {
        depends("msvc")
    } else if let Some(warning) = name.strip_prefix('W').filter(|warning| !GNU_PASS_THROUGH.is_match(warning)) {
        depends(warning_condition(warning, language))
    } else if CLANG_FLAGS.is_match(name) {
        depends("clang & !msvc")
    } else if GNU_FLAGS.is_match(name) {
        depends("!msvc")
    } else {
        None
    }
}

/// Where `-W<warning>` passes: GCC's warnings (GCC_WARNINGS, for the check's language) with GCC, and with Clang
/// unless GCC_ONLY_WARNINGS; others are Clang's. GCC takes any `-Wno-` of a warning it does not know.
fn warning_condition(warning: &str, language: Option<&str>) -> &'static str {
    if warning.is_empty() || warning == "error" {
        return "!msvc";
    }
    let negated = warning.strip_prefix("no-");
    let name = negated.unwrap_or(warning);
    let name = name.strip_prefix("error=").unwrap_or(name);
    let (name, value) = name.split_once('=').map_or((name, None), |(name, value)| (name, Some(value)));

    let listed = |list: &str| list.split_whitespace().any(|known| known == name);
    let for_c = listed(GCC_C_WARNINGS);
    let for_cxx = listed(GCC_CXX_WARNINGS);
    let gcc_knows = name.starts_with("analyzer-")
        || listed(GCC_WARNINGS)
        || match language {
            Some("C") => for_c,
            Some("CXX") => for_cxx,
            _ => for_c || for_cxx,
        };
    let other_language = !gcc_knows && (for_c || for_cxx);

    let gcc_only = name.starts_with("analyzer-")
        || GCC_ONLY_WARNINGS.is_match(name)
        || value.is_some_and(|_| !["format", "frame-larger-than", "strict-overflow", "strict-aliasing"].contains(&name));
    match (gcc_knows || (negated.is_some() && !other_language), gcc_only) {
        (true, true) => "gcc",
        (true, false) => "!msvc",
        (false, _) => "clang & !msvc",
    }
}

// `-Wl,`, `-Wa,` and `-Wp,`: options GCC and Clang hand to the linker, assembler and preprocessor
static GNU_PASS_THROUGH: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^[lap],").unwrap());

// GCC 13's warnings for C and C++ (`gcc --help=warnings,c`, `,c++`, `,common`; -Wanalyzer-* aside), then those for
// C only and C++ only, which fail a check of the other language
const GCC_WARNINGS: &str = concat!(
    "NSObject-attribute abi address address-of-packed-member aggregate-return aggressive-loop-optimizations all ",
    "alloc-size-larger-than alloc-zero alloca alloca-larger-than arith-conversion array-bounds array-compare ",
    "array-parameter attribute-alias attribute-warning attributes bidi-chars bool-compare bool-operation ",
    "builtin-declaration-mismatch builtin-macro-redefined cannot-profile cast-align cast-function-type cast-qual ",
    "char-subscripts chkp clobbered comment comments complain-wrong-lang conversion coverage-invalid-line-number ",
    "coverage-mismatch cpp dangling-else dangling-pointer date-time deprecated deprecated-declarations ",
    "disabled-optimization div-by-zero double-promotion duplicated-branches duplicated-cond empty-body ",
    "endif-labels enum-compare enum-conversion expansion-to-defined extra float-conversion float-equal format ",
    "format-contains-nul format-diag format-extra-args format-nonliteral format-overflow format-security ",
    "format-signedness format-truncation format-y2k format-zero-length frame-address frame-larger-than ",
    "free-nonheap-object hsa if-not-aligned ignored-attributes ignored-qualifiers implicit-fallthrough ",
    "infinite-recursion init-self inline int-in-bool-context int-to-pointer-cast invalid-memory-model invalid-pch ",
    "invalid-utf8 larger-than logical-not-parentheses logical-op long-long lto-type-mismatch main ",
    "maybe-uninitialized memset-elt-size memset-transposed-args misleading-indentation mismatched-dealloc ",
    "missing-attributes missing-braces missing-declarations missing-field-initializers missing-format-attribute ",
    "missing-include-dirs missing-noreturn missing-profile multichar multistatement-macros narrowing ",
    "nonnull nonnull-compare normalized null-dereference odr openacc-parallelism openmp-simd ",
    "overflow overlength-strings packed packed-bitfield-compat packed-not-aligned padded parentheses pedantic ",
    "pedantic-ms-format pointer-arith pointer-compare pragmas prio-ctor-dtor psabi redundant-decls restrict ",
    "return-local-addr return-type scalar-storage-order sequence-point shadow shadow-compatible-local shadow-local ",
    "shift-count-negative shift-count-overflow shift-negative-value shift-overflow sign-compare sign-conversion ",
    "sizeof-array-argument sizeof-array-div sizeof-pointer-div sizeof-pointer-memaccess stack-protector ",
    "stack-usage strict-aliasing strict-flex-arrays strict-overflow string-compare stringop-overflow ",
    "stringop-overread stringop-truncation suggest-attribute suggest-final-methods suggest-final-types switch ",
    "switch-bool switch-default switch-enum switch-outside-range switch-unreachable sync-nand system-headers ",
    "tautological-compare trampolines trigraphs trivial-auto-var-init tsan type-limits undef unicode uninitialized ",
    "unknown-pragmas unreachable-code unsafe-loop-optimizations unused unused-but-set-parameter ",
    "unused-but-set-variable unused-const-variable unused-function unused-label unused-local-typedefs ",
    "unused-macros unused-result unused-value unused-variable use-after-free varargs variadic-macros ",
    "vector-operation-performance vla vla-larger-than vla-parameter volatile-register-var write-strings ",
    "xor-used-as-pow zero-length-bounds",
);
const GCC_C_WARNINGS: &str = concat!(
    "absolute-value bad-function-cast c++-compat c11-c2x-compat c90-c99-compat c99-c11-compat ",
    "declaration-after-statement designated-init discarded-array-qualifiers discarded-qualifiers ",
    "duplicate-decl-specifier enum-int-mismatch error-implicit-function-declaration implicit ",
    "implicit-function-declaration implicit-int incompatible-pointer-types int-conversion jump-misses-init ",
    "missing-parameter-type missing-prototypes nested-externs old-style-declaration old-style-definition ",
    "override-init override-init-side-effects pointer-sign pointer-to-int-cast strict-prototypes traditional ",
    "traditional-conversion unsuffixed-float-constants",
);
const GCC_CXX_WARNINGS: &str = concat!(
    "abi-tag aligned-new c++0x-compat c++11-compat c++11-extensions c++14-compat c++14-extensions c++17-compat ",
    "c++17-extensions c++1z-compat c++20-compat c++20-extensions c++23-extensions c++2a-compat catch-value ",
    "changes-meaning class-conversion class-memaccess comma-subscript conditionally-supported conversion-null ",
    "ctad-maybe-unsupported ctor-dtor-privacy dangling-reference delete-incomplete delete-non-virtual-dtor ",
    "deprecated-copy deprecated-copy-dtor deprecated-enum-enum-conversion deprecated-enum-float-conversion effc++ ",
    "exceptions extra-semi inaccessible-base inherited-variadic-ctor init-list-lifetime interference-size ",
    "invalid-constexpr invalid-imported-macros invalid-offsetof literal-suffix mismatched-new-delete ",
    "mismatched-tags missing-requires missing-template-keyword multiple-inheritance namespaces noexcept ",
    "noexcept-type non-template-friend non-virtual-dtor old-style-cast overloaded-virtual pessimizing-move ",
    "placement-new pmf-conversions range-loop-construct redundant-move redundant-tags register reorder self-move ",
    "sign-promo sized-deallocation strict-null-sentinel subobject-linkage suggest-override synth templates ",
    "terminate useless-cast vexing-parse virtual-inheritance virtual-move-assign volatile ",
    "zero-as-null-pointer-constant",
);

// GCC's warnings Clang does not know (a check with Clang fails on "unknown warning option")
static GCC_ONLY_WARNINGS: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(concat!(
        r"^(logical-op|duplicated-(cond|branches)|useless-cast|jump-misses-init|restrict|maybe-uninitialized|stringop-.+|",
        r"suggest-attribute|noexcept|strict-null-sentinel|trampolines|class-memaccess|alloc-zero|alloca-larger-than|",
        r"alloc-size-larger-than|use-after-free|dangling-reference|unsafe-loop-optimizations|vector-operation-performance|",
        r"shadow-local|shadow-compatible-local|lto-type-mismatch|catch-value|conditionally-supported|subobject-linkage|",
        r"placement-new|virtual-move-assign|literal-suffix|pmf-conversions|clobbered|multistatement-macros|format-diag)$"
    ))
    .unwrap()
});

// Clang's options GCC rejects
static CLANG_FLAGS: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(concat!(
        r"^(f(no-)?color-diagnostics|fansi-escape-codes|stdlib=.+|rtlib=.+|unwindlib=.+|f(no-)?standalone-debug|f(no-)?limit-debug-info|",
        r"fprofile-instr-.+|fcoverage-mapping|fmodules.*|fexperimental-.+|Qunused-arguments|ferror-limit=.+|",
        r"fmacro-backtrace-limit=.+|f(no-)?spell-checking|fms-compatibility.*|f(no-)?delayed-template-parsing|",
        r"f(no-)?cxx-exceptions|fsanitize=(safe-stack|cfi.*|dataflow|memory|type|integer|implicit-.+|nullability.*|unsigned-integer-overflow)|flto=thin|fdebug-macro|",
        r"fcomplete-member-pointers|fforce-emit-vtables|fstrict-vtable-pointers|fwhole-program-vtables|mbranches-within-32B-boundaries)$"
    ))
    .unwrap()
});

// GCC's options (Clang takes them too): code generation, machine, optimization, debug, language, preprocessor and
// linking options, and `--long` ones
static GNU_FLAGS: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(concat!(
        r"^(f[A-Za-z].*|m[A-Za-z0-9].*|O[0-3sgz]?|Ofast|g.*|ansi|pedantic(-errors)?|pthreads?|static(-.+)?|shared(-.+)?|",
        r"rdynamic|no-?pie|pie|nostdlib.*|nostdinc.*|nodefaultlibs|nostartfiles|s|x.+|isystem.*|iquote.*|idirafter.*|",
        r"include.+|imacros.+|L.+|l.+|Xlinker|Xassembler|coverage|pg?|undef|trigraphs|save-temps.*|specs=.+|",
        r"print-.+|dumpmachine|dumpversion|v|E|S|c|C|P|H|M[DMGPTF]?|MMD|time|e.+|T.+|u.+|z.+|W[lap],.+|-.+)$"
    ))
    .unwrap()
});

/// Is a flag (without its `/` or `-`) one cl takes in check_*_flag()? Those conflicting with the /RTC1 of the Debug
/// flags that checks compile with (/O1, /O2, /Ox) fail there.
fn is_cl_flag(name: &str) -> bool {
    CL_FLAGS.is_match(name) && !CL_FAILING_FLAGS.is_match(name)
}

// cl's options, which GCC rejects (or reads as something else) except BOTH_COMPILERS_FLAGS
static CL_FLAGS: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(concat!(
        r"^(W[0-4]|Wall|WX-?|w|w[1-4]\d+|w[deo]\d+|O[12dgistxy]-?|Ob[0-3]|EH[acrs-]+|Z[7iIlsfH]|Zc:.+|Zp\d*|",
        r"G[RSyFwLdrzv]-?|Gm-?|MP\d*|M[DT]d?|LDd?|J|T[CP]|utf-8|(source|execution)-charset:.+|validate-charset-?|",
        r"permissive-?|std:.+|bigobj|sdl-?|guard:.+|fp:.+|arch:.+|nologo|diagnostics:.+|external:.+|experimental:.+|",
        r"RTC.+|Q[a-z_]+.*|openmp.*|volatile:.+|F[SCI].*|F[dop].+|analyze.*|constexpr:.+|favor:.+|kernel-?|await.*|",
        r"Zm\d+|d[12].+|Brepro|JMC-?|fsanitize=address)$"
    ))
    .unwrap()
});

// cl's options that fail its checks: they conflict with /RTC1 (or are deprecated)
static CL_FAILING_FLAGS: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^O[12gx]$").unwrap());

// Dash-spelled flags both cl and GCC pass: the same option (-Wall, -w, -Os, -MD, -std=), or one GCC reads as
// another (-utf-8 is -u tf-8, -openmp is -o penmp, -external:W0 is -e xternal:W0)
static BOTH_COMPILERS_FLAGS: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^(Wall|w|Os|MD|std=.+|utf-8|openmp.*|external:.+|FS|F[CI].*)$").unwrap());

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

// A visibility attribute on a static function's declaration
static STATIC_VISIBILITY: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\bstatic\b[^;]*visibility").unwrap());

/// Where a check's source compiles: GCC extensions not with cl (nor a visibility attribute with MinGW's GCC, nor on
/// a static function with any GCC, which warn about it, when warnings fail the check), its `#if` tests of platform
/// macros (an `#error` under them, or a main() only there), and the platform headers and functions it uses where they
/// exist (unless it picks its code by platform macros).
fn source_truth(source: &str, warnings_fail: bool) -> Truth {
    let mut truth = Truth::Known(true);
    if ["__attribute__", "__builtin_", "__asm__", "__typeof__"]
        .iter()
        .any(|extension| source.contains(extension))
    {
        truth = Truth::Depends("!msvc".into());
    }
    if warnings_fail && source.contains("__attribute__") && source.contains("visibility") {
        let on_static = STATIC_VISIBILITY.is_match(source);
        truth = truth.and(Truth::Depends(if on_static { "!gcc" } else { "!mingw" }.into()));
    }
    truth = truth.and(preprocessor_truth(source));
    if PLATFORM_MACROS.iter().any(|macro_name| source.contains(macro_name)) {
        return truth;
    }

    let mut names: Vec<String> = SOURCE_INCLUDE.captures_iter(source).map(|captures| captures[1].to_string()).collect();
    names.extend(SOURCE_WORD.find_iter(source).map(|found| found.as_str().to_string()));
    names.retain(|name| PLATFORM_NAMES.iter().any(|(known, _)| known == name) || name.starts_with("linux/"));
    names.sort();
    names.dedup();

    truth.and(names_truth(&names))
}

/// Where a source's `#if` blocks let it compile: not where an `#error` is reached, and only where a main() is there.
/// Conditions test platform macros (MACRO_CONDITIONS); a block testing anything else decides nothing.
fn preprocessor_truth(source: &str) -> Truth {
    // Each open #if: the condition of its current branch, and whether an earlier branch was taken (None: unknown)
    let mut blocks: Vec<(Option<Truth>, Option<Truth>)> = Vec::new();
    let mut compiles = Some(Truth::Known(true));
    let mut main: Option<Option<Truth>> = None;

    for line in source.lines() {
        let mut active = Some(Truth::Known(true));
        for (current, _) in &blocks {
            active = and_maybe(active, current.clone());
        }

        // Code: where main() is
        let Some(directive) = line.trim_start().strip_prefix('#') else {
            if SOURCE_WORD.find_iter(line).any(|word| word.as_str() == "main") {
                main = Some(match main {
                    None => active,
                    Some(earlier) => or_maybe(earlier, active),
                });
            }
            continue;
        };

        // Directives
        let directive = directive.trim_start();
        let (word, rest) = directive.split_at(directive.find(|character: char| !character.is_alphanumeric()).unwrap_or(directive.len()));
        let rest = rest.split("//").next().unwrap_or_default().trim();
        match word {
            "if" | "ifdef" | "ifndef" => {
                let condition = match word {
                    "if" => macro_expression(rest),
                    "ifdef" => macro_defined(rest),
                    _ => macro_defined(rest).map(Truth::not),
                };
                blocks.push((condition.clone(), condition));
            }
            "elif" | "else" => {
                let Some((current, taken)) = blocks.last_mut() else {
                    return Truth::Known(true);
                };
                let condition = if word == "elif" {
                    macro_expression(rest)
                } else {
                    Some(Truth::Known(true))
                };
                *current = and_maybe(taken.clone().map(Truth::not), condition.clone());
                *taken = or_maybe(taken.clone(), condition);
            }
            "endif" => {
                blocks.pop();
            }
            "error" => compiles = and_maybe(compiles, active.map(Truth::not)),
            _ => {}
        }
    }

    // Unknown where it compiles: as if everywhere
    let main = main.unwrap_or(Some(Truth::Known(true)));
    let truth = and_maybe(compiles, main).unwrap_or(Truth::Known(true));
    match truth {
        Truth::Depends(condition) => model::simplify(&condition, None),
        known => known,
    }
}

/// Both, where false wins over unknown (None).
fn and_maybe(left: Option<Truth>, right: Option<Truth>) -> Option<Truth> {
    match (left, right) {
        (Some(Truth::Known(false)), _) | (_, Some(Truth::Known(false))) => Some(Truth::Known(false)),
        (Some(left), Some(right)) => Some(left.and(right)),
        _ => None,
    }
}

/// Either, where true wins over unknown (None).
fn or_maybe(left: Option<Truth>, right: Option<Truth>) -> Option<Truth> {
    match (left, right) {
        (Some(Truth::Known(true)), _) | (_, Some(Truth::Known(true))) => Some(Truth::Known(true)),
        (Some(left), Some(right)) => Some(left.or(right)),
        _ => None,
    }
}

// Platform and compiler macros as conditions; the macros of platforms Anvil does not build for are never defined
const MACRO_CONDITIONS: &[(&str, &str)] = &[
    ("_WIN32", "windows"),
    ("_WIN64", "windows & (x64 | arm64)"),
    ("WIN32", "windows"),
    ("__WIN32__", "windows"),
    ("_MSC_VER", "msvc"),
    ("__GNUC__", "gnu"),
    ("__clang__", "clang"),
    ("__MINGW32__", "mingw"),
    ("__MINGW64__", "mingw & x64"),
    ("__linux__", "linux"),
    ("__linux", "linux"),
    ("__APPLE__", "macos"),
    ("__MACH__", "macos"),
    ("__unix__", "unix & !macos"),
    ("__unix", "unix & !macos"),
    ("__x86_64__", "x64"),
    ("__x86_64", "x64"),
    ("__amd64__", "x64"),
    ("_M_X64", "x64"),
    ("_M_AMD64", "x64"),
    ("__i386__", "x86"),
    ("_M_IX86", "x86"),
    ("__aarch64__", "arm64"),
    ("__arm64__", "arm64"),
    ("_M_ARM64", "arm64"),
    ("__arm__", "arm"),
    ("_M_ARM", "arm"),
];
const OTHER_PLATFORM_MACROS: &[&str] = &[
    "__EMSCRIPTEN__",
    "__wasm__",
    "__wasm32__",
    "__wasm64__",
    "__s390x__",
    "__s390__",
    "__powerpc__",
    "__powerpc64__",
    "__ppc__",
    "__ppc64__",
    "__PPC64__",
    "__riscv",
    "__mips__",
    "__sparc__",
    "__ia64__",
    "__loongarch__",
    "__hppa__",
    "__alpha__",
    "__m68k__",
    "__FreeBSD__",
    "__OpenBSD__",
    "__NetBSD__",
    "__DragonFly__",
    "__ANDROID__",
    "__CYGWIN__",
    "__sun",
    "__HAIKU__",
    "__QNX__",
    "__OS400__",
    "__MVS__",
    "__VMS",
    "__INTEL_COMPILER",
];

// A token of a `#if` expression
static PREPROCESSOR_TOKEN: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^([A-Za-z_][A-Za-z0-9_]*|\d+[A-Za-z]*|&&|\|\||[<>=!]=|[<>()!])").unwrap());

/// Where a macro is defined, from MACRO_CONDITIONS; None for other macros.
fn macro_defined(name: &str) -> Option<Truth> {
    let name = name.trim();
    if OTHER_PLATFORM_MACROS.contains(&name) {
        return Some(Truth::Known(false));
    }
    MACRO_CONDITIONS
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, condition)| Truth::Depends(condition.to_string()))
}

/// A `#if` expression of `defined`, `!`, `&&`, `||`, numbers and platform macros (compared with a number as for a
/// current compiler: `__GNUC__ >= 4` holds where the macro is defined); None when it tests anything else.
fn macro_expression(text: &str) -> Option<Truth> {
    let mut tokens = Vec::new();
    let mut rest = text.trim();
    while !rest.is_empty() {
        let found = PREPROCESSOR_TOKEN.find(rest)?;
        tokens.push(found.as_str());
        rest = rest[found.end()..].trim_start();
    }

    let mut position = 0;
    let truth = macro_or(&tokens, &mut position)?;
    (position == tokens.len()).then_some(truth)
}

fn macro_or(tokens: &[&str], position: &mut usize) -> Option<Truth> {
    let mut truth = macro_and(tokens, position)?;
    while tokens.get(*position) == Some(&"||") {
        *position += 1;
        truth = truth.or(macro_and(tokens, position)?);
    }
    Some(truth)
}

fn macro_and(tokens: &[&str], position: &mut usize) -> Option<Truth> {
    let mut truth = macro_unary(tokens, position)?;
    while tokens.get(*position) == Some(&"&&") {
        *position += 1;
        truth = truth.and(macro_unary(tokens, position)?);
    }
    Some(truth)
}

fn macro_unary(tokens: &[&str], position: &mut usize) -> Option<Truth> {
    let token = *tokens.get(*position)?;
    *position += 1;
    match token {
        "!" => macro_unary(tokens, position).map(Truth::not),
        "(" => {
            let truth = macro_or(tokens, position)?;
            (tokens.get(*position) == Some(&")")).then(|| *position += 1)?;
            Some(truth)
        }
        "defined" => {
            let parenthesized = tokens.get(*position) == Some(&"(");
            let name = tokens.get(*position + usize::from(parenthesized))?;
            *position += 1 + 2 * usize::from(parenthesized);
            if parenthesized && tokens.get(*position - 1) != Some(&")") {
                return None;
            }
            macro_defined(name)
        }
        number if number.starts_with(|character: char| character.is_ascii_digit()) => Some(Truth::Known(
            number.trim_end_matches(|character: char| character.is_ascii_alphabetic()) != "0",
        )),
        name => {
            let defined = macro_defined(name)?;
            let comparison = tokens
                .get(*position)
                .copied()
                .filter(|token| ["<", "<=", ">", ">=", "==", "!="].contains(token));
            let Some(comparison) = comparison else { return Some(defined) };
            tokens
                .get(*position + 1)
                .filter(|value| value.starts_with(|character: char| character.is_ascii_digit()))?;
            *position += 2;
            match comparison {
                "<" | "<=" => Some(Truth::Known(false)),
                ">" | ">=" | "!=" => Some(defined),
                _ => None,
            }
        }
    }
}

/// Adds a condition to a list unless it holds the same or its negation (`msvc` and `!msvc` are one choice).
fn push_distinct(conditions: &mut Vec<String>, condition: &str) {
    if !conditions.iter().any(|known| chosen_as(known, condition).is_some()) {
        conditions.push(condition.to_string());
    }
}

/// Is `condition` `known` (true) or its negation (false), or neither (None)?
fn chosen_as(known: &str, condition: &str) -> Option<bool> {
    if model::equivalent(known, condition) {
        Some(true)
    } else if model::equivalent(&model::negate(known), condition) {
        Some(false)
    } else {
        None
    }
}

/// What `render` gives under every choice of `conditions` (each true or false), the choices with the same text
/// merged into one condition; choices that cannot happen (`windows` with `unix`, or outside the `enclosing` condition
/// the text is written under) are left out, and the conditions say only what the enclosing one does not.
fn texts_by_choice(conditions: &[String], enclosing: Option<&str>, render: &ChoiceRender) -> Vec<(Truth, String)> {
    let simplified = |truth: Truth| match truth {
        Truth::Depends(condition) => match model::simplify(
            &model::and_optional(&enclosing.map(str::to_string), &Some(condition.clone())).unwrap_or_default(),
            None,
        ) {
            Truth::Known(false) => Truth::Known(false),
            _ => model::simplify(&condition, enclosing),
        },
        known => known,
    };
    let mut texts: Vec<(Truth, String)> = Vec::new();

    for assignment in 0u32..(1 << conditions.len()) {
        let chosen = |condition: &str| {
            conditions.iter().enumerate().find_map(|(index, known)| {
                let same = chosen_as(known, condition)?;
                Some((assignment & (1 << index) != 0) == same)
            })
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

/// The folders of the MSVC tools (link.exe's, and the rest of the toolchain's PATH), when MSVC is installed.
fn msvc_tool_folders() -> Vec<PathBuf> {
    let platform = crate::platform::Platform::host();
    let Ok(toolchain) = crate::toolchain::Toolchain::find_family(crate::toolchain::Family::Msvc, &platform) else {
        return Vec::new();
    };

    let mut folders: Vec<PathBuf> = toolchain.linker.parent().map(Path::to_path_buf).into_iter().collect();
    let path = toolchain.env.iter().find(|(name, _)| name.eq_ignore_ascii_case("PATH"));
    if let Some((_, path)) = path {
        folders.extend(std::env::split_paths(path));
    }
    folders
}

/// Does this machine's pkg-config (PKG_CONFIG, else pkg-config or pkgconf on PATH) know every module, or with
/// `any_one` one of them? Each spec as pkg_check_modules() takes it: `name`, or `name>=version` and the like.
fn pkg_config_knows(modules: &[String], any_one: bool) -> bool {
    let program = std::env::var("PKG_CONFIG")
        .ok()
        .filter(|value| !value.is_empty())
        .and_then(|configured| util::which(&configured))
        .or_else(|| util::which("pkg-config"))
        .or_else(|| util::which("pkgconf"));
    let Some(program) = program else { return false };

    let knows = |spec: &String| {
        // pkg-config takes a version requirement as words of its own
        let words: Vec<&str> = match spec.find(['<', '>', '=']) {
            Some(start) => {
                let operator_end = spec[start..]
                    .find(|character| !"<>=".contains(character))
                    .map_or(spec.len(), |end| start + end);
                vec![&spec[..start], &spec[start..operator_end], &spec[operator_end..]]
            }
            None => vec![spec.as_str()],
        };
        std::process::Command::new(&program)
            .arg("--exists")
            .args(words.iter().map(|word| word.trim()).filter(|word| !word.is_empty()))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    if any_one {
        modules.iter().any(knows)
    } else {
        !modules.is_empty() && modules.iter().all(knows)
    }
}

/// The ports a vcpkg port needs by default: its dependencies and its default features'.
fn port_dependencies(vcpkg_root: &Path, port: &str) -> Vec<String> {
    let manifest = std::fs::read_to_string(vcpkg_root.join("ports").join(port).join("vcpkg.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    let Some(manifest) = manifest else { return Vec::new() };

    let mut names = crate::vcpkg::dependency_names(manifest.get("dependencies"));
    let default_features = manifest
        .get("default-features")
        .and_then(|features| features.as_array())
        .cloned()
        .unwrap_or_default();
    for feature in default_features {
        let feature = feature.as_str().or_else(|| feature.get("name").and_then(|name| name.as_str()));
        let dependencies = feature.and_then(|feature| manifest.get("features")?.get(feature)?.get("dependencies"));
        names.extend(crate::vcpkg::dependency_names(dependencies));
    }
    names
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
    let resolved = if index < 0 { (length as i64).checked_add(index)? } else { index };

    (0..length as i64).contains(&resolved).then_some(resolved as usize)
}

// Above this many options, list() does not try every choice of them and says the result may change
const MAX_TRIED_OPTIONS: usize = 6;

// The longest text string(REPEAT) makes
const MAX_REPEATED_LENGTH: usize = 1 << 24;

/// The text of a list item: a conditional item's inside.
fn item_text(item: &str) -> &str {
    model::split_conditional_item(item).map_or(item, |(_, text)| text)
}

/// The positions of a list's items this machine has with `options`: plain ones, and conditional ones whose condition
/// holds.
fn visible_items(items: &[String], options: &HashMap<String, bool>) -> Vec<usize> {
    let lookup = |name: &str| options.get(name).copied();
    (0..items.len())
        .filter(|index| match model::split_conditional_item(&items[*index]) {
            Some((condition, _)) => model::holds_on_host(&condition, &lookup),
            None => true,
        })
        .collect()
}

/// A list as this machine has it with `options`: conditional items where their condition holds, as plain items.
fn resolved_items(items: &[String], options: &HashMap<String, bool>) -> Vec<String> {
    visible_items(items, options)
        .into_iter()
        .map(|index| item_text(&items[index]).to_string())
        .collect()
}

/// The conditions under which items are there that change what `read` gives for the list, or None when its
/// versions all read the same (or are too many to tell).
fn platforms_changing(items: &[String], read: &ListRead) -> Option<String> {
    let versions = super::interp::text_versions(&items.join(";"))?;
    let results: Vec<Option<(String, String)>> = versions.iter().map(|(_, text)| read(&model::list_elements(text))).collect();
    if results.windows(2).all(|pair| pair[0] == pair[1]) {
        return None;
    }
    let mut conditions: Vec<String> = Vec::new();
    for (condition, _) in items.iter().filter_map(|item| model::split_conditional_item(item)) {
        if !conditions.contains(&condition) {
            conditions.push(condition);
        }
    }
    Some(conditions.join(", "))
}

/// list(LENGTH|GET|JOIN|SUBLIST|FIND) of `items`: the output variable and its value.
fn list_read(operation: &str, items: &[String], rest: &[String]) -> Option<(String, String)> {
    match operation {
        "LENGTH" => Some((rest.first()?.clone(), items.len().to_string())),
        "GET" => {
            let (output, indexes) = rest.split_last()?;
            let picked: Vec<&str> = indexes
                .iter()
                .filter_map(|index| list_index(index, items.len()))
                .map(|index| items[index].as_str())
                .collect();
            Some((output.clone(), picked.join(";")))
        }
        "JOIN" => {
            let [glue, output, ..] = rest else { return None };
            Some((output.clone(), items.join(glue)))
        }
        "SUBLIST" => {
            let [begin, length, output, ..] = rest else { return None };
            let (begin, end) = substring_range(begin, length, items.len());
            Some((output.clone(), items[begin..end].join(";")))
        }
        "FIND" => {
            let [value, output, ..] = rest else { return None };
            let index = items.iter().position(|item| item == value).map_or(-1, |index| index as i64);
            Some((output.clone(), index.to_string()))
        }
        _ => None,
    }
}

/// The variable a string() operation writes, for those that compute it from their other arguments.
fn string_output<'a>(operation: &str, rest: &'a [String]) -> Option<&'a str> {
    let index = match operation {
        "APPEND" | "PREPEND" | "CONCAT" => 0,
        "MD5" | "SHA1" | "SHA224" | "SHA256" | "SHA384" | "SHA512" | "SHA3_224" | "SHA3_256" | "SHA3_384" | "SHA3_512" => 0,
        "JOIN" | "TOLOWER" | "TOUPPER" | "LENGTH" | "STRIP" | "GENEX_STRIP" | "MAKE_C_IDENTIFIER" | "HEX" => 1,
        "REPEAT" | "REPLACE" | "FIND" => 2,
        "SUBSTRING" | "COMPARE" => 3,
        "REGEX" => match rest.first().map(String::as_str) {
            Some("MATCH" | "MATCHALL") => 2,
            Some("REPLACE") => 3,
            _ => return None,
        },
        _ => return None,
    };
    rest.get(index).map(String::as_str)
}

/// string(SUBSTRING) and list(SUBLIST)'s begin and length (-1: to the end) as a range within `length` items.
fn substring_range(begin: &str, length: &str, total: usize) -> (usize, usize) {
    let begin: usize = begin.parse().unwrap_or(0).min(total);
    let length: i64 = length.parse().unwrap_or(-1);
    let end = match usize::try_from(length) {
        Ok(length) => begin.saturating_add(length).min(total),
        Err(_) => total,
    };
    (begin, end)
}

/// The names a condition tests.
fn condition_atoms(condition: &str) -> Vec<String> {
    fn collect(condition: &crate::cond::Cond, atoms: &mut Vec<String>) {
        use crate::cond::Cond;
        match condition {
            Cond::Atom(name) => {
                if !atoms.contains(name) {
                    atoms.push(name.clone());
                }
            }
            Cond::Not(inner) => collect(inner, atoms),
            Cond::And(left, right) | Cond::Or(left, right) => {
                collect(left, atoms);
                collect(right, atoms);
            }
        }
    }

    let mut atoms = Vec::new();
    if let Ok(parsed) = crate::cond::parse(condition) {
        collect(&parsed, &mut atoms);
    }
    atoms
}

/// Text in upper or lower case, except Anvil's placeholders (`{out}`, `{config}`, `{hash:...}`).
fn change_case_keeping_placeholders(text: &str, upper: bool) -> String {
    let mut output = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        let (before, from_brace) = rest.split_at(start);
        output.push_str(&ascii_case(before, upper));
        let placeholder_end = from_brace[1..]
            .starts_with(|character: char| character.is_ascii_lowercase())
            .then(|| from_brace.find('}'))
            .flatten();
        match placeholder_end {
            Some(end) => {
                output.push_str(&from_brace[..=end]);
                rest = &from_brace[end + 1..];
            }
            None => {
                output.push('{');
                rest = &from_brace[1..];
            }
        }
    }
    output.push_str(&ascii_case(rest, upper));
    output
}

/// CMake changes the case of ASCII letters only.
fn ascii_case(text: &str, upper: bool) -> String {
    if upper {
        text.to_ascii_uppercase()
    } else {
        text.to_ascii_lowercase()
    }
}

/// A CMake regex replacement (`\1`) in the regex crate's syntax (`${1}`).
fn cmake_replacement(replacement: &str) -> String {
    let escaped = replacement.replace('$', "$$");
    REPLACEMENT_GROUP.replace_all(&escaped, "$${$1}").into_owned()
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
    let rest = absolute.strip_prefix(base.as_str()).unwrap_or(absolute).trim_start_matches('/');
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
        "-" => math_unary(tokens, position).map(i64::wrapping_neg),
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

    /// Runs a project made of the given files (CMakeLists.txt first) in a fresh temporary folder.
    fn run(name: &str, files: &[(&str, &str)]) -> Interpreter {
        let folder = std::env::temp_dir().join(format!("anvil-builtins-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        for (path, text) in files {
            let file = folder.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        }

        let mut interpreter = Interpreter::new(&folder);
        interpreter.run_root().unwrap();
        let _ = std::fs::remove_dir_all(&folder);
        interpreter
    }

    fn variable(interpreter: &Interpreter, name: &str) -> String {
        interpreter.get_variable(name).unwrap_or_default()
    }

    /// The configured file's text written where `condition` holds.
    fn written(interpreter: &Interpreter, condition: Option<&str>) -> String {
        let write = interpreter
            .configure_writes
            .iter()
            .find(|write| write.condition.as_deref() == condition)
            .unwrap_or_else(|| panic!("no write under {condition:?}: {:?}", interpreter.configure_writes));
        write.text.clone().unwrap()
    }

    #[test]
    fn an_option_declared_in_a_branch_exists_only_there() {
        let cmake = r#"
include(CMakeDependentOption)
option(OPT "o" OFF)
if(UNIX)
  option(USE_X11 "x11" ON)
  cmake_dependent_option(USE_XI "xi" ON "OPT" OFF)
endif()
if(WIN32)
  option(BOTH "b" OFF)
else()
  option(BOTH "b" ON)
endif()
"#;
        let interpreter = run("option-branch", &[("CMakeLists.txt", cmake)]);

        assert_eq!(interpreter.option_truth("USE_X11"), Some(Truth::Depends("USE_X11 & unix".into())));
        assert_eq!(interpreter.option_truth("USE_XI"), Some(Truth::Depends("USE_XI & (unix & OPT)".into())));
        assert_eq!(interpreter.option_truth("BOTH"), Some(Truth::Depends("BOTH".into())));
        assert_eq!(interpreter.option_truth("OPT"), Some(Truth::Depends("OPT".into())));
    }

    #[test]
    fn type_sizes_that_differ_by_platform_become_if_blocks() {
        let cmake = r#"
project(sizes C)
include(CheckTypeSize)
check_type_size("long" SIZEOF_LONG)
check_type_size("unsigned  long" SIZEOF_ULONG)
check_type_size(wchar_t SIZEOF_WCHAR_T)
check_type_size("char *" SIZEOF_CHAR_P)
check_type_size(int SIZEOF_INT)
check_type_size("struct thing" SIZEOF_THING)
configure_file(config.h.in config.h)
"#;
        let template = "#define SIZEOF_LONG @SIZEOF_LONG@\n#cmakedefine SIZEOF_WCHAR_T @SIZEOF_WCHAR_T@\n#define SIZEOF_INT ${SIZEOF_INT}\n";
        let interpreter = run("type-sizes", &[("CMakeLists.txt", cmake), ("config.h.in", template)]);

        // One size per platform where it differs, this machine's among them
        let host_long = if cfg!(windows) { "4" } else { "8" };
        let long = variable(&interpreter, "SIZEOF_LONG");
        assert_eq!(model::list_elements(&long).len(), 2, "{long}");
        assert_eq!(interpreter.host_text(&long), host_long);
        assert_eq!(interpreter.host_text(&variable(&interpreter, "SIZEOF_ULONG")), host_long);
        assert_eq!(interpreter.host_text(&variable(&interpreter, "SIZEOF_CHAR_P")), "8");
        assert_eq!(variable(&interpreter, "SIZEOF_INT"), "4");

        let text = written(&interpreter, None);
        assert!(
            text.contains("#define SIZEOF_LONG 4\n") && text.contains("#define SIZEOF_LONG 8\n"),
            "{text}"
        );
        assert!(
            text.contains("#define SIZEOF_WCHAR_T 2\n") && text.contains("#define SIZEOF_WCHAR_T 4\n"),
            "{text}"
        );
        assert!(text.contains("#if !defined(_WIN32)") || text.contains("#if defined(_WIN32)"), "{text}");
        assert!(text.contains("#define SIZEOF_INT 4\n"), "{text}");
        assert!(!text.contains("SIZEOF_LONG \n"), "{text}");

        assert!(interpreter
            .warnings
            .iter()
            .any(|warning| warning.contains("check_type_size(struct thing)")));
    }

    #[test]
    fn type_size_table() {
        let long_double = type_size("long double").unwrap();
        assert_eq!(long_double.otherwise, "16");
        assert_eq!(long_double.cases[0], ("msvc | (macos & arm64)".to_string(), "8".to_string()));
        assert_eq!(type_size("ssize_t").unwrap().cases[0], ("msvc".to_string(), String::new()));
        assert_eq!(type_size("int *").unwrap().otherwise, "8");
        assert!(type_size("time_t").is_some() && type_size("off_t").is_some() && type_size("size_t").is_some());
        assert!(type_size("struct stat").is_none());
    }

    #[test]
    fn a_value_a_branch_sets_is_configured_where_it_runs() {
        let cmake = r#"
project(values C)
option(USE_FOO "foo" OFF)
if(USE_FOO)
  set(FOO_VALUE 42)
endif()
configure_file(config.h.in config.h)
"#;
        let template = "#define FOO_VALUE \"@FOO_VALUE@\"\n#cmakedefine FOO_VALUE\n";
        let interpreter = run("branch-value", &[("CMakeLists.txt", cmake), ("config.h.in", template)]);

        assert_eq!(written(&interpreter, Some("USE_FOO")), "#define FOO_VALUE \"42\"\n#define FOO_VALUE\n");
        assert_eq!(
            written(&interpreter, Some("!USE_FOO")),
            "#define FOO_VALUE \"\"\n/* #undef FOO_VALUE */\n"
        );
    }

    #[test]
    fn lists_count_the_items_this_machine_has() {
        let cmake = r#"
option(WITH_EXTRA "extra" OFF)
set(TOOLS one)
if(WITH_EXTRA)
  list(APPEND TOOLS two)
endif()
if(UNIX)
  list(APPEND TOOLS three)
endif()
list(APPEND TOOLS four)
list(LENGTH TOOLS LEN)
list(GET TOOLS -1 LAST)
list(GET TOOLS 1 SECOND)
list(FIND TOOLS four WHERE)
list(JOIN TOOLS "," JOINED)
set(COPY ${TOOLS})
list(REMOVE_ITEM COPY two)
list(TRANSFORM TOOLS APPEND .c OUTPUT_VARIABLE FILES)
"#;
        let interpreter = run("list-items", &[("CMakeLists.txt", cmake)]);

        let (length, second, joined) = if cfg!(unix) {
            ("3", "three", "one,three,four")
        } else {
            ("2", "four", "one,four")
        };
        assert_eq!(variable(&interpreter, "LEN"), length);
        assert_eq!(variable(&interpreter, "LAST"), "four");
        assert_eq!(variable(&interpreter, "SECOND"), second);
        assert_eq!(variable(&interpreter, "WHERE"), if cfg!(unix) { "2" } else { "1" });
        assert_eq!(variable(&interpreter, "JOINED"), joined);
        assert!(!variable(&interpreter, "COPY").contains("two"));
        let files = model::list_elements(&variable(&interpreter, "FILES"));
        assert_eq!(files[0], "one.c");
        assert_eq!(
            model::split_conditional_item(&files[1]).map(|(condition, text)| (condition, text.to_string())),
            Some(("WITH_EXTRA".into(), "two.c".into()))
        );

        // LENGTH, SECOND (on Windows) and JOINED change with WITH_EXTRA; LAST and WHERE (on Windows) do not
        let warned: Vec<&String> = interpreter
            .warnings
            .iter()
            .filter(|warning| warning.contains("options add (WITH_EXTRA)"))
            .collect();
        assert!(warned.iter().any(|warning| warning.contains("list(LENGTH TOOLS)")), "{warned:?}");
        assert!(
            !warned
                .iter()
                .any(|warning| warning.contains("list(GET TOOLS)") && warning.contains(":12:")),
            "{warned:?}"
        );
    }

    #[test]
    fn list_reads() {
        let items = vec!["a".to_string(), model::conditional_item("OPT", "b"), model::conditional_item("!OPT", "c")];
        let options: HashMap<String, bool> = [("OPT".to_string(), true)].into();
        assert_eq!(resolved_items(&items, &options), vec!["a", "b"]);
        assert_eq!(visible_items(&items, &HashMap::new()), vec![0, 2]);

        let words = |text: &str| text.split(' ').map(String::from).collect::<Vec<String>>();
        let plain = words("x y z");
        assert_eq!(
            list_read("SUBLIST", &plain, &words("1 9223372036854775807 out")),
            Some(("out".into(), "y;z".into()))
        );
        assert_eq!(list_read("SUBLIST", &plain, &words("5 -1 out")), Some(("out".into(), String::new())));
        assert_eq!(
            list_read("GET", &plain, &words("-9223372036854775808 out")),
            Some(("out".into(), String::new()))
        );
        assert_eq!(list_index("-1", 3), Some(2));
    }

    #[test]
    fn check_tables_match_cmake() {
        // What check_c_compiler_flag() gives with cl 19.5 and MinGW GCC 15
        let depends = |condition: &str| Truth::Depends(condition.into());
        for (flags, truth) in [
            ("-Wall", Truth::Known(true)),
            ("/W4", depends("msvc")),
            ("-W4", depends("msvc")),
            ("-EHsc -Zc:__cplusplus -wd4996 -GR -permissive-", depends("msvc")),
            ("-Od", depends("msvc")),
            ("-O2", depends("!msvc")),
            ("/O2", Truth::Known(false)),
            ("/Za /d2Something", depends("msvc")),
            ("-Wextra", depends("!msvc")),
            ("-fPIC", depends("!msvc")),
            ("-DX=1 -Iinclude -std=c11 -MD -w", Truth::Known(true)),
            ("LINKER:--as-needed", depends("!msvc")),
            ("-Wl,--gc-sections", depends("!msvc")),
            ("-fsanitize=address", depends("!mingw")),
            // Clang's warnings, GCC's that Clang lacks, one language's, and flags nobody takes
            ("-Wcomma", depends("clang & !msvc")),
            ("-Wmissing-variable-declarations -Wused-but-marked-unused", depends("clang & !msvc")),
            ("-Wlogical-op", depends("gcc")),
            ("-Wimplicit-fallthrough=3", depends("gcc")),
            ("-Wno-unknown-warning-of-clang", depends("!msvc")),
            ("-Werror=format-security", depends("!msvc")),
            ("-no-such-thing", Truth::Known(false)),
            ("-fcolor-diagnostics", depends("clang & !msvc")),
            ("-stdlib=libc++", depends("clang & !msvc")),
        ] {
            assert_eq!(flags_truth(flags, None), truth, "{flags}");
        }
        assert_eq!(flags_truth("-Wnon-virtual-dtor", Some("C")), depends("clang & !msvc"));
        assert_eq!(flags_truth("-Wnon-virtual-dtor", Some("CXX")), depends("!msvc"));
        assert_eq!(flags_truth("-Wstrict-prototypes", Some("C")), depends("!msvc"));
        assert_eq!(unknown_flags("-Wall -no-such-thing"), vec!["-no-such-thing"]);

        assert_eq!(names_truth(&["intrin.h".into()]), depends("windows"));
        assert_eq!(names_truth(&["sys/file.h".into()]), depends("!msvc"));
        assert_eq!(library_truth("m"), depends("!msvc"));
        assert_eq!(library_truth("pthread"), depends("!msvc"));
        assert_eq!(library_truth("dl"), depends("!windows"));
    }

    #[test]
    fn check_sources_follow_their_platform_tests() {
        let depends = |condition: &str| Truth::Depends(condition.into());
        for (source, truth) in [
            // brotli: main() only for Emscripten
            ("#if defined(__EMSCRIPTEN__)\n   int main() {return 0;}\n   #endif", Truth::Known(false)),
            // zlib: an #error off s390x
            ("#ifndef __s390x__\n    #error\n#endif\nint main() {return 0;}", Truth::Known(false)),
            ("#ifdef _WIN32\n#error no\n#endif\nint main(void) { return 0; }", depends("!windows")),
            (
                "#if defined(__GNUC__) && __GNUC__ >= 4\nint main() {}\n#else\n#error old\n#endif",
                depends("gnu"),
            ),
            (
                "#if !defined(_MSC_VER) || defined(__clang__)\n#error\n#endif\nint main() {}",
                depends("msvc & !clang"),
            ),
            (
                "#ifdef _WIN32\n#include <windows.h>\n#else\n#include <unistd.h>\n#endif\nint main() {}",
                Truth::Known(true),
            ),
            ("#if __has_include(<x.h>)\n#error\n#endif\nint main() {}", Truth::Known(true)),
            ("#if 0\nint main() {}\n#endif", Truth::Known(false)),
        ] {
            assert_eq!(source_truth(source, false), truth, "{source}");
        }

        // zlib's visibility check made with -Werror: GCC warns that it ignores the attribute on a static function,
        // MinGW's GCC on any
        let visibility = "static void f(void) __attribute__ ((visibility(\"hidden\")));\nint main(void) {return 0;}";
        assert_eq!(source_truth(visibility, false), depends("!msvc"));
        assert_eq!(source_truth(visibility, true), depends("!msvc & !gcc"));
        let exported = "void f(void) __attribute__ ((visibility(\"default\")));\nint main(void) {return 0;}";
        assert_eq!(source_truth(exported, true), depends("!msvc & !mingw"));
    }

    #[test]
    fn absent_optional_packages_and_programs_are_said() {
        let cmake = r#"
find_package(AnvilNoSuchPackage)
find_program(NO_TOOL anvil-no-such-program)
if(NOT NO_TOOL)
  set(SKIPPED ON)
endif()
"#;
        let interpreter = run("absent", &[("CMakeLists.txt", cmake)]);

        assert_eq!(variable(&interpreter, "AnvilNoSuchPackage_FOUND"), "OFF");
        assert_eq!(variable(&interpreter, "SKIPPED"), "ON");
        assert!(interpreter
            .warnings
            .iter()
            .any(|warning| warning.contains("find_package(AnvilNoSuchPackage) is optional")));
        assert!(interpreter
            .warnings
            .iter()
            .any(|warning| warning.contains("find_program(NO_TOOL anvil-no-such-program)")));
    }

    #[test]
    fn found_programs_and_modules_are_the_ones_this_machine_has() {
        let cmake = r#"
find_program(TOOL NAMES anvil-no-such-program anvil-test-tool PATHS ${CMAKE_CURRENT_SOURCE_DIR}/tools)
pkg_check_modules(NOPE anvil-no-such-module>=1.0)
if(NOPE_FOUND)
  set(USES_NOPE ON)
endif()
pkg_check_modules(NEEDED REQUIRED anvil-no-such-module)
"#;
        let interpreter = run("found", &[("CMakeLists.txt", cmake), ("tools/anvil-test-tool", "")]);

        // The name found, as a path where only PATHS has it
        assert!(
            variable(&interpreter, "TOOL").ends_with("/tools/anvil-test-tool"),
            "{}",
            variable(&interpreter, "TOOL")
        );

        // An optional module pkg-config does not know is not found; a required one is
        assert_eq!(variable(&interpreter, "NOPE_FOUND"), "");
        assert_eq!(variable(&interpreter, "USES_NOPE"), "");
        assert!(!interpreter.pkg_config.contains_key("NOPE"));
        assert!(interpreter
            .warnings
            .iter()
            .any(|warning| warning.contains("pkg_check_modules(NOPE anvil-no-such-module>=1.0)")));
        assert_eq!(variable(&interpreter, "NEEDED_FOUND"), "ON");
    }

    #[test]
    fn threads_variables_are_per_platform() {
        let cmake = r#"
find_package(Threads REQUIRED)
if(CMAKE_USE_PTHREADS_INIT)
  set(PTHREADS ON)
endif()
if(CMAKE_USE_WIN32_THREADS_INIT)
  set(WIN32_THREADS ON)
endif()
"#;
        let interpreter = run("threads", &[("CMakeLists.txt", cmake)]);

        assert_eq!(model::marked_condition(&variable(&interpreter, "CMAKE_USE_PTHREADS_INIT")), Some("!msvc"));
        assert_eq!(variable(&interpreter, "PTHREADS"), model::conditional_item("!msvc", "ON"));
        assert_eq!(variable(&interpreter, "WIN32_THREADS"), model::conditional_item("windows", "ON"));
    }

    #[test]
    fn check_results_are_seen_outside_the_function_that_checks() {
        let cmake = r#"
include(CheckCCompilerFlag)
function(enable_flag flag variable)
  check_c_compiler_flag(${flag} ${variable})
endfunction()
enable_flag(-Wa,--noexecstack HAS_NOEXECSTACK)
"#;
        let interpreter = run("check-cache", &[("CMakeLists.txt", cmake)]);

        assert_eq!(model::marked_condition(&variable(&interpreter, "HAS_NOEXECSTACK")), Some("!msvc"));
    }

    #[test]
    fn clang_only_flags_pass_only_with_clang() {
        assert_eq!(
            flag_truth("-mbranches-within-32B-boundaries", Some("C")),
            Some(Truth::Depends("clang & !msvc".into()))
        );
        assert_eq!(flag_truth("-mavx2", Some("C")), Some(Truth::Depends("!msvc".into())));
    }

    #[test]
    fn required_flags_of_this_machines_branch_do_not_rule_out_the_others() {
        let cmake = r#"
include(CheckCSourceCompiles)
if(MSVC)
  set(CMAKE_REQUIRED_FLAGS "-WX")
else()
  set(CMAKE_REQUIRED_FLAGS "-Werror")
endif()
check_c_source_compiles("static void f(void) __attribute__((visibility(\"hidden\"))); int main(void) { return 0; }" HAVE_HIDDEN)
check_c_source_compiles("int main(void) { return 0; }" HAVE_PLAIN)
set(CMAKE_REQUIRED_FLAGS "-mavx2")
check_c_source_compiles("int main(void) { return 0; }" HAVE_AVX2_FLAG)
"#;
        let interpreter = run("required-flags", &[("CMakeLists.txt", cmake)]);

        assert_eq!(model::marked_condition(&variable(&interpreter, "HAVE_HIDDEN")), Some("!msvc & !gcc"));
        assert_eq!(variable(&interpreter, "HAVE_PLAIN"), "1");
        if cfg!(windows) {
            assert_eq!(model::marked_condition(&variable(&interpreter, "HAVE_AVX2_FLAG")), Some("!msvc"));
        }
    }

    #[test]
    fn port_dependencies_include_default_features() {
        let root = std::env::temp_dir().join(format!("anvil-builtins-ports-{}", std::process::id()));
        let port = root.join("ports/harfbuzz");
        std::fs::create_dir_all(&port).unwrap();
        let manifest = r#"{ "name": "harfbuzz", "dependencies": [{ "name": "vcpkg-tool-meson", "host": true }],
            "default-features": ["freetype"],
            "features": { "freetype": { "description": "", "dependencies": [{ "name": "freetype", "default-features": false }] },
                          "cairo": { "description": "", "dependencies": ["cairo"] } } }"#;
        std::fs::write(port.join("vcpkg.json"), manifest).unwrap();

        let dependencies = port_dependencies(&root, "harfbuzz");
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(dependencies, vec!["vcpkg-tool-meson", "freetype"]);
    }

    #[test]
    fn huge_offsets_and_counts_do_not_panic() {
        let cmake = r#"
file(READ data.txt WHOLE OFFSET 2 LIMIT 18446744073709551615)
string(SUBSTRING "abcdef" 2 9223372036854775807 SUB)
set(L a b c)
list(SUBLIST L 1 9223372036854775807 SUBL)
list(TRANSFORM L APPEND x FOR 0 18446744073709551615 OUTPUT_VARIABLE T)
string(REPEAT "ab" 99999999999 R)
math(EXPR M "-(1 << 63)")
file(GLOB G "")
"#;
        let interpreter = run("huge", &[("CMakeLists.txt", cmake), ("data.txt", "0123456789")]);

        assert_eq!(variable(&interpreter, "WHOLE"), "23456789");
        assert_eq!(variable(&interpreter, "SUB"), "cdef");
        assert_eq!(variable(&interpreter, "SUBL"), "b;c");
        assert_eq!(variable(&interpreter, "T"), "ax;bx;cx");
        assert_eq!(variable(&interpreter, "R"), "");
        assert_eq!(variable(&interpreter, "M"), i64::MIN.to_string());
    }

    #[test]
    fn case_changes_of_platform_values_are_this_machines() {
        let cmake = r#"
string(TOLOWER "${CMAKE_C_COMPILER_ID}" CID)
string(TOUPPER "${CMAKE_BUILD_TYPE}" BT)
string(TOUPPER "{out}/gen-${CMAKE_SYSTEM_NAME}" OUT)
set(IDS "${CMAKE_CXX_COMPILER_ID}")
list(TRANSFORM IDS TOUPPER)
string(TOUPPER "abc" PLAIN)
"#;
        let interpreter = run("case", &[("CMakeLists.txt", cmake)]);

        let (id, system) = if cfg!(windows) { ("msvc", "WINDOWS") } else { ("gnu", "LINUX") };
        assert_eq!(variable(&interpreter, "CID"), id);
        assert_eq!(variable(&interpreter, "BT"), "{config}");
        assert_eq!(variable(&interpreter, "OUT"), format!("{{out}}/GEN-{system}"));
        assert_eq!(variable(&interpreter, "IDS"), id.to_uppercase());
        assert_eq!(variable(&interpreter, "PLAIN"), "ABC");
        let warned = interpreter
            .warnings
            .iter()
            .filter(|warning| warning.contains("this machine's") || warning.contains("{config}"))
            .count();
        // A platform value inside longer text is this machine's already when expanded
        assert_eq!(warned, 3, "{:?}", interpreter.warnings);
    }
}
