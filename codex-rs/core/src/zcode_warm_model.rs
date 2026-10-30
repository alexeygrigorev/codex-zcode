//! Bind each outer request to a model already registered in the native runtime.

use super::ZcodeWarmBridge;

impl ZcodeWarmBridge {
    pub(crate) fn record_model_catalog(&self, snapshot: &serde_json::Value) {
        let entries = snapshot
            .pointer("/settings/model/available")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        *self
            .model_catalog
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = entries;
    }

    pub(crate) async fn select_model(
        &self,
        session_id: &str,
        provider_id: &str,
        model_id: &str,
    ) -> Result<(), String> {
        let reasoning_level = {
            let models = self
                .model_catalog
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let model = models
                .iter()
                .find(|entry| {
                    entry
                        .pointer("/ref/providerId")
                        .and_then(serde_json::Value::as_str)
                        == Some(provider_id)
                        && entry
                            .pointer("/ref/modelId")
                            .and_then(serde_json::Value::as_str)
                            == Some(model_id)
                })
                .ok_or_else(|| {
                    "Requested ZCode model is absent from the native catalog".to_string()
                })?;
            let default = model
                .pointer("/reasoning/defaultLevel")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    "Requested ZCode model has no advertised reasoning default".to_string()
                })?;
            if !model
                .pointer("/reasoning/levels")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|levels| {
                    levels.iter().any(|level| {
                        level.get("value").and_then(serde_json::Value::as_str) == Some(default)
                    })
                })
            {
                return Err(
                    "Requested ZCode model advertises an invalid reasoning default".to_string(),
                );
            }
            default.to_string()
        };
        // A rejected or malformed acknowledgement must prevent submission.
        // Never strip the selection as a compatibility fallback.
        let selected = self
            .request(
                "session/setModel",
                serde_json::json!({
                    "sessionId": session_id,
                    "model": {
                        "providerId": provider_id,
                        "modelId": model_id,
                        "options": { "reasoningLevel": reasoning_level },
                    },
                    "persistAsWorkspaceLastUsed": false,
                }),
            )
            .await
            .map_err(|error| error.to_string())?;
        if selected
            .pointer("/session/sessionId")
            .and_then(serde_json::Value::as_str)
            != Some(session_id)
            || selected
                .pointer("/settings/model/current/providerId")
                .and_then(serde_json::Value::as_str)
                != Some(provider_id)
            || selected
                .pointer("/settings/model/current/modelId")
                .and_then(serde_json::Value::as_str)
                != Some(model_id)
            || selected
                .pointer("/settings/model/current/options/reasoningLevel")
                .and_then(serde_json::Value::as_str)
                != Some(reasoning_level.as_str())
        {
            return Err("ZCode did not confirm the requested session model".to_string());
        }
        Ok(())
    }
}
