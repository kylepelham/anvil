//! `{hash:...}` in a target's definitions and flags: 16 hex digits that change whenever what it lists changes. It is
//! the cache key CMake projects compute at configure time with file(SHA256), but current on every build.
//!
//! Items, separated by commas:
//! - `compiler`: the compiler, its version and the machine it builds for;
//! - `flags`: the target's compile flags (its definitions, flags, standard, optimization, ...) without include folders
//!   and without the hashes themselves;
//! - a file, or a glob (every match: its path and contents), relative to the target's folder.
//!
//! A file that cannot be read, or a glob that matches nothing, is an error.

use crate::targets;
use crate::util::{self, slash};
use anyhow::{bail, Context, Result};
use std::path::Path;

pub const HASH_START: &str = "{hash:";

/// What a target's hashes can take in.
pub struct HashInputs<'a> {
    pub compiler: &'a str,
    /// The target's compile flags, made when an item asks for them.
    pub flags: &'a dyn Fn() -> String,
    /// The target's folder: relative items start here.
    pub dir: &'a Path,
    /// The package root: file paths are hashed relative to it, so a moved checkout keeps its hashes.
    pub root: &'a Path,
    pub out_root: &'a Path,
}

/// The text with every `{hash:...}` replaced by its digest.
pub fn expand(text: &str, inputs: &HashInputs) -> Result<String> {
    let mut result = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(HASH_START) {
        let after = &rest[start + HASH_START.len()..];
        let Some(end) = closing_brace(after) else {
            bail!("`{text}`: `{HASH_START}` has no closing `}}`");
        };

        let items: Vec<&str> = split_items(&after[..end])
            .into_iter()
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .collect();
        if items.is_empty() {
            bail!("`{text}`: `{HASH_START}}}` lists nothing to hash");
        }
        let mut hashed = Vec::new();
        for item in items {
            add_item(item, inputs, &mut hashed)?;
            hashed.push(0);
        }

        result.push_str(&rest[..start]);
        result.push_str(&format!("{:016x}", util::hash_bytes(&hashed)));
        rest = &after[end + 1..];
    }

    result.push_str(rest);
    Ok(result)
}

fn add_item(item: &str, inputs: &HashInputs, hashed: &mut Vec<u8>) -> Result<()> {
    match item {
        "compiler" => hashed.extend_from_slice(inputs.compiler.as_bytes()),
        "flags" => hashed.extend_from_slice((inputs.flags)().as_bytes()),
        pattern if pattern.contains(['*', '?', '[', '{']) => {
            let mut files = targets::walk_glob(inputs.dir, pattern, inputs.out_root)?;
            files.sort_by_cached_key(|file| util::path_key(file));
            if files.is_empty() {
                bail!("{HASH_START}...}}: `{pattern}` matches no files");
            }
            for file in files {
                add_file(&file, inputs, hashed)?;
            }
        }
        file => add_file(&util::absolute(inputs.dir, Path::new(file)), inputs, hashed)?,
    }
    Ok(())
}

/// A file's path (relative to the package) and contents.
fn add_file(file: &Path, inputs: &HashInputs, hashed: &mut Vec<u8>) -> Result<()> {
    let contents = std::fs::read(file).with_context(|| format!("{HASH_START}...}}: cannot read {}", slash(file)))?;
    hashed.extend_from_slice(util::rel(file, inputs.root).as_bytes());
    hashed.push(0);
    hashed.extend_from_slice(&contents);
    Ok(())
}

/// Where the `}` that closes `{hash:` is, past the `{a,b}` groups of globs inside it.
fn closing_brace(text: &str) -> Option<usize> {
    let mut depth = 0;

    for (position, character) in text.char_indices() {
        match character {
            '{' => depth += 1,
            '}' if depth == 0 => return Some(position),
            '}' => depth -= 1,
            _ => {}
        }
    }

    None
}

/// The items between `{hash:` and its `}`, split at the commas outside `{a,b}` groups.
fn split_items(text: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut depth = 0;
    let mut start = 0;

    for (position, character) in text.char_indices() {
        match character {
            '{' => depth += 1,
            '}' => depth -= 1,
            ',' if depth == 0 => {
                items.push(&text[start..position]);
                start = position + 1;
            }
            _ => {}
        }
    }

    items.push(&text[start..]);
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_files_globs_and_flags() {
        let dir = std::env::temp_dir().join(format!("anvil_source_hash_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("inc")).unwrap();
        std::fs::write(dir.join("inc/a.h"), "a").unwrap();
        std::fs::write(dir.join("b.cpp"), "b").unwrap();

        let flags = std::cell::RefCell::new("/O2".to_string());
        let flags_text = || flags.borrow().clone();
        let inputs = HashInputs {
            compiler: "Msvc 19.51 x64",
            flags: &flags_text,
            dir: &dir,
            root: &dir,
            out_root: &dir.join("out"),
        };
        let hash = |text: &str| expand(text, &inputs);

        // Digits in place, and only the listed things matter
        let first = hash("X=0x{hash:compiler,flags,inc/*.h,b.cpp}ull").unwrap();
        assert!(first.starts_with("X=0x") && first.ends_with("ull") && first.len() == "X=0xull".len() + 16);
        assert_eq!(hash("X=0x{hash:compiler,flags,inc/*.h,b.cpp}ull").unwrap(), first);

        // Flags, a new header the glob takes in, and changed contents all change it
        *flags.borrow_mut() = "/O1".into();
        let other_flags = hash("X=0x{hash:compiler,flags,inc/*.h,b.cpp}ull").unwrap();
        assert_ne!(other_flags, first);
        std::fs::write(dir.join("inc/c.h"), "c").unwrap();
        let more_headers = hash("X=0x{hash:compiler,flags,inc/*.h,b.cpp}ull").unwrap();
        assert_ne!(more_headers, other_flags);

        // A missing file or an empty glob is an error, not an empty hash
        let missing = hash("{hash:b.cp}").unwrap_err();
        assert!(format!("{missing:#}").contains("cannot read"), "{missing:#}");
        let empty = hash("{hash:inc/*.hpp}").unwrap_err();
        assert!(format!("{empty:#}").contains("matches no files"), "{empty:#}");

        // Brace groups inside the item list
        std::fs::write(dir.join("inc/d.hpp"), "d").unwrap();
        let grouped = hash("A{hash:inc/*.{h,hpp},b.cpp}B").unwrap();
        assert!(grouped.starts_with('A') && grouped.ends_with('B') && grouped.len() == 18, "{grouped}");
        assert_ne!(grouped, hash("A{hash:inc/*.h,b.cpp}B").unwrap());

        std::fs::remove_dir_all(&dir).ok();
    }
}
