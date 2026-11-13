//! Tests that run a CMake script whose only work is to run one command and check its exit code and output against
//! lists of regexes (`cmake -DCOMMAND=exe@@arg -DEXPECT=re@@re -P run_and_expect.cmake`) become plain tests of that
//! command with `expect`, `reject` and `exit-code`, so they run without CMake. A script that does anything more, or a
//! test that passes it anything else, stays a `cmake -P` test.

use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// A test that ran a check script, as a test of the command the script ran.
#[derive(Debug, PartialEq)]
pub(super) struct ScriptTest {
    pub command: Vec<String>,
    /// Regexes in Anvil's syntax.
    pub expect: Vec<String>,
    pub reject: Vec<String>,
    pub exit_code: i32,
}

/// The test `words` as the command its script runs, when the script is a check script (`read` gives a file's text).
pub(super) fn convert(words: &[String], read: &dyn Fn(&str) -> Option<String>) -> Option<ScriptTest> {
    // cmake -DNAME=value ... -P script
    let (program, rest) = words.split_first()?;
    let is_cmake = Path::new(program).file_stem().is_some_and(|stem| stem.eq_ignore_ascii_case("cmake"));
    let [defines @ .., mode, script] = rest else { return None };
    if !is_cmake || mode != "-P" {
        return None;
    }
    let mut values = HashMap::new();
    for define in defines {
        let (name, value) = define.strip_prefix("-D")?.split_once('=')?;
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

    // Regexes Anvil cannot read as CMake meant them keep the script
    let regexes = |items: Vec<String>| -> Option<Vec<String>> {
        let converted: Vec<String> = items.iter().map(|item| anvil_regex(item)).collect();
        converted.iter().all(|pattern| Regex::new(pattern).is_ok()).then_some(converted)
    };
    let exit_code = match shape.exit.as_ref().and_then(|name| values.get(name)) {
        Some(value) => value.trim().parse().ok()?,
        None => shape.exit_default,
    };

    Some(ScriptTest {
        command,
        expect: regexes(list(shape.expect.as_ref()))?,
        reject: regexes(list(shape.reject.as_ref()))?,
        exit_code,
    })
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
}

impl Shape {
    fn read(text: &str) -> Option<Shape> {
        let text = strip_comments(text);
        let pattern = |source: &str| Regex::new(source).unwrap();

        // Only commands that compute and report: nothing that writes files or runs more
        let commands: Vec<String> = pattern(r"(?m)^\s*([A-Za-z_]\w*)\s*\(")
            .captures_iter(&text)
            .map(|captures| captures[1].to_lowercase())
            .collect();
        let allowed = |command: &String| ALLOWED_COMMANDS.contains(&command.as_str());
        let runs = commands.iter().filter(|command| *command == "execute_process").count();
        if !commands.iter().all(allowed) || runs != 1 {
            return None;
        }

        // execute_process(COMMAND ${COMMAND} RESULT_VARIABLE r OUTPUT_VARIABLE o ERROR_VARIABLE e)
        let run = pattern(r"execute_process\(\s*COMMAND\s+\$\{(\w+)\}((?:\s+(?:RESULT_VARIABLE|OUTPUT_VARIABLE|ERROR_VARIABLE)\s+\w+)+)\s*\)")
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
            let joined = pattern(r#"set\(\s*(\w+)\s+"\$\{(\w+)\}\$\{(\w+)\}"\s*\)"#)
                .captures_iter(&text)
                .find(|captures| (&captures[2] == out && &captures[3] == err) || (&captures[2] == err && &captures[3] == out))?;
            joined[1].to_string()
        };

        // The exit code compared: a variable with a default, or a number
        let exit_check = pattern(r#"if\(\s*NOT\s+"?\$\{(\w+)\}"?\s+(?:STREQUAL|EQUAL)\s+"?(?:\$\{(\w+)\}|(-?\d+))"?\s*\)"#)
            .captures_iter(&text)
            .find(|captures| &captures[1] == result)?;
        let (exit, exit_default) = match (exit_check.get(2), exit_check.get(3)) {
            (Some(name), _) => {
                let name = name.as_str().to_string();
                let default = pattern(r#"if\(\s*NOT\s+DEFINED\s+(\w+)\s*\)\s*set\(\s*(\w+)\s+"?(-?\d+)"?\s*\)"#)
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
        let checks = pattern(r#"foreach\(\s*(\w+)\s+IN\s+LISTS\s+(\w+)\s*\)\s*if\(\s*(NOT\s+)?("?\$\{\w+\}"?|\w+)\s+MATCHES\s+"\$\{(\w+)\}"\s*\)"#);
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

        let separators: HashMap<String, String> = pattern(r#"string\(\s*REPLACE\s+"([^"]+)"\s+";"\s+(\w+)\s+"\$\{(\w+)\}"\s*\)"#)
            .captures_iter(&text)
            .filter(|captures| captures[2] == captures[3])
            .map(|captures| (captures[2].to_string(), captures[1].to_string()))
            .collect();

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

/// A CMake regex in Anvil's (Rust's) syntax: braces are literal in CMake, and `.` matches line ends too.
pub(super) fn anvil_regex(cmake: &str) -> String {
    let mut converted = String::new();
    let mut characters = cmake.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => {
                converted.push('\\');
                converted.extend(characters.next());
            }
            '{' | '}' => {
                converted.push('\\');
                converted.push(character);
            }
            _ => converted.push(character),
        }
    }

    if converted.contains('.') {
        format!("(?s){converted}")
    } else {
        converted
    }
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
}
