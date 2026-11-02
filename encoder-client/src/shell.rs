//! Quoting and validation for values interpolated into remote shell commands and unit files.

/// POSIX single-quote escaping.
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Conservative allowlist for names/paths that end up in shell commands *and* a systemd unit
/// (where `%`, `$`, quotes and whitespace are all special).
pub fn is_safe_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b'+'))
}

pub fn is_safe_abs_path(s: &str) -> bool {
    is_safe_token(s) && s.starts_with('/') && !s.split('/').any(|c| c == "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_neutralizes_metacharacters() {
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote("$(rm -rf /)"), "'$(rm -rf /)'");
    }

    #[test]
    fn path_validation() {
        assert!(is_safe_abs_path("/mnt/nas-1/media"));
        assert!(!is_safe_abs_path("mnt/nas"));
        assert!(!is_safe_abs_path("/mnt/../etc"));
        assert!(!is_safe_abs_path("/mnt/a b"));
        assert!(!is_safe_abs_path("/mnt/$HOME"));
        assert!(!is_safe_abs_path("/mnt/100%"));
        assert!(!is_safe_abs_path(""));
    }
}
