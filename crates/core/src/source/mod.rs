/// Git source implementation.
pub mod git;
/// Local filesystem source implementation.
pub mod local;
/// OCI image source implementation.
pub mod oci;
/// Remote URL source implementation.
pub mod remote;

pub use git::GitSource;
pub use local::LocalSource;
pub use oci::ImageSource;
pub use remote::RemoteSource;
