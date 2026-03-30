use crate::helpers::load_fixture_bytes;

#[test]
fn decode_fixture_to_valid_pcm() {
    let data = load_fixture_bytes();
    let pcm = ffmpeg::decode_to_pcm(data).expect("decode_to_pcm should succeed");

    // Sample rate
    let standard_rates = [8000, 11025, 16000, 22050, 32000, 44100, 48000, 96000];
    assert!(
        standard_rates.contains(&pcm.sample_rate),
        "unexpected sample rate: {}",
        pcm.sample_rate,
    );

    // Duration
    let duration = pcm.samples.len() as f64 / pcm.sample_rate as f64;
    assert!(duration > 1.0, "expected > 1s of audio, got {:.2}s", duration);

    // No NaN/Inf
    assert!(
        pcm.samples.iter().all(|s| s.is_finite()),
        "found non-finite samples",
    );

    // Amplitudes in sane range (f32 PCM can slightly exceed ±1.0)
    assert!(
        pcm.samples.iter().all(|s| s.abs() <= 2.0),
        "found samples outside [-2.0, 2.0]",
    );

    // Not silent — check RMS energy
    let rms: f64 = (pcm
        .samples
        .iter()
        .map(|s| (*s as f64) * (*s as f64))
        .sum::<f64>()
        / pcm.samples.len() as f64)
        .sqrt();
    assert!(rms > 0.001, "RMS {:.6} too low — audio is silent", rms);

    eprintln!(
        "PCM: {} samples, {}Hz, {:.2}s, RMS={:.4}",
        pcm.samples.len(),
        pcm.sample_rate,
        duration,
        rms,
    );
}
