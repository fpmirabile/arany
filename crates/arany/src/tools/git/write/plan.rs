use super::*;

pub(super) async fn plan(
    workspace: &Path,
    scratch: &Path,
    git: &Program,
    identity: &GitIdentity,
    intent: &EffectIntent,
    hidden: &HiddenPaths,
    runtime: Duration,
) -> Result<Result<(Plan, Value), String>, ToolError> {
    let (call, max_bytes) = (&intent.call, intent.limits.result_bytes as usize);
    if max_bytes < RECEIPT_BYTES {
        return Err(ToolError::Limit);
    }
    let deadline = tokio::time::Instant::now() + runtime.min(MAX_GIT_RUNTIME);
    verify_pin(git)?;
    let repository = match inspect(workspace)? {
        Ok(repository) => repository,
        Err(message) => return Ok(Err(failure(message, &[], max_bytes, hidden))),
    };
    if !matches!(call, ToolCall::GitBranch { .. })
        && IN_PROGRESS
            .iter()
            .any(|name| repository.git_dir.symlink_metadata(name).is_ok())
    {
        return Ok(Err(failure(IN_PROGRESS_MESSAGE, &[], max_bytes, hidden)));
    }
    let view = scratch.join(VIEW);
    let snapshot = match prepare(&repository, workspace, &view)? {
        Ok(snapshot) => snapshot,
        Err(message) => return Ok(Err(failure(message, &[], max_bytes, hidden))),
    };
    #[cfg(all(test, target_os = "linux"))]
    tests::race(workspace, tests::Stage::Snapshot);
    let mut env = environment(&view, workspace, scratch);
    for (key, value) in [
        ("GIT_AUTHOR_NAME", &identity.name),
        ("GIT_AUTHOR_EMAIL", &identity.email),
        ("GIT_COMMITTER_NAME", &identity.name),
        ("GIT_COMMITTER_EMAIL", &identity.email),
    ] {
        env.push((key, value.into()));
    }
    let git = Git {
        executable: Path::new(&git.executable),
        env,
        workspace,
        scratch,
        hidden,
        view: Dir::open_ambient_dir(&view, cap_std::ambient_authority())
            .map_err(|_| ToolError::Operation)?,
        deadline,
        max_bytes,
    };
    let planned = match call {
        ToolCall::GitCommit {
            message,
            paths,
            staged,
        } => commit(&git, &repository, message, paths, *staged).await?,
        ToolCall::GitBranch {
            name: Some(name),
            start_point,
            ..
        } => branch(&git, &repository, &snapshot, name, start_point.as_deref()).await?,
        ToolCall::GitSwitch { branch } => switch(&git, &repository, branch).await?,
        _ => return Err(ToolError::Configuration),
    };
    let (publication, result) = match planned {
        Ok(planned) => planned,
        Err(failure) => return Ok(Err(failure)),
    };
    Ok(Ok((
        Plan::capture(&repository, &snapshot, publication, &git.view)?,
        result,
    )))
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct RefDigests {
    pub(super) head: [u8; 32],
    pub(super) packed: Option<[u8; 32]>,
    pub(super) loose: Vec<(String, [u8; 32])>,
}

impl From<&RefState> for RefDigests {
    fn from(refs: &RefState) -> Self {
        Self {
            head: digest(&refs.head),
            packed: refs.packed.as_deref().map(digest),
            loose: refs
                .loose
                .iter()
                .map(|(name, bytes)| (name.clone(), digest(bytes)))
                .collect(),
        }
    }
}

pub(super) struct Plan {
    pub(super) identity: (u64, u64),
    pub(super) refs: RefDigests,
    pub(super) index_before: Option<[u8; 32]>,
    pub(super) shallow: Option<[u8; 32]>,
    pub(super) publication: Publication,
    pub(super) objects: Vec<Vec<u8>>,
}

impl Plan {
    fn capture(
        repository: &Repository,
        refs: &RefState,
        publication: Publication,
        view: &Dir,
    ) -> Result<Self, ToolError> {
        let mut objects = Vec::with_capacity(publication.objects.len());
        let mut total = publication.index.as_ref().map_or(0, Vec::len);
        for name in &publication.objects {
            let bytes = read_optional(view, &format!("objects/{name}"), MAX_SNAPSHOT_BYTES)?
                .ok_or(ToolError::Operation)?;
            total = total.saturating_add(bytes.len());
            if total > MAX_SNAPSHOT_BYTES {
                return Err(ToolError::Limit);
            }
            objects.push(bytes);
        }
        Ok(Self {
            identity: repository.identity,
            refs: RefDigests::from(refs),
            index_before: repository.index.as_ref().map(|(bytes, _)| digest(bytes)),
            shallow: read_optional(view, "shallow", MAX_SNAPSHOT_BYTES)?
                .as_deref()
                .map(digest),
            publication,
            objects,
        })
    }
}

pub(super) fn hex(hash: [u8; 32]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}
