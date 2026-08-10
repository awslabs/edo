use std::path::Path;

/// Wrap `s` in POSIX single quotes, escaping any embedded single quote
/// as `'\''`. Safe against every shell metacharacter — the shell does
/// no expansion inside single quotes except for `'` itself.
///
/// The returned string is suitable to concatenate directly into a
/// command line that will be handed to `sh -c`. Callers should apply
/// this to every path, glob, and caller-supplied argument before
/// handing it to [`edo::environment::Vfs::command`] or
/// [`edo::environment::Vfs::output`].
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

/// Convenience: shell-quote a filesystem path via its lossy string
/// representation. `Vfs` itself operates on `to_string_lossy` when
/// composing the shell command, so no additional fidelity is available.
pub fn shell_quote_path(path: &Path) -> String {
    shell_quote(&path.to_string_lossy())
}
