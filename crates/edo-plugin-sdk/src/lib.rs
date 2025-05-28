pub mod bindings;
pub mod error;
pub mod stub;

type HostResult<T> = std::result::Result<T, bindings::edo::plugin::host::Error>;
