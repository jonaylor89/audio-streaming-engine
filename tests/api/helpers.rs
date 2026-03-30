use once_cell::sync::Lazy;
use std::path::PathBuf;
use streaming_engine::{
    config::get_configuration,
    startup::Application,
    telemetry::{get_subscriber, init_subscriber},
};
use tokio::task::JoinHandle;

static TRACING: Lazy<()> = Lazy::new(|| {
    let default_filter_level = "info".to_string();
    let subscriber_name = "test".to_string();

    if std::env::var("TEST_LOG").is_ok() {
        let subscriber = get_subscriber(subscriber_name, default_filter_level, std::io::stdout);
        init_subscriber(subscriber);
    } else {
        let subscriber = get_subscriber(subscriber_name, default_filter_level, std::io::sink);
        init_subscriber(subscriber);
    }
});

// ---------------------------------------------------------------------------
// Test app
// ---------------------------------------------------------------------------

pub struct TestApp {
    pub address: String,
    pub port: u16,
    pub api_client: reqwest::Client,
    server_handle: JoinHandle<()>,
}

impl Drop for TestApp {
    fn drop(&mut self) {
        self.server_handle.abort();
    }
}

pub async fn spawn_app() -> TestApp {
    Lazy::force(&TRACING);

    let configuration = {
        let mut c = get_configuration().expect("Failed to read configuration");
        c.port = 0;

        c
    };

    let application = Application::build(configuration.clone())
        .await
        .expect("Failed to build application");

    let application_port = application.port;
    let address = format!("http://localhost:{}", application_port);

    let server_handle = tokio::spawn(async move {
        application
            .run_until_stopped()
            .await
            .expect("test server exited unexpectedly");
    });

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    TestApp {
        address,
        port: application_port,
        api_client: client,
        server_handle,
    }
}

// ---------------------------------------------------------------------------
// Audio fixtures
// ---------------------------------------------------------------------------

/// Path to the MP3 test fixture (uploads/sample1.mp3).
pub fn fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("uploads/sample1.mp3");
    assert!(
        path.exists(),
        "Test fixture not found at {:?}. Ensure uploads/sample1.mp3 exists.",
        path,
    );
    path
}

/// Load the MP3 test fixture as raw bytes.
pub fn load_fixture_bytes() -> bytes::Bytes {
    bytes::Bytes::from(std::fs::read(fixture_path()).unwrap())
}

/// Decode the MP3 test fixture to mono f32 PCM.
pub fn load_fixture_pcm() -> ffmpeg::PcmData {
    ffmpeg::decode_to_pcm(load_fixture_bytes()).expect("fixture should decode to PCM")
}

/// Generate a minimal valid WAV file (1 sample, 8 kHz, 16-bit mono).
pub fn minimal_wav_file() -> Vec<u8> {
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&38u32.to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&8000u32.to_le_bytes()); // sample rate
    wav.extend_from_slice(&16000u32.to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&2u32.to_le_bytes());
    wav.extend_from_slice(&0i16.to_le_bytes());
    wav
}
