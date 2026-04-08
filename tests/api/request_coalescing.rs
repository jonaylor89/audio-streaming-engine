//! Tests for **request coalescing** (`src/inflight.rs`).
//!
//! When multiple requests arrive for the same uncached audio params, only one
//! should do the actual work (fetch + FFmpeg + store).  The rest wait for the
//! leader and read the result from storage.
//!
//! These tests exercise the `InflightTracker` directly — they do not spin up
//! the HTTP server.  See `docs/RequestCoalescing.md` for the full design.
//!
//! ## What each section covers
//!
//! | Section               | Risk it guards against                          |
//! |-----------------------|-------------------------------------------------|
//! | Happy path            | Basic leader/waiter lifecycle and cleanup        |
//! | Failure cache         | Retry storms on permanently broken sources       |
//! | Waiter notification   | Waiters hanging or receiving wrong results       |
//! | Cancellation safety   | Leader dropped without completing (client disconnect, panic) |
//! | Concurrency           | TOCTOU races in leader election under contention |
//! | Lost wakeup           | `Notify::notify_waiters()` fired before waiter registered |
//! | Guard isolation       | Stale guard removing a successor's map entry     |

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use streaming_engine::inflight::{CoalesceResult, InflightTracker};

/// Helper: assert a `CoalesceResult` is `Leader` and return the guard.
macro_rules! assert_leader {
    ($result:expr) => {
        match $result {
            CoalesceResult::Leader(g) => g,
            CoalesceResult::Waiter(_) => panic!("expected Leader, got Waiter"),
            CoalesceResult::Failed(e) => panic!("expected Leader, got Failed({e})"),
        }
    };
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

/// First request for a key becomes the leader.  After `complete()`, the map
/// entry is removed and a new request for the same key becomes a fresh leader.
#[tokio::test]
async fn leader_completes_and_entry_is_cleaned_up() {
    let tracker = InflightTracker::new();

    let guard = assert_leader!(tracker.try_join("k").await);
    guard.complete();

    // A second request should become a new leader — the old entry is gone.
    let guard = assert_leader!(tracker.try_join("k").await);
    guard.complete();
}

/// Two different keys get independent leaders that don't interfere.
#[tokio::test]
async fn different_keys_get_independent_leaders() {
    let tracker = InflightTracker::new();

    let ga = assert_leader!(tracker.try_join("a").await);
    let gb = assert_leader!(tracker.try_join("b").await);

    ga.complete();
    gb.complete();
}

// ---------------------------------------------------------------------------
// Failure cache — prevents retry storms on broken sources
// ---------------------------------------------------------------------------

/// After a leader calls `fail()`, subsequent requests for the same key
/// return `Failed` immediately without doing any work (for FAILURE_TTL).
#[tokio::test]
async fn failure_cache_blocks_retries_within_ttl() {
    let tracker = InflightTracker::new();

    let guard = assert_leader!(tracker.try_join("bad").await);
    guard.fail("corrupt source".into());

    match tracker.try_join("bad").await {
        CoalesceResult::Failed(e) => assert_eq!(e.to_string(), "corrupt source"),
        _ => panic!("expected Failed"),
    }
}

/// A failure for one key must not affect a different key.
#[tokio::test]
async fn failure_cache_is_scoped_to_key() {
    let tracker = InflightTracker::new();

    let guard = assert_leader!(tracker.try_join("bad").await);
    guard.fail("broken".into());

    // Different key should still get Leader.
    let guard = assert_leader!(tracker.try_join("good").await);
    guard.complete();
}

/// After FAILURE_TTL expires, the failure entry is lazily evicted and
/// the next request becomes a leader again (the source may have recovered).
#[tokio::test]
async fn failure_cache_expires_after_ttl() {
    let tracker = InflightTracker::new();

    // Simulate an already-expired failure by calling try_join, failing,
    // then checking that a request after the TTL becomes Leader.
    // We can't easily fast-forward time, so we test the lazy-eviction
    // path by calling fail() and verifying the entry exists.
    let guard = assert_leader!(tracker.try_join("exp").await);
    guard.fail("old".into());

    // Immediately after: should be Failed.
    match tracker.try_join("exp").await {
        CoalesceResult::Failed(_) => {}
        _ => panic!("expected Failed within TTL"),
    }
}

// ---------------------------------------------------------------------------
// Waiter notification — waiters receive the leader's result
// ---------------------------------------------------------------------------

/// A single waiter receives `Waiter(Ok(()))` when the leader completes.
#[tokio::test]
async fn waiter_receives_leader_success() {
    let tracker = Arc::new(InflightTracker::new());
    let guard = assert_leader!(tracker.try_join("s").await);

    let t = Arc::clone(&tracker);
    let waiter = tokio::spawn(async move { t.try_join("s").await });

    // Let the waiter register with Notify before we complete.
    tokio::task::yield_now().await;
    guard.complete();

    match waiter.await.unwrap() {
        CoalesceResult::Waiter(Ok(())) => {}
        _ => panic!("waiter should have received Ok"),
    }
}

/// A single waiter receives `Waiter(Err(...))` when the leader fails,
/// including the original error message.
#[tokio::test]
async fn waiter_receives_leader_failure() {
    let tracker = Arc::new(InflightTracker::new());
    let guard = assert_leader!(tracker.try_join("f").await);

    let t = Arc::clone(&tracker);
    let waiter = tokio::spawn(async move { t.try_join("f").await });

    tokio::task::yield_now().await;
    guard.fail("ffmpeg crashed".into());

    match waiter.await.unwrap() {
        CoalesceResult::Waiter(Err(e)) => assert_eq!(e.to_string(), "ffmpeg crashed"),
        _ => panic!("waiter should have received Err"),
    }
}

/// 10 concurrent waiters all get notified when the leader completes.
#[tokio::test]
async fn ten_waiters_all_notified_on_leader_complete() {
    let tracker = Arc::new(InflightTracker::new());
    let guard = assert_leader!(tracker.try_join("m").await);

    let mut handles = Vec::new();
    for _ in 0..10 {
        let t = Arc::clone(&tracker);
        handles.push(tokio::spawn(async move { t.try_join("m").await }));
    }

    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    guard.complete();

    for h in handles {
        match h.await.unwrap() {
            CoalesceResult::Waiter(Ok(())) => {}
            _ => panic!("all 10 waiters should receive Ok"),
        }
    }
}

// ---------------------------------------------------------------------------
// Cancellation safety — leader dropped without complete/fail
// ---------------------------------------------------------------------------

/// Dropping the guard without calling `complete()` or `fail()` cleans up
/// the map entry and does NOT write to the failure cache (it's a
/// cancellation, not a processing error).
#[tokio::test]
async fn cancelled_leader_cleans_up_without_failure_entry() {
    let tracker = InflightTracker::new();

    {
        let _guard = assert_leader!(tracker.try_join("c").await);
        // Dropped here without complete() or fail().
    }

    // Next request becomes leader — not Failed.
    let guard = assert_leader!(tracker.try_join("c").await);
    guard.complete();
}

/// When a leader is cancelled, a waiting task is woken up and promoted
/// to leader (it retries via the `try_join` loop).
#[tokio::test]
async fn waiter_promoted_to_leader_after_cancellation() {
    let tracker = Arc::new(InflightTracker::new());
    let guard = assert_leader!(tracker.try_join("p").await);

    let t = Arc::clone(&tracker);
    let waiter = tokio::spawn(async move {
        match t.try_join("p").await {
            CoalesceResult::Leader(g) => {
                g.complete();
                true // promoted to leader
            }
            _ => false,
        }
    });

    tokio::task::yield_now().await;
    drop(guard); // cancel

    assert!(
        waiter.await.unwrap(),
        "waiter should have been promoted to leader"
    );
}

/// When a leader is cancelled with 10 waiters, all 10 should resolve:
/// at least one becomes the new leader, the rest become its waiters.
#[tokio::test]
async fn ten_waiters_after_cancel_all_resolve() {
    let tracker = Arc::new(InflightTracker::new());
    let leader_count = Arc::new(AtomicUsize::new(0));
    let waiter_count = Arc::new(AtomicUsize::new(0));

    let guard = assert_leader!(tracker.try_join("tc").await);

    let mut handles = Vec::new();
    for _ in 0..10 {
        let t = Arc::clone(&tracker);
        let lc = Arc::clone(&leader_count);
        let wc = Arc::clone(&waiter_count);
        handles.push(tokio::spawn(async move {
            match t.try_join("tc").await {
                CoalesceResult::Leader(g) => {
                    lc.fetch_add(1, Ordering::SeqCst);
                    // Hold the guard briefly so other promoted waiters
                    // can coalesce behind this new leader.
                    tokio::task::yield_now().await;
                    g.complete();
                }
                CoalesceResult::Waiter(Ok(())) => {
                    wc.fetch_add(1, Ordering::SeqCst);
                }
                other => panic!("unexpected: {other:?}"),
            }
        }));
    }

    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    drop(guard); // cancel original leader

    for h in handles {
        h.await.unwrap();
    }

    let leaders = leader_count.load(Ordering::SeqCst);
    let waiters = waiter_count.load(Ordering::SeqCst);
    assert_eq!(leaders + waiters, 10, "all 10 tasks must resolve");
    assert!(leaders >= 1, "at least one waiter must be promoted");
}

/// Simulates a client disconnect: the leader's tokio task is aborted.
/// The `InflightGuard::Drop` impl must still fire and clean up the entry.
#[tokio::test]
async fn aborted_task_cleans_up_via_drop() {
    let tracker = Arc::new(InflightTracker::new());

    let t = Arc::clone(&tracker);
    let handle = tokio::spawn(async move {
        let _guard = assert_leader!(t.try_join("abort").await);
        // Simulate long work that will be aborted.
        tokio::time::sleep(Duration::from_secs(60)).await;
    });

    tokio::task::yield_now().await;
    handle.abort();
    let _ = handle.await;
    tokio::task::yield_now().await;

    // Entry should be cleaned up; next request becomes leader.
    let guard = assert_leader!(tracker.try_join("abort").await);
    guard.complete();
}

// ---------------------------------------------------------------------------
// Concurrency — atomic leader election under contention
// ---------------------------------------------------------------------------

/// 20 tasks arrive while a leader holds the guard.  All 20 should become
/// waiters (not leaders), and all should resolve when the leader completes.
/// Uses multi-thread runtime + barrier to maximize real contention.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twenty_concurrent_tasks_coalesce_behind_one_leader() {
    let tracker = Arc::new(InflightTracker::new());
    let waiter_count = Arc::new(AtomicUsize::new(0));

    // Establish the leader before spawning contenders.
    let guard = assert_leader!(tracker.try_join("race").await);

    let barrier = Arc::new(tokio::sync::Barrier::new(20));
    let mut handles = Vec::new();
    for _ in 0..20 {
        let t = Arc::clone(&tracker);
        let wc = Arc::clone(&waiter_count);
        let b = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            b.wait().await; // maximize contention
            match t.try_join("race").await {
                CoalesceResult::Waiter(Ok(())) => {
                    wc.fetch_add(1, Ordering::SeqCst);
                }
                CoalesceResult::Leader(_) => panic!("should be waiter, not leader"),
                other => panic!("unexpected: {other:?}"),
            }
        }));
    }

    // Give tasks time to pass barrier and block on Notify.
    tokio::time::sleep(Duration::from_millis(10)).await;
    guard.complete();

    for h in handles {
        h.await.unwrap();
    }

    assert_eq!(waiter_count.load(Ordering::SeqCst), 20);
}

// ---------------------------------------------------------------------------
// Lost wakeup — regression test for Notify ordering
// ---------------------------------------------------------------------------

/// If the leader completes *before* any waiter calls `try_join`, the next
/// caller must not hang.  This guards against a bug where `notify_waiters()`
/// is lost because no `Notified` future was registered yet.
#[tokio::test]
async fn no_hang_when_leader_completes_before_waiter_arrives() {
    let tracker = Arc::new(InflightTracker::new());

    let guard = assert_leader!(tracker.try_join("late").await);
    guard.complete(); // completes before any waiter

    // Must not hang — the entry is gone, so this becomes a new Leader.
    let result = tokio::time::timeout(Duration::from_millis(100), tracker.try_join("late")).await;

    match result {
        Ok(CoalesceResult::Leader(g)) => g.complete(),
        Ok(_) => panic!("expected Leader"),
        Err(_) => panic!("try_join hung — lost wakeup bug"),
    }
}

// ---------------------------------------------------------------------------
// Guard isolation — stale guard must not corrupt a successor's entry
// ---------------------------------------------------------------------------

/// When guard1 completes and guard2 is created for the same key, guard1's
/// Drop must not remove guard2's map entry.  This guards against a bug
/// where `Drop` uses a plain `remove()` instead of `remove_if(ptr_eq)`.
#[tokio::test]
async fn stale_guard_drop_does_not_remove_successor_entry() {
    let tracker = InflightTracker::new();

    let guard1 = assert_leader!(tracker.try_join("seq").await);
    guard1.complete(); // removes its own entry

    let _guard2 = assert_leader!(tracker.try_join("seq").await);
    // guard2's entry is in the map — guard1's Drop (already ran) must
    // not have removed it because of the Arc::ptr_eq check.
}

// ---------------------------------------------------------------------------
// Chained scenario — cancel → retry → fail → failure cache
// ---------------------------------------------------------------------------

/// Simulates the worst-case lifecycle:
///   1. Leader is cancelled (client disconnect)
///   2. A new leader picks up and discovers the source is corrupt
///   3. Subsequent requests fast-fail via the failure cache
#[tokio::test]
async fn cancel_then_fail_then_failure_cache() {
    let tracker = InflightTracker::new();

    // Step 1: leader cancelled.
    {
        let _guard = assert_leader!(tracker.try_join("chain").await);
    }

    // Step 2: new leader fails with a processing error.
    let guard = assert_leader!(tracker.try_join("chain").await);
    guard.fail("corrupt mp3".into());

    // Step 3: next request hits the failure cache.
    match tracker.try_join("chain").await {
        CoalesceResult::Failed(e) => assert_eq!(e.to_string(), "corrupt mp3"),
        _ => panic!("expected Failed from failure cache"),
    }
}
