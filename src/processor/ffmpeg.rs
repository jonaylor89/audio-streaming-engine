use std::pin::Pin;

use bytes::Bytes;
use color_eyre::Result;
use futures::Stream;
use tracing::instrument;

use crate::{
    blob::{AudioBuffer, AudioFormat},
    streamingpath::params::Params,
};

use ffmpeg::{AudioProcessor, OutputFormat, ProcessOptions};

fn is_passthrough_request(input: &AudioBuffer, params: &Params) -> bool {
    params.format.is_none_or(|format| format == input.format())
        && params.codec.is_none()
        && params.sample_rate.is_none()
        && params.channels.is_none()
        && params.bit_rate.is_none()
        && params.bit_depth.is_none()
        && params.quality.is_none()
        && params.compression_level.is_none()
        && params.start_time.is_none()
        && params.duration.is_none()
        && params.tags.as_ref().is_none_or(|tags| tags.is_empty())
        && collect_filters(params).is_none()
}

/// Convert AudioFormat to output format specification.
fn audio_format_to_output(format: AudioFormat, params: &Params) -> OutputFormat {
    let mut output = OutputFormat::from_extension(format.extension());

    // Override codec if specified
    if let Some(ref codec) = params.codec {
        output.codec = Some(codec.clone());
    }

    // Apply other parameters
    output.sample_rate = params.sample_rate;
    output.channels = params.channels;
    output.bit_rate = params.bit_rate.map(|r| r as i64 * 1000); // Convert kbps to bps
    output.quality = params.quality.map(|q| q as f32);
    output.compression_level = params.compression_level;

    output
}

/// Convert Params filters to FFmpeg filter string.
fn collect_filters(params: &Params) -> Option<String> {
    let mut filters = Vec::new();

    if let Some(speed) = params.speed
        && speed != 1.0
    {
        filters.push(format!("atempo={:.3}", speed));
    }
    if let Some(true) = params.reverse {
        filters.push("areverse".to_string());
    }
    if let Some(volume) = params.volume
        && volume != 1.0
    {
        filters.push(format!("volume={:.2}", volume));
    }
    if let Some(true) = params.normalize {
        let level = params.normalize_level.unwrap_or(-16.0);
        filters.push(format!("loudnorm=I={:.1}", level));
    }
    if let Some(freq) = params.lowpass {
        filters.push(format!("lowpass=f={:.1}", freq));
    }
    if let Some(freq) = params.highpass {
        filters.push(format!("highpass=f={:.1}", freq));
    }
    if let Some(band) = &params.bandpass {
        filters.push(format!("bandpass={}", band));
    }
    if let Some(bass) = params.bass {
        filters.push(format!("bass=g={:.1}", bass));
    }
    if let Some(treble) = params.treble {
        filters.push(format!("treble=g={:.1}", treble));
    }
    if let Some(echo) = &params.echo {
        filters.push(format!("aecho={}", echo));
    }
    if let Some(chorus) = &params.chorus {
        filters.push(format!("chorus={}", chorus));
    }
    if let Some(flanger) = &params.flanger {
        filters.push(format!("flanger={}", flanger));
    }
    if let Some(phaser) = &params.phaser {
        filters.push(format!("aphaser={}", phaser));
    }
    if let Some(tremolo) = &params.tremolo {
        filters.push(format!("tremolo={}", tremolo));
    }
    if let Some(compressor) = &params.compressor {
        filters.push(format!("acompressor={}", compressor));
    }
    if let Some(nr) = &params.noise_reduction {
        filters.push(format!("anlmdn={}", nr));
    }
    if let Some(fade) = params.fade_in {
        filters.push(format!("afade=t=in:d={:.3}", fade));
    }
    if let Some(fade) = params.fade_out {
        filters.push(format!("afade=t=out:d={:.3}", fade));
    }
    if let Some(fade) = params.cross_fade {
        filters.push(format!("acrossfade=d={:.3}", fade));
    }

    if let Some(custom_filters) = &params.custom_filters {
        filters.extend(custom_filters.clone());
    }

    if filters.is_empty() {
        None
    } else {
        Some(filters.join(","))
    }
}

#[instrument(skip(input, params))]
pub async fn process_audio(input: &AudioBuffer, params: &Params) -> Result<AudioBuffer> {
    if is_passthrough_request(input, params) {
        return Ok(input.clone());
    }

    let output_format = params.format.unwrap_or(AudioFormat::Mp3);

    // Combine tags
    let metadata = params.tags.clone().unwrap_or_default();

    // Collect filters
    let filters = collect_filters(params);

    // Build output format
    let output_spec = audio_format_to_output(output_format, params);

    // Get input data (Bytes::clone is just an Arc bump — zero-copy)
    let input_bytes = input.clone().into_bytes();
    let start_time = params.start_time;
    let duration = params.duration;

    // Process in blocking task since FFmpeg is CPU-bound
    let processed = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, ffmpeg::FfmpegError> {
        let processor = AudioProcessor::new()?;
        processor.process(ProcessOptions {
            input: input_bytes,
            output_format: output_spec,
            filters,
            metadata: &metadata,
            start_time,
            duration,
        })
    })
    .await??;

    Ok(AudioBuffer::from_bytes_with_format(
        processed,
        output_format,
    ))
}

#[instrument(skip(input, params, permit))]
pub async fn process_audio_streaming(
    input: &AudioBuffer,
    params: &Params,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> color_eyre::Result<Pin<Box<dyn Stream<Item = color_eyre::Result<Bytes>> + Send>>> {
    use std::sync::mpsc;

    if is_passthrough_request(input, params) {
        let bytes = input.clone().into_bytes();
        return Ok(Box::pin(futures::stream::once(async move { Ok(bytes) })));
    }

    let output_format = params.format.unwrap_or(AudioFormat::Mp3);
    let metadata = params.tags.clone().unwrap_or_default();
    let filters = collect_filters(params);
    let output_spec = audio_format_to_output(output_format, params);
    let input_bytes = input.clone().into_bytes();
    let start_time = params.start_time;
    let duration = params.duration;

    // Bounded sync channel: provides backpressure so FFmpeg blocks when the
    // HTTP consumer is slow, preventing unbounded buffering.
    let (std_tx, std_rx) = mpsc::sync_channel::<Bytes>(8);

    // Bridge: drain the std::sync channel from a spawn_blocking task and forward
    // to a tokio channel that the async stream can consume.
    let (tokio_tx, tokio_rx) = tokio::sync::mpsc::channel::<color_eyre::Result<Bytes>>(8);
    let bridge_tx = tokio_tx;
    tokio::task::spawn_blocking(move || {
        while let Ok(chunk) = std_rx.recv() {
            if bridge_tx.blocking_send(Ok(chunk)).is_err() {
                // Receiver dropped (client disconnected) — stop bridge.
                return;
            }
        }
        // std_rx.recv() returning Err means std_tx was dropped: FFmpeg finished.
    });

    // FFmpeg pipeline runs in spawn_blocking; sends chunks via std_tx.
    // The semaphore permit is held for the duration of FFmpeg processing.
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let processor = match ffmpeg::AudioProcessor::new() {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("Failed to create FFmpeg processor: {}", e);
                // std_tx dropped here → bridge sees EOF → stream ends
                return;
            }
        };
        if let Err(e) = processor.process_streaming(
            ffmpeg::ProcessOptions {
                input: input_bytes,
                output_format: output_spec,
                filters,
                metadata: &metadata,
                start_time,
                duration,
            },
            std_tx,
        ) {
            // process_streaming returned an error. std_tx has already been moved
            // into StreamingOutputContext (and dropped on return), so the bridge
            // sees EOF. The stream will end without an explicit error item.
            // This means mid-stream errors cause a truncated response — acceptable
            // for the initial streaming implementation.
            tracing::error!("FFmpeg streaming error: {}", e);
        }
    });

    let stream = futures::stream::unfold(tokio_rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });

    Ok(Box::pin(stream))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collect_filters_empty() {
        let params = Params::default();
        assert!(collect_filters(&params).is_none());
    }

    #[test]
    fn test_collect_filters_volume() {
        let params = Params {
            volume: Some(0.5),
            ..Default::default()
        };
        assert_eq!(collect_filters(&params), Some("volume=0.50".to_string()));
    }

    #[test]
    fn test_collect_filters_multiple() {
        let params = Params {
            volume: Some(0.8),
            speed: Some(1.5),
            lowpass: Some(8000.0),
            ..Default::default()
        };
        let filters = collect_filters(&params).unwrap();
        assert!(filters.contains("atempo=1.500"));
        assert!(filters.contains("volume=0.80"));
        assert!(filters.contains("lowpass=f=8000.0"));
    }

    #[test]
    fn test_audio_format_to_output() {
        let params = Params {
            bit_rate: Some(320),
            sample_rate: Some(48000),
            ..Default::default()
        };
        let output = audio_format_to_output(AudioFormat::Mp3, &params);
        assert_eq!(output.format, "mp3");
        assert_eq!(output.codec, Some("libmp3lame".to_string()));
        assert_eq!(output.bit_rate, Some(320_000));
        assert_eq!(output.sample_rate, Some(48000));
    }

    #[test]
    fn test_passthrough_request_same_format_no_work() {
        let input =
            AudioBuffer::from_bytes_with_format(vec![0xFF, 0xFB, 0x90, 0x00], AudioFormat::Mp3);
        let params = Params::default();

        assert!(is_passthrough_request(&input, &params));
    }

    #[test]
    fn test_passthrough_request_rejects_transcode() {
        let input =
            AudioBuffer::from_bytes_with_format(vec![0xFF, 0xFB, 0x90, 0x00], AudioFormat::Mp3);
        let params = Params {
            format: Some(AudioFormat::Wav),
            ..Default::default()
        };

        assert!(!is_passthrough_request(&input, &params));
    }
}
