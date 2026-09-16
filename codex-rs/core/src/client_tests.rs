use super::AuthRequestTelemetryContext;
use super::ModelClient;
use super::PendingUnauthorizedRetry;
use super::Prompt;
use super::UnauthorizedRecoveryExecution;
use super::X_CODEX_INSTALLATION_ID_HEADER;
use super::X_CODEX_PARENT_THREAD_ID_HEADER;
use super::X_CODEX_TURN_METADATA_HEADER;
use super::X_CODEX_WINDOW_ID_HEADER;
use super::X_OPENAI_SUBAGENT_HEADER;
use super::ZCODE_TOOL_ARGS_PERSIST_CAP;
use super::zcode_tool_args_from_accumulated;
use super::zcode_tool_args_from_authoritative;
use crate::AttestationContext;
use crate::AttestationProvider;
use crate::GenerateAttestationFuture;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::test_support::TestCodexResponsesRequestKind;
use crate::test_support::responses_metadata as test_responses_metadata;
use codex_api::AgentIdentityTelemetry;
use codex_api::ApiError;
use codex_api::ResponseEvent;
use codex_api::ResponsesEndpoint;
use codex_api::TransportError;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::auth::AgentIdentityAuthPolicy;
use codex_model_provider::BearerAuthProvider;
use codex_model_provider::ModelProvider;
use codex_model_provider::ModelProviderFuture;
use codex_model_provider::ProviderAccountResult;
use codex_model_provider::ProviderAuthRecoveryMessages;
use codex_model_provider::ProviderUnauthorizedRecovery;
use codex_model_provider::SharedModelProvider;
use codex_model_provider::create_model_provider;
use codex_model_provider_info::CHATGPT_CODEX_BASE_URL;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_model_provider_info::create_oss_provider_with_base_url;
use codex_models_manager::manager::SharedModelsManager;
use codex_otel::SessionTelemetry;
use codex_protocol::ThreadId;
use codex_protocol::auth::AuthMode;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::protocol::InternalSessionSource;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_rollout_trace::ExecutionStatus;
use codex_rollout_trace::InferenceTraceAttempt;
use codex_rollout_trace::InferenceTraceContext;
use codex_rollout_trace::RawTraceEventPayload;
use codex_rollout_trace::RolloutTrace;
use codex_rollout_trace::TraceWriter;
use codex_rollout_trace::replay_bundle;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Notify;
use tracing::Event;
use tracing::Subscriber;
use tracing::field::Visit;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context as LayerContext;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

const TEST_INSTALLATION_ID: &str = "11111111-1111-4111-8111-111111111111";

fn test_model_client(session_source: SessionSource) -> ModelClient {
    test_model_client_with_thread_id(ThreadId::new(), session_source)
}

fn test_model_client_with_thread_id(
    thread_id: ThreadId,
    session_source: SessionSource,
) -> ModelClient {
    let provider = create_oss_provider_with_base_url("https://example.com/v1", WireApi::Responses);
    ModelClient::new(
        /*auth_manager*/ None,
        AgentIdentityAuthPolicy::JwtOnly,
        thread_id,
        provider,
        session_source,
        "test_originator".to_string(),
        /*model_verbosity*/ None,
        /*content_item_kinds_enabled*/ true,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*attestation_provider*/ None,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    )
}

fn test_model_provider() -> SharedModelProvider {
    test_model_client(SessionSource::Cli).state.provider.clone()
}

fn test_responses_metadata_for_client(
    client: &ModelClient,
    turn_id: Option<&str>,
    window_id: String,
    parent_thread_id: Option<ThreadId>,
    request_kind: TestCodexResponsesRequestKind,
) -> CodexResponsesMetadata {
    let thread_id = client.state.thread_id.to_string();
    test_responses_metadata(
        TEST_INSTALLATION_ID,
        &thread_id,
        &thread_id,
        turn_id,
        window_id,
        &client.state.session_source,
        parent_thread_id,
        request_kind,
    )
}

fn test_model_info() -> ModelInfo {
    serde_json::from_value(json!({
        "slug": "gpt-test",
        "display_name": "gpt-test",
        "description": "desc",
        "default_reasoning_level": "medium",
        "supported_reasoning_levels": [
            {"effort": "medium", "description": "medium"}
        ],
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "priority": 1,
        "upgrade": null,
        "model_messages": null,
        "support_verbosity": false,
        "default_verbosity": null,
        "apply_patch_tool_type": null,
        "truncation_policy": {"mode": "bytes", "limit": 10000},
        "supports_image_detail_original": false,
        "context_window": 272000,
        "auto_compact_token_limit": null,
        "experimental_supported_tools": []
    }))
    .expect("deserialize test model info")
}

#[test]
fn responses_lite_prefix_ids_track_thread_and_payload() -> anyhow::Result<()> {
    let thread_id = ThreadId::new();
    let client = test_model_client_with_thread_id(thread_id, SessionSource::Cli);
    let mut model = test_model_info();
    model.use_responses_lite = true;
    let mut prompt = Prompt {
        base_instructions: BaseInstructions {
            text: "base instructions".to_string(),
            provenance: None,
        },
        ..Default::default()
    };
    let build = |client: &ModelClient, prompt: &Prompt| {
        client.build_responses_request(
            prompt,
            &model,
            /*effort*/ None,
            codex_protocol::config_types::ReasoningSummary::None,
            /*service_tier*/ None,
            &test_responses_metadata_for_client(
                client,
                /*turn_id*/ None,
                format!("{}:0", client.state.thread_id),
                /*parent_thread_id*/ None,
                TestCodexResponsesRequestKind::Turn,
            ),
        )
    };

    let original = build(&client, &prompt)?;
    assert_eq!(build(&client, &prompt)?, original);

    prompt.base_instructions.text.push_str(" with an update");
    let changed_instructions = build(&client, &prompt)?;
    assert_eq!(changed_instructions.input[0], original.input[0]);
    assert_ne!(changed_instructions.input[1].id(), original.input[1].id());

    prompt.tools = vec![codex_tools::ToolSpec::Freeform(codex_tools::FreeformTool {
        name: "exec".to_string(),
        description: "Execute JavaScript.".to_string(),
        defer_loading: None,
        format: codex_tools::FreeformToolFormat {
            r#type: "grammar".to_string(),
            syntax: "lark".to_string(),
            definition: "start: /.+/".to_string(),
        },
    })]
    .into();
    let changed_tools = build(&client, &prompt)?;
    assert_ne!(
        changed_tools.input[0].id(),
        changed_instructions.input[0].id()
    );
    assert_eq!(changed_tools.input[1], changed_instructions.input[1]);

    let independent = build(
        &test_model_client_with_thread_id(ThreadId::new(), SessionSource::Cli),
        &prompt,
    )?;
    assert_ne!(independent.input[0].id(), changed_tools.input[0].id());
    assert_ne!(independent.input[1].id(), changed_tools.input[1].id());
    Ok(())
}

fn test_session_telemetry() -> SessionTelemetry {
    SessionTelemetry::new(
        ThreadId::new(),
        "gpt-test",
        "gpt-test",
        /*account_id*/ None,
        /*account_email*/ None,
        /*auth_mode*/ None,
        "test-originator".to_string(),
        /*log_user_prompts*/ false,
        "test-terminal".to_string(),
        SessionSource::Cli,
    )
}

fn spawned_session_source() -> SessionSource {
    SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: ThreadId::new(),
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    })
}

fn reasoning_effort_in_request(
    model_info: &ModelInfo,
    session_source: SessionSource,
    effort: ReasoningEffort,
) -> ReasoningEffort {
    let client = test_model_client(session_source);
    client
        .build_responses_request(
            &Prompt::default(),
            model_info,
            Some(effort),
            codex_protocol::config_types::ReasoningSummary::None,
            /*service_tier*/ None,
            &test_responses_metadata_for_client(
                &client,
                /*turn_id*/ None,
                format!("{}:0", client.state.thread_id),
                /*parent_thread_id*/ None,
                TestCodexResponsesRequestKind::Turn,
            ),
        )
        .expect("build responses request")
        .reasoning
        .expect("request should include reasoning")
        .effort
        .expect("request should include reasoning effort")
}

#[test]
fn reasoning_effort_for_requests_uses_multi_agent_override_for_ultra() {
    let mut model_info = test_model_info();
    model_info.multi_agent_reasoning_effort = Some(ReasoningEffort::High);
    model_info
        .supported_reasoning_levels
        .push(ReasoningEffortPreset {
            effort: ReasoningEffort::High,
            description: "high".to_string(),
        });

    let actual = [SessionSource::Cli, spawned_session_source()].map(|session_source| {
        reasoning_effort_in_request(&model_info, session_source, ReasoningEffort::Ultra)
    });

    assert_eq!(actual, [ReasoningEffort::High, ReasoningEffort::High]);
}

#[test]
fn reasoning_effort_for_requests_falls_back_for_missing_or_invalid_override() {
    let mut model_info = test_model_info();
    model_info.supported_reasoning_levels = vec![
        ReasoningEffortPreset {
            effort: ReasoningEffort::Low,
            description: "low".to_string(),
        },
        ReasoningEffortPreset {
            effort: ReasoningEffort::XHigh,
            description: "xhigh".to_string(),
        },
        ReasoningEffortPreset {
            effort: ReasoningEffort::Ultra,
            description: "ultra".to_string(),
        },
    ];

    let actual = [
        None,
        Some(ReasoningEffort::Ultra),
        Some(ReasoningEffort::High),
    ]
    .map(|multi_agent_reasoning_effort| {
        model_info.multi_agent_reasoning_effort = multi_agent_reasoning_effort;
        reasoning_effort_in_request(&model_info, SessionSource::Cli, ReasoningEffort::Ultra)
    });

    assert_eq!(
        actual,
        [
            ReasoningEffort::XHigh,
            ReasoningEffort::XHigh,
            ReasoningEffort::XHigh,
        ]
    );

    model_info.multi_agent_reasoning_effort = None;
    model_info.supported_reasoning_levels.insert(
        1,
        ReasoningEffortPreset {
            effort: ReasoningEffort::Max,
            description: "max".to_string(),
        },
    );
    assert_eq!(
        reasoning_effort_in_request(&model_info, SessionSource::Cli, ReasoningEffort::Ultra),
        ReasoningEffort::Max
    );

    model_info.supported_reasoning_levels.clear();
    assert_eq!(
        reasoning_effort_in_request(&model_info, SessionSource::Cli, ReasoningEffort::Ultra),
        ReasoningEffort::Medium
    );
}

#[test]
fn reasoning_effort_for_requests_preserves_non_ultra_and_persistent_behavior() {
    let mut model_info = test_model_info();
    model_info.multi_agent_reasoning_effort = Some(ReasoningEffort::Low);

    assert_eq!(
        (
            reasoning_effort_in_request(&model_info, SessionSource::Cli, ReasoningEffort::High,),
            reasoning_effort_in_request(
                &model_info,
                SessionSource::Cli,
                ReasoningEffort::Persistent,
            ),
        ),
        (
            ReasoningEffort::High,
            ReasoningEffort::Custom("disabled".to_string()),
        )
    );
}

#[derive(Default)]
struct TagCollectorVisitor {
    tags: BTreeMap<String, String>,
}

impl Visit for TagCollectorVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.tags
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.tags
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

#[derive(Clone)]
struct TagCollectorLayer {
    tags: Arc<Mutex<BTreeMap<String, String>>>,
}

impl<S> Layer<S> for TagCollectorLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: LayerContext<'_, S>) {
        if event.metadata().target() != "feedback_tags" {
            return;
        }
        let mut visitor = TagCollectorVisitor::default();
        event.record(&mut visitor);
        self.tags.lock().unwrap().extend(visitor.tags);
    }
}

fn started_inference_attempt(temp: &TempDir) -> anyhow::Result<InferenceTraceAttempt> {
    let writer = Arc::new(TraceWriter::create(
        temp.path(),
        "trace-1".to_string(),
        "rollout-1".to_string(),
        "thread-root".to_string(),
    )?);
    writer.append(RawTraceEventPayload::ThreadStarted {
        thread_id: "thread-root".to_string(),
        agent_path: "/root".to_string(),
        metadata_payload: None,
    })?;
    writer.append(RawTraceEventPayload::CodexTurnStarted {
        codex_turn_id: "turn-1".to_string(),
        thread_id: "thread-root".to_string(),
    })?;

    let inference_trace = InferenceTraceContext::enabled(
        writer,
        "thread-root".to_string(),
        "turn-1".to_string(),
        "gpt-test".to_string(),
        "test-provider".to_string(),
    );
    let attempt = inference_trace.start_attempt();
    attempt.record_started(&json!({
        "model": "gpt-test",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}]
        }],
    }));
    Ok(attempt)
}

fn output_message(id: &str, text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some(codex_protocol::ResponseItemId::with_suffix("msg", id)),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

async fn replay_until_cancelled(temp: &TempDir) -> anyhow::Result<RolloutTrace> {
    let mut rollout = replay_bundle(temp.path())?;
    for _ in 0..50 {
        let inference = rollout
            .inference_calls
            .values()
            .next()
            .expect("inference should be reduced");
        if inference.execution.status == ExecutionStatus::Cancelled {
            return Ok(rollout);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        rollout = replay_bundle(temp.path())?;
    }
    Ok(rollout)
}

struct NotifyAfterEventStream {
    events: VecDeque<ResponseEvent>,
    yielded: usize,
    notify_after: usize,
    notify: Arc<Notify>,
}

impl futures::Stream for NotifyAfterEventStream {
    type Item = std::result::Result<ResponseEvent, ApiError>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Some(event) = self.events.pop_front() else {
            return Poll::Pending;
        };
        self.yielded += 1;
        if self.yielded == self.notify_after {
            self.notify.notify_one();
        }
        Poll::Ready(Some(Ok(event)))
    }
}

#[test]
fn build_subagent_headers_sets_other_subagent_label() {
    let client = test_model_client(SessionSource::SubAgent(SubAgentSource::Other(
        "memory_consolidation".to_string(),
    )));
    let headers = client.build_subagent_headers();
    let value = headers
        .get(X_OPENAI_SUBAGENT_HEADER)
        .and_then(|value| value.to_str().ok());
    assert_eq!(value, Some("memory_consolidation"));
}

#[test]
fn internal_session_prompt_cache_key_is_scoped_to_parent_thread() {
    let parent_thread_id = ThreadId::new();
    let client = test_model_client(SessionSource::Internal(InternalSessionSource::Guardian));
    let metadata = test_responses_metadata_for_client(
        &client,
        Some("turn-123"),
        "window-1".to_string(),
        Some(parent_thread_id),
        TestCodexResponsesRequestKind::Turn,
    );

    assert_eq!(
        client.prompt_cache_key(&metadata),
        format!("guardian:{parent_thread_id}")
    );
}

#[test]
fn build_subagent_headers_sets_internal_memory_consolidation_label() {
    let client = test_model_client(SessionSource::Internal(
        InternalSessionSource::MemoryConsolidation,
    ));
    let headers = client.build_subagent_headers();
    let value = headers
        .get(X_OPENAI_SUBAGENT_HEADER)
        .and_then(|value| value.to_str().ok());
    assert_eq!(value, Some("memory_consolidation"));
    assert_eq!(
        headers.get("originator"),
        Some(&http::HeaderValue::from_static("test_originator"))
    );
}

#[test]
fn build_ws_client_metadata_includes_window_lineage_and_turn_metadata() {
    let parent_thread_id = ThreadId::new();
    let client = test_model_client(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 2,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    }));

    let thread_id = client.state.thread_id.to_string();
    let expected_window_id = format!("{thread_id}:1");
    let responses_metadata = test_responses_metadata_for_client(
        &client,
        Some("turn-123"),
        expected_window_id.clone(),
        Some(parent_thread_id),
        TestCodexResponsesRequestKind::Turn,
    );
    let client_metadata =
        client.build_ws_client_metadata(&responses_metadata, /*use_responses_lite*/ false);
    let parent_thread_id = parent_thread_id.to_string();
    let turn_metadata: serde_json::Value = serde_json::from_str(
        client_metadata
            .get(X_CODEX_TURN_METADATA_HEADER)
            .expect("turn metadata"),
    )
    .expect("valid turn metadata");
    for (client_key, metadata_key, expected) in [
        (
            X_CODEX_INSTALLATION_ID_HEADER,
            "installation_id",
            "11111111-1111-4111-8111-111111111111",
        ),
        ("session_id", "session_id", thread_id.as_str()),
        ("thread_id", "thread_id", thread_id.as_str()),
        ("turn_id", "turn_id", "turn-123"),
        (
            X_CODEX_WINDOW_ID_HEADER,
            "window_id",
            expected_window_id.as_str(),
        ),
        (
            X_CODEX_PARENT_THREAD_ID_HEADER,
            "parent_thread_id",
            parent_thread_id.as_str(),
        ),
    ] {
        assert_eq!(
            client_metadata.get(client_key).map(String::as_str),
            Some(expected)
        );
        assert_eq!(turn_metadata[metadata_key].as_str(), Some(expected));
    }
    assert_eq!(
        client_metadata
            .get(X_OPENAI_SUBAGENT_HEADER)
            .map(String::as_str),
        Some("collab_spawn")
    );
}

#[tokio::test]
async fn summarize_memories_returns_empty_for_empty_input() {
    let client = test_model_client(SessionSource::Cli);
    let model_info = test_model_info();
    let session_telemetry = test_session_telemetry();

    let output = client
        .summarize_memories(
            Vec::new(),
            &model_info,
            /*effort*/ None,
            &session_telemetry,
        )
        .await
        .expect("empty summarize request should succeed");
    assert_eq!(output.len(), 0);
}

#[tokio::test]
async fn dropped_response_stream_traces_cancelled_partial_output() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let attempt = started_inference_attempt(&temp)?;

    // The provider has produced one complete output item, but no terminal
    // response.completed event. The harness has enough information to keep this
    // item in history, so the trace should preserve it when the stream is
    // abandoned.
    let item = output_message("1", "partial answer");
    let api_stream = futures::stream::iter([Ok(ResponseEvent::OutputItemDone(item))])
        .chain(futures::stream::pending());
    let (mut stream, _) = super::map_response_events(
        /*upstream_request_id*/ None,
        api_stream,
        test_session_telemetry(),
        attempt,
        test_model_provider(),
    );

    let observed = stream
        .next()
        .await
        .expect("mapped stream should yield output item")?;
    assert!(matches!(observed, ResponseEvent::OutputItemDone(_)));

    // Dropping the consumer is how turn interruption/preemption stops polling
    // the provider stream. The mapper task observes that drop asynchronously
    // and records cancellation using the output items it has already seen.
    drop(stream);

    // Cancellation is recorded by the mapper task after Drop wakes it, so the
    // replay may need a short wait before the terminal event appears on disk.
    let rollout = replay_until_cancelled(&temp).await?;
    let inference = rollout
        .inference_calls
        .values()
        .next()
        .expect("inference should be reduced");

    assert_eq!(inference.execution.status, ExecutionStatus::Cancelled);
    assert_eq!(inference.response_item_ids.len(), 1);
    assert_eq!(rollout.raw_payloads.len(), 2);

    Ok(())
}

#[tokio::test]
async fn response_stream_records_last_model_feedback_ids() {
    let tags = Arc::new(Mutex::new(BTreeMap::new()));
    let _guard = tracing_subscriber::registry()
        .with(TagCollectorLayer { tags: tags.clone() })
        .set_default();

    let api_stream = futures::stream::iter([
        Ok(ResponseEvent::Created { response_id: None }),
        Ok(ResponseEvent::Completed {
            response_id: "resp-123".to_string(),
            token_usage: None,
            usage_metadata: None,
            end_turn: Some(true),
        }),
    ]);
    let (mut stream, _) = super::map_response_events(
        Some("req-123".to_string()),
        api_stream,
        test_session_telemetry(),
        InferenceTraceAttempt::disabled(),
        test_model_provider(),
    );

    while stream.next().await.is_some() {}

    let tags = tags.lock().unwrap().clone();
    assert_eq!(
        tags.get("last_model_request_id").map(String::as_str),
        Some("\"req-123\"")
    );
    assert_eq!(
        tags.get("last_model_response_id").map(String::as_str),
        Some("\"resp-123\"")
    );
}

#[cfg(feature = "bedrock")]
#[tokio::test]
async fn bedrock_unauthorized_error_uses_provider_mapping() {
    let provider = create_model_provider(
        ModelProviderInfo::create_amazon_bedrock_provider(/*aws*/ None),
        /*auth_manager*/ None,
    );
    let mut auth_recovery = None;
    let mut provider_auth_recovery_attempted = false;
    let url = "https://bedrock-mantle.us-east-2.api.aws/openai/v1/responses";
    let error = super::handle_unauthorized(
        TransportError::Http {
            status: http::StatusCode::UNAUTHORIZED,
            url: Some(url.to_string()),
            headers: None,
            body: Some(
                "Signature expired: 20260609T133205Z is now earlier than 20260614T062525Z"
                    .to_string(),
            ),
        },
        &mut auth_recovery,
        &mut provider_auth_recovery_attempted,
        &test_session_telemetry(),
        &provider,
        /*event_sender*/ None,
        /*turn_id*/ None,
    )
    .await
    .expect_err("expired Bedrock signature should fail");

    assert_eq!(
        error.to_string(),
        format!(
            "Amazon Bedrock rejected the request because its AWS signature has expired. Refresh your AWS credentials and retry. If `AWS_BEARER_TOKEN_BEDROCK` is set, update or unset it, then restart Codex, url: {url}"
        )
    );
}

#[derive(Debug)]
struct TestRecoveryProvider {
    inner: SharedModelProvider,
    should_fail: bool,
    attempts: Arc<AtomicUsize>,
}

impl ModelProvider for TestRecoveryProvider {
    fn info(&self) -> &ModelProviderInfo {
        self.inner.info()
    }

    fn auth_manager(&self) -> Option<Arc<AuthManager>> {
        None
    }

    fn auth(&self) -> ModelProviderFuture<'_, Option<CodexAuth>> {
        self.inner.auth()
    }

    fn account_state(&self) -> ProviderAccountResult {
        self.inner.account_state()
    }

    fn auth_recovery_messages(&self) -> Option<ProviderAuthRecoveryMessages> {
        Some(ProviderAuthRecoveryMessages {
            started: "Refreshing provider authentication.",
            succeeded: "Provider authentication recovered.",
        })
    }

    fn recover_from_unauthorized(
        &self,
    ) -> ModelProviderFuture<'_, codex_protocol::error::Result<ProviderUnauthorizedRecovery>> {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            if self.should_fail {
                Err(CodexErr::Io(std::io::Error::other(
                    "provider recovery failed",
                )))
            } else {
                Ok(ProviderUnauthorizedRecovery::Recovered)
            }
        })
    }

    fn models_manager(
        &self,
        codex_home: PathBuf,
        config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager {
        self.inner.models_manager(codex_home, config_model_catalog)
    }
}

#[tokio::test]
async fn provider_owned_auth_recovery_is_bounded_and_preserves_unauthorized_failures() {
    for should_fail in [false, true] {
        let attempts = Arc::new(AtomicUsize::new(0));
        let provider: SharedModelProvider = Arc::new(TestRecoveryProvider {
            inner: test_model_provider(),
            should_fail,
            attempts: Arc::clone(&attempts),
        });
        assert!(provider.auth_manager().is_none());

        let unauthorized = || TransportError::Http {
            status: http::StatusCode::UNAUTHORIZED,
            url: Some("https://example.com/v1/responses".to_string()),
            headers: None,
            body: Some("unauthorized".to_string()),
        };
        let mut auth_recovery = None;
        let mut provider_auth_recovery_attempted = false;
        let telemetry = test_session_telemetry();
        let (event_sender, event_receiver) = async_channel::unbounded();
        let result = super::handle_unauthorized(
            unauthorized(),
            &mut auth_recovery,
            &mut provider_auth_recovery_attempted,
            &telemetry,
            &provider,
            Some(&event_sender),
            Some("turn-1"),
        )
        .await;

        let error = if should_fail {
            result.expect_err("failed provider recovery should return the original error")
        } else {
            let recovered = result.expect("provider recovery should succeed without AuthManager");
            assert_eq!(
                (recovered.mode, recovered.phase),
                ("provider", "provider_refresh")
            );
            super::handle_unauthorized(
                unauthorized(),
                &mut auth_recovery,
                &mut provider_auth_recovery_attempted,
                &telemetry,
                &provider,
                Some(&event_sender),
                Some("turn-1"),
            )
            .await
            .expect_err("provider recovery should not run more than once")
        };

        match error.details() {
            CodexErrorDetails::UnexpectedStatus(response) => {
                assert_eq!(response.status, http::StatusCode::UNAUTHORIZED);
                assert_eq!(response.body, "unauthorized");
            }
            other => panic!("unexpected error after provider recovery: {other}"),
        }
        assert_eq!(attempts.load(Ordering::Relaxed), 1);

        let events = std::iter::from_fn(|| event_receiver.try_recv().ok())
            .map(|event| serde_json::to_value(event).expect("recovery event should serialize"))
            .collect::<Vec<_>>();
        let mut expected = vec![json!({
            "id": "turn-1",
            "msg": {
                "type": "auth_recovery_started",
                "provider": provider.info().name,
                "message": "Refreshing provider authentication.",
            }
        })];
        if !should_fail {
            expected.push(json!({
                "id": "turn-1",
                "msg": {
                    "type": "auth_recovery_completed",
                    "provider": provider.info().name,
                    "message": "Provider authentication recovered.",
                }
            }));
        }
        assert_eq!(events, expected);
    }
}

#[tokio::test]
async fn dropped_backpressured_response_stream_traces_cancelled_partial_output()
-> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let attempt = started_inference_attempt(&temp)?;
    let backpressured_item_yielded = Arc::new(Notify::new());
    let mut events = VecDeque::new();
    for _ in 0..super::RESPONSE_STREAM_CHANNEL_CAPACITY {
        events.push_back(ResponseEvent::Created { response_id: None });
    }
    events.push_back(ResponseEvent::OutputItemDone(output_message(
        "1",
        "partial answer",
    )));
    let api_stream = NotifyAfterEventStream {
        events,
        yielded: 0,
        notify_after: super::RESPONSE_STREAM_CHANNEL_CAPACITY + 1,
        notify: Arc::clone(&backpressured_item_yielded),
    };

    let (stream, _) = super::map_response_events(
        /*upstream_request_id*/ None,
        api_stream,
        test_session_telemetry(),
        attempt,
        test_model_provider(),
    );

    // Fill the mapper channel with non-terminal events, then yield one output
    // item. The mapper has observed that item and is blocked trying to send it
    // downstream, so dropping the consumer covers the send-failure path rather
    // than the `consumer_dropped` select branch.
    backpressured_item_yielded.notified().await;
    drop(stream);

    let rollout = replay_until_cancelled(&temp).await?;
    let inference = rollout
        .inference_calls
        .values()
        .next()
        .expect("inference should be reduced");

    assert_eq!(inference.execution.status, ExecutionStatus::Cancelled);
    assert_eq!(inference.response_item_ids.len(), 1);
    assert_eq!(rollout.raw_payloads.len(), 2);

    Ok(())
}

#[test]
fn auth_request_telemetry_context_tracks_attached_auth_and_retry_phase() {
    let auth_context = AuthRequestTelemetryContext::new(
        Some(AuthMode::Chatgpt),
        &BearerAuthProvider::for_test(Some("access-token"), Some("workspace-123")),
        /*agent_identity_telemetry*/ None,
        PendingUnauthorizedRetry::from_recovery(UnauthorizedRecoveryExecution {
            mode: "managed",
            phase: "refresh_token",
        }),
    );

    assert_eq!(auth_context.auth_mode, Some("Chatgpt"));
    assert!(auth_context.auth_header_attached);
    assert_eq!(auth_context.auth_header_name, Some("authorization"));
    assert!(auth_context.retry_after_unauthorized);
    assert_eq!(auth_context.recovery_mode, Some("managed"));
    assert_eq!(auth_context.recovery_phase, Some("refresh_token"));
}

#[test]
fn auth_request_telemetry_context_tracks_agent_identity_ids() {
    let auth_context = AuthRequestTelemetryContext::new(
        Some(AuthMode::Chatgpt),
        &BearerAuthProvider::for_test(/*token*/ None, /*account_id*/ None),
        Some(AgentIdentityTelemetry {
            agent_id: "agent-runtime-context".to_string(),
            task_id: "task-run-context".to_string(),
        }),
        PendingUnauthorizedRetry::default(),
    );

    assert_eq!(
        auth_context.agent_identity_telemetry(),
        Some(&AgentIdentityTelemetry {
            agent_id: "agent-runtime-context".to_string(),
            task_id: "task-run-context".to_string(),
        })
    );
}

fn model_client_with_counting_attestation(
    include_attestation: bool,
) -> (ModelClient, Arc<AtomicUsize>) {
    #[derive(Debug)]
    struct CountingAttestationProvider {
        calls: Arc<AtomicUsize>,
    }

    impl AttestationProvider for CountingAttestationProvider {
        fn header_for_request(
            &self,
            _context: AttestationContext,
        ) -> GenerateAttestationFuture<'_> {
            let calls = self.calls.clone();
            Box::pin(async move {
                let call = calls.fetch_add(1, Ordering::Relaxed) + 1;
                Some(http::HeaderValue::from_bytes(format!("v1.header-{call}").as_bytes()).unwrap())
            })
        }
    }

    let attestation_calls = Arc::new(AtomicUsize::new(0));
    let (auth_manager, provider) = if include_attestation {
        (
            Some(AuthManager::from_auth_for_testing(
                CodexAuth::create_dummy_chatgpt_auth_for_testing(),
            )),
            ModelProviderInfo::create_openai_provider(Some(CHATGPT_CODEX_BASE_URL.to_string())),
        )
    } else {
        (
            None,
            create_oss_provider_with_base_url("https://example.com/v1", WireApi::Responses),
        )
    };
    let model_client = ModelClient::new(
        auth_manager,
        AgentIdentityAuthPolicy::JwtOnly,
        ThreadId::new(),
        provider,
        SessionSource::Exec,
        "test_originator".to_string(),
        /*model_verbosity*/ None,
        /*content_item_kinds_enabled*/ true,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/ false,
        Some(Arc::new(CountingAttestationProvider {
            calls: attestation_calls.clone(),
        })),
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    (model_client, attestation_calls)
}

#[test]
fn guardian_reviewer_uses_dedicated_endpoint_only_with_codex_backend_auth() {
    let (mut model_client, _) =
        model_client_with_counting_attestation(/*include_attestation*/ true);
    Arc::get_mut(&mut model_client.state)
        .expect("test client should have unique session state")
        .session_source = SessionSource::SubAgent(SubAgentSource::Other("guardian".to_owned()));

    assert_eq!(
        model_client.responses_endpoint(
            Some(&CodexAuth::create_dummy_chatgpt_auth_for_testing()),
            "codex-auto-review",
        ),
        ResponsesEndpoint::Responses
    );

    model_client = model_client.with_free_guardian_enabled(/*free_guardian_enabled*/ true);
    assert_eq!(
        model_client.responses_endpoint(
            Some(&CodexAuth::create_dummy_chatgpt_auth_for_testing()),
            "codex-auto-review",
        ),
        ResponsesEndpoint::Guardian
    );
    assert_eq!(
        model_client.responses_endpoint(
            Some(&CodexAuth::create_dummy_chatgpt_auth_for_testing()),
            "required-reviewer-model",
        ),
        ResponsesEndpoint::Responses
    );
    assert_eq!(
        model_client.responses_endpoint(
            Some(&CodexAuth::create_dummy_chatgpt_auth_for_testing()),
            "parent-fallback-model",
        ),
        ResponsesEndpoint::Responses
    );
    assert_eq!(
        model_client.responses_endpoint(
            Some(&CodexAuth::from_api_key("test-api-key")),
            "codex-auto-review",
        ),
        ResponsesEndpoint::Responses
    );

    Arc::get_mut(&mut model_client.state)
        .expect("test client should have unique session state")
        .provider = create_model_provider(
        ModelProviderInfo::create_openai_provider(Some("https://proxy.example.com/v1".to_owned())),
        Some(AuthManager::from_auth_for_testing(
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        )),
    );
    assert_eq!(
        model_client.responses_endpoint(
            Some(&CodexAuth::create_dummy_chatgpt_auth_for_testing()),
            "codex-auto-review",
        ),
        ResponsesEndpoint::Responses
    );

    Arc::get_mut(&mut model_client.state)
        .expect("test client should have unique session state")
        .session_source = SessionSource::Exec;
    assert_eq!(
        model_client.responses_endpoint(
            Some(&CodexAuth::create_dummy_chatgpt_auth_for_testing()),
            "codex-auto-review",
        ),
        ResponsesEndpoint::Responses
    );
}

#[tokio::test]
async fn websocket_handshake_includes_attestation_for_chatgpt_codex_responses() {
    let (model_client, attestation_calls) =
        model_client_with_counting_attestation(/*include_attestation*/ true);
    let responses_metadata = test_responses_metadata_for_client(
        &model_client,
        /*turn_id*/ None,
        format!("{}:0", model_client.state.thread_id),
        /*parent_thread_id*/ None,
        TestCodexResponsesRequestKind::WebsocketConnection,
    );

    let headers = model_client
        .build_websocket_headers(&responses_metadata)
        .await;

    assert_eq!(
        headers
            .get(crate::attestation::X_OAI_ATTESTATION_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("v1.header-1"),
    );
    assert_eq!(attestation_calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn existing_call_sideband_headers_include_attestation() {
    let (model_client, attestation_calls) =
        model_client_with_counting_attestation(/*include_attestation*/ true);

    let headers = model_client
        .realtime_sideband_headers(http::HeaderMap::new())
        .await
        .expect("existing call sideband headers should build");

    assert_eq!(
        headers
            .get(crate::attestation::X_OAI_ATTESTATION_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("v1.header-1"),
    );
    assert_eq!(attestation_calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn non_chatgpt_codex_endpoints_omit_attestation_generation() {
    let (model_client, attestation_calls) =
        model_client_with_counting_attestation(/*include_attestation*/ false);
    let mut response_headers = http::HeaderMap::new();

    if let Some(header_value) = model_client.generate_attestation_header_for().await {
        response_headers.insert(crate::attestation::X_OAI_ATTESTATION_HEADER, header_value);
    }
    let mut compaction_headers = http::HeaderMap::new();
    if let Some(header_value) = model_client.generate_attestation_header_for().await {
        compaction_headers.insert(crate::attestation::X_OAI_ATTESTATION_HEADER, header_value);
    }
    let mut realtime_headers = http::HeaderMap::new();
    if let Some(header_value) = model_client.generate_attestation_header_for().await {
        realtime_headers.insert(crate::attestation::X_OAI_ATTESTATION_HEADER, header_value);
    }

    assert_eq!(
        response_headers.get(crate::attestation::X_OAI_ATTESTATION_HEADER),
        None,
    );
    assert_eq!(
        compaction_headers.get(crate::attestation::X_OAI_ATTESTATION_HEADER),
        None,
    );
    assert_eq!(
        realtime_headers.get(crate::attestation::X_OAI_ATTESTATION_HEADER),
        None,
    );
    assert_eq!(attestation_calls.load(Ordering::Relaxed), 0);
}

#[test]
fn zcode_authoritative_small_input_is_preserved() {
    let input = json!({
        "command": "git remote -v",
        "description": "Show remotes",
    });
    let rendered = zcode_tool_args_from_authoritative("Bash", Some(&input))
        .expect("small input should render");
    let parsed: serde_json::Value =
        serde_json::from_str(&rendered).expect("rendered args should parse");
    assert_eq!(
        parsed.get("command").and_then(|value| value.as_str()),
        Some("git remote -v")
    );
    assert!(rendered.len() < 1024);
}

#[test]
fn zcode_authoritative_oversized_input_becomes_placeholder() {
    let big_command = "x".repeat(ZCODE_TOOL_ARGS_PERSIST_CAP + 1024);
    let input = json!({ "command": big_command });
    let rendered = zcode_tool_args_from_authoritative("Bash", Some(&input))
        .expect("oversized input should still return a placeholder");
    assert!(
        rendered.len() < 1024,
        "placeholder must stay small, got {} bytes",
        rendered.len()
    );
    assert!(rendered.contains("_error"));
}

#[test]
fn zcode_accumulated_valid_json_is_normalized() {
    let accumulated = r#"{"command":"git remote -v","description":"Show remotes"}"#;
    let rendered = zcode_tool_args_from_accumulated("Bash", accumulated);
    let parsed: serde_json::Value =
        serde_json::from_str(&rendered).expect("normalized args should parse");
    assert_eq!(
        parsed.get("cmd").and_then(|value| value.as_str()),
        Some("git remote -v")
    );
}

/// Regression test for the wild 421 KiB repetition loop: the same key
/// repeated ~12k times, truncated mid-key so it cannot parse. The old code
/// persisted the raw 421 KiB string into history, bloating every future turn
/// until execve failed with E2BIG ("could not launch ZCode"). It must now
/// become a small placeholder instead.
#[test]
fn zcode_accumulated_truncated_repetition_becomes_placeholder() {
    let mut accumulated = String::from(
        r#"{"command":"git remote set-url origin git@github.com:alexeygrigorev/codex-zcode.git && git remote -v","description":"Change origin remote to SSH URL""#,
    );
    for _ in 0..12_781 {
        accumulated.push_str(r#","dangerouslyDisableSandbox":true"#);
    }
    // Truncate mid-key like the observed payload (missing closing brace).
    accumulated.push_str(r#","dangerouslyDisableSandbox":"#);
    assert!(accumulated.len() > 400_000);

    let rendered = zcode_tool_args_from_accumulated("Bash", &accumulated);
    assert!(
        rendered.len() < 1024,
        "pathological input must collapse, got {} bytes",
        rendered.len()
    );
    assert!(rendered.contains("_error"));
    assert!(!rendered.contains("dangerouslyDisableSandbox"));
}

#[test]
fn zcode_accumulated_oversized_becomes_placeholder() {
    let accumulated = format!("{{\"command\":\"{}\"}}", "y".repeat(200 * 1024));
    let rendered = zcode_tool_args_from_accumulated("Bash", &accumulated);
    assert!(rendered.len() < 1024);
    assert!(rendered.contains("_error"));
}

#[test]
fn zcode_accumulated_empty_is_empty_object() {
    assert_eq!(
        zcode_tool_args_from_accumulated("Bash", ""),
        "{}".to_string()
    );
}

#[test]
fn zcode_agent_arguments_are_rebuilt_without_harness_extras() {
    let input = json!({
        "description": "Probe Agent",
        "prompt": "Reply with: PROBE_OK",
        "subagent_type": "general-purpose",
        "run_in_background": true,
        "model": "glm-5.3-flash",
        "reasoning_effort": "max",
    });
    let normalized = super::normalize_zcode_tool_arguments("Agent", &input);
    let task_name = normalized
        .get("task_name")
        .and_then(|value| value.as_str())
        .expect("task_name should be derived from description");
    assert_eq!(task_name, "probe_agent");
    let message = normalized
        .get("message")
        .and_then(|value| value.as_str())
        .expect("message should be derived from prompt");
    assert!(message.starts_with("Reply with: PROBE_OK"));
    assert!(message.contains("do not spawn additional sub-agents"));
    assert!(normalized.get("run_in_background").is_none());
    assert!(normalized.get("subagent_type").is_none());
    assert!(normalized.get("model").is_none());
    assert!(normalized.get("reasoning_effort").is_none());
}

#[test]
fn zcode_spawn_agent_arguments_pass_task_name_through() {
    let input = json!({
        "task_name": "probe_child",
        "message": "Reply with: PROBE_OK",
        "run_in_background": true,
    });
    let normalized = super::normalize_zcode_tool_arguments("spawn_agent", &input);
    assert_eq!(
        normalized.get("task_name").and_then(|value| value.as_str()),
        Some("probe_child")
    );
    assert!(normalized.get("run_in_background").is_none());
}

#[test]
fn zcode_send_message_strips_agent_prefix_from_uuid_targets() {
    let input = json!({
        "target": "agent_28a74254-19a2-46bd-81a7-b939c72d4ddb",
        "message": "The code word is BANANA",
        "summary": "Sending code word",
    });
    let normalized = super::normalize_zcode_tool_arguments("SendMessage", &input);
    assert_eq!(
        normalized.get("target").and_then(|value| value.as_str()),
        Some("28a74254-19a2-46bd-81a7-b939c72d4ddb")
    );
    let message = normalized
        .get("message")
        .and_then(|value| value.as_str())
        .expect("message should be kept");
    assert!(message.starts_with("The code word is BANANA"));
    assert!(message.contains("Summary: Sending code word"));
    assert!(normalized.get("summary").is_none());
}

#[test]
fn zcode_send_message_keeps_non_uuid_targets() {
    let input = json!({
        "to": "visual_probe2",
        "message": "hello",
    });
    let normalized = super::normalize_zcode_tool_arguments("send_message", &input);
    assert_eq!(
        normalized.get("target").and_then(|value| value.as_str()),
        Some("visual_probe2")
    );
}

#[test]
fn zcode_followup_task_arguments_are_normalized() {
    let input = json!({
        "to": "agent_28a74254-19a2-46bd-81a7-b939c72d4ddb",
        "message": "follow up",
        "summary": "extra",
    });
    let normalized = super::normalize_zcode_tool_arguments("followup_task", &input);
    assert_eq!(
        normalized.get("target").and_then(|value| value.as_str()),
        Some("28a74254-19a2-46bd-81a7-b939c72d4ddb")
    );
    assert_eq!(
        normalized.get("message").and_then(|value| value.as_str()),
        Some("follow up\n\nSummary: extra")
    );
}

#[test]
fn zcode_goal_status_protocol_only_when_goal_tool_present() {
    let goal_tool = |name: &str| {
        ToolSpec::Function(ResponsesApiTool {
            name: name.to_string(),
            description: "goal tool".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                std::collections::BTreeMap::new(),
                Some(Vec::new()),
                Some(false.into()),
            ),
            output_schema: None,
        })
    };

    let with_goal = super::zcode_goal_status_protocol(&[goal_tool("update_goal")])
        .expect("active goal should emit the protocol note");
    assert!(with_goal.contains(super::ZCODE_GOAL_COMPLETE_MARKER));
    assert!(with_goal.contains(super::ZCODE_GOAL_BLOCKED_MARKER));

    assert!(super::zcode_goal_status_protocol(&[goal_tool("exec_command")]).is_none());
    assert!(super::zcode_goal_status_protocol(&[]).is_none());
}

#[test]
fn zcode_goal_status_from_reply_accepts_only_trailing_markers() {
    assert_eq!(
        super::zcode_goal_status_from_reply("All work is done.\n[GOAL:COMPLETE]"),
        Some("complete")
    );
    assert_eq!(
        super::zcode_goal_status_from_reply("Still stuck on X.\n\n[GOAL:BLOCKED]"),
        Some("blocked")
    );
    // Marker mentioned mid-text, or followed by more prose: not a status.
    assert_eq!(
        super::zcode_goal_status_from_reply("[GOAL:COMPLETE] but then I kept going"),
        None
    );
    assert_eq!(super::zcode_goal_status_from_reply("plain answer"), None);
    assert_eq!(super::zcode_goal_status_from_reply(""), None);
}

#[test]
fn zcode_strip_goal_status_marker_removes_only_the_marker_line() {
    assert_eq!(
        super::zcode_strip_goal_status_marker("All work is done.\n[GOAL:COMPLETE]"),
        "All work is done."
    );
    assert_eq!(
        super::zcode_strip_goal_status_marker("Report\n\n[GOAL:BLOCKED]\n"),
        "Report"
    );
    // No trailing marker: reply passes through untouched.
    assert_eq!(
        super::zcode_strip_goal_status_marker("[GOAL:COMPLETE] mid-text"),
        "[GOAL:COMPLETE] mid-text"
    );
}

#[test]
fn zcode_failure_message_extracts_model_error_with_code() {
    let payload = serde_json::json!({
        "error": {
            "type": "model_error",
            "code": "model_output_limit_exceeded",
            "message": "The model's response exceeded the output token maximum."
        }
    });
    assert_eq!(
        super::zcode_failure_message(&payload),
        Some(
            "The model's response exceeded the output token maximum. \
             (model_output_limit_exceeded)"
                .to_string()
        )
    );
}

#[test]
fn zcode_failure_message_supports_last_error_shape() {
    let payload = serde_json::json!({ "lastError": { "message": "provider unavailable" } });
    assert_eq!(
        super::zcode_failure_message(&payload),
        Some("provider unavailable".to_string())
    );
}

#[test]
fn zcode_failure_message_without_message_is_none() {
    assert_eq!(
        super::zcode_failure_message(&serde_json::json!({
            "error": { "code": "boom" }
        })),
        None
    );
    assert_eq!(super::zcode_failure_message(&serde_json::json!({})), None);
}

#[test]
fn zcode_failure_message_skips_duplicate_code_suffix() {
    assert_eq!(
        super::zcode_failure_message(&serde_json::json!({
            "error": {
                "code": "rate_limited",
                "message": "request failed: rate_limited by provider"
            }
        })),
        Some("request failed: rate_limited by provider".to_string())
    );
}

#[test]
fn zcode_turn_failure_error_maps_output_limit_to_context_window() {
    let error = super::zcode_turn_failure_error(
        "The model's response exceeded the output token maximum. \
         (model_output_limit_exceeded)",
        "",
    );
    assert!(matches!(error, ApiError::ContextWindowExceeded));

    // The exit-status path folds the stderr tail into the message; the code
    // must still be recognized there.
    let error = super::zcode_turn_failure_error(
        "ZCode exited unsuccessfully (exit status: 1); stderr: \
         Error: model_output_limit_exceeded",
        "",
    );
    assert!(matches!(error, ApiError::ContextWindowExceeded));
}

#[test]
fn zcode_turn_failure_error_maps_projection_stderr_output_limit() {
    // The result line's projection carries no error detail, so the generic
    // fallback must consult the stderr tail for the provider code.
    let error = super::zcode_turn_failure_error(
        super::ZCODE_PROJECTION_FAILURE_MESSAGE,
        "Error: model_output_limit_exceeded",
    );
    assert!(matches!(error, ApiError::ContextWindowExceeded));

    // A detailed failure message is authoritative: stderr is not consulted.
    let error = super::zcode_turn_failure_error(
        "provider unavailable (rate_limited)",
        "Error: model_output_limit_exceeded",
    );
    match error {
        ApiError::Stream(message) => assert_eq!(message, "provider unavailable (rate_limited)"),
        other => panic!("expected stream error, got {other:?}"),
    }
}

#[test]
fn zcode_turn_failure_error_keeps_other_failures_retryable() {
    let error = super::zcode_turn_failure_error("provider unavailable (boom)", "");
    match error {
        ApiError::Stream(message) => assert_eq!(message, "provider unavailable (boom)"),
        other => panic!("expected stream error, got {other:?}"),
    }
}

#[test]
fn zcode_token_usage_prefers_backend_context_used() {
    let usage = super::zcode_token_usage(Some(432_000), 40_000, 4);
    assert_eq!(usage.input_tokens, 432_000);
    assert_eq!(usage.total_tokens, 432_000 + usage.output_tokens);
    assert!(usage.output_tokens > 0);
}

#[test]
fn zcode_token_usage_falls_back_to_byte_estimate() {
    // A missing backend count falls back to the 4-bytes/token heuristic so
    // a silent backend still feeds the auto-compact signal.
    assert_eq!(
        super::zcode_token_usage(None, 40_000, 0).total_tokens,
        10_000
    );
}

#[test]
fn zcode_goal_already_reported_only_counts_current_turn_calls() {
    let earlier_turn = vec![
        zcode_user_message("first question"),
        zcode_goal_call("zcode_goal_old"),
        zcode_function_call_output("zcode_goal_old"),
        zcode_user_message("second question"),
    ];
    // A synthesized call from an earlier turn must not silence a new goal.
    assert!(!super::zcode_goal_already_reported_in_current_turn(
        &earlier_turn
    ));

    let mut current_turn = earlier_turn;
    current_turn.push(zcode_goal_call("zcode_goal_new"));
    assert!(super::zcode_goal_already_reported_in_current_turn(
        &current_turn
    ));

    // No user message means no turn boundary; nothing counts as reported.
    let history_only = vec![
        zcode_goal_call("zcode_goal_old"),
        zcode_function_call_output("zcode_goal_old"),
    ];
    assert!(!super::zcode_goal_already_reported_in_current_turn(
        &history_only
    ));
}

/// A fake `zcode.cjs` that scripts NDJSON streams per scenario tag found in
/// the prompt, so `stream_zcode` is exercised end to end without the real
/// runtime. The default branch marks an unexpected spawn.
const FAKE_ZCODE_RUNTIME: &str = r#"
const prompt = process.argv[3] || "";
const emit = (obj) => console.log(JSON.stringify(obj));
if (prompt.includes("ZCODEFAKE=segments")) {
  emit({ type: "session.updated", sessionId: "sess-fake" });
  emit({ type: "model.streaming", payload: { kind: "text_delta", delta: "Let me check." } });
  emit({ type: "model.streaming", payload: { kind: "tool_input_start", toolCallId: "t1", toolName: "Bash" } });
  emit({ type: "model.streaming", payload: { kind: "tool_input_delta", toolCallId: "t1", delta: '{"command":"echo hi"}' } });
  emit({ type: "model.streaming", payload: { kind: "tool_input_end", toolCallId: "t1" } });
  emit({ type: "model.streaming", payload: { kind: "tool_call", toolCallId: "t1", toolName: "Bash", input: { command: "echo hi" } } });
  emit({ type: "model.streaming", payload: { kind: "text_delta", delta: "Done checking." } });
  emit({ type: "result", response: "stale result line" });
} else if (prompt.includes("ZCODEFAKE=streamed-wins")) {
  emit({ type: "model.streaming", payload: { kind: "text_delta", delta: "watched reply" } });
  emit({ type: "result", response: "different result line" });
} else if (prompt.includes("ZCODEFAKE=goal-marker")) {
  emit({ type: "model.streaming", payload: { kind: "text_delta", delta: "All work is done.\n[GOAL:COMPLETE]" } });
  emit({ type: "result", response: "All work is done.\n[GOAL:COMPLETE]" });
} else {
  emit({ type: "model.streaming", payload: { kind: "text_delta", delta: "spawned unexpectedly" } });
  emit({ type: "result", response: "spawned unexpectedly" });
}
"#;

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

fn write_fake_zcode_runtime() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zcode-fake-runtime-{}-{}.cjs",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be after the epoch")
            .as_nanos()
    ));
    std::fs::write(&path, FAKE_ZCODE_RUNTIME).expect("write fake runtime");
    path
}

fn zcode_user_message(text: &str) -> ResponseItem {
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

fn zcode_goal_call(call_id: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: None,
        name: "update_goal".to_string(),
        namespace: None,
        arguments: r#"{"status":"complete"}"#.to_string(),
        encrypted_function_args: Some(Vec::new()),
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn zcode_function_call_output(call_id: &str) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some(call_id.to_string()),
        name: None,
        namespace: None,
        output: codex_protocol::models::FunctionCallOutputPayload::from_text(
            "recorded".to_string(),
        ),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn zcode_goal_tool_spec() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: "update_goal".to_string(),
        description: "goal tool".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            std::collections::BTreeMap::new(),
            Some(Vec::new()),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

fn completed_message_texts(events: &[ResponseEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            ResponseEvent::OutputItemDone(ResponseItem::Message { content, .. }) => Some(
                content
                    .iter()
                    .filter_map(|item| match item {
                        ContentItem::OutputText { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn zcode_stream_completes_text_segments_around_tool_calls() {
    if !node_available() {
        // The fake-runtime tests spawn `node`; nothing to verify without it.
        return;
    }
    let cjs_path = write_fake_zcode_runtime();
    let prompt = Prompt {
        input: vec![zcode_user_message("ZCODEFAKE=segments")],
        ..Default::default()
    };
    let mut stream = super::ModelClientSession::stream_zcode(
        super::ZcodeRuntime {
            node: "node".to_string(),
            cjs: cjs_path.to_string_lossy().into_owned(),
        },
        &prompt,
    )
    .await
    .expect("stream_zcode should launch the fake runtime");

    let mut events = Vec::new();
    while let Some(event) = stream.rx_event.recv().await {
        events.push(event.expect("fake runtime should not fail"));
    }
    let _ = std::fs::remove_file(&cjs_path);

    assert_eq!(events.len(), 10, "unexpected event sequence: {events:?}");
    assert!(matches!(
        &events[0],
        ResponseEvent::OutputItemAdded(ResponseItem::Message { .. })
    ));
    assert!(
        matches!(&events[1], ResponseEvent::OutputTextDelta(delta) if delta == "Let me check.")
    );
    // The first segment completes with its own text before the tool item,
    // so core finalizes text the user already watched stream in.
    assert_eq!(
        completed_message_texts(&events[..3]),
        vec!["Let me check.".to_string()]
    );
    assert!(matches!(
        &events[3],
        ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall { name, .. }) if name == "exec_command"
    ));
    assert!(matches!(
        &events[4],
        ResponseEvent::ToolCallInputDelta { call_id: Some(call_id), .. } if call_id == "zcode_tool_t1"
    ));
    assert!(matches!(
        &events[5],
        ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { name, .. }) if name == "exec_command"
    ));
    assert!(matches!(
        &events[6],
        ResponseEvent::OutputItemAdded(ResponseItem::Message { .. })
    ));
    assert!(
        matches!(&events[7], ResponseEvent::OutputTextDelta(delta) if delta == "Done checking.")
    );
    assert!(matches!(
        &events[8],
        ResponseEvent::OutputItemDone(ResponseItem::Message { .. })
    ));
    assert!(matches!(
        &events[9],
        ResponseEvent::Completed {
            end_turn: Some(true),
            ..
        }
    ));
    // The result line ("stale result line") must not replace streamed text.
    assert_eq!(
        completed_message_texts(&events),
        vec!["Let me check.".to_string(), "Done checking.".to_string()]
    );
}

#[tokio::test]
async fn zcode_stream_final_reply_keeps_streamed_text_over_result_line() {
    if !node_available() {
        // The fake-runtime tests spawn `node`; nothing to verify without it.
        return;
    }
    let cjs_path = write_fake_zcode_runtime();
    let prompt = Prompt {
        input: vec![zcode_user_message("ZCODEFAKE=streamed-wins")],
        ..Default::default()
    };
    let mut stream = super::ModelClientSession::stream_zcode(
        super::ZcodeRuntime {
            node: "node".to_string(),
            cjs: cjs_path.to_string_lossy().into_owned(),
        },
        &prompt,
    )
    .await
    .expect("stream_zcode should launch the fake runtime");

    let mut events = Vec::new();
    while let Some(event) = stream.rx_event.recv().await {
        events.push(event.expect("fake runtime should not fail"));
    }
    let _ = std::fs::remove_file(&cjs_path);

    assert_eq!(events.len(), 4, "unexpected event sequence: {events:?}");
    assert_eq!(
        completed_message_texts(&events),
        vec!["watched reply".to_string()]
    );
}

#[tokio::test]
async fn zcode_goal_marker_reply_emits_message_then_synthesized_call() {
    if !node_available() {
        // The fake-runtime tests spawn `node`; nothing to verify without it.
        return;
    }
    let cjs_path = write_fake_zcode_runtime();
    let mut prompt = Prompt {
        input: vec![zcode_user_message("ZCODEFAKE=goal-marker")],
        ..Default::default()
    };
    prompt.tools = vec![zcode_goal_tool_spec()].into();
    let mut stream = super::ModelClientSession::stream_zcode(
        super::ZcodeRuntime {
            node: "node".to_string(),
            cjs: cjs_path.to_string_lossy().into_owned(),
        },
        &prompt,
    )
    .await
    .expect("stream_zcode should launch the fake runtime");

    let mut events = Vec::new();
    while let Some(event) = stream.rx_event.recv().await {
        events.push(event.expect("fake runtime should not fail"));
    }
    let _ = std::fs::remove_file(&cjs_path);

    assert_eq!(events.len(), 5, "unexpected event sequence: {events:?}");
    // The marker is stripped from the visible reply.
    assert_eq!(
        completed_message_texts(&events),
        vec!["All work is done.".to_string()]
    );
    let call_index = events
        .iter()
        .position(
            |event| matches!(event, ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { name, .. }) if name == "update_goal"),
        )
        .expect("marker should translate into an update_goal call");
    assert!(
        call_index > 2,
        "the reply must be finalized before the goal bookkeeping call"
    );
    match &events[call_index] {
        ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
            arguments, call_id, ..
        }) => {
            assert_eq!(arguments, r#"{"status":"complete"}"#);
            assert!(call_id.starts_with("zcode_goal_"));
        }
        other => panic!("expected a function call item, got {other:?}"),
    }
    assert!(matches!(
        &events[4],
        ResponseEvent::Completed {
            end_turn: Some(true),
            ..
        }
    ));
}

#[tokio::test]
async fn zcode_goal_bookkeeping_continuation_skips_the_runtime() {
    if !node_available() {
        // The fake-runtime tests spawn `node`; nothing to verify without it.
        return;
    }
    let cjs_path = write_fake_zcode_runtime();
    let prompt = Prompt {
        input: vec![
            zcode_user_message("ZCODEFAKE=never-spawn"),
            zcode_goal_call("zcode_goal_r1"),
            zcode_function_call_output("zcode_goal_r1"),
        ],
        ..Default::default()
    };
    let mut stream = super::ModelClientSession::stream_zcode(
        super::ZcodeRuntime {
            node: "node".to_string(),
            cjs: cjs_path.to_string_lossy().into_owned(),
        },
        &prompt,
    )
    .await
    .expect("stream_zcode should short-circuit without spawning");

    let mut events = Vec::new();
    while let Some(event) = stream.rx_event.recv().await {
        events.push(event.expect("short-circuit should not fail"));
    }
    let _ = std::fs::remove_file(&cjs_path);

    // Only the Completed event: spawning the runtime would emit
    // "spawned unexpectedly" from the fake's default branch.
    assert_eq!(events.len(), 1, "unexpected event sequence: {events:?}");
    assert!(matches!(
        &events[0],
        ResponseEvent::Completed {
            end_turn: Some(true),
            ..
        }
    ));
}

#[tokio::test]
async fn zcode_real_tool_continuation_still_invokes_the_runtime() {
    if !node_available() {
        // The fake-runtime tests spawn `node`; nothing to verify without it.
        return;
    }
    let cjs_path = write_fake_zcode_runtime();
    let prompt = Prompt {
        input: vec![
            zcode_user_message("ZCODEFAKE=streamed-wins"),
            ResponseItem::FunctionCall {
                id: None,
                name: "exec_command".to_string(),
                namespace: None,
                arguments: r#"{"command":"echo hi"}"#.to_string(),
                encrypted_function_args: Some(Vec::new()),
                call_id: "zcode_tool_1".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
            zcode_function_call_output("zcode_tool_1"),
        ],
        ..Default::default()
    };
    let mut stream = super::ModelClientSession::stream_zcode(
        super::ZcodeRuntime {
            node: "node".to_string(),
            cjs: cjs_path.to_string_lossy().into_owned(),
        },
        &prompt,
    )
    .await
    .expect("stream_zcode should launch the fake runtime");

    let mut events = Vec::new();
    while let Some(event) = stream.rx_event.recv().await {
        events.push(event.expect("fake runtime should not fail"));
    }
    let _ = std::fs::remove_file(&cjs_path);

    // Real tool results must reach ZCode; the short-circuit applies only to
    // goal bookkeeping, so the runtime actually answered here.
    assert_eq!(events.len(), 4, "unexpected event sequence: {events:?}");
    assert_eq!(
        completed_message_texts(&events),
        vec!["watched reply".to_string()]
    );
}
