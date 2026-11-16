//! Filesystem and path helpers shared by every module.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

// Windows FILETIME counts 100 ns ticks from 1601; this is 1970 in those ticks
const FILETIME_UNIX_EPOCH: i64 = 116_444_736_000_000_000;

// GetFileAttributesExW fails on longer paths without the \\?\ prefix
#[cfg(windows)]
const WINDOWS_MAX_PATH: usize = 260;

/// Lexically normalizes a path: drops `.`, resolves `..` against earlier components, never touches the disk.
pub fn normalize(path: &Path) -> PathBuf {
    let mut components: Vec<Component> = Vec::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match components.last() {
                Some(Component::Normal(_)) => {
                    components.pop();
                }
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                _ => components.push(component),
            },
            _ => components.push(component),
        }
    }

    let mut normalized = PathBuf::new();
    for component in components {
        normalized.push(component.as_os_str());
    }

    if normalized.as_os_str().is_empty() {
        normalized.push(".");
    }

    normalized
}

/// `path` made absolute against `base`, normalized.
pub fn absolute(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize(path)
    } else {
        normalize(&base.join(path))
    }
}

/// Forward-slash form of a path, for messages and generated files.
pub fn slash(path: &Path) -> String {
    let text = path.to_string_lossy();

    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text.into_owned()
    }
}

/// `path` relative to `base` when it lies under it (forward slashes), else the full path.
pub fn rel(path: &Path, base: &Path) -> String {
    match strip_prefix_ci(path, base) {
        Some(relative) if !relative.as_os_str().is_empty() => slash(&relative),
        Some(_) => ".".into(),
        None => slash(path),
    }
}

/// `Path::strip_prefix`, case-insensitive on Windows.
pub fn strip_prefix_ci(path: &Path, base: &Path) -> Option<PathBuf> {
    if !cfg!(windows) {
        return path.strip_prefix(base).ok().map(|relative| relative.to_path_buf());
    }

    // Compare lowercased forward-slash forms, but return the original spelling
    let path_text = slash(path).to_lowercase();
    let mut base_text = slash(base).to_lowercase();
    if !base_text.ends_with('/') {
        base_text.push('/');
    }

    if path_text == base_text.trim_end_matches('/') {
        return Some(PathBuf::new());
    }

    if path_text.starts_with(&base_text) {
        let original = slash(path);
        return Some(PathBuf::from(&original[base_text.len()..]));
    }

    None
}

/// A key that compares equal for the same file (case-folded on Windows).
pub fn path_key(path: &Path) -> String {
    if cfg!(windows) {
        slash(path).to_lowercase()
    } else {
        path.to_string_lossy().into_owned()
    }
}

/// Modification time in the platform's native ticks, or None when the file does not exist.
#[cfg(windows)]
pub fn mtime(path: &Path) -> Option<i64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesExW, GetFileExInfoStandard, WIN32_FILE_ATTRIBUTE_DATA};

    let wide_path: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();

    // One system call, much cheaper than std::fs::metadata (which opens a handle)
    if wide_path.len() < WINDOWS_MAX_PATH {
        let mut data: WIN32_FILE_ATTRIBUTE_DATA = unsafe { std::mem::zeroed() };
        let succeeded = unsafe { GetFileAttributesExW(wide_path.as_ptr(), GetFileExInfoStandard, &mut data as *mut _ as *mut _) };
        if succeeded == 0 {
            return None;
        }

        let write_time = ((data.ftLastWriteTime.dwHighDateTime as u64) << 32) | data.ftLastWriteTime.dwLowDateTime as u64;
        return Some(write_time as i64);
    }

    let metadata = std::fs::metadata(path).ok()?;
    Some(system_time_ticks(metadata.modified().ok()?))
}

/// Every entry of a folder with its modification time, keyed as `path_key` keys file names: one directory listing
/// instead of a system call per file, which is what makes a no-op build of thousands of files cheap on Windows.
/// None when the folder cannot be listed.
#[cfg(windows)]
pub fn folder_mtimes(folder: &Path) -> Option<HashMap<String, i64>> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW, FIND_FIRST_EX_LARGE_FETCH, WIN32_FIND_DATAW,
    };

    let pattern = folder.join("*");
    let wide_pattern: Vec<u16> = pattern.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    if wide_pattern.len() >= WINDOWS_MAX_PATH {
        return None;
    }

    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let handle = unsafe {
        FindFirstFileExW(
            wide_pattern.as_ptr(),
            FindExInfoBasic,
            &mut data as *mut _ as *mut _,
            FindExSearchNameMatch,
            std::ptr::null(),
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }

    let mut entries = HashMap::new();
    loop {
        let length = data
            .cFileName
            .iter()
            .position(|character| *character == 0)
            .unwrap_or(data.cFileName.len());
        let name = std::ffi::OsString::from_wide(&data.cFileName[..length]).to_string_lossy().to_lowercase();
        let write_time = ((data.ftLastWriteTime.dwHighDateTime as u64) << 32) | data.ftLastWriteTime.dwLowDateTime as u64;
        entries.insert(name, write_time as i64);

        if unsafe { FindNextFileW(handle, &mut data) } == 0 {
            break;
        }
    }
    unsafe { FindClose(handle) };

    Some(entries)
}

/// Elsewhere a stat is cheap: no listing.
#[cfg(not(windows))]
pub fn folder_mtimes(_folder: &Path) -> Option<HashMap<String, i64>> {
    None
}

/// A file's modification time (as `mtime` gives it) and size, or None when it does not exist.
#[cfg(windows)]
pub fn stamp(path: &Path) -> Option<(i64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesExW, GetFileExInfoStandard, WIN32_FILE_ATTRIBUTE_DATA};

    let wide_path: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();

    // One system call, as `mtime`
    if wide_path.len() < WINDOWS_MAX_PATH {
        let mut data: WIN32_FILE_ATTRIBUTE_DATA = unsafe { std::mem::zeroed() };
        let succeeded = unsafe { GetFileAttributesExW(wide_path.as_ptr(), GetFileExInfoStandard, &mut data as *mut _ as *mut _) };
        if succeeded == 0 {
            return None;
        }

        let write_time = ((data.ftLastWriteTime.dwHighDateTime as u64) << 32) | data.ftLastWriteTime.dwLowDateTime as u64;
        let size = ((data.nFileSizeHigh as u64) << 32) | data.nFileSizeLow as u64;
        return Some((write_time as i64, size));
    }

    let metadata = std::fs::metadata(path).ok()?;
    Some((system_time_ticks(metadata.modified().ok()?), metadata.len()))
}

/// A file's modification time (as `mtime` gives it) and size, or None when it does not exist.
#[cfg(not(windows))]
pub fn stamp(path: &Path) -> Option<(i64, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((system_time_ticks(metadata.modified().ok()?), metadata.len()))
}

/// Modification time in the platform's native ticks, or None when the file does not exist.
#[cfg(not(windows))]
pub fn mtime(path: &Path) -> Option<i64> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(system_time_ticks(metadata.modified().ok()?))
}

/// A SystemTime in the same units `mtime` returns.
pub fn system_time_ticks(time: std::time::SystemTime) -> i64 {
    let nanoseconds = match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos() as i64,
        Err(error) => -(error.duration().as_nanos() as i64),
    };

    if cfg!(windows) {
        nanoseconds / 100 + FILETIME_UNIX_EPOCH
    } else {
        nanoseconds
    }
}

pub fn now_ticks() -> i64 {
    system_time_ticks(std::time::SystemTime::now())
}

pub fn hash_bytes(bytes: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(bytes)
}

pub fn hash_file(path: &Path) -> Option<u64> {
    std::fs::read(path).ok().map(|bytes| hash_bytes(&bytes))
}

/// Writes `content` only when it differs, so an unchanged generated file keeps its time stamp.
pub fn write_if_changed(path: &Path, content: &[u8]) -> Result<bool> {
    if let Ok(existing) = std::fs::read(path) {
        if existing == content {
            return Ok(false);
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))?;

    Ok(true)
}

/// Atomic replace: write a sibling temporary file, then rename over the target.
pub fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let temporary = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&temporary, content).with_context(|| format!("writing {}", temporary.display()))?;
    std::fs::rename(&temporary, path).with_context(|| format!("replacing {}", path.display()))?;

    Ok(())
}

/// Expands `$VAR`, `${VAR}` and a leading `~` in a manifest string. A missing variable is an error, not "".
pub fn expand_env(text: &str) -> Result<String> {
    // Home folder
    if let Some(rest) = text.strip_prefix('~') {
        if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') {
            return Ok(slash(&home_dir()) + &expand_env(rest)?);
        }
    }

    let mut expanded = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();

    while let Some(character) = characters.next() {
        if character != '$' {
            expanded.push(character);
            continue;
        }

        // Read the variable name, braced or bare
        let mut name = String::new();
        if characters.peek() == Some(&'{') {
            characters.next();
            for character in characters.by_ref() {
                if character == '}' {
                    break;
                }
                name.push(character);
            }
        } else {
            while let Some(&character) = characters.peek() {
                if !character.is_ascii_alphanumeric() && character != '_' {
                    break;
                }
                name.push(character);
                characters.next();
            }
        }

        if name.is_empty() {
            expanded.push('$');
            continue;
        }
        if name == "ENV" && characters.peek() == Some(&'{') {
            let variable: String = characters.clone().skip(1).take_while(|character| *character != '}').collect();
            bail!("`$ENV{{{variable}}}` in \"{text}\" is CMake's; write {{env:{variable}}} or ${variable}");
        }

        match std::env::var(&name) {
            Ok(value) => expanded.push_str(&value),
            Err(_) => bail!("environment variable `{name}` is not set (used in \"{text}\")"),
        }
    }

    expand_env_placeholders(&expanded)
}

/// Expands `{env:NAME}` placeholders (in dependencies' definitions and flags too, where `$` is left alone). A missing
/// variable is an error, not "".
pub fn expand_env_placeholders(text: &str) -> Result<String> {
    // A placeholder in another case is no placeholder ({ENV:X} would stay in a path as written)
    if let Some(start) = text
        .to_ascii_lowercase()
        .match_indices("{env:")
        .map(|(index, _)| index)
        .find(|index| !text[*index..].starts_with("{env:"))
    {
        let written: String = text[start..].chars().take_while(|character| *character != '}').chain(['}']).collect();
        let name = &written[5..written.len() - 1];
        bail!("`{written}` in \"{text}\" is no placeholder: placeholders are lower case (did you mean `{{env:{name}}}`?)");
    }

    let mut expanded = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(start) = rest.find("{env:") {
        expanded.push_str(&rest[..start]);
        let after = &rest[start + "{env:".len()..];
        let Some(end) = after.find('}') else {
            bail!("`{{env:` has no closing `}}` in \"{text}\"; write {{env:NAME}}")
        };
        let name = &after[..end];
        match std::env::var(name) {
            Ok(value) => expanded.push_str(&value),
            Err(_) => bail!("environment variable `{name}` is not set (used in \"{text}\")"),
        }
        rest = &after[end + 1..];
    }
    expanded.push_str(rest);

    Ok(expanded)
}

/// A definition or flag with its `{env:NAME}` placeholders expanded (`expand_env_placeholders`). One that the
/// variables leave empty is an error: the compiler would get an empty argument.
pub fn expand_env_word(text: &str) -> Result<String> {
    let expanded = expand_env_placeholders(text)?;
    if expanded.trim().is_empty() && !text.trim().is_empty() {
        bail!("\"{text}\" is empty once its environment variables are filled in: set them, or leave the entry out");
    }

    Ok(expanded)
}

/// A path's environment variables expanded (`expand_env`), where a variable set but empty is an error: the path would
/// silently become another (the package root, for a path of only that variable). With `split`, a path that is one
/// variable alone (`{env:CMAKE_PREFIX_PATH}`) whose value is a list (`a;b` on Windows, `a:b` elsewhere) gives each entry.
pub fn expand_env_path(text: &str, split: bool) -> Result<Vec<String>> {
    static VARIABLE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\{env:([^}]*)\}|\$\{([^}]*)\}|\$([A-Za-z0-9_]+)").unwrap());

    let names: Vec<&str> = VARIABLE
        .captures_iter(text)
        .filter_map(|captures| captures.iter().skip(1).flatten().next().map(|name| name.as_str()))
        .collect();
    if let Some(name) = names.iter().find(|name| std::env::var(name).is_ok_and(|value| value.trim().is_empty())) {
        bail!("environment variable `{name}` is empty (used in \"{text}\"); set it to a folder, or write the path");
    }

    // Anvil's other placeholders ({root}, {target:...}) mean nothing in a path
    static PLACEHOLDER: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?:^|[^$])(\{([A-Za-z_]+)(?::[^}]*)?\})").unwrap());
    if let Some(captures) = PLACEHOLDER.captures_iter(text).find(|captures| !captures[2].eq_ignore_ascii_case("env")) {
        bail!(
            "`{}` in \"{text}\" is no placeholder here: paths take {{env:NAME}}, $NAME and ~, and a relative path is from the package's folder",
            &captures[1]
        );
    }

    // One variable alone: its list's entries
    let whole = VARIABLE.find(text.trim()).is_some_and(|found| found.len() == text.trim().len());
    if split && whole {
        let separator = if cfg!(windows) { ';' } else { ':' };
        let value = expand_env(text.trim())?;
        return Ok(value
            .split(separator)
            .filter(|entry| !entry.trim().is_empty())
            .map(String::from)
            .collect());
    }

    Ok(vec![expand_env(text)?])
}

pub fn home_dir() -> PathBuf {
    let variable = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(variable).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// Per-user data folder for caches and settings: %LOCALAPPDATA%/anvil or ~/.cache/anvil.
pub fn user_data_dir() -> PathBuf {
    if cfg!(windows) {
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local_app_data).join("anvil");
        }
    } else if let Some(cache_home) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(cache_home).join("anvil");
    }

    home_dir().join(".cache").join("anvil")
}

/// Finds a program on PATH (honouring PATHEXT on Windows).
pub fn which(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        return candidate.exists().then(|| candidate.to_path_buf());
    }

    // Each name is searched once per run: a build step's program is looked up for every file it runs over
    static FOUND: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Option<PathBuf>>>> = std::sync::OnceLock::new();
    let found = FOUND.get_or_init(Default::default);
    if let Some(known) = found.lock().unwrap().get(name) {
        return known.clone();
    }

    let result = search_path(name);
    found.lock().unwrap().insert(name.to_string(), result.clone());
    result
}

/// The first file named `name` (with a PATHEXT extension on Windows) in a PATH folder.
fn search_path(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);

    // Extensions to try: PATHEXT on Windows unless the name has one, else none
    let extensions: Vec<String> = if cfg!(windows) && candidate.extension().is_none() {
        let path_extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT".into());
        path_extensions
            .split(';')
            .map(|extension| extension.to_ascii_lowercase())
            .chain(std::iter::once(String::new()))
            .collect()
    } else {
        vec![String::new()]
    };

    let search_path = std::env::var_os("PATH")?;
    for folder in std::env::split_paths(&search_path) {
        for extension in &extensions {
            let file = folder.join(format!("{name}{extension}"));
            if file.is_file() {
                return Some(file);
            }
        }
    }

    None
}

/// Splits a command string into words the way a shell would (quotes; backslash escapes only off Windows, so that
/// Windows paths survive).
pub fn split_command(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted_empty = false;
    let mut quote: Option<char> = None;
    let mut characters = text.chars();

    while let Some(character) = characters.next() {
        // Inside quotes everything is literal up to the closing quote
        if let Some(open) = quote {
            if character == open {
                quote = None;
            } else {
                word.push(character);
            }
            continue;
        }

        match character {
            '"' | '\'' => {
                quote = Some(character);
                quoted_empty = true;
            }
            '\\' if !cfg!(windows) => {
                if let Some(escaped) = characters.next() {
                    word.push(escaped);
                }
            }
            whitespace if whitespace.is_whitespace() => {
                if quoted_empty || !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                    quoted_empty = false;
                }
            }
            other => word.push(other),
        }
    }

    if quoted_empty || !word.is_empty() {
        words.push(word);
    }

    words
}

/// " (did you mean `close`?)" when a candidate is close to `word`, else nothing: put right after the word in a message.
pub fn did_you_mean<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> String {
    suggest(word, candidates)
        .map(|close| format!(" (did you mean `{close}`?)"))
        .unwrap_or_default()
}

/// The closest candidate by edit distance, for "did you mean" hints.
pub fn suggest<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let max_distance = (word.len().max(3) / 3).max(2);
    let mut best: Option<(&str, usize)> = None;

    for candidate in candidates {
        let distance = edit_distance(word, candidate);
        if distance <= max_distance && best.is_none_or(|(_, best_distance)| distance < best_distance) {
            best = Some((candidate, distance));
        }
    }

    best.map(|(candidate, _)| candidate)
}

pub fn is_source_ext(extension: &str) -> bool {
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "c" | "cc" | "cpp" | "cxx" | "c++" | "rc" | "asm" | "s" | "cu"
    )
}

fn edit_distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous_row: Vec<usize> = (0..=right.len()).collect();

    for (row, left_character) in left.chars().enumerate() {
        let mut current_row = vec![row + 1];
        for (column, right_character) in right.iter().enumerate() {
            let substitution = previous_row[column] + usize::from(left_character != *right_character);
            let deletion = previous_row[column + 1] + 1;
            let insertion = current_row[column] + 1;
            current_row.push(substitution.min(deletion).min(insertion));
        }
        previous_row = current_row;
    }

    previous_row[right.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        assert_eq!(slash(&normalize(Path::new("a/./b/../c"))), "a/c");
        assert_eq!(slash(&normalize(Path::new("../x"))), "../x");
    }

    #[test]
    fn splits_commands() {
        assert_eq!(split_command(r#"slangc "a b.slang" -o x"#), vec!["slangc", "a b.slang", "-o", "x"]);
        assert_eq!(split_command(r#"x "" y"#), vec!["x", "", "y"]);
    }

    #[test]
    fn suggests() {
        assert_eq!(suggest("privat-include", ["private-include", "include"]), Some("private-include"));
        assert_eq!(suggest("zzzzzz", ["include"]), None);
    }

    #[test]
    fn strips_prefix_case_insensitively() {
        if cfg!(windows) {
            assert_eq!(rel(Path::new("C:/Foo/Bar/x.h"), Path::new("c:/foo")), "Bar/x.h");
        }
    }

    #[test]
    fn expands_environment_placeholders() {
        let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let value = std::env::var(home).unwrap();
        assert_eq!(expand_env_placeholders(&format!("{{env:{home}}}/sdk")).unwrap(), format!("{value}/sdk"));
        assert_eq!(expand_env(&format!("{{env:{home}}}/a/${home}")).unwrap(), format!("{value}/a/{value}"));
        assert_eq!(expand_env_placeholders("-DCOST=$5").unwrap(), "-DCOST=$5");

        let error = expand_env_placeholders("{env:ANVIL_SURELY_UNSET_VARIABLE}/Include")
            .unwrap_err()
            .to_string();
        assert!(error.contains("`ANVIL_SURELY_UNSET_VARIABLE` is not set"), "{error}");
        assert!(expand_env_placeholders("{env:HOME").is_err());

        // Another case, or CMake's form, is no placeholder
        let error = expand_env_placeholders(&format!("{{ENV:{home}}}/sdk")).unwrap_err().to_string();
        assert!(error.contains(&format!("did you mean `{{env:{home}}}`")), "{error}");
        let error = expand_env(&format!("$ENV{{{home}}}/sdk")).unwrap_err().to_string();
        assert!(error.contains(&format!("write {{env:{home}}}")), "{error}");
    }

    #[test]
    fn expands_environment_paths() {
        let (separator, list) = if cfg!(windows) { (';', "PATHEXT") } else { (':', "PATH") };
        let value = std::env::var(list).unwrap();
        let entries: Vec<String> = value.split(separator).filter(|entry| !entry.is_empty()).map(String::from).collect();

        // A list split only where it is the whole path, and asked for
        assert_eq!(expand_env_path(&format!("{{env:{list}}}"), true).unwrap(), entries);
        assert_eq!(expand_env_path(&format!("{{env:{list}}}"), false).unwrap(), vec![value.clone()]);
        assert_eq!(expand_env_path(&format!("{{env:{list}}}/x"), true).unwrap(), vec![format!("{value}/x")]);

        // Other placeholders are said, not taken as folder names
        let error = expand_env_path("{root}/sdk", true).unwrap_err().to_string();
        assert!(error.contains("`{root}` in \"{root}/sdk\" is no placeholder here"), "{error}");
        assert!(expand_env_path(&format!("${{{list}}}/x"), true).is_ok());

        // An empty variable would make the path another
        let empty = std::env::vars().find(|(_, value)| value.is_empty()).map(|(name, _)| name);
        if let Some(name) = empty {
            let error = expand_env_path(&format!("{{env:{name}}}"), true).unwrap_err().to_string();
            assert!(error.contains(&format!("`{name}` is empty")), "{error}");
        }
    }
}
