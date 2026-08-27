use std::process::Stdio;
use std::sync::Arc;

use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolPayload;
use codex_extension_api::ToolSpec;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_tools::JsonSchema;
use codex_tools::ToolExposure;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

pub fn install<C: Sync>(registry: &mut ExtensionRegistryBuilder<C>) {
    registry.tool_contributor(Arc::new(ZaiSearchExtension));
    registry.tool_contributor(Arc::new(ZCodeExtension));
}

const ZCODE_PROMPT_TOOL_NAME: &str = "zcode_prompt";

#[derive(Debug, Default)]
struct ZCodeExtension;

#[derive(Debug, Deserialize)]
struct ZCodePromptArgs {
    prompt: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    max_turns: Option<u32>,
}

impl ToolContributor for ZCodeExtension {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        _thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn ToolExecutor<ToolCall>>> {
        vec![Arc::new(ZCodePromptTool)]
    }
}

#[derive(Debug, Default)]
struct ZCodePromptTool;

impl ToolExecutor<ToolCall> for ZCodePromptTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(ZCODE_PROMPT_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(codex_extension_api::ResponsesApiTool {
            name: ZCODE_PROMPT_TOOL_NAME.to_string(),
            description: "Run one independent ZCode task. Pass a prior sessionId only when explicitly continuing that ZCode conversation.".to_string(),
            strict: false,
            parameters: JsonSchema::object(
                BTreeMap::from([
                    (
                        "prompt".to_string(),
                        JsonSchema::string(Some("Task or question for ZCode".to_string())),
                    ),
                    (
                        "session_id".to_string(),
                        JsonSchema::string(Some("Existing ZCode sessionId to resume".to_string())),
                    ),
                    (
                        "cwd".to_string(),
                        JsonSchema::string(Some("Absolute working directory; defaults to the active Codex workspace".to_string())),
                    ),
                    (
                        "mode".to_string(),
                        JsonSchema::string_enum(
                            vec![serde_json::json!("build"), serde_json::json!("edit"), serde_json::json!("plan"), serde_json::json!("yolo")],
                            Some("ZCode permission mode".to_string()),
                        ),
                    ),
                    (
                        "max_turns".to_string(),
                        JsonSchema::integer(Some("Maximum model turns".to_string())),
                    ),
                ]),
                Some(vec!["prompt".to_string()]),
                Some(false.into()),
            ),
            output_schema: None,
            defer_loading: None,
        })
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        false
    }

    fn handle(&self, call: ToolCall) -> codex_extension_api::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let args: ZCodePromptArgs =
                serde_json::from_str(call.function_arguments()?).map_err(|error| {
                    codex_extension_api::FunctionCallError::RespondToModel(error.to_string())
                })?;
            let cwd = args
                .cwd
                .as_deref()
                .map(str::to_string)
                .or_else(|| {
                    call.environments
                        .first()
                        .map(|environment| environment.cwd.as_path().to_string_lossy().to_string())
                })
                .unwrap_or_else(|| {
                    std::env::current_dir()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                });
            let mode = args.mode.unwrap_or_else(|| "yolo".to_string());
            let requested_session = args.session_id;

            let runtime = std::env::var("ZCODE_CJS")
                .unwrap_or_else(|_| "/opt/ZCode/resources/glm/zcode.cjs".to_string());
            let node = std::env::var("ZCODE_NODE").unwrap_or_else(|_| "node".to_string());
            let mut command = Command::new(node);
            command
                .arg(&runtime)
                .arg("--prompt")
                .arg(&args.prompt)
                .args(["--json", "--mode", &mode, "--cwd", &cwd]);
            if let Some(session_id) = requested_session.as_deref() {
                command.args(["--resume", session_id]);
            }
            if let Some(max_turns) = args.max_turns {
                command.args(["--max-turns", max_turns.to_string().as_str()]);
            }
            command
                .current_dir(&cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);

            const ZCODE_PROMPT_TIMEOUT: Duration = Duration::from_secs(20 * 60);
            let output = timeout(ZCODE_PROMPT_TIMEOUT, command.output())
                .await
                .map_err(|_| {
                    codex_extension_api::FunctionCallError::RespondToModel(
                        "ZCode timed out after 20 minutes".to_string(),
                    )
                })?
                .map_err(|error| {
                    codex_extension_api::FunctionCallError::RespondToModel(format!(
                        "could not launch ZCode: {error}"
                    ))
                })?;
            if !output.status.success() {
                return Err(codex_extension_api::FunctionCallError::RespondToModel(
                    format!(
                        "ZCode failed ({}): {}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr)
                    ),
                ));
            }
            let result: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
                codex_extension_api::FunctionCallError::RespondToModel(format!(
                    "invalid ZCode JSON output: {error}; {}",
                    String::from_utf8_lossy(&output.stdout)
                ))
            })?;
            let session_id = result
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    codex_extension_api::FunctionCallError::RespondToModel(
                        "ZCode output is missing sessionId".to_string(),
                    )
                })?
                .to_string();
            let output = serde_json::json!({
                "sessionId": session_id,
                "response": result.get("response").and_then(Value::as_str).unwrap_or_default(),
                "usage": result.get("usage"),
            });
            let output = serde_json::to_string_pretty(&output).map_err(|error| {
                codex_extension_api::FunctionCallError::Fatal(error.to_string())
            })?;
            Ok(Box::new(TextToolOutput { output }) as Box<dyn ToolOutput>)
        })
    }
}

const ZAI_SEARCH_TOOL_NAME: &str = "zai_search";

#[derive(Debug, Default)]
struct ZaiSearchTool;

#[derive(Debug, Deserialize)]
struct ZaiSearchArgs {
    query: String,
    #[serde(default)]
    count: Option<u32>,
}

impl ToolExecutor<ToolCall> for ZaiSearchTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(ZAI_SEARCH_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(codex_extension_api::ResponsesApiTool {
            name: ZAI_SEARCH_TOOL_NAME.to_string(),
            description: "Search the web using Z.AI Search Prime. Use for current facts, releases, documentation, and URLs.".to_string(),
            strict: false,
            parameters: JsonSchema::object(
                BTreeMap::from([
                    (
                        "query".to_string(),
                        JsonSchema::string(Some("Search query".to_string())),
                    ),
                    (
                        "count".to_string(),
                        JsonSchema::integer(Some("Maximum results to return".to_string())),
                    ),
                ]),
                Some(vec!["query".to_string()]),
                Some(false.into()),
            ),
            output_schema: None,
            defer_loading: None,
        })
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, call: ToolCall) -> codex_extension_api::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let args: ZaiSearchArgs =
                serde_json::from_str(call.function_arguments()?).map_err(|error| {
                    codex_extension_api::FunctionCallError::RespondToModel(error.to_string())
                })?;
            let endpoint = std::env::var("ZAI_SEARCH_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:18765/zai/v1/responses".to_string());

            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .map_err(|error| {
                    codex_extension_api::FunctionCallError::Fatal(error.to_string())
                })?;
            let count = args.count.unwrap_or(5);
            let body = serde_json::json!({
                "model": "glm-5-turbo",
                "input": format!(
                    "Search the web for: {query}. Return only the results as a JSON array of objects with title, url, and snippet fields. Limit to {count} results.",
                    query = args.query,
                    count = count
                ),
                "tools": [{
                    "type": "web_search",
                    "web_search": {
                        "enable": true,
                        "search_engine": "search-prime"
                    }
                }],
                "stream": false,
                "max_output_tokens": 2048
            });

            let response = client
                .post(endpoint)
                .json(&body)
                .send()
                .await
                .map_err(|error| {
                    codex_extension_api::FunctionCallError::Fatal(error.to_string())
                })?;
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if !status.is_success() {
                return Err(codex_extension_api::FunctionCallError::RespondToModel(
                    format!("Z.AI search failed ({status}): {text}"),
                ));
            }
            let value: Value = serde_json::from_str(&text).map_err(|error| {
                codex_extension_api::FunctionCallError::Fatal(error.to_string())
            })?;
            let results = normalize_search_results(&value);
            let output = serde_json::to_string_pretty(&results).map_err(|error| {
                codex_extension_api::FunctionCallError::Fatal(error.to_string())
            })?;

            Ok(Box::new(TextToolOutput { output }) as Box<dyn ToolOutput>)
        })
    }
}

fn normalize_search_results(value: &Value) -> Value {
    let mut normalized = Vec::new();
    let model_text = value
        .pointer("/output/0/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let parsed_results = serde_json::from_str::<Value>(model_text.trim())
        .ok()
        .filter(|value| value.is_array() || value.get("results").is_some());
    if let Some(parsed) = parsed_results {
        return serde_json::json!({ "engine": "zai-web", "results": parsed });
    }
    let candidates = value
        .pointer("/search_result")
        .or_else(|| value.pointer("/data/search_result"))
        .or_else(|| value.pointer("/data/search_results"))
        .or_else(|| value.pointer("/results"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for result in candidates {
        let title = string_field(&result, &["title", "name"]);
        let url = string_field(&result, &["url", "link", "web_url"]);
        let snippet = string_field(&result, &["content", "snippet", "summary", "description"]);
        if title.is_empty() && url.is_empty() && snippet.is_empty() {
            continue;
        }
        let mut item = BTreeMap::new();
        if !title.is_empty() {
            item.insert("title".to_string(), Value::String(title));
        }
        if !url.is_empty() {
            item.insert("url".to_string(), Value::String(url));
        }
        if !snippet.is_empty() {
            item.insert("snippet".to_string(), Value::String(snippet));
        }
        normalized.push(Value::Object(item.into_iter().collect()));
    }

    serde_json::json!({ "engine": "search-prime", "results": normalized })
}

fn string_field(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| value.get(key).and_then(Value::as_str))
        .unwrap_or_default()
        .to_string()
}

struct TextToolOutput {
    output: String,
}

impl ToolOutput for TextToolOutput {
    fn log_output(&self) -> String {
        "[Z.AI web search output]".to_string()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn contains_external_context(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::InputText {
                    text: self.output.clone(),
                },
            ]),
        }
    }
}

#[derive(Debug, Default)]
struct ZaiSearchExtension;

impl ToolContributor for ZaiSearchExtension {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        _thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn codex_extension_api::ToolExecutor<ToolCall>>> {
        vec![Arc::new(ZaiSearchTool)]
    }
}
