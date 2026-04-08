//! Chromagram extraction using FFT.
//!
//! Computes 12-bin pitch class profiles from audio samples.

use rustfft::{FftPlanner, num_complex::Complex};

const NUM_CHROMA: usize = 12;

/// Extract chroma features from audio samples.
///
/// Returns a flat vector of chroma frames, each frame being 12 f32 values.
/// Total length = num_frames * 12.
pub fn extract_chroma(samples: &[f32], sample_rate: i32, hop_size: usize) -> Vec<f32> {
    let fft_size = 4096;
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(fft_size);

    let num_frames = if samples.len() >= fft_size {
        (samples.len() - fft_size) / hop_size + 1
    } else {
        0
    };

    let mut chroma_features = Vec::with_capacity(num_frames * NUM_CHROMA);

    // Precompute bin-to-chroma mapping
    let bin_chroma_map = build_bin_chroma_map(fft_size, sample_rate);

    let mut buffer = vec![Complex::new(0.0f32, 0.0); fft_size];
    let window = hann_window(fft_size);

    for frame_idx in 0..num_frames {
        let start = frame_idx * hop_size;

        // Apply window and load into FFT buffer
        for i in 0..fft_size {
            let sample = if start + i < samples.len() {
                samples[start + i]
            } else {
                0.0
            };
            buffer[i] = Complex::new(sample * window[i], 0.0);
        }

        // Run FFT
        fft.process(&mut buffer);

        // Compute magnitude spectrum and fold into chroma bins
        let mut chroma = [0.0f32; NUM_CHROMA];
        let half = fft_size / 2;
        for bin in 1..half {
            let magnitude = buffer[bin].norm();
            if let Some(chroma_bin) = bin_chroma_map[bin] {
                chroma[chroma_bin] += magnitude * magnitude; // energy
            }
        }

        // Normalize chroma vector
        let max_val = chroma.iter().cloned().fold(0.0f32, f32::max);
        if max_val > 1e-10 {
            for c in &mut chroma {
                *c /= max_val;
            }
        }

        chroma_features.extend_from_slice(&chroma);
    }

    chroma_features
}

/// Map FFT bin index to chroma bin (0..11), or None if outside useful range.
fn build_bin_chroma_map(fft_size: usize, sample_rate: i32) -> Vec<Option<usize>> {
    let half = fft_size / 2;
    let mut map = vec![None; half];

    for (bin, chroma_slot) in map.iter_mut().enumerate().take(half).skip(1) {
        let freq = bin as f64 * sample_rate as f64 / fft_size as f64;
        if !(32.0..=8000.0).contains(&freq) {
            continue;
        }

        // Convert frequency to MIDI note number, then to chroma
        let midi = 69.0 + 12.0 * (freq / 440.0).log2();
        let chroma_bin = ((midi.round() as i32) % 12).unsigned_abs() as usize;
        *chroma_slot = Some(chroma_bin % NUM_CHROMA);
    }

    map
}

/// Generate a Hann window of the given size.
fn hann_window(size: usize) -> Vec<f32> {
    (0..size)
        .map(|i| {
            let x = std::f32::consts::PI * i as f32 / size as f32;
            x.sin().powi(2)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chroma_extraction_basic() {
        let sample_rate = 44100;
        let duration_samples = sample_rate as usize * 5;
        // 440 Hz sine wave (A4 = chroma bin 9)
        let samples: Vec<f32> = (0..duration_samples)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin())
            .collect();

        let hop_size = sample_rate as usize / 2;
        let chroma = extract_chroma(&samples, sample_rate, hop_size);

        assert!(!chroma.is_empty());
        assert_eq!(chroma.len() % 12, 0);

        // A4 should produce the highest energy in chroma bin 9
        let num_frames = chroma.len() / 12;
        for frame in 0..num_frames {
            let frame_chroma = &chroma[frame * 12..(frame + 1) * 12];
            let max_bin = frame_chroma
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                .unwrap()
                .0;
            assert_eq!(max_bin, 9, "Expected A (bin 9), got bin {}", max_bin);
        }
    }

    #[test]
    fn test_empty_input() {
        let chroma = extract_chroma(&[], 44100, 22050);
        assert!(chroma.is_empty());
    }
}
