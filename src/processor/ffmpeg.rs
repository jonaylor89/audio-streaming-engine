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
    is_passthrough_for_format(input.format(), params)
}

/// Check if a request is a passthrough given only the source format.
/// Useful for deciding to stream directly from storage without buffering.
pub fn is_passthrough_for_format(format: AudioFormat, params: &Params) -> bool {
    params.format.is_none_or(|f| f == format)
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
        // afade=t=out defaults st=0, which fades from the START — producing silence.
        // We need st=(output_duration - fade) so the fade ends at the clip boundary.
        if let Some(dur) = params.duration {
            let st = (dur - fade).max(0.0);
            filters.push(format!("afade=t=out:st={:.3}:d={:.3}", st, fade));
        } else {
            // No known duration: use FFmpeg's EOF-relative reverse-fade trick.
            // areverse → afade=t=in → areverse gives a fade-out at the true end.
            filters.push("areverse".to_string());
            filters.push(format!("afade=t=in:d={:.3}", fade));
            filters.push("areverse".to_string());
        }
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
) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>> {
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

    // Bounded channel: FFmpeg's thread blocks in `blocking_send` when the HTTP
    // consumer is slow, preventing unbounded buffering. Mid-stream FFmpeg
    // errors are forwarded as an `Err` item so consumers can tell a truncated
    // stream from a complete one.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes>>(8);
    let err_tx = tx.clone();

    // FFmpeg pipeline runs in spawn_blocking; the semaphore permit is held for
    // the duration of FFmpeg processing.
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let result = ffmpeg::AudioProcessor::new().and_then(|processor| {
            processor.process_streaming(
                ffmpeg::ProcessOptions {
                    input: input_bytes,
                    output_format: output_spec,
                    filters,
                    metadata: &metadata,
                    start_time,
                    duration,
                },
                // Returns false once the receiver is dropped (client
                // disconnected), which makes FFmpeg abort.
                Box::new(move |chunk| tx.blocking_send(Ok(chunk)).is_ok()),
            )
        });
        if let Err(e) = result {
            tracing::error!("FFmpeg streaming error: {}", e);
            let _ = err_tx.blocking_send(Err(e.into()));
        }
    });

    let stream = futures::stream::unfold(rx, |mut rx| async move {
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

    #[test]
    fn test_fade_out_with_duration_sets_start_time() {
        let params = Params {
            fade_out: Some(1.0),
            duration: Some(30.0),
            ..Default::default()
        };
        let filters = collect_filters(&params).unwrap();
        assert!(
            filters.contains("afade=t=out:st=29.000:d=1.000"),
            "expected st=29s for 30s clip with 1s fade, got: {}",
            filters,
        );
    }

    #[test]
    fn test_fade_out_without_duration_uses_reverse_trick() {
        let params = Params {
            fade_out: Some(2.0),
            ..Default::default()
        };
        let filters = collect_filters(&params).unwrap();
        assert!(
            filters.contains("areverse") && filters.contains("afade=t=in:d=2.000"),
            "expected areverse+fade_in+areverse trick, got: {}",
            filters,
        );
    }

    /// Mono 16-bit WAV of `secs` seconds: silent except a 440 Hz tone
    /// between `tone_from` and `tone_to` seconds.
    fn wav_with_tone(secs: u32, tone_from: f32, tone_to: f32) -> AudioBuffer {
        const RATE: u32 = 44_100;
        let n = (RATE * secs) as usize;
        let mut wav = Vec::with_capacity(44 + n * 2);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + n as u32 * 2).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&RATE.to_le_bytes());
        wav.extend_from_slice(&(RATE * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(n as u32 * 2).to_le_bytes());
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            let s = if (tone_from..tone_to).contains(&t) {
                (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5
            } else {
                0.0
            };
            wav.extend_from_slice(&((s * i16::MAX as f32) as i16).to_le_bytes());
        }
        AudioBuffer::from_bytes_with_format(wav, AudioFormat::Wav)
    }

    /// Returns (duration in seconds, RMS) of an encoded clip.
    fn duration_and_rms(clip: &AudioBuffer) -> (f64, f32) {
        let pcm = ffmpeg::decode_to_pcm(clip.bytes()).unwrap();
        let rms =
            (pcm.samples.iter().map(|s| s * s).sum::<f32>() / pcm.samples.len() as f32).sqrt();
        (pcm.samples.len() as f64 / pcm.sample_rate as f64, rms)
    }

    /// Trimming seeks into the source instead of decoding from the start; the
    /// clip must still come from the requested position.
    #[tokio::test]
    async fn test_trim_after_seek_selects_requested_range() {
        let source = process_audio(
            &wav_with_tone(60, 40.0, 45.0),
            &Params {
                format: Some(AudioFormat::Mp3),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let trim = |start: f64| Params {
            format: Some(AudioFormat::Mp3),
            start_time: Some(start),
            duration: Some(5.0),
            ..Default::default()
        };

        let (duration, rms) = duration_and_rms(&process_audio(&source, &trim(40.0)).await.unwrap());
        assert!(
            (duration - 5.0).abs() < 0.2,
            "expected ~5s clip, got {duration}s"
        );
        assert!(rms > 0.2, "clip at 40s should contain the tone, rms={rms}");

        let (duration, rms) = duration_and_rms(&process_audio(&source, &trim(30.0)).await.unwrap());
        assert!(
            (duration - 5.0).abs() < 0.2,
            "expected ~5s clip, got {duration}s"
        );
        assert!(rms < 0.01, "clip at 30s should be silent, rms={rms}");
    }
}
