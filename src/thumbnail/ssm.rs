//! Self-Similarity Matrix construction.
//!
//! Builds a symmetric matrix where entry (i, j) is the cosine similarity
//! between chroma frame i and chroma frame j.

/// Build a self-similarity matrix from chroma features.
///
/// `chroma` is a flat array of `num_frames * 12` f32 values.
/// Returns a flat `num_frames × num_frames` matrix (row-major).
pub fn build_ssm(chroma: &[f32], num_frames: usize) -> Vec<f32> {
    let dim = 12;
    assert_eq!(chroma.len(), num_frames * dim);

    let mut ssm = vec![0.0f32; num_frames * num_frames];

    // Precompute norms
    let norms: Vec<f32> = (0..num_frames)
        .map(|i| {
            let frame = &chroma[i * dim..(i + 1) * dim];
            let sum_sq: f32 = frame.iter().map(|x| x * x).sum();
            sum_sq.sqrt().max(1e-10)
        })
        .collect();

    for i in 0..num_frames {
        ssm[i * num_frames + i] = 1.0; // diagonal
        let frame_i = &chroma[i * dim..(i + 1) * dim];

        for j in (i + 1)..num_frames {
            let frame_j = &chroma[j * dim..(j + 1) * dim];

            let dot: f32 = frame_i.iter().zip(frame_j.iter()).map(|(a, b)| a * b).sum();
            let sim = dot / (norms[i] * norms[j]);

            ssm[i * num_frames + j] = sim;
            ssm[j * num_frames + i] = sim;
        }
    }

    ssm
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ssm_diagonal_is_one() {
        let chroma = vec![1.0f32; 3 * 12];
        let ssm = build_ssm(&chroma, 3);
        for i in 0..3 {
            assert!((ssm[i * 3 + i] - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn test_ssm_identical_frames() {
        let chroma = vec![0.5f32; 4 * 12];
        let ssm = build_ssm(&chroma, 4);
        for val in &ssm {
            assert!((val - 1.0).abs() < 1e-5, "Expected ~1.0, got {}", val);
        }
    }

    #[test]
    fn test_ssm_orthogonal_frames() {
        let mut chroma = vec![0.0f32; 2 * 12];
        chroma[0] = 1.0;
        chroma[12 + 6] = 1.0;
        let ssm = build_ssm(&chroma, 2);

        assert!((ssm[0] - 1.0).abs() < 1e-6);
        assert!((ssm[3] - 1.0).abs() < 1e-6);
        assert!(ssm[1].abs() < 1e-6);
        assert!(ssm[2].abs() < 1e-6);
    }
}
