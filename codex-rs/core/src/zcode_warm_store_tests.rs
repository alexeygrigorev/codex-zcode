use std::path::Path;

use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

use crate::zcode_warm_store::ZcodeWarmRecord;
use crate::zcode_warm_store::load_record_from;
use crate::zcode_warm_store::protocol_drift_message;
use crate::zcode_warm_store::store_record_to;

fn test_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zcode-warm-store-{name}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn sample_record() -> ZcodeWarmRecord {
    ZcodeWarmRecord {
        zcode_session_id: "sess_abc123".to_string(),
        workspace_path: "/repo".to_string(),
        last_event_seq: 42,
        protocol_version: Some(1),
    }
}

fn record_path(dir: &Path, file: &str) -> std::path::PathBuf {
    dir.join(format!("{file}.json"))
}

#[test]
fn store_then_load_round_trips_the_record() {
    let dir = test_dir("round-trip");
    let thread_id = ThreadId::from_u128(0xF17E);
    store_record_to(&dir, &thread_id, &sample_record());
    assert_eq!(
        load_record_from(&dir, &thread_id),
        Some(sample_record()),
        "the record must survive a round trip unchanged"
    );
    assert!(
        !record_path(&dir, &format!("{thread_id}"))
            .with_extension("json.tmp")
            .exists(),
        "the staging file must not survive the rename"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn load_missing_record_is_none() {
    let dir = test_dir("missing");
    let thread_id = ThreadId::from_u128(0xF12E);
    assert_eq!(load_record_from(&dir, &thread_id), None);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn load_unparsable_record_is_none() {
    let dir = test_dir("unparsable");
    let thread_id = ThreadId::from_u128(0xF34E);
    let path = record_path(&dir, &format!("{thread_id}"));
    std::fs::write(&path, "{ not json").expect("write garbage record");
    assert_eq!(
        load_record_from(&dir, &thread_id),
        None,
        "an unparsable record must degrade to no record, not an error"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn store_overwrites_a_previous_record() {
    let dir = test_dir("overwrite");
    let thread_id = ThreadId::from_u128(0xF56E);
    store_record_to(&dir, &thread_id, &sample_record());
    let replaced = ZcodeWarmRecord {
        zcode_session_id: "sess_fresh".to_string(),
        workspace_path: "/other".to_string(),
        last_event_seq: 0,
        protocol_version: None,
    };
    store_record_to(&dir, &thread_id, &replaced);
    assert_eq!(load_record_from(&dir, &thread_id), Some(replaced));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn protocol_version_change_produces_a_drift_warning() {
    let old = sample_record();
    let mut new = sample_record();
    assert_eq!(
        protocol_drift_message(&old, &new),
        None,
        "an unchanged version is not drift"
    );
    new.protocol_version = None;
    assert_eq!(
        protocol_drift_message(&old, &new),
        None,
        "an unknown new version carries no verdict"
    );
    new.protocol_version = Some(2);
    let message = protocol_drift_message(&old, &new).expect("drift must warn");
    assert!(
        message.contains("v1") && message.contains("v2") && message.contains("sess_abc123"),
        "the warning must name both versions and the session: {message}"
    );
}
