fn main() {
    divan::main();
}

use divan::{black_box, Bencher};
use futures::StreamExt;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use streaming_engine::{
    blob::{AudioBuffer, AudioFormat},
    config::ProcessorSettings,
    processor::{AudioProcessor, Processor},
    streamingpath::params::Params,
    thumbnail::{analyze, chroma::extract_chroma, ssm::build_ssm, ThumbnailConfig},
};

static SAMPLE_MP3: LazyLock<AudioBuffer> = LazyLock::new(|| {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("uploads/sample1.mp3");
    let bytes = fs::read(path).expect("uploads/sample1.mp3 fixture must exist");
    AudioBuffer::from_bytes_with_format(bytes, AudioFormat::Mp3)
});

static TEST_WAV: LazyLock<AudioBuffer> = LazyLock::new(|| {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("uploads/test_tone.wav");
    let bytes = fs::read(path).expect("uploads/test_tone.wav fixture must exist");
    AudioBuffer::from_bytes_with_format(bytes, AudioFormat::Wav)
});

struct PcmFixture {
    samples: Vec<f32>,
    sample_rate: i32,
}

static PCM: LazyLock<PcmFixture> = LazyLock::new(|| {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("uploads/sample1.mp3");
    let data = bytes::Bytes::from(fs::read(path).expect("fixture"));
    let pcm = ffmpeg::decode_to_pcm(data).expect("decode_to_pcm");
    PcmFixture {
        samples: pcm.samples,
        sample_rate: pcm.sample_rate,
    }
});

fn processor(concurrency: usize) -> Processor {
    Processor::new(ProcessorSettings {
        disabled_filters: Vec::new(),
        max_filter_ops: 100,
        concurrency: Some(concurrency),
        max_cache_files: 100,
        max_cache_mem: 50 * 1024 * 1024,
        max_cache_size: 200 * 1024 * 1024,
    })
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

// ---------------------------------------------------------------------------
// Passthrough & transcode with real fixtures
// ---------------------------------------------------------------------------
mod passthrough {
    use super::*;

    #[divan::bench(ignore = cfg!(codspeed))]
    fn mp3_passthrough(bencher: Bencher<'_, '_>) {
        let proc = processor(1);
        let rt = rt();
        let params = Params {
            key: "sample1.mp3".into(),
            ..Default::default()
        };

        bencher.bench(|| {
            rt.block_on(async {
                black_box(proc.process(black_box(&*SAMPLE_MP3), black_box(&params)).await)
            })
        });
    }

    #[divan::bench(ignore = cfg!(codspeed))]
    fn wav_passthrough(bencher: Bencher<'_, '_>) {
        let proc = processor(1);
        let rt = rt();
        let params = Params {
            key: "test_tone.wav".into(),
            ..Default::default()
        };

        bencher.bench(|| {
            rt.block_on(async {
                black_box(proc.process(black_box(&*TEST_WAV), black_box(&params)).await)
            })
        });
    }
}

mod transcode {
    use super::*;

    #[divan::bench(ignore = cfg!(codspeed))]
    fn mp3_to_wav(bencher: Bencher<'_, '_>) {
        let proc = processor(1);
        let rt = rt();
        let params = Params {
            key: "sample1.mp3".into(),
            format: Some(AudioFormat::Wav),
            sample_rate: Some(44100),
            channels: Some(2),
            ..Default::default()
        };

        bencher.bench(|| {
            rt.block_on(async {
                black_box(proc.process(black_box(&*SAMPLE_MP3), black_box(&params)).await)
            })
        });
    }

    #[divan::bench(ignore = cfg!(codspeed))]
    fn wav_to_mp3(bencher: Bencher<'_, '_>) {
        let proc = processor(1);
        let rt = rt();
        let params = Params {
            key: "test_tone.wav".into(),
            format: Some(AudioFormat::Mp3),
            ..Default::default()
        };

        bencher.bench(|| {
            rt.block_on(async {
                black_box(proc.process(black_box(&*TEST_WAV), black_box(&params)).await)
            })
        });
    }
}

// ---------------------------------------------------------------------------
// Filter chains of increasing complexity
// ---------------------------------------------------------------------------
mod filters {
    use super::*;

    #[divan::bench(ignore = cfg!(codspeed))]
    fn volume_only(bencher: Bencher<'_, '_>) {
        let proc = processor(1);
        let rt = rt();
        let params = Params {
            key: "sample1.mp3".into(),
            format: Some(AudioFormat::Mp3),
            volume: Some(0.75),
            ..Default::default()
        };

        bencher.bench(|| {
            rt.block_on(async {
                black_box(proc.process(black_box(&*SAMPLE_MP3), black_box(&params)).await)
            })
        });
    }

    #[divan::bench(ignore = cfg!(codspeed))]
    fn eq_chain(bencher: Bencher<'_, '_>) {
        let proc = processor(1);
        let rt = rt();
        let params = Params {
            key: "sample1.mp3".into(),
            format: Some(AudioFormat::Mp3),
            volume: Some(0.9),
            lowpass: Some(14000.0),
            highpass: Some(80.0),
            ..Default::default()
        };

        bencher.bench(|| {
            rt.block_on(async {
                black_box(proc.process(black_box(&*SAMPLE_MP3), black_box(&params)).await)
            })
        });
    }

    #[divan::bench(ignore = cfg!(codspeed))]
    fn full_chain(bencher: Bencher<'_, '_>) {
        let proc = processor(1);
        let rt = rt();
        let params = Params {
            key: "sample1.mp3".into(),
            format: Some(AudioFormat::Mp3),
            volume: Some(0.85),
            normalize: Some(true),
            lowpass: Some(16000.0),
            highpass: Some(40.0),
            compressor: Some(
                "threshold=0.125:ratio=4:attack=20:release=200:makeup=1".into(),
            ),
            fade_in: Some(1.0),
            fade_out: Some(2.0),
            ..Default::default()
        };

        bencher.bench(|| {
            rt.block_on(async {
                black_box(proc.process(black_box(&*SAMPLE_MP3), black_box(&params)).await)
            })
        });
    }
}

// ---------------------------------------------------------------------------
// Streaming vs buffered comparison
// ---------------------------------------------------------------------------
mod streaming_vs_buffered {
    use super::*;

    fn transcode_params() -> Params {
        Params {
            key: "sample1.mp3".into(),
            format: Some(AudioFormat::Mp3),
            volume: Some(0.9),
            lowpass: Some(12000.0),
            ..Default::default()
        }
    }

    #[divan::bench(ignore = cfg!(codspeed))]
    fn buffered_total(bencher: Bencher<'_, '_>) {
        let proc = Arc::new(processor(2));
        let rt = rt();
        let params = transcode_params();

        bencher.bench(|| {
            rt.block_on(async {
                black_box(
                    proc.process(black_box(&*SAMPLE_MP3), black_box(&params))
                        .await
                        .unwrap(),
                )
            })
        });
    }

    #[divan::bench(ignore = cfg!(codspeed))]
    fn streaming_drain(bencher: Bencher<'_, '_>) {
        let proc = Arc::new(processor(2));
        let rt = rt();
        let params = transcode_params();

        bencher.bench(|| {
            rt.block_on(async {
                let stream = proc
                    .process_streaming(black_box(&*SAMPLE_MP3), black_box(&params))
                    .await
                    .unwrap();
                futures::pin_mut!(stream);
                let mut total = 0usize;
                while let Some(chunk) = stream.next().await {
                    total += chunk.unwrap().len();
                }
                black_box(total)
            })
        });
    }

    #[divan::bench(ignore = cfg!(codspeed))]
    fn streaming_ttfb(bencher: Bencher<'_, '_>) {
        let proc = Arc::new(processor(2));
        let rt = rt();
        let params = transcode_params();

        bencher.bench(|| {
            rt.block_on(async {
                let stream = proc
                    .process_streaming(black_box(&*SAMPLE_MP3), black_box(&params))
                    .await
                    .unwrap();
                futures::pin_mut!(stream);
                let first = stream.next().await;
                black_box(first.unwrap().unwrap().len())
            })
        });
    }
}

// ---------------------------------------------------------------------------
// Full pipeline: storage.get → process/process_streaming → result_storage.put
// Parameterised over common real-world Params configurations.
//
// Scenarios:
//   passthrough  – serve file as-is (no FFmpeg decode/encode)
//   transcode    – MP3 → WAV, 44.1 kHz stereo
//   normalize    – loudness normalisation (podcast / audiobook)
//   full_chain   – volume + EQ + compressor + fades (mastering)
// ---------------------------------------------------------------------------
mod pipeline {
    use super::*;
    use streaming_engine::{
        storage::{file::FileStorage, AudioStorage},
        streamingpath::{hasher::suffix_result_storage_hasher, normalize::SafeCharsType},
    };

    #[derive(Clone, Copy)]
    enum Scenario {
        Passthrough,
        Transcode,
        Normalize,
        FullChain,
    }
    use Scenario::*;

    const SCENARIOS: &[Scenario] = &[Passthrough, Transcode, Normalize, FullChain];

    // divan needs Display for the args label
    impl std::fmt::Display for Scenario {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(match self {
                Passthrough => "passthrough",
                Transcode => "transcode",
                Normalize => "normalize",
                FullChain => "full_chain",
            })
        }
    }

    fn scenario_params(s: Scenario) -> Params {
        match s {
            Passthrough => Params {
                key: "sample1.mp3".into(),
                ..Default::default()
            },
            Transcode => Params {
                key: "sample1.mp3".into(),
                format: Some(AudioFormat::Wav),
                sample_rate: Some(44100),
                channels: Some(2),
                ..Default::default()
            },
            Normalize => Params {
                key: "sample1.mp3".into(),
                format: Some(AudioFormat::Mp3),
                normalize: Some(true),
                compressor: Some(
                    "threshold=0.125:ratio=3:attack=20:release=200:makeup=1".into(),
                ),
                ..Default::default()
            },
            FullChain => Params {
                key: "sample1.mp3".into(),
                format: Some(AudioFormat::Mp3),
                volume: Some(0.85),
                lowpass: Some(14000.0),
                highpass: Some(80.0),
                compressor: Some(
                    "threshold=0.125:ratio=4:attack=20:release=200:makeup=1".into(),
                ),
                fade_in: Some(1.0),
                fade_out: Some(2.0),
                ..Default::default()
            },
        }
    }

    struct Harness {
        proc: Processor,
        source: Arc<dyn AudioStorage>,
        results: Arc<dyn AudioStorage>,
    }

    fn setup(dir: &std::path::Path) -> Harness {
        let src_dir = dir.join("source");
        let res_dir = dir.join("results");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&res_dir).unwrap();

        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("uploads/sample1.mp3");
        let dest = src_dir.join("audio/sample1.mp3");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(&fixture, &dest).unwrap();

        Harness {
            proc: processor(2),
            source: Arc::new(FileStorage::new(
                src_dir,
                "audio".into(),
                SafeCharsType::Default,
            )),
            results: Arc::new(FileStorage::new(
                res_dir,
                "results".into(),
                SafeCharsType::Default,
            )),
        }
    }

    /// Buffered cold path: storage fetch → process() → result store.
    #[divan::bench(ignore = cfg!(codspeed), args = SCENARIOS)]
    fn cold_buffered(bencher: Bencher<'_, '_>, scenario: Scenario) {
        let dir = tempfile::tempdir().unwrap();
        let h = setup(dir.path());
        let rt = rt();
        let params = scenario_params(scenario);
        let rk = suffix_result_storage_hasher(&params);

        bencher.bench(|| {
            rt.block_on(async {
                let _ = h.results.delete(&rk).await;
                let blob = h.source.get(&params.key).await.unwrap();
                let processed = h.proc.process(&blob, &params).await.unwrap();
                h.results.put(&rk, &processed).await.unwrap();
                black_box(processed.len())
            })
        });
    }

    /// Streaming cold path: storage fetch → process_streaming() (drain all
    /// chunks) → reassemble → result store.
    #[divan::bench(ignore = cfg!(codspeed), args = SCENARIOS)]
    fn cold_streaming(bencher: Bencher<'_, '_>, scenario: Scenario) {
        let dir = tempfile::tempdir().unwrap();
        let h = setup(dir.path());
        let rt = rt();
        let params = scenario_params(scenario);
        let rk = suffix_result_storage_hasher(&params);

        bencher.bench(|| {
            rt.block_on(async {
                let _ = h.results.delete(&rk).await;
                let blob = h.source.get(&params.key).await.unwrap();

                let stream = h.proc.process_streaming(&blob, &params).await.unwrap();
                futures::pin_mut!(stream);
                let mut chunks = bytes::BytesMut::new();
                while let Some(chunk) = stream.next().await {
                    chunks.extend_from_slice(&chunk.unwrap());
                }
                let out = AudioBuffer::from_bytes_with_format(
                    chunks.freeze(),
                    params.format.unwrap_or(blob.format()),
                );
                h.results.put(&rk, &out).await.unwrap();
                black_box(out.len())
            })
        });
    }

    /// Hot path: result storage hit — no FFmpeg.
    /// One representative scenario is enough; I/O dominates and is format-independent.
    #[divan::bench(ignore = cfg!(codspeed))]
    fn hot_request(bencher: Bencher<'_, '_>) {
        let dir = tempfile::tempdir().unwrap();
        let h = setup(dir.path());
        let rt = rt();
        let params = scenario_params(FullChain);
        let rk = suffix_result_storage_hasher(&params);

        rt.block_on(async {
            let blob = h.source.get(&params.key).await.unwrap();
            let processed = h.proc.process(&blob, &params).await.unwrap();
            h.results.put(&rk, &processed).await.unwrap();
        });

        bencher.bench(|| {
            rt.block_on(async {
                let hit = h.results.get(&rk).await.unwrap();
                black_box(hit.len())
            })
        });
    }
}

// ---------------------------------------------------------------------------
// Thumbnail analysis pipeline (PCM → chroma → SSM → analyze)
// ---------------------------------------------------------------------------
mod thumbnail {
    use super::*;

    #[divan::bench(ignore = cfg!(codspeed))]
    fn decode_to_pcm(bencher: Bencher<'_, '_>) {
        let raw = fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("uploads/sample1.mp3"),
        )
        .unwrap();

        bencher
            .with_inputs(|| bytes::Bytes::from(raw.clone()))
            .bench_values(|data| black_box(ffmpeg::decode_to_pcm(data).unwrap()));
    }

    #[divan::bench(ignore = cfg!(codspeed))]
    fn chroma(bencher: Bencher<'_, '_>) {
        let f = &*PCM;
        let hop = f.sample_rate as usize / 2;

        bencher
            .bench(|| black_box(extract_chroma(black_box(&f.samples), f.sample_rate, hop)));
    }

    #[divan::bench(ignore = cfg!(codspeed))]
    fn ssm(bencher: Bencher<'_, '_>) {
        let f = &*PCM;
        let hop = f.sample_rate as usize / 2;
        let chroma_data = extract_chroma(&f.samples, f.sample_rate, hop);
        let frames = chroma_data.len() / 12;

        bencher.bench(|| black_box(build_ssm(black_box(&chroma_data), frames)));
    }

    #[divan::bench(ignore = cfg!(codspeed))]
    fn full_analyze(bencher: Bencher<'_, '_>) {
        let f = &*PCM;
        let config = ThumbnailConfig::default();

        bencher.bench(|| {
            black_box(analyze(black_box(&f.samples), f.sample_rate, black_box(&config)).unwrap())
        });
    }
}
