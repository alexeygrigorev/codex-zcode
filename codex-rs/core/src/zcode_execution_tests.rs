use super::ZcodeExecutionPolicy;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;

#[test]
fn admission_requires_the_captured_unrestricted_local_contract() {
    let workspace = AbsolutePathBuf::try_from(std::env::current_dir().unwrap()).unwrap();
    for (approval_policy, permission_profile, local_workspace, admitted) in [
        (
            AskForApproval::Never,
            PermissionProfile::Disabled,
            Some(workspace.clone()),
            true,
        ),
        (
            AskForApproval::OnRequest,
            PermissionProfile::Disabled,
            Some(workspace.clone()),
            false,
        ),
        (
            AskForApproval::Never,
            PermissionProfile::read_only(),
            Some(workspace.clone()),
            false,
        ),
        (
            AskForApproval::Never,
            PermissionProfile::Disabled,
            None,
            false,
        ),
    ] {
        let policy = ZcodeExecutionPolicy {
            approval_policy,
            permission_profile,
            workspace: local_workspace,
        };
        assert_eq!(
            policy.admitted_workspace().ok(),
            admitted.then_some(&workspace)
        );
    }
}

#[tokio::test]
async fn dropping_warm_slot_holds_checkout_until_child_is_reaped() {
    use crate::zcode_warm::SessionSeed;
    use crate::zcode_warm::WarmMode;
    use crate::zcode_warm::ZcodeWarmBridge;
    use crate::zcode_warm_permissions::WarmPermissionPolicy;
    use std::sync::Arc;
    use std::time::Duration;

    let work = tempfile::TempDir::new().unwrap();
    let workspace_path = work.path().to_string_lossy().into_owned();
    let fixture = crate::zcode_warm::tests::write_fake_server();
    let bridge = ZcodeWarmBridge::spawn(
        &crate::zcode_warm::tests::test_runtime(&fixture),
        &workspace_path,
        WarmMode::Build,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Fresh,
    )
    .unwrap();
    let checkout = crate::zcode_process::acquire_zcode_session(&workspace_path, "drop-test")
        .await
        .unwrap();
    bridge
        .prepare_session(&workspace_path, WarmMode::Build)
        .await
        .unwrap();
    drop(super::super::ZcodeWarmSlotEntry {
        workspace: AbsolutePathBuf::try_from(work.path().to_path_buf()).unwrap(),
        permission_policy: WarmPermissionPolicy::DenyAll,
        bridge: Arc::clone(&bridge),
        checkout: Some(checkout),
    });
    let _next_checkout = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 5),
        crate::zcode_process::acquire_zcode_session(&workspace_path, "next-test"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        (
            bridge.is_dead(),
            bridge.take_child_for_retirement().is_none()
        ),
        (true, true),
    );
}

#[tokio::test]
async fn cold_goal_continuation_preserves_history_and_steering_before_reporting_completion() {
    use codex_api::ResponseEvent;
    use codex_protocol::models::ResponseItem;
    use codex_tools::JsonSchema;
    use codex_tools::ResponsesApiTool;
    use codex_tools::ToolSpec;
    use std::collections::BTreeMap;

    let work = tempfile::TempDir::new().unwrap();
    let fixture = crate::zcode_warm::tests::write_fake_server();
    let source = std::fs::read_to_string(&fixture).unwrap();
    std::fs::write(
        &fixture,
        source.replace(
            "function runTurn(sessionId, content) {",
            r#"function runTurn(sessionId, content) {
                fs.writeFileSync(process.argv[1] + ".received", JSON.stringify(content));
                content = "done\n[GOAL:COMPLETE]";"#,
        ),
    )
    .unwrap();
    let user_message = |text: &str| ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![codex_protocol::models::ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let continuation = "New steering: do not repeat the migration; verify the receipt only.\n<objective>Finish the migration safely</objective>";
    let prompt = crate::Prompt {
        input: vec![
            user_message("Migrate the records once."),
            ResponseItem::FunctionCall {
                id: None,
                name: "exec_command".to_string(),
                namespace: None,
                arguments: r#"{"cmd":"migrate-once"}"#.to_string(),
                encrypted_function_args: None,
                call_id: "migration-effect".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: Some("migration-effect".to_string()),
                name: None,
                namespace: None,
                output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                    "Migration committed; receipt 731.".to_string(),
                ),
                internal_chat_message_metadata_passthrough: None,
            },
            crate::zcode_warm::assistant_message("Migration already committed.".to_string()),
            user_message(continuation),
        ],
        tools: vec![ToolSpec::Function(ResponsesApiTool {
            name: "update_goal".to_string(),
            description: String::new(),
            strict: false,
            parameters: JsonSchema::object(
                BTreeMap::new(),
                /*required*/ Some(Vec::new()),
                /*additional_properties*/ Some(false.into()),
            ),
            output_schema: None,
            defer_loading: None,
        })]
        .into(),
        zcode_execution: ZcodeExecutionPolicy {
            approval_policy: AskForApproval::Never,
            permission_profile: PermissionProfile::Disabled,
            workspace: Some(AbsolutePathBuf::try_from(work.path().to_path_buf()).unwrap()),
        },
        ..Default::default()
    };
    let mut stream = super::stream_cold(
        crate::zcode_warm::tests::test_runtime(&fixture),
        &prompt,
        "goal-test",
    )
    .await
    .unwrap();
    let mut outputs = Vec::new();
    while let Some(event) = stream.rx_event.recv().await {
        match event.unwrap() {
            ResponseEvent::OutputItemDone(item) => outputs.push(item),
            ResponseEvent::Completed { .. } => break,
            _ => {}
        }
    }
    assert_eq!(outputs.len(), 2);
    assert_eq!(
        outputs.remove(0),
        crate::zcode_warm::assistant_message("done".to_string())
    );
    let ResponseItem::FunctionCall {
        name, arguments, ..
    } = outputs.remove(0)
    else {
        panic!("expected goal completion call");
    };
    assert_eq!(
        (name.as_str(), arguments.as_str()),
        ("update_goal", r#"{"status":"complete"}"#)
    );
    let received_path = std::path::PathBuf::from(format!("{}.received", fixture.display()));
    let received: String =
        serde_json::from_str(&std::fs::read_to_string(&received_path).unwrap()).unwrap();
    std::fs::remove_file(received_path).unwrap();
    let expected_history = format!(
        "user: Migrate the records once.\n[tool call exec_command] {{\"cmd\":\"migrate-once\"}}\n[tool result for migration-effect] Migration committed; receipt 731.\nassistant: Migration already committed.\nuser: {continuation}\n"
    );
    assert_eq!(
        received,
        format!(
            "{expected_history}\n{}{}",
            super::super::zcode_goal_status_protocol(&prompt.tools).unwrap(),
            super::super::ZCODE_REPLY_BUDGET_NOTE,
        ),
    );
    let stats = std::fs::read_to_string(crate::zcode_warm::tests::stats_path(&fixture)).unwrap();
    assert_eq!(
        stats
            .lines()
            .filter(|line| line.starts_with("send:"))
            .count(),
        1
    );
    assert_eq!(stats.lines().last(), Some("close"));
}

#[tokio::test]
async fn aborted_cold_consumer_retires_before_releasing_checkout() {
    use std::time::Duration;
    let work = tempfile::TempDir::new().unwrap();
    let workspace = AbsolutePathBuf::try_from(work.path().to_path_buf()).unwrap();
    let fixture = crate::zcode_warm::tests::write_fake_server();
    let source = std::fs::read_to_string(&fixture).unwrap();
    std::fs::write(&fixture, source.replace(
        "function runTurn(sessionId, content) {",
        r#"function runTurn(sessionId, content) {
          emit({ sessionId, type: "model.streaming", payload: { kind: "text_delta", delta: "started" } });
          return;"#,
    )).unwrap();
    let prompt = crate::Prompt {
        zcode_execution: ZcodeExecutionPolicy {
            approval_policy: AskForApproval::Never,
            permission_profile: PermissionProfile::Disabled,
            workspace: Some(workspace.clone()),
        },
        ..Default::default()
    };
    let mut stream = super::stream_cold(
        crate::zcode_warm::tests::test_runtime(&fixture),
        &prompt,
        "abort-test",
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), stream.rx_event.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(stream);
    let _next_checkout = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 5),
        crate::zcode_process::acquire_zcode_session(&workspace.to_string_lossy(), "after-abort"),
    )
    .await
    .unwrap()
    .unwrap();
    let stats = std::fs::read_to_string(crate::zcode_warm::tests::stats_path(&fixture)).unwrap();
    assert!(stats.lines().any(|line| line == "close"), "{stats}");
}
