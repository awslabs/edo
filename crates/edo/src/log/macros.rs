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

#[macro_export]
macro_rules! header {
    ($tool: literal @ $version: expr) => {
        $crate::__tracing::info!(
            tool = $tool,
            version = $version.to_string(),
            "starting execution"
        );
    };
    ($tool: literal @ $version: expr => $addr: expr) => {
        $crate::__tracing::info!(
            tool = $tool,
            version = $version.to_string(),
            target = $addr,
            "starting execution"
        );
    };
    ($tool: literal @ $version: expr => $addr: expr, $args: expr) => {
        $crate::__tracing::info!(
            tool = $tool,
            version = $version.to_string(),
            target = $addr,
            args = ?$args,
            "starting execution"
        );
    };
    ($tool: literal @ $version: expr, $args: expr) => {
        $crate::__tracing::info!(
            tool = $tool,
            version = $version.to_string(),
            args = ?$args,
            "starting execution"
        );
    };
}

#[macro_export]
macro_rules! summary {
    (path = $path: expr, transforms = $t: expr, sources = $s: expr, farms = $f: expr, locked = $l: expr) => {
        $crate::__tracing::info!(
            path = ?$path,
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
        let __phase: Option<String> = $message;
        $crate::__tracing::info!(
            subsystem = $subsystem $(, component = $component)?, op = $op,
            id = %$id,
            status = %$status,
            phase = ?__phase,
            "task started"
        );
    }};
}

#[macro_export]
macro_rules! ui_update_task {
    (subsystem = $subsystem: expr $(, component = $component: expr)?, op = $op: expr ; $id: expr, $status: expr, $message: expr ) => {{
        let __phase: Option<String> = $message;
        $crate::__tracing::info!(
            subsystem = $subsystem $(, component = $component)?, op = $op,
            id = %$id,
            status = %$status,
            phase = ?__phase,
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
    (subsystem = $subsystem: expr $(, component = $component: literal)? $(, op = $op: literal)?, id = $id: expr ; $($arg:tt)*) => {{
        $crate::__tracing::info!(subsystem = $subsystem $(, component = $component)? $(, op = $op)?, id = %$id, $($arg)*);
    }};
    (subsystem = $subsystem: expr $(, component = $component: literal)? $(, op = $op: literal)? ; $($arg:tt)*) => {{
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
