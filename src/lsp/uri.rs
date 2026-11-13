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
    if !authority.is_empty() && authority != "localhost" {
        return Some(PathBuf::from(format!("//{authority}{path}")));
    }
    let bytes = path.as_bytes();
    if cfg!(windows) && bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        return Some(PathBuf::from(&path[1..]));
    }

    Some(PathBuf::from(path))
}

/// The file URI of a path.
pub fn from_path(path: &Path) -> String {
    let written = crate::util::slash(path);
    let mut uri = String::from("file://");

    if let Some(unc) = written.strip_prefix("//") {
        // A UNC path: the server is the authority
        uri.push_str(&encode(unc));
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
            assert_eq!(from_path(Path::new("C:/My Project/é.toml")), "file:///C:/My%20Project/%C3%A9.toml");
            assert_eq!(to_path(&from_path(Path::new("C:/a b/#x.toml"))).unwrap(), PathBuf::from("C:/a b/#x.toml"));
        }
        assert_eq!(to_path("file:///home/u/anvil.toml").unwrap(), PathBuf::from("/home/u/anvil.toml"));
        assert!(to_path("untitled:Untitled-1").is_none());
        assert!(to_path("file:///bad%zz").is_none());
    }
}
