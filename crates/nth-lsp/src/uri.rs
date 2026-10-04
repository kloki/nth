//! `file://` URIs, which is how LSP names every document.

use std::{
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

/// Percent-encodes everything but the unreserved characters and `/`, the
/// way Node's `pathToFileURL` and VS Code do for the bytes that matter.
pub fn from_path(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// `None` for anything that is not a local `file://` URI.
pub fn to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = bytes
                .get(i + 1..i + 3)
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            decoded.push(byte);
            i += 3;
            continue;
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(OsString::from_vec(decoded)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_what_needs_encoding() {
        assert_eq!(from_path(Path::new("/a/b_c-d.rs")), "file:///a/b_c-d.rs");
        assert_eq!(
            from_path(Path::new("/my dir/#1/é.rs")),
            "file:///my%20dir/%231/%C3%A9.rs"
        );
    }

    #[test]
    fn round_trips() {
        for path in ["/x/y.rs", "/my dir/%20/é ü.ts", "/a+b/c@d"] {
            assert_eq!(
                to_path(&from_path(Path::new(path))).unwrap(),
                Path::new(path)
            );
        }
    }

    #[test]
    fn decodes_other_spellings() {
        assert_eq!(
            to_path("file://localhost/a%3Ab").unwrap(),
            Path::new("/a:b")
        );
        assert_eq!(to_path("file:///a%2fb").unwrap(), Path::new("/a/b"));
        assert!(to_path("untitled:Untitled-1").is_none());
        assert!(to_path("file://server/share").is_none());
    }
}
