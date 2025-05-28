use std::fmt;

use crate::storage::StorageError;
use crate::transform::TransformError;
use snafu::Snafu;

#[derive(Snafu, Debug)]
#[snafu(visibility(pub))]
pub enum PluginError {
    #[snafu(transparent)]
    Cache {
        #[snafu(source(from(crate::storage::StorageError, Box::new)))]
        source: Box<crate::storage::StorageError>,
    },
    #[snafu(transparent)]
    Environment {
        source: crate::environment::EnvironmentError,
    },
    #[snafu(display("io error occured: {source}"))]
    Io { source: std::io::Error },
    #[snafu(transparent)]
    Implementation {
        source: Box<dyn snafu::Error + Send + Sync>,
    },
    #[snafu(display("no plugin loaded with name {name}"))]
    NoPlugin { name: String },
    #[snafu(display("{guest}"))]
    Guest { guest: GuestError },
    #[snafu(transparent)]
    Project {
        #[snafu(source(from(crate::context::ContextError, Box::new)))]
        source: Box<crate::context::ContextError>,
    },
    #[snafu(transparent)]
    Source { source: crate::source::SourceError },
    #[snafu(display("plugin definition is missing a source"))]
    SourceRequired,
    #[snafu(transparent)]
    Transform {
        source: crate::transform::TransformError,
    },
    #[snafu(display("plugin {name} does not support a transform type {kind}"))]
    TransformUnsupported { name: String, kind: String },
    #[snafu(display("unknown plugin kind {kind}"))]
    Unknown { kind: String },
    #[snafu(display("execution in wasm plugin failed: {source}"))]
    WasmExec { source: wasmtime::Error },
    #[snafu(display("failed to operate with object in wasm context: {source}"))]
    WasmContext {
        source: wasmtime::component::ResourceTableError,
    },
    #[snafu(display("failed to load edo plugin: {source}"))]
    WasmPlugin { source: wasmtime::Error },
    #[snafu(display("{message}"))]
    Wrapped { message: String },
}

impl From<PluginError> for TransformError {
    fn from(value: PluginError) -> Self {
        Self::Implementation {
            source: Box::new(value),
        }
    }
}

impl From<PluginError> for StorageError {
    fn from(value: PluginError) -> Self {
        Self::Implementation {
            source: Box::new(value),
        }
    }
}

unsafe impl Send for PluginError {}

#[derive(Clone, Debug)]
pub struct GuestError {
    pub plugin: String,
    pub message: String,
}

impl fmt::Display for GuestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("[{}] {}", self.plugin, self.message))
    }
}

macro_rules! wasm_ok {
    (with $table: expr => $expr: expr) => {
        match $expr {
            Ok(data) => Ok(data),
            Err(e) => Err($table
                .push(crate::plugin::error::GuestError {
                    plugin: "host".to_string(),
                    message: e.to_string(),
                })
                .unwrap()),
        }
    };

    (async $table: expr => $expr: block) => {
        match async move $expr.await {
            Ok(data) => Ok(data),
            Err(e) => Err($table
                .push(crate::plugin::error::GuestError {
                    plugin: "host".to_string(),
                    message: e.to_string(),
                })
                .unwrap()),
        }
    };

    (with $table: expr => $expr: expr; with $result: ident $block: block) => {
        match $expr {
            Ok(data) => {
                #[allow(unused_variables)]
                let $result = data;
                async move $block.await
            },
            Err(e) => Err($table
                .push(crate::plugin::error::GuestError {
                    plugin: "host".to_string(),
                    message: e.to_string(),
                })
                .unwrap()),
        }
    };

    (async $table: expr => $expr: block; with $result: ident $block: block) => {
        match async move $expr.await {
            Ok(data) => {
                #[allow(unused_variables)]
                let $result = data;
                async move $block.await
            },
            Err(e) => Err($table
                .push(crate::plugin::error::GuestError {
                    plugin: "host".to_string(),
                    message: e.to_string(),
                })
                .unwrap()),
        }
    };
}

pub(crate) use wasm_ok;
