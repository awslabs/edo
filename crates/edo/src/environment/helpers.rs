use std::path::Path;

/// Wrap `s` in POSIX single quotes, escaping any embedded single quote
/// as `'\''`. Safe against every shell metacharacter — the shell does
/// no expansion inside single quotes except for `'` itself.
///
/// The returned string is suitable to concatenate directly into a
/// command line that will be handed to `sh -c`. Callers should apply
/// this to every path, glob, and caller-supplied argument before
/// handing it to [`crate::environment::Vfs::command`] or
/// [`crate::environment::Vfs::output`].
pub fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            // Close the current single-quoted run, insert an escaped
            // single quote, then reopen the single-quoted run.
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Shell-quote a filesystem path. Non-UTF-8 bytes are replaced lossily.
pub fn shell_quote_path(path: &Path) -> String {
    shell_quote(&path.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Round-trip `s` through `sh -c 'printf %s <quoted>'`.
    fn sh_roundtrip(s: &str) -> String {
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {}", shell_quote(s)))
            .output()
            .expect("spawn sh");
        assert!(out.status.success(), "sh failed for {s:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn quotes_empty_string() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(sh_roundtrip(""), "");
    }

    #[test]
    fn escapes_embedded_single_quote() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(sh_roundtrip("it's"), "it's");
    }

    #[test]
    fn metacharacters_are_not_expanded() {
        for s in [
            "$(echo pwned)",
            "`echo pwned`",
            "${HOME}",
            "a b\tc",
            "line1\nline2",
            "; rm -rf /",
            "*?[a]",
            "\\\"'",
        ] {
            assert_eq!(sh_roundtrip(s), s);
        }
    }

    #[test]
    fn quotes_paths() {
        assert_eq!(shell_quote_path(Path::new("/a b/c'd")), "'/a b/c'\\''d'");
    }
}
