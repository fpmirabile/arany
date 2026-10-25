/// The Tools one submission may use.
#[derive(Clone, Default)]
pub enum ToolGrant {
    #[default]
    None,
    /// The State's private `tools.json`, loaded at submission.
    Configured,
    /// The trusted Workspace's built-in grant.
    Trusted(crate::tools::WorkspacePermissions),
}
