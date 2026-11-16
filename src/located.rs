//! Where an error or warning about a manifest or tests.toml is: the key path of the table, key and value it is about,
//! and the file. Leaves say what they know (`kind`, the value), scopes around them add the table and the file as the
//! error passes out, with the same context text as before, so the command line's message reads as it always did and
//! the language server marks the exact place.

use std::path::{Path, PathBuf};

/// One step of a key path: a key, or an item of an array (also an array of tables, `[[test]]`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Part {
    Key(String),
    Index(usize),
}

/// A place in a manifest or tests.toml.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Place {
    pub file: Option<PathBuf>,
    pub path: Vec<Part>,
    /// The value inside the key the message is about (a list's item, a word of a condition).
    pub value: Option<String>,
    /// The path starts at the top of its file: scopes around it add their text and file, not their keys.
    pub absolute: bool,
}

/// An error at a place: its message is exactly what it says, the place only tells the language server where.
#[derive(Debug)]
pub struct Located {
    pub place: Place,
    /// The diagnostic code the language server gives it.
    pub code: &'static str,
    pub message: String,
    /// Other places the message names (the other of two duplicates), with what each is.
    pub related: Vec<(Place, String)>,
}

/// A table or key an error passes through on its way out: its keys, its file, and the context text it adds.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    place: Place,
    text: Option<String>,
}

/// Errors of a check that goes on past the first: the first is the check's error; under the language server the
/// others are reported too (outside it the check stops at the first).
pub struct Errors {
    first: Option<anyhow::Error>,
    file: Option<PathBuf>,
    /// How many errors the session had before the first.
    before: usize,
}

/// Where the values of a target came from: which table set each key and list item, as targets merge [defaults],
/// presets, their own table and when blocks.
#[derive(Debug, Clone, Default)]
pub struct Origins {
    file: Option<PathBuf>,
    /// The target's own table, for what no table set (the layout convention).
    table: Vec<Part>,
    /// (the key under its table, the value, the table)
    entries: Vec<(Vec<Part>, String, Vec<Part>)>,
}

/// `bail!` with the place the error is about: `bail_at!(Place::key("kind").value(text), "invalid-value", "...")`.
#[macro_export]
macro_rules! bail_at {
    ($place:expr, $code:expr, $($message:tt)+) => {
        return Err($crate::located::Located::new($place, $code, format!($($message)+)).into())
    };
}

/// A key path from keys: `keys(&["target", "app"])`.
pub fn keys(names: &[&str]) -> Vec<Part> {
    names.iter().map(|name| Part::Key(name.to_string())).collect()
}

/// The text of a scalar TOML value as the manifest writes it (a string's content), for marking it.
pub fn value_text(value: &toml::Value) -> Option<String> {
    match value {
        toml::Value::String(text) => Some(text.clone()),
        toml::Value::Integer(number) => Some(number.to_string()),
        toml::Value::Float(number) => Some(number.to_string()),
        toml::Value::Boolean(flag) => Some(flag.to_string()),
        _ => None,
    }
}

impl Place {
    /// A key of the table the error is in.
    pub fn key(key: &str) -> Place {
        Place::keys(&[key])
    }

    pub fn keys(names: &[&str]) -> Place {
        Place {
            path: keys(names),
            ..Default::default()
        }
    }

    /// A key path from the top of the file.
    pub fn absolute(path: Vec<Part>) -> Place {
        Place {
            path,
            absolute: true,
            ..Default::default()
        }
    }

    pub fn value(mut self, value: impl Into<String>) -> Place {
        self.value = Some(value.into());
        self
    }

    pub fn file(mut self, file: Option<&Path>) -> Place {
        self.file = file.map(Path::to_path_buf);
        self.absolute = true;
        self
    }

    /// One more key at the end.
    pub fn join(mut self, key: &str) -> Place {
        self.path.push(Part::Key(key.to_string()));
        self
    }
}

impl Located {
    pub fn new(place: Place, code: &'static str, message: String) -> Located {
        Located {
            place,
            code,
            message,
            related: Vec::new(),
        }
    }

    /// Another place the message names.
    pub fn related(mut self, place: Place, label: impl Into<String>) -> Located {
        self.related.push((place, label.into()));
        self
    }
}

impl std::fmt::Display for Located {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Located {}

impl Scope {
    /// `[target.<name>]`, saying "in [target.<name>]".
    pub fn target(name: &str) -> Scope {
        Scope::keys(&["target", name]).text(format!("in [target.{name}]"))
    }

    pub fn preset(name: &str) -> Scope {
        Scope::keys(&["preset", name]).text(format!("in [preset.{name}]"))
    }

    pub fn defaults() -> Scope {
        Scope::keys(&["defaults"]).text("in [defaults]")
    }

    pub fn when(condition: &str) -> Scope {
        Scope::keys(&["when", condition]).text(format!("in when.\"{condition}\""))
    }

    /// Keys of the table the error is in, saying nothing.
    pub fn keys(names: &[&str]) -> Scope {
        Scope {
            place: Place::keys(names),
            text: None,
        }
    }

    pub fn key(key: &str) -> Scope {
        Scope::keys(&[key])
    }

    /// An item of an array of tables (`[[test]]`, `[[write]]`), saying nothing.
    pub fn item(key: &str, index: usize) -> Scope {
        Scope {
            place: Place {
                path: vec![Part::Key(key.to_string()), Part::Index(index)],
                ..Default::default()
            },
            text: None,
        }
    }

    /// A whole place: the error is there unless it knows better.
    pub fn place(place: Place) -> Scope {
        Scope { place, text: None }
    }

    /// The file the error is in (its path complete), saying nothing.
    pub fn file(file: &Path) -> Scope {
        Scope::place(Place::default().file(Some(file)))
    }

    /// The context text this scope adds.
    pub fn text(mut self, text: impl Into<String>) -> Scope {
        self.text = Some(text.into());
        self
    }

    /// The error with this scope's keys, file and value added to its place, and its context text.
    pub fn wrap(&self, mut error: anyhow::Error) -> anyhow::Error {
        if let Some(located) = error.downcast_mut::<Located>() {
            self.apply(&mut located.place);
        }

        match &self.text {
            Some(text) => error.context(text.clone()),
            None => error,
        }
    }

    fn apply(&self, place: &mut Place) {
        if !place.absolute {
            let mut path = self.place.path.clone();
            path.append(&mut place.path);
            place.path = path;
            place.absolute = self.place.absolute;
        }
        if place.file.is_none() {
            place.file = self.place.file.clone();
        }
        if place.value.is_none() {
            place.value = self.place.value.clone();
        }
    }
}

/// `.at(scope)` on a result: its error passes through the scope.
pub trait At<T> {
    fn at(self, scope: Scope) -> anyhow::Result<T>;

    /// The error at `place`: one that does not say where it is (a library's) gets it and `code`; a placed one that
    /// does not know its table gets it, keeping its own value when `place` has none.
    fn located(self, place: Place, code: &'static str) -> anyhow::Result<T>;
}

impl<T> At<T> for anyhow::Result<T> {
    fn at(self, scope: Scope) -> anyhow::Result<T> {
        self.map_err(|error| scope.wrap(error))
    }

    fn located(self, place: Place, code: &'static str) -> anyhow::Result<T> {
        self.map_err(|mut error| {
            let Some(located) = error.downcast_mut::<Located>() else {
                return Located::new(place, code, format!("{error:#}")).into();
            };
            if !located.place.absolute {
                let value = located.place.value.take();
                located.place = place;
                located.place.value = located.place.value.take().or(value);
            }
            error
        })
    }
}

impl Errors {
    /// `file`: the file the errors are in (set where they do not say).
    pub fn new(file: Option<&Path>) -> Errors {
        Errors {
            first: None,
            file: file.map(Path::to_path_buf),
            before: 0,
        }
    }

    /// Keeps a check's error.
    pub fn add(&mut self, result: anyhow::Result<()>) {
        let Err(mut error) = result else { return };

        if self.first.is_none() {
            self.first = Some(error);
            self.before = crate::lsp::session::error_count();
            return;
        }
        if let Some(file) = &self.file {
            error = Scope::file(file).wrap(error);
        }
        crate::lsp::session::add_error(error);
    }

    /// Whether the check stops here: after an error, outside the language server.
    pub fn stop(&self) -> bool {
        self.first.is_some() && !crate::lsp::session::active()
    }

    pub fn finish(self) -> anyhow::Result<()> {
        match self.first {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Under the language server every error said, the first before the others, and the work goes on; else the first.
    pub fn say(self) -> anyhow::Result<()> {
        if !crate::lsp::session::active() {
            return self.finish();
        }

        if let Some(error) = self.first {
            crate::lsp::session::insert_error(self.before, error);
        }
        Ok(())
    }
}

impl Origins {
    /// `table`: the target's own table.
    pub fn new(file: Option<&Path>, table: Vec<Part>) -> Origins {
        Origins {
            file: file.map(Path::to_path_buf),
            table,
            entries: Vec::new(),
        }
    }

    /// The values `table` sets for `key` (a key under it, `public.deps` as ["public", "deps"]).
    pub fn note<'a>(&mut self, table: &[Part], key: &[&str], values: impl IntoIterator<Item = &'a String>) {
        for value in values {
            self.entries.push((keys(key), value.clone(), table.to_vec()));
        }
    }

    /// Where `value` of one of `keys` was written: the last table that set it, else the target's own table.
    pub fn place(&self, keys: &[&[&str]], value: &str) -> Place {
        self.find(keys, |written| written == value).unwrap_or_else(|| Place {
            file: self.file.clone(),
            path: [self.table.clone(), crate::located::keys(keys.first().copied().unwrap_or_default())].concat(),
            value: Some(value.to_string()),
            absolute: true,
        })
    }

    /// Where the last value of one of `keys` that `wanted` takes was written, with that value.
    pub fn find(&self, keys: &[&[&str]], wanted: impl Fn(&str) -> bool) -> Option<Place> {
        let (key, value, table) = self
            .entries
            .iter()
            .rev()
            .find(|(key, written, _)| wanted(written) && keys.iter().any(|candidate| crate::located::keys(candidate) == *key))?;

        Some(Place {
            file: self.file.clone(),
            path: [table.clone(), key.clone()].concat(),
            value: Some(value.clone()),
            absolute: true,
        })
    }

    /// Where `key` was set (its last value's table), without a value: for a message about the key as a whole.
    pub fn key(&self, key: &[&str]) -> Place {
        let wanted = keys(key);
        let table = self
            .entries
            .iter()
            .rev()
            .find(|(written_key, _, _)| *written_key == wanted)
            .map_or(self.table.clone(), |(_, _, table)| table.clone());

        Place {
            file: self.file.clone(),
            path: [table, wanted].concat(),
            value: None,
            absolute: true,
        }
    }

    /// A scope placing errors about `key` (which say the key themselves) in the table that set it.
    pub fn scope(&self, key: &[&str]) -> Scope {
        let mut place = self.key(key);
        place.path.truncate(place.path.len() - key.len());
        Scope::place(place)
    }

    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    fn leaf() -> anyhow::Result<()> {
        bail_at!(Place::key("kind").value("exe2"), "invalid-value", "kind = \"{}\": expected exe", "exe2")
    }

    #[test]
    fn scopes_add_keys_and_the_same_text() {
        let error = leaf()
            .at(Scope::when("windows"))
            .context("plain context")
            .at(Scope::target("app"))
            .at(Scope::file(Path::new("C:/p/anvil.toml")).text("C:/p/anvil.toml"))
            .unwrap_err();

        assert_eq!(
            format!("{error:#}"),
            "C:/p/anvil.toml: in [target.app]: plain context: in when.\"windows\": kind = \"exe2\": expected exe"
        );
        let located = error.downcast_ref::<Located>().unwrap();
        assert_eq!(located.place.path, keys(&["target", "app", "when", "windows", "kind"]));
        assert_eq!(located.place.value.as_deref(), Some("exe2"));
        assert_eq!(located.place.file.as_deref(), Some(Path::new("C:/p/anvil.toml")));
        assert_eq!(located.code, "invalid-value");
    }

    #[test]
    fn absolute_places_keep_their_keys() {
        let error: anyhow::Result<()> = Err(Located::new(Place::absolute(keys(&["defaults", "std"])), "invalid-value", "std".into()).into());
        let error = error.at(Scope::target("app")).unwrap_err();
        assert_eq!(error.downcast_ref::<Located>().unwrap().place.path, keys(&["defaults", "std"]));

        let plain: anyhow::Result<()> = Err(anyhow::anyhow!("bad pattern")).context("outer");
        let error = plain.located(Place::key("sources").value("x"), "invalid-value").unwrap_err();
        assert_eq!(error.to_string(), "outer: bad pattern");
        assert_eq!(error.downcast_ref::<Located>().unwrap().place.path, keys(&["sources"]));
    }

    #[test]
    fn origins_name_the_table_that_set_a_value() {
        let mut origins = Origins::new(None, keys(&["target", "app"]));
        origins.note(&keys(&["defaults"]), &["deps"], &["fmt".to_string()]);
        origins.note(&keys(&["target", "app", "when", "windows"]), &["public", "deps"], &["zlib".to_string()]);

        assert_eq!(origins.place(&[&["deps"]], "fmt").path, keys(&["defaults", "deps"]));
        assert_eq!(
            origins.place(&[&["deps"], &["public", "deps"]], "zlib").path,
            keys(&["target", "app", "when", "windows", "public", "deps"])
        );
        assert_eq!(origins.place(&[&["deps"]], "other").path, keys(&["target", "app", "deps"]));
    }
}
