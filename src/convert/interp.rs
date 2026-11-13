//! A CMake interpreter for conversion: variables, scopes, if/foreach/while, functions and macros run as CMake would,
//! while platform, compiler, build-type and option tests stay symbolic. Branches whose outcome depends on them all
//! run, under the matching condition, so the result describes every platform instead of this machine.

use super::model::{self, CustomCommand, ExportHeader, Item, Target, Test, Truth};
use super::parse::{self, Arg, ArgKind, Command, Node};
use crate::util::{self, slash};
use anyhow::Result;
use indexmap::{IndexMap, IndexSet};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::LazyLock;

// Function calls and includes nested deeper than this are runaway recursion
const MAX_DEPTH: usize = 64;

// A while() that loops this often is assumed not to terminate in conversion
const MAX_WHILE_ITERATIONS: usize = 10_000;

// Most versions one text may have, from values set per condition inside it; past this it reads as this machine's
const MAX_TEXT_VERSIONS: usize = 32;

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

// Commands that read names, paths and text at configure time, not build settings: an item a branch added under a
// condition (which the target commands keep as a generator expression) reads as this machine's there
const HOST_CHOICE_COMMANDS: &[&str] = &[
    "message",
    "include",
    "add_subdirectory",
    "project",
    "find_package",
    "find_program",
    "find_library",
    "find_path",
    "find_file",
    "pkg_check_modules",
    "configure_file",
    "execute_process",
    "file",
    "get_filename_component",
    "cmake_path",
    "set_tests_properties",
    "add_dependencies",
    "get_target_property",
    "enable_language",
];

// A scope's value for a variable it unset
const UNSET: &str = "\u{1}unset";

// Values of variables that depend on the platform or compiler; tests against them become conditions
const SYMBOL_PREFIX: &str = "\u{1}sym:";

// A platform value inside longer text, and its name
static SYMBOL_PATTERN: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(&format!("{}([A-Za-z0-9_]+)", regex::escape(SYMBOL_PREFIX))).unwrap());

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

/// The variable scopes (directory, function and block() ones), innermost last. While a branch runs under a mark,
/// each write is journaled with the value it replaced, so the branch can be read back and undone at a cost in what
/// it changed.
pub struct Scopes {
    maps: Vec<HashMap<String, String>>,
    // Each scope's id, which journal entries name (an index is reused once its scope ends)
    ids: Vec<usize>,
    next_id: usize,
    journal: Vec<(usize, String, Option<String>)>,
    marks: usize,
}

/// The cache variables, journaled as Scopes are.
pub struct Cache {
    map: HashMap<String, String>,
    journal: Vec<(String, Option<String>)>,
    marks: usize,
}

/// Where the journals stood when a branch started.
struct Mark {
    scopes: usize,
    cache: usize,
    functions: usize,
    overridden_options: HashSet<String>,
}

/// A variable a branch wrote: in a scope (by id) or the cache.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum VariableKey {
    Scope(usize, String),
    Cache(String),
}

/// The values variables had at a mark, from the journals: by scope id and name, and in the cache.
#[derive(Default)]
struct Before<'a> {
    scopes: IndexMap<(usize, &'a str), &'a Option<String>>,
    cache: IndexMap<&'a str, &'a Option<String>>,
}

/// What one branch under a condition changed: each variable's value before it and after it.
struct BranchChanges {
    condition: Truth,
    changes: IndexMap<VariableKey, (Option<String>, Option<String>)>,
}

impl Scopes {
    fn new() -> Scopes {
        Scopes {
            maps: vec![HashMap::new()],
            ids: vec![0],
            next_id: 1,
            journal: Vec::new(),
            marks: 0,
        }
    }

    pub fn push(&mut self, scope: HashMap<String, String>) {
        self.maps.push(scope);
        self.ids.push(self.next_id);
        self.next_id += 1;
    }

    pub fn pop(&mut self) -> Option<HashMap<String, String>> {
        self.ids.pop();
        self.maps.pop()
    }

    /// Sets (Some) or removes (None) a variable in the scope at `index`.
    fn set(&mut self, index: usize, name: &str, value: Option<String>) {
        let old = match value {
            Some(value) => self.maps[index].insert(name.to_string(), value),
            None => self.maps[index].remove(name),
        };
        if self.marks > 0 {
            self.journal.push((self.ids[index], name.to_string(), old));
        }
    }

    fn index_of(&self, id: usize) -> Option<usize> {
        self.ids.iter().rposition(|scope_id| *scope_id == id)
    }

    fn undo(&mut self, mark: usize) {
        while self.journal.len() > mark {
            let (id, name, old) = self.journal.pop().unwrap();
            let Some(index) = self.index_of(id) else { continue };
            match old {
                Some(old) => self.maps[index].insert(name, old),
                None => self.maps[index].remove(&name),
            };
        }
    }
}

impl std::ops::Deref for Scopes {
    type Target = [HashMap<String, String>];

    fn deref(&self) -> &Self::Target {
        &self.maps
    }
}

impl Cache {
    fn new() -> Cache {
        Cache {
            map: HashMap::new(),
            journal: Vec::new(),
            marks: 0,
        }
    }

    pub fn get(&self, name: &str) -> Option<&String> {
        self.map.get(name)
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.map.contains_key(name)
    }

    pub fn insert(&mut self, name: String, value: String) -> Option<String> {
        let old = self.map.insert(name.clone(), value);
        if self.marks > 0 {
            self.journal.push((name, old.clone()));
        }
        old
    }

    pub fn remove(&mut self, name: &str) -> Option<String> {
        let old = self.map.remove(name);
        if self.marks > 0 {
            self.journal.push((name.to_string(), old.clone()));
        }
        old
    }

    fn undo(&mut self, mark: usize) {
        while self.journal.len() > mark {
            let (name, old) = self.journal.pop().unwrap();
            match old {
                Some(old) => self.map.insert(name, old),
                None => self.map.remove(&name),
            };
        }
    }
}

/// One option's declarations: each default under its branch (None: everywhere), and whether one whose branch holds
/// on this machine set its default.
pub(super) type OptionDeclarations = (Vec<(Option<String>, bool)>, bool);

pub struct Interpreter {
    pub root: PathBuf,

    // Variables: function and directory scopes over the cache
    pub(super) scopes: Scopes,
    pub(super) cache: Cache,
    pub(super) functions: HashMap<String, Rc<Function>>,
    /// Functions a branch under a mark redefined, with their earlier definition.
    function_journal: Vec<(String, Rc<Function>)>,

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
    /// The components and version each find_package asks of a package (`find_package(Qt6 6.5 REQUIRED COMPONENTS
    /// Widgets)`), for a `cmake` dependency.
    pub package_requests: HashMap<String, (Vec<String>, Option<String>)>,
    /// Where each found package was looked for (packages not here everywhere).
    pub(super) package_conditions: HashMap<String, Truth>,
    pub fetched: IndexMap<String, (String, Option<String>)>,
    pub options: IndexMap<String, (bool, String)>,
    pub(super) overridden_options: HashSet<String>,
    pub tests: Vec<Test>,
    pub custom_commands: Vec<CustomCommand>,
    pub source_properties: Vec<SourceProperties>,
    /// Files set_source_files_properties(HEADER_FILE_ONLY) keeps out of every target (path keys), with the condition
    /// it holds under (None: always).
    pub(super) header_only_sources: HashMap<String, Option<String>>,
    /// Words added to a CMAKE_<LANG>_FLAGS variable under a condition: (variable, word) -> condition.
    pub(super) flag_word_conditions: HashMap<(String, String), String>,
    /// Words added to a flag variable in a branch this machine does not take, which puts the variable back
    /// afterwards: (variable, the word before it then, word), in the order they were added.
    pub(super) off_host_flag_words: Vec<(String, Option<String>, String)>,
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
    /// Warnings about the running command, given its location once it ran (expanding its arguments may add some).
    pub(super) command_warnings: RefCell<Vec<String>>,
    /// Each option's defaults as declared, under the branch of each declaration (None: everywhere), and whether one
    /// whose branch holds on this machine set its default.
    pub(super) option_declarations: HashMap<String, OptionDeclarations>,
    pub(super) unknown_commands: IndexSet<String>,
    /// What the built-in commands keep besides variables (builtins.rs).
    pub(super) builtin: super::builtins::BuiltinState,
}

impl Interpreter {
    pub fn new(root: &Path) -> Interpreter {
        let mut interpreter = Interpreter {
            root: root.to_path_buf(),
            scopes: Scopes::new(),
            cache: Cache::new(),
            functions: HashMap::new(),
            function_journal: Vec::new(),
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
            package_requests: HashMap::new(),
            fetched: IndexMap::new(),
            options: IndexMap::new(),
            overridden_options: HashSet::new(),
            tests: Vec::new(),
            custom_commands: Vec::new(),
            source_properties: Vec::new(),
            header_only_sources: HashMap::new(),
            flag_word_conditions: HashMap::new(),
            off_host_flag_words: Vec::new(),
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
            command_warnings: RefCell::new(Vec::new()),
            option_declarations: HashMap::new(),
            package_conditions: HashMap::new(),
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
        self.decide_target_exists();
        self.drop_unsatisfiable();

        Ok(())
    }

    /// Leaves out what is set under a condition that cannot hold (`a & !a`, or `!a` on a target made under `a`).
    fn drop_unsatisfiable(&mut self) {
        let possible = |condition: &Option<String>, within: &Option<String>| match model::and_optional(within, condition) {
            Some(both) => model::satisfiable(&both),
            None => true,
        };

        for target in self.targets.values_mut() {
            let own = target.condition.clone();
            target.language_rules.retain(|rule| possible(&rule.condition, &own));
            target.retain_items(&mut |condition| possible(condition, &own));
        }
        self.tests.retain(|test| possible(&test.condition, &None));
        self.custom_commands.retain(|command| possible(&command.condition, &None));
        self.configure_writes.retain(|write| possible(&write.condition, &None));
    }

    /// Decides the `$<TARGET_EXISTS:...>` tests in conditions now that every target is known (a target made later in
    /// the project counts too), dropping what they rule out.
    fn decide_target_exists(&mut self) {
        // The names asked about, and the answers
        let mut conditions: Vec<&String> = Vec::new();
        for target in self.targets.values() {
            conditions.extend(target.condition.iter().chain(&target.shared_condition));
            conditions.extend(target.language_rules.iter().filter_map(|rule| rule.condition.as_ref()));
            target.each_condition(&mut |condition| conditions.push(condition));
        }
        conditions.extend(self.tests.iter().filter_map(|test| test.condition.as_ref()));
        conditions.extend(self.custom_commands.iter().filter_map(|command| command.condition.as_ref()));
        conditions.extend(self.configure_writes.iter().filter_map(|write| write.condition.as_ref()));
        let names: HashSet<String> = conditions
            .into_iter()
            .flat_map(|condition| model::target_exists_names(condition))
            .collect();
        if names.is_empty() {
            return;
        }
        let existing: HashMap<String, Truth> = names.into_iter().map(|name| (name.clone(), self.target_existence(&name))).collect();

        // Each condition decided; what can no longer hold goes
        let decide = |condition: &mut Option<String>| -> bool {
            let Some(text) = condition.as_deref() else { return true };
            match model::decide_target_exists(text, &|name| existing.get(name).cloned().unwrap_or(Truth::Known(false))) {
                Truth::Known(holds) => {
                    *condition = None;
                    holds
                }
                Truth::Depends(decided) => {
                    *condition = Some(decided);
                    true
                }
            }
        };
        for target in self.targets.values_mut() {
            if !decide(&mut target.condition) {
                target.condition = Some("false".into());
            }
            decide(&mut target.shared_condition);
            target.language_rules.retain_mut(|rule| decide(&mut rule.condition));
            target.retain_items(&mut |condition| decide(condition));
        }
        self.tests.retain_mut(|test| decide(&mut test.condition));
        self.custom_commands.retain_mut(|command| decide(&mut command.condition));
        self.configure_writes.retain_mut(|write| decide(&mut write.condition));
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
        let innermost = self.scopes.len() - 1;
        self.scopes.set(innermost, name, Some(value.to_string()));
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
        self.scopes.set(count - 2, name, Some(value.to_string()));
    }

    /// Unsets a variable in the current scope: the scopes below no longer show through, as in CMake, where a
    /// function's scope is a copy of its caller's.
    pub fn unset_variable(&mut self, name: &str) {
        let count = self.scopes.len();
        self.unset_in(count - 1, name);
    }

    pub fn unset_in(&mut self, index: usize, name: &str) {
        if index == 0 {
            self.scopes.set(0, name, None);
        } else {
            self.scopes.set(index, name, Some(UNSET.to_string()));
        }
    }

    pub fn current_dir(&self) -> PathBuf {
        PathBuf::from(self.get_variable("CMAKE_CURRENT_SOURCE_DIR").unwrap_or_else(|| slash(&self.root)))
    }

    pub fn current_dir_id(&self) -> usize {
        *self.dir_stack.last().unwrap()
    }

    /// A path from CMake text: relative to the current source folder; {out} paths stay as they are. A part that
    /// branches set differently is this machine's.
    pub fn resolve_path(&self, text: &str) -> PathBuf {
        let text: &str = &model::choose_list_items(text, &|condition| {
            model::holds_on_host(condition, &|name| self.options.get(name).map(|(default, _)| *default))
        });
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

    /// Can `truth` hold together with the conditions being run under?
    fn possible_here(&self, truth: &Truth) -> bool {
        match (truth, self.current_condition()) {
            (Truth::Depends(condition), Some(current)) => model::satisfiable(&model::and(&current, condition)),
            (truth, _) => *truth != Truth::Known(false),
        }
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
        // Nothing runs where the conditions it is under cannot hold
        if self.current_condition().is_some_and(|condition| !model::satisfiable(&condition)) {
            return;
        }
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

    /// A function or macro call's arguments: as evaluate_args, but a value with one item per condition is one
    /// argument, as it is in each configuration CMake makes.
    pub(super) fn evaluate_call_args(&mut self, args: &[Arg]) -> Vec<String> {
        let mut values = Vec::new();

        for arg in args {
            match arg.kind {
                ArgKind::Unquoted => {
                    let value = self.expand_argument(&arg.text);
                    if per_condition_value(&value).is_some() {
                        values.push(value);
                    } else {
                        values.extend(model::split_list(&value));
                    }
                }
                _ => values.extend(self.evaluate_args(std::slice::from_ref(arg))),
            }
        }

        values
    }

    /// A built-in command's arguments, evaluated; for commands that read names and text at configure time, with
    /// this machine's choice of the items branches added under a condition.
    pub(super) fn command_args(&mut self, command: &Command) -> Vec<String> {
        let args = self.evaluate_args(&command.args);
        if !HOST_CHOICE_COMMANDS.contains(&command.name.as_str()) {
            return args;
        }

        let holds = |condition: &str| model::holds_on_host(condition, &|name| self.options.get(name).map(|(default, _)| *default));
        let mut chosen = Vec::new();
        for arg in args {
            match model::choose_list_items(&arg, &holds) {
                Cow::Borrowed(_) => chosen.push(arg),
                Cow::Owned(text) if model::split_conditional_item(&arg).is_some() && text.is_empty() => {}
                Cow::Owned(text) => chosen.push(text),
            }
        }
        chosen
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
        // How many versions the text has so far, from values with one item per condition
        let mut versions = 1;

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
                        let whole = output.is_empty() && end + 1 == characters.len();
                        let option_condition = kind == "var" && in_genex_condition(&output) && self.option_truth(&name).is_some();
                        let mut value = match kind {
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
                                match model::marked_condition(&value) {
                                    Some(_) if !whole && !in_genex_condition(&output) => self.host_text(&value),
                                    _ => value,
                                }
                            }
                        };

                        // So does a platform value
                        if let Some(symbol) = value.strip_prefix(SYMBOL_PREFIX) {
                            if in_genex_test(&output) {
                                value = model::symbol_marker(symbol);
                            }
                        }

                        // One value per condition, inside longer text: the text under each (`-D$<c:a>$<!c:b>`)
                        let value = self.resolve_conditional_items(value);
                        match per_condition_value(&value) {
                            Some(alternatives) if !whole && versions * alternatives.len() > MAX_TEXT_VERSIONS => {
                                let host = self.host_text(&value);
                                self.command_warnings.borrow_mut().push(format!(
                                    "`${{{name}}}` has a value per condition, too many to combine with the rest of the text \
                                     (over {MAX_TEXT_VERSIONS} versions); converted with this machine's: `{host}`"
                                ));
                                output.push_str(&host);
                            }
                            Some(alternatives) if !whole => {
                                versions *= alternatives.len();
                                output.push_str(&model::list_elements(&value).concat());
                            }
                            _ => output.push_str(&value),
                        }
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
            return SYMBOL_PATTERN
                .replace_all(&output, |captures: &regex::Captures| symbol_host_text(&captures[1]))
                .into_owned();
        }

        output
    }

    /// A value as text on this machine: platform symbols, check results and conditional items become what they are
    /// here.
    pub(super) fn host_text(&self, value: &str) -> String {
        let holds = |condition: &str| model::holds_on_host(condition, &|name| self.options.get(name).map(|(default, _)| *default));
        if let Some(condition) = model::marked_condition(value) {
            return if holds(condition) { "1" } else { "" }.to_string();
        }

        let value = model::choose_list_items(value, &holds);
        if !value.contains(SYMBOL_PREFIX) {
            return value.into_owned();
        }
        SYMBOL_PATTERN
            .replace_all(&value, |captures: &regex::Captures| symbol_host_text(&captures[1]))
            .into_owned()
    }

    /// Records where a find_package() just run looked for its package, so the targets it provides exist there.
    fn note_package_condition(&mut self, command: &Command) {
        let Some(first) = command.args.first() else { return };
        let name = self.expand_argument(&first.text);
        if !self.packages.contains(&name) {
            return;
        }
        let here = match self.current_condition() {
            Some(condition) => Truth::Depends(condition),
            None => Truth::Known(true),
        };
        let existing = self.package_conditions.get(&name).cloned().unwrap_or(Truth::Known(false));
        let either = match existing.or(here) {
            Truth::Depends(condition) => model::simplify(&condition, None),
            known => known,
        };
        self.package_conditions.insert(name, either);
    }

    /// A command-line string's words (COMPILE_FLAGS); in text set per condition, each word under the conditions of
    /// the versions that have it.
    pub(super) fn command_words(&self, text: &str) -> Vec<String> {
        let versions = text_versions(text).unwrap_or_else(|| vec![(Truth::Known(true), self.host_text(text))]);
        let mut words: Vec<(String, Truth)> = Vec::new();
        for (condition, version) in versions {
            for word in util::split_command(&version) {
                match words.iter_mut().find(|(known, _)| *known == word) {
                    Some((_, either)) => *either = either.clone().or(condition.clone()),
                    None => words.push((word, condition.clone())),
                }
            }
        }

        words
            .into_iter()
            .filter_map(|(word, condition)| match condition {
                Truth::Depends(condition) => match model::simplify(&condition, None) {
                    Truth::Depends(condition) => Some(model::conditional_item(&condition, &word)),
                    Truth::Known(holds) => holds.then_some(word),
                },
                Truth::Known(holds) => holds.then_some(word),
            })
            .collect()
    }

    /// Versions (`text_versions`) with this machine's last, so what running them leaves behind is its.
    pub(super) fn host_version_last<T>(&self, versions: &mut [(Truth, T)]) {
        let holds = |truth: &Truth| match truth {
            Truth::Known(known) => *known,
            Truth::Depends(condition) => model::holds_on_host(condition, &|name| self.options.get(name).map(|(default, _)| *default)),
        };
        versions.sort_by_key(|(condition, _)| holds(condition));
    }

    /// A variable's text in a configured file where its truth depends on a condition: for a value branches set
    /// under conditions, its items under those `choice` takes; otherwise `text` (1 for a check's result).
    pub(super) fn chosen_text(&self, value: Option<&str>, text: &str, choice: &dyn Fn(&str) -> bool) -> String {
        let Some(value) = value.filter(|value| conditional_value_truth(value).is_some()) else {
            return text.to_string();
        };

        let items = model::split_list(value);
        let chosen = items
            .iter()
            .filter_map(|item| model::split_conditional_item(item))
            .filter(|(condition, _)| choice(condition));
        chosen.map(|(_, item)| item).collect::<Vec<_>>().join(";")
    }

    // Execution

    pub fn run_block(&mut self, nodes: &[Node]) -> Result<Flow> {
        let mut pushed = 0;
        let mut flow = Flow::Normal;

        for node in nodes {
            // A return under a condition that holds everywhere returns
            let result = match self.run_node(node)? {
                Flow::Return(Truth::Depends(condition)) => Flow::Return(model::simplify(&condition, None)),
                other => other,
            };
            match result {
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
                    // One value per condition stays one operand
                    let value = self.expand_argument(&arg.text);
                    if per_condition_value(&value).is_some() {
                        tokens.push((value, false));
                        continue;
                    }

                    let items = model::split_list(&value);
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

    /// Where this target exists: a real one or an alias under the condition it was made under, one a found package
    /// provides where the package was looked for.
    pub fn target_existence(&self, name: &str) -> Truth {
        let mut name = name;
        for _ in 0..MAX_DEPTH {
            match self.aliases.get(name) {
                Some(aliased) if aliased != name => name = aliased,
                _ => break,
            }
        }
        if let Some(target) = self.targets.get(name) {
            return match &target.condition {
                Some(condition) => model::simplify(condition, None),
                None => Truth::Known(true),
            };
        }
        if !self.target_exists(name) {
            return Truth::Known(false);
        }

        let Some((namespace, short)) = name.split_once("::") else {
            return Truth::Known(true);
        };
        let normalized = |text: &str| text.to_lowercase().replace(['-', '_'], "");
        let mut existence = Truth::Known(false);
        for package in &self.packages {
            if normalized(package) == normalized(namespace) || normalized(package) == normalized(short) {
                existence = existence.or(self.package_conditions.get(package).cloned().unwrap_or(Truth::Known(true)));
            }
        }
        match existence {
            Truth::Depends(condition) => model::simplify(&condition, None),
            Truth::Known(false) => Truth::Known(true),
            known => known,
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
            Node::Command(command) => {
                let flow = self.run_command(command);
                if command.name == "find_package" {
                    self.note_package_condition(command);
                }
                for message in self.command_warnings.take() {
                    self.warn(command, &message);
                }
                flow
            }
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
                let earlier = self.functions.insert(name.clone(), Rc::new(function));
                if let (Some(earlier), true) = (earlier, self.scopes.marks > 0) {
                    self.function_journal.push((name.clone(), earlier));
                }
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
            // Its own condition and no earlier one's, worked out once for the whole chain
            let effective = match taken.clone().not().and(truth.clone()) {
                Truth::Depends(condition) => model::simplify(&condition, None),
                known => known,
            };
            if effective != Truth::Known(false) && self.possible_here(&effective) {
                chosen.push((effective, body));
            }

            taken = taken.or(truth);
            if taken == Truth::Known(true) {
                break;
            }
        }
        // Where no branch runs: the else() branch, if any
        let rest = match taken.clone().not() {
            Truth::Depends(condition) => model::simplify(&condition, None),
            known => known,
        };
        let uncovered = match otherwise {
            Some(_) => Truth::Known(false),
            None => rest.clone(),
        };
        if let (Some(body), false) = (otherwise, taken == Truth::Known(true)) {
            if self.possible_here(&rest) {
                chosen.push((rest, body));
            }
        }

        // Only this machine's branch keeps the variables it sets: the others run from the state before the if() (their
        // targets and settings are recorded under their conditions), then their variables are put back. Then what the
        // branches did to each variable is merged in, under their conditions.
        let on_host: Vec<bool> = chosen
            .iter()
            .map(|(truth, _)| match truth {
                Truth::Depends(condition) => model::holds_on_host(condition, &|name| self.options.get(name).map(|(default, _)| *default)),
                Truth::Known(_) => true,
            })
            .collect();
        let mut order: Vec<usize> = (0..chosen.len()).collect();
        order.sort_by_key(|index| on_host[*index]);
        let mut changes: Vec<(usize, BranchChanges)> = Vec::new();

        let mut flow = Flow::Normal;
        for index in order {
            let (truth, body) = chosen[index].clone();
            let branch_flow = match &truth {
                Truth::Known(_) => self.run_block(body)?,
                Truth::Depends(condition) => {
                    let mark = self.mark();
                    self.conditions.push(condition.clone());
                    let result = self.run_block(body);
                    self.conditions.pop();

                    changes.push((index, self.changes_since(&mark, truth.clone())));
                    if on_host[index] {
                        self.release_mark();
                    } else {
                        self.undo(mark);
                    }
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

        changes.sort_by_key(|(index, _)| *index);
        let changes: Vec<BranchChanges> = changes.into_iter().map(|(_, branch)| branch).collect();
        self.merge_branches(&changes, &uncovered);
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

    /// Starts journaling variable writes, to read back or undo what a branch does.
    fn mark(&mut self) -> Mark {
        self.scopes.marks += 1;
        self.cache.marks += 1;
        Mark {
            scopes: self.scopes.journal.len(),
            cache: self.cache.journal.len(),
            functions: self.function_journal.len(),
            overridden_options: self.overridden_options.clone(),
        }
    }

    /// Keeps what was done since the latest mark (this machine's branch).
    fn release_mark(&mut self) {
        self.scopes.marks -= 1;
        self.cache.marks -= 1;
        if self.scopes.marks == 0 {
            self.scopes.journal.clear();
            self.cache.journal.clear();
            self.function_journal.clear();
        }
    }

    /// Puts variables back as they were at the mark; functions the branch defined stay (calls to them run under its
    /// condition), those it redefined go back.
    fn undo(&mut self, mark: Mark) {
        self.scopes.undo(mark.scopes);
        self.cache.undo(mark.cache);
        while self.function_journal.len() > mark.functions {
            let (name, function) = self.function_journal.pop().unwrap();
            self.functions.insert(name, function);
        }
        self.overridden_options = mark.overridden_options;
        self.release_mark();
    }

    /// The variables written since the mark whose value changed: in every scope (PARENT_SCOPE too) and the cache.
    fn changes_since(&self, mark: &Mark, condition: Truth) -> BranchChanges {
        // Each one's value before (its first journal entry)
        let mut before = Before::default();
        for (id, name, old) in &self.scopes.journal[mark.scopes..] {
            before.scopes.entry((*id, name.as_str())).or_insert(old);
        }
        for (name, old) in &self.cache.journal[mark.cache..] {
            before.cache.entry(name.as_str()).or_insert(old);
        }

        let mut changes = IndexMap::new();
        for (id, name) in before.scopes.keys() {
            let Some(index) = self.scopes.index_of(*id) else { continue };
            let (old, new) = (self.value_from(index, name, Some(&before)), self.value_from(index, name, None));
            if old != new {
                changes.insert(VariableKey::Scope(*id, name.to_string()), (old, new));
            }
        }
        for (name, old) in &before.cache {
            let new = self.cache.get(name).cloned();
            if **old != new {
                changes.insert(VariableKey::Cache(name.to_string()), ((*old).clone(), new));
            }
        }

        BranchChanges { condition, changes }
    }

    /// A variable as the scope at `index` sees it, now or (with `before`) at the mark.
    fn value_from(&self, index: usize, name: &str, before: Option<&Before>) -> Option<String> {
        for scope in (0..=index).rev() {
            let value = match before.and_then(|before| before.scopes.get(&(self.scopes.ids[scope], name))) {
                Some(old) => old.as_ref(),
                None => self.scopes[scope].get(name),
            };
            match value {
                Some(value) if value == UNSET => break,
                Some(value) => return Some(value.clone()),
                None => {}
            }
        }

        match before.and_then(|before| before.cache.get(name)) {
            Some(old) => (*old).clone(),
            None => self.cache.get(name).cloned(),
        }
    }

    /// Sets each variable the branches changed to what they did, together: items they appended, each under its
    /// branch's condition, or one value per condition where two branches or more set it differently. Otherwise it
    /// keeps this machine's value. `uncovered` is where no branch runs.
    fn merge_branches(&mut self, branches: &[BranchChanges], uncovered: &Truth) {
        let mut variables: IndexMap<&VariableKey, &Option<String>> = IndexMap::new();
        for branch in branches {
            for (key, (before, _)) in &branch.changes {
                variables.entry(key).or_insert(before);
            }
        }

        let mut merged: Vec<(VariableKey, String)> = Vec::new();
        for (key, before) in variables {
            // CMake's own variables (flags, module paths) are read at configure time; an option's cache entry is the
            // option, whichever branch declared it
            let (VariableKey::Scope(_, name) | VariableKey::Cache(name)) = key;
            if name.starts_with("CMAKE_") || (matches!(key, VariableKey::Cache(_)) && self.options.contains_key(name)) {
                continue;
            }

            // What each branch left, and what is left where none runs
            let mut outcomes: Vec<(Truth, Option<String>, bool)> = branches
                .iter()
                .map(|branch| match branch.changes.get(key) {
                    Some((_, after)) => (branch.condition.clone(), after.clone(), true),
                    None => (branch.condition.clone(), before.clone(), false),
                })
                .collect();
            if *uncovered != Truth::Known(false) {
                outcomes.push((uncovered.clone(), before.clone(), false));
            }

            if let Some(value) = merged_value(before, &outcomes) {
                merged.push((key.clone(), value));
            }
        }

        for (key, value) in merged {
            match key {
                VariableKey::Scope(id, name) => {
                    let Some(index) = self.scopes.index_of(id) else { continue };
                    if self.scopes[index].get(&name) != Some(&value) {
                        self.scopes.set(index, &name, Some(value));
                    }
                }
                VariableKey::Cache(name) => {
                    if self.cache.get(&name) != Some(&value) {
                        self.cache.insert(name, value);
                    }
                }
            }
        }
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
            // An item a branch appended: the body runs for it as in that branch (keeping its variables only where
            // this machine takes it)
            let conditional = model::split_conditional_item(&item).map(|(condition, value)| (condition, value.to_string()));
            if let Some((condition, value)) = conditional {
                let on_host = model::holds_on_host(&condition, &|name| options.get(name).copied());
                self.set_variable(variable, &value);
                self.loops.push((variable.clone(), value));
                let mark = self.mark();
                self.conditions.push(condition.clone());
                let result = self.run_block(body);
                self.conditions.pop();
                self.loops.pop();

                let changes = self.changes_since(&mark, Truth::Depends(condition.clone()));
                if on_host {
                    self.release_mark();
                } else {
                    self.undo(mark);
                }
                self.merge_branches(&[changes], &Truth::Depends(model::negate(&condition)));
                match result? {
                    Flow::Return(inner) => flow = Flow::Return(Truth::Depends(condition).and(inner)),
                    Flow::Break if on_host => break,
                    _ => {}
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

        if scope < self.scopes.len() {
            self.scopes.set(scope, variable, previous);
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
            "TARGET" => self.target_existence(&operand),
            // A path holding values set per condition: tested under each
            "EXISTS" | "IS_DIRECTORY" | "IS_ABSOLUTE" => {
                let test = |path: &str| match operator {
                    "EXISTS" => !path.contains("{out}") && !path.is_empty() && Path::new(path).exists(),
                    "IS_DIRECTORY" => !path.is_empty() && Path::new(path).is_dir(),
                    _ => Path::new(path).is_absolute(),
                };
                match text_versions(&operand) {
                    Some(versions) => versions.into_iter().fold(Truth::Known(false), |result, (condition, path)| {
                        result.or(condition.and(Truth::Known(test(&path))))
                    }),
                    None => Truth::Known(test(&self.host_text(&operand))),
                }
            }
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
        // One value per condition: true where its value there is
        if let Some(alternatives) = per_condition_value(text) {
            return alternatives.into_iter().fold(Truth::Known(false), |result, (condition, value)| {
                result.or(Truth::Depends(condition).and(self.truthiness(&value, quoted)))
            });
        }
        if let Some(symbol) = text.strip_prefix(SYMBOL_PREFIX) {
            return model::symbol_truth(symbol);
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

        // A variable name: platform symbols, options, or its value. An option a branch set (as a normal variable)
        // under a condition is that value there and the option elsewhere
        if let Some(truth) = self.option_truth(text) {
            let normal = self.scopes.iter().rev().find_map(|scope| scope.get(text)).filter(|value| *value != UNSET);
            let Some(alternatives) = normal.and_then(|value| per_condition_value(value)) else {
                return truth;
            };
            let covered = alternatives
                .iter()
                .fold(Truth::Known(false), |result, (condition, _)| result.or(Truth::Depends(condition.clone())));
            return alternatives.into_iter().fold(covered.not().and(truth), |result, (condition, value)| {
                result.or(Truth::Depends(condition).and(self.truthiness(&value, false)))
            });
        }
        match self.get_variable(text) {
            Some(value) if value.starts_with(SYMBOL_PREFIX) => model::symbol_truth(&value[SYMBOL_PREFIX.len()..]),
            Some(value) if model::marked_condition(&value).is_some() => self.truthiness(&value, false),
            Some(value) => conditional_value_truth(&value).unwrap_or(Truth::Known(model::constant_truth(&value).unwrap_or(true))),
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

        // A list with items under conditions holds the value where an item that is it is there
        if operator == "IN_LIST" {
            let list = self.get_variable(&right_value).unwrap_or_default();
            if list.contains("$<$<BOOL:") {
                return model::split_list(&list)
                    .iter()
                    .fold(Truth::Known(false), |result, item| match model::split_conditional_item(item) {
                        Some((condition, text)) if text == left_value => result.or(Truth::Depends(condition)),
                        _ => result.or(Truth::Known(*item == left_value)),
                    });
            }
        }

        // Values set per condition, whole or inside longer text: compared under each condition (and as empty where
        // none holds)
        for (alternatives_on_left, value) in [(true, &left_value), (false, &right_value)] {
            if !value.contains("$<$<BOOL:") || (!alternatives_on_left && operator == "IN_LIST") {
                continue;
            }
            let alternatives = text_versions(value).unwrap_or_else(|| vec![(Truth::Known(true), self.host_text(value))]);
            return alternatives.into_iter().fold(Truth::Known(false), |result, (condition, text)| {
                let truth = match alternatives_on_left {
                    true => self.compare(&(text, true), operator, right),
                    false => self.compare(left, operator, &(text, true)),
                };
                result.or(condition.and(truth))
            });
        }

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

    // Each item is true as a check's result or platform value is, else as a constant
    let item_truth = |item: &str| match (model::marked_condition(item), item.strip_prefix(SYMBOL_PREFIX)) {
        (Some(condition), _) => Truth::Depends(condition.to_string()),
        (None, Some(symbol)) => model::symbol_truth(symbol),
        (None, None) => Truth::Known(model::constant_truth(item).unwrap_or(true)),
    };
    let truth = conditions.into_iter().fold(Truth::Known(false), |result, (condition, item)| {
        result.or(Truth::Depends(condition).and(item_truth(item)))
    });

    // One value per condition covers every case: `msvc | !msvc` is true
    Some(match truth {
        Truth::Depends(condition) if items.len() > 1 => model::simplify(&condition, None),
        truth => truth,
    })
}

/// A command naming targets some of which are conditional items: its arguments for the plain ones (under no
/// condition), then for each conditional one alone (under its condition). None when it names none.
pub(super) fn conditional_target_runs(name: &str, args: &[String]) -> Option<Vec<(Option<String>, Vec<String>)>> {
    // Where the target names are
    let (start, end) = match name {
        "set_target_properties" => (0, args.iter().position(|arg| arg == "PROPERTIES")?),
        "set_property" if args.first().map(String::as_str) == Some("TARGET") => {
            let end = args
                .iter()
                .position(|arg| matches!(arg.as_str(), "APPEND" | "APPEND_STRING" | "PROPERTY"))?;
            (1, end)
        }
        _ if name.starts_with("target_") && !args.is_empty() => (0, 1),
        _ => return None,
    };
    let names = &args[start..end];
    if !names.iter().any(|name| model::split_conditional_item(name).is_some()) {
        return None;
    }

    let with_names = |names: Vec<String>| [&args[..start], &names[..], &args[end..]].concat();
    let plain: Vec<String> = names
        .iter()
        .filter(|name| model::split_conditional_item(name).is_none())
        .cloned()
        .collect();
    let mut runs = Vec::new();
    if !plain.is_empty() {
        runs.push((None, with_names(plain)));
    }
    for (condition, target) in names.iter().filter_map(|name| model::split_conditional_item(name)) {
        runs.push((Some(condition), with_names(vec![target.to_string()])));
    }
    Some(runs)
}

/// The versions of a text holding conditional items (items of a list, or inside longer text): each one's condition
/// and the text there, the conditions excluding each other and covering every case; one version, under no condition,
/// when it holds none. None when it has more than MAX_TEXT_VERSIONS.
pub(super) fn text_versions(text: &str) -> Option<Vec<(Truth, String)>> {
    let versions = texts_versions(&[text])?;
    Some(versions.into_iter().map(|(condition, mut texts)| (condition, texts.remove(0))).collect())
}

/// text_versions of several texts together: in each version, each text as it is there.
pub(super) fn texts_versions(texts: &[&str]) -> Option<Vec<(Truth, Vec<String>)>> {
    if !texts.iter().any(|text| text.contains("$<$<BOOL:")) {
        return Some(vec![(Truth::Known(true), texts.iter().map(|text| text.to_string()).collect())]);
    }

    // The conditions in them, nested ones too
    let conditions: RefCell<Vec<String>> = RefCell::new(Vec::new());
    for text in texts {
        let _ = model::choose_list_items(text, &|condition| {
            let mut conditions = conditions.borrow_mut();
            if !conditions.iter().any(|known| known == condition) {
                conditions.push(condition.to_string());
            }
            true
        });
    }

    // The cases they make: every way they can hold together
    let mut cases = vec![Truth::Known(true)];
    for condition in conditions.into_inner() {
        let mut split = Vec::new();
        for case in &cases {
            for part in [Truth::Depends(condition.clone()), Truth::Depends(model::negate(&condition))] {
                let both = match case.clone().and(part) {
                    Truth::Depends(both) => model::simplify(&both, None),
                    known => known,
                };
                if both != Truth::Known(false) {
                    split.push(both);
                }
            }
        }
        if split.len() > MAX_TEXT_VERSIONS {
            return None;
        }
        cases = split;
    }

    // The texts in each, cases with the same texts joined
    let mut versions: Vec<(Truth, Vec<String>)> = Vec::new();
    for case in cases {
        let holds = |condition: &str| match &case {
            Truth::Depends(case) => model::simplify(condition, Some(case)) == Truth::Known(true),
            Truth::Known(_) => model::simplify(condition, None) == Truth::Known(true),
        };
        let chosen: Vec<String> = texts.iter().map(|text| model::choose_list_items(text, &holds).into_owned()).collect();
        match versions.iter_mut().find(|(_, texts)| *texts == chosen) {
            Some((joined, _)) => {
                *joined = match joined.clone().or(case) {
                    Truth::Depends(either) => model::simplify(&either, None),
                    known => known,
                }
            }
            None => versions.push((case, chosen)),
        }
    }
    Some(versions)
}

/// A value from what it is in each version (`text_versions`' conditions): plain where they agree, else one item per
/// condition (none where it is empty).
pub(super) fn versions_value(results: Vec<(Truth, String)>) -> String {
    let mut joined: Vec<(Truth, String)> = Vec::new();
    for (condition, text) in results {
        match joined.iter_mut().find(|(_, other)| *other == text) {
            Some((either, _)) => {
                *either = match either.clone().or(condition) {
                    Truth::Depends(either) => model::simplify(&either, None),
                    known => known,
                }
            }
            None => joined.push((condition, text)),
        }
    }

    if let [(_, text)] = joined.as_slice() {
        return text.clone();
    }
    joined
        .into_iter()
        .filter(|(_, text)| !text.is_empty())
        .filter_map(|(condition, text)| match condition {
            Truth::Depends(condition) => Some(model::conditional_item(&condition, &text)),
            Truth::Known(true) => Some(text),
            Truth::Known(false) => None,
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// A value with one item per condition (a variable branches set differently): its conditions and items, when the
/// conditions exclude each other.
fn per_condition_value(value: &str) -> Option<Vec<(String, String)>> {
    if !value.starts_with("$<$<BOOL:") {
        return None;
    }

    let items = model::list_elements(value);
    let alternatives: Vec<(String, String)> = items
        .iter()
        .map(|item| model::split_conditional_item(item).map(|(condition, text)| (condition, text.to_string())))
        .collect::<Option<Vec<_>>>()?;
    for (index, (left, _)) in alternatives.iter().enumerate() {
        for (right, _) in &alternatives[index + 1..] {
            if model::simplify(&model::and(left, right), None) != Truth::Known(false) {
                return None;
            }
        }
    }

    Some(alternatives)
}

/// A variable's value after branches changed it (`outcomes`: each branch's condition, what it left, and whether it
/// changed it). Items branches removed stay under the conditions of those that kept them, and items they appended
/// come after, under the conditions of those that appended them; a single value two branches or more set differently
/// becomes one item per condition. None leaves this machine's value: a list a branch replaced, or a keyword that
/// chooses how a command runs.
fn merged_value(before: &Option<String>, outcomes: &[(Truth, Option<String>, bool)]) -> Option<String> {
    let elements = |value: &Option<String>| model::list_elements(value.as_deref().unwrap_or_default());
    let is_value = |item: &String| !item.contains(char::is_whitespace) && !COMMAND_KEYWORDS.contains(&item.as_str());
    let before_items = elements(before);

    // Each outcome as the earlier items it kept and the items it appended (not both removing and appending). A
    // branch reads an earlier item under a condition as plain where its own condition implies it, and does not see
    // one its condition rules out (which it therefore keeps)
    let mut kept_and_appended: Vec<(Vec<bool>, Vec<String>)> = Vec::new();
    for (condition, value, _) in outcomes {
        let items = elements(value);
        let mut kept = vec![false; before_items.len()];
        let mut seen: Vec<Option<&str>> = before_items.iter().map(|item| Some(item.as_str())).collect();
        if let (Truth::Depends(condition), false) = (condition, items.starts_with(&before_items)) {
            for (index, item) in before_items.iter().enumerate() {
                let Some((inner, text)) = model::split_conditional_item(item) else {
                    continue;
                };
                match model::simplify(&inner, Some(condition)) {
                    Truth::Known(true) => seen[index] = Some(text),
                    Truth::Known(false) => (kept[index], seen[index]) = (true, None),
                    Truth::Depends(_) => {}
                }
            }
        }

        let mut next = 0;
        let mut appended_from = items.len();
        for (index, item) in items.iter().enumerate() {
            match seen[next..].iter().position(|earlier| *earlier == Some(item.as_str())) {
                Some(offset) => {
                    kept[next + offset] = true;
                    next += offset + 1;
                }
                None => {
                    appended_from = index;
                    break;
                }
            }
        }

        let appended = items[appended_from..].to_vec();
        let removed: Vec<&String> = before_items.iter().zip(&kept).filter(|(_, kept)| !**kept).map(|(item, _)| item).collect();
        if !(removed.is_empty() || appended.is_empty()) || !removed.into_iter().chain(&appended).all(is_value) {
            kept_and_appended.clear();
            break;
        }
        kept_and_appended.push((kept, appended));
    }

    let either = |left: Truth, right: &Truth| match left.or(right.clone()) {
        Truth::Depends(condition) => model::simplify(&condition, None),
        known => known,
    };
    let tagged = |item: &str, condition: Truth| match condition {
        Truth::Known(true) => item.to_string(),
        Truth::Known(false) => String::new(),
        Truth::Depends(condition) => conditional_item_under(&condition, item),
    };

    if kept_and_appended.len() == outcomes.len() {
        // The earlier items, each where it was kept
        let mut items: Vec<String> = Vec::new();
        for (index, item) in before_items.iter().enumerate() {
            if kept_and_appended.iter().all(|(kept, _)| kept[index]) {
                items.push(item.clone());
                continue;
            }
            let condition = outcomes
                .iter()
                .zip(&kept_and_appended)
                .filter(|(_, (kept, _))| kept[index])
                .fold(Truth::Known(false), |result, ((condition, _, _), _)| either(result, condition));
            items.push(tagged(item, condition));
        }

        // Then the appended ones, the same item from different branches (or their inner ones) joined
        let mut appended: Vec<(String, Truth, usize)> = Vec::new();
        for (index, ((condition, _, _), (_, added))) in outcomes.iter().zip(&kept_and_appended).enumerate() {
            for item in added.iter().filter(|item| !item.is_empty()) {
                let (condition, item) = match model::split_conditional_item(item) {
                    Some((inner, text)) => match condition.clone().and(Truth::Depends(inner)) {
                        Truth::Depends(both) => (model::simplify(&both, None), text.to_string()),
                        known => (known, text.to_string()),
                    },
                    None => (condition.clone(), item.clone()),
                };
                match appended.iter_mut().find(|(other, _, from)| *other == item && *from != index) {
                    Some((_, joined, from)) => {
                        *joined = either(joined.clone(), &condition);
                        *from = index;
                    }
                    None => appended.push((item, condition, index)),
                }
            }
        }
        items.extend(appended.into_iter().map(|(item, condition, _)| tagged(&item, condition)));
        items.retain(|item| !item.is_empty());
        return Some(items.join(";"));
    }

    // A single value set differently (flags with spaces are one value too): one item per value, under the conditions
    // it has, when it reads back from one (text with a stray `>` does not)
    let changed = outcomes.iter().filter(|(_, _, changed)| *changed).count();
    let single = |value: &Option<String>| {
        let items = elements(value);
        items.len() <= 1
            && items.iter().all(|item| {
                let encoded = model::conditional_item("x", item);
                !COMMAND_KEYWORDS.contains(&item.as_str()) && model::split_conditional_item(&encoded).map(|(_, read)| read) == Some(item.as_str())
            })
    };
    if changed >= 2 && single(before) && outcomes.iter().all(|(_, value, _)| single(value)) {
        let mut values: Vec<(String, Truth)> = Vec::new();
        for (condition, value, _) in outcomes {
            let Some(value) = value.as_ref().filter(|value| !value.is_empty()) else {
                continue;
            };
            match values.iter_mut().find(|(other, _)| other == value) {
                Some((_, joined)) => *joined = either(joined.clone(), condition),
                None => values.push((value.clone(), condition.clone())),
            }
        }
        return Some(
            values
                .into_iter()
                .map(|(value, condition)| tagged(&value, condition))
                .collect::<Vec<_>>()
                .join(";"),
        );
    }

    None
}

/// `item` under `condition`; an item already under a condition gets both.
fn conditional_item_under(condition: &str, item: &str) -> String {
    match model::split_conditional_item(item) {
        Some((inner, text)) => match model::simplify(&model::and(condition, &inner), None) {
            Truth::Depends(both) => model::conditional_item(&both, text),
            Truth::Known(true) => text.to_string(),
            Truth::Known(false) => String::new(),
        },
        None => model::conditional_item(condition, item),
    }
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
        run_files(name, &[("CMakeLists.txt", cmake)])
    }

    /// Runs a project of these files (one is CMakeLists.txt) in a fresh temporary folder.
    fn run_files(name: &str, files: &[(&str, &str)]) -> Interpreter {
        let folder = std::env::temp_dir().join(format!("anvil-interp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        for (path, text) in files {
            std::fs::write(folder.join(path), text).unwrap();
        }

        let mut interpreter = Interpreter::new(&folder);
        interpreter.run_root().unwrap();
        let _ = std::fs::remove_dir_all(&folder);
        interpreter
    }

    fn variable(interpreter: &Interpreter, name: &str) -> String {
        interpreter.get_variable(name).unwrap_or_default()
    }

    /// A variable's items, each written `condition: item` when it is under one.
    fn items(interpreter: &Interpreter, name: &str) -> Vec<String> {
        model::list_elements(&variable(interpreter, name))
            .into_iter()
            .map(|item| match model::split_conditional_item(&item) {
                Some((condition, text)) => format!("{condition}: {text}"),
                None => item,
            })
            .collect()
    }

    #[test]
    fn every_branch_keeps_what_it_added_under_its_condition() {
        let cmake = r#"
set(SRCS main.c)
if(MSVC)
  list(APPEND SRCS cl.c)
  set(D USING_CL)
else()
  list(APPEND SRCS gnu.c)
  set(D USING_GNU)
endif()
set(L a b c)
if(MSVC)
  list(REMOVE_ITEM L b)
endif()
if(WIN32)
  set(KIND SHARED)
else()
  set(KIND STATIC)
endif()
set(BOTH x)
if(MSVC)
  list(APPEND BOTH y)
else()
  list(APPEND BOTH y)
endif()
set(DEF "-D${D}")
if(D STREQUAL "USING_GNU")
  set(IS_GNU 1)
endif()
if(${D} STREQUAL "USING_CL")
  set(IS_CL 1)
endif()
set(PICKED "")
foreach(source ${SRCS})
  list(APPEND PICKED "picked/${source}")
endforeach()
set(INNER "")
if(WIN32)
  if(MSVC)
    list(APPEND INNER cl)
  endif()
endif()
option(PUBLIC_SYMBOLS "p" ON)
if(PUBLIC_SYMBOLS)
  list(APPEND CHECKED -fvisibility=hidden)
endif()
foreach(flag ${CHECKED})
  check_c_compiler_flag(${flag} SUPPORTED)
endforeach()
if(SUPPORTED)
  set(USE_DEFAULT 1)
endif()
"#;
        let interpreter = run("added", cmake);

        // This machine's branch is no different from the others
        assert_eq!(items(&interpreter, "SRCS"), ["main.c", "msvc: cl.c", "!msvc: gnu.c"]);
        assert_eq!(items(&interpreter, "D"), ["msvc: USING_CL", "!msvc: USING_GNU"]);

        // An item one branch removed is under the others; a keyword stays this machine's; one added everywhere is plain
        assert_eq!(items(&interpreter, "L"), ["a", "!msvc: b", "c"]);
        assert_eq!(variable(&interpreter, "KIND"), if cfg!(windows) { "SHARED" } else { "STATIC" });
        assert_eq!(variable(&interpreter, "BOTH"), "x;y");

        // Inside longer text, and tested: under each condition
        let mut unknown = Vec::new();
        let alternatives = model::expand_genex(&variable(&interpreter, "DEF"), &mut unknown, None);
        assert!(
            alternatives.contains(&(Truth::Depends("msvc".into()), "-DUSING_CL".into())),
            "{alternatives:?}"
        );
        assert!(
            alternatives.contains(&(Truth::Depends("!msvc".into()), "-DUSING_GNU".into())),
            "{alternatives:?}"
        );
        assert_eq!(items(&interpreter, "IS_GNU"), ["!msvc: 1"]);
        assert_eq!(items(&interpreter, "IS_CL"), ["msvc: 1"]);

        // A loop over them runs under each item's condition; nested branches add both conditions
        assert_eq!(
            items(&interpreter, "PICKED"),
            ["picked/main.c", "msvc: picked/cl.c", "!msvc: picked/gnu.c"]
        );
        assert_eq!(items(&interpreter, "INNER"), ["windows & msvc: cl"]);

        // A check's result set under a condition is true where both hold
        assert_eq!(items(&interpreter, "USE_DEFAULT"), ["PUBLIC_SYMBOLS & !msvc: 1"]);
    }

    #[test]
    fn branches_not_taken_keep_parent_scope_and_cache_writes() {
        let cmake = r#"
function(add_platform_sources)
  if(NOT MSVC)
    set(SRCS ${SRCS} gnu.c PARENT_SCOPE)
  endif()
endfunction()
set(SRCS main.c)
add_platform_sources()
if(UNIX)
  set(THREADS_LIB pthread CACHE STRING "")
endif()
macro(define_helper)
  function(helper)
    set(HELPED 1 PARENT_SCOPE)
  endfunction()
endmacro()
if(UNIX)
  function(helper)
  endfunction()
endif()
"#;
        let interpreter = run("parent", cmake);

        assert_eq!(items(&interpreter, "SRCS"), ["main.c", "!msvc: gnu.c"]);
        assert_eq!(items(&interpreter, "THREADS_LIB"), ["unix: pthread"]);
        assert!(interpreter.functions.contains_key("helper"));
        assert!(interpreter.scopes.journal.is_empty() && interpreter.cache.journal.is_empty());
    }

    #[test]
    fn configure_time_commands_read_this_machines_items() {
        let cmake = r#"
project(configured C)
if(WIN32)
  set(EXT dll)
endif()
if(UNIX)
  set(PLATFORM posix)
else()
  set(PLATFORM windows)
endif()
set(NAMES first)
if(WIN32)
  list(APPEND NAMES second)
endif()
file(WRITE "${CMAKE_CURRENT_BINARY_DIR}/names.txt" "${NAMES}")
configure_file(config.h.in config.h)
"#;
        let interpreter = run_files(
            "configured",
            &[
                ("CMakeLists.txt", cmake),
                ("config.h.in", "#define EXT \"@EXT@\"\n#define PLATFORM \"@PLATFORM@\"\n"),
            ],
        );
        let text = |file: &str| {
            let writes = interpreter.configure_writes.iter().filter(|write| write.path.ends_with(file));
            writes.filter_map(|write| write.text.clone()).collect::<Vec<_>>().join("\n")
        };

        // Names and text as on this machine; a configured value under each condition, not its truth
        assert_eq!(text("names.txt"), if cfg!(windows) { "first;second" } else { "first" });
        let header = text("config.h");
        assert!(header.contains("#define EXT \"dll\"") && header.contains("#define EXT \"\""), "{header}");
        let platform = if cfg!(windows) { "windows" } else { "posix" };
        assert!(header.contains(&format!("#define PLATFORM \"{platform}\"")), "{header}");
    }

    #[test]
    fn target_exists_counts_targets_made_later() {
        let cmake = r#"
project(later C)
add_library(early STATIC early.c)
target_compile_definitions(early PRIVATE $<$<TARGET_EXISTS:late>:SAW_LATE> $<$<TARGET_EXISTS:never>:SAW_NEVER>)
add_library(late STATIC late.c)
"#;
        let interpreter = run("exists", cmake);
        let defines: Vec<(Option<String>, String)> = interpreter.targets["early"]
            .defines
            .iter()
            .map(|item| (item.condition.clone(), item.value.clone()))
            .collect();

        assert_eq!(defines, [(None, "SAW_LATE".to_string())]);
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

        // A value one branch replaced stays this machine's; one the branches set differently is one per condition
        assert_eq!(variable(&interpreter, "X"), "a");
        assert_eq!(items(&interpreter, "Y"), ["windows: win", "!windows: other"]);
        assert_eq!(items(&interpreter, "Z"), ["!windows: other", "windows: win"]);

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

    #[test]
    fn long_elseif_chains_and_deep_branches_stay_fast() {
        // if(O0) ... elseif(O12) ... else() with the value used twenty times
        let mut cmake = String::from("project(chain C)\n");
        for index in 0..13 {
            cmake += &format!("option(O{index} \"o\" OFF)\n");
        }
        for index in 0..13 {
            cmake += &format!("{}(O{index})\n  set(V v{index})\n", if index == 0 { "if" } else { "elseif" });
        }
        cmake += "else()\n  set(V none)\nendif()\nadd_executable(app main.c)\n";
        for index in 0..20 {
            cmake += &format!("target_compile_definitions(app PRIVATE \"D{index}=${{V}}_x\")\n");
        }
        let started = std::time::Instant::now();
        let interpreter = run("chain", &cmake);
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());

        // Each value under its own branch's condition, worked out once
        let defines: Vec<(Option<String>, String)> = interpreter.targets["app"]
            .defines
            .iter()
            .filter(|item| item.value.starts_with("D0="))
            .map(|item| (item.condition.clone(), item.value.clone()))
            .collect();
        assert_eq!(defines.len(), 14, "{defines:?}");
        assert_eq!(defines[0], (Some("O0".to_string()), "D0=v0_x".to_string()));
        assert_eq!(defines[2], (Some("!O0 & !O1 & O2".to_string()), "D0=v2_x".to_string()));

        // Nested if/else ten deep, each arm adding to a list
        let mut cmake = String::from("project(nest C)\nset(V base)\nadd_executable(app main.c)\n");
        for index in 0..10 {
            cmake += &format!("option(O{index} \"o\" OFF)\n");
        }
        for index in 0..10 {
            cmake += &format!("if(O{index})\n  list(APPEND V x{index})\n");
        }
        for index in (0..10).rev() {
            cmake += &format!("else()\n  list(APPEND V y{index})\nendif()\n");
        }
        cmake += "target_compile_definitions(app PRIVATE \"V=${V}\")\n";
        let started = std::time::Instant::now();
        let interpreter = run("nest", &cmake);
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());
        let defines: Vec<(Option<String>, String)> = interpreter.targets["app"]
            .defines
            .iter()
            .map(|item| (item.condition.clone(), item.value.clone()))
            .collect();
        assert_eq!(defines.len(), 21, "{defines:?}");
        assert!(defines.contains(&(Some("O0 & O1 & O2".to_string()), "x2".to_string())), "{defines:?}");
        assert!(defines.contains(&(Some("O0 & !O1".to_string()), "y1".to_string())), "{defines:?}");
    }

    #[test]
    fn text_with_too_many_versions_reads_as_this_machines() {
        let mut cmake = String::from("project(versions C)\n");
        let mut text = String::new();
        for index in 0..8 {
            cmake += &format!("option(O{index} \"o\" OFF)\n");
            cmake += &format!("if(O{index})\n  set(V{index} a{index})\nelse()\n  set(V{index} b{index})\nendif()\n");
            text += &format!("${{V{index}}}");
        }
        cmake += &format!("set(ALL \"{text}\")\n");
        let interpreter = run("text-versions", &cmake);

        let mut unknown = Vec::new();
        let alternatives = model::expand_genex(&variable(&interpreter, "ALL"), &mut unknown, None);
        assert_eq!(alternatives.len(), 32, "{alternatives:?}");
        assert!(alternatives.iter().all(|(_, text)| text.ends_with("b5b6b7")), "{alternatives:?}");
        assert_eq!(interpreter.warnings.len(), 3, "{:?}", interpreter.warnings);
    }

    #[test]
    fn string_operations_and_path_tests_run_per_condition() {
        let cmake = r#"
if(WIN32)
  set(PLAT win)
else()
  set(PLAT posix)
endif()
string(LENGTH "${PLAT}" PLAT_LEN)
string(REPLACE "o" "0" PLAT_R "${PLAT}")
string(SUBSTRING "${PLAT}" 0 1 INITIAL)
string(TOUPPER "${PLAT}" UP)
string(APPEND PLAT_SUFFIXED "-${PLAT}")
set(SAME "${PLAT}")
string(REGEX REPLACE "[a-z]" "x" MASKED "${SAME}")
if(EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/src/${PLAT}/impl.c")
  set(IMPL_EXISTS 1)
endif()
if(EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/src/${PLAT}/other.c")
  set(OTHER_EXISTS 1)
endif()
"#;
        let folder = std::env::temp_dir().join(format!("anvil-interp-strings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        for platform in ["win", "posix"] {
            std::fs::create_dir_all(folder.join("src").join(platform)).unwrap();
            std::fs::write(folder.join("src").join(platform).join("impl.c"), "").unwrap();
        }
        std::fs::create_dir_all(folder.join("src/posix")).unwrap();
        std::fs::write(folder.join("src/posix/other.c"), "").unwrap();
        std::fs::write(folder.join("CMakeLists.txt"), cmake).unwrap();
        let mut interpreter = Interpreter::new(&folder);
        interpreter.run_root().unwrap();
        let _ = std::fs::remove_dir_all(&folder);

        assert_eq!(items(&interpreter, "PLAT_LEN"), ["windows: 3", "!windows: 5"]);
        assert_eq!(items(&interpreter, "PLAT_R"), ["windows: win", "!windows: p0six"]);
        assert_eq!(items(&interpreter, "INITIAL"), ["windows: w", "!windows: p"]);
        assert_eq!(items(&interpreter, "UP"), ["windows: WIN", "!windows: POSIX"]);
        assert_eq!(items(&interpreter, "PLAT_SUFFIXED"), ["windows: -win", "!windows: -posix"]);
        assert_eq!(variable(&interpreter, "MASKED").matches("$<$<BOOL:").count(), 2);
        assert_eq!(variable(&interpreter, "IMPL_EXISTS"), "1");
        assert_eq!(items(&interpreter, "OTHER_EXISTS"), ["!windows: 1"]);
        assert!(interpreter.warnings.is_empty(), "{:?}", interpreter.warnings);
    }

    #[test]
    fn configured_files_and_math_keep_values_per_condition() {
        let cmake = r#"
project(configured C)
include(CheckTypeSize)
check_type_size("long" TOP_LONG)
set(ALIAS_LONG ${TOP_LONG})
math(EXPR BITS_LONG "${TOP_LONG} * 8")
if(TOP_LONG EQUAL 8)
  set(LONG64 1)
endif()
if(MSVC)
  set(KIND cl)
else()
  set(KIND gnu)
endif()
set(COMBINED "${KIND}-${TOP_LONG}")
configure_file(cfg.h.in cfg.h)
"#;
        let template = "#define ALIAS_LONG @ALIAS_LONG@\n#define BITS_LONG @BITS_LONG@\n#cmakedefine LONG64\n#define COMBINED \"@COMBINED@\"\n";
        let interpreter = run_files("configured-versions", &[("CMakeLists.txt", cmake), ("cfg.h.in", template)]);
        let text: String = interpreter.configure_writes.iter().filter_map(|write| write.text.clone()).collect();

        for line in [
            "#define ALIAS_LONG 4\n",
            "#define ALIAS_LONG 8\n",
            "#define BITS_LONG 32\n",
            "#define BITS_LONG 64\n",
            "#define LONG64\n",
            "/* #undef LONG64 */\n",
            "#define COMBINED \"cl-4\"\n",
            "#define COMBINED \"gnu-4\"\n",
            "#define COMBINED \"gnu-8\"\n",
        ] {
            assert!(text.contains(line), "{line} in {text}");
        }
        assert!(interpreter.warnings.is_empty(), "{:?}", interpreter.warnings);
    }

    #[test]
    fn values_with_spaces_set_per_branch_keep_their_conditions() {
        let cmake = r#"
if(MSVC)
  set(cxx_base_flags "-GS -W4 -WX -wd4251")
  set(header_text "builds/<system> only")
elseif(CMAKE_COMPILER_IS_GNUCXX)
  set(cxx_base_flags "-Wall -Wshadow")
  set(header_text "unix builds")
endif()
set(cxx_exception_flags "${cxx_base_flags} -EHsc")
"#;
        let interpreter = run("spaces", cmake);

        assert_eq!(
            items(&interpreter, "cxx_base_flags"),
            ["msvc: -GS -W4 -WX -wd4251", "gcc: -Wall -Wshadow"]
        );
        let mut unknown = Vec::new();
        let alternatives = model::expand_genex(&variable(&interpreter, "cxx_exception_flags"), &mut unknown, None);
        assert!(
            alternatives.contains(&(Truth::Depends("msvc".into()), "-GS -W4 -WX -wd4251 -EHsc".into())),
            "{alternatives:?}"
        );
        assert!(
            alternatives.contains(&(Truth::Depends("gcc".into()), "-Wall -Wshadow -EHsc".into())),
            "{alternatives:?}"
        );

        // Text that a conditional item cannot hold is this machine's
        let header_text = if cfg!(windows) { "builds/<system> only" } else { "unix builds" };
        assert_eq!(variable(&interpreter, "header_text"), header_text);
    }

    #[test]
    fn options_declared_with_defaults_per_condition_take_this_machines() {
        let cmake = r#"
project(defaults C)
if(MSVC)
  option(USE_FAST "fast" ON)
else()
  option(USE_FAST "fast" OFF)
endif()
option(BIG "big" OFF)
option(TOP_SHARED "top shared" ON)
if(BIG)
  set(SUB_SHARED ${TOP_SHARED} CACHE BOOL "")
endif()
option(SUB_SHARED "sub shared" OFF)
configure_file(cfg.h.in cfg.h)
if(APPLE)
  option(FRAMEWORK "framework" ON)
else()
  option(FRAMEWORK "framework (not here)" OFF)
endif()
if(FRAMEWORK AND NOT APPLE)
  set(FRAMEWORK OFF)
endif()
if(FRAMEWORK)
  add_library(framework SHARED f.c)
endif()
"#;
        let interpreter = run_files("defaults", &[("CMakeLists.txt", cmake), ("cfg.h.in", "#define SUB \"@SUB_SHARED@\"\n")]);

        assert_eq!(interpreter.options["USE_FAST"].0, cfg!(windows));
        assert!(!interpreter.options["SUB_SHARED"].0);
        let warned = |name: &str| {
            interpreter
                .warnings
                .iter()
                .any(|warning| warning.contains(&format!("option {name} has a default per condition")))
        };
        assert!(warned("USE_FAST") && warned("SUB_SHARED"), "{:?}", interpreter.warnings);
        assert!(!warned("BIG"));

        // The configured file reads the option's value as ON or OFF
        let texts: Vec<String> = interpreter.configure_writes.iter().filter_map(|write| write.text.clone()).collect();
        assert!(texts.contains(&"#define SUB \"OFF\"\n".to_string()), "{texts:?}");
        assert!(texts.contains(&"#define SUB \"ON\"\n".to_string()), "{texts:?}");

        // An option forced off under a condition is off there, and the option elsewhere
        let framework = model::simplify(interpreter.targets["framework"].condition.as_deref().unwrap(), None);
        assert_eq!(framework, Truth::Depends("FRAMEWORK & macos".into()));
    }

    #[test]
    fn nothing_is_kept_under_a_condition_that_cannot_hold() {
        let cmake = r#"
project(unsatisfiable C)
option(A "a" OFF)
option(B "b" OFF)
if(NOT A)
  set(V x)
endif()
if(A)
  configure_file(t.in t.txt)
  add_library(lib STATIC lib.c)
  if(NOT A AND B)
    target_compile_definitions(lib PRIVATE NEVER)
  endif()
endif()
"#;
        let interpreter = run_files("unsatisfiable", &[("CMakeLists.txt", cmake), ("t.in", "v=@V@ b=@B@\n")]);

        let writes: Vec<(Option<String>, Option<String>)> = interpreter
            .configure_writes
            .iter()
            .map(|write| (write.condition.clone(), write.text.clone()))
            .collect();
        assert_eq!(writes.len(), 2, "{writes:?}");
        assert!(writes.iter().all(|(condition, text)| {
            let condition = condition.clone().unwrap_or_default();
            model::satisfiable(&condition) && text.as_deref().is_some_and(|text| text.starts_with("v= "))
        }));
        assert!(interpreter.targets["lib"].defines.is_empty(), "{:?}", interpreter.targets["lib"].defines);
    }

    #[test]
    fn values_per_condition_as_arguments_and_target_names() {
        let cmake = r#"
project(arguments C)
option(BUILD_CLI "cli" ON)
macro(shy_set var default cache type)
  if(NOT ${cache} STREQUAL "CACHE")
    message(SEND_ERROR "Macro usage is: shy_set(var default CACHE type)")
  endif()
  set(${var} ${default})
endmacro()
if(WIN32)
  set(DEFAULT_VALUE ON)
else()
  set(DEFAULT_VALUE OFF)
endif()
shy_set(PICKED ${DEFAULT_VALUE} CACHE BOOL)
add_library(core STATIC core.c)
if(BUILD_CLI)
  set(PROGRAMS cli)
  add_executable(cli main.c)
endif()
set_property(TARGET core ${PROGRAMS} APPEND PROPERTY COMPILE_DEFINITIONS THREADS=0)
function(guarded)
  if(MSVC)
    return()
  elseif(NOT MSVC)
    return()
  endif()
  find_program(NEVER_FOUND never-there-tool)
endfunction()
guarded()
"#;
        let interpreter = run("arguments", cmake);

        assert!(interpreter.warnings.is_empty(), "{:?}", interpreter.warnings);
        assert_eq!(items(&interpreter, "PICKED"), ["windows: ON", "!windows: OFF"]);
        let defines = |target: &str| -> Vec<(Option<String>, String)> {
            interpreter.targets[target]
                .defines
                .iter()
                .map(|item| (item.condition.clone(), item.value.clone()))
                .collect()
        };
        assert_eq!(defines("core"), [(None, "THREADS=0".to_string())]);
        assert_eq!(defines("cli").len(), 1, "{:?}", defines("cli"));
        assert_eq!(defines("cli")[0].1, "THREADS=0");

        // A condition shown in a message reads as a condition, not as a variable
        let marker = model::option_marker("STATIC_CRT & msvc");
        assert_eq!(model::readable(&format!("$<BOOL:{marker}>")), "$<BOOL:(STATIC_CRT & msvc)>");
        assert_eq!(model::readable(&format!("$<BOOL:{}>", model::option_marker("OPT"))), "$<BOOL:${OPT}>");
    }
}
