use std::collections::BTreeMap;
use std::path::Path;

use pretty_assertions::assert_eq;
use tokio::sync::mpsc;

use super::MIRROR_CALL_ID_PREFIX;
use super::goal_objective_from_prompt;
use super::mirrored_goal_objective_for;
use super::turn_mirrored;
use crate::zcode_warm::SessionSeed;
use crate::zcode_warm::WarmMode;
use crate::zcode_warm::ZcodeWarmBridge;
use crate::zcode_warm::assistant_message;
use crate::zcode_warm::tests::FakeServerOptions;
use crate::zcode_warm::tests::GoalLoopMode;
use crate::zcode_warm::tests::next_event;
use crate::zcode_warm::tests::stats_path;
use crate::zcode_warm::tests::test_runtime;
use crate::zcode_warm::tests::write_fake_server_with;
use crate::zcode_warm_permissions::WarmPermissionPolicy;
use crate::zcode_warm_v4_send::TurnInputChannel;
use codex_api::ResponseEvent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;

fn user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn goal_tool_spec() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: "update_goal".to_string(),
        description: String::new(),
        strict: false,
        parameters: JsonSchema::object(BTreeMap::new(), Some(Vec::new()), Some(false.into())),
        output_schema: None,
        defer_loading: None,
    })
}

fn continuation_prompt(objective: &str) -> String {
    format!(
        "Continue working toward the active thread goal.\n\n<objective>\n{objective}\n\
         </objective>\n\nCompletion audit: verify before claiming done."
    )
}

#[test]
fn goal_objective_parses_from_the_continuation_template() {
    let input = vec![
        user_message("earlier user turn"),
        assistant_message("earlier reply".to_string()),
        user_message(&continuation_prompt("Ship the release notes")),
    ];
    assert_eq!(
        goal_objective_from_prompt(&input).as_deref(),
        Some("Ship the release notes")
    );
}

#[test]
fn goal_objective_ignores_prompts_without_the_template() {
    let input = vec![user_message(
        "plain user turn mentioning <objective> loosely",
    )];
    assert_eq!(goal_objective_from_prompt(&input), None);
    let empty_objective = vec![user_message("<objective>\n   \n</objective>")];
    assert_eq!(goal_objective_from_prompt(&empty_objective), None);
}

#[test]
fn mirror_selector_defaults_on_and_requires_goal_tool_and_objective() {
    let tools = vec![goal_tool_spec()];
    let input = vec![user_message(&continuation_prompt("Refactor the parser"))];
    assert_eq!(
        mirrored_goal_objective_for(Some("1"), &tools, &input).as_deref(),
        Some("Refactor the parser")
    );
    assert_eq!(
        mirrored_goal_objective_for(None, &tools, &input).as_deref(),
        Some("Refactor the parser"),
        "the mirror is on by default since the live validation (issue #50)"
    );
    assert_eq!(mirrored_goal_objective_for(Some("0"), &tools, &input), None);
    assert_eq!(
        mirrored_goal_objective_for(Some("false"), &tools, &input),
        None
    );
    assert_eq!(mirrored_goal_objective_for(Some(""), &tools, &input), None);

    let plain_tools: Vec<ToolSpec> = Vec::new();
    assert_eq!(
        mirrored_goal_objective_for(Some("1"), &plain_tools, &input),
        None
    );

    let no_template = vec![user_message("plain turn")];
    assert_eq!(
        mirrored_goal_objective_for(Some("1"), &tools, &no_template),
        None
    );
}

fn synthesized_goal_items(status: &str, call_id: &str) -> Vec<ResponseItem> {
    vec![
        ResponseItem::FunctionCall {
            id: None,
            name: "update_goal".to_string(),
            namespace: None,
            arguments: format!(r#"{{"status":"{status}"}}"#),
            encrypted_function_args: Some(Vec::new()),
            call_id: call_id.to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some(call_id.to_string()),
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload::from_text("goal recorded".to_string()),
            internal_chat_message_metadata_passthrough: None,
        },
    ]
}

#[test]
fn selector_skips_the_agent_loop_round_after_a_mirrored_completion() {
    let tools = vec![goal_tool_spec()];
    let answered_round = [
        vec![
            user_message(&continuation_prompt("Ship the release notes")),
            assistant_message("goal work done".to_string()),
        ],
        synthesized_goal_items("complete", &format!("{MIRROR_CALL_ID_PREFIX}req")),
    ]
    .concat();
    assert_eq!(
        mirrored_goal_objective_for(Some("1"), &tools, &answered_round),
        None,
        "the function-output round must degrade to a plain warm turn, not \
         restart the settled core goal loop"
    );

    // A goal re-set after the completion arrives as a newer continuation
    // template and must mirror again.
    let mut reactivated = answered_round;
    reactivated.push(user_message(&continuation_prompt("Ship the changelog")));
    assert_eq!(
        mirrored_goal_objective_for(Some("1"), &tools, &reactivated).as_deref(),
        Some("Ship the changelog")
    );
}

#[test]
fn selector_skips_the_agent_loop_round_after_a_capped_block() {
    let tools = vec![goal_tool_spec()];
    let answered_round = [
        vec![
            user_message(&continuation_prompt("Finish it by 2027")),
            assistant_message("capped-cycle work".to_string()),
        ],
        synthesized_goal_items("blocked", &format!("{MIRROR_CALL_ID_PREFIX}req")),
    ]
    .concat();
    assert_eq!(
        mirrored_goal_objective_for(Some("1"), &tools, &answered_round),
        None,
        "the round after a capped block must degrade to a plain warm turn; \
         re-mirroring the same objective is what loops forever (issue #53)"
    );
}

async fn mirrored_turn(
    fixture: &Path,
    objective: &str,
) -> (
    mpsc::Receiver<std::result::Result<ResponseEvent, codex_api::ApiError>>,
    std::sync::Arc<ZcodeWarmBridge>,
) {
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");
    let stream = turn_mirrored(&bridge, &session_id, objective)
        .await
        .expect("mirror starts");
    (stream.rx_event, bridge)
}

#[tokio::test]
async fn mirrored_goal_turn_sets_the_goal_and_reports_verified_completion() {
    let fixture = write_fake_server_with(FakeServerOptions {
        goal_loop: GoalLoopMode::Complete,
        ..FakeServerOptions::default()
    });
    let (mut rx, bridge) = mirrored_turn(&fixture, "Write the epic poem").await;

    match next_event(&mut rx).await {
        ResponseEvent::OutputItemAdded(item) => assert_eq!(item, assistant_message(String::new())),
        other => panic!("expected OutputItemAdded, got {other:?}"),
    }
    match next_event(&mut rx).await {
        ResponseEvent::OutputTextDelta(delta) => assert_eq!(delta, "go"),
        other => panic!("expected OutputTextDelta, got {other:?}"),
    }
    match next_event(&mut rx).await {
        ResponseEvent::OutputTextDelta(delta) => assert_eq!(delta, "al"),
        other => panic!("expected OutputTextDelta, got {other:?}"),
    }
    match next_event(&mut rx).await {
        ResponseEvent::OutputItemDone(item) => {
            assert_eq!(item, assistant_message("goal".to_string()))
        }
        other => panic!("expected the turn reply, got {other:?}"),
    }
    // The verified completion is reported through the same synthesized
    // `update_goal` call the marker translation emits, so the normal registry
    // path records the status change.
    let call_id = match next_event(&mut rx).await {
        ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
            name,
            arguments,
            call_id,
            ..
        }) => {
            assert_eq!(name, "update_goal");
            assert_eq!(arguments, r#"{"status":"complete"}"#);
            call_id
        }
        other => panic!("expected the synthesized update_goal call, got {other:?}"),
    };
    assert!(call_id.starts_with("zcode_goal_"));
    match next_event(&mut rx).await {
        ResponseEvent::Completed {
            token_usage,
            end_turn,
            ..
        } => {
            assert_eq!(end_turn, Some(true));
            let usage = token_usage.expect("usage present");
            assert_eq!(
                (usage.input_tokens, usage.output_tokens, usage.total_tokens),
                (7, 3, 10)
            );
        }
        other => panic!("expected Completed, got {other:?}"),
    }

    let stats = std::fs::read_to_string(stats_path(&fixture)).expect("stats file");
    let set_frame = stats
        .lines()
        .find(|line| line.starts_with("goal:"))
        .expect("the goal must be set on the wire");
    assert!(
        set_frame.contains("\"action\":\"set\"")
            && set_frame.contains("\"objective\":\"Write the epic poem\"")
            && set_frame.contains("\"sessionId\":\"sess_fake\""),
        "the set frame must carry the objective: {set_frame}"
    );
    assert!(
        stats
            .lines()
            .any(|line| line.starts_with("goal:") && line.contains("\"action\":\"pause\"")),
        "the verified completion pauses the core goal so a later mirror \
         cannot restart it: {stats}"
    );
    bridge.kill("test end");
}

#[tokio::test]
async fn the_round_after_a_mirrored_completion_does_not_restart_the_core_goal_loop() {
    let fixture = write_fake_server_with(FakeServerOptions {
        goal_loop: GoalLoopMode::Complete,
        ..FakeServerOptions::default()
    });
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");
    let tools = vec![goal_tool_spec()];
    let objective = "Write the epic poem";

    // Round 1: the fresh continuation template mirrors into the core goal
    // loop and reports the verified completion.
    let first = vec![user_message(&continuation_prompt(objective))];
    assert_eq!(
        mirrored_goal_objective_for(Some("1"), &tools, &first).as_deref(),
        Some(objective)
    );
    let mut rx = {
        let stream = turn_mirrored(&bridge, &session_id, objective)
            .await
            .expect("mirror starts");
        stream.rx_event
    };
    let mut call_id = String::new();
    loop {
        match next_event(&mut rx).await {
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                call_id: synthesized,
                ..
            }) => call_id = synthesized,
            ResponseEvent::Completed { .. } => break,
            _ => {}
        }
    }
    assert!(call_id.starts_with(MIRROR_CALL_ID_PREFIX));

    // Round 2: the agent loop's function-output round still carries the
    // template as its newest user message; the mirror must stay out and let
    // client.rs send a plain warm turn.
    let mut second = vec![
        user_message(&continuation_prompt(objective)),
        assistant_message("goal".to_string()),
    ];
    second.extend(synthesized_goal_items("complete", &call_id));
    assert_eq!(
        mirrored_goal_objective_for(Some("1"), &tools, &second),
        None,
        "re-mirroring here restarted the core goal loop and re-completed the \
         same goal eight times in the live run (issue #52)"
    );
    let mut plain = bridge
        .turn(&session_id, "plain warm turn", TurnInputChannel::LegacySend)
        .expect("plain warm turn starts");
    loop {
        if let ResponseEvent::Completed { .. } = next_event(&mut plain.rx_event).await {
            break;
        }
    }

    let stats = std::fs::read_to_string(stats_path(&fixture)).expect("stats file");
    assert_eq!(
        (
            stats
                .lines()
                .filter(|line| line.starts_with("goal:") && line.contains("\"action\":\"set\""))
                .count(),
            stats
                .lines()
                .filter(|line| line.starts_with("goal:") && line.contains("\"action\":\"pause\""))
                .count(),
        ),
        (1, 1),
        "exactly one mirrored set followed by its completion pause, with no \
         restart for the answered round: {stats}"
    );
    bridge.kill("test end");
}

#[tokio::test]
async fn mirrored_goal_turn_reports_paused_without_completion() {
    let fixture = write_fake_server_with(FakeServerOptions {
        goal_loop: GoalLoopMode::Paused,
        ..FakeServerOptions::default()
    });
    let (mut rx, bridge) = mirrored_turn(&fixture, "Write the epic poem").await;

    let mut saw_reply = false;
    loop {
        match next_event(&mut rx).await {
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { .. }) => {
                panic!("a paused goal must not report completion")
            }
            ResponseEvent::OutputItemDone(item) => {
                assert_eq!(item, assistant_message("goal".to_string()));
                saw_reply = true;
            }
            ResponseEvent::Completed { end_turn, .. } => {
                assert_eq!(end_turn, Some(true));
                break;
            }
            ResponseEvent::OutputItemAdded(_) | ResponseEvent::OutputTextDelta(_) => {}
            other => panic!("unexpected event {other:?}"),
        }
    }
    assert!(saw_reply, "the turn reply must reach the stream");
    bridge.kill("test end");
}

#[tokio::test]
async fn mirrored_goal_turn_caps_runaway_loops_and_reports_the_goal_blocked() {
    let fixture = write_fake_server_with(FakeServerOptions {
        goal_loop: GoalLoopMode::Capped,
        ..FakeServerOptions::default()
    });
    let (mut rx, bridge) = mirrored_turn(&fixture, "Impossible objective").await;

    let mut completions = 0;
    loop {
        match next_event(&mut rx).await {
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            }) => {
                // The capped cycle reports the goal blocked so the runtime
                // stops scheduling continuations (issue #53); the call id
                // prefix makes the follow-up round's output a selector
                // sentinel, just like a completion.
                assert_eq!(name, "update_goal");
                assert_eq!(arguments, r#"{"status":"blocked"}"#);
                assert!(call_id.starts_with(MIRROR_CALL_ID_PREFIX));
            }
            ResponseEvent::OutputItemDone(_) => completions += 1,
            ResponseEvent::Completed { end_turn, .. } => {
                assert_eq!(end_turn, Some(true));
                break;
            }
            _ => {}
        }
    }
    assert_eq!(
        completions, 12,
        "the loop must stop at the turn cap, not drain the fake's 14 turns"
    );

    let stats = std::fs::read_to_string(stats_path(&fixture)).expect("stats file");
    assert!(
        stats.lines().any(|line| line.starts_with("stop")),
        "the runaway loop is stopped: {stats}"
    );
    assert!(
        stats
            .lines()
            .any(|line| line.starts_with("goal:") && line.contains("\"action\":\"pause\"")),
        "the core goal is paused so only the blocked report reaches Codex: {stats}"
    );
    bridge.kill("test end");
}

#[tokio::test]
async fn mirrored_goal_turn_fails_when_the_set_is_refused() {
    let fixture = write_fake_server_with(FakeServerOptions {
        goal_loop: GoalLoopMode::FailSet,
        ..FakeServerOptions::default()
    });
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");
    let error = match turn_mirrored(&bridge, &session_id, "Any objective").await {
        Ok(_) => panic!("a refused set falls back to the plain turn"),
        Err(error) => error,
    };
    assert!(
        error.contains("session/goal failed"),
        "unexpected error: {error}"
    );
    bridge.kill("test end");
}

#[tokio::test]
async fn mirrored_goal_turn_fails_when_the_core_skips_the_goal_loop() {
    let fixture = write_fake_server_with(FakeServerOptions {
        goal_loop: GoalLoopMode::NotStarted,
        ..FakeServerOptions::default()
    });
    let bridge = ZcodeWarmBridge::spawn(
        &test_runtime(&fixture),
        "/tmp",
        WarmMode::Yolo,
        WarmPermissionPolicy::DenyAll,
        SessionSeed::Fresh,
    )
    .expect("spawn fake app-server");
    let session_id = bridge
        .ensure_session("/tmp")
        .await
        .expect("session created");
    let error = match turn_mirrored(&bridge, &session_id, "Any objective").await {
        Ok(_) => panic!("no started loop falls back to the plain turn"),
        Err(error) => error,
    };
    assert!(
        error.contains("did not start the goal loop"),
        "unexpected error: {error}"
    );
    bridge.kill("test end");
}
