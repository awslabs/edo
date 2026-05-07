use snafu::Snafu;

/// Errors produced by the environment subsystem.
///
/// Covers failures from environment setup, command execution, storage access,
/// plugin invocation, and handlebars template rendering used by [`super::Command`].
#[derive(Snafu, Debug)]
#[snafu(visibility(pub))]
pub enum EnvironmentError {
    /// A propagated context-layer error (e.g. project/config issues).
    #[snafu(transparent)]
    Context {
        #[snafu(source(from(crate::context::ContextError, Box::new)))]
        source: Box<crate::context::ContextError>,
    },
    /// An opaque error surfaced by an environment implementation (plugin or builtin).
    #[snafu(transparent)]
    Implementation {
        source: Box<dyn snafu::Error + Send + Sync>,
    },
    #[snafu(display("IO error occured inside environment: {source}"))]
    Io { source: std::io::Error },
    /// A command executed inside the environment returned a non-zero exit status.
    #[snafu(display("command execution failed"))]
    Run,
    /// A propagated storage-layer error encountered during environment setup or I/O.
    #[snafu(transparent)]
    Storage {
        #[snafu(source(from(crate::storage::StorageError, Box::new)))]
        source: Box<crate::storage::StorageError>,
    },
    /// Handlebars template rendering failed while substituting command variables.
    #[snafu(display("failed to render substitution: {source}"))]
    Template { source: handlebars::RenderError },
    /// Failure during VFS operation
    #[snafu(display("vfs operation failed: {action}"))]
    Vfs { action: String },
}

#[cfg(test)]
mod tests {
    //! Unit tests for [`EnvironmentError`] Display/Debug formatting and
    //! transparent-variant plumbing. These tests are pure constructors — no
    //! async runtime, no filesystem.
    use super::*;
    use crate::context::{Addr, ContextError};

    #[test]
    fn run_variant_display_matches_snafu_message() {
        let e = EnvironmentError::Run;
        assert_eq!(e.to_string(), "command execution failed");
    }

    #[test]
    fn template_variant_display_includes_source() {
        let render_err = handlebars::Handlebars::new()
            .render_template("{{", &())
            .unwrap_err();
        let e = EnvironmentError::Template { source: render_err };
        let s = e.to_string();
        assert!(
            s.starts_with("failed to render substitution:"),
            "unexpected display: {s:?}"
        );
    }

    #[test]
    fn context_variant_transparent_display() {
        // `ContextError::NoEnvironmentFound` is a simple, stable variant.
        let addr = Addr::parse("//x/y").unwrap();
        let inner = ContextError::NoEnvironmentFound { addr };
        let inner_display = inner.to_string();
        let env_err: EnvironmentError = inner.into();
        // Snafu's `#[snafu(transparent)]` renders the inner Display verbatim.
        assert_eq!(env_err.to_string(), inner_display);
    }

    #[test]
    fn storage_variant_transparent_display() {
        use crate::storage::StorageError;
        let inner = StorageError::Id {
            reason: "bad-id".to_string(),
        };
        let inner_display = inner.to_string();
        let env_err: EnvironmentError = inner.into();
        assert_eq!(env_err.to_string(), inner_display);
    }

    #[test]
    fn implementation_variant_preserves_source() {
        let boxed: Box<dyn snafu::Error + Send + Sync> = Box::new(std::io::Error::other("boom"));
        let e = EnvironmentError::Implementation { source: boxed };
        let s = e.to_string();
        // Snafu's transparent variant delegates to the inner error's Display.
        assert!(s.contains("boom"), "expected 'boom' in display: {s:?}");
    }

    #[test]
    fn debug_format_is_nonempty() {
        // Smoke test that `#[derive(Debug)]` produces usable output.
        let s = format!("{:?}", EnvironmentError::Run);
        assert!(!s.is_empty());
        assert!(s.contains("Run"), "unexpected debug output: {s:?}");
    }
}
