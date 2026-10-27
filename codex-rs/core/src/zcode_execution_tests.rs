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
