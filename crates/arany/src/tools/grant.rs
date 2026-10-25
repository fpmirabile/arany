#[derive(Clone, Default)]
pub enum ToolGrant {
    #[default]
    None,
    Configured,
    Trusted(crate::tools::WorkspacePermissions),
}

impl std::fmt::Debug for ToolGrant {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::None => "None",
            Self::Configured => "Configured",
            Self::Trusted(_) => "Trusted(..)",
        })
    }
}
