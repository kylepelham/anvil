//! Runs what CMake package files (<Name>Config.cmake, <Name>Targets.cmake and the files they include) are written in:
//! a safe part of CMake's language. Variables, conditions, loops, macros and functions, include() and file(GLOB), and
//! the commands that define imported targets and set their properties; never a program, never a write to the disk.
//! A command it does not know does nothing. A variable no command it ran set (a find module's result, the user's)
//! reads as a mark naming it, which every value made from it carries (unknown_variables), so a property built from
//! one is reported instead of guessed.

use crate::convert::parse::{self, Arg, ArgKind, Command, Node};
use crate::util::{self, slash};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

// The mark of an unknown variable in a value: start, its name, the file naming it, end
const UNKNOWN_START: char = '\u{1}';
const UNKNOWN_FILE: char = '\u{2}';
const UNKNOWN_END: char = '\u{3}';

// Limits that keep a strange file from running away: nested includes and calls, and commands run in all
const MAXIMUM_DEPTH: usize = 64;
const MAXIMUM_COMMANDS: usize = 1_000_000;

// Commands `if(COMMAND ...)` finds besides the files' own functions
const BUILTIN_COMMANDS: &[&str] = &[
    "set",
    "unset",
    "list",
    "string",
    "get_filename_component",
    "file",
    "include",
    "add_library",
    "add_executable",
    "set_target_properties",
    "set_property",
    "get_target_property",
    "get_property",
    "target_link_libraries",
    "target_include_directories",
    "target_compile_definitions",
    "target_compile_options",
    "target_compile_features",
    "target_link_options",
    "find_package",
    "message",
];

// Properties of the consuming target that Anvil's targets have values for, so $<TARGET_PROPERTY:...> of them is unknown
// (crate::vcpkg_tree reads the others as unset)
pub(crate) const CONSUMER_PROPERTIES: &[&str] = &[
    "TYPE",
    "NAME",
    "WIN32_EXECUTABLE",
    "MACOSX_BUNDLE",
    "POSITION_INDEPENDENT_CODE",
    "CXX_STANDARD",
    "C_STANDARD",
    "CXX_EXTENSIONS",
    "C_EXTENSIONS",
    "OUTPUT_NAME",
    "SOURCES",
    "COMPILE_DEFINITIONS",
    "COMPILE_OPTIONS",
    "INCLUDE_DIRECTORIES",
    "LINK_LIBRARIES",
    "LINK_OPTIONS",
    "MSVC_RUNTIME_LIBRARY",
    "AUTOMOC",
    "AUTOUIC",
    "AUTORCC",
];

/// An imported target the files define: its kind (`add_library(<name> <kind> IMPORTED)`, EXECUTABLE for
/// add_executable; empty when none says), the target an alias names, and its properties in the order first set.
#[derive(Debug, Default)]
pub(crate) struct ScriptTarget {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) aliased: Option<String>,
    pub(crate) properties: Vec<(String, String)>,
}

/// A function or macro a file defines.
struct Function {
    params: Vec<String>,
    body: Rc<Vec<Node>>,
    is_macro: bool,
}

/// How a block of commands ended.
#[derive(Debug, PartialEq)]
enum Flow {
    Normal,
    Break,
    Continue,
    Return,
}

/// One package's files as they run: the variables, functions and imported targets they define.
pub(crate) struct Script {
    variables: HashMap<String, String>,
    /// Variables set() or unset() left without a value: empty, where one never set is unknown.
    cleared: HashSet<String>,
    /// Variables a command Anvil does not run sets (find_library's): unknown, but taken as found by if().
    poisoned: HashSet<String>,
    functions: HashMap<String, Rc<Function>>,
    targets: Vec<ScriptTarget>,

    /// Each file run so far and how often (include_guard() stops a second run), by util::path_key.
    runs: HashMap<String, usize>,
    /// The file running, for the marks of unknown variables.
    file: String,
    /// For each function running, what it sets in its caller's scope (set(... PARENT_SCOPE)).
    parent_writes: Vec<Vec<(String, Option<String>)>>,
    depth: usize,
    commands: usize,
}

impl Script {
    /// A script with these variables set (CMake's own: WIN32, CMAKE_SIZEOF_VOID_P, ...) and these known to be unset.
    pub(crate) fn new(variables: Vec<(String, String)>, unset: &[&str]) -> Script {
        Script {
            variables: variables.into_iter().collect(),
            cleared: unset.iter().map(|name| name.to_string()).collect(),
            poisoned: HashSet::new(),
            functions: HashMap::new(),
            targets: Vec::new(),
            runs: HashMap::new(),
            file: String::new(),
            parent_writes: Vec::new(),
            depth: 0,
            commands: 0,
        }
    }

    /// Sets a variable before a file runs.
    pub(crate) fn set(&mut self, name: &str, value: &str) {
        self.set_variable(name, Some(value.to_string()));
    }

    pub(crate) fn has_run(&self, file: &Path) -> bool {
        self.runs.contains_key(&util::path_key(file))
    }

    /// Runs a file as include() does: with its own CMAKE_CURRENT_LIST_FILE and CMAKE_CURRENT_LIST_DIR, in the
    /// variables of whatever ran before.
    pub(crate) fn run_file(&mut self, file: &Path) {
        if self.depth >= MAXIMUM_DEPTH {
            return;
        }
        let Some(nodes) = parsed(file) else { return };

        // The list file variables are the file's while it runs, its includer's again after
        let saved: Vec<(&str, Option<String>)> = ["CMAKE_CURRENT_LIST_FILE", "CMAKE_CURRENT_LIST_DIR"]
            .into_iter()
            .map(|name| (name, self.variables.get(name).cloned()))
            .collect();
        let saved_file = std::mem::replace(&mut self.file, slash(file));
        self.set_variable("CMAKE_CURRENT_LIST_FILE", Some(slash(file)));
        self.set_variable("CMAKE_CURRENT_LIST_DIR", Some(file.parent().map(slash).unwrap_or_default()));
        *self.runs.entry(util::path_key(file)).or_default() += 1;

        self.depth += 1;
        self.run_nodes(&nodes);
        self.depth -= 1;

        for (name, value) in saved {
            self.set_variable(name, value);
        }
        self.file = saved_file;
    }

    pub(crate) fn into_targets(self) -> Vec<ScriptTarget> {
        self.targets
    }

    fn run_nodes(&mut self, nodes: &[Node]) -> Flow {
        for node in nodes {
            self.commands += 1;
            if self.commands > MAXIMUM_COMMANDS {
                return Flow::Return;
            }

            let flow = match node {
                Node::Command(command) => self.run_command(command),
                Node::If { branches, otherwise } => self.run_if(branches, otherwise.as_deref()),
                Node::Foreach { args, body } => self.run_foreach(args, body),
                // Package files have no use for them, and one could run forever
                Node::While { .. } => Flow::Normal,
                Node::Function {
                    name,
                    params,
                    body,
                    is_macro,
                } => {
                    let function = Function {
                        params: params.clone(),
                        body: body.clone(),
                        is_macro: *is_macro,
                    };
                    self.functions.insert(name.clone(), Rc::new(function));
                    Flow::Normal
                }
            };
            if flow != Flow::Normal {
                return flow;
            }
        }

        Flow::Normal
    }

    fn run_if(&mut self, branches: &[(Vec<Arg>, Vec<Node>)], otherwise: Option<&[Node]>) -> Flow {
        for (condition, body) in branches {
            if self.condition(condition) {
                return self.run_nodes(body);
            }
        }

        otherwise.map_or(Flow::Normal, |body| self.run_nodes(body))
    }

    fn run_foreach(&mut self, args: &[Arg], body: &[Node]) -> Flow {
        let words = self.arguments(args);
        let Some((variable, rest)) = words.split_first() else {
            return Flow::Normal;
        };

        // The items: a range, lists and items after IN, or the words themselves
        let items: Vec<String> = match rest.first().map(String::as_str) {
            Some("RANGE") => {
                let numbers: Vec<i64> = rest[1..].iter().filter_map(|word| word.parse().ok()).collect();
                let (start, stop, step) = match numbers[..] {
                    [stop] => (0, stop, 1),
                    [start, stop] => (start, stop, 1),
                    [start, stop, step] if step > 0 => (start, stop, step),
                    _ => return Flow::Normal,
                };
                (start..=stop).step_by(step as usize).map(|number| number.to_string()).collect()
            }
            Some("IN") => {
                let mut items = Vec::new();
                let mut lists = false;
                for word in &rest[1..] {
                    match word.as_str() {
                        "LISTS" => lists = true,
                        "ITEMS" => lists = false,
                        "ZIP_LISTS" => return Flow::Normal,
                        name if lists => items.extend(split_list(&self.list_value(name))),
                        item => items.push(item.to_string()),
                    }
                }
                items
            }
            _ => rest.to_vec(),
        };

        // The loop variable has its old value again after the loop
        let saved = self.variables.get(variable).cloned();
        let mut flow = Flow::Normal;
        for item in items {
            self.set_variable(variable, Some(item));
            match self.run_nodes(body) {
                Flow::Break => break,
                Flow::Return => {
                    flow = Flow::Return;
                    break;
                }
                _ => {}
            }
        }
        self.set_variable(variable, saved);

        flow
    }

    /// Calls a function (its own scope; set(... PARENT_SCOPE) reaches the caller's) or a macro (the caller's scope,
    /// its parameters set while it runs).
    fn call(&mut self, function: &Function, args: Vec<String>) -> Flow {
        if self.depth >= MAXIMUM_DEPTH {
            return Flow::Normal;
        }

        // The parameters, ARGC, ARGV, ARGN and ARGV0...
        let mut bindings: Vec<(String, String)> = function
            .params
            .iter()
            .enumerate()
            .map(|(index, param)| (param.clone(), args.get(index).cloned().unwrap_or_default()))
            .collect();
        bindings.push(("ARGC".into(), args.len().to_string()));
        bindings.push(("ARGV".into(), args.join(";")));
        bindings.push(("ARGN".into(), args.get(function.params.len()..).unwrap_or_default().join(";")));
        bindings.extend(args.iter().enumerate().map(|(index, arg)| (format!("ARGV{index}"), arg.clone())));

        self.depth += 1;
        let flow = if function.is_macro {
            let saved: Vec<(String, Option<String>)> = bindings
                .iter()
                .map(|(name, _)| (name.clone(), self.variables.get(name).cloned()))
                .collect();
            for (name, value) in bindings {
                self.set_variable(&name, Some(value));
            }
            let flow = self.run_nodes(&function.body);
            for (name, value) in saved {
                self.set_variable(&name, value);
            }
            flow
        } else {
            let saved = (self.variables.clone(), self.cleared.clone(), self.poisoned.clone());
            self.parent_writes.push(Vec::new());
            for (name, value) in bindings {
                self.set_variable(&name, Some(value));
            }
            self.run_nodes(&function.body);

            (self.variables, self.cleared, self.poisoned) = saved;
            for (name, value) in self.parent_writes.pop().unwrap_or_default() {
                self.set_variable(&name, value);
            }
            Flow::Normal
        };
        self.depth -= 1;

        // return() in a macro leaves what called it; break() and continue() reach its loop
        flow
    }

    fn set_variable(&mut self, name: &str, value: Option<String>) {
        self.poisoned.remove(name);
        match value {
            Some(value) => {
                self.cleared.remove(name);
                self.variables.insert(name.to_string(), value);
            }
            None => {
                self.variables.remove(name);
                self.cleared.insert(name.to_string());
            }
        }
    }

    /// A variable a command Anvil does not run would set: unknown from now on.
    fn poison(&mut self, name: &str) {
        let mark = self.mark(name);
        self.set_variable(name, Some(mark));
        self.poisoned.insert(name.to_string());
    }

    fn mark(&self, name: &str) -> String {
        format!("{UNKNOWN_START}{name}{UNKNOWN_FILE}{}{UNKNOWN_END}", self.file)
    }

    /// A variable's value as a list command reads it: one never set is an empty list.
    fn list_value(&self, name: &str) -> String {
        self.variables.get(name).cloned().unwrap_or_default()
    }

    /// A command's arguments with variables and escapes evaluated; unquoted ones split at `;`, without empty items.
    fn arguments(&self, args: &[Arg]) -> Vec<String> {
        let mut words = Vec::new();
        for arg in args {
            let value = self.expand(arg);
            match arg.kind {
                ArgKind::Unquoted => words.extend(split_list(&value).into_iter().filter(|item| !item.is_empty())),
                _ => words.push(value),
            }
        }

        words
    }

    /// An argument's text with its escapes and variable references evaluated (a bracket argument as written).
    fn expand(&self, arg: &Arg) -> String {
        if arg.kind == ArgKind::Bracket {
            return arg.text.clone();
        }

        let characters: Vec<char> = arg.text.chars().collect();
        let mut output = String::new();
        let mut index = 0;
        while index < characters.len() {
            match characters[index] {
                '\\' if index + 1 < characters.len() => {
                    match characters[index + 1] {
                        'n' => output.push('\n'),
                        't' => output.push('\t'),
                        'r' => output.push('\r'),
                        ';' => output.push_str("\\;"),
                        other => output.push(other),
                    }
                    index += 2;
                }
                '$' => match self.reference(&characters, index) {
                    Some((value, end)) => {
                        output.push_str(&value);
                        index = end;
                    }
                    None => {
                        output.push('$');
                        index += 1;
                    }
                },
                other => {
                    output.push(other);
                    index += 1;
                }
            }
        }

        output
    }

    /// At `$`: the value of `${name}`, `$ENV{name}` or `$CACHE{name}` (names may hold references) and the index after
    /// it; None when no reference starts there.
    fn reference(&self, characters: &[char], start: usize) -> Option<(String, usize)> {
        let rest: String = characters[start + 1..].iter().take(6).collect();
        let (kind, open) = if rest.starts_with('{') {
            ("", start + 1)
        } else if rest.starts_with("ENV{") {
            ("ENV", start + 4)
        } else if rest.starts_with("CACHE{") {
            ("CACHE", start + 6)
        } else {
            return None;
        };

        // The name, references in it evaluated first
        let mut name = String::new();
        let mut index = open + 1;
        loop {
            match characters.get(index)? {
                '}' => break,
                '$' => match self.reference(characters, index) {
                    Some((value, end)) => {
                        name.push_str(&value);
                        index = end;
                    }
                    None => {
                        name.push('$');
                        index += 1;
                    }
                },
                other => {
                    name.push(*other);
                    index += 1;
                }
            }
        }

        let value = match kind {
            "ENV" => std::env::var(&name).unwrap_or_default(),
            _ => self.lookup(&name),
        };
        Some((value, index + 1))
    }

    /// A variable's value: empty when a command unset it, its mark when none set it.
    fn lookup(&self, name: &str) -> String {
        match self.variables.get(name) {
            Some(value) => value.clone(),
            None if self.cleared.contains(name) => String::new(),
            None => self.mark(name),
        }
    }
}

/// The parsed commands of a file, each file parsed once while it is unchanged; None when it cannot be read or parsed.
fn parsed(file: &Path) -> Option<Rc<Vec<Node>>> {
    type Cache = HashMap<(PathBuf, Option<i64>), Option<Rc<Vec<Node>>>>;
    thread_local! {
        static PARSED: RefCell<Cache> = RefCell::new(HashMap::new());
    }

    let key = (file.to_path_buf(), util::mtime(file));
    PARSED.with(|cache| {
        cache
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| parse::parse_file(file).ok().map(Rc::new))
            .clone()
    })
}

/// A list's items, split at the `;` not escaped (`\;`); empty items kept, none for an empty text.
fn split_list(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }

    let mut items = Vec::new();
    let mut item = String::new();
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\\' if characters.peek() == Some(&';') => {
                item.push_str("\\;");
                characters.next();
            }
            ';' => items.push(std::mem::take(&mut item)),
            other => item.push(other),
        }
    }
    items.push(item);

    items
}

/// The unknown variables a value holds, each with the file that named it.
pub(crate) fn unknown_variables(value: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find(UNKNOWN_START) {
        let after = &rest[start + UNKNOWN_START.len_utf8()..];
        let Some(end) = after.find(UNKNOWN_END) else { break };
        let (name, file) = after[..end].split_once(UNKNOWN_FILE).unwrap_or((&after[..end], ""));
        if !found.iter().any(|(known, _): &(String, String)| known == name) {
            found.push((name.to_string(), file.to_string()));
        }
        rest = &after[end + UNKNOWN_END.len_utf8()..];
    }

    found
}

/// Whether a value holds an unknown variable.
pub(crate) fn is_unknown(value: &str) -> bool {
    value.contains(UNKNOWN_START)
}

/// A value with its unknown variables read as empty, as CMake reads a variable no one set.
fn without_unknown(value: &str) -> String {
    let mut output = String::new();
    let mut rest = value;
    while let Some(start) = rest.find(UNKNOWN_START) {
        output.push_str(&rest[..start]);
        match rest[start..].find(UNKNOWN_END) {
            Some(end) => rest = &rest[start + end + UNKNOWN_END.len_utf8()..],
            None => rest = "",
        }
    }
    output.push_str(rest);

    output
}

/// `result`, computed from `input`: unknown when the input held unknown variables the result lost.
fn carry_unknown(input: &str, result: String) -> String {
    let marks: Vec<String> = unknown_marks(input).into_iter().filter(|mark| !result.contains(mark.as_str())).collect();
    if marks.is_empty() {
        result
    } else {
        marks.concat()
    }
}

/// The marks of unknown variables in a value, as written.
fn unknown_marks(value: &str) -> Vec<String> {
    let mut marks = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find(UNKNOWN_START) {
        let Some(end) = rest[start..].find(UNKNOWN_END) else { break };
        let stop = start + end + UNKNOWN_END.len_utf8();
        marks.push(rest[start..stop].to_string());
        rest = &rest[stop..];
    }

    marks
}

// Commands
impl Script {
    fn run_command(&mut self, command: &Command) -> Flow {
        let words = self.arguments(&command.args);
        if let Some(function) = self.functions.get(&command.name).cloned() {
            return self.call(&function, words);
        }

        match command.name.as_str() {
            // Control
            "return" => return Flow::Return,
            "break" => return Flow::Break,
            "continue" => return Flow::Continue,
            "include_guard" if self.runs.get(&util::path_key(Path::new(&self.file))).is_some_and(|runs| *runs > 1) => return Flow::Return,
            "include" => self.include(&words),

            // Variables
            "set" => self.command_set(&words),
            "unset" => self.command_unset(&words),
            "option" => {
                if let [name, _, rest @ ..] = &words[..] {
                    if !self.variables.contains_key(name) {
                        self.set_variable(name, Some(rest.first().cloned().unwrap_or_else(|| "OFF".into())));
                    }
                }
            }
            "list" => self.command_list(&words),
            "string" => self.command_string(&words),
            "get_filename_component" => self.get_filename_component(&words),
            "file" => self.command_file(&words),

            // Targets
            "add_library" | "add_executable" => self.add_target(&command.name, &words),
            "set_target_properties" => self.set_target_properties(&words),
            "set_property" => self.command_set_property(&words),
            "get_target_property" => self.get_target_property(&words),
            "get_property" => self.command_get_property(&words),
            "target_link_libraries" => self.target_command(&words, "INTERFACE_LINK_LIBRARIES"),
            "target_include_directories" => self.target_command(&words, "INTERFACE_INCLUDE_DIRECTORIES"),
            "target_compile_definitions" => self.target_command(&words, "INTERFACE_COMPILE_DEFINITIONS"),
            "target_compile_options" => self.target_command(&words, "INTERFACE_COMPILE_OPTIONS"),
            "target_compile_features" => self.target_command(&words, "INTERFACE_COMPILE_FEATURES"),
            "target_link_options" => self.target_command(&words, "INTERFACE_LINK_OPTIONS"),
            "target_link_directories" => self.target_command(&words, "INTERFACE_LINK_DIRECTORIES"),

            // Other packages are read by their own users (crate::cmake_package): taken as found here
            "find_package" | "find_dependency" => {
                if let Some(name) = words.first() {
                    self.set_variable(&format!("{name}_FOUND"), Some("TRUE".into()));
                    self.set_variable(&format!("{}_FOUND", name.to_uppercase()), Some("TRUE".into()));
                }
            }

            // Commands Anvil does not run that set variables: those are unknown
            "find_library" | "find_path" | "find_file" | "find_program" => {
                if let Some(name) = words.first() {
                    let found = self.variables.get(name).is_some_and(|value| !value.ends_with("NOTFOUND"));
                    if !found {
                        self.poison(name);
                    }
                }
            }
            "execute_process" => {
                for (index, word) in words.iter().enumerate() {
                    if word.ends_with("_VARIABLE") {
                        if let Some(name) = words.get(index + 1) {
                            self.poison(name);
                        }
                    }
                }
            }
            "math" | "cmake_path" => {
                if let Some(name) = words.get(1) {
                    self.poison(name);
                }
                if let Some(name) = words.last() {
                    self.poison(name);
                }
            }
            "separate_arguments" | "get_directory_property" | "get_source_file_property" | "get_test_property" | "site_name" => {
                if let Some(name) = words.first() {
                    self.poison(name);
                }
            }
            "get_cmake_property" => {
                if let Some(name) = words.first() {
                    self.set_variable(name, None);
                }
            }
            _ => {}
        }

        Flow::Normal
    }

    /// include(<file> [OPTIONAL] [RESULT_VARIABLE <name>]): a file by its full path (modules of CMake's own, such as
    /// CMakeFindDependencyMacro, are not there).
    fn include(&mut self, words: &[String]) {
        let Some(file) = words.first() else { return };
        let path = PathBuf::from(file);
        let found = !is_unknown(file) && path.is_absolute() && path.is_file();
        if found {
            self.run_file(&path);
        }

        if let Some(index) = words.iter().position(|word| word == "RESULT_VARIABLE") {
            if let Some(name) = words.get(index + 1) {
                let value = if found { file.clone() } else { "NOTFOUND".to_string() };
                self.set_variable(name, Some(value));
            }
        }
    }

    /// set(<name> <values>... [PARENT_SCOPE] [CACHE <type> <doc> [FORCE]]); no values unsets it.
    fn command_set(&mut self, words: &[String]) {
        let Some((name, rest)) = words.split_first() else { return };
        if name.starts_with("ENV{") {
            return;
        }

        if let Some(cache) = rest.iter().position(|word| word == "CACHE") {
            let force = rest[cache..].iter().any(|word| word == "FORCE");
            if force || !self.variables.contains_key(name) {
                self.set_variable(name, Some(rest[..cache].join(";")));
            }
            return;
        }
        if rest.last().is_some_and(|word| word == "PARENT_SCOPE") {
            let values = &rest[..rest.len() - 1];
            let value = (!values.is_empty()).then(|| values.join(";"));
            if let Some(writes) = self.parent_writes.last_mut() {
                writes.push((name.clone(), value));
            }
            return;
        }

        let value = (!rest.is_empty()).then(|| rest.join(";"));
        self.set_variable(name, value);
    }

    fn command_unset(&mut self, words: &[String]) {
        let Some(name) = words.first() else { return };
        if name.starts_with("ENV{") {
            return;
        }

        if words.iter().any(|word| word == "PARENT_SCOPE") {
            if let Some(writes) = self.parent_writes.last_mut() {
                writes.push((name.clone(), None));
            }
        } else {
            self.set_variable(name, None);
        }
    }

    /// list(): the operations that read and change lists; the others make their variables unknown.
    fn command_list(&mut self, words: &[String]) {
        let [operation, list_name, rest @ ..] = words else { return };
        let mut items = split_list(&self.list_value(list_name));

        // Operations that answer in a variable
        let answer = match (operation.as_str(), rest) {
            ("LENGTH", [out]) => Some((out, items.len().to_string())),
            ("GET", [indices @ .., out]) if !indices.is_empty() => {
                let picked: Option<Vec<String>> = indices
                    .iter()
                    .map(|index| list_index(index, items.len()).and_then(|index| items.get(index).cloned()))
                    .collect();
                Some((out, picked.map(|picked| picked.join(";")).unwrap_or_else(|| "NOTFOUND".into())))
            }
            ("FIND", [value, out]) => {
                let position = items.iter().position(|item| item == value);
                Some((out, position.map_or("-1".to_string(), |position| position.to_string())))
            }
            ("JOIN", [glue, out]) => Some((out, items.join(glue))),
            ("SUBLIST", [begin, length, out]) => {
                let begin: usize = begin.parse().unwrap_or(usize::MAX);
                let length: i64 = length.parse().unwrap_or(-1);
                let end = if length < 0 {
                    items.len()
                } else {
                    (begin + length as usize).min(items.len())
                };
                Some((out, items.get(begin..end).map(|slice| slice.join(";")).unwrap_or_default()))
            }
            _ => None,
        };
        if let Some((out, value)) = answer {
            self.set_variable(out, Some(value));
            return;
        }

        // Operations that change the list
        match (operation.as_str(), rest) {
            ("APPEND", values) if !values.is_empty() => items.extend(values.iter().cloned()),
            ("PREPEND", values) if !values.is_empty() => {
                items.splice(0..0, values.iter().cloned());
            }
            ("INSERT", [index, values @ ..]) => {
                let Some(index) = list_index(index, items.len() + 1) else { return };
                items.splice(index..index, values.iter().cloned());
            }
            ("REMOVE_ITEM", values) => items.retain(|item| !values.contains(item)),
            ("REMOVE_AT", indices) => {
                let removed: Vec<usize> = indices.iter().filter_map(|index| list_index(index, items.len())).collect();
                let mut position = 0;
                items.retain(|_| {
                    position += 1;
                    !removed.contains(&(position - 1))
                });
            }
            ("REMOVE_DUPLICATES", []) => {
                let mut seen = HashSet::new();
                items.retain(|item| seen.insert(item.clone()));
            }
            ("REVERSE", []) => items.reverse(),
            ("SORT", _) => items.sort(),
            ("FILTER", [mode, regex_word, pattern]) if regex_word == "REGEX" => {
                let Ok(regex) = regex::Regex::new(pattern) else {
                    return self.poison(list_name);
                };
                let include = mode == "INCLUDE";
                items.retain(|item| regex.is_match(item) == include);
            }
            ("APPEND" | "PREPEND", _) => return,
            _ => {
                self.poison(list_name);
                if let Some(last) = rest.last() {
                    self.poison(last);
                }
                return;
            }
        }
        self.set_variable(list_name, Some(items.join(";")));
    }

    /// string(): regular expressions, replacing, joining, case, stripping, finding; the other operations make their
    /// variables unknown.
    fn command_string(&mut self, words: &[String]) {
        let Some((operation, rest)) = words.split_first() else { return };

        let answer: Option<(&String, String, String)> = match (operation.as_str(), rest) {
            ("REGEX", [mode, pattern, rest @ ..]) => {
                let Ok(regex) = regex::Regex::new(pattern) else {
                    if let Some(last) = rest.last() {
                        self.poison(last);
                    }
                    return;
                };
                match (mode.as_str(), rest) {
                    ("MATCH", [out, inputs @ ..]) => {
                        let input = inputs.concat();
                        let found = regex.find(&input).map(|found| found.as_str().to_string()).unwrap_or_default();
                        Some((out, input, found))
                    }
                    ("MATCHALL", [out, inputs @ ..]) => {
                        let input = inputs.concat();
                        let found: Vec<&str> = regex.find_iter(&input).map(|found| found.as_str()).collect();
                        let found = found.join(";");
                        Some((out, input, found))
                    }
                    ("REPLACE", [replacement, out, inputs @ ..]) => {
                        let input = inputs.concat();
                        let replaced = regex.replace_all(&input, cmake_replacement(replacement).as_str()).into_owned();
                        Some((out, input, replaced))
                    }
                    _ => None,
                }
            }
            ("REPLACE", [from, to, out, inputs @ ..]) if !from.is_empty() => {
                let input = inputs.concat();
                let replaced = input.replace(from.as_str(), to);
                Some((out, input, replaced))
            }
            ("APPEND", [name, inputs @ ..]) => Some((name, String::new(), self.list_value(name) + &inputs.concat())),
            ("PREPEND", [name, inputs @ ..]) => Some((name, String::new(), inputs.concat() + &self.list_value(name))),
            ("CONCAT", [out, inputs @ ..]) => Some((out, String::new(), inputs.concat())),
            ("JOIN", [glue, out, inputs @ ..]) => Some((out, String::new(), inputs.join(glue))),
            ("TOLOWER", [input, out]) => Some((out, input.clone(), input.to_lowercase())),
            ("TOUPPER", [input, out]) => Some((out, input.clone(), input.to_uppercase())),
            ("STRIP", [input, out]) => Some((out, input.clone(), input.trim().to_string())),
            ("LENGTH", [input, out]) => Some((out, input.clone(), input.len().to_string())),
            ("FIND", [input, needle, out, rest @ ..]) => {
                let found = if rest.iter().any(|word| word == "REVERSE") {
                    input.rfind(needle.as_str())
                } else {
                    input.find(needle.as_str())
                };
                Some((out, input.clone(), found.map_or("-1".to_string(), |found| found.to_string())))
            }
            ("SUBSTRING", [input, begin, length, out]) => {
                let begin: usize = begin.parse().unwrap_or(0).min(input.len());
                let length: i64 = length.parse().unwrap_or(-1);
                let end = if length < 0 {
                    input.len()
                } else {
                    (begin + length as usize).min(input.len())
                };
                Some((out, input.clone(), input.get(begin..end).unwrap_or_default().to_string()))
            }
            ("COMPARE", [comparison, left, right, out]) => {
                let holds = match comparison.as_str() {
                    "EQUAL" => left == right,
                    "NOTEQUAL" => left != right,
                    "LESS" => left < right,
                    "GREATER" => left > right,
                    "LESS_EQUAL" => left <= right,
                    "GREATER_EQUAL" => left >= right,
                    _ => false,
                };
                Some((out, format!("{left}{right}"), if holds { "1" } else { "0" }.to_string()))
            }
            _ => None,
        };

        match answer {
            Some((out, input, result)) => {
                let out = out.clone();
                self.set_variable(&out, Some(carry_unknown(&input, result)));
            }
            None => {
                for name in [rest.first(), rest.last()].into_iter().flatten().cloned().collect::<Vec<_>>() {
                    self.poison(&name);
                }
            }
        }
    }

    /// get_filename_component(<name> <path> <mode> [BASE_DIR <dir>] [CACHE]).
    fn get_filename_component(&mut self, words: &[String]) {
        let [name, path, mode, rest @ ..] = words else { return };
        if rest.iter().any(|word| word == "CACHE") && self.variables.contains_key(name) {
            return;
        }

        let path = path.replace('\\', "/");
        let file_name = path.rsplit('/').next().unwrap_or_default().to_string();
        let base = match rest.iter().position(|word| word == "BASE_DIR") {
            Some(index) => rest.get(index + 1).cloned().unwrap_or_default(),
            None => self.lookup("CMAKE_CURRENT_SOURCE_DIR"),
        };

        let value = match mode.as_str() {
            "DIRECTORY" | "PATH" => match path.trim_end_matches('/').rfind('/') {
                Some(0) => "/".to_string(),
                Some(index) => path[..index].to_string(),
                None => String::new(),
            },
            "NAME" => file_name,
            "EXT" => file_name.find('.').map(|dot| file_name[dot..].to_string()).unwrap_or_default(),
            "NAME_WE" => file_name.split('.').next().unwrap_or_default().to_string(),
            "LAST_EXT" => file_name.rfind('.').map(|dot| file_name[dot..].to_string()).unwrap_or_default(),
            "NAME_WLE" => file_name.rfind('.').map_or(file_name.clone(), |dot| file_name[..dot].to_string()),
            "ABSOLUTE" => absolute_path(&path, &base),
            "REALPATH" => real_path(&absolute_path(&path, &base)),
            _ => return self.poison(name),
        };
        self.set_variable(name, Some(value));
    }

    /// file(GLOB), file(TO_CMAKE_PATH), file(REAL_PATH); the operations that read files make their variables unknown,
    /// and those that write or download never run.
    fn command_file(&mut self, words: &[String]) {
        let Some((operation, rest)) = words.split_first() else { return };

        match (operation.as_str(), rest) {
            ("GLOB", [name, rest @ ..]) => {
                let value = glob(rest);
                match value {
                    Some(files) => self.set_variable(name, Some(files.join(";"))),
                    None => self.poison(name),
                }
            }
            ("TO_CMAKE_PATH", [path, out]) => self.set_variable(out, Some(path.replace('\\', "/"))),
            ("TO_NATIVE_PATH", [path, out]) => {
                let native = if cfg!(windows) { path.replace('/', "\\") } else { path.clone() };
                self.set_variable(out, Some(native));
            }
            ("REAL_PATH", [path, out, rest @ ..]) => {
                let base = match rest.iter().position(|word| word == "BASE_DIRECTORY") {
                    Some(index) => rest.get(index + 1).cloned().unwrap_or_default(),
                    None => self.lookup("CMAKE_CURRENT_SOURCE_DIR"),
                };
                let value = real_path(&absolute_path(&path.replace('\\', "/"), &base));
                self.set_variable(out, Some(value));
            }
            ("RELATIVE_PATH" | "GLOB_RECURSE", [name, ..]) => self.poison(name),
            ("READ" | "STRINGS" | "SIZE" | "TIMESTAMP" | "READ_SYMLINK" | "MD5" | "SHA1" | "SHA256" | "SHA512", [_, name, ..]) => self.poison(name),
            _ => {}
        }
    }
}

/// A list index, negative ones from the end; None outside `0..length`.
fn list_index(text: &str, length: usize) -> Option<usize> {
    let index: i64 = text.parse().ok()?;
    let index = if index < 0 { length as i64 + index } else { index };
    (0..length as i64).contains(&index).then_some(index as usize)
}

/// A REGEX REPLACE replacement in the regex crate's syntax: `\1` is `${1}`, a `$` is itself.
fn cmake_replacement(replacement: &str) -> String {
    let mut output = String::new();
    let mut characters = replacement.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\\' => match characters.next() {
                Some(digit) if digit.is_ascii_digit() => output.push_str(&format!("${{{digit}}}")),
                Some(other) => output.push(other),
                None => {}
            },
            '$' => output.push_str("$$"),
            other => output.push(other),
        }
    }

    output
}

/// A path made absolute against `base` and normalized, with forward slashes.
fn absolute_path(path: &str, base: &str) -> String {
    let rooted = path.starts_with('/') || Path::new(path).is_absolute();
    let joined = if rooted {
        path.to_string()
    } else {
        format!("{}/{path}", base.trim_end_matches('/'))
    };

    // The root (`/`, `C:/`, `//server`), then the components with `.` and `..` resolved
    let (root, rest) = if let Some(rest) = joined.strip_prefix("//") {
        ("//", rest)
    } else if let Some(rest) = joined.strip_prefix('/') {
        ("/", rest)
    } else if joined.as_bytes().get(1) == Some(&b':') && joined.as_bytes().get(2) == Some(&b'/') {
        (&joined[..3], &joined[3..])
    } else {
        ("", joined.as_str())
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|last| *last != "..") => {
                parts.pop();
            }
            ".." if !root.is_empty() => {}
            part => parts.push(part),
        }
    }

    format!("{root}{}", parts.join("/"))
}

/// A path with its symbolic links resolved, when it exists; else as it is.
fn real_path(path: &str) -> String {
    if is_unknown(path) {
        return path.to_string();
    }

    match std::fs::canonicalize(path) {
        Ok(resolved) => {
            let text = slash(&resolved);
            text.strip_prefix("//?/").map(str::to_string).unwrap_or(text)
        }
        Err(_) => path.to_string(),
    }
}

/// file(GLOB)'s matches: the files and folders whose names match a pattern's last part (`*`, `?`), sorted; None when
/// a pattern's folder is unknown or has wildcards.
fn glob(words: &[String]) -> Option<Vec<String>> {
    let mut relative: Option<&String> = None;
    let mut patterns = Vec::new();
    let mut index = 0;
    while index < words.len() {
        match words[index].as_str() {
            "LIST_DIRECTORIES" => index += 1,
            "RELATIVE" => {
                relative = words.get(index + 1);
                index += 1;
            }
            "CONFIGURE_DEPENDS" => {}
            pattern => patterns.push(pattern.replace('\\', "/")),
        }
        index += 1;
    }

    let mut found = Vec::new();
    for pattern in patterns {
        let (folder, name_pattern) = pattern.rsplit_once('/')?;
        if is_unknown(folder) || folder.contains(['*', '?', '[']) {
            return None;
        }
        let Ok(entries) = std::fs::read_dir(folder) else { continue };
        let mut names: Vec<String> = entries
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| wildcard_match(name_pattern, name))
            .collect();
        names.sort();
        found.extend(names.into_iter().map(|name| format!("{folder}/{name}")));
    }

    if let Some(relative) = relative {
        let prefix = format!("{}/", relative.trim_end_matches('/'));
        found = found
            .into_iter()
            .map(|file| file.strip_prefix(&prefix).map(str::to_string).unwrap_or(file))
            .collect();
    }

    Some(found)
}

/// Whether `name` matches a glob pattern of `*` (any run) and `?` (any one character).
fn wildcard_match(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let mut position = 0;
    let mut matched = 0;
    let mut star: Option<(usize, usize)> = None;

    while matched < name.len() {
        match pattern.get(position) {
            Some('*') => {
                star = Some((position, matched));
                position += 1;
            }
            Some(character) if *character == '?' || *character == name[matched] => {
                position += 1;
                matched += 1;
            }
            _ => match star {
                Some((star_position, star_matched)) => {
                    position = star_position + 1;
                    matched = star_matched + 1;
                    star = Some((star_position, star_matched + 1));
                }
                None => return false,
            },
        }
    }

    pattern[position..].iter().all(|character| *character == '*')
}

// Targets
impl Script {
    /// add_library(<name> <kind> IMPORTED), add_executable(<name> IMPORTED), and the ALIAS forms of both.
    fn add_target(&mut self, command: &str, words: &[String]) {
        let [name, rest @ ..] = words else { return };

        if let [alias, aliased] = rest {
            if alias == "ALIAS" {
                let target = self.target_named(name);
                target.kind = "INTERFACE".into();
                target.aliased = Some(aliased.clone());
                return;
            }
        }
        if !rest.iter().any(|word| word == "IMPORTED") {
            return;
        }

        let kind = match command {
            "add_executable" => "EXECUTABLE".to_string(),
            _ => rest.first().filter(|word| *word != "IMPORTED").cloned().unwrap_or_default(),
        };
        self.target_named(name).kind = kind;
    }

    /// set_target_properties(<targets>... PROPERTIES <name> <value>...).
    fn set_target_properties(&mut self, words: &[String]) {
        let Some(split) = words.iter().position(|word| word == "PROPERTIES") else {
            return;
        };
        for name in &words[..split] {
            for [property, value] in words[split + 1..].as_chunks::<2>().0 {
                self.set_target_property(name, property, Some(value.clone()));
            }
        }
    }

    /// set_property(TARGET <targets>... [APPEND|APPEND_STRING] PROPERTY <name> <values>...); other scopes are not
    /// Anvil's business.
    fn command_set_property(&mut self, words: &[String]) {
        let [scope, rest @ ..] = words else { return };
        if scope != "TARGET" {
            return;
        }
        let Some(property_index) = rest.iter().position(|word| word == "PROPERTY") else {
            return;
        };
        let Some(property) = rest.get(property_index + 1) else { return };

        let append = rest[..property_index].iter().any(|word| word == "APPEND");
        let append_string = rest[..property_index].iter().any(|word| word == "APPEND_STRING");
        let names = rest[..property_index]
            .iter()
            .filter(|word| !matches!(word.as_str(), "APPEND" | "APPEND_STRING"));
        let values = rest[property_index + 2..].join(";");
        for name in names.cloned().collect::<Vec<_>>() {
            let old = self.target_property(&name, property).unwrap_or_default();
            let value = if append_string {
                Some(old + &values)
            } else if append && !old.is_empty() && !values.is_empty() {
                Some(format!("{old};{values}"))
            } else if append && values.is_empty() {
                Some(old)
            } else {
                (!values.is_empty()).then_some(values.clone())
            };
            self.set_target_property(&name, property, value);
        }
    }

    /// target_link_libraries(), target_include_directories() and the like on an imported target: what they give
    /// its users (INTERFACE and PUBLIC items, or items after no keyword) appended to `property`.
    fn target_command(&mut self, words: &[String], property: &str) {
        let [name, rest @ ..] = words else { return };

        let mut items: Vec<String> = Vec::new();
        let mut used = true;
        let mut configuration: Option<&str> = None;
        for word in rest {
            match word.as_str() {
                "INTERFACE" | "PUBLIC" | "LINK_PUBLIC" | "LINK_INTERFACE_LIBRARIES" => used = true,
                "PRIVATE" | "LINK_PRIVATE" => used = false,
                "SYSTEM" | "BEFORE" | "AFTER" => {}
                "debug" | "optimized" | "general" if property == "INTERFACE_LINK_LIBRARIES" => configuration = Some(word),
                item if used => {
                    let item = if property == "INTERFACE_COMPILE_DEFINITIONS" {
                        item.strip_prefix("-D").unwrap_or(item)
                    } else {
                        item
                    };
                    items.push(match configuration.take() {
                        Some("debug") => format!("$<$<CONFIG:Debug>:{item}>"),
                        Some("optimized") => format!("$<$<NOT:$<CONFIG:Debug>>:{item}>"),
                        _ => item.to_string(),
                    });
                }
                _ => {}
            }
        }
        if items.is_empty() {
            return;
        }

        let old = self.target_property(name, property).unwrap_or_default();
        let value = if old.is_empty() {
            items.join(";")
        } else {
            format!("{old};{}", items.join(";"))
        };
        self.set_target_property(name, property, Some(value));
    }

    /// get_target_property(<name> <target> <property>): the value, `<name>-NOTFOUND` when it has none.
    fn get_target_property(&mut self, words: &[String]) {
        let [name, target, property, ..] = words else { return };
        let value = self.target_property(target, property).unwrap_or_else(|| format!("{name}-NOTFOUND"));
        self.set_variable(name, Some(value));
    }

    /// get_property(<name> TARGET <target> PROPERTY <property> [SET]); other scopes read as unset.
    fn command_get_property(&mut self, words: &[String]) {
        let [name, scope, rest @ ..] = words else { return };
        let property = rest.iter().position(|word| word == "PROPERTY").and_then(|index| rest.get(index + 1));

        let value = match (scope.as_str(), rest.first(), property) {
            ("TARGET", Some(target), Some(property)) => {
                let value = self.target_property(target, property);
                if rest.iter().any(|word| word == "SET") {
                    Some(if value.is_some() { "1" } else { "0" }.to_string())
                } else {
                    value
                }
            }
            _ => None,
        };
        self.set_variable(name, value);
    }

    /// A property of a target the files defined (an alias reads its target's), with the ones CMake computes.
    fn target_property(&self, name: &str, property: &str) -> Option<String> {
        let target = self.targets.iter().find(|target| target.name == name)?;
        if property == "ALIASED_TARGET" {
            return target.aliased.clone();
        }
        if let Some(aliased) = &target.aliased {
            if aliased != name {
                return self.target_property(aliased, property);
            }
        }

        match property {
            "TYPE" => Some(match target.kind.as_str() {
                "EXECUTABLE" => "EXECUTABLE".to_string(),
                kind => format!("{kind}_LIBRARY"),
            }),
            "NAME" => Some(target.name.clone()),
            "IMPORTED" => Some("TRUE".to_string()),
            _ => target
                .properties
                .iter()
                .find(|(known, _)| known == property)
                .map(|(_, value)| value.clone()),
        }
    }

    fn set_target_property(&mut self, name: &str, property: &str, value: Option<String>) {
        let target = self.target_named(name);
        let position = target.properties.iter().position(|(known, _)| known == property);
        match (position, value) {
            (Some(index), Some(value)) => target.properties[index].1 = value,
            (Some(index), None) => {
                target.properties.remove(index);
            }
            (None, Some(value)) => target.properties.push((property.to_string(), value)),
            (None, None) => {}
        }
    }

    /// The target of this name, added when no file defined it yet.
    fn target_named(&mut self, name: &str) -> &mut ScriptTarget {
        let index = match self.targets.iter().position(|target| target.name == name) {
            Some(index) => index,
            None => {
                self.targets.push(ScriptTarget {
                    name: name.to_string(),
                    ..Default::default()
                });
                self.targets.len() - 1
            }
        };

        &mut self.targets[index]
    }
}

// Conditions
impl Script {
    /// Whether an if() or elseif() condition holds, as CMake decides it (quoted arguments are never variable names).
    fn condition(&mut self, args: &[Arg]) -> bool {
        let mut tokens: Vec<(String, bool)> = Vec::new();
        for arg in args {
            let value = self.expand(arg);
            match arg.kind {
                ArgKind::Unquoted => tokens.extend(split_list(&value).into_iter().filter(|item| !item.is_empty()).map(|item| (item, false))),
                _ => tokens.push((value, true)),
            }
        }

        let mut position = 0;
        self.condition_or(&tokens, &mut position)
    }

    fn condition_or(&mut self, tokens: &[(String, bool)], position: &mut usize) -> bool {
        let mut holds = self.condition_and(tokens, position);
        while is_keyword(tokens, *position, "OR") {
            *position += 1;
            let right = self.condition_and(tokens, position);
            holds = holds || right;
        }

        holds
    }

    fn condition_and(&mut self, tokens: &[(String, bool)], position: &mut usize) -> bool {
        let mut holds = self.condition_not(tokens, position);
        while is_keyword(tokens, *position, "AND") {
            *position += 1;
            let right = self.condition_not(tokens, position);
            holds = holds && right;
        }

        holds
    }

    fn condition_not(&mut self, tokens: &[(String, bool)], position: &mut usize) -> bool {
        if is_keyword(tokens, *position, "NOT") {
            *position += 1;
            return !self.condition_not(tokens, position);
        }

        self.condition_primary(tokens, position)
    }

    /// A group in parentheses, a test (EXISTS <path>, DEFINED <name>, ...), a comparison, or one value.
    fn condition_primary(&mut self, tokens: &[(String, bool)], position: &mut usize) -> bool {
        if is_keyword(tokens, *position, "(") {
            *position += 1;
            let holds = self.condition_or(tokens, position);
            if is_keyword(tokens, *position, ")") {
                *position += 1;
            }
            return holds;
        }

        // Tests of one argument
        let Some((first, quoted)) = tokens.get(*position) else { return false };
        if !quoted {
            if let Some((operand, _)) = tokens.get(*position + 1) {
                let operand = without_unknown(operand);
                let path = Path::new(&operand);
                let tested = match first.as_str() {
                    "EXISTS" => Some(path.is_absolute() && path.exists()),
                    "IS_DIRECTORY" => Some(path.is_absolute() && path.is_dir()),
                    "IS_SYMLINK" => Some(path.is_symlink()),
                    "IS_ABSOLUTE" => Some(operand.starts_with('/') || path.is_absolute()),
                    "IS_READABLE" | "IS_WRITABLE" | "IS_EXECUTABLE" => Some(path.exists()),
                    "DEFINED" => Some(match (operand.strip_prefix("ENV{"), operand.strip_prefix("CACHE{")) {
                        (Some(name), _) => std::env::var_os(name.trim_end_matches('}')).is_some(),
                        (_, Some(name)) => self.variables.contains_key(name.trim_end_matches('}')),
                        _ => self.variables.contains_key(&operand),
                    }),
                    "TARGET" => Some(self.targets.iter().any(|target| target.name == operand)),
                    "COMMAND" => {
                        Some(self.functions.contains_key(&operand.to_lowercase()) || BUILTIN_COMMANDS.contains(&operand.to_lowercase().as_str()))
                    }
                    "POLICY" => Some(true),
                    "TEST" => Some(false),
                    _ => None,
                };
                if let Some(holds) = tested {
                    *position += 2;
                    return holds;
                }
            }
        }

        // A comparison, or the value alone
        let left = tokens[*position].clone();
        *position += 1;
        let operator = tokens
            .get(*position)
            .filter(|(word, quoted)| !quoted && is_comparison(word))
            .map(|(word, _)| word.clone());
        match (operator, tokens.get(*position + 1).cloned()) {
            (Some(operator), Some(right)) => {
                *position += 2;
                self.compare(&left, &operator, &right)
            }
            _ => self.truth(&left),
        }
    }

    /// Whether one value holds: a constant, else (unquoted) a variable whose value is not a false constant.
    fn truth(&self, (text, quoted): &(String, bool)) -> bool {
        let known = without_unknown(text);
        if let Some(holds) = constant_truth(&known) {
            return holds;
        }
        if *quoted {
            return false;
        }

        // A variable a command Anvil does not run set is taken as found
        if self.poisoned.contains(text.as_str()) {
            return true;
        }
        self.variables
            .get(text.as_str())
            .is_some_and(|value| !constant_truth(&without_unknown(value)).is_some_and(|holds| !holds))
    }

    fn compare(&mut self, left: &(String, bool), operator: &str, right: &(String, bool)) -> bool {
        let left_value = self.operand(left);

        match operator {
            "IN_LIST" => split_list(&self.list_value(&right.0)).contains(&left_value),
            "MATCHES" => {
                let Ok(regex) = regex::Regex::new(&without_unknown(&right.0)) else {
                    return false;
                };
                let Some(captures) = regex.captures(&left_value) else { return false };
                for (index, group) in captures.iter().enumerate() {
                    self.set_variable(
                        &format!("CMAKE_MATCH_{index}"),
                        Some(group.map(|group| group.as_str().to_string()).unwrap_or_default()),
                    );
                }
                true
            }
            _ => {
                let right_value = self.operand(right);
                let ordering = if operator.starts_with("VERSION_") {
                    compare_versions(&left_value, &right_value)
                } else if operator.starts_with("STR") {
                    left_value.cmp(&right_value)
                } else if operator == "PATH_EQUAL" {
                    absolute_path(&left_value, "").cmp(&absolute_path(&right_value, ""))
                } else {
                    match (left_value.trim().parse::<f64>(), right_value.trim().parse::<f64>()) {
                        (Ok(left), Ok(right)) => left.partial_cmp(&right).unwrap_or(std::cmp::Ordering::Less),
                        _ => return false,
                    }
                };

                // STRLESS_EQUAL and VERSION_LESS_EQUAL test what LESS_EQUAL does
                let relation = operator
                    .trim_start_matches("VERSION_")
                    .trim_start_matches("STR")
                    .trim_start_matches("PATH_");
                match relation {
                    "EQUAL" => ordering.is_eq(),
                    "LESS" => ordering.is_lt(),
                    "GREATER" => ordering.is_gt(),
                    "LESS_EQUAL" => ordering.is_le(),
                    "GREATER_EQUAL" => ordering.is_ge(),
                    _ => false,
                }
            }
        }
    }

    /// A comparison's operand: an unquoted variable name's value, else the text.
    fn operand(&self, (text, quoted): &(String, bool)) -> String {
        match self.variables.get(text.as_str()).filter(|_| !quoted) {
            Some(value) => without_unknown(value),
            None => without_unknown(text),
        }
    }
}

/// Whether a token at `position` is this keyword (unquoted).
fn is_keyword(tokens: &[(String, bool)], position: usize, keyword: &str) -> bool {
    tokens.get(position).is_some_and(|(word, quoted)| !quoted && word == keyword)
}

fn is_comparison(word: &str) -> bool {
    matches!(
        word,
        "EQUAL"
            | "LESS"
            | "GREATER"
            | "LESS_EQUAL"
            | "GREATER_EQUAL"
            | "STREQUAL"
            | "STRLESS"
            | "STRGREATER"
            | "STRLESS_EQUAL"
            | "STRGREATER_EQUAL"
            | "VERSION_EQUAL"
            | "VERSION_LESS"
            | "VERSION_GREATER"
            | "VERSION_LESS_EQUAL"
            | "VERSION_GREATER_EQUAL"
            | "MATCHES"
            | "IN_LIST"
            | "PATH_EQUAL"
    )
}

/// The truth of a CMake constant (ON, 0, NOTFOUND, a non-zero number, ...); None for any other text.
fn constant_truth(text: &str) -> Option<bool> {
    let upper = text.to_uppercase();
    if matches!(upper.as_str(), "1" | "ON" | "YES" | "TRUE" | "Y") {
        return Some(true);
    }
    if matches!(upper.as_str(), "" | "0" | "OFF" | "NO" | "FALSE" | "N" | "IGNORE" | "NOTFOUND") || upper.ends_with("-NOTFOUND") {
        return Some(false);
    }

    text.trim().parse::<f64>().ok().map(|number| number != 0.0)
}

/// Versions compared number by number ("3.10" is newer than "3.9"), missing numbers 0.
fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    let numbers = |text: &str| -> Vec<u64> {
        text.split('.')
            .map(|part| part.chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0))
            .collect()
    };
    let left = numbers(left);
    let right = numbers(right);

    (0..left.len().max(right.len()))
        .map(|index| left.get(index).unwrap_or(&0).cmp(right.get(index).unwrap_or(&0)))
        .find(|ordering| ordering.is_ne())
        .unwrap_or(std::cmp::Ordering::Equal)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A script that ran `text` as a file, with WIN32 set.
    fn run(test: &str, text: &str) -> Script {
        let folder = std::env::temp_dir().join(format!("anvil-cmake-script-{test}-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join("FooConfig.cmake");
        std::fs::write(&file, text).unwrap();

        let mut script = Script::new(vec![("WIN32".into(), "1".into())], &["EMPTY"]);
        script.run_file(&file);
        std::fs::remove_dir_all(&folder).ok();
        script
    }

    fn variable(script: &Script, name: &str) -> String {
        script.variables.get(name).cloned().unwrap_or_else(|| panic!("{name} is not set"))
    }

    #[test]
    fn conditions() {
        let script = run(
            "conditions",
            r#"set(A 1)
set(EMPTYISH "")
set(LIST "x;y")
if(A AND NOT B AND WIN32)
  set(r1 yes)
endif()
if("A")
  set(r2 quoted_name_is_not_a_variable)
else()
  set(r2 no)
endif()
if((NOT A OR A) AND "3.10" VERSION_GREATER 3.9 AND 10 GREATER 9 AND "b" STRGREATER "a")
  set(r3 yes)
endif()
if(x IN_LIST LIST AND DEFINED EMPTYISH AND NOT DEFINED NEVER AND "${NEVER}" STREQUAL "")
  set(r4 yes)
endif()
if("libfoo.so.1" MATCHES "^lib(.*)\\.so")
  set(r5 ${CMAKE_MATCH_1})
endif()
if(COMMAND set AND NOT TARGET Foo::foo AND POLICY CMP0057 AND 1.0 LESS_EQUAL 1 AND "a" STREQUAL "a")
  set(r6 yes)
elseif(1)
  set(r6 elseif)
endif()
"#,
        );
        assert_eq!(variable(&script, "r1"), "yes");
        assert_eq!(variable(&script, "r2"), "no");
        assert_eq!(variable(&script, "r3"), "yes");
        assert_eq!(variable(&script, "r4"), "yes");
        assert_eq!(variable(&script, "r5"), "foo");
        assert_eq!(variable(&script, "r6"), "yes");
    }

    #[test]
    fn functions_macros_and_lists() {
        let script = run(
            "functions",
            r#"function(set_twice name value)
  set(local "${value}${value}")
  set(${name} "${local}" PARENT_SCOPE)
  set(seen_args "${ARGC}:${ARGN}" PARENT_SCOPE)
endfunction()
macro(set_once name value)
  set(${name} "${value}")
endmacro()
set_twice(twice ab extra)
set_once(once cd)
set(items c a b a)
list(APPEND items d)
list(REMOVE_DUPLICATES items)
list(REMOVE_ITEM items b)
list(INSERT items 0 first)
list(LENGTH items count)
list(GET items -1 last)
list(FIND items a found)
foreach(item IN ITEMS 1 2 3)
  if(item EQUAL 2)
    continue()
  endif()
  list(APPEND picked ${item})
endforeach()
foreach(number RANGE 1 5 2)
  string(APPEND numbers "${number}")
endforeach()
string(REPLACE "a" "o" replaced "banana")
string(TOUPPER "abc" upper)
string(REGEX REPLACE "([a-z]+)-([0-9]+)" "\\2-\\1" swapped "lib-42")
get_filename_component(folder "/a/b/../c/file.tar.gz" DIRECTORY)
get_filename_component(stem "/a/c/file.tar.gz" NAME_WE)
get_filename_component(absolute "/a/b/../c/./d/" ABSOLUTE)
set(empty_join "${EMPTY}-")
"#,
        );
        assert_eq!(variable(&script, "twice"), "abab");
        assert_eq!(variable(&script, "seen_args"), "3:extra");
        assert!(!script.variables.contains_key("local"));
        assert_eq!(variable(&script, "once"), "cd");
        assert_eq!(variable(&script, "items"), "first;c;a;d");
        assert_eq!(
            [variable(&script, "count"), variable(&script, "last"), variable(&script, "found")],
            ["4", "d", "2"]
        );
        assert_eq!(variable(&script, "picked"), "1;3");
        assert_eq!(variable(&script, "numbers"), "135");
        assert_eq!(variable(&script, "replaced"), "bonono");
        assert_eq!(variable(&script, "upper"), "ABC");
        assert_eq!(variable(&script, "swapped"), "42-lib");
        assert_eq!(variable(&script, "folder"), "/a/b/../c");
        assert_eq!(variable(&script, "stem"), "file");
        assert_eq!(variable(&script, "absolute"), "/a/c/d");
        assert_eq!(variable(&script, "empty_join"), "-");
    }

    #[test]
    fn unknown_variables_are_marked() {
        let script = run(
            "unknown",
            "set(path \"${NEVER_SET}/lib\")\nfind_library(FOO_LIB foo)\nif(FOO_LIB)\n  set(found yes)\nendif()\nif(NEVER_SET)\n  set(never yes)\nendif()\n\
             string(REGEX MATCHALL \"[a-z]+\" words \"${NEVER_SET}\")\nset(escaped \"\\${NOT_A_REFERENCE}\")\nfile(WRITE \"/never/written\" \"x\")\n",
        );
        let path = variable(&script, "path");
        assert!(is_unknown(&path) && path.ends_with("/lib"));
        let unknown = unknown_variables(&path);
        assert_eq!(unknown[0].0, "NEVER_SET");
        assert!(unknown[0].1.ends_with("FooConfig.cmake"), "{unknown:?}");
        assert_eq!(without_unknown(&path), "/lib");

        // A find_library result is taken as found; a variable never set is not
        assert!(is_unknown(&variable(&script, "FOO_LIB")));
        assert_eq!(variable(&script, "found"), "yes");
        assert!(!script.variables.contains_key("never"));

        // What is computed from an unknown value is unknown; an escaped reference is text
        assert!(is_unknown(&variable(&script, "words")));
        assert_eq!(variable(&script, "escaped"), "${NOT_A_REFERENCE}");
        assert!(!Path::new("/never/written").exists());
    }

    #[test]
    fn targets_and_properties() {
        let script = run(
            "targets",
            "add_library(Foo::foo STATIC IMPORTED)\nadd_library(Foo::alias ALIAS Foo::foo)\nadd_executable(Foo::tool IMPORTED)\n\
             set_target_properties(Foo::foo PROPERTIES INTERFACE_COMPILE_DEFINITIONS \"A\" IMPORTED_LOCATION \"/x/libfoo.a\")\n\
             set_property(TARGET Foo::foo APPEND PROPERTY INTERFACE_COMPILE_DEFINITIONS B C)\nset_property(TARGET Foo::foo APPEND_STRING PROPERTY NOTE \"x\")\n\
             set_property(TARGET Foo::foo APPEND_STRING PROPERTY NOTE \"y\")\ntarget_compile_definitions(Foo::foo INTERFACE -DD PRIVATE E)\n\
             target_link_libraries(Foo::foo INTERFACE bar debug bard optimized baro)\nget_target_property(type Foo::alias TYPE)\n\
             get_target_property(missing Foo::foo NOPE)\nget_property(location TARGET Foo::foo PROPERTY IMPORTED_LOCATION)\n\
             set_property(TARGET Foo::tool PROPERTY IMPORTED_LOCATION /x/tool)\nset_property(TARGET Foo::tool PROPERTY IMPORTED_LOCATION)\n",
        );
        assert_eq!(variable(&script, "type"), "STATIC_LIBRARY");
        assert_eq!(variable(&script, "missing"), "missing-NOTFOUND");
        assert_eq!(variable(&script, "location"), "/x/libfoo.a");

        let targets = script.into_targets();
        let property = |target: &ScriptTarget, name: &str| target.properties.iter().find(|(known, _)| known == name).map(|(_, value)| value.clone());
        assert_eq!(property(&targets[0], "INTERFACE_COMPILE_DEFINITIONS").unwrap(), "A;B;C;D");
        assert_eq!(property(&targets[0], "NOTE").unwrap(), "xy");
        assert_eq!(
            property(&targets[0], "INTERFACE_LINK_LIBRARIES").unwrap(),
            "bar;$<$<CONFIG:Debug>:bard>;$<$<NOT:$<CONFIG:Debug>>:baro>"
        );
        assert_eq!(targets[1].aliased.as_deref(), Some("Foo::foo"));
        assert_eq!(
            (targets[2].kind.as_str(), property(&targets[2], "IMPORTED_LOCATION")),
            ("EXECUTABLE", None)
        );
    }

    #[test]
    fn matches_globs() {
        assert!(wildcard_match("libboost_*-variant*.cmake", "libboost_fs-variant-shared.cmake"));
        assert!(wildcard_match("a?c", "abc"));
        assert!(!wildcard_match("*.cmake", "x.txt"));
        assert_eq!(absolute_path("../x", "C:/a/b"), "C:/a/x");
        assert_eq!(absolute_path("/../x", ""), "/x");
    }
}
