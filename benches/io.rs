fn main() {
    divan::main();
}

use divan::Bencher;
use streaming_engine::{
    blob::AudioBuffer,
    cache::{AudioCache, fs::FileSystemCache},
    storage::{AudioStorage, file::FileStorage},
    streamingpath::normalize::SafeCharsType,
};
use tempfile::tempdir;

fn generate_audio_data(size_kb: usize) -> Vec<u8> {
    let size_bytes = size_kb * 1024;
    let mut data = Vec::with_capacity(size_bytes);
    // MP3 header
    data.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
    for i in 0..size_bytes - 4 {
        data.push(((i * 31) % 256) as u8);
    }
    data
}

const SIZES_KB: &[usize] = &[10, 100, 1024];

mod file_storage {
    use super::*;

    #[divan::bench(args = SIZES_KB)]
    fn put(bencher: Bencher<'_, '_>, size_kb: usize) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        bencher
            .with_inputs(|| {
                let dir = tempdir().unwrap();
                let storage = FileStorage::new(
                    dir.path().to_path_buf(),
                    "audio".to_string(),
                    SafeCharsType::Default,
                );
                let buffer = AudioBuffer::from_bytes(generate_audio_data(size_kb));
                (dir, storage, buffer)
            })
            .bench_values(|(_dir, storage, buffer)| {
                rt.block_on(async { storage.put("bench.mp3", &buffer).await.unwrap() })
            });
    }

    #[divan::bench(args = SIZES_KB)]
    fn get(bencher: Bencher<'_, '_>, size_kb: usize) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        bencher
            .with_inputs(|| {
                let dir = tempdir().unwrap();
                let storage = FileStorage::new(
                    dir.path().to_path_buf(),
                    "audio".to_string(),
                    SafeCharsType::Default,
                );
                let buffer = AudioBuffer::from_bytes(generate_audio_data(size_kb));
                rt.block_on(async { storage.put("bench.mp3", &buffer).await.unwrap() });
                (dir, storage)
            })
            .bench_values(|(_dir, storage)| {
                rt.block_on(async { storage.get("bench.mp3").await.unwrap() })
            });
    }
}

mod filesystem_cache {
    use super::*;

    #[divan::bench(args = SIZES_KB)]
    fn set(bencher: Bencher<'_, '_>, size_kb: usize) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempdir().unwrap();
        let cache = rt.block_on(async { FileSystemCache::new(dir.path(), 1000).unwrap() });
        let data = generate_audio_data(size_kb);

        bencher.bench(|| rt.block_on(async { cache.set("bench_key", &data, None).await.unwrap() }));
    }

    #[divan::bench(args = SIZES_KB)]
    fn get(bencher: Bencher<'_, '_>, size_kb: usize) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempdir().unwrap();
        let cache = rt.block_on(async {
            let c = FileSystemCache::new(dir.path(), 1000).unwrap();
            let data = generate_audio_data(size_kb);
            c.set("bench_key", &data, None).await.unwrap();
            c
        });

        bencher.bench(|| rt.block_on(async { cache.get("bench_key").await.unwrap() }));
    }
}

mod storage_vs_cache {
    use super::*;

    #[divan::bench(args = SIZES_KB)]
    fn storage_put(bencher: Bencher<'_, '_>, size_kb: usize) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        bencher
            .with_inputs(|| {
                let dir = tempdir().unwrap();
                let storage = FileStorage::new(
                    dir.path().to_path_buf(),
                    "audio".to_string(),
                    SafeCharsType::Default,
                );
                let buffer = AudioBuffer::from_bytes(generate_audio_data(size_kb));
                (dir, storage, buffer)
            })
            .bench_values(|(_dir, storage, buffer)| {
                rt.block_on(async { storage.put("cmp.mp3", &buffer).await.unwrap() })
            });
    }

    #[divan::bench(args = SIZES_KB)]
    fn cache_set(bencher: Bencher<'_, '_>, size_kb: usize) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempdir().unwrap();
        let cache = rt.block_on(async { FileSystemCache::new(dir.path(), 1000).unwrap() });
        let data = generate_audio_data(size_kb);

        bencher.bench(|| rt.block_on(async { cache.set("cmp_key", &data, None).await.unwrap() }));
    }
}
