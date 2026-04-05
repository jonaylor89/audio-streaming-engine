use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use color_eyre::Result;
use futures::Stream;

use crate::blob::AudioBuffer;

/// A stream of byte chunks from storage.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>;

#[async_trait]
pub trait AudioStorage: Send + Sync {
    async fn get(&self, key: &str) -> Result<AudioBuffer>;
    async fn put(&self, key: &str, blob: &AudioBuffer) -> Result<()>;
    async fn delete(&self, key: &str) -> Result<()>;
    async fn list(&self) -> Result<Vec<String>> {
        Ok(vec![])
    }

    /// Stream the object as chunks without buffering the entire file into memory.
    ///
    /// The default implementation falls back to `get()` and emits a single chunk.
    async fn get_stream(&self, key: &str) -> Result<ByteStream> {
        let buf = self.get(key).await?;
        Ok(Box::pin(futures::stream::once(async move {
            Ok(buf.into_bytes())
        })))
    }
}
