//! Tests for the `AudioCache` trait using `Bytes` instead of `Vec<u8>`.

use bytes::Bytes;
use streaming_engine::cache::AudioCache;
use streaming_engine::cache::fs::FileSystemCache;

/// `FileSystemCache` round-trips `Bytes` through `set` and `get` without
/// unnecessary copies. The returned `Bytes` must equal the original value,
/// confirming the `Vec<u8>` ↔ `Bytes` conversion path is correct.
#[tokio::test]
async fn filesystem_cache_roundtrips_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = FileSystemCache::new(tmp.path(), 100).unwrap();

    let data = Bytes::from_static(b"hello audio cache bytes");
    cache.set("test_key", data.clone(), None).await.unwrap();

    let retrieved = cache.get("test_key").await.unwrap();
    assert_eq!(retrieved, Some(data));
}

/// After deletion, `get` returns `None`.
#[tokio::test]
async fn filesystem_cache_delete_removes_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = FileSystemCache::new(tmp.path(), 100).unwrap();

    let data = Bytes::from_static(b"ephemeral");
    cache.set("del_key", data, None).await.unwrap();
    cache.delete("del_key").await.unwrap();

    assert_eq!(cache.get("del_key").await.unwrap(), None);
}
