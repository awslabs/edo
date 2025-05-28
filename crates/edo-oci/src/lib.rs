#[macro_use]
extern crate tracing;

pub(crate) mod client;
pub mod compression;
pub mod error;
pub mod image;
pub mod index;
pub mod layer;
pub mod models;
pub mod registry;
pub mod repository;
pub mod uri;

pub type Result<T> = std::result::Result<T, error::Error>;
