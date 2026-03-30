use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use color_eyre::Result;
use futures::Stream;
use tokio::sync::Semaphore;
use tracing::{info, instrument};

use crate::{
    blob::AudioBuffer, config::ProcessorSettings,
    processor::ffmpeg::{process_audio, process_audio_streaming},
    streamingpath::params::Params,
};

#[async_trait]
pub trait AudioProcessor: Send + Sync {
    async fn process(&self, blob: &AudioBuffer, params: &Params) -> Result<AudioBuffer>;

    async fn process_streaming(
        &self,
        blob: &AudioBuffer,
        params: &Params,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>>;
}

#[derive(Debug)]
pub struct Processor {
    semaphore: Arc<Semaphore>,
}

#[async_trait]
impl AudioProcessor for Processor {
    #[tracing::instrument(skip(self, blob, params))]
    async fn process(&self, blob: &AudioBuffer, params: &Params) -> Result<AudioBuffer> {
        let _permit = self.semaphore.acquire().await?;
        info!(params = ?params, "Processing with FFmpeg native bindings");

        let processed_audio = process_audio(blob, params).await?;
        info!("Audio processing completed successfully");

        Ok(processed_audio)
    }

    #[tracing::instrument(skip(self, blob, params))]
    async fn process_streaming(
        &self,
        blob: &AudioBuffer,
        params: &Params,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>> {
        let permit = self.semaphore.clone().acquire_owned().await?;
        info!(params = ?params, "Starting streaming FFmpeg processing");
        process_audio_streaming(blob, params, permit).await
    }
}

impl Processor {
    #[instrument(skip(config))]
    pub fn new(config: ProcessorSettings) -> Self {
        let max_concurrent = config
            .concurrency
            .map(|concurrency| {
                NonZeroUsize::new(concurrency).expect("Concurrency should be non-zero")
            })
            .unwrap_or_else(|| {
                NonZeroUsize::new(num_cpus::get())
                    .expect("Number of CPUs should always be non-zero")
            });

        info!(
            max_concurrent = max_concurrent.get(),
            "Initializing processor"
        );

        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent.get())),
        }
    }
}
