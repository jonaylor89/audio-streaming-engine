//! Tests for `FileStorage` read optimizations.

use crate::helpers::minimal_wav_file;
use streaming_engine::blob::AudioFormat;
use streaming_engine::storage::AudioStorage;
use streaming_engine::storage::file::FileStorage;
use streaming_engine::streamingpath::normalize::SafeCharsType;

/// `FileStorage::get` pre-sizes its read buffer using file metadata so that
/// no reallocations occur during the read. We verify that the returned data
/// matches what was written and has the correct length — confirming the
/// pre-sized path produces identical results to a naive `Vec::new()` read.
#[tokio::test]
async fn get_presizes_read_buffer_from_file_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let storage = FileStorage::new(tmp.path().to_path_buf(), String::new(), SafeCharsType::Noop);

    let wav = minimal_wav_file();
    let blob =
        streaming_engine::blob::AudioBuffer::from_bytes_with_format(wav.clone(), AudioFormat::Wav);
    storage.put("test.wav", &blob).await.unwrap();

    let retrieved = storage.get("test.wav").await.unwrap();
    assert_eq!(retrieved.as_ref(), wav.as_slice());
    assert_eq!(retrieved.len(), wav.len());
}
