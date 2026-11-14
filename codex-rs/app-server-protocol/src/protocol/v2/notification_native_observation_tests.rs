use super::*;
#[test]
fn native_result_never_enters_public_schema_or_typescript() {
    let schema = schemars::schema_for!(BridgeToolActivityNotification);
    let encoded = serde_json::to_string(&schema).unwrap();
    assert!(!encoded.contains("native_result"));
    assert!(!encoded.contains("nativeResult"));
    assert!(!encoded.contains("NativeToolObservation"));
    let declaration = BridgeToolActivityNotification::decl();
    assert!(!declaration.contains("native_result"));
    assert!(!declaration.contains("nativeResult"));
    assert!(!declaration.contains("NativeToolObservation"));
}
