pub mod git;
pub mod local;
pub mod oci;
pub mod remote;
pub mod vendor;

pub use git::GitSource;
pub use local::LocalSource;
pub use oci::ImageSource;
pub use remote::RemoteSource;
pub use vendor::VendorSource;
