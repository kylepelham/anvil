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

// Words that commands read as keywords: a branch that sets a variable to one chooses how a command runs, and keeps
// no list item
const COMMAND_KEYWORDS: &[&str] = &[
    "SYSTEM",
    "BEFORE",
    "AFTER",
    "PUBLIC",
    "PRIVATE",
    "INTERFACE",
    "LINK_PUBLIC",
    "LINK_PRIVATE",
    "STATIC",
    "SHARED",
    "MODULE",
    "OBJECT",
    "IMPORTED",
    "GLOBAL",
    "ALIAS",
    "EXCLUDE_FROM_ALL",
    "WIN32",
    "MACOSX_BUNDLE",
    "ALL",
    "OPTIONAL",
    "REQUIRED",
    "QUIET",
    "CONFIG",
    "COMPONENTS",
    "VERBATIM",
    "debug",
    "optimized",
    "general",
];

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

/// Variables and functions as they were before a branch this machine does not take.
struct VariableState {
    scopes: Vec<HashMap<String, String>>,
    cache: HashMap<String, String>,
    functions: HashMap<String, Rc<Function>>,
    overridden_options: HashSet<String>,
}

/// Items a branch this machine does not take appended to a list, kept under the branch's condition.
struct AppendedItems {
    variable: String,
    condition: String,
    before: Vec<String>,
    items: Vec<String>,
}

impl VariableState {
    fn get_variable(&self, name: &str) -> Option<String> {
        for scope in self.scopes.iter().rev() {
            match scope.get(name) {
                Some(value) if value == UNSET => break,
                Some(value) => return Some(value.clone()),
                None => {}
            }
        }
        self.cache.get(name).cloned()
    }
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
    /// Files set_source_files_properties(HEADER_FILE_ONLY) keeps out of every target (path keys).
    pub(super) header_only_sources: HashSet<String>,
    /// Words added to a CMAKE_<LANG>_FLAGS variable under a condition: (variable, word) -> condition.
    pub(super) flag_word_conditions: HashMap<(String, String), String>,
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
            header_only_sources: HashSet::new(),
            flag_word_conditions: HashMap::new(),
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
        self.note_flag_words(name, value);
        self.scopes.last_mut().unwrap().insert(name.to_string(), value.to_string());
        if self.options.contains_key(name) {
            self.overridden_options.insert(name.to_string());
        }
    }

    pub fn set_parent_variable(&mut self, name: &str, value: &str) {
        self.note_flag_words(name, value);
        // The top scope has no parent: CMake does nothing
        let count = self.scopes.len();
        if count < 2 {
            return;
        }
        self.scopes[count - 2].insert(name.to_string(), value.to_string());
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
                ArgKind::Quoted => values.push(self.expand_argument(&arg.text)),
                ArgKind::Unquoted => values.extend(model::split_list(&self.expand_argument(&arg.text))),
            }
        }

        values
    }

    /// Replaces ${VAR}, $ENV{VAR} and $CACHE{VAR} references (nested ones too); escapes stay as written.
    pub fn expand(&self, text: &str) -> String {
        self.expand_text(text, false)
    }

    /// An argument's text with its escape sequences read (`\;` stays, for lists) and its references replaced by
    /// their values as they are.
    fn expand_argument(&self, text: &str) -> String {
        self.expand_text(text, true)
    }

    fn expand_text(&self, text: &str, escapes: bool) -> String {
        if !text.contains('$') && !(escapes && text.contains('\\')) {
            return text.to_string();
        }

        let characters: Vec<char> = text.chars().collect();
        let mut output = String::new();
        let mut position = 0;

        while position < characters.len() {
            let character = characters[position];

            // Escapes (\${ is not a reference)
            if character == '\\' && position + 1 < characters.len() {
                let escaped = characters[position + 1];
                match escaped {
                    _ if !escapes => {
                        output.push(character);
                        output.push(escaped);
                    }
                    'n' => output.push('\n'),
                    't' => output.push('\t'),
                    'r' => output.push('\r'),
                    ';' => output.push_str("\\;"),
                    _ if escaped.is_ascii_alphanumeric() => {
                        output.push(character);
                        output.push(escaped);
                    }
                    _ => output.push(escaped),
                }
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
                        let mut value = match kind {
                            // An option read as a generator expression's condition stays the option
                            _ if option_condition => model::option_marker(&name),
                            "env" => std::env::var(&name).unwrap_or_default(),
                            "cache" => self.cache.get(&name).cloned().unwrap_or_default(),
                            _ => self.get_variable(&name).unwrap_or_default(),
                        };

                        // So does a platform value
                        if let Some(symbol) = value.strip_prefix(SYMBOL_PREFIX) {
                            if in_genex_test(&output) {
                                value = model::symbol_marker(symbol);
                            }
                        }
                        output.push_str(&self.resolve_conditional_items(value));
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

    /// Calls a user function or macro. A macro's return(), break() and continue() act in its caller.
    pub fn call_function(&mut self, function: Rc<Function>, args: Vec<String>) -> Result<Flow> {
        if self.depth >= MAX_DEPTH {
            self.warnings.push("function calls nested too deeply; stopped".into());
            return Ok(Flow::Normal);
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
            result.map(|_| Flow::Normal)
        };
        self.calls_ran_process.pop();
        self.call_stack.pop();
        let flow = result?;

        self.depth -= 1;
        Ok(flow)
    }

    /// Runs another CMake file in the current scope (include()).
    pub fn include_file(&mut self, file: &Path) -> Result<()> {
        if self.depth >= MAX_DEPTH {
            return Ok(());
        }

        let nodes = parse::parse_file(file)?;
        let previous_list_dir = self.get_variable("CMAKE_CURRENT_LIST_DIR");
        let previous_list_file = self.get_variable("CMAKE_CURRENT_LIST_FILE");
        self.set_variable("CMAKE_CURRENT_LIST_DIR", &slash(file.parent().unwrap()));
        self.set_variable("CMAKE_CURRENT_LIST_FILE", &slash(file));
        self.list_files.push(file.to_path_buf());

        self.depth += 1;
        let result = self.run_block(&nodes);
        self.depth -= 1;

        self.list_files.pop();
        for (name, previous) in [
            ("CMAKE_CURRENT_LIST_DIR", previous_list_dir),
            ("CMAKE_CURRENT_LIST_FILE", previous_list_file),
        ] {
            match previous {
                Some(previous) => self.set_variable(name, &previous),
                None => self.unset_variable(name),
            }
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
        self.set_variable("CMAKE_CURRENT_LIST_FILE", &slash(&file));
        let relative = util::rel(dir, &self.root);
        self.set_variable("CMAKE_CURRENT_BINARY_DIR", &format!("{{out}}/{relative}"));
        self.list_files.push(file);

        self.depth += 1;
        let result = self.run_block(&nodes);
        self.depth -= 1;

        self.list_files.pop();
        self.finish_directory(dir_id);
        self.scopes.pop();
        self.dir_stack.pop();

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
                    let items = model::split_list(&self.expand_argument(&arg.text));
                    if items.is_empty() {
                        tokens.push((String::new(), false));
                    }
                    tokens.extend(items.into_iter().map(|item| (item, false)));
                }
                ArgKind::Quoted => tokens.push((self.expand_argument(&arg.text), true)),
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

        // A test that cannot hold anywhere (`cl & gcc`), or holds everywhere, is known
        match &truth {
            Truth::Depends(condition) => match model::simplify(condition, None) {
                known @ Truth::Known(_) => known,
                Truth::Depends(_) => truth,
            },
            Truth::Known(_) => truth,
        }
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
        self.set_variable("CMAKE_CURRENT_LIST_FILE", &format!("{root}/CMakeLists.txt"));

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
            "CMAKE_C_COMPILER_FRONTEND_VARIANT",
            "CMAKE_CXX_SIMULATE_ID",
            "CMAKE_C_SIMULATE_ID",
            "CMAKE_CXX_COMPILER_VERSION",
            "CMAKE_C_COMPILER_VERSION",
            "MSVC_VERSION",
            "CMAKE_COMPILER_IS_GNUCXX",
            "CMAKE_COMPILER_IS_GNUCC",
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

        // Only this machine's branch sets variables: the others run from the state before the if() (their targets and
        // settings are recorded under their conditions), then their variables are put back. Items they appended to a
        // list stay in it, under their condition.
        let options = self.option_defaults();
        let on_host = |truth: &Truth| match truth {
            Truth::Depends(condition) => model::holds_on_host(condition, &|name| options.get(name).copied()),
            Truth::Known(_) => true,
        };
        chosen.sort_by_key(|(truth, _)| on_host(truth));
        let before = chosen.iter().any(|(truth, _)| !on_host(truth)).then(|| self.variable_state());
        let mut appended: Vec<AppendedItems> = Vec::new();

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

            if let (Some(before), Truth::Depends(condition), false) = (&before, &truth, on_host(&truth)) {
                appended.extend(self.appended_items(before, condition));
                self.restore_variable_state(before);
            }

            flow = match (flow, branch_flow, truth) {
                (Flow::Return(previous), Flow::Return(inner), truth) => Flow::Return(previous.or(truth.and(inner))),
                (_, Flow::Return(inner), truth) => Flow::Return(truth.and(inner)),
                (previous @ Flow::Return(_), _, _) => previous,
                (_, Flow::Break, Truth::Known(true)) => Flow::Break,
                (_, Flow::Continue, Truth::Known(true)) => Flow::Continue,
                (previous, _, _) => previous,
            };
        }

        for appended in appended {
            self.keep_appended_items(appended);
        }
        Ok(flow)
    }

    /// A value with its conditional items (from branches this machine does not take) decided where the conditions
    /// being run under settle them: inside `if(OPT)`, an item an earlier `if(OPT)` appended is plain again.
    fn resolve_conditional_items(&self, value: String) -> String {
        let Some(current) = self.current_condition() else { return value };
        if !value.contains("$<$<BOOL:") {
            return value;
        }

        let mut items = Vec::new();
        for item in model::list_elements(&value) {
            match model::split_conditional_item(&item).map(|(condition, text)| (model::simplify(&condition, Some(&current)), text)) {
                Some((Truth::Known(true), text)) => items.push(text.to_string()),
                Some((Truth::Known(false), _)) => {}
                _ => items.push(item),
            }
        }
        items.join(";")
    }

    /// The variables and functions, to run a branch this machine does not take and undo what it set.
    fn variable_state(&self) -> VariableState {
        VariableState {
            scopes: self.scopes.clone(),
            cache: self.cache.clone(),
            functions: self.functions.clone(),
            overridden_options: self.overridden_options.clone(),
        }
    }

    /// Puts variables back as they were; functions the branch defined stay (calls to them run under its condition).
    fn restore_variable_state(&mut self, state: &VariableState) {
        if self.scopes.len() == state.scopes.len() {
            self.scopes.clone_from(&state.scopes);
        } else {
            // A block() left open in the branch: keep the scopes it pushed apart from the rest
            let count = state.scopes.len().min(self.scopes.len());
            self.scopes[..count].clone_from_slice(&state.scopes[..count]);
        }
        self.cache.clone_from(&state.cache);
        for (name, function) in &state.functions {
            self.functions.insert(name.clone(), function.clone());
        }
        self.overridden_options.clone_from(&state.overridden_options);
    }

    /// Lists in the current scope that a branch only appended to: what it appended, under the branch's condition.
    fn appended_items(&self, before: &VariableState, condition: &str) -> Vec<AppendedItems> {
        let scope = self.scopes.len() - 1;
        let mut found = Vec::new();

        for (name, value) in &self.scopes[scope] {
            // CMake's own variables (flags, module paths) are read at configure time, as are strings built with spaces
            if value == UNSET || name.starts_with("CMAKE_") {
                continue;
            }
            let previous = before.get_variable(name).unwrap_or_default();
            if *value == previous {
                continue;
            }

            let (old_items, new_items) = (model::list_elements(&previous), model::list_elements(value));
            let appended = new_items.len() > old_items.len() && new_items.starts_with(&old_items);
            let is_value = |item: &String| !item.contains(char::is_whitespace) && !COMMAND_KEYWORDS.contains(&item.as_str());
            if appended && new_items[old_items.len()..].iter().all(is_value) {
                found.push(AppendedItems {
                    variable: name.clone(),
                    condition: condition.to_string(),
                    before: old_items.clone(),
                    items: new_items[old_items.len()..].to_vec(),
                });
            }
        }

        found.sort_by(|left, right| left.variable.cmp(&right.variable));
        found
    }

    /// Adds a branch's appended items back, each under the branch's condition, where the branch this machine takes
    /// (if any) left the list as it was or only appended to it too. A list the host's branch set from nothing is
    /// its own: `if(A) set(X a) else() set(X b) endif()`.
    fn keep_appended_items(&mut self, appended: AppendedItems) {
        let current = model::list_elements(&self.get_variable(&appended.variable).unwrap_or_default());
        let host_appended = current.len() > appended.before.len();
        if !current.starts_with(&appended.before) || (appended.before.is_empty() && host_appended) {
            return;
        }

        let mut items = current;
        items.extend(
            appended
                .items
                .iter()
                .filter(|item| !item.is_empty())
                .map(|item| model::conditional_item(&appended.condition, item)),
        );
        self.set_variable(&appended.variable, &items.join(";"));
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

        // The loop variable gets its value back afterwards
        let scope = self.scopes.len() - 1;
        let previous = self.scopes[scope].get(variable).cloned();

        let options = self.option_defaults();
        let mut flow = Flow::Normal;
        for item in items {
            // An item a branch this machine does not take appended: the body runs for it as in that branch
            let conditional = model::split_conditional_item(&item)
                .filter(|(condition, _)| !model::holds_on_host(condition, &|name| options.get(name).copied()))
                .map(|(condition, value)| (condition, value.to_string()));
            if let Some((condition, value)) = conditional {
                let before = self.variable_state();
                self.conditions.push(condition.clone());
                self.set_variable(variable, &value);
                self.loops.push((variable.clone(), value));
                let result = self.run_block(body);
                self.loops.pop();
                self.conditions.pop();

                let appended = self.appended_items(&before, &condition);
                self.restore_variable_state(&before);
                for appended in appended {
                    self.keep_appended_items(appended);
                }
                if let Flow::Return(inner) = result? {
                    flow = Flow::Return(Truth::Depends(condition).and(inner));
                }
                continue;
            }

            self.set_variable(variable, &item);
            self.loops.push((variable.clone(), item));
            let result = self.run_block(body);
            self.loops.pop();
            match result? {
                Flow::Break => break,
                returned @ Flow::Return(_) => {
                    flow = returned;
                    break;
                }
                _ => {}
            }
        }

        if let Some(scope) = self.scopes.get_mut(scope) {
            match previous {
                Some(previous) => scope.insert(variable.clone(), previous),
                None => scope.remove(variable),
            };
        }
        Ok(flow)
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

    /// Applies a directory's settings to the targets defined in it: its definitions and include folders (also those
    /// added after a target was made, as in CMake) and the flag variables' final values. Its compile options,
    /// libraries and link options are copied when each target is made.
    fn finish_directory(&mut self, dir_id: usize) {
        self.apply_flag_variables(dir_id);
        let state = self.dirs[dir_id].clone();

        for target in self
            .targets
            .values_mut()
            .filter(|target| target.dir_id == dir_id || target.other_dir_ids.contains(&dir_id))
        {
            let mut includes = state.includes.clone();
            includes.append(&mut target.includes);
            target.includes = includes;

            let mut defines = state.defines.clone();
            defines.append(&mut target.defines);
            target.defines = defines;
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
                if operand.starts_with("ENV{") {
                    Truth::Known(std::env::var(name).is_ok())
                } else {
                    match self.get_variable(name) {
                        // A platform variable is defined where it applies (MSVC with MSVC, ...)
                        Some(value) if value.starts_with(SYMBOL_PREFIX) => model::symbol_truth(&value[SYMBOL_PREFIX.len()..]),
                        Some(value) => conditional_value_truth(&value).unwrap_or(Truth::Known(true)),
                        None => Truth::Known(false),
                    }
                }
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
            return model::symbol_truth(symbol);
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
            Some(value) if value.starts_with(SYMBOL_PREFIX) => model::symbol_truth(&value[SYMBOL_PREFIX.len()..]),
            Some(value) => conditional_value_truth(&value).unwrap_or(Truth::Known(model::constant_truth(&value).unwrap_or(true))),
            None => Truth::Known(false),
        }
    }

    /// An option() that nothing set since is a condition of its own.
    pub(super) fn option_truth(&self, name: &str) -> Option<Truth> {
        if self.options.contains_key(name) && !self.overridden_options.contains(name) {
            return Some(Truth::Depends(name.to_string()));
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
        }

        // MATCHES sets CMAKE_MATCH_<n>
        if operator == "MATCHES" {
            let Ok(pattern) = regex::Regex::new(&super::check_script::anvil_regex(&right_value)) else {
                return Truth::Known(false);
            };
            let captures = pattern.captures(&left_value).map(|captures| {
                (0..captures.len())
                    .map(|index| captures.get(index).map(|found| found.as_str().to_string()).unwrap_or_default())
                    .collect::<Vec<String>>()
            });
            self.set_match_variables(captures.as_deref().unwrap_or_default());
            return Truth::Known(captures.is_some());
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
        let getter = |name: &str| self.get_variable(name);
        model::symbol_matches(symbol, &|candidate| {
            if symbol_on_left {
                compare_values(candidate, operator, other, &getter)
            } else {
                compare_values(other, operator, candidate, &getter)
            }
        })
    }
}

/// A platform symbol's value on this machine, for text that embeds it.
fn symbol_host_text(symbol: &str) -> &'static str {
    let on = |flag: bool| if flag { "1" } else { "" };

    match symbol {
        "WIN32" | "CMAKE_HOST_WIN32" | "MSVC" => on(cfg!(windows)),
        "CMAKE_COMPILER_IS_GNUCXX" | "CMAKE_COMPILER_IS_GNUCC" => on(!cfg!(windows)),
        "MSVC_VERSION" if cfg!(windows) => "1950",
        "UNIX" | "CMAKE_HOST_UNIX" => on(cfg!(unix)),
        "LINUX" => on(cfg!(target_os = "linux")),
        "APPLE" | "CMAKE_HOST_APPLE" => on(cfg!(target_os = "macos")),
        "CMAKE_CXX_COMPILER_ID" | "CMAKE_C_COMPILER_ID" | "CMAKE_CXX_COMPILER_FRONTEND_VARIANT" | "CMAKE_C_COMPILER_FRONTEND_VARIANT" => {
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
    in_genex_argument(before, &["BOOL", "NOT", "AND", "OR"], &["AND", "OR"])
}

/// Is text being expanded where a generator expression tests a value: a condition as above, either side of
/// `$<STREQUAL:` or `$<EQUAL:`, the condition of `$<IF:`, or the head of `$<...:value>`?
fn in_genex_test(before: &str) -> bool {
    before.ends_with("$<")
        || in_genex_argument(
            before,
            &["BOOL", "NOT", "AND", "OR", "STREQUAL", "EQUAL", "IF"],
            &["AND", "OR", "STREQUAL", "EQUAL"],
        )
}

/// Is text right after `$<NAME:` for a name in `first`, or right after a comma in an expression named in `later`?
fn in_genex_argument(before: &str, first: &[&str], later: &[&str]) -> bool {
    if first.iter().any(|name| before.ends_with(&format!("$<{name}:"))) {
        return true;
    }
    if !before.ends_with(',') {
        return false;
    }

    // After a comma: the innermost open expression
    let mut depth = 0;
    let characters: Vec<char> = before.chars().collect();
    for index in (0..characters.len()).rev() {
        match characters[index] {
            '>' => depth += 1,
            '<' if index > 0 && characters[index - 1] == '$' => {
                if depth == 0 {
                    let opened: String = characters[index + 1..].iter().collect();
                    return later.iter().any(|name| opened.starts_with(&format!("{name}:")));
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    false
}

/// A value made of conditional items only (a list a branch this machine does not take appended to): where it is
/// not empty.
fn conditional_value_truth(value: &str) -> Option<Truth> {
    let items = model::split_list(value);
    let conditions: Vec<(String, &str)> = items.iter().map(|item| model::split_conditional_item(item)).collect::<Option<Vec<_>>>()?;

    let truth = conditions.into_iter().fold(Truth::Known(false), |result, (condition, item)| {
        result.or(Truth::Depends(condition).and(Truth::Known(model::constant_truth(item).unwrap_or(true))))
    });
    Some(truth)
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
        "MATCHES" => regex::Regex::new(&super::check_script::anvil_regex(right)).is_ok_and(|pattern| pattern.is_match(left)),
        "IN_LIST" => model::split_list(&variable(right).unwrap_or_default()).iter().any(|item| item == left),
        "PATH_EQUAL" => util::path_key(Path::new(left)) == util::path_key(Path::new(right)),
        _ => false,
    }
}

pub(super) fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs a CMakeLists.txt in a fresh temporary folder and returns the interpreter.
    fn run(name: &str, cmake: &str) -> Interpreter {
        let folder = std::env::temp_dir().join(format!("anvil-interp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("CMakeLists.txt"), cmake).unwrap();

        let mut interpreter = Interpreter::new(&folder);
        interpreter.run_root().unwrap();
        let _ = std::fs::remove_dir_all(&folder);
        interpreter
    }

    fn variable(interpreter: &Interpreter, name: &str) -> String {
        interpreter.get_variable(name).unwrap_or_default()
    }

    #[test]
    fn only_this_machines_branch_sets_variables() {
        let cmake = r#"
option(OPT "o" OFF)
set(X a)
if(OPT)
  set(X b)
endif()
if(WIN32)
  set(Y win)
else()
  set(Y other)
endif()
if(NOT WIN32)
  set(Z other)
else()
  set(Z win)
endif()
set(SOURCES main.c)
if(UNIX)
  list(APPEND SOURCES unix.c)
endif()
if(OPT)
  set(OPT_DIR opt/dir)
endif()
if(OPT)
  set(USED "${OPT_DIR}/file")
endif()
set(OUTSIDE "${OPT_DIR}")
"#;
        let interpreter = run("branches", cmake);
        let host = if cfg!(windows) { "win" } else { "other" };

        assert_eq!(variable(&interpreter, "X"), "a");
        assert_eq!(variable(&interpreter, "Y"), host);
        assert_eq!(variable(&interpreter, "Z"), host);

        // A value an option's branch set is plain inside the same option's branch
        assert_eq!(
            model::split_conditional_item(&variable(&interpreter, "USED")),
            Some(("OPT".to_string(), "opt/dir/file"))
        );
        assert_eq!(
            model::split_conditional_item(&variable(&interpreter, "OUTSIDE")),
            Some(("OPT".to_string(), "opt/dir"))
        );

        // The other platform's items stay, under its condition
        let sources = model::list_elements(&variable(&interpreter, "SOURCES"));
        assert_eq!(sources[0], "main.c");
        if cfg!(windows) {
            assert_eq!(model::split_conditional_item(&sources[1]), Some(("unix".to_string(), "unix.c")));
        }
    }

    #[test]
    fn escapes_are_read_once() {
        let cmake = r#"
set(a "x\\ny")
set(b "${a}")
set(c "a\\$b")
set(d "PATH=C:/a\;C:/b")
set(e "tab\there")
set(f x\ y)
"#;
        let interpreter = run("escapes", cmake);

        assert_eq!(variable(&interpreter, "a"), "x\\ny");
        assert_eq!(variable(&interpreter, "b"), "x\\ny");
        assert_eq!(variable(&interpreter, "c"), "a\\$b");
        assert_eq!(variable(&interpreter, "d"), "PATH=C:/a\\;C:/b");
        assert_eq!(model::split_list(&variable(&interpreter, "d")), vec!["PATH=C:/a;C:/b"]);
        assert_eq!(variable(&interpreter, "e"), "tab\there");
        assert_eq!(variable(&interpreter, "f"), "x y");
    }

    #[test]
    fn platform_versions_and_matches() {
        let cmake = r#"
set(v "1.2.3")
if(v MATCHES "^([0-9]+)\\.([0-9]+)")
  set(MAJOR "${CMAKE_MATCH_1}")
endif()
set(text "a\nVERSION 7.")
string(REGEX MATCH "VERSION (.)\\." found "${text}")
set(L "a;;b")
list(LENGTH L length)
set(i keep)
foreach(i 1 2)
endforeach()
macro(leave)
  return()
endmacro()
function(f)
  leave()
  set(AFTER_RETURN 1 PARENT_SCOPE)
endfunction()
f()
set(TOP 1 PARENT_SCOPE)
"#;
        let mut interpreter = run("versions", cmake);

        assert_eq!(variable(&interpreter, "MAJOR"), "1");
        assert_eq!(variable(&interpreter, "CMAKE_MATCH_1"), "7");
        assert_eq!(variable(&interpreter, "length"), "3");
        assert_eq!(variable(&interpreter, "i"), "keep");
        assert!(interpreter.get_variable("AFTER_RETURN").is_none());
        assert!(interpreter.get_variable("TOP").is_none());

        // Version and DEFINED tests depend on the compiler
        let condition = |interpreter: &mut Interpreter, text: &str| {
            let args: Vec<Arg> = text
                .split(' ')
                .map(|word| Arg {
                    text: word.to_string(),
                    kind: ArgKind::Unquoted,
                })
                .collect();
            interpreter.evaluate_condition(&args)
        };
        assert_eq!(
            condition(&mut interpreter, "MSVC_VERSION GREATER_EQUAL 1910"),
            Truth::Depends("msvc".into())
        );
        assert_eq!(condition(&mut interpreter, "MSVC_VERSION"), Truth::Depends("msvc".into()));
        assert_eq!(condition(&mut interpreter, "DEFINED MSVC"), Truth::Depends("msvc".into()));
        assert_eq!(condition(&mut interpreter, "CMAKE_COMPILER_IS_GNUCXX"), Truth::Depends("gcc".into()));
        assert_eq!(
            condition(&mut interpreter, "CMAKE_CXX_COMPILER_VERSION VERSION_LESS 9"),
            Truth::Known(false)
        );
        assert_eq!(
            condition(&mut interpreter, "CMAKE_CXX_COMPILER_VERSION VERSION_GREATER_EQUAL 5"),
            Truth::Known(true)
        );
        assert_eq!(
            condition(&mut interpreter, "CMAKE_CXX_COMPILER_VERSION VERSION_LESS 19.20"),
            Truth::Depends("gcc".into())
        );
    }

    #[test]
    fn platform_values_in_generator_expressions_stay_conditions() {
        let cmake = r#"
set(a $<$<BOOL:${WIN32}>:ws2_32>)
set(b $<$<STREQUAL:${CMAKE_CXX_COMPILER_ID},GNU>:-Wshadow>)
"#;
        let interpreter = run("genex", cmake);
        let mut unknown = Vec::new();

        let a = model::expand_genex(&variable(&interpreter, "a"), &mut unknown, None);
        assert_eq!(a, vec![(Truth::Depends("windows".into()), "ws2_32".to_string())]);
        let b = model::expand_genex(&variable(&interpreter, "b"), &mut unknown, None);
        assert_eq!(b, vec![(Truth::Depends("gcc".into()), "-Wshadow".to_string())]);
        assert!(unknown.is_empty());
    }
}
