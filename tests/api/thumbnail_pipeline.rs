use crate::helpers::{load_fixture_bytes, load_fixture_pcm};
use streaming_engine::blob::AudioBuffer;
use streaming_engine::processor::{AudioProcessor, Processor};
use streaming_engine::streamingpath::params::Params;
use streaming_engine::thumbnail::{ThumbnailConfig, analyze, chroma, ssm};

#[test]
fn analyze_selects_valid_subsection() {
    let pcm = load_fixture_pcm();
    let total_duration = pcm.samples.len() as f64 / pcm.sample_rate as f64;
    let config = ThumbnailConfig::default();

    let result = analyze(&pcm.samples, pcm.sample_rate, &config).unwrap();

    eprintln!(
        "Thumbnail: start={:.2}s, duration={:.2}s, confidence={:.3} (track={:.2}s)",
        result.start_time, result.duration, result.confidence, total_duration,
    );

    assert!(result.start_time >= 0.0);
    assert!(result.duration > 0.0);
    assert!(result.start_time + result.duration <= total_duration + 0.5);
    assert!(result.confidence > 0.0 && result.confidence <= 1.0);

    // Must be a genuine sub-section, not the whole track
    assert!(
        result.duration < total_duration * 0.9,
        "thumbnail {:.2}s is too close to full track {:.2}s",
        result.duration,
        total_duration,
    );
}

#[test]
fn analyze_respects_custom_duration_bounds() {
    let pcm = load_fixture_pcm();
    let total_duration = pcm.samples.len() as f64 / pcm.sample_rate as f64;
    let config = ThumbnailConfig {
        target_duration: 10.0,
        min_duration: 5.0,
        max_duration: 15.0,
    };

    let result = analyze(&pcm.samples, pcm.sample_rate, &config).unwrap();

    if total_duration > config.min_duration {
        let effective_max = config.max_duration.min(total_duration * 0.75);
        assert!(
            result.duration >= config.min_duration - 0.5,
            "duration {:.2}s below min {:.2}s",
            result.duration,
            config.min_duration,
        );
        assert!(
            result.duration <= effective_max + 0.5,
            "duration {:.2}s above effective max {:.2}s",
            result.duration,
            effective_max,
        );
    }
}

#[test]
fn chroma_extracts_non_zero_energy() {
    let pcm = load_fixture_pcm();
    let hop_size = pcm.sample_rate as usize / 2;
    let features = chroma::extract_chroma(&pcm.samples, pcm.sample_rate, hop_size);
    let num_frames = features.len() / 12;

    assert!(num_frames > 0, "should produce at least one chroma frame");

    let non_zero = (0..num_frames)
        .filter(|&f| features[f * 12..(f + 1) * 12].iter().sum::<f32>() > 0.01)
        .count();

    eprintln!("Chroma: {}/{} frames with energy", non_zero, num_frames);
    assert!(
        non_zero > num_frames / 2,
        "majority of frames should have energy, got {}/{}",
        non_zero,
        num_frames,
    );
}

#[test]
fn ssm_has_diagonal_ones_and_structural_variation() {
    let pcm = load_fixture_pcm();
    let hop_size = pcm.sample_rate as usize / 2;
    let features = chroma::extract_chroma(&pcm.samples, pcm.sample_rate, hop_size);
    let n = features.len() / 12;
    let matrix = ssm::build_ssm(&features, n);

    // Diagonal must be 1.0
    for i in 0..n {
        assert!(
            (matrix[i * n + i] - 1.0).abs() < 1e-5,
            "diagonal [{i},{i}] = {}",
            matrix[i * n + i],
        );
    }

    // Off-diagonal should show real variation (not uniform)
    let off: Vec<f32> = (0..n)
        .flat_map(|i| (0..n).filter(move |&j| i != j).map(move |j| (i, j)))
        .map(|(i, j)| matrix[i * n + j])
        .collect();

    let min = off.iter().cloned().fold(f32::INFINITY, f32::min);
    let max = off.iter().cloned().fold(f32::NEG_INFINITY, f32::max);

    eprintln!("SSM off-diagonal: min={min:.3}, max={max:.3}");
    assert!(
        max - min > 0.05,
        "SSM has no variation ({min:.3}..{max:.3})"
    );
}

/// End-to-end: analyze → process with start_time/duration → verify output is audible.
/// This reproduces the exact flow of the /thumbnail handler.
#[tokio::test]
async fn processed_thumbnail_audio_is_not_silent() {
    let fixture_bytes = load_fixture_bytes();
    let blob = AudioBuffer::from_bytes(fixture_bytes.to_vec());

    // Step 1: decode + analyze (same as handler)
    let pcm = ffmpeg::decode_to_pcm(fixture_bytes).unwrap();
    let config = ThumbnailConfig::default();
    let result = analyze(&pcm.samples, pcm.sample_rate, &config).unwrap();
    eprintln!(
        "Analysis: start={:.2}s, duration={:.2}s, confidence={:.3}",
        result.start_time, result.duration, result.confidence,
    );

    // Step 2: process with start_time + duration (same as handler)
    let params = Params {
        key: "sample1.mp3".to_string(),
        start_time: Some(result.start_time),
        duration: Some(result.duration),
        ..Default::default()
    };

    let processor = Processor::new(streaming_engine::config::ProcessorSettings {
        disabled_filters: Vec::new(),
        max_filter_ops: 100,
        concurrency: Some(1),
        max_cache_files: 1000,
        max_cache_mem: 100 * 1024 * 1024,
        max_cache_size: 1024 * 1024 * 1024,
        ..Default::default()
    });

    let processed = processor.process(&blob, &params).await.unwrap();
    eprintln!(
        "Processed: {} bytes, format={:?}",
        processed.len(),
        processed.format(),
    );
    assert!(
        processed.len() > 100,
        "output too small: {} bytes",
        processed.len()
    );

    // Step 3: decode the OUTPUT back to PCM and check it's not silent
    let output_pcm = ffmpeg::decode_to_pcm(processed.into_bytes()).unwrap();
    let out_duration = output_pcm.samples.len() as f64 / output_pcm.sample_rate as f64;
    let rms: f64 = (output_pcm
        .samples
        .iter()
        .map(|s| (*s as f64) * (*s as f64))
        .sum::<f64>()
        / output_pcm.samples.len() as f64)
        .sqrt();

    eprintln!(
        "Output PCM: {:.2}s, {} samples, RMS={:.4}",
        out_duration,
        output_pcm.samples.len(),
        rms,
    );

    assert!(out_duration > 1.0, "output too short: {:.2}s", out_duration);
    assert!(rms > 0.001, "output is silent (RMS={:.6})", rms);
}
