fn main() {
    divan::main();
}

use divan::{black_box, Bencher};
use std::path::PathBuf;
use std::sync::LazyLock;
use streaming_engine::thumbnail::{analyze, ThumbnailConfig};

struct Fixture {
    samples: Vec<f32>,
    sample_rate: i32,
}

static PCM_FIXTURE: LazyLock<Fixture> = LazyLock::new(|| {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("uploads/sample1.mp3");
    let data = bytes::Bytes::from(std::fs::read(path).expect("sample fixture should exist"));
    let pcm = ffmpeg::decode_to_pcm(data).expect("decode_to_pcm should succeed");
    Fixture {
        samples: pcm.samples,
        sample_rate: pcm.sample_rate,
    }
});

mod pcm_decode {
    use super::*;

    #[divan::bench(ignore = cfg!(codspeed))]
    fn decode_fixture_to_pcm(bencher: Bencher<'_, '_>) {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("uploads/sample1.mp3");
        let data = std::fs::read(path).expect("fixture should exist");

        bencher.bench(|| {
            let bytes = bytes::Bytes::from(data.clone());
            black_box(ffmpeg::decode_to_pcm(bytes).unwrap())
        })
    }
}

mod chroma_extraction {
    use super::*;
    use streaming_engine::thumbnail::chroma::extract_chroma;

    #[divan::bench(ignore = cfg!(codspeed))]
    fn extract_chroma_fixture(bencher: Bencher<'_, '_>) {
        let fixture = &*PCM_FIXTURE;
        let hop_size = fixture.sample_rate as usize / 2;

        bencher.bench(|| {
            black_box(extract_chroma(
                black_box(&fixture.samples),
                fixture.sample_rate,
                hop_size,
            ))
        })
    }
}

mod ssm_construction {
    use super::*;
    use streaming_engine::thumbnail::chroma::extract_chroma;
    use streaming_engine::thumbnail::ssm::build_ssm;

    #[divan::bench(ignore = cfg!(codspeed))]
    fn build_ssm_fixture(bencher: Bencher<'_, '_>) {
        let fixture = &*PCM_FIXTURE;
        let hop_size = fixture.sample_rate as usize / 2;
        let chroma = extract_chroma(&fixture.samples, fixture.sample_rate, hop_size);
        let num_frames = chroma.len() / 12;

        bencher.bench(|| black_box(build_ssm(black_box(&chroma), num_frames)))
    }
}

mod full_analysis {
    use super::*;

    #[divan::bench(ignore = cfg!(codspeed))]
    fn analyze_fixture_default_config(bencher: Bencher<'_, '_>) {
        let fixture = &*PCM_FIXTURE;
        let config = ThumbnailConfig::default();

        bencher.bench(|| {
            black_box(
                analyze(
                    black_box(&fixture.samples),
                    fixture.sample_rate,
                    black_box(&config),
                )
                .unwrap(),
            )
        })
    }

    #[divan::bench(ignore = cfg!(codspeed))]
    fn analyze_fixture_short_thumbnail(bencher: Bencher<'_, '_>) {
        let fixture = &*PCM_FIXTURE;
        let config = ThumbnailConfig {
            target_duration: 10.0,
            min_duration: 5.0,
            max_duration: 15.0,
        };

        bencher.bench(|| {
            black_box(
                analyze(
                    black_box(&fixture.samples),
                    fixture.sample_rate,
                    black_box(&config),
                )
                .unwrap(),
            )
        })
    }
}
