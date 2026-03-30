//! Fitness measure for thumbnail segment selection.
//!
//! Evaluates each candidate segment by how well it "explains" the rest of the recording:
//! fitness = repetition_score × coverage_score.

/// Find the best thumbnail segment using the fitness measure.
///
/// Returns (best_start_frame, best_length_frames, confidence).
pub fn find_best_segment(
    ssm: &[f32],
    num_frames: usize,
    min_len: usize,
    max_len: usize,
    target_len: usize,
) -> (usize, usize, f64) {
    let mut best_start = 0usize;
    let mut best_len = target_len.min(num_frames);
    let mut best_fitness = f64::NEG_INFINITY;

    let sim_threshold = 0.6;

    // Stride to reduce computation
    let start_stride = 1.max(num_frames / 200);
    let len_stride = 1.max((max_len - min_len) / 20);

    let mut length = min_len;
    while length <= max_len && length <= num_frames {
        let mut start = 0;
        while start + length <= num_frames {
            let fitness = segment_fitness(
                ssm,
                num_frames,
                start,
                length,
                target_len,
                sim_threshold,
            );

            if fitness > best_fitness {
                best_fitness = fitness;
                best_start = start;
                best_len = length;
            }

            start += start_stride;
        }
        length += len_stride.max(1);
    }

    let confidence = if best_fitness > 0.0 {
        (best_fitness / 1.0).min(1.0)
    } else {
        0.1
    };

    (best_start, best_len, confidence)
}

/// Compute the fitness of a candidate segment [start..start+length].
fn segment_fitness(
    ssm: &[f32],
    num_frames: usize,
    start: usize,
    length: usize,
    target_len: usize,
    sim_threshold: f32,
) -> f64 {
    let mut total_similarity = 0.0f64;
    let mut covered_frames = 0usize;

    let step = 1.max(num_frames / 100);
    let mut pos = 0;
    while pos + length <= num_frames {
        if pos == start {
            covered_frames += length;
            total_similarity += 1.0;
            pos += step;
            continue;
        }

        // Average similarity along the diagonal stripe
        let diag_step = 1.max(length / 16);
        let mut diag_sum = 0.0f32;
        let mut diag_count = 0;
        let mut k = 0;
        while k < length {
            let i = start + k;
            let j = pos + k;
            diag_sum += ssm[i * num_frames + j];
            diag_count += 1;
            k += diag_step;
        }

        let avg_sim = if diag_count > 0 {
            diag_sum / diag_count as f32
        } else {
            0.0
        };

        if avg_sim >= sim_threshold {
            covered_frames += length;
            total_similarity += avg_sim as f64;
        }

        pos += step;
    }

    let coverage = (covered_frames as f64 / num_frames as f64).min(1.0);

    let num_positions = (num_frames.saturating_sub(length)) / step + 1;
    let repetition = if num_positions > 0 {
        total_similarity / num_positions as f64
    } else {
        0.0
    };

    let duration_ratio = length as f64 / target_len as f64;
    let duration_bonus = 1.0 - (duration_ratio - 1.0).abs() * 0.2;

    coverage * repetition * duration_bonus.max(0.5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uniform_ssm() {
        let n = 60;
        let ssm = vec![1.0f32; n * n];
        let (start, len, confidence) = find_best_segment(&ssm, n, 10, 30, 20);

        assert!(start + len <= n);
        assert!(len >= 10);
        assert!(len <= 30);
        assert!(confidence > 0.0);
    }

    #[test]
    fn test_no_repetition() {
        let n = 60;
        let mut ssm = vec![0.0f32; n * n];
        for i in 0..n {
            ssm[i * n + i] = 1.0;
        }
        let (start, len, confidence) = find_best_segment(&ssm, n, 10, 30, 20);

        assert!(start + len <= n);
        assert!(confidence < 0.8);
    }
}
