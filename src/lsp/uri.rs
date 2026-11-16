//! file:// URIs to paths and back.

use std::path::{Path, PathBuf};

/// The path of a file URI (`file:///C:/x`, `file:///c%3A/x`, `file://server/share/x`).
pub fn to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let (authority, path) = match rest.find('/') {
        Some(slash) => (&rest[..slash], &rest[slash..]),
        None => (rest, "/"),
    };
    let path = decode(path)?;

    // A host is a UNC share; `/C:/x` is a drive path on Windows
    let bytes = path.as_bytes();
    let path = if !authority.is_empty() && authority != "localhost" {
        PathBuf::from(format!("//{authority}{path}"))
    } else if cfg!(windows) && bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        PathBuf::from(&path[1..])
    } else {
        PathBuf::from(path)
    };

    // `x/../y` is the file `y` the checks know
    let dotted = path
        .components()
        .any(|component| matches!(component, std::path::Component::CurDir | std::path::Component::ParentDir));
    match dotted {
        true => Some(crate::util::normalize(&path)),
        false => Some(path),
    }
}

/// How a client writes a drive in its file URIs (`file:///C:/x`, `file:///c%3A/x`): the files it did not name are
/// written the same way, so that a client keying diagnostics by the URI's text never holds one file twice.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Style {
    lowercase_drive: bool,
    escaped_colon: bool,
}

impl Style {
    /// The style of a client's URI, when it is a file URI with a drive letter.
    pub fn of(uri: &str) -> Option<Style> {
        let rest = uri.strip_prefix("file:///")?;
        let drive = rest.chars().next().filter(char::is_ascii_alphabetic)?;
        let after = &rest[1..];

        let escaped_colon = after.get(..3).is_some_and(|colon| colon.eq_ignore_ascii_case("%3A"));
        if !escaped_colon && !after.starts_with(':') {
            return None;
        }
        Some(Style {
            lowercase_drive: drive.is_ascii_lowercase(),
            escaped_colon,
        })
    }
}

/// The file URI of a path, a drive written in the client's style.
pub fn from_path(path: &Path, style: Style) -> String {
    let written = crate::util::slash(path);
    let mut uri = String::from("file://");

    if let Some(unc) = written.strip_prefix("//") {
        // A UNC path: the server is the authority
        uri.push_str(&encode(unc));
        return uri;
    }

    // A drive path: its letter and colon as the client writes them
    let bytes = written.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let drive = bytes[0] as char;
        uri.push('/');
        uri.push(if style.lowercase_drive {
            drive.to_ascii_lowercase()
        } else {
            drive.to_ascii_uppercase()
        });
        uri.push_str(if style.escaped_colon { "%3A" } else { ":" });
        uri.push_str(&encode(&written[2..]));
        return uri;
    }

    if !written.starts_with('/') {
        uri.push('/');
    }
    uri.push_str(&encode(&written));

    uri
}

fn encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/:!$&'()*+,;=@".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }

    encoded
}

fn decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());

    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }

    String::from_utf8(decoded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        if cfg!(windows) {
            assert_eq!(
                to_path("file:///c%3A/My%20Project/anvil.toml").unwrap(),
                PathBuf::from("c:/My Project/anvil.toml")
            );
            assert_eq!(to_path("file:///C:/x/anvil.toml").unwrap(), PathBuf::from("C:/x/anvil.toml"));
            assert_eq!(
                from_path(Path::new("C:/My Project/é.toml"), Style::default()),
                "file:///C:/My%20Project/%C3%A9.toml"
            );
            assert_eq!(
                to_path(&from_path(Path::new("C:/a b/#x.toml"), Style::default())).unwrap(),
                PathBuf::from("C:/a b/#x.toml")
            );
        }
        assert_eq!(to_path("file:///home/u/anvil.toml").unwrap(), PathBuf::from("/home/u/anvil.toml"));
        assert_eq!(from_path(Path::new("/home/u/a b.toml"), Style::default()), "file:///home/u/a%20b.toml");
        assert_eq!(to_path("file:///home/u/a/../anvil.toml").unwrap(), PathBuf::from("/home/u/anvil.toml"));
        assert!(to_path("untitled:Untitled-1").is_none());
        assert!(to_path("file:///bad%zz").is_none());
    }

    #[test]
    fn writes_drives_in_the_client_style() {
        let vscode = Style::of("file:///c%3A/Users/x/anvil.toml").unwrap();
        assert_eq!(from_path(Path::new("C:/a b/anvil.toml"), vscode), "file:///c%3A/a%20b/anvil.toml");
        let plain = Style::of("file:///D:/x").unwrap();
        assert_eq!(plain, Style::default());
        assert_eq!(from_path(Path::new("c:/x/anvil.toml"), plain), "file:///C:/x/anvil.toml");
        assert!(Style::of("file:///home/u").is_none());
        assert!(Style::of("untitled:x").is_none());
    }
}
