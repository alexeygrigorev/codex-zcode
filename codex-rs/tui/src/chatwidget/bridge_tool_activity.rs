//! Live status for tool calls executing inside a bridged external agent core
//! (the ZCode warm bridge).
//!
//! Status previews remain separate from genuine Rust-only native returns.
//! A return can enter local UI history, never model-visible execution history.

use super::*;
use codex_app_server_protocol::BridgeToolActivityNotification;
use codex_app_server_protocol::BridgeToolActivityStatus;

impl ChatWidget {
    pub(super) fn on_bridge_tool_activity(&mut self, notification: BridgeToolActivityNotification) {
        let thread = self.thread_id().map(|id| id.to_string());
        if let Some(cell) = self.transcript.bridge_native.observe(
            thread.as_deref(),
            self.turn_lifecycle.last_turn_id.as_deref(),
            &notification,
        ) {
            self.add_to_history(cell);
        }
        match notification.status {
            BridgeToolActivityStatus::Started => {
                self.set_status(
                    format!("Running {}", notification.tool),
                    notification.detail,
                    // Tool input and output are verbatim content; never
                    // rewrite their casing.
                    StatusDetailsCapitalization::Preserve,
                    STATUS_DETAILS_DEFAULT_MAX_LINES,
                );
            }
            BridgeToolActivityStatus::Completed | BridgeToolActivityStatus::Failed => {
                self.set_status(
                    String::from("Working"),
                    /*details*/ None,
                    StatusDetailsCapitalization::CapitalizeFirst,
                    STATUS_DETAILS_DEFAULT_MAX_LINES,
                );
            }
        }
    }
}
