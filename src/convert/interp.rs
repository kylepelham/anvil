//! A CMake interpreter for conversion: variables, scopes, if/foreach/while, functions and macros run as CMake would,
//! while platform, compiler, build-type and option tests stay symbolic. Branches whose outcome depends on them all
//! run, under the matching condition, so the result describes every platform instead of this machine.

use super::model::{self, CustomCommand, ExportHeader, Item, Target, Test, Truth};
use super::parse::{self, Arg, ArgKind, Command, Node};
use crate::util::{self, slash};
use anyhow::Result;
use indexmap::{IndexMap, IndexSet};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

// Function calls and includes nested deeper than this are runaway recursion
const MAX_DEPTH: usize = 64;

// A while() that loops this often is assumed not to terminate in conversion
const MAX_WHILE_ITERATIONS: usize = 10_000;

// A scope's value for a variable it unset
const UNSET: &str = "\u{1}unset";

// Values of variables that depend on the platform or compiler; tests against them become conditions
const SYMBOL_PREFIX: &str = "\u{1}sym:";

pub struct Function {
    pub name: String,
    pub params: Vec<String>,
    pub body: Rc<Vec<Node>>,
    pub is_macro: bool,
}

/// Directory-level settings (include_directories, add_compile_options, ...), inherited by subdirectories.
#[derive(Debug, Clone, Default)]
pub struct DirState {
    pub dir: PathBuf,
    pub includes: Vec<Item<(PathBuf, bool)>>,
    pub defines: Vec<Item<String>>,
    pub options: Vec<Item<String>>,
    pub links: Vec<Item<String>>,
    pub link_options: Vec<Item<String>>,
    pub lib_dirs: Vec<Item<String>>,
}

/// Source file properties, matched to targets at the end.
#[derive(Debug, Clone)]
pub struct SourceProperties {
    pub files: Vec<PathBuf>,
    pub condition: Option<String>,
    pub options: Vec<String>,
    pub defines: Vec<String>,
}

pub enum Flow {
    Normal,
    Return(Truth),
    Break,
    Continue,
}

pub struct Interpreter {
    pub root: PathBuf,

    // Variables: function and directory scopes over the cache
    pub(super) scopes: Vec<HashMap<String, String>>,
    pub(super) cache: HashMap<String, String>,
    pub(super) functions: HashMap<String, Rc<Function>>,

    // Where we are: directories and the list file being read
    pub dirs: Vec<DirState>,
    pub(super) dir_stack: Vec<usize>,
    pub(super) list_files: Vec<PathBuf>,
    pub(super) conditions: Vec<String>,
    pub(super) depth: usize,

    // What we found
    pub targets: IndexMap<String, Target>,
    pub target_prefixes: HashMap<String, Vec<String>>,
    pub aliases: HashMap<String, String>,
    pub packages: Vec<String>,
    pub fetched: IndexMap<String, (String, Option<String>)>,
    pub options: IndexMap<String, (bool, String)>,
    pub(super) overridden_options: HashSet<String>,
    pub tests: Vec<Test>,
    pub custom_commands: Vec<CustomCommand>,
    pub source_properties: Vec<SourceProperties>,
    pub export_headers: Vec<ExportHeader>,
    pub pkg_config: IndexMap<String, Vec<String>>,
    pub project_name: Option<String>,
    pub project_version: Option<String>,
    pub c_language: bool,

    // What we could not translate
    pub warnings: Vec<String>,
    pub configure_writes: Vec<model::ConfigureWrite>,
    /// Per function call running now: did it run execute_process (whose output conversion cannot know)?
    pub(super) calls_ran_process: Vec<bool>,
    /// The project's functions running now, innermost last.
    pub(super) call_stack: Vec<model::HelperCall>,
    /// The foreach loops running now, outermost first: each one's variable and current item.
    pub(super) loops: Vec<(String, String)>,
    /// What each file(GLOB) found: its pattern and files (absolute), to turn hashed file lists back into globs.
    pub(super) glob_results: Vec<(String, Vec<String>)>,
    pub unknown_genex: Vec<String>,
    pub(super) unknown_commands: IndexSet<String>,
    /// What the built-in commands keep besides variables (builtins.rs).
    pub(super) builtin: super::builtins::BuiltinState,
}

impl Interpreter {
    pub fn new(root: &Path) -> Interpreter {
        let mut interpreter = Interpreter {
            root: root.to_path_buf(),
            scopes: vec![HashMap::new()],
            cache: HashMap::new(),
            functions: HashMap::new(),
            dirs: vec![DirState {
                dir: root.to_path_buf(),
                ..Default::default()
            }],
            dir_stack: vec![0],
            list_files: vec![root.join("CMakeLists.txt")],
            conditions: Vec::new(),
            depth: 0,
            targets: IndexMap::new(),
            target_prefixes: HashMap::new(),
            aliases: HashMap::new(),
            packages: Vec::new(),
            fetched: IndexMap::new(),
            options: IndexMap::new(),
            overridden_options: HashSet::new(),
            tests: Vec::new(),
            custom_commands: Vec::new(),
            source_properties: Vec::new(),
            export_headers: Vec::new(),
            pkg_config: IndexMap::new(),
            project_name: None,
            project_version: None,
            c_language: false,
            warnings: Vec::new(),
            configure_writes: Vec::new(),
            calls_ran_process: Vec::new(),
            call_stack: Vec::new(),
            loops: Vec::new(),
            glob_results: Vec::new(),
            unknown_genex: Vec::new(),
            unknown_commands: IndexSet::new(),
            builtin: Default::default(),
        };
        interpreter.define_builtin_variables();
        interpreter
    }

    /// Runs the project's top CMakeLists.txt.
    pub fn run_root(&mut self) -> Result<()> {
        self.load_preset_variables();

        let file = self.root.join("CMakeLists.txt");
        let nodes = parse::parse_file(&file)?;
        self.run_block(&nodes)?;
        self.finish_directory(0);
        self.finish_builtins();

        Ok(())
    }

    // Variables

    pub fn get_variable(&self, name: &str) -> Option<String> {
        for scope in self.scopes.iter().rev() {
            match scope.get(name) {
                Some(value) if value == UNSET => break,
                Some(value) => return Some(value.clone()),
                None => {}
            }
        }
        self.cache.get(name).cloned()
    }

    pub fn set_variable(&mut self, name: &str, value: &str) {
        self.scopes.last_mut().unwrap().insert(name.to_string(), value.to_string());
        if self.options.contains_key(name) {
            self.overridden_options.insert(name.to_string());
        }
    }

    pub fn set_parent_variable(&mut self, name: &str, value: &str) {
        let count = self.scopes.len();
        let index = if count >= 2 { count - 2 } else { 0 };
        self.scopes[index].insert(name.to_string(), value.to_string());
    }

    /// Unsets a variable in the current scope: the scopes below no longer show through, as in CMake, where a
    /// function's scope is a copy of its caller's.
    pub fn unset_variable(&mut self, name: &str) {
        let count = self.scopes.len();
        self.unset_in(count - 1, name);
    }

    pub fn unset_in(&mut self, index: usize, name: &str) {
        if index == 0 {
            self.scopes[0].remove(name);
        } else {
            self.scopes[index].insert(name.to_string(), UNSET.to_string());
        }
    }

    pub fn current_dir(&self) -> PathBuf {
        PathBuf::from(self.get_variable("CMAKE_CURRENT_SOURCE_DIR").unwrap_or_else(|| slash(&self.root)))
    }

    pub fn current_dir_id(&self) -> usize {
        *self.dir_stack.last().unwrap()
    }

    /// A path from CMake text: relative to the current source folder; {out} paths stay as they are.
    pub fn resolve_path(&self, text: &str) -> PathBuf {
        if text.starts_with("{out}") {
            return PathBuf::from(text);
        }
        util::absolute(&self.current_dir(), Path::new(text))
    }

    /// The condition everything happening now is under.
    pub fn current_condition(&self) -> Option<String> {
        self.conditions
            .iter()
            .fold(None, |combined, condition| model::and_optional(&combined, &Some(condition.clone())))
    }

    /// The condition of something set on a target now: the part not already implied by the target's own.
    pub fn condition_for_target(&self, target: &str) -> Option<String> {
        let prefix = self.target_prefixes.get(target).cloned().unwrap_or_default();
        let remaining = if self.conditions.starts_with(&prefix) {
            &self.conditions[prefix.len()..]
        } else {
            &self.conditions[..]
        };

        remaining
            .iter()
            .fold(None, |combined, condition| model::and_optional(&combined, &Some(condition.clone())))
    }

    pub fn warn(&mut self, command: &Command, message: &str) {
        let location = self.location(command);
        self.warnings.push(format!("{location}: {message}"));
    }

    /// `file:line` of a command, relative to the project.
    pub fn location(&self, command: &Command) -> String {
        format!("{}:{}", util::rel(&command.file, &self.root), command.line)
    }

    // Expansion

    /// Evaluates a command's arguments: variables expanded, unquoted ones split into lists.
    pub fn evaluate_args(&mut self, args: &[Arg]) -> Vec<String> {
        let mut values = Vec::new();

        for arg in args {
            match arg.kind {
                ArgKind::Bracket => values.push(arg.text.clone()),
                ArgKind::Quoted => values.push(unescape(&self.expand(&arg.text))),
                ArgKind::Unquoted => {
                    let expanded = self.expand(&arg.text);
                    values.extend(model::split_list(&expanded).iter().map(|item| unescape(item)));
                }
            }
        }

        values
    }

    /// Replaces ${VAR}, $ENV{VAR} and $CACHE{VAR} references (nested ones too).
    pub fn expand(&self, text: &str) -> String {
        if !text.contains('$') {
            return text.to_string();
        }

        let characters: Vec<char> = text.chars().collect();
        let mut output = String::new();
        let mut position = 0;

        while position < characters.len() {
            let character = characters[position];

            // Escapes pass through untouched (\${ is not a reference)
            if character == '\\' && position + 1 < characters.len() {
                output.push(character);
                output.push(characters[position + 1]);
                position += 2;
                continue;
            }

            if character == '$' {
                let rest: String = characters[position..].iter().take(7).collect();
                let (kind, skip) = if rest.starts_with("${") {
                    ("var", 2)
                } else if rest.starts_with("$ENV{") {
                    ("env", 5)
                } else if rest.starts_with("$CACHE{") {
                    ("cache", 7)
                } else {
                    ("", 0)
                };

                if skip > 0 {
                    if let Some(end) = find_closing_brace(&characters, position + skip) {
                        let inner: String = characters[position + skip..end].iter().collect();
                        let name = self.expand(&inner);
                        let option_condition = kind == "var" && in_genex_condition(&output) && self.option_truth(&name).is_some();
                        let value = match kind {
                            // An option read as a generator expression's condition stays the option
                            _ if option_condition => match self.option_truth(&name) {
                                Some(Truth::Depends(condition)) => model::option_marker(&condition),
                                _ => model::option_marker(&name),
                            },
                            "env" => std::env::var(&name).unwrap_or_default(),
                            "cache" => self.cache.get(&name).cloned().unwrap_or_default(),
                            _ => {
                                // A check's result stays a condition alone or in a generator expression's condition;
                                // inside longer text it reads as this machine's
                                let value = self.get_variable(&name).unwrap_or_default();
                                let whole = output.is_empty() && end + 1 == characters.len();
                                match model::marked_condition(&value) {
                                    Some(_) if !whole && !in_genex_condition(&output) => self.host_text(&value),
                                    _ => value,
                                }
                            }
                        };
                        output.push_str(&value);
                        position = end + 1;
                        continue;
                    }
                }
            }

            output.push(character);
            position += 1;
        }

        // A platform value inside longer text cannot stay symbolic: it reads as this machine's
        let is_single_symbol = output.starts_with(SYMBOL_PREFIX) && output.rfind(SYMBOL_PREFIX) == Some(0) && !output.contains(' ');
        if output.contains(SYMBOL_PREFIX) && !is_single_symbol {
            let pattern = regex::Regex::new(&format!("{}([A-Za-z0-9_]+)", regex::escape(SYMBOL_PREFIX))).unwrap();
            return pattern
                .replace_all(&output, |captures: &regex::Captures| symbol_host_text(&captures[1]))
                .into_owned();
        }

        output
    }

    /// A value as text on this machine: platform symbols and check results become what they are here.
    pub(super) fn host_text(&self, value: &str) -> String {
        if let Some(condition) = model::marked_condition(value) {
            let options = self.option_defaults();
            let holds = model::holds_on_host(condition, &|name| options.get(name).copied());
            return if holds { "1" } else { "" }.to_string();
        }

        let pattern = regex::Regex::new(&format!("{}([A-Za-z0-9_]+)", regex::escape(SYMBOL_PREFIX))).unwrap();
        pattern
            .replace_all(value, |captures: &regex::Captures| symbol_host_text(&captures[1]))
            .into_owned()
    }

    // Execution

    pub fn run_block(&mut self, nodes: &[Node]) -> Result<Flow> {
        let mut pushed = 0;
        let mut flow = Flow::Normal;

        for node in nodes {
            match self.run_node(node)? {
                Flow::Normal => {}
                Flow::Return(Truth::Known(true)) => {
                    flow = Flow::Return(Truth::Known(true));
                    break;
                }

                // A return under a condition: the rest runs only where it did not happen
                Flow::Return(Truth::Depends(condition)) => {
                    self.conditions.push(model::negate(&condition));
                    pushed += 1;
                    flow = match flow {
                        Flow::Return(previous) => Flow::Return(previous.or(Truth::Depends(condition))),
                        _ => Flow::Return(Truth::Depends(condition)),
                    };
                }
                Flow::Return(Truth::Known(false)) => {}
                other => {
                    flow = other;
                    break;
                }
            }
        }

        for _ in 0..pushed {
            self.conditions.pop();
        }
        Ok(flow)
    }

    /// Calls a user function or macro.
    pub fn call_function(&mut self, function: Rc<Function>, args: Vec<String>) -> Result<()> {
        if self.depth >= MAX_DEPTH {
            self.warnings.push("function calls nested too deeply; stopped".into());
            return Ok(());
        }
        self.depth += 1;

        // ARGC, ARGV, ARGN, ARGV<n> and the named parameters
        let mut bindings: Vec<(String, String)> = Vec::new();
        for (index, param) in function.params.iter().enumerate() {
            bindings.push((param.clone(), args.get(index).cloned().unwrap_or_default()));
        }

        bindings.push(("ARGC".into(), args.len().to_string()));
        bindings.push(("ARGV".into(), args.join(";")));
        bindings.push((
            "ARGN".into(),
            args.iter().skip(function.params.len()).cloned().collect::<Vec<_>>().join(";"),
        ));

        for (index, arg) in args.iter().enumerate() {
            bindings.push((format!("ARGV{index}"), arg.clone()));
        }

        self.calls_ran_process.push(false);
        self.call_stack.push(model::HelperCall {
            function: function.name.clone(),
            params: function.params.clone(),
            args: args.clone(),
        });
        let result = if function.is_macro {
            // Macros substitute their parameters into the body's text and run in the caller's scope
            let body = substitute_macro(&function.body, &bindings);
            self.run_block(&body)
        } else {
            self.scopes.push(bindings.into_iter().collect());
            let result = self.run_block(&function.body);
            self.scopes.pop();
            result
        };
        self.calls_ran_process.pop();
        self.call_stack.pop();
        result?;

        self.depth -= 1;
        Ok(())
    }

    /// Runs another CMake file in the current scope (include()).
    pub fn include_file(&mut self, file: &Path) -> Result<()> {
        if self.depth >= MAX_DEPTH {
            return Ok(());
        }

        let nodes = parse::parse_file(file)?;
        let previous_list_dir = self.get_variable("CMAKE_CURRENT_LIST_DIR");
        self.set_variable("CMAKE_CURRENT_LIST_DIR", &slash(file.parent().unwrap()));
        self.set_variable("CMAKE_CURRENT_LIST_FILE", &slash(file));
        self.list_files.push(file.to_path_buf());

        self.depth += 1;
        let result = self.run_block(&nodes);
        self.depth -= 1;

        self.list_files.pop();
        if let Some(previous) = previous_list_dir {
            self.set_variable("CMAKE_CURRENT_LIST_DIR", &previous);
        }
        result.map(|_| ())
    }

    /// Runs a subdirectory's CMakeLists.txt in a new scope with inherited directory settings.
    pub fn add_subdirectory(&mut self, dir: &Path) -> Result<()> {
        let file = dir.join("CMakeLists.txt");
        let nodes = parse::parse_file(&file)?;

        let mut state = self.dirs[self.current_dir_id()].clone();
        state.dir = dir.to_path_buf();
        self.dirs.push(state);
        let dir_id = self.dirs.len() - 1;

        self.dir_stack.push(dir_id);
        self.scopes.push(HashMap::new());
        self.set_variable("CMAKE_CURRENT_SOURCE_DIR", &slash(dir));
        self.set_variable("CMAKE_CURRENT_LIST_DIR", &slash(dir));
        let relative = util::rel(dir, &self.root);
        self.set_variable("CMAKE_CURRENT_BINARY_DIR", &format!("{{out}}/{relative}"));
        self.list_files.push(file);

        self.depth += 1;
        let result = self.run_block(&nodes);
        self.depth -= 1;

        self.list_files.pop();
        self.scopes.pop();
        self.dir_stack.pop();
        self.finish_directory(dir_id);

        result.map(|_| ())
    }

    pub fn option_defaults(&self) -> HashMap<String, bool> {
        self.options.iter().map(|(name, (default, _))| (name.clone(), *default)).collect()
    }

    // Conditions

    /// Evaluates an if() condition; platform, compiler, build-type and option tests stay symbolic.
    pub fn evaluate_condition(&mut self, args: &[Arg]) -> Truth {
        let mut tokens: Vec<(String, bool)> = Vec::new();
        for arg in args {
            match arg.kind {
                ArgKind::Unquoted => {
                    let expanded = self.expand(&arg.text);
                    let items = model::split_list(&expanded);
                    if items.is_empty() {
                        tokens.push((String::new(), false));
                    }
                    tokens.extend(items.into_iter().map(|item| (unescape(&item), false)));
                }
                ArgKind::Quoted => tokens.push((unescape(&self.expand(&arg.text)), true)),
                ArgKind::Bracket => tokens.push((arg.text.clone(), true)),
            }
        }

        let mut position = 0;
        let truth = self.condition_or(&tokens, &mut position);
        if position < tokens.len() {
            self.warnings.push(format!(
                "could not fully read if({})",
                tokens.iter().map(|(text, _)| text.as_str()).collect::<Vec<_>>().join(" ")
            ));
        }
        truth
    }

    /// Does this target exist (a real one, an alias, or one a found package provides)?
    pub fn target_exists(&self, name: &str) -> bool {
        if self.targets.contains_key(name) || self.aliases.contains_key(name) || name == "Threads::Threads" {
            return true;
        }

        let Some((namespace, short)) = name.split_once("::") else { return false };
        let normalized = |text: &str| text.to_lowercase().replace(['-', '_'], "");
        self.packages
            .iter()
            .chain(self.fetched.keys())
            .any(|package| normalized(package) == normalized(namespace) || normalized(package) == normalized(short))
    }

    // Setup

    /// vcpkg's settings from CMakePresets.json, where projects keep them: the first visible configure preset's
    /// VCPKG_* cache variables, through the presets it inherits from.
    fn load_preset_variables(&mut self) {
        let Ok(text) = std::fs::read_to_string(self.root.join("CMakePresets.json")) else {
            return;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
            return;
        };
        let Some(presets) = json.get("configurePresets").and_then(|presets| presets.as_array()) else {
            return;
        };

        let Some(visible) = presets
            .iter()
            .find(|preset| preset.get("hidden").and_then(|hidden| hidden.as_bool()) != Some(true))
        else {
            return;
        };

        let root = slash(&self.root);
        for (name, value) in preset_cache_variables(presets, visible, 0) {
            if name.starts_with("VCPKG_") && self.get_variable(&name).is_none() {
                self.cache.insert(name, value.replace("${sourceDir}", &root));
            }
        }
    }

    fn define_builtin_variables(&mut self) {
        let root = slash(&self.root);
        for name in [
            "CMAKE_SOURCE_DIR",
            "CMAKE_CURRENT_SOURCE_DIR",
            "CMAKE_CURRENT_LIST_DIR",
            "PROJECT_SOURCE_DIR",
            "CMAKE_HOME_DIRECTORY",
        ] {
            self.set_variable(name, &root);
        }
        for name in ["CMAKE_BINARY_DIR", "CMAKE_CURRENT_BINARY_DIR", "PROJECT_BINARY_DIR"] {
            self.set_variable(name, "{out}");
        }

        // Fixed facts of a 64-bit build with a modern compiler
        let fixed = [
            ("CMAKE_COMMAND", "cmake"),
            ("CMAKE_CTEST_COMMAND", "ctest"),
            ("CMAKE_CONFIGURATION_TYPES", "Debug;Release;RelWithDebInfo;MinSizeRel"),
            ("PROJECT_IS_TOP_LEVEL", "ON"),
            ("CMAKE_DL_LIBS", "dl"),
            ("CMAKE_VERSION", "3.31.0"),
            ("CMAKE_MAJOR_VERSION", "3"),
            ("CMAKE_MINOR_VERSION", "31"),
            ("CMAKE_CXX_COMPILER", "{cxx}"),
            ("CMAKE_C_COMPILER", "{cc}"),
            ("CMAKE_INSTALL_PREFIX", "{out}/install"),
            ("CMAKE_INSTALL_INCLUDEDIR", "include"),
            ("CMAKE_INSTALL_LIBDIR", "lib"),
            ("CMAKE_INSTALL_BINDIR", "bin"),
            ("CMAKE_INSTALL_DATADIR", "share"),
        ];
        for (name, value) in fixed {
            self.set_variable(name, value);
        }

        // Depends on the platform or compiler: tests against these become conditions
        for name in [
            "WIN32",
            "UNIX",
            "APPLE",
            "MSVC",
            "MINGW",
            "LINUX",
            "CYGWIN",
            "ANDROID",
            "IOS",
            "EMSCRIPTEN",
            "CMAKE_HOST_WIN32",
            "CMAKE_HOST_UNIX",
            "CMAKE_HOST_APPLE",
            "CMAKE_CXX_COMPILER_ID",
            "CMAKE_C_COMPILER_ID",
            "CMAKE_SYSTEM_NAME",
            "CMAKE_HOST_SYSTEM_NAME",
            "CMAKE_SIZEOF_VOID_P",
            "CMAKE_SYSTEM_PROCESSOR",
            "CMAKE_CXX_COMPILER_ARCHITECTURE_ID",
            "CMAKE_C_COMPILER_ARCHITECTURE_ID",
            "CMAKE_BUILD_TYPE",
            "CMAKE_CXX_COMPILER_FRONTEND_VARIANT",
            "CMAKE_CXX_SIMULATE_ID",
            "CMAKE_CXX_COMPILER_VERSION",
            "CMAKE_C_COMPILER_VERSION",
            "MSVC_VERSION",
            "CMAKE_GENERATOR",
        ] {
            self.set_variable(name, &format!("{SYMBOL_PREFIX}{name}"));
        }
    }

    // Execution

    fn run_node(&mut self, node: &Node) -> Result<Flow> {
        match node {
            Node::Command(command) => self.run_command(command),
            Node::If { branches, otherwise } => self.run_if(branches, otherwise.as_ref()),
            Node::Foreach { args, body } => self.run_foreach(args, body),
            Node::While { args, body } => self.run_while(args, body),
            Node::Function {
                name,
                params,
                body,
                is_macro,
            } => {
                let function = Function {
                    name: name.clone(),
                    params: params.clone(),
                    body: body.clone(),
                    is_macro: *is_macro,
                };
                self.functions.insert(name.clone(), Rc::new(function));
                Ok(Flow::Normal)
            }
        }
    }

    fn run_if(&mut self, branches: &[(Vec<Arg>, Vec<Node>)], otherwise: Option<&Vec<Node>>) -> Result<Flow> {
        // Which branches can run, and under what
        let mut taken = Truth::Known(false);
        let mut chosen: Vec<(Truth, &Vec<Node>)> = Vec::new();
        for (args, body) in branches {
            let truth = self.evaluate_condition(args);
            let effective = taken.clone().not().and(truth.clone());
            if effective != Truth::Known(false) {
                chosen.push((effective, body));
            }

            taken = taken.or(truth);
            if taken == Truth::Known(true) {
                break;
            }
        }
        if let (Some(body), false) = (otherwise, taken == Truth::Known(true)) {
            chosen.push((taken.not(), body));
        }

        // This machine's branch runs last, so the variables it sets win
        let options = self.option_defaults();
        chosen.sort_by_key(|(truth, _)| match truth {
            Truth::Depends(condition) => model::holds_on_host(condition, &|name| options.get(name).copied()),
            Truth::Known(_) => true,
        });

        let mut flow = Flow::Normal;
        for (truth, body) in chosen {
            let branch_flow = match &truth {
                Truth::Known(_) => self.run_block(body)?,
                Truth::Depends(condition) => {
                    self.conditions.push(condition.clone());
                    let result = self.run_block(body);
                    self.conditions.pop();
                    result?
                }
            };

            flow = match (flow, branch_flow, truth) {
                (Flow::Return(previous), Flow::Return(inner), truth) => Flow::Return(previous.or(truth.and(inner))),
                (_, Flow::Return(inner), truth) => Flow::Return(truth.and(inner)),
                (previous @ Flow::Return(_), _, _) => previous,
                (_, Flow::Break, Truth::Known(true)) => Flow::Break,
                (_, Flow::Continue, Truth::Known(true)) => Flow::Continue,
                (previous, _, _) => previous,
            };
        }

        Ok(flow)
    }

    fn run_foreach(&mut self, args: &[Arg], body: &[Node]) -> Result<Flow> {
        let values = self.evaluate_args(args);
        let Some((variable, rest)) = values.split_first() else {
            return Ok(Flow::Normal);
        };

        // The items: RANGE, IN LISTS/ITEMS, or plain
        let items: Vec<String> = match rest.first().map(|word| word.as_str()) {
            Some("RANGE") => {
                let numbers: Vec<i64> = rest[1..].iter().filter_map(|number| number.parse().ok()).collect();
                let (start, stop, step) = match numbers.as_slice() {
                    [stop] => (0, *stop, 1),
                    [start, stop] => (*start, *stop, 1),
                    [start, stop, step, ..] => (*start, *stop, (*step).max(1)),
                    _ => (0, -1, 1),
                };
                (start..=stop).step_by(step as usize).map(|number| number.to_string()).collect()
            }
            Some("IN") => {
                let mut items = Vec::new();
                let mut mode = "";
                for word in &rest[1..] {
                    match word.as_str() {
                        "LISTS" | "ITEMS" | "ZIP_LISTS" => mode = word,
                        _ if mode == "LISTS" => items.extend(model::split_list(&self.get_variable(word).unwrap_or_default())),
                        _ => items.push(word.clone()),
                    }
                }
                items
            }
            _ => rest.to_vec(),
        };

        for item in items {
            self.set_variable(variable, &item);
            self.loops.push((variable.clone(), item));
            let flow = self.run_block(body);
            self.loops.pop();
            match flow? {
                Flow::Break => break,
                flow @ Flow::Return(_) => return Ok(flow),
                _ => {}
            }
        }

        Ok(Flow::Normal)
    }

    fn run_while(&mut self, args: &[Arg], body: &[Node]) -> Result<Flow> {
        for _ in 0..MAX_WHILE_ITERATIONS {
            match self.evaluate_condition(args) {
                Truth::Known(true) => {}
                Truth::Known(false) => return Ok(Flow::Normal),
                Truth::Depends(_) => {
                    self.warnings.push("a while() loop depends on the platform; skipped".into());
                    return Ok(Flow::Normal);
                }
            }

            match self.run_block(body)? {
                Flow::Break => break,
                flow @ Flow::Return(_) => return Ok(flow),
                _ => {}
            }
        }

        Ok(Flow::Normal)
    }

    /// Applies a directory's settings to the targets defined in it.
    fn finish_directory(&mut self, dir_id: usize) {
        let state = self.dirs[dir_id].clone();

        for target in self.targets.values_mut().filter(|target| target.dir_id == dir_id) {
            let mut includes = state.includes.clone();
            includes.append(&mut target.includes);
            target.includes = includes;

            let mut defines = state.defines.clone();
            defines.append(&mut target.defines);
            target.defines = defines;

            let mut options = state.options.clone();
            options.append(&mut target.options);
            target.options = options;

            target.links.extend(state.links.iter().cloned());
            target.link_options.extend(state.link_options.iter().cloned());
            target.lib_dirs.extend(state.lib_dirs.iter().cloned());
        }
    }

    // Conditions

    fn condition_or(&mut self, tokens: &[(String, bool)], position: &mut usize) -> Truth {
        let mut truth = self.condition_and(tokens, position);
        while is_keyword(tokens, *position, "OR") {
            *position += 1;
            let right = self.condition_and(tokens, position);
            truth = truth.or(right);
        }
        truth
    }

    fn condition_and(&mut self, tokens: &[(String, bool)], position: &mut usize) -> Truth {
        let mut truth = self.condition_not(tokens, position);
        while is_keyword(tokens, *position, "AND") {
            *position += 1;
            let right = self.condition_not(tokens, position);
            truth = truth.and(right);
        }
        truth
    }

    fn condition_not(&mut self, tokens: &[(String, bool)], position: &mut usize) -> Truth {
        if is_keyword(tokens, *position, "NOT") {
            *position += 1;
            return self.condition_not(tokens, position).not();
        }
        self.condition_primary(tokens, position)
    }

    fn condition_primary(&mut self, tokens: &[(String, bool)], position: &mut usize) -> Truth {
        let Some((text, quoted)) = tokens.get(*position).cloned() else {
            return Truth::Known(false);
        };

        // ( ... )
        if !quoted && text == "(" {
            *position += 1;
            let truth = self.condition_or(tokens, position);
            if is_keyword(tokens, *position, ")") {
                *position += 1;
            }
            return truth;
        }

        // Unary tests
        if !quoted {
            if let Some(truth) = self.unary_test(&text, tokens, position) {
                return truth;
            }
        }

        // Binary tests
        *position += 1;
        if let Some((operator, false)) = tokens.get(*position).cloned() {
            if is_binary_operator(&operator) {
                *position += 1;
                let right = tokens.get(*position).cloned().unwrap_or_default();
                *position += 1;
                return self.compare(&(text, quoted), &operator, &right);
            }
        }

        self.truthiness(&text, quoted)
    }

    fn unary_test(&mut self, operator: &str, tokens: &[(String, bool)], position: &mut usize) -> Option<Truth> {
        let operand = tokens.get(*position + 1).map(|(text, _)| text.clone()).unwrap_or_default();
        let truth = match operator {
            "DEFINED" => {
                let name = operand.trim_start_matches("ENV{").trim_start_matches("CACHE{").trim_end_matches('}');
                let defined = if operand.starts_with("ENV{") {
                    std::env::var(name).is_ok()
                } else {
                    self.get_variable(name).is_some()
                };
                Truth::Known(defined)
            }
            "TARGET" => Truth::Known(self.target_exists(&operand)),
            "EXISTS" => Truth::Known(!operand.contains("{out}") && !operand.is_empty() && Path::new(&operand).exists()),
            "IS_DIRECTORY" => Truth::Known(!operand.is_empty() && Path::new(&operand).is_dir()),
            "IS_ABSOLUTE" => Truth::Known(Path::new(&operand).is_absolute()),
            "COMMAND" => Truth::Known(self.functions.contains_key(&operand.to_lowercase()) || super::builtins::is_builtin(&operand.to_lowercase())),
            "POLICY" => Truth::Known(true),
            "TEST" => Truth::Known(self.tests.iter().any(|test| test.name == operand)),
            "IS_SYMLINK" | "IS_READABLE" | "IS_WRITABLE" | "IS_EXECUTABLE" => Truth::Known(Path::new(&operand).exists()),
            _ => return None,
        };

        *position += 2;
        Some(truth)
    }

    pub(super) fn truthiness(&self, text: &str, quoted: bool) -> Truth {
        if let Some(symbol) = text.strip_prefix(SYMBOL_PREFIX) {
            return symbol_truth(symbol);
        }
        if let Some(condition) = model::marked_condition(text) {
            return Truth::Depends(condition.to_string());
        }
        if let Some(constant) = model::constant_truth(text) {
            return Truth::Known(constant);
        }
        if quoted {
            return Truth::Known(false);
        }

        // A variable name: platform symbols, options, or its value
        if let Some(truth) = self.option_truth(text) {
            return truth;
        }
        match self.get_variable(text) {
            Some(value) if value.starts_with(SYMBOL_PREFIX) => symbol_truth(&value[SYMBOL_PREFIX.len()..]),
            Some(value) if model::marked_condition(&value).is_some() => self.truthiness(&value, false),
            Some(value) => Truth::Known(model::constant_truth(&value).unwrap_or(true)),
            None => Truth::Known(false),
        }
    }

    /// An option() that nothing set since is a condition of its own (and of its cmake_dependent_option() dependencies).
    pub(super) fn option_truth(&self, name: &str) -> Option<Truth> {
        if self.options.contains_key(name) && !self.overridden_options.contains(name) {
            let condition = match self.builtin.option_conditions.get(name) {
                Some(depends) => model::and(name, depends),
                None => name.to_string(),
            };
            return Some(Truth::Depends(condition));
        }
        None
    }

    fn compare(&mut self, left: &(String, bool), operator: &str, right: &(String, bool)) -> Truth {
        // IN_LIST's right side names the list variable itself
        let left_value = self.operand_value(left);
        let right_value = match operator {
            "IN_LIST" => right.0.clone(),
            _ => self.operand_value(right),
        };

        // Against a platform or compiler symbol: test each value it can have
        for (symbol_side, other) in [(&left_value, &right_value), (&right_value, &left_value)] {
            if let Some(symbol) = symbol_side.strip_prefix(SYMBOL_PREFIX) {
                let symbol = symbol.to_string();
                let other = other.clone();
                return self.compare_symbol(&symbol, operator, &other, std::ptr::eq(symbol_side, &left_value));
            }

            // A check's result against a truth value: `HAVE_X EQUAL 1`
            let marked = model::marked_condition(symbol_side).map(|condition| Truth::Depends(condition.to_string()));
            if let (Some(truth), "EQUAL" | "STREQUAL", Some(constant)) = (marked, operator, model::constant_truth(other)) {
                return if constant { truth } else { truth.not() };
            }
        }

        Truth::Known(compare_values(&left_value, operator, &right_value, &|name| self.get_variable(name)))
    }

    /// A comparison's operand: an unquoted variable name reads its value.
    fn operand_value(&self, (text, quoted): &(String, bool)) -> String {
        if *quoted {
            return text.clone();
        }
        self.get_variable(text).unwrap_or_else(|| text.clone())
    }

    fn compare_symbol(&mut self, symbol: &str, operator: &str, other: &str, symbol_on_left: bool) -> Truth {
        // Version checks: assume a current compiler
        if symbol.ends_with("_VERSION") {
            let newer = matches!(operator, "VERSION_GREATER" | "VERSION_GREATER_EQUAL" | "GREATER" | "GREATER_EQUAL");
            return Truth::Known(if symbol_on_left { newer } else { !newer && operator != "VERSION_EQUAL" });
        }

        let candidates: Vec<(&str, Truth)> = match symbol {
            "CMAKE_CXX_COMPILER_ID" | "CMAKE_C_COMPILER_ID" | "CMAKE_CXX_SIMULATE_ID" => ["MSVC", "Clang", "AppleClang", "GNU"]
                .iter()
                .map(|id| (*id, model::compiler_truth(id)))
                .collect(),
            "CMAKE_SYSTEM_NAME" | "CMAKE_HOST_SYSTEM_NAME" => ["Windows", "Linux", "Darwin"]
                .iter()
                .map(|name| (*name, model::platform_truth(name)))
                .collect(),
            "CMAKE_BUILD_TYPE" => ["Debug", "Release", "RelWithDebInfo", "MinSizeRel"]
                .iter()
                .map(|config| (*config, Truth::Depends(model::profile_name(config))))
                .collect(),
            "CMAKE_CXX_COMPILER_FRONTEND_VARIANT" => vec![("MSVC", Truth::Depends("msvc".into())), ("GNU", Truth::Depends("gnu".into()))],
            "CMAKE_SIZEOF_VOID_P" => vec![("8", Truth::Depends("x64 | arm64".into())), ("4", Truth::Depends("x86 | arm".into()))],
            "CMAKE_SYSTEM_PROCESSOR" | "CMAKE_CXX_COMPILER_ARCHITECTURE_ID" | "CMAKE_C_COMPILER_ARCHITECTURE_ID" => PROCESSOR_NAMES
                .iter()
                .map(|(name, atom)| (*name, Truth::Depends(atom.to_string())))
                .collect(),
            "CMAKE_GENERATOR" => vec![("Ninja", Truth::Known(true))],
            _ => return Truth::Known(false),
        };

        let getter = |name: &str| self.get_variable(name);
        candidates.into_iter().fold(Truth::Known(false), |result, (candidate, truth)| {
            let matches = if symbol_on_left {
                compare_values(candidate, operator, other, &getter)
            } else {
                compare_values(other, operator, candidate, &getter)
            };
            if matches {
                result.or(truth)
            } else {
                result
            }
        })
    }
}

// CMAKE_SYSTEM_PROCESSOR and the MSVC architecture ids, and the condition each means
const PROCESSOR_NAMES: &[(&str, &str)] = &[
    ("AMD64", "x64"),
    ("x86_64", "x64"),
    ("x64", "x64"),
    ("X86", "x86"),
    ("x86", "x86"),
    ("i386", "x86"),
    ("i686", "x86"),
    ("ARM64", "arm64"),
    ("aarch64", "arm64"),
    ("arm64", "arm64"),
    ("ARMV7", "arm"),
    ("armv7l", "arm"),
    ("arm", "arm"),
];

/// A platform symbol's value on this machine, for text that embeds it.
fn symbol_host_text(symbol: &str) -> &'static str {
    let on = |flag: bool| if flag { "1" } else { "" };

    match symbol {
        "WIN32" | "CMAKE_HOST_WIN32" | "MSVC" => on(cfg!(windows)),
        "UNIX" | "CMAKE_HOST_UNIX" => on(cfg!(unix)),
        "LINUX" => on(cfg!(target_os = "linux")),
        "APPLE" | "CMAKE_HOST_APPLE" => on(cfg!(target_os = "macos")),
        "CMAKE_CXX_COMPILER_ID" | "CMAKE_C_COMPILER_ID" | "CMAKE_CXX_SIMULATE_ID" | "CMAKE_CXX_COMPILER_FRONTEND_VARIANT" => {
            if cfg!(windows) {
                "MSVC"
            } else {
                "GNU"
            }
        }
        "CMAKE_SYSTEM_NAME" | "CMAKE_HOST_SYSTEM_NAME" => {
            if cfg!(windows) {
                "Windows"
            } else if cfg!(target_os = "macos") {
                "Darwin"
            } else {
                "Linux"
            }
        }
        "CMAKE_BUILD_TYPE" => "{config}",
        "CMAKE_GENERATOR" => "Ninja",
        "CMAKE_SIZEOF_VOID_P" => {
            if cfg!(target_pointer_width = "64") {
                "8"
            } else {
                "4"
            }
        }
        "CMAKE_SYSTEM_PROCESSOR" => match (cfg!(windows), cfg!(target_arch = "aarch64")) {
            (true, true) => "ARM64",
            (true, false) => "AMD64",
            (false, true) => "aarch64",
            (false, false) => "x86_64",
        },
        "CMAKE_CXX_COMPILER_ARCHITECTURE_ID" | "CMAKE_C_COMPILER_ARCHITECTURE_ID" if cfg!(windows) => {
            if cfg!(target_arch = "aarch64") {
                "ARM64"
            } else {
                "x64"
            }
        }
        _ => "",
    }
}

/// Is text being expanded at a generator expression's condition: right after `$<BOOL:`, `$<NOT:`, or an argument of
/// `$<AND:` or `$<OR:`?
fn in_genex_condition(before: &str) -> bool {
    if ["$<BOOL:", "$<NOT:", "$<AND:", "$<OR:"].iter().any(|opening| before.ends_with(opening)) {
        return true;
    }
    if !before.ends_with(',') {
        return false;
    }

    // After a comma: the innermost open expression must be AND or OR
    let mut depth = 0;
    let characters: Vec<char> = before.chars().collect();
    for index in (0..characters.len()).rev() {
        match characters[index] {
            '>' => depth += 1,
            '<' if index > 0 && characters[index - 1] == '$' => {
                if depth == 0 {
                    let opened: String = characters[index + 1..].iter().collect();
                    return opened.starts_with("AND:") || opened.starts_with("OR:");
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    false
}

/// The condition a platform symbol stands for.
fn symbol_truth(symbol: &str) -> Truth {
    let condition = match symbol {
        "WIN32" | "CMAKE_HOST_WIN32" => "windows",
        "UNIX" | "CMAKE_HOST_UNIX" => "unix",
        "APPLE" | "CMAKE_HOST_APPLE" => "macos",
        "LINUX" => "linux",
        "MSVC" => "msvc",
        "MINGW" => "mingw",
        "CYGWIN" | "ANDROID" | "IOS" | "EMSCRIPTEN" => return Truth::Known(false),
        _ => return Truth::Known(true),
    };
    Truth::Depends(condition.into())
}

fn compare_values(left: &str, operator: &str, right: &str, variable: &dyn Fn(&str) -> Option<String>) -> bool {
    let number = |text: &str| text.trim().parse::<f64>().ok();

    match operator {
        "STREQUAL" => left == right,
        "STRLESS" => left < right,
        "STRGREATER" => left > right,
        "STRLESS_EQUAL" => left <= right,
        "STRGREATER_EQUAL" => left >= right,
        "EQUAL" => number(left).is_some() && number(left) == number(right),
        "LESS" => matches!((number(left), number(right)), (Some(left_number), Some(right_number)) if left_number < right_number),
        "GREATER" => matches!((number(left), number(right)), (Some(left_number), Some(right_number)) if left_number > right_number),
        "LESS_EQUAL" => matches!((number(left), number(right)), (Some(left_number), Some(right_number)) if left_number <= right_number),
        "GREATER_EQUAL" => matches!((number(left), number(right)), (Some(left_number), Some(right_number)) if left_number >= right_number),
        "VERSION_LESS" => compare_versions(left, right) == std::cmp::Ordering::Less,
        "VERSION_GREATER" => compare_versions(left, right) == std::cmp::Ordering::Greater,
        "VERSION_EQUAL" => compare_versions(left, right) == std::cmp::Ordering::Equal,
        "VERSION_LESS_EQUAL" => compare_versions(left, right) != std::cmp::Ordering::Greater,
        "VERSION_GREATER_EQUAL" => compare_versions(left, right) != std::cmp::Ordering::Less,
        "MATCHES" => regex::Regex::new(right).map(|pattern| pattern.is_match(left)).unwrap_or(false),
        "IN_LIST" => model::split_list(&variable(right).unwrap_or_default()).iter().any(|item| item == left),
        "PATH_EQUAL" => util::path_key(Path::new(left)) == util::path_key(Path::new(right)),
        _ => false,
    }
}

fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    let parts = |text: &str| -> Vec<u64> {
        text.split('.')
            .map(|part| {
                part.chars()
                    .take_while(|character| character.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0)
            })
            .collect()
    };
    let (mut left, mut right) = (parts(left), parts(right));
    let length = left.len().max(right.len());
    left.resize(length, 0);
    right.resize(length, 0);

    left.cmp(&right)
}

fn is_keyword(tokens: &[(String, bool)], position: usize, keyword: &str) -> bool {
    matches!(tokens.get(position), Some((text, false)) if text == keyword)
}

/// A configure preset's cache variables, the presets it inherits from first (deeper chains are cut off).
fn preset_cache_variables(presets: &[serde_json::Value], preset: &serde_json::Value, depth: usize) -> Vec<(String, String)> {
    let mut variables = Vec::new();
    if depth > 8 {
        return variables;
    }

    // Inherited ones, then its own
    let parents: Vec<&str> = match preset.get("inherits") {
        Some(serde_json::Value::String(name)) => vec![name.as_str()],
        Some(serde_json::Value::Array(names)) => names.iter().filter_map(|name| name.as_str()).collect(),
        _ => Vec::new(),
    };
    for parent_name in parents.iter().rev() {
        let parent = presets
            .iter()
            .find(|candidate| candidate.get("name").and_then(|name| name.as_str()) == Some(parent_name));
        if let Some(parent) = parent {
            variables.extend(preset_cache_variables(presets, parent, depth + 1));
        }
    }

    let own = preset.get("cacheVariables").and_then(|variables| variables.as_object());
    for (name, value) in own.into_iter().flatten() {
        let text = match value {
            serde_json::Value::String(text) => Some(text.clone()),
            serde_json::Value::Object(typed) => typed.get("value").and_then(|value| value.as_str()).map(String::from),
            serde_json::Value::Bool(flag) => Some(if *flag { "ON" } else { "OFF" }.to_string()),
            _ => None,
        };
        if let Some(text) = text {
            variables.push((name.clone(), text));
        }
    }

    variables
}

fn is_binary_operator(text: &str) -> bool {
    matches!(
        text,
        "STREQUAL"
            | "STRLESS"
            | "STRGREATER"
            | "STRLESS_EQUAL"
            | "STRGREATER_EQUAL"
            | "EQUAL"
            | "LESS"
            | "GREATER"
            | "LESS_EQUAL"
            | "GREATER_EQUAL"
            | "VERSION_LESS"
            | "VERSION_GREATER"
            | "VERSION_EQUAL"
            | "VERSION_LESS_EQUAL"
            | "VERSION_GREATER_EQUAL"
            | "MATCHES"
            | "IN_LIST"
            | "PATH_EQUAL"
    )
}

fn find_closing_brace(characters: &[char], start: usize) -> Option<usize> {
    let mut depth = 1;
    let mut position = start;

    while position < characters.len() {
        match characters[position] {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(position);
                }
            }
            _ => {}
        }
        position += 1;
    }

    None
}

/// CMake escape sequences to their characters.
pub fn unescape(text: &str) -> String {
    if !text.contains('\\') {
        return text.to_string();
    }

    let mut output = String::new();
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }

        match characters.next() {
            Some('n') => output.push('\n'),
            Some('t') => output.push('\t'),
            Some('r') => output.push('\r'),
            // `\;` stays escaped until a list is split (`split_list` reads it), as in CMake
            Some(';') => output.push_str("\\;"),
            Some(other) if "\\\"$()#^@[]{}<> ".contains(other) => output.push(other),
            Some(other) => {
                output.push('\\');
                output.push(other);
            }
            None => output.push('\\'),
        }
    }

    output
}

/// A macro body with ${param}, ${ARGN}, ${ARGV}, ${ARGC} and ${ARGV<n>} replaced in its text.
fn substitute_macro(body: &[Node], bindings: &[(String, String)]) -> Vec<Node> {
    let substitute = |text: &str| -> String {
        let mut result = text.to_string();
        for (name, value) in bindings {
            result = result.replace(&format!("${{{name}}}"), value);
        }
        result
    };
    let substitute_args = |args: &[Arg]| -> Vec<Arg> {
        args.iter()
            .map(|arg| Arg {
                text: substitute(&arg.text),
                kind: arg.kind,
            })
            .collect()
    };

    body.iter()
        .map(|node| match node {
            Node::Command(command) => Node::Command(Command {
                args: substitute_args(&command.args),
                ..command.clone()
            }),
            Node::If { branches, otherwise } => Node::If {
                branches: branches
                    .iter()
                    .map(|(args, body)| (substitute_args(args), substitute_macro(body, bindings)))
                    .collect(),
                otherwise: otherwise.as_ref().map(|body| substitute_macro(body, bindings)),
            },
            Node::Foreach { args, body } => Node::Foreach {
                args: substitute_args(args),
                body: substitute_macro(body, bindings),
            },
            Node::While { args, body } => Node::While {
                args: substitute_args(args),
                body: substitute_macro(body, bindings),
            },
            other => other.clone(),
        })
        .collect()
}
