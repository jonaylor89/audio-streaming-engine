fn main() {
    divan::main();
}

use divan::{Bencher, black_box};
use secrecy::SecretString;
use std::collections::HashMap;
use streaming_engine::{
    blob::AudioFormat,
    streamingpath::{
        hasher::{
            compute_hash, digest_result_storage_hasher, digest_storage_hasher,
            suffix_result_storage_hasher, verify_hash,
        },
        normalize::{SafeCharsType, normalize},
        params::Params,
    },
};

fn simple_params() -> Params {
    Params {
        key: "test.mp3".to_string(),
        format: Some(AudioFormat::Mp3),
        ..Default::default()
    }
}

fn complex_params() -> Params {
    Params {
        key: "path/to/audio.mp3".to_string(),
        format: Some(AudioFormat::Wav),
        sample_rate: Some(96000),
        channels: Some(2),
        bit_depth: Some(24),
        bit_rate: Some(320),
        volume: Some(0.8),
        normalize: Some(true),
        lowpass: Some(20000.0),
        highpass: Some(20.0),
        echo: Some("0.8:0.88:60:0.4".to_string()),
        compressor: Some("threshold=0.125:ratio=6:attack=20:release=250:makeup=1".to_string()),
        fade_in: Some(2.0),
        fade_out: Some(3.0),
        tags: Some({
            let mut tags = HashMap::new();
            tags.insert("artist".to_string(), "Test Artist".to_string());
            tags.insert("album".to_string(), "Test Album".to_string());
            tags.insert("title".to_string(), "Test Song".to_string());
            tags
        }),
        custom_filters: Some(vec!["volume=0.5".to_string(), "highpass=f=200".to_string()]),
        ..Default::default()
    }
}

mod storage_hashing {
    use super::*;

    #[divan::bench]
    fn digest_short() -> String {
        black_box(digest_storage_hasher("test.mp3"))
    }

    #[divan::bench]
    fn digest_long() -> String {
        black_box(digest_storage_hasher(
            "very/long/path/to/audio/file/with/many/segments/song.flac?format=mp3&sample_rate=44100&channels=2&bit_rate=320&volume=0.8",
        ))
    }
}

mod params_hashing {
    use super::*;

    #[divan::bench]
    fn digest_result_simple() -> String {
        black_box(digest_result_storage_hasher(&simple_params()))
    }

    #[divan::bench]
    fn digest_result_complex() -> String {
        black_box(digest_result_storage_hasher(&complex_params()))
    }

    #[divan::bench]
    fn suffix_result_simple() -> String {
        black_box(suffix_result_storage_hasher(&simple_params()))
    }

    #[divan::bench]
    fn suffix_result_complex() -> String {
        black_box(suffix_result_storage_hasher(&complex_params()))
    }
}

mod hmac {
    use super::*;

    fn secret() -> SecretString {
        SecretString::from("bench-secret-key-that-is-long-enough-for-hmac".to_string())
    }

    #[divan::bench]
    fn compute_short() -> Result<SecretString, color_eyre::eyre::Error> {
        black_box(compute_hash("test.mp3".to_string(), &secret()))
    }

    #[divan::bench]
    fn compute_long() -> Result<SecretString, color_eyre::eyre::Error> {
        black_box(compute_hash(
            "very/long/path/to/audio/file/with/many/segments/song.flac".to_string(),
            &secret(),
        ))
    }

    #[divan::bench]
    fn verify(bencher: Bencher<'_, '_>) {
        let s = secret();
        let path = "path/to/audio/file.wav";
        let hash = compute_hash(path.to_string(), &s).unwrap();
        let path_secret = SecretString::from(path.to_string());

        bencher.bench(|| {
            black_box(verify_hash(
                black_box(hash.clone()),
                black_box(path_secret.clone()),
                &s,
            ))
        })
    }
}

mod params_parsing {
    use super::*;

    #[divan::bench]
    fn from_str_simple() -> Result<Params, color_eyre::eyre::Error> {
        black_box("test.mp3".parse::<Params>())
    }

    #[divan::bench]
    fn from_str_complex() -> Result<Params, color_eyre::eyre::Error> {
        black_box("audio/track.mp3?format=flac&sample_rate=96000&channels=2&bit_depth=24&volume=0.8&normalize=true&lowpass=20000&highpass=20&echo=0.8:0.88:60:0.4&compressor=threshold=0.125:ratio=6:attack=20:release=250:makeup=1&fade_in=2.0&fade_out=3.0".parse::<Params>())
    }

    #[divan::bench]
    fn from_path_simple() -> color_eyre::Result<Params> {
        black_box(Params::from_path("test.mp3".to_string(), HashMap::new()))
    }

    #[divan::bench]
    fn from_path_complex() -> color_eyre::Result<Params> {
        let mut query = HashMap::new();
        query.insert("format".to_string(), "mp3".to_string());
        query.insert("sample_rate".to_string(), "44100".to_string());
        query.insert("channels".to_string(), "2".to_string());
        query.insert("bit_rate".to_string(), "320".to_string());
        query.insert("volume".to_string(), "0.8".to_string());
        query.insert("normalize".to_string(), "true".to_string());
        query.insert("lowpass".to_string(), "20000".to_string());
        query.insert("highpass".to_string(), "20".to_string());
        query.insert("echo".to_string(), "0.8:0.88:60:0.4".to_string());
        query.insert("fade_in".to_string(), "2.0".to_string());
        query.insert("fade_out".to_string(), "3.0".to_string());
        black_box(Params::from_path("test.mp3".to_string(), query))
    }
}

mod params_serialization {
    use super::*;

    #[divan::bench]
    fn to_string_simple() -> String {
        black_box(simple_params().to_string())
    }

    #[divan::bench]
    fn to_string_complex() -> String {
        black_box(complex_params().to_string())
    }

    #[divan::bench]
    fn to_query_complex() -> HashMap<String, Vec<String>> {
        black_box(complex_params().to_query())
    }

    #[divan::bench]
    fn to_ffmpeg_args_complex() -> Vec<String> {
        black_box(complex_params().to_ffmpeg_args())
    }

    #[divan::bench]
    fn to_unsafe_string_complex() -> String {
        black_box(Params::to_unsafe_string(&complex_params()))
    }
}

mod path_normalization {
    use super::*;

    #[divan::bench]
    fn normalize_simple() -> String {
        let safe = SafeCharsType::default();
        black_box(normalize("test.mp3", &safe))
    }

    #[divan::bench]
    fn normalize_complex() -> String {
        let safe = SafeCharsType::default();
        black_box(normalize(
            "path/to/файл with spaces & symbols (2024).mp3",
            &safe,
        ))
    }
}
