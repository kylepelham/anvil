//! What the converter collects from CMake, and the condition logic it uses: CMake's platform and compiler tests and
//! generator expressions become Anvil conditions (`windows`, `msvc`, `debug`) instead of being decided for one
//! machine.

use std::path::PathBuf;

/// The result of a condition: decided, or depending on the platform, compiler, profile or an option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Truth {
    Known(bool),
    Depends(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Exe,
    Static,
    Shared,
    Object,
    Interface,
    Custom,
}

/// A setting with the condition it applies under and whether it passes on to dependents.
#[derive(Debug, Clone)]
pub struct Item<T> {
    pub condition: Option<String>,
    pub public: bool,
    pub value: T,
}

/// A file CMake writes at configure time; `text` is None when the converter cannot know it.
#[derive(Debug, Clone)]
pub struct ConfigureWrite {
    pub location: String,
    pub path: PathBuf,
    pub text: Option<String>,
    pub condition: Option<String>,
}

/// Settings for a target's files of one language: those under `folder` matching `pattern` (`*.c`).
#[derive(Debug, Clone)]
pub struct LanguageRule {
    pub folder: PathBuf,
    pub pattern: String,
    pub condition: Option<String>,
    pub options: Vec<String>,
    pub defines: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Target {
    pub name: String,
    pub kind: TargetKind,
    pub dir: PathBuf,
    pub dir_id: usize,
    /// Folders of later definitions (one per if/else branch), whose settings it takes too.
    pub other_dir_ids: Vec<usize>,
    pub condition: Option<String>,

    pub sources: Vec<Item<PathBuf>>,
    pub includes: Vec<Item<(PathBuf, bool)>>,
    pub defines: Vec<Item<String>>,
    /// INTERFACE settings of a target that compiles: its users' only.
    pub interface_defines: Vec<Item<String>>,
    pub interface_includes: Vec<Item<(PathBuf, bool)>>,
    pub interface_options: Vec<Item<String>>,
    pub options: Vec<Item<String>>,
    pub links: Vec<Item<String>>,
    pub link_options: Vec<Item<String>>,
    pub lib_dirs: Vec<Item<String>>,

    pub std: Option<u32>,
    /// The standard its dependents need too (a PUBLIC cxx_std_NN).
    pub public_std: Option<u32>,
    /// The C standard of its C files (C_STANDARD, c_std_NN).
    pub c_std: Option<u32>,
    /// The C standard its dependents need too (a PUBLIC or INTERFACE c_std_NN).
    pub public_c_std: Option<u32>,
    pub pch: Option<PathBuf>,
    pub gui: bool,
    pub output_name: Option<String>,
    pub depends: Vec<String>,
    pub notes: Vec<String>,
    pub imported: bool,

    /// Anvil keys properties set, each under its condition: output-prefix, output-suffix, exports, link-depends;
    /// and single values set only under a condition (pch, std, kind, ...), which override the target's own there.
    pub settings: Vec<Item<(&'static str, String)>>,
    /// add_library without a type under an unset BUILD_SHARED_LIBS: shared when this holds.
    pub shared_condition: Option<String>,
    /// DEFINE_SYMBOL: what a shared build defines instead of `<target>_EXPORTS` (empty: nothing).
    pub define_symbol: Option<String>,
    /// Flags and definitions for its files of one language only (C flags in a target that also compiles C++).
    pub language_rules: Vec<LanguageRule>,
    /// generate_export_header as Anvil's key: the header's path under its include folder, and the macro.
    pub export_header: Option<(String, String)>,
    /// The project's function that created it, if one did: its targets share a preset.
    pub helper: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Test {
    pub name: String,
    pub command: Vec<String>,
    pub workdir: Option<String>,
    pub condition: Option<String>,
    pub properties: TestProperties,
    /// The project's function that called add_test, if one did: tests from one helper become one [[test]] with cases.
    pub helper: Option<HelperCall>,
    /// The foreach loops add_test ran in: each one's variable and item.
    pub loops: Vec<(String, String)>,
    /// The CMake directory (add_subdirectory) that added it: its tests.toml.
    pub dir: PathBuf,
}

/// A call of one of the project's functions or macros.
#[derive(Debug, Clone, PartialEq)]
pub struct HelperCall {
    pub function: String,
    pub params: Vec<String>,
    pub args: Vec<String>,
}

/// set_tests_properties values Anvil has a key for; the rest become notes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TestProperties {
    pub will_fail: bool,
    pub serial: bool,
    pub timeout: Option<u64>,
    pub pass_regex: Option<String>,
    pub fail_regex: Option<String>,
    pub skip_exit_code: Option<i32>,
    pub skip_regex: Option<String>,
    /// Regexes the output must each match, and must not match; the exit code that passes.
    pub expect: Vec<String>,
    pub reject: Vec<String>,
    pub exit_code: Option<i32>,
    pub env: Vec<(String, String)>,
    /// DEPENDS, plus the tests that set up the fixtures this one requires.
    pub after: Vec<String>,
    pub fixtures_setup: Vec<String>,
    pub fixtures_required: Vec<String>,
    pub disabled: bool,
    pub notes: Vec<String>,
}

/// generate_export_header: the header CMake would write into the build folder.
#[derive(Debug, Clone)]
pub struct ExportHeader {
    pub target: String,
    pub base: String,
    pub macro_name: String,
    /// Where CMake writes it: `{out}/...` or a full path.
    pub generated_path: String,
    pub custom_content: String,
}

/// add_custom_command(OUTPUT ...): a build step.
#[derive(Debug, Clone)]
pub struct CustomCommand {
    pub outputs: Vec<PathBuf>,
    pub commands: Vec<Vec<String>>,
    pub depends: Vec<String>,
    pub depfile: Option<String>,
    pub workdir: Option<String>,
    pub comment: Option<String>,
    pub condition: Option<String>,
    pub dir: PathBuf,
    /// add_custom_target and add_custom_command(TARGET): the target the commands belong to.
    pub target: Option<String>,
    /// PRE_BUILD, PRE_LINK or POST_BUILD, for add_custom_command(TARGET).
    pub stage: Option<String>,
}

impl Target {
    /// Puts every item under `condition` too.
    pub fn qualify_items(&mut self, condition: &Option<String>) {
        fn qualify<T>(items: &mut [Item<T>], condition: &Option<String>) {
            for item in items {
                item.condition = and_optional(condition, &item.condition);
            }
        }

        qualify(&mut self.sources, condition);
        qualify(&mut self.includes, condition);
        qualify(&mut self.defines, condition);
        qualify(&mut self.interface_defines, condition);
        qualify(&mut self.interface_includes, condition);
        qualify(&mut self.interface_options, condition);
        qualify(&mut self.options, condition);
        qualify(&mut self.links, condition);
        qualify(&mut self.link_options, condition);
        qualify(&mut self.lib_dirs, condition);
        qualify(&mut self.settings, condition);
    }

    pub fn new(name: &str, kind: TargetKind, dir: PathBuf, dir_id: usize, condition: Option<String>) -> Target {
        Target {
            name: name.to_string(),
            kind,
            dir,
            dir_id,
            other_dir_ids: Vec::new(),
            condition,
            sources: Vec::new(),
            includes: Vec::new(),
            defines: Vec::new(),
            interface_defines: Vec::new(),
            interface_includes: Vec::new(),
            interface_options: Vec::new(),
            options: Vec::new(),
            links: Vec::new(),
            link_options: Vec::new(),
            lib_dirs: Vec::new(),
            std: None,
            public_std: None,
            c_std: None,
            public_c_std: None,
            pch: None,
            gui: false,
            output_name: None,
            depends: Vec::new(),
            notes: Vec::new(),
            imported: false,
            settings: Vec::new(),
            shared_condition: None,
            define_symbol: None,
            language_rules: Vec::new(),
            export_header: None,
            helper: None,
        }
    }
}

impl TargetKind {
    /// The kind as anvil.toml writes it.
    pub fn text(self) -> &'static str {
        match self {
            TargetKind::Exe => "exe",
            TargetKind::Static => "lib",
            TargetKind::Shared => "shared",
            TargetKind::Object => "object",
            TargetKind::Interface => "headers",
            TargetKind::Custom => "custom",
        }
    }
}

impl Truth {
    pub fn not(self) -> Truth {
        match self {
            Truth::Known(value) => Truth::Known(!value),
            Truth::Depends(condition) => Truth::Depends(negate(&condition)),
        }
    }

    pub fn and(self, other: Truth) -> Truth {
        match (self, other) {
            (Truth::Known(false), _) | (_, Truth::Known(false)) => Truth::Known(false),
            (Truth::Known(true), other) | (other, Truth::Known(true)) => other,
            (Truth::Depends(left), Truth::Depends(right)) => Truth::Depends(and(&left, &right)),
        }
    }

    pub fn or(self, other: Truth) -> Truth {
        match (self, other) {
            (Truth::Known(true), _) | (_, Truth::Known(true)) => Truth::Known(true),
            (Truth::Known(false), other) | (other, Truth::Known(false)) => other,
            (Truth::Depends(left), Truth::Depends(right)) if left == right => Truth::Depends(left),
            (Truth::Depends(left), Truth::Depends(right)) => Truth::Depends(format!("{} | {}", group(&left), group(&right))),
        }
    }
}

// Brackets an option's name expanded inside a generator expression's condition (no character the parser splits on)
const OPTION_MARKER: &str = "\u{1}option-";
const OPTION_MARKER_END: &str = "\u{2}";

// Brackets a platform variable's name (WIN32, CMAKE_CXX_COMPILER_ID, ...) expanded inside a generator expression's
// condition; it ends as OPTION_MARKER does
const SYMBOL_MARKER: &str = "\u{1}symbol-";

// Stands for a space inside a marked condition
const MARKER_SPACE: char = '\u{3}';

/// `a & b`, skipping duplicates.
pub fn and(left: &str, right: &str) -> String {
    if left == right {
        return left.to_string();
    }
    format!("{} & {}", group(left), group(right))
}

/// Both optional conditions, combined.
pub fn and_optional(left: &Option<String>, right: &Option<String>) -> Option<String> {
    match (left, right) {
        (Some(left), Some(right)) => Some(and(left, right)),
        (Some(single), None) | (None, Some(single)) => Some(single.clone()),
        (None, None) => None,
    }
}

pub fn negate(condition: &str) -> String {
    if let Some(inner) = condition.strip_prefix('!') {
        if is_atom(inner) {
            return inner.to_string();
        }
    }
    if is_atom(condition) {
        format!("!{condition}")
    } else {
        format!("!({condition})")
    }
}

fn group(condition: &str) -> String {
    let simple = is_atom(condition.trim_start_matches('!')) || !condition.contains(['&', '|']);
    if simple {
        condition.to_string()
    } else {
        format!("({condition})")
    }
}

fn is_atom(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_-.+".contains(character))
}

/// Would a condition hold on this machine with this compiler (Windows + MSVC here)? Used to let the host's branch
/// set variables last, so values computed later match a real configure.
pub fn holds_on_host(condition: &str, options: &dyn Fn(&str) -> Option<bool>) -> bool {
    let Ok(parsed) = crate::cond::parse(condition) else { return false };

    let lookup = |atom: &str| -> Option<bool> {
        Some(match atom {
            "windows" => cfg!(windows),
            "linux" => cfg!(target_os = "linux"),
            "macos" => cfg!(target_os = "macos"),
            "unix" => cfg!(unix),
            "msvc" | "cl" => cfg!(windows),
            "gnu" | "gcc" => !cfg!(windows),
            "clang" | "clang-cl" | "mingw" | "cross" => false,
            "x64" => cfg!(target_arch = "x86_64"),
            "x86" => cfg!(target_arch = "x86"),
            "arm64" => cfg!(target_arch = "aarch64"),
            "arm" => cfg!(target_arch = "arm"),
            "debug" | "release" => atom == "release",
            "true" => true,
            "false" => false,
            other => options(other).unwrap_or(false),
        })
    };

    parsed.eval(&lookup).unwrap_or(false)
}

/// The shortest equivalent of `condition` (a sum of products), assuming `assumption` holds: where the assumption is
/// false the result may be anything. `(a | b) & (a & c)` given nothing is `a & c`; `windows & x` given `windows` is
/// `x`. Conditions over too many names to tabulate come back unchanged.
pub fn simplify(condition: &str, assumption: Option<&str>) -> Truth {
    let Ok(parsed) = crate::cond::parse(condition) else {
        return Truth::Depends(condition.to_string());
    };
    let assumed = match assumption.map(crate::cond::parse) {
        Some(Ok(assumed)) => Some(assumed),
        Some(Err(_)) | None => None,
    };

    // The names, in order of appearance
    let mut atoms: Vec<String> = Vec::new();
    collect_atoms(&parsed, &mut atoms);
    if let Some(assumed) = &assumed {
        collect_atoms(assumed, &mut atoms);
    }
    if atoms.len() > MAX_SIMPLIFIED_ATOMS {
        return Truth::Depends(condition.to_string());
    }

    // The truth table: where it must hold, and where it does not matter (the assumption is false, or the
    // combination cannot happen, like cl without msvc)
    let mut required = Vec::new();
    let mut free = Vec::new();
    for assignment in 0u32..(1 << atoms.len()) {
        let lookup = |name: &str| match name {
            "true" => Some(true),
            "false" => Some(false),
            _ => atoms.iter().position(|atom| atom == name).map(|index| assignment & (1 << index) != 0),
        };
        let applies = assumed.as_ref().map_or(true, |assumed| assumed.eval(&lookup).unwrap_or(true));
        if !applies || !is_possible(&lookup) {
            free.push(assignment);
        } else if parsed.eval(&lookup).unwrap_or(false) {
            required.push(assignment);
        }
    }

    if required.is_empty() {
        return Truth::Known(false);
    }
    if required.len() + free.len() == 1 << atoms.len() {
        return Truth::Known(true);
    }

    let cover = minimal_cover(&required, &free, atoms.len() as u32);
    Truth::Depends(write_sum_of_products(&cover, &atoms))
}

// Above this many names a truth table is too large to be worth it
const MAX_SIMPLIFIED_ATOMS: usize = 12;

// Condition words where the first implies the second
const IMPLICATIONS: &[(&str, &str)] = &[
    ("cl", "msvc"),
    ("clang-cl", "msvc"),
    ("clang-cl", "clang"),
    ("gcc", "gnu"),
    ("mingw", "gcc"),
    ("mingw", "windows"),
    ("linux", "unix"),
    ("macos", "unix"),
];

// Condition words that cannot both hold
const EXCLUSIONS: &[(&str, &str)] = &[
    ("windows", "unix"),
    ("linux", "macos"),
    ("msvc", "gnu"),
    ("cl", "clang"),
    ("gcc", "clang"),
    ("x64", "arm64"),
    ("debug", "release"),
];

/// Can these condition words hold together? Words not in the condition are unknown and allow anything.
fn is_possible(lookup: &dyn Fn(&str) -> Option<bool>) -> bool {
    let implied = IMPLICATIONS
        .iter()
        .all(|(word, implied)| !(lookup(word) == Some(true) && lookup(implied) == Some(false)));
    let excluded = EXCLUSIONS
        .iter()
        .any(|(left, right)| holds_with_implications(left, lookup) && holds_with_implications(right, lookup));

    implied && !excluded
}

/// Does a condition word hold, or follow from one that does (`gcc` gives `gnu`, so `cl & gcc` cannot hold)?
fn holds_with_implications(word: &str, lookup: &dyn Fn(&str) -> Option<bool>) -> bool {
    lookup(word) == Some(true)
        || IMPLICATIONS
            .iter()
            .any(|(implying, implied)| *implied == word && holds_with_implications(implying, lookup))
}

fn collect_atoms(condition: &crate::cond::Cond, atoms: &mut Vec<String>) {
    use crate::cond::Cond;

    match condition {
        Cond::Atom(name) if name != "true" && name != "false" => {
            if !atoms.contains(name) {
                atoms.push(name.clone());
            }
        }
        Cond::Atom(_) => {}
        Cond::Not(inner) => collect_atoms(inner, atoms),
        Cond::And(left, right) | Cond::Or(left, right) => {
            collect_atoms(left, atoms);
            collect_atoms(right, atoms);
        }
    }
}

/// Quine-McCluskey: the prime implicants of required + free, then the fewest that cover every required assignment.
/// An implicant is (value, mask): bits in `mask` may be anything.
fn minimal_cover(required: &[u32], free: &[u32], width: u32) -> Vec<(u32, u32)> {
    let mut current: Vec<(u32, u32)> = required.iter().chain(free).map(|assignment| (*assignment, 0)).collect();
    let mut primes: Vec<(u32, u32)> = Vec::new();

    // Combine implicants that differ in one bit until nothing combines
    while !current.is_empty() {
        let mut combined = vec![false; current.len()];
        let mut next: Vec<(u32, u32)> = Vec::new();

        for first in 0..current.len() {
            for second in first + 1..current.len() {
                let (left, right) = (current[first], current[second]);
                let difference = left.0 ^ right.0;
                if left.1 == right.1 && difference.count_ones() == 1 {
                    let merged = (left.0 & !difference, left.1 | difference);
                    if !next.contains(&merged) {
                        next.push(merged);
                    }
                    combined[first] = true;
                    combined[second] = true;
                }
            }
        }

        for (index, implicant) in current.iter().enumerate() {
            if !combined[index] && !primes.contains(implicant) {
                primes.push(*implicant);
            }
        }
        current = next;
    }

    // Cover the required assignments: the implicant covering most of the rest, fewest literals on a tie
    let covers = |implicant: &(u32, u32), assignment: u32| assignment & !implicant.1 == implicant.0;
    let literal_count = |implicant: &(u32, u32)| width - implicant.1.count_ones();
    let mut uncovered: Vec<u32> = required.to_vec();
    let mut cover: Vec<(u32, u32)> = Vec::new();

    // Essential primes: the only cover of some required assignment
    for assignment in required {
        let covering: Vec<&(u32, u32)> = primes.iter().filter(|implicant| covers(implicant, *assignment)).collect();
        if let [only] = covering.as_slice() {
            if !cover.contains(only) {
                cover.push(**only);
            }
        }
    }
    uncovered.retain(|assignment| !cover.iter().any(|implicant| covers(implicant, *assignment)));

    while !uncovered.is_empty() {
        let best = *primes
            .iter()
            .max_by_key(|implicant| {
                let covered = uncovered.iter().filter(|assignment| covers(implicant, **assignment)).count();
                (covered, std::cmp::Reverse(literal_count(implicant)))
            })
            .unwrap();
        uncovered.retain(|assignment| !covers(&best, *assignment));
        cover.push(best);
    }

    // Shorter terms first, then in the order their names appeared
    let first_literal = |implicant: &(u32, u32)| (0..width).find(|index| implicant.1 & (1 << index) == 0).unwrap_or(width);
    cover.sort_by_key(|implicant| (literal_count(implicant), first_literal(implicant)));
    cover
}

fn write_sum_of_products(cover: &[(u32, u32)], atoms: &[String]) -> String {
    let terms: Vec<Vec<String>> = cover
        .iter()
        .map(|(value, mask)| {
            (0..atoms.len())
                .filter(|index| mask & (1 << index) == 0)
                .map(|index| {
                    if value & (1 << index) != 0 {
                        atoms[index].clone()
                    } else {
                        format!("!{}", atoms[index])
                    }
                })
                .collect()
        })
        .collect();

    if terms.len() == 1 {
        return terms[0].join(" & ");
    }

    let written: Vec<String> = terms
        .iter()
        .map(|literals| {
            if literals.len() > 1 {
                format!("({})", literals.join(" & "))
            } else {
                literals[0].clone()
            }
        })
        .collect();
    written.join(" | ")
}

/// CMake's truth constants: None when the text is not one (then it may be a variable name).
pub fn constant_truth(text: &str) -> Option<bool> {
    let upper = text.to_ascii_uppercase();
    match upper.as_str() {
        "1" | "ON" | "YES" | "TRUE" | "Y" => Some(true),
        "0" | "OFF" | "NO" | "FALSE" | "N" | "IGNORE" | "NOTFOUND" | "" => Some(false),
        _ if upper.ends_with("-NOTFOUND") => Some(false),
        _ if text.parse::<f64>().is_ok() => Some(text.parse::<f64>().unwrap() != 0.0),
        _ => None,
    }
}

/// A CMake configuration name as an Anvil profile name.
pub fn profile_name(config: &str) -> String {
    config.trim().to_lowercase()
}

/// Splits a value on `;` outside generator expressions, honouring `\;`.
pub fn split_list(value: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut characters = value.chars().peekable();

    while let Some(character) = characters.next() {
        match character {
            '\\' if characters.peek() == Some(&';') => {
                current.push(';');
                characters.next();
            }
            '$' if characters.peek() == Some(&'<') => {
                depth += 1;
                current.push(character);
            }
            '>' if depth > 0 => {
                depth -= 1;
                current.push(character);
            }
            ';' if depth == 0 => items.push(std::mem::take(&mut current)),
            _ => current.push(character),
        }
    }
    items.push(current);

    items.into_iter().filter(|item| !item.is_empty()).collect()
}

/// A list's elements as list() sees them: empty ones kept, `\;` left inside its element ("" is no elements).
pub fn list_elements(value: &str) -> Vec<String> {
    if value.is_empty() {
        return Vec::new();
    }

    let mut items = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut characters = value.chars().peekable();

    while let Some(character) = characters.next() {
        match character {
            '\\' if characters.peek() == Some(&';') => {
                current.push_str("\\;");
                characters.next();
            }
            '$' if characters.peek() == Some(&'<') => {
                depth += 1;
                current.push(character);
            }
            '>' if depth > 0 => {
                depth -= 1;
                current.push(character);
            }
            ';' if depth == 0 => items.push(std::mem::take(&mut current)),
            _ => current.push(character),
        }
    }
    items.push(current);
    items
}

/// Words a command received, with a generator expression the parser cut at whitespace joined again as CMake reads
/// it: `$<$<C>:a` and `b>` are the one list item `$<$<C>:a;b>`.
pub fn join_genex_fragments(words: Vec<String>) -> Vec<String> {
    let open_count = |text: &str| {
        let characters: Vec<char> = text.chars().collect();
        let mut depth = 0usize;
        for (index, character) in characters.iter().enumerate() {
            if *character == '$' && characters.get(index + 1) == Some(&'<') {
                depth += 1;
            } else if *character == '>' && depth > 0 {
                depth -= 1;
            }
        }
        depth
    };

    let mut joined: Vec<String> = Vec::new();
    let mut open = false;
    for word in words {
        match joined.last_mut() {
            Some(last) if open => {
                last.push(';');
                last.push_str(&word);
            }
            _ => joined.push(word),
        }
        open = open_count(joined.last().unwrap()) > 0;
    }
    joined
}

/// Evaluates generator expressions in one list item: each result is (condition, value). Unknown expressions are
/// kept and reported. `target_exists` answers $<TARGET_EXISTS:...> (without it, that expression is unknown).
pub fn expand_genex(item: &str, unknown: &mut Vec<String>, target_exists: Option<&dyn Fn(&str) -> bool>) -> Vec<(Truth, String)> {
    if !item.contains("$<") {
        return vec![(Truth::Known(true), item.to_string())];
    }

    let mut parser = GenexParser {
        characters: item.chars().collect(),
        position: 0,
        unknown,
        target_exists,
    };
    let parts = parser.sequence(&[]);

    // A compiler version outside a version comparison is a value Anvil cannot know
    let mut results: Vec<(Truth, String)> = join_alternatives(parts).into_iter().filter(|(_, value)| !value.is_empty()).collect();
    for (_, value) in &mut results {
        if value.contains(COMPILER_VERSION_MARKER) {
            *value = value.replace(COMPILER_VERSION_MARKER, "$<CXX_COMPILER_VERSION>");
            unknown.push("$<CXX_COMPILER_VERSION>".into());
        }
    }

    results
}

/// Evaluates generator expressions in one list item and splits the results into items, each with where it applies:
/// `$<$<C>:a;$<$<D>:b>>` is `a` under C and `b` under C and D.
pub fn expand_genex_items(item: &str, unknown: &mut Vec<String>, target_exists: Option<&dyn Fn(&str) -> bool>) -> Vec<(Truth, String)> {
    let mut items: Vec<(Truth, String)> = Vec::new();

    for (truth, text) in expand_genex(item, unknown, target_exists) {
        for part in split_list(&text) {
            match items.iter_mut().find(|(_, existing)| *existing == part) {
                Some((existing, _)) => *existing = existing.clone().or(truth.clone()),
                None => items.push((truth.clone(), part)),
            }
        }
    }

    // An item that came from several alternatives holds where any of them does
    for (truth, _) in &mut items {
        if let Truth::Depends(condition) = truth {
            if condition.contains('|') {
                *truth = simplify(condition, None);
            }
        }
    }
    items
}

struct GenexParser<'a> {
    characters: Vec<char>,
    position: usize,
    unknown: &'a mut Vec<String>,
    target_exists: Option<&'a dyn Fn(&str) -> bool>,
}

type Alternatives = Vec<(Truth, String)>;

/// The text of an argument that has one unconditional value.
fn plain_text(alternatives: &Alternatives) -> Option<&str> {
    match alternatives.as_slice() {
        [(Truth::Known(true), text)] => Some(text.as_str()),
        _ => None,
    }
}

// Generator expressions that test something (their value is "1" or "0")
const CONDITION_EXPRESSIONS: &[&str] = &[
    "CONFIG",
    "PLATFORM_ID",
    "CXX_COMPILER_ID",
    "C_COMPILER_ID",
    "CXX_COMPILER_FRONTEND_VARIANT",
    "C_COMPILER_FRONTEND_VARIANT",
    "COMPILE_LANGUAGE",
    "LINK_LANGUAGE",
    "COMPILE_LANG_AND_ID",
    "CXX_COMPILER_VERSION",
    "C_COMPILER_VERSION",
    "VERSION_LESS",
    "VERSION_GREATER",
    "VERSION_EQUAL",
    "VERSION_LESS_EQUAL",
    "VERSION_GREATER_EQUAL",
    "BOOL",
    "NOT",
    "AND",
    "OR",
    "STREQUAL",
    "EQUAL",
    "TARGET_EXISTS",
];

// The condition words a COMPILE_LANGUAGE test becomes; decided per target (and per file) once its sources are known
pub const LANGUAGE_C: &str = "language.c";
pub const LANGUAGE_CXX: &str = "language.cxx";
const LANGUAGE_PREFIX: &str = "language.";

// $<CXX_COMPILER_VERSION> as a value, for a version comparison to recognise
const COMPILER_VERSION_MARKER: &str = "\u{1}compiler-version\u{2}";

// $<TARGET_OBJECTS:x> as a value: `{objects:x}`, a dependency on the object library x
pub const OBJECTS_PREFIX: &str = "{objects:";

// The version each compiler family is assumed to have (a current one), for version tests
const CURRENT_COMPILER_VERSIONS: &[(&str, &str)] = &[("cl", "19.44"), ("gcc", "14.2"), ("clang", "19.1")];

impl GenexParser<'_> {
    /// Text and expressions up to the end, or up to one of `stops` (not consumed).
    fn sequence(&mut self, stops: &[char]) -> Vec<Alternatives> {
        let mut parts = Vec::new();
        let mut text = String::new();

        while let Some(character) = self.peek() {
            if stops.contains(&character) {
                break;
            }

            if self.at_expression() {
                if !text.is_empty() {
                    parts.push(vec![(Truth::Known(true), std::mem::take(&mut text))]);
                }
                self.position += 2;
                parts.push(self.expression());
                continue;
            }

            text.push(character);
            self.position += 1;
        }

        if !text.is_empty() {
            parts.push(vec![(Truth::Known(true), text)]);
        }
        parts
    }

    /// One `$<...>`, its `$<` already consumed, through its closing `>`.
    fn expression(&mut self) -> Alternatives {
        let start = self.position;

        // $<$<cond>:value>: the head is a nested condition; one Anvil cannot read leaves the whole expression unknown
        if self.at_expression() {
            self.position += 2;
            let unknown_count = self.unknown.len();
            let condition = truth_of(&self.expression());
            if self.unknown.len() > unknown_count || self.peek() != Some(':') {
                self.unknown.truncate(unknown_count);
                return self.unknown_expression(start);
            }
            self.position += 1;

            let value = join_alternatives(self.sequence(&['>']));
            self.expect_close();
            return conditional(condition, value);
        }

        let head = self.text_until(&[':', '>']);

        // $<${WIN32}:value>: a marked option or platform value as the condition
        if is_marker(&head) && self.peek() == Some(':') {
            self.position += 1;
            let condition = truth_of(&vec![(Truth::Known(true), head)]);
            let value = join_alternatives(self.sequence(&['>']));
            self.expect_close();
            return conditional(condition, value);
        }

        // No argument: $<CONFIG>
        if self.peek() == Some('>') {
            self.position += 1;
            return match head.as_str() {
                "CONFIG" | "CONFIGURATION" => vec![(Truth::Known(true), "{config}".into())],
                "CXX_COMPILER_VERSION" | "C_COMPILER_VERSION" => vec![(Truth::Known(true), COMPILER_VERSION_MARKER.into())],
                "SEMICOLON" => vec![(Truth::Known(true), ";".into())],
                "COMMA" => vec![(Truth::Known(true), ",".into())],
                "ANGLE-R" => vec![(Truth::Known(true), ">".into())],
                _ => self.unknown_expression(start),
            };
        }
        self.position += 1;

        match head.as_str() {
            // $<1:value>, $<0:value>, interface wrappers: the value runs to the closing '>'
            "1" | "0" | "BUILD_INTERFACE" | "BUILD_LOCAL_INTERFACE" | "INSTALL_INTERFACE" | "LINK_ONLY" => {
                let value = join_alternatives(self.sequence(&['>']));
                self.expect_close();
                if head == "0" || head == "INSTALL_INTERFACE" {
                    vec![(Truth::Known(true), String::new())]
                } else {
                    value
                }
            }
            "TARGET_FILE" | "TARGET_LINKER_FILE" => {
                let name = self.text_until(&['>']);
                self.expect_close();
                vec![(Truth::Known(true), format!("{{target:{name}}}"))]
            }
            "TARGET_FILE_DIR" => {
                self.text_until(&['>']);
                self.expect_close();
                vec![(Truth::Known(true), "{out}".into())]
            }

            // An object library's objects: a dependency on it
            "TARGET_OBJECTS" => {
                let name = self.text_until(&['>']);
                self.expect_close();
                vec![(Truth::Known(true), format!("{OBJECTS_PREFIX}{name}}}"))]
            }
            "TARGET_NAME_IF_EXISTS" => {
                let name = self.text_until(&['>']);
                self.expect_close();
                match self.target_exists {
                    Some(target_exists) if target_exists(&name) => vec![(Truth::Known(true), name)],
                    Some(_) => Vec::new(),
                    None => self.unknown_expression(start),
                }
            }

            // A target's headers without linking it: Anvil's header-deps
            "COMPILE_ONLY" => {
                let name = self.text_until(&['>']);
                self.expect_close();
                vec![(Truth::Known(true), format!("{{header-dep:{name}}}"))]
            }
            "TARGET_PROPERTY" => {
                let arguments = self.arguments();
                match arguments.as_slice() {
                    [target, property] if plain_text(property) == Some("INTERFACE_INCLUDE_DIRECTORIES") => match plain_text(target) {
                        Some(name) => vec![(Truth::Known(true), format!("{{header-dep:{name}}}"))],
                        None => self.unknown_expression(start),
                    },
                    _ => self.unknown_expression(start),
                }
            }
            name if CONDITION_EXPRESSIONS.contains(&name) => {
                let unknown_count = self.unknown.len();
                let arguments = self.arguments();
                let truth = match self.unknown.len() > unknown_count {
                    true => None,
                    false => condition_expression(name, &arguments, self.target_exists),
                };
                match truth {
                    Some(truth) => truth_only(truth),
                    None => {
                        self.unknown.truncate(unknown_count);
                        self.unknown_expression(start)
                    }
                }
            }

            // $<IF:cond,then,else>
            "IF" => {
                let unknown_count = self.unknown.len();
                let arguments = self.arguments();
                let ([condition, then, otherwise], false) = (arguments.as_slice(), self.unknown.len() > unknown_count) else {
                    self.unknown.truncate(unknown_count);
                    return self.unknown_expression(start);
                };
                let truth = truth_of(condition);

                let mut results: Alternatives = Vec::new();
                for (branch_truth, branch) in [(truth.clone(), then), (truth.not(), otherwise)] {
                    for (value_truth, text) in branch {
                        let combined = branch_truth.clone().and(value_truth.clone());
                        if combined != Truth::Known(false) {
                            results.push((combined, text.clone()));
                        }
                    }
                }
                results
            }
            _ => self.unknown_expression(start),
        }
    }

    /// Comma-separated arguments up to the closing `>`, which is consumed.
    fn arguments(&mut self) -> Vec<Alternatives> {
        let mut arguments: Vec<Alternatives> = Vec::new();

        loop {
            arguments.push(join_alternatives(self.sequence(&[',', '>'])));
            if self.peek() == Some(',') {
                self.position += 1;
                continue;
            }
            break;
        }
        self.expect_close();

        arguments
    }

    /// Keeps an expression Anvil cannot translate as raw text, and reports it.
    fn unknown_expression(&mut self, start: usize) -> Alternatives {
        self.position = start;
        let mut depth = 1;

        while let Some(character) = self.peek() {
            if self.at_expression() {
                depth += 1;
                self.position += 2;
                continue;
            }
            self.position += 1;
            if character == '>' {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
        }

        let raw = format!("$<{}", self.characters[start..self.position].iter().collect::<String>());
        let raw = without_option_markers(&raw);
        self.unknown.push(raw.clone());
        vec![(Truth::Known(true), raw)]
    }

    fn at_expression(&self) -> bool {
        self.peek() == Some('$') && self.characters.get(self.position + 1) == Some(&'<')
    }

    fn expect_close(&mut self) {
        if self.peek() == Some('>') {
            self.position += 1;
        }
    }

    fn text_until(&mut self, stops: &[char]) -> String {
        let mut text = String::new();
        while let Some(character) = self.peek() {
            if stops.contains(&character) {
                break;
            }
            text.push(character);
            self.position += 1;
        }
        text
    }

    fn peek(&self) -> Option<char> {
        self.characters.get(self.position).copied()
    }
}

/// The truth of a condition expression from its (already expanded) arguments; None when Anvil cannot decide it.
fn condition_expression(name: &str, arguments: &[Alternatives], target_exists: Option<&dyn Fn(&str) -> bool>) -> Option<Truth> {
    let flat = |index: usize| -> String {
        arguments
            .get(index)
            .and_then(|alternatives| alternatives.first())
            .map(|(_, text)| text.clone())
            .unwrap_or_default()
    };
    let any_of =
        |from: usize, test: &dyn Fn(&str) -> Truth| (from..arguments.len()).fold(Truth::Known(false), |result, index| result.or(test(&flat(index))));

    Some(match name {
        "CONFIG" => any_of(0, &|config| Truth::Depends(profile_name(config))),
        "PLATFORM_ID" => any_of(0, &platform_truth),
        "CXX_COMPILER_ID" | "C_COMPILER_ID" => any_of(0, &compiler_truth),
        "CXX_COMPILER_FRONTEND_VARIANT" | "C_COMPILER_FRONTEND_VARIANT" => Truth::Depends(if flat(0) == "MSVC" { "msvc" } else { "gnu" }.into()),
        "COMPILE_LANGUAGE" | "LINK_LANGUAGE" => any_of(0, &language_truth),
        "COMPILE_LANG_AND_ID" => language_truth(&flat(0)).and(any_of(1, &compiler_truth)),
        "CXX_COMPILER_VERSION" | "C_COMPILER_VERSION" => version_comparison("VERSION_EQUAL", COMPILER_VERSION_MARKER, &flat(0)),
        "VERSION_LESS" | "VERSION_GREATER" | "VERSION_EQUAL" | "VERSION_LESS_EQUAL" | "VERSION_GREATER_EQUAL" => {
            version_comparison(name, &flat(0), &flat(1))
        }
        "BOOL" => truth_of(&arguments[0]),
        "NOT" => truth_of(&arguments[0]).not(),
        "AND" => arguments.iter().map(truth_of).fold(Truth::Known(true), Truth::and),
        "OR" => arguments.iter().map(truth_of).fold(Truth::Known(false), Truth::or),
        "STREQUAL" | "EQUAL" => {
            let equal = |left: &str, right: &str| match name {
                "EQUAL" => left
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .is_some_and(|number| right.trim().parse::<f64>().ok() == Some(number)),
                _ => left == right,
            };
            let (left, right) = (flat(0), flat(1));

            // A platform value: each value it can have, and where it has it
            match (marked(&left, SYMBOL_MARKER), marked(&right, SYMBOL_MARKER)) {
                (Some(symbol), _) => symbol_matches(symbol, &|candidate| equal(candidate, &right)),
                (_, Some(symbol)) => symbol_matches(symbol, &|candidate| equal(&left, candidate)),
                _ => Truth::Known(equal(&left, &right)),
            }
        }
        "TARGET_EXISTS" => Truth::Known(target_exists?(&flat(0))),
        _ => return None,
    })
}

/// $<COMPILE_LANGUAGE:...> of one language: the language's condition word; languages Anvil passes no flags to are
/// never true.
fn language_truth(language: &str) -> Truth {
    match language {
        "C" => Truth::Depends(LANGUAGE_C.into()),
        "CXX" => Truth::Depends(LANGUAGE_CXX.into()),
        _ => Truth::Known(false),
    }
}

/// A $<VERSION_...> comparison; against the compiler's version, true for the compiler families whose current version
/// passes.
fn version_comparison(operator: &str, left: &str, right: &str) -> Truth {
    let compare = |left: &str, right: &str| {
        let ordering = super::interp::compare_versions(left, right);
        match operator {
            "VERSION_LESS" => ordering.is_lt(),
            "VERSION_GREATER" => ordering.is_gt(),
            "VERSION_LESS_EQUAL" => ordering.is_le(),
            "VERSION_GREATER_EQUAL" => ordering.is_ge(),
            _ => ordering.is_eq(),
        }
    };
    if left != COMPILER_VERSION_MARKER && right != COMPILER_VERSION_MARKER {
        return Truth::Known(compare(left, right));
    }

    let passing: Vec<&str> = CURRENT_COMPILER_VERSIONS
        .iter()
        .filter(|(_, version)| {
            let side = |text: &str| {
                if text == COMPILER_VERSION_MARKER {
                    version.to_string()
                } else {
                    text.to_string()
                }
            };
            compare(&side(left), &side(right))
        })
        .map(|(family, _)| *family)
        .collect();
    match passing.len() {
        0 => Truth::Known(false),
        count if count == CURRENT_COMPILER_VERSIONS.len() => Truth::Known(true),
        _ => passing
            .iter()
            .fold(Truth::Known(false), |result, family| result.or(Truth::Depends(family.to_string()))),
    }
}

/// A condition with the language words decided for files of `language` (LANGUAGE_C or LANGUAGE_CXX).
pub fn for_language(condition: &str, language: &str) -> Truth {
    use crate::cond::Cond;

    fn decide(condition: &Cond, language: &str) -> Truth {
        match condition {
            Cond::Atom(name) if name.starts_with(LANGUAGE_PREFIX) => Truth::Known(name == language),
            Cond::Atom(name) => match name.as_str() {
                "true" => Truth::Known(true),
                "false" => Truth::Known(false),
                _ => Truth::Depends(name.clone()),
            },
            Cond::Not(inner) => decide(inner, language).not(),
            Cond::And(left, right) => decide(left, language).and(decide(right, language)),
            Cond::Or(left, right) => decide(left, language).or(decide(right, language)),
        }
    }

    match crate::cond::parse(condition) {
        Ok(parsed) => decide(&parsed, language),
        Err(_) => Truth::Depends(condition.to_string()),
    }
}

/// Does a condition test the language of the file compiled?
pub fn mentions_language(condition: &Option<String>) -> bool {
    condition.as_ref().is_some_and(|condition| condition.contains(LANGUAGE_PREFIX))
}

/// An option's value in a generator expression's condition: the option itself, not its default.
pub fn option_marker(name: &str) -> String {
    format!("{OPTION_MARKER}{name}{OPTION_MARKER_END}")
}

/// Text with option markers written back as the `${NAME}` they came from, for messages.
pub fn without_option_markers(text: &str) -> String {
    let mut result = String::new();
    let mut rest = text;

    while let Some(start) = rest.find(OPTION_MARKER) {
        let after = &rest[start + OPTION_MARKER.len()..];
        let Some(end) = after.find(OPTION_MARKER_END) else { break };
        result.push_str(&rest[..start]);
        result.push_str(&format!("${{{}}}", &after[..end]));
        rest = &after[end + OPTION_MARKER_END.len()..];
    }
    result.push_str(rest);

    result
}

/// A platform variable's value in a generator expression's condition: the variable, not this machine's value.
pub fn symbol_marker(name: &str) -> String {
    format!("{SYMBOL_MARKER}{name}{OPTION_MARKER_END}")
}

/// `item` where `condition` holds, as a generator expression the target commands turn into a `when` block.
pub fn conditional_item(condition: &str, item: &str) -> String {
    let marker = option_marker(&condition.replace(' ', &MARKER_SPACE.to_string()));
    format!("$<$<BOOL:{marker}>:{item}>")
}

/// The condition and item of a conditional_item, or None when the text is not one.
pub fn split_conditional_item(text: &str) -> Option<(String, &str)> {
    let rest = text.strip_prefix("$<$<BOOL:")?.strip_prefix(OPTION_MARKER)?;
    let (condition, rest) = rest.split_once(OPTION_MARKER_END)?;
    let item = rest.strip_prefix(">:")?.strip_suffix('>')?;
    Some((condition.replace(MARKER_SPACE, " "), item))
}

/// Text with its option and platform markers written as the `${NAME}` they stand for, to show in a comment.
pub fn readable(text: &str) -> String {
    text.replace(OPTION_MARKER, "${")
        .replace(SYMBOL_MARKER, "${")
        .replace(OPTION_MARKER_END, "}")
        .replace(MARKER_SPACE, " ")
}

fn marked<'a>(text: &'a str, marker: &str) -> Option<&'a str> {
    text.strip_prefix(marker).and_then(|rest| rest.strip_suffix(OPTION_MARKER_END))
}

fn is_marker(text: &str) -> bool {
    marked(text, OPTION_MARKER).is_some() || marked(text, SYMBOL_MARKER).is_some()
}

/// The condition a marker value stands for: an option read in a generator expression, or a check's result
/// (`option_marker` of a whole condition, such as `!msvc`).
pub fn marked_condition(text: &str) -> Option<&str> {
    text.strip_prefix(OPTION_MARKER)?.strip_suffix(OPTION_MARKER_END)
}

/// A value read as a condition: true where its text is a true constant, or the option or platform value it names.
fn truth_of(alternatives: &Alternatives) -> Truth {
    alternatives.iter().fold(Truth::Known(false), |result, (condition, text)| {
        let text_truth = if let Some(name) = marked(text, OPTION_MARKER) {
            Truth::Depends(name.replace(MARKER_SPACE, " "))
        } else if let Some(symbol) = marked(text, SYMBOL_MARKER) {
            symbol_truth(symbol)
        } else {
            Truth::Known(constant_truth(text).unwrap_or(!text.is_empty()))
        };
        result.or(condition.clone().and(text_truth))
    })
}

/// A condition used as a value: "1" where it holds, "0" elsewhere.
fn truth_only(truth: Truth) -> Alternatives {
    match truth {
        Truth::Known(true) => vec![(Truth::Known(true), "1".into())],
        Truth::Known(false) => vec![(Truth::Known(true), "0".into())],
        depends => vec![(depends.clone(), "1".into()), (depends.not(), "0".into())],
    }
}

/// `$<condition:value>`: the value where the condition holds, and nothing where it does not (so text around it
/// stays: `a;$<$<C>:b>` is `a` without C).
fn conditional(condition: Truth, value: Alternatives) -> Alternatives {
    let mut results: Alternatives = value
        .into_iter()
        .map(|(value_condition, text)| (condition.clone().and(value_condition), text))
        .filter(|(truth, _)| *truth != Truth::Known(false))
        .collect();
    if condition != Truth::Known(true) {
        results.push((condition.not(), String::new()));
    }
    results
}

/// Concatenates parts: their alternatives multiply out.
fn join_alternatives(parts: Vec<Alternatives>) -> Alternatives {
    let mut results: Alternatives = vec![(Truth::Known(true), String::new())];

    for part in parts {
        let mut next = Vec::new();
        for (condition, prefix) in &results {
            for (part_condition, text) in &part {
                let combined = condition.clone().and(part_condition.clone());
                if combined != Truth::Known(false) {
                    next.push((combined, format!("{prefix}{text}")));
                }
            }
        }
        results = next;
    }

    results
}

/// CMAKE_SYSTEM_NAME / PLATFORM_ID values.
pub fn platform_truth(name: &str) -> Truth {
    match name {
        "Windows" => Truth::Depends("windows".into()),
        "Linux" => Truth::Depends("linux".into()),
        "Darwin" => Truth::Depends("macos".into()),
        _ => Truth::Known(false),
    }
}

/// CMAKE_CXX_COMPILER_ID values (a comma list in generator expressions).
pub fn compiler_truth(names: &str) -> Truth {
    names.split(',').fold(Truth::Known(false), |result, name| {
        let truth = match name {
            "MSVC" => Truth::Depends("cl".into()),
            "Clang" | "AppleClang" | "ARMClang" | "IntelLLVM" => Truth::Depends("clang".into()),
            "GNU" => Truth::Depends("gcc".into()),
            _ => Truth::Known(false),
        };
        result.or(truth)
    })
}

/// The condition a platform variable's value stands for when read as true or false.
pub fn symbol_truth(symbol: &str) -> Truth {
    let condition = match symbol {
        "WIN32" | "CMAKE_HOST_WIN32" => "windows",
        "UNIX" | "CMAKE_HOST_UNIX" => "unix",
        "APPLE" | "CMAKE_HOST_APPLE" => "macos",
        "LINUX" => "linux",
        "MSVC" | "MSVC_VERSION" => "msvc",
        "MINGW" => "mingw",
        "CMAKE_COMPILER_IS_GNUCXX" | "CMAKE_COMPILER_IS_GNUCC" => "gcc",
        "CMAKE_CXX_SIMULATE_ID" | "CMAKE_C_SIMULATE_ID" => "clang-cl",
        "CYGWIN" | "ANDROID" | "IOS" | "EMSCRIPTEN" => return Truth::Known(false),
        _ => return Truth::Known(true),
    };
    Truth::Depends(condition.into())
}

/// The values a platform variable can have, and where it has each; None for one without a table. Compiler versions
/// are a current compiler of each family.
pub fn symbol_candidates(symbol: &str) -> Option<Vec<(&'static str, Truth)>> {
    Some(match symbol {
        "CMAKE_CXX_COMPILER_ID" | "CMAKE_C_COMPILER_ID" => ["MSVC", "Clang", "AppleClang", "GNU"]
            .iter()
            .map(|id| (*id, compiler_truth(id)))
            .collect(),
        "CMAKE_SYSTEM_NAME" | "CMAKE_HOST_SYSTEM_NAME" => ["Windows", "Linux", "Darwin"].iter().map(|name| (*name, platform_truth(name))).collect(),
        "CMAKE_BUILD_TYPE" => ["Debug", "Release", "RelWithDebInfo", "MinSizeRel"]
            .iter()
            .map(|config| (*config, Truth::Depends(profile_name(config))))
            .collect(),
        "CMAKE_CXX_COMPILER_FRONTEND_VARIANT" | "CMAKE_C_COMPILER_FRONTEND_VARIANT" => {
            vec![("MSVC", Truth::Depends("msvc".into())), ("GNU", Truth::Depends("gnu".into()))]
        }
        "CMAKE_SIZEOF_VOID_P" => vec![("8", Truth::Depends("x64 | arm64".into())), ("4", Truth::Depends("x86 | arm".into()))],
        "CMAKE_SYSTEM_PROCESSOR" | "CMAKE_CXX_COMPILER_ARCHITECTURE_ID" | "CMAKE_C_COMPILER_ARCHITECTURE_ID" => PROCESSOR_NAMES
            .iter()
            .map(|(name, atom)| (*name, Truth::Depends(atom.to_string())))
            .collect(),
        "CMAKE_CXX_COMPILER_VERSION" | "CMAKE_C_COMPILER_VERSION" => {
            COMPILER_VERSIONS.iter().map(|(id, version)| (*version, compiler_truth(id))).collect()
        }
        "CMAKE_CXX_SIMULATE_ID" | "CMAKE_C_SIMULATE_ID" => {
            vec![("MSVC", Truth::Depends("clang-cl".into())), ("", Truth::Depends("!clang-cl".into()))]
        }
        "MSVC_VERSION" => vec![("1950", Truth::Depends("msvc".into())), ("", Truth::Depends("!msvc".into()))],
        "CMAKE_GENERATOR" => vec![("Ninja", Truth::Known(true))],
        _ => return None,
    })
}

/// Where a platform variable's value passes `test` (everywhere when each value it can have does).
pub fn symbol_matches(symbol: &str, test: &dyn Fn(&str) -> bool) -> Truth {
    let candidates = symbol_candidates(symbol).unwrap_or_default();
    if !candidates.is_empty() && candidates.iter().all(|(candidate, _)| test(candidate)) {
        return Truth::Known(true);
    }

    candidates.into_iter().fold(
        Truth::Known(false),
        |result, (candidate, truth)| {
            if test(candidate) {
                result.or(truth)
            } else {
                result
            }
        },
    )
}

// CMAKE_SYSTEM_PROCESSOR and the MSVC architecture ids, and the condition each means
pub const PROCESSOR_NAMES: &[(&str, &str)] = &[
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

// A current compiler of each family, for version tests
const COMPILER_VERSIONS: &[(&str, &str)] = &[("MSVC", "19.50"), ("GNU", "15.1"), ("Clang", "20.1")];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genex_conditions() {
        let mut unknown = Vec::new();
        assert_eq!(
            expand_genex("$<$<CONFIG:Debug>:FOO>", &mut unknown, None),
            vec![(Truth::Depends("debug".into()), "FOO".into())]
        );
        assert_eq!(
            expand_genex("$<BUILD_INTERFACE:inc>", &mut unknown, None),
            vec![(Truth::Known(true), "inc".into())]
        );
        assert!(expand_genex("$<INSTALL_INTERFACE:inc>", &mut unknown, None).is_empty());

        let negated = expand_genex("$<$<NOT:$<CONFIG:Shipping>>:Tracy::TracyClient>", &mut unknown, None);
        assert_eq!(negated, vec![(Truth::Depends("!shipping".into()), "Tracy::TracyClient".into())]);

        let platform = expand_genex("$<$<PLATFORM_ID:Windows>:NOMINMAX>", &mut unknown, None);
        assert_eq!(platform, vec![(Truth::Depends("windows".into()), "NOMINMAX".into())]);

        let choice = expand_genex("lib/$<IF:$<CONFIG:Debug>,d,r>/x", &mut unknown, None);
        assert_eq!(
            choice,
            vec![
                (Truth::Depends("debug".into()), "lib/d/x".into()),
                (Truth::Depends("!debug".into()), "lib/r/x".into())
            ]
        );
        assert!(unknown.is_empty());
    }

    #[test]
    fn genex_compilers_languages_and_targets() {
        let mut unknown = Vec::new();
        let depends = |condition: &str, value: &str| vec![(Truth::Depends(condition.into()), value.to_string())];

        // A compiler of a language, and the compiler's version (each family at a current one)
        let msvc_cxx = expand_genex("$<$<COMPILE_LANG_AND_ID:CXX,MSVC>:/wd4251>", &mut unknown, None);
        assert_eq!(msvc_cxx, depends("language.cxx & cl", "/wd4251"));
        let new_enough = expand_genex("$<$<VERSION_GREATER_EQUAL:$<CXX_COMPILER_VERSION>,10>:-Wfoo>", &mut unknown, None);
        assert_eq!(new_enough, vec![(Truth::Known(true), "-Wfoo".to_string())]);
        let old_compilers = expand_genex("$<$<VERSION_LESS:$<CXX_COMPILER_VERSION>,15>:-Wbar>", &mut unknown, None);
        assert_eq!(old_compilers, depends("gcc", "-Wbar"));
        assert_eq!(
            expand_genex("$<$<COMPILE_LANGUAGE:C>:-Wstrict-prototypes>", &mut unknown, None),
            depends("language.c", "-Wstrict-prototypes")
        );
        assert!(unknown.is_empty(), "{unknown:?}");

        // TARGET_EXISTS asks the interpreter; without one it is unknown
        let exists = |name: &str| name == "real";
        assert_eq!(
            expand_genex("$<$<TARGET_EXISTS:real>:real>", &mut unknown, Some(&exists)),
            vec![(Truth::Known(true), "real".to_string())]
        );
        assert!(expand_genex("$<$<TARGET_EXISTS:nosuch>:nosuch>", &mut unknown, Some(&exists)).is_empty());
        assert!(unknown.is_empty(), "{unknown:?}");
        expand_genex("$<$<TARGET_EXISTS:real>:real>", &mut unknown, None);
        assert_eq!(unknown, vec!["$<$<TARGET_EXISTS:real>:real>"]);
    }

    #[test]
    fn unknown_conditions_keep_the_whole_expression() {
        let mut unknown = Vec::new();
        let kept = expand_genex("$<$<FOO_ID:x>:-Wflag>", &mut unknown, None);

        assert_eq!(kept, vec![(Truth::Known(true), "$<$<FOO_ID:x>:-Wflag>".to_string())]);
        assert_eq!(unknown, vec!["$<$<FOO_ID:x>:-Wflag>"]);
    }

    #[test]
    fn language_conditions_decide_per_language() {
        assert_eq!(for_language("language.c & gcc", LANGUAGE_C), Truth::Depends("gcc".into()));
        assert_eq!(for_language("language.c & gcc", LANGUAGE_CXX), Truth::Known(false));
        assert_eq!(
            for_language("(language.c | language.cxx) & debug", LANGUAGE_CXX),
            Truth::Depends("debug".into())
        );
        assert_eq!(for_language("!language.cxx", LANGUAGE_C), Truth::Known(true));
    }

    #[test]
    fn lists_keep_genex_whole() {
        assert_eq!(split_list("a;$<$<CONFIG:Debug>:b;c>;d"), vec!["a", "$<$<CONFIG:Debug>:b;c>", "d"]);
        assert_eq!(list_elements("a;;b\\;c"), vec!["a", "", "b\\;c"]);
        assert!(list_elements("").is_empty());

        let words = ["PRIVATE", "$<$<CONFIG:Debug>:-g", "-O0>", "-Wall"].map(String::from).to_vec();
        assert_eq!(join_genex_fragments(words), vec!["PRIVATE", "$<$<CONFIG:Debug>:-g;-O0>", "-Wall"]);
    }

    #[test]
    fn nested_conditions_keep_the_outer_items() {
        let mut unknown = Vec::new();
        let items = expand_genex_items("$<$<CXX_COMPILER_ID:MSVC>:/W4;$<$<CONFIG:Release>:/WX>>", &mut unknown, None);
        assert_eq!(
            items,
            vec![
                (Truth::Depends("cl".into()), "/W4".to_string()),
                (Truth::Depends("cl & release".into()), "/WX".to_string()),
            ]
        );
        assert_eq!(expand_genex("a$<0:b>c", &mut unknown, None), vec![(Truth::Known(true), "ac".to_string())]);
    }

    #[test]
    fn truth_algebra() {
        let windows = Truth::Depends("windows".into());
        assert_eq!(windows.clone().not().not(), windows);
        assert_eq!(windows.clone().and(Truth::Known(true)), windows);
        assert_eq!(windows.clone().or(Truth::Known(true)), Truth::Known(true));
    }

    #[test]
    fn simplification() {
        let text = |truth: Truth| match truth {
            Truth::Depends(condition) => condition,
            Truth::Known(value) => value.to_string(),
        };

        assert_eq!(text(simplify("((a | b) | c) & (b & c)", None)), "b & c");
        assert_eq!(text(simplify("windows & x", Some("windows"))), "x");
        assert_eq!(text(simplify("windows", Some("windows"))), "true");
        assert_eq!(text(simplify("a & !a", None)), "false");
        assert_eq!(text(simplify("(a & b) | (a & !b) | c", None)), "a | c");
        assert_eq!(text(simplify("(a & b) | (!a & c)", None)), "(a & b) | (!a & c)");
        assert_eq!(text(simplify("msvc & cl", None)), "cl");
        assert_eq!(text(simplify("(false) | (msvc)", None)), "msvc");
        assert_eq!(text(simplify("windows & !unix", None)), "windows");
        assert_eq!(text(simplify("cl & gcc", None)), "false");
    }
}
