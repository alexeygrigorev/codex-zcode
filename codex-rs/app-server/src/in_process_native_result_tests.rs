use super::*;
#[test]
fn owned_native_observations_require_delivery() {
    let mut activity = codex_app_server_protocol::BridgeToolActivityNotification {
        native_result: None,
        thread_id: "t".to_owned(),
        turn_id: "u".to_owned(),
        call_id: "c".to_owned(),
        tool: "Bash".to_owned(),
        status: codex_app_server_protocol::BridgeToolActivityStatus::Started,
        detail: None,
    };
    assert!(!server_notification_requires_delivery(
        &ServerNotification::BridgeToolActivity(activity.clone())
    ));
    activity.native_result = Some(
        codex_protocol::protocol::NativeToolObservation::Invalidated {
            reason: "native_stream_unqualified".to_owned(),
        },
    );
    assert!(server_notification_requires_delivery(
        &ServerNotification::BridgeToolActivity(activity)
    ));
}
