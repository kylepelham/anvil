//! Repeated tests: a `[[test]]` with a `matrix` is a template, one test per combination of its axes' entries.
//!
//! ```toml
//! [[test]]
//! name = "stills_{path}_{view}"
//! command = ["python", "stills.py", "--out", "{out}/{name}", "--view", "{view}", "{path-args}"]
//! matrix.path = [
//!     { path = "rt", path-args = [] },
//!     { path = "compute", path-args = ["--", "--no-hwrt"] },
//! ]
//! matrix.view = ["street", "arch", { view = "glass", timeout = 600 }]
//! ```
//!
//! A text entry sets the parameter named after its axis; a table entry sets its keys: parameters, or test keys
//! (`name`, `after`, `timeout`, ...) that replace the template's for its tests. `{param}` takes a parameter anywhere in
//! the template's text; a list parameter fills a whole list item (`"{path-args}"` becomes its words) or is joined:
//! `{path-args:@@}`. `{name}` is the test's name.

use crate::bail_at;
use crate::located::{self, At, Part, Place};
use anyhow::{bail, Context, Result};
use std::collections::HashSet;

// Keys an entry may set for its tests instead of the template's
pub const TEST_KEYS: &[&str] = &[
    "name",
    "command",
    "workdir",
    "timeout",
    "env",
    "if",
    "will-fail",
    "after",
    "serial",
    "pass-regex",
    "fail-regex",
    "skip-exit-code",
    "skip-regex",
    "expect",
    "reject",
    "exit-code",
    "labels",
];

/// A label `anvil test -L` picks tests by: letters, digits, `_`, `.` and `-`, so the command line takes it as it is.
pub fn check_label(label: &str) -> Result<()> {
    let valid = !label.is_empty()
        && label
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-'));
    if !valid {
        bail_at!(
            Place::key("labels").value(label),
            "invalid-label",
            "labels: \"{label}\" is not a label; use letters, digits, `_`, `.` and `-`"
        );
    }
    Ok(())
}

/// A parameter's value.
enum Parameter {
    Text(String),
    List(Vec<String>),
}

/// The tests a `[[test]]` table stands for: itself, or one per combination of its matrix.
pub fn expand(table: &toml::Table) -> Result<Vec<toml::Table>> {
    let Some(matrix) = table.get("matrix") else {
        return Ok(vec![table.clone()]);
    };
    let Some(matrix) = matrix.as_table() else {
        let place = match located::value_text(matrix) {
            Some(text) => Place::key("matrix").value(text),
            None => Place::key("matrix"),
        };
        bail_at!(
            place,
            "invalid-matrix",
            "[[test]] matrix: expected a table of axes (`matrix.view = [\"a\", \"b\"]`)"
        );
    };

    let mut template = table.clone();
    template.remove("matrix");

    // Every combination: one entry of each axis, the first axis outermost
    let mut combinations: Vec<Vec<(&str, usize, &toml::Value)>> = vec![Vec::new()];
    for (axis, entries) in matrix {
        let entries = match entries.as_array() {
            Some(entries) if !entries.is_empty() => entries,
            _ => {
                let place = Place::keys(&["matrix", axis]);
                let place = match located::value_text(entries) {
                    Some(text) => place.value(text),
                    None => place,
                };
                bail_at!(place, "invalid-matrix", "[[test]] matrix.{axis}: expected a list of entries")
            }
        };
        combinations = combinations
            .into_iter()
            .flat_map(|combination| {
                entries.iter().enumerate().map(move |(index, entry)| {
                    let mut combination = combination.clone();
                    combination.push((axis.as_str(), index, entry));
                    combination
                })
            })
            .collect();
    }

    let tests: Vec<toml::Table> = combinations
        .iter()
        .map(|combination| expand_combination(&template, combination))
        .collect::<Result<_>>()?;

    // Two tests of one name (and the same `if`) would be indistinguishable: an axis is missing from the name
    for (position, test) in tests.iter().enumerate() {
        let twin = tests[..position]
            .iter()
            .find(|earlier| earlier.get("name") == test.get("name") && earlier.get("if") == test.get("if"));
        if twin.is_some() {
            let name = test.get("name").and_then(|name| name.as_str()).unwrap_or_default();
            let Some(pattern) = template.get("name").and_then(|name| name.as_str()) else {
                bail_at!(
                    Place::key("matrix").value(name),
                    "invalid-matrix",
                    "[[test]] matrix: two of its entries name their tests `{name}`; test names must be unique"
                );
            };
            let missing: Vec<String> = matrix
                .keys()
                .filter(|axis| !pattern.contains(&format!("{{{axis}}}")))
                .map(|axis| format!("{{{axis}}}"))
                .collect();
            bail_at!(
                Place::key("name").value(pattern),
                "invalid-matrix",
                "[[test]] `{pattern}`: its matrix makes two tests named `{name}`; put every axis in the name pattern (not in it: {})",
                missing.join(", ")
            );
        }
    }

    Ok(tests)
}

fn expand_combination(template: &toml::Table, combination: &[(&str, usize, &toml::Value)]) -> Result<toml::Table> {
    // The entries' parameters (with where each is) and test keys, each set once
    let mut parameters: Vec<(String, Parameter)> = Vec::new();
    let mut places: Vec<Place> = Vec::new();
    let mut keys = toml::Table::new();
    for (axis, index, entry) in combination {
        let entry_place = Place {
            path: vec![Part::Key("matrix".into()), Part::Key(axis.to_string()), Part::Index(*index)],
            ..Default::default()
        };
        let (entry, is_table) = match entry {
            toml::Value::Table(table) => (table.clone(), true),
            text => (toml::Table::from_iter([(axis.to_string(), (*text).clone())]), false),
        };
        for (key, value) in entry {
            let place = if is_table { entry_place.clone().join(&key) } else { entry_place.clone() };
            let taken = keys.contains_key(&key) || parameters.iter().any(|(name, _)| *name == key);
            if taken {
                bail_at!(place, "invalid-matrix", "[[test]] matrix: two entries of one test set `{key}`");
            }
            if TEST_KEYS.contains(&key.as_str()) {
                keys.insert(key, value);
            } else {
                let parameter = parameter(&value)
                    .map_err(|error| error.context(format!("[[test]] matrix.{axis}: `{key}`")))
                    .located(place.clone(), "invalid-matrix")?;
                parameters.push((key, parameter));
                places.push(place);
            }
        }
    }

    // The name first ({name} may not appear in it), then the rest
    let mut filler = Filler {
        name: String::new(),
        parameters: &parameters,
        used: HashSet::new(),
    };
    let name = keys.get("name").or(template.get("name")).and_then(|name| name.as_str());
    let Some(name) = name.map(|name| filler.fill_text(name)) else {
        bail_at!(
            Place::key("matrix"),
            "invalid-matrix",
            "[[test]] with a matrix: give it a `name` pattern (`name = \"stills_{{view}}\"`) or name every entry"
        );
    };
    filler.name = name.clone();

    let mut test = toml::Table::new();
    for (key, value) in template.iter().chain(&keys).filter(|(key, _)| *key != "name") {
        test.insert(key.clone(), filler.fill(value));
    }
    test.insert("name".to_string(), toml::Value::String(name.clone()));

    // A parameter nothing uses is a typo
    if let Some(position) = parameters.iter().position(|(key, _)| !filler.used.contains(key.as_str())) {
        let unused = &parameters[position].0;
        bail_at!(
            places[position].clone(),
            "invalid-matrix",
            "test `{name}`: parameter `{unused}` is used nowhere in its [[test]] (a test key? those are: {})",
            TEST_KEYS.join(", ")
        );
    }

    Ok(test)
}

fn parameter(value: &toml::Value) -> Result<Parameter> {
    Ok(match value {
        toml::Value::String(text) => Parameter::Text(text.clone()),
        toml::Value::Integer(number) => Parameter::Text(number.to_string()),
        toml::Value::Array(items) => Parameter::List(
            items
                .iter()
                .map(|item| item.as_str().map(str::to_string).context("list parameters hold text"))
                .collect::<Result<_>>()?,
        ),
        _ => bail!("a parameter is text or a list of text"),
    })
}

struct Filler<'a> {
    name: String,
    parameters: &'a [(String, Parameter)],
    used: HashSet<&'a str>,
}

impl<'a> Filler<'a> {
    fn fill(&mut self, value: &toml::Value) -> toml::Value {
        match value {
            toml::Value::String(text) => toml::Value::String(self.fill_text(text)),
            toml::Value::Array(items) => {
                let mut filled = Vec::new();
                for item in items {
                    // A whole item naming a list parameter becomes its items
                    if let Some(list) = item.as_str().and_then(|text| self.whole_list(text)) {
                        filled.extend(list.into_iter().map(toml::Value::String));
                        continue;
                    }
                    filled.push(self.fill(item));
                }
                toml::Value::Array(filled)
            }
            toml::Value::Table(table) => toml::Value::Table(table.iter().map(|(key, value)| (key.clone(), self.fill(value))).collect()),
            other => other.clone(),
        }
    }

    /// `{param}` where `param` is a list parameter: its items.
    fn whole_list(&mut self, text: &str) -> Option<Vec<String>> {
        let key = text.strip_prefix('{')?.strip_suffix('}')?;
        let (name, parameter) = self.parameters.iter().find(|(name, _)| name == key)?;
        let Parameter::List(items) = parameter else { return None };
        self.used.insert(name.as_str());
        Some(items.clone())
    }

    /// `{name}`, `{param}` and `{param:separator}` replaced, also inside other placeholders (`{target:{program}}`);
    /// other placeholders left for later.
    fn fill_text(&mut self, text: &str) -> String {
        let mut result = String::new();
        let mut rest = text;
        while let Some(start) = rest.find('{') {
            result.push_str(&rest[..start]);
            let after = &rest[start + 1..];
            let Some(end) = after.find('}') else {
                result.push_str(&rest[start..]);
                return result;
            };

            // A placeholder inside another (`{target:{program}}`): the inner one first
            if after[..end].contains('{') {
                result.push('{');
                rest = after;
                continue;
            }

            let inside = &after[..end];
            let (key, separator) = match inside.split_once(':') {
                Some((key, separator)) => (key, Some(separator)),
                None => (inside, None),
            };
            match self.value_of(key, separator) {
                Some(value) => result.push_str(&value),
                None => result.push_str(&rest[start..start + end + 2]),
            }
            rest = &after[end + 1..];
        }

        result.push_str(rest);
        result
    }

    fn value_of(&mut self, key: &str, separator: Option<&str>) -> Option<String> {
        if key == "name" && separator.is_none() && !self.name.is_empty() {
            return Some(self.name.clone());
        }

        let (name, parameter) = self.parameters.iter().find(|(name, _)| name == key)?;
        self.used.insert(name.as_str());
        Some(match parameter {
            Parameter::Text(text) => text.clone(),
            Parameter::List(items) => items.join(separator.unwrap_or(" ")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(test: &toml::Table) -> Vec<&str> {
        test["command"].as_array().unwrap().iter().map(|word| word.as_str().unwrap()).collect()
    }

    #[test]
    fn expands_every_combination() {
        let text = r#"
name = "stills_{path}_{view}"
command = ["py", "--out", "{out}/{name}", "--view", "{view}", "{path-args}", "-DARGS={path-args:;}"]
matrix.path = [{ path = "rt", path-args = [] }, { path = "cpu", path-args = ["--", "--no-hwrt"] }]
matrix.view = ["street", { view = "glass", timeout = 600, after = ["{view}_assets"] }]
"#;
        let table: toml::Table = toml::from_str(text).unwrap();
        let tests = expand(&table).unwrap();
        let names: Vec<&str> = tests.iter().map(|test| test["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["stills_rt_street", "stills_rt_glass", "stills_cpu_street", "stills_cpu_glass"]);

        assert_eq!(
            command(&tests[0]),
            ["py", "--out", "{out}/stills_rt_street", "--view", "street", "-DARGS="]
        );
        assert_eq!(
            command(&tests[3]),
            [
                "py",
                "--out",
                "{out}/stills_cpu_glass",
                "--view",
                "glass",
                "--",
                "--no-hwrt",
                "-DARGS=--;--no-hwrt"
            ]
        );

        // An entry's test keys are its tests' own, filled in too
        assert!(tests[0].get("timeout").is_none());
        assert_eq!(tests[3]["timeout"].as_integer(), Some(600));
        assert_eq!(tests[3]["after"].as_array().unwrap()[0].as_str(), Some("glass_assets"));

        // Inside another placeholder
        let text = "name = \"run_{program}\"
command = [\"{target:{program}}\"]
matrix.program = [\"app\"]
";
        let tests = expand(&toml::from_str(text).unwrap()).unwrap();
        assert_eq!(command(&tests[0]), ["{target:app}"]);
    }

    #[test]
    fn entries_may_name_their_tests() {
        let text = "command = [\"x\", \"{level}\"]\nmatrix.level = [{ name = \"town_test\", level = \"town\" }]\n";
        let tests = expand(&toml::from_str(text).unwrap()).unwrap();
        assert_eq!(tests[0]["name"].as_str(), Some("town_test"));
    }

    #[test]
    fn rejects_mistakes() {
        let expand_text = |text: &str| expand(&toml::from_str(text).unwrap());

        // An unused parameter (a misspelt key), no name, a key set twice
        assert!(expand_text("name = \"t_{v}\"\ncommand = [\"x\"]\nmatrix.v = [{ v = \"a\", w = \"b\" }]\n").is_err());
        assert!(expand_text("command = [\"x\", \"{v}\"]\nmatrix.v = [\"a\"]\n").is_err());
        assert!(
            expand_text("name = \"t_{v}\"\ncommand = [\"{v}\"]\nmatrix.v = [{ v = \"a\", timeout = 1 }]\nmatrix.w = [{ timeout = 2 }]\n").is_err()
        );

        // Two tests of one name: an axis left out of the name
        let error = expand_text("name = \"a_{x}\"\ncommand = [\"{x}\", \"{y}\"]\nmatrix.x = [\"1\"]\nmatrix.y = [\"p\", \"q\"]\n").unwrap_err();
        assert!(error.to_string().contains("two tests named `a_1`") && error.to_string().contains("{y}"));
    }
}
