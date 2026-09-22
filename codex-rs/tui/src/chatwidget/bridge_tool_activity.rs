//! Live status for tool calls executing inside a bridged external agent core
//! (the ZCode warm bridge).
//!
//! The tool runs on the far side of the bridge, so there is no codex item to
//! render in history; the activity surfaces through the status indicator
//! instead, the same surface used for background terminal waits.

use super::*;
use codex_app_server_protocol::BridgeToolActivityNotification;
use codex_app_server_protocol::BridgeToolActivityStatus;

impl ChatWidget {
    pub(super) fn on_bridge_tool_activity(&mut self, notification: BridgeToolActivityNotification) {
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
