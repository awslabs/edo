use snafu::Snafu;

#[derive(Snafu, Debug)]
#[snafu(visibility(pub))]
pub enum EnvironmentError {
    #[snafu(transparent)]
    Context {
        #[snafu(source(from(crate::context::ContextError, Box::new)))]
        source: Box<crate::context::ContextError>,
    },
    #[snafu(transparent)]
    Implementation {
        source: Box<dyn snafu::Error + Send + Sync>,
    },
    #[snafu(transparent)]
    Plugin {
        #[snafu(source(from(crate::plugin::error::PluginError, Box::new)))]
        source: Box<crate::plugin::error::PluginError>,
    },
    #[snafu(display("command execution failed"))]
    Run,
    #[snafu(transparent)]
    Storage {
        #[snafu(source(from(crate::storage::StorageError, Box::new)))]
        source: Box<crate::storage::StorageError>,
    },
    #[snafu(display("failed to render substitution: {source}"))]
    Template { source: handlebars::RenderError },
}
