//! Tests for [`DiskEvictor`] — the shared, background eviction tracker.
//!
//! All tests use `DiskEvictor::manual()` so eviction is driven explicitly
//! by calling `scan()` and `evict()`.  This makes every test fully
//! deterministic — no sleeps, no polling, no timing dependence.

use streaming_engine::disk_evictor::DiskEvictor;
use tempfile::tempdir;
use tokio::fs;

/// Helper: write `n` files of `size` bytes each into `dir`, naming them
/// `0`, `1`, … and setting ascending mtimes via `filetime` so eviction
/// order is deterministic even on fast machines.  Returns total bytes.
async fn seed_files(dir: &std::path::Path, n: usize, size: usize) -> u64 {
    let data = vec![0xABu8; size];
    for i in 0..n {
        fs::write(dir.join(i.to_string()), &data).await.unwrap();
        // Stagger mtime so oldest-first ordering is unambiguous.
        let mtime = filetime::FileTime::from_unix_time((1_000_000 + i as i64) * 60, 0);
        filetime::set_file_mtime(dir.join(i.to_string()), mtime).unwrap();
    }
    (n * size) as u64
}

/// Count non-skipped files in a directory (sync, for assertions).
fn count_files(dir: &std::path::Path, skip_ext: Option<&str>) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .filter(|e| {
            let Ok(e) = e else { return false };
            if let Some(ext) = skip_ext {
                return e.path().extension().and_then(|x| x.to_str()) != Some(ext);
            }
            true
        })
        .count()
}

// ─── Counter basics ──────────────────────────────────────────────────────────

/// `track_delete` must saturate at zero, never wrap to `u64::MAX`.
#[tokio::test]
async fn track_delete_saturates_at_zero() {
    let tmp = tempdir().unwrap();
    let ev = DiskEvictor::manual(tmp.path().to_path_buf(), 1024, None);

    ev.track_write(100);
    ev.track_delete(200); // more than was written
    assert_eq!(ev.current_bytes(), 0, "counter must saturate, not wrap");
}

/// Repeated deletes past zero still stay at zero.
#[tokio::test]
async fn repeated_deletes_stay_at_zero() {
    let tmp = tempdir().unwrap();
    let ev = DiskEvictor::manual(tmp.path().to_path_buf(), 1024, None);

    for _ in 0..10 {
        ev.track_delete(999);
    }
    assert_eq!(ev.current_bytes(), 0);
}

/// `track_write` followed by exact `track_delete` gives zero.
#[tokio::test]
async fn write_then_delete_exact() {
    let tmp = tempdir().unwrap();
    let ev = DiskEvictor::manual(tmp.path().to_path_buf(), 1024, None);

    ev.track_write(500);
    ev.track_delete(500);
    assert_eq!(ev.current_bytes(), 0);
}

// ─── Scan ────────────────────────────────────────────────────────────────────

/// `scan()` sets the counter to the actual total on disk.
#[tokio::test]
async fn scan_initialises_counter() {
    let tmp = tempdir().unwrap();
    let total = seed_files(tmp.path(), 3, 100).await;

    let ev = DiskEvictor::manual(tmp.path().to_path_buf(), 10_000, None);
    ev.scan().await.unwrap();

    assert_eq!(ev.current_bytes(), total);
}

/// `scan()` with `skip_ext` excludes matching files from the counter.
#[tokio::test]
async fn scan_skips_extension() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("a"), vec![0u8; 100])
        .await
        .unwrap();
    fs::write(tmp.path().join("a.meta"), b"12345")
        .await
        .unwrap();
    fs::write(tmp.path().join("b"), vec![0u8; 200])
        .await
        .unwrap();

    let ev = DiskEvictor::manual(tmp.path().to_path_buf(), 10_000, Some("meta"));
    ev.scan().await.unwrap();

    assert_eq!(ev.current_bytes(), 300); // 100 + 200, .meta skipped
}

/// `scan()` on a non-existent directory returns 0, not an error.
#[tokio::test]
async fn scan_nonexistent_dir_is_zero() {
    let ev = DiskEvictor::manual("/tmp/does_not_exist_evictor_test".into(), 1024, None);
    ev.scan().await.unwrap();
    assert_eq!(ev.current_bytes(), 0);
}

// ─── Eviction ────────────────────────────────────────────────────────────────

/// `evict()` removes the oldest files until total is at or below `max_bytes`.
#[tokio::test]
async fn evict_removes_oldest_files() {
    let tmp = tempdir().unwrap();
    // 5 × 100 = 500 bytes, limit = 250
    seed_files(tmp.path(), 5, 100).await;

    let ev = DiskEvictor::manual(tmp.path().to_path_buf(), 250, None);
    ev.scan().await.unwrap();
    assert_eq!(ev.current_bytes(), 500);

    let freed = ev.evict().await.unwrap();
    assert_eq!(freed, 300); // must delete 3 files (oldest first)
    assert_eq!(ev.current_bytes(), 200);
    assert_eq!(count_files(tmp.path(), None), 2);

    // The two survivors should be the newest files: "3" and "4"
    assert!(tmp.path().join("3").exists());
    assert!(tmp.path().join("4").exists());
}

/// `evict()` is a no-op when already under the limit.
#[tokio::test]
async fn evict_noop_when_under_limit() {
    let tmp = tempdir().unwrap();
    seed_files(tmp.path(), 2, 50).await; // 100 bytes, limit = 1000

    let ev = DiskEvictor::manual(tmp.path().to_path_buf(), 1000, None);
    ev.scan().await.unwrap();

    let freed = ev.evict().await.unwrap();
    assert_eq!(freed, 0);
    assert_eq!(count_files(tmp.path(), None), 2);
}

/// Eviction with companion `.meta` sidecars: evicted data files' `.meta`
/// companions are also deleted.
#[tokio::test]
async fn evict_removes_companion_meta_files() {
    let tmp = tempdir().unwrap();
    for i in 0..4u8 {
        let name = format!("f{i}");
        fs::write(tmp.path().join(&name), vec![0xABu8; 100])
            .await
            .unwrap();
        fs::write(tmp.path().join(format!("{name}.meta")), b"999999999999")
            .await
            .unwrap();
        let mtime = filetime::FileTime::from_unix_time((1_000_000 + i as i64) * 60, 0);
        filetime::set_file_mtime(tmp.path().join(&name), mtime).unwrap();
    }

    // 4 × 100 = 400 counted bytes, limit = 150
    let ev = DiskEvictor::manual(tmp.path().to_path_buf(), 150, Some("meta"));
    ev.scan().await.unwrap();
    assert_eq!(ev.current_bytes(), 400);

    let freed = ev.evict().await.unwrap();
    assert_eq!(freed, 300); // 3 data files evicted
    assert_eq!(ev.current_bytes(), 100);

    // Only 1 data file + its .meta companion should remain
    let data = count_files(tmp.path(), Some("meta"));
    let metas = count_files(tmp.path(), None) - data;
    assert_eq!(data, 1);
    assert_eq!(
        metas, 1,
        "surviving data file should keep its .meta companion"
    );

    // The survivor should be the newest: f3
    assert!(tmp.path().join("f3").exists());
    assert!(tmp.path().join("f3.meta").exists());
}

/// `max_bytes = 0` evicts every counted file.
#[tokio::test]
async fn evict_with_max_zero_removes_everything() {
    let tmp = tempdir().unwrap();
    seed_files(tmp.path(), 3, 50).await;

    let ev = DiskEvictor::manual(tmp.path().to_path_buf(), 0, None);
    ev.scan().await.unwrap();
    assert_eq!(ev.current_bytes(), 150);

    let freed = ev.evict().await.unwrap();
    assert_eq!(freed, 150);
    assert_eq!(ev.current_bytes(), 0);
    assert_eq!(count_files(tmp.path(), None), 0);
}

// ─── Counter + eviction interaction ──────────────────────────────────────────

/// `track_write` then `evict` keeps the counter consistent.
#[tokio::test]
async fn track_write_then_evict_counter_stays_consistent() {
    let tmp = tempdir().unwrap();
    let ev = DiskEvictor::manual(tmp.path().to_path_buf(), 100, None);

    // Simulate writes the way the cache layer does: write file, then track.
    for i in 0..5u32 {
        fs::write(tmp.path().join(i.to_string()), vec![0u8; 50])
            .await
            .unwrap();
        let mtime = filetime::FileTime::from_unix_time((1_000_000 + i as i64) * 60, 0);
        filetime::set_file_mtime(tmp.path().join(i.to_string()), mtime).unwrap();
        ev.track_write(50);
    }
    assert_eq!(ev.current_bytes(), 250);

    let freed = ev.evict().await.unwrap();
    assert_eq!(freed, 150); // delete 3 oldest (3×50) to get to 100
    assert_eq!(ev.current_bytes(), 100);
    assert_eq!(count_files(tmp.path(), None), 2);
}

/// Calling `evict()` when on-disk reality differs from counter (e.g.
/// external deletion) still works — eviction frees what it can and the
/// counter is reduced by the actual freed amount.
#[tokio::test]
async fn evict_after_external_deletion() {
    let tmp = tempdir().unwrap();
    seed_files(tmp.path(), 4, 100).await;

    let ev = DiskEvictor::manual(tmp.path().to_path_buf(), 200, None);
    ev.scan().await.unwrap();
    assert_eq!(ev.current_bytes(), 400);

    // Externally delete file "0" without telling the evictor
    fs::remove_file(tmp.path().join("0")).await.unwrap();

    // Counter is stale (400) but disk only has 300.
    // Eviction scans the actual directory, so it should still bring
    // disk usage to ≤ 200.
    let freed = ev.evict().await.unwrap();
    assert_eq!(freed, 100); // only needed to delete one more file
    assert_eq!(count_files(tmp.path(), None), 2);
}

// ─── Background-mode smoke test ──────────────────────────────────────────────

/// Quick sanity check that the background constructor doesn't panic and the
/// shutdown path works.
#[tokio::test]
async fn background_mode_constructs_and_drops() {
    let tmp = tempdir().unwrap();
    let ev = DiskEvictor::new(tmp.path().to_path_buf(), 1024, None);
    // Just verify basic operations work
    ev.track_write(10);
    ev.track_delete(5);
    assert_eq!(ev.current_bytes(), 5);
    drop(ev);
    // If the background task leaked we would see this in sanitiser runs,
    // but the test completing without hanging is the primary assertion.
}
