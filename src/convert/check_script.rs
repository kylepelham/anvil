//! Tests that run a CMake script whose only work is to run one command and check its exit code and output against
//! lists of regexes (`cmake -DCOMMAND=exe@@arg -DEXPECT=re@@re -P run_and_expect.cmake`) become plain tests of that
//! command with `expect`, `reject` and `exit-code`, so they run without CMake. A script that does anything more, or a
//! test that passes it anything else, stays a `cmake -P` test.

use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{LazyLock, Mutex};

/// A test that ran a check script, as a test of the command the script ran.
#[derive(Debug, PartialEq)]
pub(super) struct ScriptTest {
    pub command: Vec<String>,
    /// Regexes in Anvil's syntax.
    pub expect: Vec<String>,
    pub reject: Vec<String>,
    pub exit_code: i32,
    /// Environment variables the script set (`set(ENV{X} value)`) before running the command.
    pub env: Vec<(String, String)>,
}

/// The test `words` as the command its script runs, when the script is a check script (`read` gives a file's text).
pub(super) fn convert(words: &[String], read: &dyn Fn(&str) -> Option<String>) -> Option<ScriptTest> {
    // cmake -DNAME[:TYPE]=value ... -P script
    let (program, rest) = words.split_first()?;
    let is_cmake = Path::new(program).file_stem().is_some_and(|stem| stem.eq_ignore_ascii_case("cmake"));
    let [defines @ .., mode, script] = rest else { return None };
    if !is_cmake || mode != "-P" {
        return None;
    }
    let mut values = HashMap::new();
    for define in defines {
        let (name, value) = define.strip_prefix("-D")?.split_once('=')?;
        let name = name.split(':').next().unwrap_or(name);
        values.insert(name.to_string(), value.to_string());
    }

    let shape = Shape::read(&read(script)?)?;
    let known = [Some(&shape.command), shape.expect.as_ref(), shape.reject.as_ref(), shape.exit.as_ref()];
    if !values.keys().all(|name| known.contains(&Some(name))) {
        return None;
    }

    let list = |name: Option<&String>| -> Vec<String> {
        let Some(value) = name.and_then(|name| values.get(name)) else {
            return Vec::new();
        };
        if shape.whole.contains(name.unwrap()) {
            return [value.clone()].into_iter().filter(|value| !value.is_empty()).collect();
        }
        let separator = shape.separators.get(name.unwrap()).map(String::as_str).unwrap_or(";");
        value
            .replace(separator, ";")
            .split(';')
            .filter(|item| !item.is_empty())
            .map(str::to_string)
            .collect()
    };
    let command = list(Some(&shape.command));
    if command.is_empty() {
        return None;
    }

    // Regexes Anvil cannot read as CMake meant them keep the script; ones the script escaped are plain text
    let regexes = |name: Option<&String>| -> Option<Vec<String>> {
        let escaped = name.and_then(|name| shape.escaped.get(name));
        let mut converted = Vec::new();
        for item in list(name) {
            match escaped {
                Some(characters) => converted.push(literal_regex(&item, characters)?),
                None => converted.push(anvil_regex(&item)),
            }
        }
        converted.iter().all(|pattern| Regex::new(pattern).is_ok()).then_some(converted)
    };
    let exit_code = match shape.exit.as_ref().and_then(|name| values.get(name)) {
        Some(value) => value.trim().parse().ok()?,
        None => shape.exit_default,
    };

    Some(ScriptTest {
        command,
        expect: regexes(shape.expect.as_ref())?,
        reject: regexes(shape.reject.as_ref())?,
        exit_code,
        env: shape.env.clone(),
    })
}

/// A pattern of Shape::read, compiled once (each test reads its script again).
fn compiled(source: &str) -> Regex {
    static COMPILED: LazyLock<Mutex<HashMap<String, Regex>>> = LazyLock::new(Default::default);
    let mut compiled = COMPILED.lock().unwrap();
    compiled.entry(source.to_string()).or_insert_with(|| Regex::new(source).unwrap()).clone()
}

/// The variables a check script reads, and how it splits them.
struct Shape {
    command: String,
    expect: Option<String>,
    reject: Option<String>,
    /// The variable holding the expected exit code, and its default (or a fixed code, with no variable).
    exit: Option<String>,
    exit_default: i32,
    /// Variables split at something other than `;` (`string(REPLACE "@@" ";" COMMAND "${COMMAND}")`).
    separators: HashMap<String, String>,
    /// Variables read as one regex, not a list of them (`if(EXPECTED AND NOT OUTPUT MATCHES "${EXPECTED}")`).
    whole: HashSet<String>,
    /// Variables a macro escaped before matching, so their text matches literally: the characters it escaped.
    escaped: HashMap<String, String>,
    /// `set(ENV{X} value)` before the command runs.
    env: Vec<(String, String)>,
}

impl Shape {
    fn read(text: &str) -> Option<Shape> {
        let text = strip_comments(text);
        let pattern = compiled;

        // A macro that only escapes a variable's regex characters (`string(REGEX REPLACE "([.+*?^$()])" "\\\\\\1" ...)`)
        let escaping_macro = pattern(
            r#"(?i)macro\s*\(\s*(\w+)\s+(\w+)\s*\)\s*string\s*\(\s*REGEX\s+REPLACE\s+"\(\[([^\]"]+)\]\)"\s+"(?:\\\\)+1"\s+\$\{(\w+)\}\s+"\$\{\$\{(\w+)\}\}"\s*\)\s*endmacro\s*\(\s*\w*\s*\)"#,
        )
        .captures(&text)
        .filter(|captures| captures[2] == captures[4] && captures[2] == captures[5])
        .map(|captures| (captures[1].to_lowercase(), captures[3].to_string()));

        // Only commands that compute and report: nothing that writes files or runs more
        let commands: Vec<String> = pattern(r"(?m)^\s*([A-Za-z_]\w*)\s*\(")
            .captures_iter(&text)
            .map(|captures| captures[1].to_lowercase())
            .collect();
        let allowed = |command: &String| {
            let escaping = escaping_macro
                .as_ref()
                .is_some_and(|(name, _)| command == name || command == "macro" || command == "endmacro");
            ALLOWED_COMMANDS.contains(&command.as_str()) || escaping
        };
        let runs = commands.iter().filter(|command| *command == "execute_process").count();
        if !commands.iter().all(allowed) || runs != 1 {
            return None;
        }

        // execute_process(COMMAND ${COMMAND} RESULT_VARIABLE r OUTPUT_VARIABLE o ERROR_VARIABLE e)
        let run = pattern(r"execute_process\s*\(\s*COMMAND\s+\$\{(\w+)\}((?:\s+(?:RESULT_VARIABLE|OUTPUT_VARIABLE|ERROR_VARIABLE)\s+\w+)+)\s*\)")
            .captures(&text)?;
        let command = run[1].to_string();
        let outputs: HashMap<String, String> = pattern(r"(\w+)\s+(\w+)")
            .captures_iter(&run[2])
            .map(|captures| (captures[1].to_string(), captures[2].to_string()))
            .collect();
        let (result, out, err) = (
            outputs.get("RESULT_VARIABLE")?,
            outputs.get("OUTPUT_VARIABLE")?,
            outputs.get("ERROR_VARIABLE")?,
        );

        // The output checked: both streams together
        let all = if out == err {
            out.clone()
        } else {
            let joined = pattern(r#"set\s*\(\s*(\w+)\s+"\$\{(\w+)\}\$\{(\w+)\}"\s*\)"#)
                .captures_iter(&text)
                .find(|captures| (&captures[2] == out && &captures[3] == err) || (&captures[2] == err && &captures[3] == out))?;
            joined[1].to_string()
        };

        // The exit code compared (`${result}` or `result`): a variable with a default, or a number
        let exit_check = pattern(r#"if\s*\(\s*NOT\s+"?(?:\$\{(\w+)\}|(\w+))"?\s+(?:STREQUAL|EQUAL)\s+"?(?:\$\{(\w+)\}|(-?\d+)|(\w+))"?\s*\)"#)
            .captures_iter(&text)
            .find(|captures| captures.get(1).or(captures.get(2)).is_some_and(|name| name.as_str() == result))?;
        let (exit, exit_default) = match (exit_check.get(3).or(exit_check.get(5)), exit_check.get(4)) {
            (Some(name), _) => {
                let name = name.as_str().to_string();
                let default = pattern(r#"if\s*\(\s*NOT\s+DEFINED\s+(\w+)\s*\)\s*set\s*\(\s*(\w+)\s+"?(-?\d+)"?\s*\)"#)
                    .captures_iter(&text)
                    .find(|captures| captures[1] == name && captures[2] == name)?;
                (Some(name), default[3].parse().ok()?)
            }
            (None, Some(code)) => (None, code.as_str().parse().ok()?),
            _ => return None,
        };

        // foreach(re IN LISTS EXPECT) if(NOT all MATCHES "${re}"): must match; without NOT: must not
        let mut expect = None;
        let mut reject = None;
        let checks =
            pattern(r#"foreach\s*\(\s*(\w+)\s+IN\s+LISTS\s+(\w+)\s*\)\s*if\s*\(\s*(NOT\s+)?("?\$\{\w+\}"?|\w+)\s+MATCHES\s+"\$\{(\w+)\}"\s*\)"#);
        for captures in checks.captures_iter(&text) {
            let subject = captures[4].trim_matches('"').trim_start_matches("${").trim_end_matches('}');
            if subject != all || captures[1] != captures[5] {
                return None;
            }
            let slot = if captures.get(3).is_some() { &mut expect } else { &mut reject };
            if slot.replace(captures[2].to_string()).is_some() {
                return None;
            }
        }

        // if(EXPECTED AND NOT all MATCHES "${EXPECTED}"): one regex that must match (when given); without NOT: must not
        let mut whole = HashSet::new();
        let single_checks = pattern(r#"if\s*\(\s*(\w+)\s+AND\s+(NOT\s+)?("?\$\{\w+\}"?|\w+)\s+MATCHES\s+"\$\{(\w+)\}"\s*\)"#);
        for captures in single_checks.captures_iter(&text) {
            let subject = captures[3].trim_matches('"').trim_start_matches("${").trim_end_matches('}');
            if subject != all || captures[1] != captures[4] {
                return None;
            }
            let slot = if captures.get(2).is_some() { &mut expect } else { &mut reject };
            if slot.replace(captures[1].to_string()).is_some() {
                return None;
            }
            whole.insert(captures[1].to_string());
        }

        let separators: HashMap<String, String> = pattern(r#"string\s*\(\s*REPLACE\s+"([^"]+)"\s+";"\s+(\w+)\s+"\$\{(\w+)\}"\s*\)"#)
            .captures_iter(&text)
            .filter(|captures| captures[2] == captures[3])
            .map(|captures| (captures[2].to_string(), captures[1].to_string()))
            .collect();

        // The variables the escaping macro is called on
        let mut escaped = HashMap::new();
        if let Some((name, characters)) = &escaping_macro {
            let calls = pattern(&format!(r"(?im)^\s*{}\s*\(\s*(\w+)\s*\)", regex::escape(name)));
            for captures in calls.captures_iter(&text) {
                escaped.insert(captures[1].to_string(), characters.clone());
            }
        }

        // set(ENV{X} "value"): the test's environment
        let env: Vec<(String, String)> = pattern(r#"(?m)^\s*set\s*\(\s*ENV\{(\w+)\}\s+(?:"([^"$\\]*)"|([^\s")$\\]+))\s*\)"#)
            .captures_iter(&text)
            .map(|captures| {
                let value = captures.get(2).or(captures.get(3)).map(|value| value.as_str()).unwrap_or_default();
                (captures[1].to_string(), value.to_string())
            })
            .collect();
        let env_sets = pattern(r"(?i)set\s*\(\s*ENV\{").find_iter(&text).count();
        if env_sets != env.len() {
            return None;
        }

        // It reads nothing else from outside: every variable it uses is one of these, or set in the script
        let inputs: HashSet<&str> = [Some(&command), expect.as_ref(), reject.as_ref(), exit.as_ref()]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect();
        let without_references = pattern(r"\$\{\w+\}").replace_all(&text, "");
        let bare_words: HashSet<&str> = pattern(r"\w+").find_iter(&without_references).map(|word| word.as_str()).collect();
        for reference in pattern(r"\$\{(\w+)\}").captures_iter(&text) {
            let name = reference.get(1).unwrap().as_str();
            if !inputs.contains(name) && !bare_words.contains(name) && !name.starts_with("CMAKE_") {
                return None;
            }
        }

        Some(Shape {
            command,
            expect,
            reject,
            exit,
            exit_default,
            separators,
            whole,
            escaped,
            env,
        })
    }
}

// Commands a check script may use
const ALLOWED_COMMANDS: &[&str] = &[
    "if",
    "elseif",
    "else",
    "endif",
    "foreach",
    "endforeach",
    "set",
    "unset",
    "string",
    "math",
    "list",
    "message",
    "execute_process",
    "cmake_minimum_required",
    "cmake_policy",
];

/// A value a script escaped (`characters` made literal) as an Anvil regex matching it literally; None when it keeps
/// other regex characters, which CMake still reads as a regex.
fn literal_regex(value: &str, characters: &str) -> Option<String> {
    // The regex the script matches: the value with a backslash before each escaped character
    let escaped: String = value
        .chars()
        .flat_map(|character| characters.contains(character).then_some('\\').into_iter().chain([character]))
        .collect();

    // As CMake reads it: `\c` is the character c; any other special character means a real regex
    let mut text = String::new();
    let mut rest = escaped.chars();
    while let Some(character) = rest.next() {
        match character {
            '\\' => text.push(rest.next()?),
            _ if CMAKE_REGEX_CHARACTERS.contains(character) => return None,
            _ => text.push(character),
        }
    }

    Some(regex::escape(&text))
}

// Characters CMake's regexes give a meaning (braces are literal there)
const CMAKE_REGEX_CHARACTERS: &str = ".+*?^$()[]|\\";

/// A CMake regex in Anvil's (Rust's) syntax. CMake's has `^ $ . [ ] [^ ] * + ? ( ) |`, and `\` before a character
/// that stands for itself (`\d` is a `d`); braces are literal, a bracket expression takes what is up to its `]`
/// literally (`^` first and ranges aside, `\` included), and `.` matches line ends too.
pub(super) fn anvil_regex(cmake: &str) -> String {
    let characters: Vec<char> = cmake.chars().collect();
    let mut converted = String::new();
    let mut any_character = false;

    let mut index = 0;
    while let Some(&character) = characters.get(index) {
        index += 1;
        match character {
            '\\' => match characters.get(index) {
                Some(&escaped) => {
                    push_literal(&mut converted, escaped);
                    index += 1;
                }
                None => converted.push_str(r"\\"),
            },
            '{' | '}' => push_literal(&mut converted, character),
            '[' => index = push_bracket(&mut converted, &characters, index),
            '.' => {
                any_character = true;
                converted.push('.');
            }
            _ => converted.push(character),
        }
    }

    if any_character {
        format!("(?s){converted}")
    } else {
        converted
    }
}

/// A character matched as itself, in or out of a class.
fn push_literal(converted: &mut String, character: char) {
    if "\\.+*?()|[]{}^$#&-~".contains(character) {
        converted.push('\\');
    }
    converted.push(character);
}

/// A CMake bracket expression from `start` (after its `[`) as a Rust class; returns where it ends. A `]` first is
/// literal, `a-z` is a range and any other `-` literal. One without its `]` is left unclosed, which Rust rejects too.
fn push_bracket(converted: &mut String, characters: &[char], start: usize) -> usize {
    let mut index = start;
    converted.push('[');
    if characters.get(index) == Some(&'^') {
        converted.push('^');
        index += 1;
    }

    let first = index;
    while let Some(&character) = characters.get(index) {
        if character == ']' && index > first {
            converted.push(']');
            return index + 1;
        }

        // A range, or one character
        let range_end = characters.get(index + 2).filter(|end| characters[index + 1] == '-' && **end != ']');
        push_literal(converted, character);
        match range_end {
            Some(&end) => {
                converted.push('-');
                push_literal(converted, end);
                index += 3;
            }
            None => index += 1,
        }
    }

    index
}

/// The script without `#` comments (outside quotes).
fn strip_comments(text: &str) -> String {
    let mut stripped = String::new();
    for line in text.lines() {
        let mut quoted = false;
        let mut previous = ' ';
        for character in line.chars() {
            if character == '"' && previous != '\\' {
                quoted = !quoted;
            }
            if character == '#' && !quoted {
                break;
            }
            stripped.push(character);
            previous = character;
        }
        stripped.push('\n');
    }
    stripped
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUN_AND_EXPECT: &str = r#"# Runs a program and checks its output
if(NOT DEFINED EXIT_CODE)
  set(EXIT_CODE 0)
endif()
string(REPLACE "@@" ";" COMMAND "${COMMAND}")
string(REPLACE "@@" ";" EXPECT "${EXPECT}")
string(REPLACE "@@" ";" REJECT "${REJECT}")
execute_process(COMMAND ${COMMAND} RESULT_VARIABLE result OUTPUT_VARIABLE out ERROR_VARIABLE err)
set(all "${out}${err}")
set(failed FALSE)
if(NOT "${result}" STREQUAL "${EXIT_CODE}")
  message(SEND_ERROR "exit code ${result}, expected ${EXIT_CODE}")
  set(failed TRUE)
endif()
foreach(re IN LISTS EXPECT)
  if(NOT all MATCHES "${re}")
    message(SEND_ERROR "output has no match for '${re}'")
    set(failed TRUE)
  endif()
endforeach()
foreach(re IN LISTS REJECT)
  if(all MATCHES "${re}")
    message(SEND_ERROR "output matches rejected '${re}': '${CMAKE_MATCH_0}'")
    set(failed TRUE)
  endif()
endforeach()
if(failed)
  string(LENGTH "${all}" n)
  if(n GREATER 6000)
    math(EXPR start "${n} - 6000")
    string(SUBSTRING "${all}" ${start} 6000 all)
  endif()
  message(FATAL_ERROR "output (end):\n${all}")
endif()
"#;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn check_scripts_become_plain_tests() {
        let read = |_: &str| Some(RUN_AND_EXPECT.to_string());
        let test = words(&[
            "cmake",
            "-DCOMMAND=app@@--json",
            "-DEXPECT={\"id\": 1}@@ran .* frames",
            "-DREJECT=ERROR",
            "-DEXIT_CODE=2",
            "-P",
            "run.cmake",
        ]);

        assert_eq!(
            convert(&test, &read),
            Some(ScriptTest {
                command: words(&["app", "--json"]),
                expect: words(&[r#"\{"id": 1\}"#, "(?s)ran .* frames"]),
                reject: words(&["ERROR"]),
                exit_code: 2,
                env: Vec::new(),
            })
        );

        // The default exit code; no lists
        let test = words(&["cmake", "-DCOMMAND=app", "-P", "run.cmake"]);
        assert_eq!(convert(&test, &read).unwrap().exit_code, 0);
    }

    #[test]
    fn other_scripts_stay() {
        let read = |_: &str| Some(RUN_AND_EXPECT.to_string());

        // A variable the script does not know of, or no command
        assert!(convert(&words(&["cmake", "-DCOMMAND=app", "-DSCRATCH=x", "-P", "run.cmake"]), &read).is_none());
        assert!(convert(&words(&["cmake", "-DEXPECT=x", "-P", "run.cmake"]), &read).is_none());

        // A script that writes a file, or reads another input
        let writes = |_: &str| Some(format!("{RUN_AND_EXPECT}\nfile(WRITE out.txt \"${{all}}\")\n"));
        assert!(convert(&words(&["cmake", "-DCOMMAND=app", "-P", "run.cmake"]), &writes).is_none());
        let reads_more = |_: &str| Some(RUN_AND_EXPECT.replace("message(SEND_ERROR \"exit code", "message(SEND_ERROR \"${MODE} exit code"));
        assert!(convert(&words(&["cmake", "-DCOMMAND=app", "-P", "run.cmake"]), &reads_more).is_none());
    }

    // gflags' cmake/execute_test.cmake, trimmed
    const EXECUTE_TEST: &str = r#"macro (sanitize_for_regex STRVAR)
  string (REGEX REPLACE "([.+*?^$()])" "\\\\\\1" ${STRVAR} "${${STRVAR}}")
endmacro ()
if (NOT COMMAND)
  message (FATAL_ERROR "Test command not specified!")
endif ()
if (NOT DEFINED EXPECTED_RC)
  set (EXPECTED_RC 0)
endif ()
if (EXPECTED_OUTPUT)
  sanitize_for_regex(EXPECTED_OUTPUT)
endif ()
if (UNEXPECTED_OUTPUT)
  sanitize_for_regex(UNEXPECTED_OUTPUT)
endif ()
set (ENV{FLAGS_undefok} "foo,bar")
set (ENV{FLAGS_weirdo}  "")
execute_process(
  COMMAND ${COMMAND}
  RESULT_VARIABLE RC
  OUTPUT_VARIABLE OUTPUT
  ERROR_VARIABLE  OUTPUT
)
if (NOT RC EQUAL EXPECTED_RC)
  message (FATAL_ERROR "Exit status is ${RC}, expected ${EXPECTED_RC}")
endif ()
if (EXPECTED_OUTPUT AND NOT OUTPUT MATCHES "${EXPECTED_OUTPUT}")
  message (FATAL_ERROR "Test output does not match expected output: ${EXPECTED_OUTPUT}")
endif ()
if (UNEXPECTED_OUTPUT AND OUTPUT MATCHES "${UNEXPECTED_OUTPUT}")
  message (FATAL_ERROR "Test output matches unexpected output: ${UNEXPECTED_OUTPUT}")
endif ()
"#;

    #[test]
    fn typed_defines_escaped_text_and_environment() {
        let read = |_: &str| Some(EXECUTE_TEST.to_string());
        let test = words(&[
            "cmake",
            "-DCOMMAND:STRING=app;--x",
            "-DEXPECTED_RC:STRING=1",
            "-DEXPECTED_OUTPUT:STRING=a.b (c)",
            "-DUNEXPECTED_OUTPUT:STRING=",
            "-P",
            "execute_test.cmake",
        ]);

        // The escaped value matches literally, whole; the environment the script set comes along
        assert_eq!(
            convert(&test, &read),
            Some(ScriptTest {
                command: words(&["app", "--x"]),
                expect: words(&[r"a\.b \(c\)"]),
                reject: Vec::new(),
                exit_code: 1,
                env: vec![
                    ("FLAGS_undefok".to_string(), "foo,bar".to_string()),
                    ("FLAGS_weirdo".to_string(), String::new())
                ],
            })
        );

        // A character the macro leaves special keeps the script
        let test = words(&["cmake", "-DCOMMAND=app", "-DEXPECTED_OUTPUT=[ab]", "-P", "execute_test.cmake"]);
        assert!(convert(&test, &read).is_none());

        // A backslash escapes the next character, as in CMake's regexes (gflags' `\\gflags.cc:` on Windows)
        let test = words(&["cmake", "-DCOMMAND=app", r"-DEXPECTED_OUTPUT=\\gflags.cc:", "-P", "execute_test.cmake"]);
        assert_eq!(convert(&test, &read).unwrap().expect, words(&[r"\\gflags\.cc:"]));
    }

    #[test]
    fn cmake_regexes_read_as_in_cmake() {
        let matches = |cmake: &str, text: &str| regex::Regex::new(&anvil_regex(cmake)).unwrap().is_match(text);

        // Braces and escaped letters are literal; `.` matches line ends
        assert_eq!(anvil_regex(r#""source-location": {"#), r#""source-location": \{"#);
        assert!(matches(r"a{2}", "a{2}") && !matches(r"a{2}", "aa"));
        assert!(matches(r"\d\.", "d.") && !matches(r"\d", "1"));
        assert!(matches("a.b", "a\nb"));

        // A bracket expression takes `\`, `[` and `&&` literally, `]` first and `-` last too
        assert!(matches(r"[/\]x", r"\x") && matches(r"[/\]x", "/x"));
        assert!(matches(r"[]a]", "]") && matches(r"[a-]", "-") && matches(r"[^a-c]", "d") && !matches(r"[^a-c]", "b"));
        assert!(matches(r"[[&&]", "[") && matches(r"[[&&]", "&"));

        // Unclosed brackets do not read
        assert!(regex::Regex::new(&anvil_regex("[abc")).is_err());
    }
}
