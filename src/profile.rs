//! Build profiles: the built-in `debug` and `release`, and user profiles that inherit from them.

use crate::bail_at;
use crate::located::{self, At, Place, Scope};
use crate::manifest::ProfileSpec;
use anyhow::Result;
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
    /// A [profile] table asks for the debug runtime itself (debug-runtime = true), not only the built-in `debug`.
    pub debug_runtime_requested: bool,
    pub static_runtime: bool,
    pub assertions: bool,

    // Extra settings for every target
    pub defines: Vec<String>,
    pub flags: Vec<String>,
    pub link_flags: Vec<String>,

    /// The machine to build for, when not this one.
    pub target: Option<String>,
}

// What `sanitize` may name: the sanitizers and the checks of -fsanitize in clang, gcc and MSVC
pub const SANITIZERS: &[&str] = &[
    "address",
    "hwaddress",
    "kernel-address",
    "kernel-hwaddress",
    "memory",
    "kernel-memory",
    "thread",
    "leak",
    "undefined",
    "dataflow",
    "cfi",
    "cfi-cast-strict",
    "cfi-derived-cast",
    "cfi-unrelated-cast",
    "cfi-nvcall",
    "cfi-vcall",
    "cfi-icall",
    "cfi-mfcall",
    "kcfi",
    "safe-stack",
    "shadow-call-stack",
    "fuzzer",
    "fuzzer-no-link",
    "scudo",
    "memtag",
    "memtag-stack",
    "memtag-heap",
    "memtag-globals",
    "realtime",
    "type",
    "numerical",
    "function",
    "vptr",
    "alignment",
    "bool",
    "builtin",
    "bounds",
    "bounds-strict",
    "array-bounds",
    "local-bounds",
    "enum",
    "float-cast-overflow",
    "float-divide-by-zero",
    "implicit-conversion",
    "implicit-integer-truncation",
    "implicit-unsigned-integer-truncation",
    "implicit-signed-integer-truncation",
    "implicit-integer-sign-change",
    "implicit-integer-arithmetic-value-change",
    "implicit-bitfield-conversion",
    "integer",
    "integer-divide-by-zero",
    "nonnull-attribute",
    "null",
    "nullability",
    "nullability-arg",
    "nullability-assign",
    "nullability-return",
    "objc-cast",
    "object-size",
    "pointer-overflow",
    "pointer-compare",
    "pointer-subtract",
    "return",
    "returns-nonnull-attribute",
    "shift",
    "shift-base",
    "shift-exponent",
    "signed-integer-overflow",
    "unreachable",
    "unsigned-integer-overflow",
    "unsigned-shift-base",
    "vla-bound",
];

impl Opt {
    pub fn parse(value: &toml::Value) -> Result<Opt> {
        let text = match value {
            toml::Value::Integer(level) => level.to_string(),
            toml::Value::String(text) => text.clone(),
            _ => bail_at!(
                Place::key("opt").value(located::value_text(value).unwrap_or_default()),
                "invalid-value",
                "opt: expected 0, 1, 2, 3, \"s\" or \"z\""
            ),
        };

        Ok(match text.as_str() {
            "0" => Opt::None,
            "1" => Opt::Some,
            "2" => Opt::Fast,
            "3" => Opt::Fastest,
            "s" => Opt::Size,
            "z" => Opt::MinSize,
            other => bail_at!(
                Place::key("opt").value(other),
                "invalid-value",
                "opt = \"{other}\": expected 0, 1, 2, 3, \"s\" or \"z\""
            ),
        })
    }
}

impl Profile {
    /// The named profile, with `[profile.*]` tables applied over the built-ins along its inheritance chain.
    pub fn resolve(name: &str, specs: &IndexMap<String, ProfileSpec>) -> Result<Profile> {
        // Walk up the chain: the named profile first, a built-in last
        let mut chain = vec![name.to_string()];
        let mut current = name.to_string();
        let inherits = |profile: &str| Place::absolute(located::keys(&["profile", profile, "inherits"]));

        loop {
            let parent = match specs.get(&current).and_then(|spec| spec.inherits.clone()) {
                Some(parent) => parent,
                None if Profile::builtin(&current).is_some() => break,
                None if specs.contains_key(&current) => "release".to_string(),
                None => {
                    let known: Vec<&str> = ["debug", "release"].into_iter().chain(specs.keys().map(|key| key.as_str())).collect();
                    let hint = crate::util::did_you_mean(&current, known.iter().copied());
                    let place = match chain.iter().rev().nth(1) {
                        Some(child) => inherits(child).value(&current),
                        None => Place::default(),
                    };
                    bail_at!(
                        place,
                        "unknown-profile",
                        "unknown profile `{current}`{hint}; known profiles: {}",
                        known.join(", ")
                    );
                }
            };

            if chain.contains(&parent) {
                bail_at!(
                    inherits(name).value(chain.get(1).unwrap_or(&parent)),
                    "profile-cycle",
                    "profile `{name}` inherits from itself (through {})",
                    chain.join(" -> ")
                );
            }

            chain.push(parent.clone());
            current = parent;
        }

        // Apply from the root built-in down to the requested profile
        let mut profile = Profile::builtin(chain.last().unwrap()).unwrap();
        for profile_name in chain.iter().rev() {
            if let Some(spec) = specs.get(profile_name) {
                profile.apply(spec).at(Scope::keys(&["profile", profile_name]))?;
            }
        }

        profile.name = name.to_string();
        profile.chain = chain;

        Ok(profile)
    }

    /// Checks every `[profile]` table, used or not: what it inherits, its values, the machine it names.
    pub fn check_all(specs: &IndexMap<String, ProfileSpec>) -> Result<()> {
        for (name, spec) in specs {
            let context = || Scope::default().text(format!("in [profile.{name}]"));
            Profile::resolve(name, specs).at(context())?;
            if let Some(target) = &spec.target {
                crate::platform::Platform::parse(target)
                    .at(Scope::keys(&["profile", name]))
                    .at(context())?;
            }
        }

        Ok(())
    }

    /// Fills `{env:NAME}` in the definitions and flags, for the profile a build uses (one it does not use may name
    /// variables this machine does not set). An error names the `[profile]` table that wrote the value.
    pub fn expand_environment(&mut self, specs: &IndexMap<String, ProfileSpec>) -> Result<()> {
        let chain = self.chain.clone();
        let expand = |key: &str, list: &mut Vec<String>, written: fn(&ProfileSpec) -> &Option<Vec<String>>| -> Result<()> {
            for text in list.iter_mut() {
                let owner = chain.iter().find(|name| {
                    specs
                        .get(*name)
                        .is_some_and(|spec| written(spec).iter().flatten().any(|value| value == text))
                });
                let owner = owner.map_or("", String::as_str);
                let place = Place::absolute(located::keys(&["profile", owner, key])).value(text.as_str());
                *text = crate::util::expand_env_word(text)
                    .located(place, "invalid-value")
                    .at(Scope::default().text(format!("in [profile.{owner}]")))?;
            }
            Ok(())
        };

        expand("defines", &mut self.defines, |spec| &spec.defines)?;
        expand("flags", &mut self.flags, |spec| &spec.flags)?;
        expand("link-flags", &mut self.link_flags, |spec| &spec.link_flags)?;

        Ok(())
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
            debug_runtime_requested: false,
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
        self.debug_runtime_requested = spec.debug_runtime.unwrap_or(self.debug_runtime_requested);
        self.static_runtime = spec.static_runtime.unwrap_or(self.static_runtime);
        self.assertions = spec.assertions.unwrap_or(self.assertions);
        if spec.target.is_some() {
            self.target = spec.target.clone();
        }

        // Lists add up along the chain
        if let Some(sanitize) = &spec.sanitize {
            if let Some(sanitizer) = sanitize.iter().find(|sanitizer| !SANITIZERS.contains(&sanitizer.as_str())) {
                let hint = crate::util::did_you_mean(sanitizer, SANITIZERS.iter().copied());
                bail_at!(
                    Place::key("sanitize").value(sanitizer),
                    "invalid-value",
                    "sanitize: unknown sanitizer `{sanitizer}`{hint}; common ones are address, undefined, thread, memory and leak"
                );
            }
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
