//! Black-box coverage for the per-session ZCode gate, the lock every ZCode
//! spawn path takes. The lib target disables unit tests, so these run as an
//! integration test against the public API.
//!
//! Semantics under test: invocations of the SAME session in the same
//! checkout serialize (double-launch protection across processes), while
//! different sessions in the same checkout — the supported parallel-sessions
//! workflow — never block each other.

use std::fs;
use std::time::Duration;

use codex_file_system::acquire_zcode_session_in;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::timeout;

/// How long a second acquirer is given to wrongly slip past a held permit.
/// Slowness can only make the acquirer stay blocked longer, so any success
/// inside this window means the lock failed to exclude.
const STILL_BLOCKED_WINDOW: Duration = Duration::from_millis(500);

/// Generous ceiling for acquisitions expected to succeed.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test]
async fn second_acquirer_for_same_session_waits_for_permit_drop() {
    let lock_dir = TempDir::new().unwrap();
    let checkout = TempDir::new().unwrap();
    let cwd = checkout.path().to_string_lossy().into_owned();

    let first = acquire_zcode_session_in(lock_dir.path().to_path_buf(), &cwd, "thread-a")
        .await
        .unwrap();
    let mut second = tokio::spawn({
        let lock_dir = lock_dir.path().to_path_buf();
        let cwd = cwd.clone();
        async move { acquire_zcode_session_in(lock_dir, &cwd, "thread-a").await }
    });

    assert!(
        timeout(STILL_BLOCKED_WINDOW, &mut second).await.is_err(),
        "second acquirer of the same session entered while the permit was held"
    );

    drop(first);
    let _ = timeout(ACQUIRE_TIMEOUT, second)
        .await
        .expect("second acquirer never unblocked after the permit was dropped")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn same_checkout_spelled_differently_shares_one_slot() {
    let lock_dir = TempDir::new().unwrap();
    let checkout = TempDir::new().unwrap();
    let cwd = checkout.path().to_string_lossy().into_owned();

    let first = acquire_zcode_session_in(lock_dir.path().to_path_buf(), &cwd, "thread-a")
        .await
        .unwrap();
    let mut second = tokio::spawn({
        let lock_dir = lock_dir.path().to_path_buf();
        async move {
            acquire_zcode_session_in(lock_dir, &format!("{cwd}/"), "thread-a")
                .await
                .unwrap()
        }
    });

    assert!(
        timeout(STILL_BLOCKED_WINDOW, &mut second).await.is_err(),
        "trailing-slash spelling bypassed the session lock"
    );

    drop(first);
    let _ = timeout(ACQUIRE_TIMEOUT, second)
        .await
        .expect("spelling-variant acquirer never unblocked")
        .unwrap();
}

/// The supported parallel-sessions workflow: two different ZCode sessions in
/// the same checkout must hold their slots at the same time.
#[tokio::test]
async fn different_sessions_in_same_checkout_hold_their_slots_concurrently() {
    let lock_dir = TempDir::new().unwrap();
    let checkout = TempDir::new().unwrap();
    let cwd = checkout.path().to_string_lossy().into_owned();

    let _a = acquire_zcode_session_in(lock_dir.path().to_path_buf(), &cwd, "thread-a")
        .await
        .unwrap();
    let _b = timeout(
        ACQUIRE_TIMEOUT,
        acquire_zcode_session_in(lock_dir.path().to_path_buf(), &cwd, "thread-b"),
    )
    .await
    .expect("a different session was blocked by a held permit in the same checkout")
    .unwrap();
}

#[tokio::test]
async fn different_checkouts_hold_their_slots_concurrently() {
    let lock_dir = TempDir::new().unwrap();
    let checkout_a = TempDir::new().unwrap();
    let checkout_b = TempDir::new().unwrap();

    let _a = acquire_zcode_session_in(
        lock_dir.path().to_path_buf(),
        &checkout_a.path().to_string_lossy(),
        "thread-a",
    )
    .await
    .unwrap();
    // Must succeed promptly even though checkout A's permit is still held.
    let _b = timeout(
        ACQUIRE_TIMEOUT,
        acquire_zcode_session_in(
            lock_dir.path().to_path_buf(),
            &checkout_b.path().to_string_lossy(),
            "thread-a",
        ),
    )
    .await
    .expect("unrelated checkout blocked on a held permit")
    .unwrap();
}

#[tokio::test]
async fn each_session_and_checkout_pair_gets_its_own_hex_named_lockfile() {
    let lock_dir = TempDir::new().unwrap();
    let checkout_a = TempDir::new().unwrap();
    let checkout_b = TempDir::new().unwrap();

    for (checkout, session) in [
        (checkout_a.path(), "thread-a"),
        (checkout_b.path(), "thread-b"),
    ] {
        let _permit = acquire_zcode_session_in(
            lock_dir.path().to_path_buf(),
            &checkout.to_string_lossy(),
            session,
        )
        .await
        .unwrap();
    }

    let mut names: Vec<String> = fs::read_dir(lock_dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names.len(),
        2,
        "expected one lockfile per session/checkout pair"
    );
    for name in &names {
        let (key, extension) = name.split_at(name.len() - ".lock".len());
        assert_eq!(extension, ".lock");
        assert_eq!(key.len(), 64, "lock key is not a sha256 hex digest");
        assert!(
            key.bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')),
            "lock key {key} is not lowercase hex"
        );
    }
    assert_ne!(
        names[0], names[1],
        "two session/checkout pairs shared one lockfile"
    );
}
