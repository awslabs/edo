//! `#[macro_export]` sugar for emitting UI actions from anywhere in the
//! crate (and from `crates/core`). The macros defer `format!` into the
//! `if let Some(c) = CONSOLE.get()` block so no heap allocation happens
//! when no console is installed (tests, ad-hoc tooling).
//!
//! Public surface: `header!`, `summary!`, `ui_start_build!`,
//! `ui_trace!`, `ui_debug!`, `ui_info!`, `ui_warn!`, `ui_error!`,
//! `ui_fatal!`. Every macro is unchanged from the previous
//! implementation — the split is purely a file-organisation change.

#[macro_export]
macro_rules! header {
    ($tool: literal @ $version: expr) => {
        tracing::info!(
            tool = $tool,
            version = $version.to_string(),
            "starting execution"
        );
        if let Some(c) = $crate::ui::CONSOLE.get() {
            c.emit_header($tool, $version, None, Vec::default()).await;
        }
    };
    ($tool: literal @ $version: expr => $addr: expr) => {
        tracing::info!(
            tool = $tool,
            version = $version.to_string(),
            target = $addr,
            "starting execution"
        );
        if let Some(c) = $crate::ui::CONSOLE.get() {
            c.emit_header(
                $tool,
                $version,
                $crate::context::Addr::parse($addr).ok(),
                Vec::default(),
            )
            .await;
        }
    };
    ($tool: literal @ $version: expr => $addr: expr, $args: expr) => {
        tracing::info!(
            tool = $tool,
            version = $version.to_string(),
            target = $addr,
            "starting execution"
        );
        if let Some(c) = $crate::ui::CONSOLE.get() {
            c.emit_header(
                $tool,
                $version,
                $crate::context::Addr::parse($addr).ok(),
                $args,
            )
            .await;
        }
    };
    ($tool: literal @ $version: expr, $args: expr) => {
        tracing::info!(
            tool = $tool,
            version = $version.to_string(),
            "starting execution"
        );
        if let Some(c) = $crate::ui::CONSOLE.get() {
            c.emit_header($tool, $version, None, $args).await;
        }
    };
}

#[macro_export]
macro_rules! summary {
    (path = $path: expr, transforms = $t: expr, sources = $s: expr, farms = $f: expr, locked = $l: expr) => {
        tracing::info!(
            path = $path,
            transforms = $t,
            sources = $s,
            farms = $f,
            locked = $l,
            "project loaded"
        );
        if let Some(c) = $crate::ui::CONSOLE.get() {
            c.emit_summary($path, $t, $s, $f, $l).await;
        }
    };
}

#[macro_export]
macro_rules! ui_start_task {
    (subsystem = $subsystem: expr $(, component = $component: expr)?, op = $op: expr ; $id: expr, $status: expr, $message: expr ) => {
        tracing::trace!(
            subsystem = $subsystem $(, component = $component)?, op = $op,
            id = %$id,
           "starting task"
        );
        if let Some(c) = $crate::ui::CONSOLE.get() {
            c.start_task(
                $subsystem,
                $id,
                $op,
                $status,
                $message
            ).await;
        }
    };
}

#[macro_export]
macro_rules! ui_update_task {
    (subsystem = $subsystem: expr $(, component = $component: expr)?, op = $op: expr ; $id: expr, $status: expr, $message: expr ) => {
        tracing::trace!(
            subsystem = $subsystem $(, component = $component)?, op = $op,
            id = %$id,
           "starting task"
        );
        if let Some(c) = $crate::ui::CONSOLE.get() {
            c.update_task(
                $subsystem,
                $id,
                $op,
                $status,
                $message
            ).await;
        }
    };
}

#[macro_export]
macro_rules! ui_start_build {
    ($addr: expr, $count: expr) => {
        tracing::info!(addr = $addr, total = $count, "starting build");
        if let Some(c) = $crate::ui::CONSOLE.get() {
            c.start_build($addr, $count).await;
        }
    };
}

#[macro_export]
macro_rules! ui_finish_build {
    () => {
        tracing::info!("finishing build");
        if let Some(c) = $crate::ui::CONSOLE.get() {
            c.finish_build().await;
        }
    };
}

#[macro_export]
macro_rules! ui_info {
    (subsystem = $subsystem: expr $(, component = $component: literal)? $(, op = $op: literal)?, id = $id: expr ; $($arg:tt)*) => {{
        tracing::info!(subsystem = $subsystem $(, component = $component)? $(, op = $op)?, id = %$id, $($arg)*);
        if let Some(c) = $crate::ui::CONSOLE.get() {
            let msg = format!($($arg)*);
            c.emit_diagnostic(
                $subsystem,
                Some($id.to_string()),
                $crate::ui::action::Severity::Info,
                &msg,
            ).await;
        }
    }};
    (subsystem = $subsystem: expr $(, component = $component: literal)? $(, op = $op: literal)? ; $($arg:tt)*) => {{
        tracing::info!(subsystem = $subsystem $(, component = $component)? $(, op = $op)?, $($arg)*);
        if let Some(c) = $crate::ui::CONSOLE.get() {
            let msg = format!($($arg)*);
            c.emit_diagnostic(
                $subsystem,
                None,
                $crate::ui::action::Severity::Info,
                &msg,
            ).await;
        }
    }};
}

#[macro_export]
macro_rules! ui_warn {
    (subsystem = $subsystem: expr $(, component = $component: expr)? $(, op = $op: expr)?, id = $id: expr ; $($arg:tt)*) => {{
        tracing::warn!(subsystem = $subsystem $(, component = $component)? $(, op = $op)?, id = %$id, $($arg)*);
        if let Some(c) = $crate::ui::CONSOLE.get() {
            let msg = format!($($arg)*);
            c.emit_diagnostic(
                $subsystem,
                Some($id.to_string()),
                $crate::ui::action::Severity::Warn,
                &msg,
            ).await;
        }
    }};
    (subsystem = $subsystem: expr $(, component = $component: expr)? $(, op = $op: expr)? ; $($arg:tt)*) => {{
        tracing::warn!(subsystem = $subsystem $(, component = $component)? $(, op = $op)?, $($arg)*);
        if let Some(c) = $crate::ui::CONSOLE.get() {
            let msg = format!($($arg)*);
            c.emit_diagnostic(
                $subsystem,
                None,
                $crate::ui::action::Severity::Warn,
                &msg,
            ).await;
        }
    }};
}

#[macro_export]
macro_rules! ui_error {
    (subsystem = $subsystem: expr $(, component = $component: expr)? $(, op = $op: expr)?, id = $id: expr ; $($arg:tt)*) => {{
        tracing::error!(subsystem = $subsystem $(, component = $component)? $(, op = $op)?, id = %$id, $($arg)*);
        if let Some(c) = $crate::ui::CONSOLE.get() {
            let msg = format!($($arg)*);
            c.emit_diagnostic(
                $subsystem,
                Some($id.to_string()),
                $crate::ui::action::Severity::Error,
                &msg,
            ).await;
        }
    }};
    (subsystem = $subsystem: expr $(, component = $component: expr)? $(, op = $op: expr)? ; $($arg:tt)*) => {{
        tracing::error!(subsystem = $subsystem $(, component = $component)? $(, op = $op)?, $($arg)*);
        if let Some(c) = $crate::ui::CONSOLE.get() {
            let msg = format!($($arg)*);
            c.emit_diagnostic(
                $subsystem,
                None,
                $crate::ui::action::Severity::Error,
                &msg,
            ).await;
        }
    }};
}

#[macro_export]
macro_rules! ui_fatal {
    (subsystem = $subsystem: expr $(, component = $component: expr)? $(, op = $op: expr)?, id = $id: expr ; $($arg:tt)*) => {{
        tracing::error!(subsystem = $subsystem $(, component = $component)? $(, op = $op)?, id = %$id, $($arg)*);
        if let Some(c) = $crate::ui::CONSOLE.get() {
            let msg = format!($($arg)*);
            c.emit_diagnostic(
                $subsystem,
                Some($id.to_string()),
                $crate::ui::action::Severity::Fatal,
                &msg,
            ).await;
        }
    }};
    (subsystem = $subsystem: expr $(, component = $component: expr)? $(, op = $op: expr)? ; $($arg:tt)*) => {{
        tracing::error!(subsystem = $subsystem $(, component = $component)? $(, op = $op)?, $($arg)*);
        if let Some(c) = $crate::ui::CONSOLE.get() {
            let msg = format!($($arg)*);
            c.emit_diagnostic(
                $subsystem,
                None,
                $crate::ui::action::Severity::Fatal,
                &msg,
            ).await;
        }
    }};
}
