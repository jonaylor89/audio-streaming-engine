pub mod chroma;
mod fitness;
pub mod ssm;

use color_eyre::Result;
use tracing::{info, instrument};

/// Result of thumbnail analysis.
#[derive(Debug, Clone)]
pub struct ThumbnailResult {
    /// Start time in seconds
    pub start_time: f64,
    /// Duration in seconds
    pub duration: f64,
    /// Confidence score (0.0 to 1.0)
    pub confidence: f64,
}

/// Configuration for thumbnail analysis.
#[derive(Debug, Clone)]
pub struct ThumbnailConfig {
    /// Target thumbnail duration in seconds
    pub target_duration: f64,
    /// Minimum thumbnail duration in seconds
    pub min_duration: f64,
    /// Maximum thumbnail duration in seconds
    pub max_duration: f64,
}

impl Default for ThumbnailConfig {
    fn default() -> Self {
        Self {
            target_duration: 30.0,
            min_duration: 15.0,
            max_duration: 45.0,
        }
    }
}

/// Analyze audio samples and find the best thumbnail segment.
///
/// Implements the classical approach: chroma features → self-similarity matrix → fitness measure.
/// Beat-synchronous framing reduces the chroma frame count to keep the fitness sweep fast.
#[instrument(skip(samples))]
pub fn analyze(
    samples: &[f32],
    sample_rate: i32,
    config: &ThumbnailConfig,
) -> Result<ThumbnailResult> {
    let total_duration = samples.len() as f64 / sample_rate as f64;
    info!(
        total_duration,
        sample_rate,
        num_samples = samples.len(),
        "Starting thumbnail analysis"
    );

    // For very short tracks, just return the whole thing
    if total_duration <= config.min_duration {
        return Ok(ThumbnailResult {
            start_time: 0.0,
            duration: total_duration,
            confidence: 1.0,
        });
    }

    // Cap max duration to 75% of track length so we always select a sub-section,
    // not the whole track. The thumbnail should be a preview, not the full song.
    let effective_max = config.max_duration.min(total_duration * 0.75);
    let effective_target = config.target_duration.min(effective_max);
    let effective_min = config.min_duration.min(effective_max);

    // Step 1: Extract chroma features with beat-synchronous framing
    // Use a hop size that gives us roughly 2 frames/sec (simulating ~120 BPM beat sync)
    let hop_size = sample_rate as usize / 2;
    let chroma = chroma::extract_chroma(samples, sample_rate, hop_size);
    let num_frames = chroma.len() / 12;
    info!(num_frames, hop_size, "Chroma features extracted");

    if num_frames < 4 {
        return Ok(ThumbnailResult {
            start_time: 0.0,
            duration: total_duration.min(config.target_duration),
            confidence: 0.5,
        });
    }

    // Step 2: Build self-similarity matrix
    let ssm = ssm::build_ssm(&chroma, num_frames);
    info!(
        "Self-similarity matrix built ({}x{})",
        num_frames, num_frames
    );

    // Step 3: Find best segment using fitness measure
    let frame_duration = hop_size as f64 / sample_rate as f64;
    let min_frames = (effective_min / frame_duration).ceil() as usize;
    let max_frames = (effective_max / frame_duration).floor() as usize;
    let target_frames = (effective_target / frame_duration).round() as usize;

    let min_frames = min_frames.max(1).min(num_frames);
    let max_frames = max_frames.max(min_frames).min(num_frames);
    let target_frames = target_frames.clamp(min_frames, max_frames);

    let (best_start, best_len, confidence) =
        fitness::find_best_segment(&ssm, num_frames, min_frames, max_frames, target_frames);

    let start_time = best_start as f64 * frame_duration;
    let duration = best_len as f64 * frame_duration;

    // Clamp to not exceed track bounds
    let duration = duration.min(total_duration - start_time);

    info!(start_time, duration, confidence, "Thumbnail analysis complete");

    Ok(ThumbnailResult {
        start_time,
        duration,
        confidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_track_returns_whole() {
        let sample_rate = 44100;
        let samples = vec![0.0f32; sample_rate as usize * 10]; // 10 seconds
        let config = ThumbnailConfig {
            min_duration: 15.0,
            ..Default::default()
        };
        let result = analyze(&samples, sample_rate, &config).unwrap();
        assert_eq!(result.start_time, 0.0);
        assert!((result.duration - 10.0).abs() < 0.1);
        assert_eq!(result.confidence, 1.0);
    }

    #[test]
    fn test_analyze_returns_valid_bounds() {
        let sample_rate = 44100;
        // 3 minutes of audio (sine wave with repeating pattern)
        let num_samples = sample_rate as usize * 180;
        let samples: Vec<f32> = (0..num_samples)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                let phase = (t % 30.0) / 30.0;
                (phase * 2.0 * std::f32::consts::PI * 440.0).sin()
            })
            .collect();

        let config = ThumbnailConfig::default();
        let result = analyze(&samples, sample_rate, &config).unwrap();

        assert!(result.start_time >= 0.0);
        assert!(result.duration >= config.min_duration);
        assert!(result.duration <= config.max_duration);
        assert!(result.start_time + result.duration <= 180.5);
        assert!(result.confidence >= 0.0 && result.confidence <= 1.0);
    }
}
