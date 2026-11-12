//! Condition expressions: `windows`, `!windows`, `msvc & debug`, `linux | macos`, `(a | b) & !c`.
//! Used by `if = "..."` keys and `[when.<cond>]` tables.

use crate::platform::{Cpu, Os, Platform};
use crate::toolchain::Family;
use anyhow::{bail, Result};
use indexmap::IndexMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Cond {
    Atom(String),
    Not(Box<Cond>),
    And(Box<Cond>, Box<Cond>),
    Or(Box<Cond>, Box<Cond>),
}

/// What conditions are evaluated against.
#[derive(Debug, Clone)]
pub struct CondContext {
    pub family: Family,

    /// The machine the build is for (not the one it runs on, in a cross build).
    pub platform: Platform,

    /// The profile and every profile it inherits from (`shipping` -> `release`).
    pub profiles: Vec<String>,

    /// Every profile name declared anywhere, so `when.shipping` is false (not unknown) under debug.
    pub all_profiles: Vec<String>,

    pub options: IndexMap<String, bool>,
}

struct Parser<'a> {
    tokens: &'a [String],
    position: usize,
    source: &'a str,
}

// The words conditions know without a profile or an option: no profile or option may take one of them
const BUILTIN_WORDS: &[&str] = &[
    "true", "false", "windows", "win32", "linux", "macos", "osx", "apple", "unix", "x64", "x86_64", "amd64", "x86", "i686", "arm64", "aarch64",
    "arm", "cross", "msvc", "gnu", "cl", "clang-cl", "clang", "gcc", "mingw",
];

/// Profile and option names must differ from the words conditions already know, and from each other: otherwise
/// `when.<name>` would test something other than what its author meant (an option `x64` that is never true).
pub fn check_names(profiles: &[String], options: &[String]) -> Result<()> {
    for profile in profiles {
        if BUILTIN_WORDS.contains(&profile.as_str()) {
            bail!("profile `{profile}`: `{profile}` is already a condition word (platform, processor or compiler); rename the profile");
        }
    }

    for option in options {
        if BUILTIN_WORDS.contains(&option.as_str()) {
            bail!("option `{option}`: `{option}` is already a condition word (platform, processor or compiler); rename the option");
        }
        let is_profile = matches!(option.as_str(), "debug" | "release") || profiles.contains(option);
        if is_profile {
            bail!("option `{option}` has the name of a profile, so `when.{option}` could not tell them apart; rename the option");
        }
    }

    Ok(())
}

pub fn parse(source: &str) -> Result<Cond> {
    let tokens = tokenize(source)?;
    let mut parser = Parser {
        tokens: &tokens,
        position: 0,
        source,
    };

    let condition = parser.parse_or()?;
    if parser.position != tokens.len() {
        bail!("condition \"{source}\": unexpected `{}`", tokens[parser.position]);
    }

    Ok(condition)
}

impl Cond {
    /// Evaluates with `lookup` answering each name; None means the name is unknown, which is an error so that a typo
    /// never passes silently as "false".
    pub fn eval(&self, lookup: &dyn Fn(&str) -> Option<bool>) -> Result<bool> {
        Ok(match self {
            Cond::Atom(name) => match lookup(name) {
                Some(value) => value,
                None => bail!(
                    "unknown condition `{name}`. Known: windows, linux, macos, unix, x64, x86, arm64, arm, cross, msvc, gnu, \
                     cl, clang-cl, clang, gcc, mingw, debug, release, any profile name, any [options] name, true, false"
                ),
            },
            Cond::Not(inner) => !inner.eval(lookup)?,
            Cond::And(left, right) => left.eval(lookup)? && right.eval(lookup)?,
            Cond::Or(left, right) => left.eval(lookup)? || right.eval(lookup)?,
        })
    }
}

impl CondContext {
    pub fn test(&self, source: &str) -> Result<bool> {
        parse(source)?.eval(&|name| self.lookup(name))
    }

    pub fn lookup(&self, name: &str) -> Option<bool> {
        let family = self.family;
        let (os, cpu) = (self.platform.os, self.platform.cpu);

        let value = match name {
            "true" => true,
            "false" => false,

            // The platform built for
            "windows" | "win32" => os == Os::Windows,
            "linux" => os == Os::Linux,
            "macos" | "osx" | "apple" => os == Os::Macos,
            "unix" => os != Os::Windows,
            "x64" | "x86_64" | "amd64" => cpu == Cpu::X64,
            "x86" | "i686" => cpu == Cpu::X86,
            "arm64" | "aarch64" => cpu == Cpu::Arm64,
            "arm" => cpu == Cpu::Arm,
            "cross" => self.platform.is_cross(),

            // Compiler: msvc and gnu are the command-line styles, the rest are compilers
            "msvc" => matches!(family, Family::Msvc | Family::ClangCl),
            "gnu" => matches!(family, Family::Gcc | Family::Clang),
            "cl" => family == Family::Msvc,
            "clang-cl" => family == Family::ClangCl,
            "clang" => matches!(family, Family::Clang | Family::ClangCl),
            "gcc" => family == Family::Gcc,
            "mingw" => os == Os::Windows && family == Family::Gcc,

            _ => return self.lookup_profile_or_option(name),
        };

        Some(value)
    }

    fn lookup_profile_or_option(&self, name: &str) -> Option<bool> {
        if self.profiles.iter().any(|profile| profile == name) {
            return Some(true);
        }

        if let Some(value) = self.options.get(name) {
            return Some(*value);
        }

        // A profile outside the active chain is simply false
        let known_profile = matches!(name, "debug" | "release") || self.all_profiles.iter().any(|profile| profile == name);
        known_profile.then_some(false)
    }
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a str> {
        self.tokens.get(self.position).map(|token| token.as_str())
    }

    fn parse_or(&mut self) -> Result<Cond> {
        let mut left = self.parse_and()?;

        while self.peek() == Some("|") {
            self.position += 1;
            left = Cond::Or(Box::new(left), Box::new(self.parse_and()?));
        }

        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Cond> {
        let mut left = self.parse_unary()?;

        while self.peek() == Some("&") {
            self.position += 1;
            left = Cond::And(Box::new(left), Box::new(self.parse_unary()?));
        }

        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Cond> {
        match self.peek() {
            Some("!") => {
                self.position += 1;
                Ok(Cond::Not(Box::new(self.parse_unary()?)))
            }
            Some("(") => {
                self.position += 1;
                let inner = self.parse_or()?;

                if self.peek() != Some(")") {
                    bail!("condition \"{}\": missing `)`", self.source);
                }
                self.position += 1;

                Ok(inner)
            }
            Some(word) if !"&|)".contains(word) => {
                self.position += 1;
                Ok(Cond::Atom(word.to_string()))
            }
            Some(word) => bail!("condition \"{}\": unexpected `{word}`", self.source),
            None => bail!("condition \"{}\": ends too early", self.source),
        }
    }
}

fn is_word_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || "_-.+".contains(character)
}

fn tokenize(source: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut characters = source.chars().peekable();

    while let Some(&character) = characters.peek() {
        if character.is_whitespace() {
            characters.next();
            continue;
        }

        // Operators: `&&` and `||` read as `&` and `|`; `,` is `&` (vcpkg's spelling)
        if "!&|(),".contains(character) {
            characters.next();
            if (character == '&' || character == '|') && characters.peek() == Some(&character) {
                characters.next();
            }

            let operator = if character == ',' { "&".to_string() } else { character.to_string() };
            tokens.push(operator);
            continue;
        }

        if !is_word_character(character) {
            bail!("condition \"{source}\": unexpected character `{character}`");
        }

        // A name, or one of the word operators
        let mut word = String::new();
        while let Some(&character) = characters.peek() {
            if !is_word_character(character) {
                break;
            }
            word.push(character);
            characters.next();
        }

        let token = match word.as_str() {
            "and" => "&".to_string(),
            "or" => "|".to_string(),
            "not" => "!".to_string(),
            _ => word,
        };
        tokens.push(token);
    }

    if tokens.is_empty() {
        bail!("empty condition");
    }

    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_words_are_reserved() {
        let context = CondContext {
            family: Family::Msvc,
            platform: Platform::host(),
            profiles: Vec::new(),
            all_profiles: Vec::new(),
            options: IndexMap::new(),
        };
        for word in BUILTIN_WORDS {
            assert!(context.lookup(word).is_some(), "{word} is not answered");
        }

        assert!(check_names(&["shipping".into()], &["tracy".into()]).is_ok());
        assert!(check_names(&[], &["x64".into()]).is_err());
        assert!(check_names(&[], &["debug".into()]).is_err());
        assert!(check_names(&["shipping".into()], &["shipping".into()]).is_err());
        assert!(check_names(&["msvc".into()], &[]).is_err());
    }

    fn evaluate(source: &str) -> bool {
        let lookup = |name: &str| match name {
            "a" => Some(true),
            "b" => Some(false),
            _ => None,
        };
        parse(source).unwrap().eval(&lookup).unwrap()
    }

    #[test]
    fn precedence() {
        assert!(evaluate("a | b & b"));
        assert!(!evaluate("(a | b) & b"));
        assert!(evaluate("!b"));
        assert!(evaluate("a && !b"));
        assert!(evaluate("not b and a"));
    }

    #[test]
    fn unknown_is_error() {
        assert!(parse("zz").unwrap().eval(&|_| None).is_err());
        assert!(parse("a &").is_err());
    }
}
