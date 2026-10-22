//! `#[macro_export]` sugar for emitting structured diagnostics from
//! anywhere in the crate (and from `crates/core`).
//!
//! Every macro expands to a single `tracing::*!` call with a fixed
//! vocabulary of structured fields (`subsystem`, `component`, `op`,
//! `id`, `status`, `addr`, …). The `tracing_subscriber` registry set
//! up in [`crate::context::logmgr`] fans those events out to three
//! sinks: per-`id` `indicatif::ProgressBar` rows for task lifecycle
//! events, a compact console formatter for everything else, and a
//! JSONL structured log at `<logdir>/edo.jsonl`.
//!
//! Public surface: `header!`, `summary!`, `ui_start_build!`,
//! `ui_finish_build!`, `ui_start_task!`, `ui_update_task!`,
//! `ui_info!`, `ui_warn!`, `ui_error!`, `ui_fatal!`.

/// Emits the session provenance record: tool name, version, and
/// optionally the build target and the *names* of `--arg` variables.
///
/// Only pass argument keys, never values: the record is persisted to the
/// JSONL log and values may carry secrets.
#[macro_export]
macro_rules! header {
    (tool = $tool: expr, version = $version: expr $(, target = $target: expr)? $(, arg_keys = $keys: expr)? $(,)?) => {
        $crate::__tracing::info!(
            tool = %$tool,
            version = %$version
            $(, target = %$target)?
            $(, arg_keys = ?$keys)?,
            "starting execution"
        );
    };
}

#[macro_export]
macro_rules! summary {
    (path = $path: expr, transforms = $t: expr, sources = $s: expr, farms = $f: expr, locked = $l: expr) => {
        $crate::__tracing::info!(
            path = %$path.display(),
            transforms = $t,
            sources = $s,
            farms = $f,
            locked = $l,
            "project loaded"
        );
    };
}

#[macro_export]
macro_rules! ui_start_task {
    (subsystem = $subsystem: expr $(, component = $component: expr)?, op = $op: expr ; $id: expr, $status: expr, $message: expr ) => {{
        let __id = ::std::string::ToString::to_string(&$id);
        let __status: $crate::log::TaskStatus = $status;
        let __phase: ::std::option::Option<::std::string::String> = $message;
        $crate::__tracing::info!(
            subsystem = $subsystem $(, component = $component)?, op = $op,
            id = __id.as_str(),
            status = __status.as_str(),
            phase = __phase.as_deref(),
            "task started"
        );
    }};
}

#[macro_export]
macro_rules! ui_update_task {
    (subsystem = $subsystem: expr $(, component = $component: expr)?, op = $op: expr ; $id: expr, $status: expr, $message: expr ) => {{
        let __id = ::std::string::ToString::to_string(&$id);
        let __status: $crate::log::TaskStatus = $status;
        let __phase: ::std::option::Option<::std::string::String> = $message;
        $crate::__tracing::info!(
            subsystem = $subsystem $(, component = $component)?, op = $op,
            id = __id.as_str(),
            status = __status.as_str(),
            phase = __phase.as_deref(),
            "task updated"
        );
    }};
}

#[macro_export]
macro_rules! ui_start_build {
    ($addr: expr, $count: expr) => {
        $crate::__tracing::info!(addr = $addr, total = $count, "starting build");
    };
}

#[macro_export]
macro_rules! ui_finish_build {
    () => {
        $crate::__tracing::info!("finishing build");
    };
}

#[macro_export]
macro_rules! ui_info {
    (subsystem = $subsystem: expr $(, component = $component: expr)? $(, op = $op: expr)?, id = $id: expr ; $($arg:tt)*) => {{
        $crate::__tracing::info!(subsystem = $subsystem $(, component = $component)? $(, op = $op)?, id = %$id, $($arg)*);
    }};
    (subsystem = $subsystem: expr $(, component = $component: expr)? $(, op = $op: expr)? ; $($arg:tt)*) => {{
        $crate::__tracing::info!(subsystem = $subsystem $(, component = $component)? $(, op = $op)?, $($arg)*);
    }};
}

#[macro_export]
macro_rules! ui_warn {
    (subsystem = $subsystem: expr $(, component = $component: expr)? $(, op = $op: expr)?, id = $id: expr ; $($arg:tt)*) => {{
        $crate::__tracing::warn!(subsystem = $subsystem $(, component = $component)? $(, op = $op)?, id = %$id, $($arg)*);
    }};
    (subsystem = $subsystem: expr $(, component = $component: expr)? $(, op = $op: expr)? ; $($arg:tt)*) => {{
        $crate::__tracing::warn!(subsystem = $subsystem $(, component = $component)? $(, op = $op)?, $($arg)*);
    }};
}

#[macro_export]
macro_rules! ui_error {
    (subsystem = $subsystem: expr $(, component = $component: expr)? $(, op = $op: expr)?, id = $id: expr ; $($arg:tt)*) => {{
        $crate::__tracing::error!(subsystem = $subsystem $(, component = $component)? $(, op = $op)?, id = %$id, $($arg)*);
    }};
    (subsystem = $subsystem: expr $(, component = $component: expr)? $(, op = $op: expr)? ; $($arg:tt)*) => {{
        $crate::__tracing::error!(subsystem = $subsystem $(, component = $component)? $(, op = $op)?, $($arg)*);
    }};
}

#[macro_export]
macro_rules! ui_fatal {
    (subsystem = $subsystem: expr $(, component = $component: expr)? $(, op = $op: expr)?, id = $id: expr ; $($arg:tt)*) => {{
        $crate::__tracing::error!(subsystem = $subsystem $(, component = $component)? $(, op = $op)?, id = %$id, fatal = true, $($arg)*);
    }};
    (subsystem = $subsystem: expr $(, component = $component: expr)? $(, op = $op: expr)? ; $($arg:tt)*) => {{
        $crate::__tracing::error!(subsystem = $subsystem $(, component = $component)? $(, op = $op)?, fatal = true, $($arg)*);
    }};
}
