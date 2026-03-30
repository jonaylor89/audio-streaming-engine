//! Benchmarks demonstrating the measurable impact of performance improvements.
//!
//! These benchmarks compare the old (slow) patterns against the new (optimized)
//! patterns to quantify the gains from each P0-P2 fix.

fn main() {
    divan::main();
}

use bytes::Bytes;
use divan::{Bencher, black_box};
use std::time::Duration;
use streaming_engine::{
    blob::{AudioBuffer, AudioFormat},
    cache::{AudioCache, fs::FileSystemCache},
};
use tempfile::tempdir;

fn generate_audio_data(size_kb: usize) -> Vec<u8> {
    let size_bytes = size_kb * 1024;
    let mut data = vec![0xFF, 0xFB, 0x90, 0x00]; // MP3 header
    data.resize(size_bytes, 0xAB);
    data
}

/// P1: Zero-copy Bytes vs the old Vec::to_vec() pattern.
///
/// Before: `InputContext::open(opts.input.to_vec())` copied the entire buffer.
/// After:  `InputContext::open(bytes)` where Bytes::clone is an Arc bump.
///
/// This directly measures the allocation cost that was removed.
mod zero_copy_bytes {
    use super::*;

    #[divan::bench(args = [64, 256, 1024, 4096, 16384])]
    fn old_pattern_to_vec(bencher: Bencher<'_, '_>, size_kb: usize) {
        let data = generate_audio_data(size_kb);
        let bytes = Bytes::from(data);

        bencher.bench(|| {
            // OLD: copied entire buffer into a new Vec for FFmpeg
            let vec = black_box(bytes.to_vec());
            black_box(vec.len())
        })
    }

    #[divan::bench(args = [64, 256, 1024, 4096, 16384])]
    fn new_pattern_bytes_clone(bencher: Bencher<'_, '_>, size_kb: usize) {
        let data = generate_audio_data(size_kb);
        let bytes = Bytes::from(data);

        bencher.bench(|| {
            // NEW: Bytes::clone is an Arc bump — zero allocation
            let cloned = black_box(bytes.clone());
            black_box(cloned.len())
        })
    }

    #[divan::bench(args = [64, 256, 1024, 4096, 16384])]
    fn old_pattern_audiobuffer_clone_and_borrow(bencher: Bencher<'_, '_>, size_kb: usize) {
        let data = generate_audio_data(size_kb);
        let buf = AudioBuffer::from_bytes_with_format(data, AudioFormat::Mp3);

        bencher.bench(|| {
            // OLD: clone AudioBuffer + .as_ref() to get &[u8], then .to_vec() inside FFmpeg
            let cloned = buf.clone();
            let slice: &[u8] = cloned.as_ref();
            let vec = black_box(slice.to_vec());
            black_box(vec.len())
        })
    }

    #[divan::bench(args = [64, 256, 1024, 4096, 16384])]
    fn new_pattern_audiobuffer_into_bytes(bencher: Bencher<'_, '_>, size_kb: usize) {
        let data = generate_audio_data(size_kb);
        let buf = AudioBuffer::from_bytes_with_format(data, AudioFormat::Mp3);

        bencher.bench(|| {
            // NEW: clone (Arc bump) + into_bytes() — zero-copy all the way
            let bytes = black_box(buf.clone().into_bytes());
            black_box(bytes.len())
        })
    }
}

/// P2: Background eviction vs inline eviction.
///
/// Before: every `set()` call walked the entire cache directory inline.
/// After:  `set()` writes the file and signals a background task.
///
/// This measures `set()` latency with a pre-populated cache.
mod background_eviction {
    use super::*;

    fn populate_cache(cache: &FileSystemCache, count: usize, rt: &tokio::runtime::Runtime) {
        rt.block_on(async {
            for i in 0..count {
                let key = format!("entry_{:04}", i);
                let data = generate_audio_data(10); // 10KB each
                cache
                    .set(&key, &data, Some(Duration::from_secs(3600)))
                    .await
                    .unwrap();
            }
        });
    }

    #[divan::bench(args = [10, 100, 500])]
    fn set_with_populated_cache(bencher: Bencher<'_, '_>, existing_entries: usize) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = tempdir().unwrap();
        // max_size_mb=1 so eviction will be triggered with many entries
        let cache = rt.block_on(async { FileSystemCache::new(temp_dir.path(), 1).unwrap() });

        populate_cache(&cache, existing_entries, &rt);

        let counter = AtomicUsize::new(0);
        bencher.bench(|| {
            let i = counter.fetch_add(1, Ordering::Relaxed);
            let key = format!("bench_entry_{}", i);
            let data = generate_audio_data(10);
            rt.block_on(async {
                black_box(
                    cache
                        .set(&key, &data, Some(Duration::from_secs(3600)))
                        .await,
                )
            })
        })
    }
}

/// P0: reqwest::Client construction cost.
///
/// Before: `Client::new()` was called per request.
/// After:  A single shared Client is reused.
///
/// This measures the raw cost of constructing a new client vs cloning a shared one.
mod client_reuse {
    use super::*;

    #[divan::bench]
    fn old_pattern_client_new() {
        // OLD: every remote fetch created a brand new Client
        let client = black_box(reqwest::Client::new());
        black_box(std::mem::size_of_val(&client));
    }

    #[divan::bench]
    fn new_pattern_client_clone() {
        // Use a LazyLock to init once — same as our AppStateDyn pattern
        static SHARED: std::sync::LazyLock<reqwest::Client> =
            std::sync::LazyLock::new(reqwest::Client::new);

        // NEW: clone is an Arc bump
        let client = black_box(SHARED.clone());
        black_box(std::mem::size_of_val(&client));
    }
}
