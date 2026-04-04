//! Safe Rust wrapper for FFmpeg audio processing.
//!
//! This crate provides a high-level API for audio transcoding and filtering
//! using FFmpeg's libav* libraries with in-memory buffers.

mod error;
mod handle;
mod io;
pub mod metadata;
mod pcm;
mod pipeline;

pub use error::FfmpegError;
pub use metadata::{extract_metadata, AudioFileMetadata};
pub use pcm::{decode_to_pcm, PcmData};
pub use pipeline::{AudioProcessor, OutputFormat, ProcessOptions};

use std::sync::Once;

static INIT: Once = Once::new();

/// Initialize FFmpeg. Called automatically when creating an AudioProcessor.
pub fn init() {
    INIT.call_once(|| {
        unsafe {
            // Suppress noisy FFmpeg warnings (e.g. mp3float timestamp warnings).
            // Only errors and above will be printed.
            ffmpeg_sys::av_log_set_level(ffmpeg_sys::AV_LOG_ERROR as libc::c_int);
        }

        // In modern FFmpeg (4.0+), av_register_all() is deprecated and no-op.
        // Network initialization is still needed for some protocols.
        #[cfg(feature = "network")]
        unsafe {
            ffmpeg_sys::avformat_network_init();
        }
    });
}
