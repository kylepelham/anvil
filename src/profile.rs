//! Build profiles: the built-in `debug` and `release`, and user profiles that inherit from them.

use crate::manifest::ProfileSpec;
use anyhow::{bail, Result};
use indexmap::IndexMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opt {
    None,
    Some,
    Fast,
    Fastest,
    Size,
    MinSize,
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,

    /// This profile's name, then the names of the profiles it inherits from.
    pub chain: Vec<String>,

    // Code generation
    pub opt: Opt,
    pub debug: bool,
    pub lto: bool,
    pub sanitize: Vec<String>,

    // Runtime
    pub debug_runtime: bool,
    pub static_runtime: bool,
    pub assertions: bool,

    // Extra settings for every target
    pub defines: Vec<String>,
    pub flags: Vec<String>,
    pub link_flags: Vec<String>,

    /// The machine to build for, when not this one.
    pub target: Option<String>,
}

// Profiles nested deeper than this are certainly an inheritance cycle
const MAX_INHERITANCE_DEPTH: usize = 32;

impl Opt {
    pub fn parse(value: &toml::Value) -> Result<Opt> {
        let text = match value {
            toml::Value::Integer(level) => level.to_string(),
            toml::Value::String(text) => text.clone(),
            _ => bail!("opt: expected 0, 1, 2, 3, \"s\" or \"z\""),
        };

        Ok(match text.as_str() {
            "0" => Opt::None,
            "1" => Opt::Some,
            "2" => Opt::Fast,
            "3" => Opt::Fastest,
            "s" => Opt::Size,
            "z" => Opt::MinSize,
            other => bail!("opt = \"{other}\": expected 0, 1, 2, 3, \"s\" or \"z\""),
        })
    }
}

impl Profile {
    /// The named profile, with `[profile.*]` tables applied over the built-ins along its inheritance chain.
    pub fn resolve(name: &str, specs: &IndexMap<String, ProfileSpec>) -> Result<Profile> {
        // Walk up the chain: the named profile first, a built-in last
        let mut chain = vec![name.to_string()];
        let mut current = name.to_string();

        loop {
            let parent = match specs.get(&current).and_then(|spec| spec.inherits.clone()) {
                Some(parent) => parent,
                None if Profile::builtin(&current).is_some() => break,
                None if specs.contains_key(&current) => "release".to_string(),
                None => {
                    let known: Vec<&str> = ["debug", "release"].into_iter().chain(specs.keys().map(|key| key.as_str())).collect();
                    let hint = crate::util::suggest(&current, known.iter().copied())
                        .map(|close| format!(" (did you mean `{close}`?)"))
                        .unwrap_or_default();
                    bail!("unknown profile `{current}`{hint}; known profiles: {}", known.join(", "));
                }
            };

            if chain.contains(&parent) || chain.len() > MAX_INHERITANCE_DEPTH {
                bail!("profile `{name}` inherits from itself (through {})", chain.join(" -> "));
            }

            chain.push(parent.clone());
            current = parent;
        }

        // Apply from the root built-in down to the requested profile
        let mut profile = Profile::builtin(chain.last().unwrap()).unwrap();
        for profile_name in chain.iter().rev() {
            if let Some(spec) = specs.get(profile_name) {
                profile.apply(spec)?;
            }
        }

        profile.name = name.to_string();
        profile.chain = chain;

        Ok(profile)
    }

    fn builtin(name: &str) -> Option<Profile> {
        let debug = name == "debug";
        if !debug && name != "release" {
            return None;
        }

        Some(Profile {
            name: name.to_string(),
            chain: vec![name.to_string()],
            opt: if debug { Opt::None } else { Opt::Fastest },
            debug,
            lto: false,
            sanitize: Vec::new(),
            debug_runtime: debug,
            static_runtime: false,
            assertions: debug,
            defines: Vec::new(),
            flags: Vec::new(),
            link_flags: Vec::new(),
            target: None,
        })
    }

    fn apply(&mut self, spec: &ProfileSpec) -> Result<()> {
        if let Some(opt) = &spec.opt {
            self.opt = Opt::parse(opt)?;
        }

        // Plain switches
        self.debug = spec.debug.unwrap_or(self.debug);
        self.lto = spec.lto.unwrap_or(self.lto);
        self.debug_runtime = spec.debug_runtime.unwrap_or(self.debug_runtime);
        self.static_runtime = spec.static_runtime.unwrap_or(self.static_runtime);
        self.assertions = spec.assertions.unwrap_or(self.assertions);
        if spec.target.is_some() {
            self.target = spec.target.clone();
        }

        // Lists add up along the chain
        if let Some(sanitize) = &spec.sanitize {
            self.sanitize = sanitize.clone();
        }
        self.defines.extend(spec.defines.iter().flatten().cloned());
        self.flags.extend(spec.flags.iter().flatten().cloned());
        self.link_flags.extend(spec.link_flags.iter().flatten().cloned());

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherits() {
        let mut specs = IndexMap::new();
        specs.insert(
            "shipping".to_string(),
            ProfileSpec {
                inherits: Some("release".into()),
                lto: Some(true),
                ..Default::default()
            },
        );

        let shipping = Profile::resolve("shipping", &specs).unwrap();
        assert!(shipping.lto);
        assert_eq!(shipping.opt, Opt::Fastest);
        assert_eq!(shipping.chain, vec!["shipping", "release"]);
    }

    #[test]
    fn rejects_cycles_and_unknowns() {
        let mut specs = IndexMap::new();
        specs.insert(
            "a".to_string(),
            ProfileSpec {
                inherits: Some("b".into()),
                ..Default::default()
            },
        );
        specs.insert(
            "b".to_string(),
            ProfileSpec {
                inherits: Some("a".into()),
                ..Default::default()
            },
        );

        assert!(Profile::resolve("a", &specs).is_err());
        assert!(Profile::resolve("relase", &specs)
            .unwrap_err()
            .to_string()
            .contains("did you mean `release`"));
    }
}
