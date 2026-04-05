mod backend;
pub mod cached;
#[cfg(feature = "filesystem")]
pub mod file;
#[cfg(feature = "gcs")]
pub mod gcs;
#[cfg(feature = "s3")]
pub mod s3;

pub use backend::{AudioStorage, ByteStream};
pub use cached::CachedStorage;
