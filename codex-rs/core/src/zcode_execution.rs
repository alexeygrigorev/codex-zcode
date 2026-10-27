//! Captured permission admission for ZCode's host-local inner executor.

use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_utils_absolute_path::AbsolutePathBuf;

#[derive(Debug, Clone)]
pub(crate) struct ZcodeExecutionPolicy {
    pub(crate) approval_policy: AskForApproval,
    pub(crate) permission_profile: PermissionProfile,
    pub(crate) workspace: Option<AbsolutePathBuf>,
}

impl Default for ZcodeExecutionPolicy {
    fn default() -> Self {
        Self {
            approval_policy: AskForApproval::UnlessTrusted,
            permission_profile: PermissionProfile::read_only(),
            workspace: None,
        }
    }
}

impl ZcodeExecutionPolicy {
    pub(crate) fn admitted_workspace(&self) -> Result<&AbsolutePathBuf, String> {
        if self.approval_policy != AskForApproval::Never
            || self.permission_profile != PermissionProfile::Disabled
        {
            return Err("ZCode inner execution requires an explicitly unrestricted permission profile and approval policy never; restricted sandbox or interactive approval enforcement is unavailable".to_string());
        }
        self.workspace.as_ref().ok_or_else(|| {
            "ZCode inner execution requires exactly one ready host-local environment".to_string()
        })
    }
}

#[cfg(test)]
#[path = "zcode_execution_tests.rs"]
mod tests;
