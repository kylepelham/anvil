//! Reads the header lists compilers write: make-style depfiles (gcc, clang, run steps), cl's /sourceDependencies
//! JSON, and clang-cl's /showIncludes lines.

/// The prerequisites of a make-style depfile ("out.o: a.cpp b.h \\\n c.h"), with escaped spaces resolved.
pub fn parse_make(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut word = String::new();
    let mut characters = text.chars().peekable();
    let mut seen_colon = false;

    let finish_word = |word: &mut String, seen_colon: &mut bool, paths: &mut Vec<String>| {
        if word.is_empty() {
            return;
        }

        // The first word ending in ':' is the target
        if !*seen_colon && word.ends_with(':') {
            *seen_colon = true;
        } else if *seen_colon && word != ":" {
            paths.push(std::mem::take(word));
        }
        word.clear();
    };

    while let Some(character) = characters.next() {
        match character {
            '\\' => match characters.peek() {
                // Line continuation
                Some('\n') | Some('\r') => {
                    characters.next();
                    if characters.peek() == Some(&'\n') {
                        characters.next();
                    }
                    finish_word(&mut word, &mut seen_colon, &mut paths);
                }
                // Escaped space, hash or backslash
                Some(' ') | Some('#') | Some('\\') => word.push(characters.next().unwrap()),
                // A Windows path separator
                _ => word.push('\\'),
            },
            '$' if characters.peek() == Some(&'$') => {
                characters.next();
                word.push('$');
            }
            ' ' | '\t' | '\n' | '\r' => finish_word(&mut word, &mut seen_colon, &mut paths),
            ':' if !seen_colon && matches!(characters.peek(), Some(' ') | Some('\n') | Some('\r') | None) => {
                word.push(':');
                finish_word(&mut word, &mut seen_colon, &mut paths);
            }
            other => word.push(other),
        }
    }
    finish_word(&mut word, &mut seen_colon, &mut paths);

    paths
}

/// The includes listed in cl's /sourceDependencies output.
pub fn parse_msvc_json(text: &str) -> Vec<String> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };

    json.get("Data")
        .and_then(|data| data.get("Includes"))
        .and_then(|includes| includes.as_array())
        .map(|includes| {
            includes
                .iter()
                .filter_map(|include| include.as_str().map(|path| path.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Splits clang-cl's stdout into the /showIncludes header paths and the remaining output.
pub fn split_show_includes(output: &str) -> (Vec<String>, String) {
    const PREFIX: &str = "Note: including file:";
    let mut headers = Vec::new();
    let mut remaining = String::new();

    for line in output.lines() {
        match line.strip_prefix(PREFIX) {
            Some(path) => headers.push(path.trim().to_string()),
            None => {
                remaining.push_str(line);
                remaining.push('\n');
            }
        }
    }

    (headers, remaining)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_depfile() {
        let text = "obj/a.o: src/a.cpp include/my\\ file.h \\\n  C:/x/y.h C:\\win\\path.h\n";
        assert_eq!(parse_make(text), vec!["src/a.cpp", "include/my file.h", "C:/x/y.h", "C:\\win\\path.h"]);
    }

    #[test]
    fn make_depfile_with_windows_target() {
        let text = "C:/out/a.o: C:/src/a.cpp\n";
        assert_eq!(parse_make(text), vec!["C:/src/a.cpp"]);
    }

    #[test]
    fn msvc_json() {
        let text = r#"{"Version":"1.2","Data":{"Source":"c:\\a.cpp","Includes":["c:\\b.h","c:\\c.h"]}}"#;
        assert_eq!(parse_msvc_json(text), vec!["c:\\b.h", "c:\\c.h"]);
    }

    #[test]
    fn show_includes() {
        let (headers, rest) = split_show_includes("a.cpp\nNote: including file:  C:\\x.h\nwarning here\n");
        assert_eq!(headers, vec!["C:\\x.h"]);
        assert_eq!(rest, "a.cpp\nwarning here\n");
    }
}
