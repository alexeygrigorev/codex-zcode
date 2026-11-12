use crate::session::tests::make_session_and_context_with_rx;
use crate::state::ActiveTurn;
use codex_features::Feature;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_protocol::models::ConfigurationReasoning;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::EventMsg;
use pretty_assertions::assert_eq;
use std::sync::Arc;

#[tokio::test]
async fn client_items_after_task_finish_are_recorded_not_queued() {
    let (session, turn_context, _rx_event) = make_session_and_context_with_rx().await;
    // `on_task_finished` drains pending input and emits `TurnComplete` while the
    // finished turn remains active without a task; queued items would be lost.
    let finished_turn = ActiveTurn::default();
    let turn_state = Arc::clone(&finished_turn.turn_state);
    *session.active_turn.lock().await = Some(finished_turn);
    let content = vec![ContentItem::InputText {
        text: "injected after turn completion".to_owned(),
    }];
    let item = ResponseItem::Message {
        id: None,
        role: "user".to_owned(),
        content: content.clone(),
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };

    session
        .inject_client_response_items(vec![item], &turn_context)
        .await;

    // Recording assigns an item id and turn metadata; compare the message itself.
    let recorded = session
        .clone_history()
        .await
        .into_raw_items()
        .into_iter()
        .map(|item| match item {
            ResponseItem::Message { role, content, .. } => (role, content),
            other => panic!("unexpected history item: {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(recorded, vec![("user".to_owned(), content)]);
    assert_eq!(
        session
            .input_queue
            .take_pending_input_for_turn_state(turn_state.as_ref())
            .await,
        Vec::new()
    );
}

#[tokio::test]
async fn harness_authored_configuration_updates_preserve_metadata_and_resume() {
    let (session, turn_context, rx_event) = make_session_and_context_with_rx().await;
    assert!(!session.enabled(Feature::RetainClientDeveloperMessages));

    let expected = ResponseItemEnvelope {
        item: ResponseItem::ConfigurationUpdate {
            reasoning: ConfigurationReasoning {
                effort: ReasoningEffort::High,
            },
        },
        metadata: Some(CodexHarnessMetadata {
            harness_authored_configuration: true,
            ..Default::default()
        }),
    };
    session
        .record_annotated_conversation_items(
            &turn_context,
            turn_context.model_info(),
            vec![expected.clone()],
        )
        .await;

    let recorded = session.clone_history().await.into_annotated_items();
    assert_eq!(recorded, vec![expected.clone()]);
    let mut raw_items = Vec::new();
    while let Ok(event) = rx_event.try_recv() {
        if let EventMsg::RawResponseItem(event) = event.msg {
            raw_items.push(event.item);
        }
    }
    assert_eq!(raw_items, vec![expected.item]);

    let rollout_items = recorded
        .iter()
        .cloned()
        .map(RolloutItem::ResponseItem)
        .collect::<Vec<_>>();
    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;
    assert_eq!(reconstructed.history, recorded);
}
