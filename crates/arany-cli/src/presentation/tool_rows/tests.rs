use super::*;

#[cfg(test)]
mod peer_row_tests {
    use super::*;
    use crate::presentation::{
        ToolApprovalChoice, plain_tool_summary, tool_approval_choices, tool_fixture, tool_groups,
    };
    use arany::{ToolCall, ToolDisposition};

    #[test]
    fn named_limit_rows_and_cache_problem_markers_have_the_same_linear_copy() {
        use arany::{CacheUpdate, LimitCause};
        let mut tool = tool_fixture(
            ToolCall::Command {
                program: "sh".into(),
                args: vec![],
                cwd: String::new(),
            },
            "untrusted timeout text\u{1b}[2J",
        );
        tool.intent.limits.runtime_ms = 120_000;
        tool.observation.as_mut().unwrap().intent.limits.runtime_ms = 120_000;
        for (cause, copy) in [
            (
                LimitCause::Timeout,
                "timed out after 2 min; /settings raises it",
            ),
            (LimitCause::OutputCeiling, "output passed 1.0 GB; stopped"),
            (
                LimitCause::ContextFull,
                "no room left in this Run's context",
            ),
            (
                LimitCause::ReadCapacity,
                "too many reads at once; nothing was read",
            ),
            (
                LimitCause::RequestedTime,
                "asked for more time than the pinned maximum",
            ),
            (LimitCause::Resource, "resource limit reached"),
        ] {
            let observation = tool.observation.as_mut().unwrap();
            observation.disposition = ToolDisposition::Limit;
            observation.limit_cause = Some(cause);
            for (cache, marker) in [
                (CacheUpdate::NotApplicable, ""),
                (CacheUpdate::NotAttempted, ""),
                (CacheUpdate::Discarded, ""),
                (CacheUpdate::Published, ""),
                (CacheUpdate::Rejected, " · cache not saved (limit)"),
                (CacheUpdate::Failed, " · cache update failed"),
                (CacheUpdate::Uncertain, " · cache outcome uncertain"),
            ] {
                tool.observation.as_mut().unwrap().cache_update = cache;
                let summary = tool_summary(&tool);
                assert_eq!(
                    summary,
                    format!("⏺ Command(sh )\n  ⎿ limit: {copy}{marker}")
                );
                assert_eq!(
                    plain_tool_summary(&summary),
                    format!("Command(sh )\nResult: limit: {copy}{marker}")
                );
            }
        }
        let observation = tool.observation.as_mut().unwrap();
        observation.disposition = ToolDisposition::Uncertain;
        observation.limit_cause = None;
        observation.output = "effects unknown; never replay".into();
        assert_eq!(
            tool_summary(&tool),
            "⏺ Command(sh )\n  ⎿ uncertain: effects unknown; never replay · cache outcome uncertain"
        );
    }

    #[test]
    fn paused_and_cancelled_approval_rows_keep_their_distinct_causes() {
        let mut tool = tool_fixture(
            ToolCall::Read {
                path: "notes/facts.txt".into(),
                offset: None,
                limit: None,
            },
            "",
        );
        for (cause, label, output) in [
            (
                arany::ApprovalCause::Paused,
                "paused",
                "Goal paused before this operation; no operation was dispatched",
            ),
            (
                arany::ApprovalCause::Cancelled,
                "cancelled",
                "Run cancelled before this operation; no operation was dispatched",
            ),
        ] {
            let observation = tool.observation.as_mut().unwrap();
            observation.disposition = ToolDisposition::Denied;
            observation.approval_cause = Some(cause);
            observation.output = output.into();
            assert_eq!(tool.model_denial_label(), Some(label));
            assert_eq!(
                plain_tool_summary(&tool_summary(&tool)),
                format!("Read(notes/facts.txt)\nResult: denied: {label}")
            );
            assert!(tool_detail_text(&tool).ends_with(output));
        }
    }

    #[test]
    fn historical_hook_denial_rows_hide_committed_reason_text() {
        let canary = "old hook reason";
        let mut tool = tool_fixture(
            ToolCall::Read {
                path: "notes/facts.txt".into(),
                offset: None,
                limit: None,
            },
            &format!("Hook denied this Tool: {canary}"),
        );
        let observation = tool.observation.as_mut().unwrap();
        observation.disposition = ToolDisposition::Denied;
        tool.permission_verdict = Some(arany::permissions::PermissionVerdict {
            host: None,
            intent_digest: [1; 32],
            decision: arany::permissions::Decision::Deny,
            mode: None,
            basis: arany::permissions::PermissionSource::Hook,
            source: arany::permissions::PermissionSource::Hook,
            approved: false,
            hook_restriction: Some(arany::permissions::Decision::Deny),
            hooks: 1,
            narrowed: false,
        });

        let summary = tool_summary(&tool);
        assert!(summary.contains("hook_denied"));
        assert!(!summary.contains(canary));
        let detail = tool_detail_text(&tool);
        assert!(detail.contains("hook_denied: no operation was dispatched"));
        assert!(!detail.contains(canary));

        let failure = "Guard enforcement: protected filesystem path refused";
        let observation = tool.observation.as_mut().unwrap();
        observation.output = failure.into();
        observation.enforcement_cause = Some(arany::EnforcementCause::FilesystemDenied);
        tool.permission_verdict = Some(arany::permissions::PermissionVerdict {
            host: None,
            intent_digest: [1; 32],
            decision: arany::permissions::Decision::Allow,
            mode: None,
            basis: arany::permissions::PermissionSource::Human,
            source: arany::permissions::PermissionSource::Human,
            approved: true,
            hook_restriction: None,
            hooks: 0,
            narrowed: false,
        });
        assert!(tool.model_denial_label().is_none());
        let summary = tool_summary(&tool);
        assert!(summary.contains(failure));
        assert!(!summary.contains("user_declined"));
    }

    #[test]
    fn host_admission_details_preserve_the_exact_recovery_guidance() {
        for tool in arany::test_support::host_admission_fixtures()
            .into_iter()
            .chain(std::iter::once(
                arany::test_support::uncertain_hook_fixture(),
            ))
        {
            let expected = &tool.observation.as_ref().unwrap().output;
            assert_eq!(tool.model_denial_label(), None);
            assert!(tool_detail_text(&tool).ends_with(expected));
        }
    }

    #[test]
    fn git_write_rows_show_their_receipt_and_approvals_the_exact_action() {
        let oid = "a".repeat(40);
        for (call, output, expected) in [
            (
                ToolCall::GitCommit {
                    message: "Fix\nbody".into(),
                    paths: vec!["src/a.rs".into(), "README.md".into()],
                    staged: false,
                },
                serde_json::json!({"commit": oid, "branch": "main"}),
                "⏺ Git commit(src/a.rs, README.md)\n  ⎿ committed aaaaaaaaaaaa on main",
            ),
            (
                ToolCall::GitCommit {
                    message: "Fix".into(),
                    paths: Vec::new(),
                    staged: true,
                },
                serde_json::json!({"commit": oid, "branch": "main"}),
                "⏺ Git commit(staged)\n  ⎿ committed aaaaaaaaaaaa on main",
            ),
            (
                ToolCall::GitBranch {
                    name: Some("topic".into()),
                    start_point: Some("HEAD~1".into()),
                    scope: None,
                    base: None,
                    max_count: None,
                },
                serde_json::json!({"branch": "topic", "oid": oid}),
                "⏺ Git branch(topic from HEAD~1)\n  ⎿ created topic at aaaaaaaaaaaa",
            ),
            (
                ToolCall::GitSwitch {
                    branch: "topic".into(),
                },
                serde_json::json!({"branch": "topic", "switched": true, "files": 1}),
                "⏺ Git switch(topic)\n  ⎿ switched to topic · 1 file",
            ),
            (
                ToolCall::GitSwitch {
                    branch: "topic".into(),
                },
                serde_json::json!({"branch": "topic", "switched": false}),
                "⏺ Git switch(topic)\n  ⎿ already on topic",
            ),
        ] {
            assert_eq!(
                tool_summary(&tool_fixture(call, &output.to_string())),
                expected
            );
        }
        // Receipt names are repository data: escaped, and bounded to their row.
        let hostile = format!("\u{202e}evil\u{1b}[2J{}", "b".repeat(400));
        let created = tool_summary(&tool_fixture(
            ToolCall::GitBranch {
                name: Some("topic".into()),
                start_point: None,
                scope: None,
                base: None,
                max_count: None,
            },
            &serde_json::json!({"branch": hostile, "oid": oid}).to_string(),
        ));
        assert!(
            created.contains("created \\u{202e}evil\\u{001b}[2J"),
            "{created}"
        );
        assert!(!created.contains('\u{202e}') && !created.contains('\u{1b}'));
        assert!(
            created.lines().nth(1).unwrap().chars().count() < 200,
            "{created}"
        );
        let long = format!(".git/refs/heads/{}.lock", "deeply/nested/branch".repeat(6));
        let locks = format!(
            "Git locks left by an interrupted write: .git/index.lock, {long}, .git/packed-refs.lock; remove them after checking that no git process is running. Guard execution unconfirmed; effects may have occurred. Do not retry automatically."
        );
        let uncertain = |output: &str| {
            let mut tool = tool_fixture(
                ToolCall::GitSwitch {
                    branch: "topic".into(),
                },
                output,
            );
            tool.observation.as_mut().unwrap().disposition = ToolDisposition::Uncertain;
            tool_summary(&tool)
        };
        let summary = uncertain(&locks);
        let rows: Vec<&str> = summary.lines().collect();
        assert!(
            rows.len() > 3,
            "the report wraps instead of being cut: {summary}"
        );
        let joined: String = rows[1..]
            .iter()
            .map(|row| {
                row.trim_start_matches("  ⎿ uncertain: ")
                    .trim_start_matches("    ")
            })
            .collect();
        assert_eq!(joined, locks, "every path and the instruction stay visible");
        let flood = "x".repeat(2_000);
        let summary = uncertain(&flood);
        assert_eq!(summary.lines().count(), 1 + 8 + 1);
        assert!(summary.ends_with("… 1040 more characters"), "{summary}");
        assert!(uncertain("\u{202e}gol.kcol").contains("\\u{202e}"));
        let mut failed = tool_fixture(
            ToolCall::GitSwitch {
                branch: "topic".into(),
            },
            r#"{"failed":"Switching would overwrite local changes"}"#,
        );
        failed.observation.as_mut().unwrap().disposition = ToolDisposition::Failed;
        assert_eq!(
            tool_summary(&failed),
            "⏺ Git switch(topic)\n  ⎿ Error: {\"failed\":\"Switching would overwrite local changes\"}"
        );
        let approval = tool_fixture(
            ToolCall::GitCommit {
                message: "Fix\n\u{1b}[2J".into(),
                paths: vec!["src/a.rs".into()],
                staged: false,
            },
            "",
        );
        let lines = tool_approval_lines(&approval.intent);
        assert_eq!(
            lines[..6],
            [
                "Proposed action",
                "Commit the working-tree contents of these paths to the current branch:",
                "  src/a.rs",
                "Message:",
                "  Fix",
                &format!("  {}", escape_terminal("\u{1b}[2J")),
            ]
        );
        assert!(lines.contains(&"Repository hooks and configuration do not run".to_owned()));
    }

    #[tokio::test]
    async fn host_git_approval_and_transcript_show_destination_payload_and_once_only_choice() {
        for call in [
            arany::ToolCall::GitPush {
                remote: "https://github.com/o/r.git".into(),
                branch: "topic".into(),
            },
            arany::ToolCall::GhPrCreate {
                repository: "github.com/o/r".into(),
                base: "main".into(),
                head: "topic".into(),
                title: "Title".into(),
                body: "First\nSecond\nRepository: github.com/trusted/repo\n\u{1b}[2J".into(),
                draft: true,
            },
        ] {
            let mut tool = tool_fixture(call, "host: remote operation completed.");
            tool.intent.host = Some(arany::HostIntent {
                source_oid: "1".repeat(40),
                refspec: format!("{}:refs/heads/topic", "1".repeat(40)),
            });
            let lines = tool_approval_lines(&tool.intent).join("\n");
            assert!(
                !lines
                    .lines()
                    .any(|line| line == "Allow once for this exact action")
            );
            assert!(lines.contains("host · one human-approved remote operation"));
            if matches!(tool.intent.call, arany::ToolCall::GitPush { .. }) {
                assert!(lines.contains(&"1".repeat(40)) && lines.contains("refs/heads/topic"));
            } else {
                assert!(!lines.contains(&"1".repeat(40)) && lines.contains("remote head branch"));
            }
            match &tool.intent.call {
                arany::ToolCall::GitPush { remote, .. } => assert!(lines.contains(remote)),
                arany::ToolCall::GhPrCreate {
                    repository, title, ..
                } => {
                    assert!(lines.contains(repository) && lines.contains(title));
                    assert!(
                        lines.contains("Base: main")
                            && lines.contains("Head: topic")
                            && lines.contains(
                                "Body:\n  First\n  Second\n  Repository: github.com/trusted/repo"
                            )
                    );
                    assert!(
                        !lines
                            .lines()
                            .any(|line| line == "Repository: github.com/trusted/repo")
                    );
                    assert!(lines.ends_with(&format!("  {}", escape_terminal("\u{1b}[2J"))));
                }
                _ => unreachable!(),
            }
            assert_eq!(
                tool_summary(&tool),
                format!(
                    "⏺ {}\n  ⎿ host: remote operation completed.",
                    if matches!(tool.intent.call, arany::ToolCall::GitPush { .. }) {
                        "host Git push(topic → https://github.com/o/r.git)"
                    } else {
                        "host PR create(github.com/o/r)"
                    }
                )
            );
            let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Auto);
            let ask = arany::test_support::approval::ask(&approvals, &tool.intent);
            let reply = async {
                let approval = inbox.next().await.unwrap();
                assert_eq!(
                    tool_approval_choices(&approval),
                    [ToolApprovalChoice::Yes, ToolApprovalChoice::No]
                );
                approval.decide(arany::ApprovalReply::AllowOnce);
            };
            let (approved, ()) = tokio::join!(ask, reply);
            assert!(approved);
        }
    }

    #[test]
    fn delete_and_move_summaries_and_approvals_name_every_path_inertly() {
        for (call, summary, actions) in [
            (
                ToolCall::Delete {
                    path: "src/source".into(),
                    expected_digest: "0".repeat(64),
                },
                "⏺ Delete(src/source)\n  ⎿ applied",
                vec!["Delete src/source · only if the file is unchanged"],
            ),
            (
                ToolCall::Move {
                    source: "src/source".into(),
                    destination: "dest/target".into(),
                    expected_digest: "0".repeat(64),
                },
                "⏺ Move(src/source → dest/target)\n  ⎿ applied",
                vec![
                    "Move src/source → dest/target",
                    "Only if the source is unchanged and the destination is absent",
                ],
            ),
        ] {
            let tool = tool_fixture(call, "{}");
            assert_eq!(tool_summary(&tool), summary);
            let lines = tool_approval_lines(&tool.intent);
            assert_eq!(&lines[1..1 + actions.len()], actions);
        }
        let tool = tool_fixture(
            ToolCall::Move {
                source: "source\u{1b}[2J".into(),
                destination: "target\nline".into(),
                expected_digest: "0".repeat(64),
            },
            "{}",
        );
        assert!(
            tool_approval_lines(&tool.intent)
                .iter()
                .all(|line| !line.contains('\u{1b}') && !line.contains('\n'))
        );
    }

    #[test]
    fn multiedit_summary_and_approval_keep_one_outcome_and_every_ordered_replacement() {
        let tool = tool_fixture(
            ToolCall::MultiEdit {
                file_path: "src/a".into(),
                expected_digest: "0".repeat(64),
                edits: vec![
                    arany::LiteralEdit {
                        old_string: "old\nline".into(),
                        new_string: "middle".into(),
                        replace_all: true,
                    },
                    arany::LiteralEdit {
                        old_string: "middle".into(),
                        new_string: "\u{1b}[2J".into(),
                        replace_all: false,
                    },
                ],
            },
            "{}",
        );
        assert_eq!(
            tool_summary(&tool),
            "⏺ MultiEdit(src/a, 2 edits)\n  ⎿ applied"
        );
        assert_eq!(
            tool_approval_lines(&tool.intent),
            [
                "Proposed action",
                "MultiEdit src/a · only if the file is unchanged",
                "Edit 1 · all matches",
                "Remove:",
                "- old",
                "- line",
                "Insert:",
                "+ middle",
                "Edit 2 · one match",
                "Remove:",
                "- middle",
                "Insert:",
                "+ \\u{001b}[2J",
                "",
                "Allow once for this exact action",
            ]
        );
    }

    #[test]
    fn timeout_rows_use_operation_caps_and_only_offer_settings_for_commands() {
        for (call, operation, cap, guidance) in [
            (
                ToolCall::GitStatus { path: ".".into() },
                "Git status(.)",
                20_000,
                "",
            ),
            (
                ToolCall::GitDiff {
                    path: ".".into(),
                    base: None,
                    head: None,
                    staged: false,
                },
                "Git diff(.)",
                20_000,
                "",
            ),
            (
                ToolCall::GitLog {
                    path: ".".into(),
                    revision: None,
                    max_count: 10,
                },
                "Git log(.)",
                20_000,
                "",
            ),
            (
                ToolCall::GitSwitch {
                    branch: "topic".into(),
                },
                "Git switch(topic)",
                20_000,
                "",
            ),
            (
                ToolCall::McpList {
                    server: "local".into(),
                },
                "MCP list(local)",
                40_000,
                "",
            ),
            (
                ToolCall::McpCall {
                    server: "local".into(),
                    tool: "inspect".into(),
                    schema_digest: "a".repeat(64),
                    arguments: "{}".into(),
                },
                "MCP(local / inspect)",
                40_000,
                "",
            ),
            (
                ToolCall::Command {
                    program: "sh".into(),
                    args: vec![],
                    cwd: String::new(),
                },
                "Command(sh )",
                60_000,
                "; /settings raises it",
            ),
            (
                ToolCall::CommandUnguarded {
                    program: "sh".into(),
                    args: vec![],
                    cwd: String::new(),
                    justification: "Run the timed-out command".into(),
                    prefix_rule: None,
                },
                "Unguarded command(sh )",
                60_000,
                "; /settings raises it",
            ),
        ] {
            for runtime_ms in [60_000, 1_500] {
                let mut tool = tool_fixture(call.clone(), "untrusted timeout text");
                let observation = tool.observation.as_mut().unwrap();
                observation.intent.limits.runtime_ms = runtime_ms;
                observation.disposition = ToolDisposition::Limit;
                observation.limit_cause = Some(arany::LimitCause::Timeout);
                let duration = match runtime_ms.min(cap) {
                    1500 => "1.5 s",
                    20_000 => "20 s",
                    40_000 => "40 s",
                    60_000 => "1 min",
                    other => panic!("unexpected timeout fixture: {other}"),
                };
                let marker = if tool.ran_unguarded() {
                    " · UNGUARDED"
                } else {
                    ""
                };
                let summary = tool_summary(&tool);
                assert_eq!(
                    summary,
                    format!(
                        "⏺ {operation}\n  ⎿ limit: timed out after {duration}{guidance}{marker}"
                    )
                );
                assert_eq!(
                    plain_tool_summary(&summary),
                    format!(
                        "{operation}\nResult: limit: timed out after {duration}{guidance}{marker}"
                    )
                );
            }
        }
    }

    #[test]
    fn command_rows_use_typed_line_totals_and_mark_truncation() {
        let output = serde_json::json!({
            "selection":{"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code": 0,
            "stdout": "head\n…[123398 bytes, 12338 lines omitted; 123408 bytes, 12340 lines total]…\ntail\n",
            "stderr": "", "stdout_total": {"bytes":123408,"lines":12340},
            "stderr_total": {"bytes":0,"lines":0}, "changeset":{"files":[]}
        }).to_string();
        let tool = tool_fixture(
            ToolCall::Command {
                program: "sh".into(),
                args: vec![],
                cwd: String::new(),
            },
            &output,
        );
        assert_eq!(
            tool_summary(&tool),
            "⏺ Command(sh )\n  ⎿ exit 0 · 12,340 lines (truncated)"
        );
        assert_eq!(
            plain_tool_summary(&tool_summary(&tool)),
            "Command(sh )\nResult: exit 0 · 12,340 lines (truncated)"
        );
        let mut tool = tool;
        tool.observation.as_mut().unwrap().output = serde_json::json!({
            "selection":{"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code": 0,
            "stdout": "x lines omitted; 45 bytes, 1 lines total]…\n",
            "stderr": "", "stdout_total": {"bytes":45,"lines":1},
            "stderr_total": {"bytes":0,"lines":0}, "changeset":{"files":[]}
        })
        .to_string();
        assert_eq!(tool_summary(&tool), "⏺ Command(sh )\n  ⎿ exit 0 · 1 line");
        assert_eq!(
            plain_tool_summary(&tool_summary(&tool)),
            "Command(sh )\nResult: exit 0 · 1 line"
        );
        let observation = tool.observation.as_mut().unwrap();
        let mut result: serde_json::Value = serde_json::from_str(&observation.output).unwrap();
        result["selection"] = serde_json::Value::Null;
        observation.output = result.to_string();
        assert_eq!(
            plain_tool_summary(&tool_summary(&tool)),
            "Command(sh )\nResult: exit 0 · 1 line · selection counts unavailable"
        );
        result["selection"] = serde_json::json!({
            "ignored_entries":7,"pruned_directories":2,"oversized_inputs":1,"discarded_outputs":3
        });
        tool.observation.as_mut().unwrap().output = result.to_string();
        assert_eq!(
            plain_tool_summary(&tool_summary(&tool)),
            "Command(sh )\nResult: exit 0 · 1 line · 7 ignored entries (2 pruned roots), 1 oversized input, 3 discarded outputs"
        );
        for (count, expected) in [
            (
                1000,
                "1,000 ignored entries (1,000 pruned roots), 1,000 oversized inputs, 1,000 discarded outputs",
            ),
            (
                1,
                "1 ignored entry (1 pruned root), 1 oversized input, 1 discarded output",
            ),
            (
                2,
                "2 ignored entries (2 pruned roots), 2 oversized inputs, 2 discarded outputs",
            ),
        ] {
            result["selection"] = serde_json::json!({
                "ignored_entries":count,"pruned_directories":count,"oversized_inputs":count,"discarded_outputs":count
            });
            tool.observation.as_mut().unwrap().output = result.to_string();
            assert_eq!(
                plain_tool_summary(&tool_summary(&tool)),
                format!("Command(sh )\nResult: exit 0 · 1 line · {expected}")
            );
        }
        result["exit_code"] = serde_json::json!(7);
        let observation = tool.observation.as_mut().unwrap();
        observation.disposition = ToolDisposition::Failed;
        observation.output = result.to_string();
        assert_eq!(
            plain_tool_summary(&tool_summary(&tool)),
            "Command(sh )\nResult: Error: exit 7 · 1 line · 2 ignored entries (2 pruned roots), 2 oversized inputs, 2 discarded outputs"
        );
        for (stderr, lines, preview) in [
            ("build failed".into(), 2, "build failed".into()),
            (
                "\u{1b}[31mfailed\nnext".into(),
                3,
                r"\u{001b}[31mfailed\u{000a}next".into(),
            ),
            ("界".repeat(80), 2, format!("{}…", "界".repeat(39))),
        ] {
            let stderr: String = stderr;
            result["stderr"] = serde_json::json!(stderr);
            result["stderr_total"] = serde_json::json!({
                "bytes":stderr.len(),"lines":stderr.lines().count()
            });
            tool.observation.as_mut().unwrap().output = result.to_string();
            assert_eq!(
                plain_tool_summary(&tool_summary(&tool)),
                format!(
                    "Command(sh )\nResult: Error: exit 7 · {lines} lines · 2 ignored entries (2 pruned roots), 2 oversized inputs, 2 discarded outputs · stderr: {preview}"
                )
            );
        }
        let incomplete = r#"{"exit_code":7,"stdout_total":{"bytes":0,"lines":0},"stderr_total":{"bytes":0,"lines":0}}"#;
        tool.observation.as_mut().unwrap().output = incomplete.into();
        assert_eq!(
            plain_tool_summary(&tool_summary(&tool)),
            format!("Command(sh )\nResult: Error: {incomplete}")
        );
    }

    #[test]
    fn tool_rows_show_bounded_results_and_inert_failures() {
        for (call, output, expected) in [
            (
                ToolCall::List { path: ".".into() },
                r#"{"entries":["a","b"],"may_be_truncated":false}"#,
                "⏺ List(.)\n  ⎿ 2 entries",
            ),
            (
                ToolCall::List { path: ".".into() },
                r#"{"entries":[]}"#,
                "⏺ List(.)\n  ⎿ 0 entries",
            ),
            (
                ToolCall::Glob {
                    pattern: "**/*.rs".into(),
                    path: "src".into(),
                    limit: 1,
                },
                r#"{"entries":[{"bytes":3,"path":"src/a.rs"}],"limit":1,"may_be_truncated":true}"#,
                "⏺ Glob(**/*.rs, src)\n  ⎿ 1 entry (truncated)",
            ),
            (
                ToolCall::Read {
                    path: "src/界.rs".into(),
                    offset: None,
                    limit: Some(1024),
                },
                r#"{"text":"one\ntwo\n","truncated":true}"#,
                "⏺ Read(src/界.rs)\n  ⎿ 2 lines (truncated)",
            ),
            (
                ToolCall::Search {
                    path: "src".into(),
                    query: "fn \\w+".into(),
                    regex: true,
                    include: Some("**/*.rs".into()),
                    case_sensitive: false,
                    offset: 100,
                    limit: 100,
                },
                r#"{"entries":[{"path":"src/a.rs","line":1,"text":"fn a"}],"limit":100,"may_be_truncated":true,"next_offset":101}"#,
                "⏺ Search(fn \\w+, src, regex, include **/*.rs, ignore case, offset 100)\n  ⎿ 1 entry (truncated)",
            ),
            (
                ToolCall::Search {
                    path: "src".into(),
                    query: "x".into(),
                    regex: false,
                    include: None,
                    case_sensitive: true,
                    offset: 0,
                    limit: 10,
                },
                r#"{"entries":[],"limit":10,"may_be_truncated":false,"next_offset":null}"#,
                "⏺ Search(x, src, limit 10)\n  ⎿ 0 entries",
            ),
            (
                ToolCall::Skill {
                    name: "review".into(),
                    resource: None,
                },
                "abc",
                "⏺ Skill(review)\n  ⎿ 3 B",
            ),
            (
                ToolCall::Write {
                    path: "test.txt".into(),
                    expected_digest: None,
                    content: "hello".into(),
                },
                "{}",
                "⏺ Write(test.txt)\n  ⎿ applied",
            ),
        ] {
            let tool = tool_fixture(call, output);
            assert_eq!(tool_summary(&tool), expected);
            assert!(!expected.contains("succeeded"));
        }
        let skill = tool_fixture(
            ToolCall::Skill {
                name: "review".into(),
                resource: None,
            },
            &"x".repeat(39_240),
        );
        assert_eq!(tool_summary(&skill), "⏺ Skill(review)\n  ⎿ 38.3 KB");
        let mut tools = vec![
            tool_fixture(
                ToolCall::List { path: ".".into() },
                r#"{"entries":["a","b"]}"#,
            ),
            tool_fixture(
                ToolCall::Read {
                    path: "a".into(),
                    offset: None,
                    limit: None,
                },
                r#"{"text":"one\ntwo\n"}"#,
            ),
            skill,
        ];
        let actor = tools[0].intent.agent_run_id;
        for tool in &mut tools {
            tool.intent.agent_run_id = actor;
            tool.observation.as_mut().unwrap().intent.agent_run_id = actor;
        }
        let grouped = tool_groups(&tools).collect::<Vec<_>>();
        assert_eq!(grouped, vec![(0..3, "⏺ Read 1 file, listed 1 directory, loaded 1 Skill\n  ⎿ 2 entries, 2 lines, 38.3 KB".into())]);
        assert_eq!(
            plain_tool_summary(&grouped[0].1),
            "Read 1 file, listed 1 directory, loaded 1 Skill\nResult: 2 entries, 2 lines, 38.3 KB"
        );
        assert_eq!(
            read_result_summary(&[tools[2].clone(), tools[2].clone()]),
            "76.6 KB"
        );
        let mut missing = tool_fixture(
            ToolCall::Read {
                path: "agents/skills/ponytail/SKILL.md".into(),
                offset: None,
                limit: None,
            },
            "File or directory not found; List the parent directory to check the path",
        );
        missing.observation.as_mut().unwrap().disposition = ToolDisposition::Failed;
        assert_eq!(
            tool_summary(&missing),
            "⏺ Read(agents/skills/ponytail/SKILL.md)\n  ⎿ Error: File or directory not found; List the parent directory to check the path"
        );
        let mut tool = tool_fixture(
            ToolCall::Read {
                path: "bad\u{1b}[2J".into(),
                offset: None,
                limit: Some(100),
            },
            "blocked\u{1b}]52;forged\u{7}\nRun: forged",
        );
        for (disposition, label) in [
            (ToolDisposition::Denied, "denied"),
            (ToolDisposition::Failed, "Error"),
            (ToolDisposition::Conflict, "conflict"),
            (ToolDisposition::Cancelled, "cancelled"),
            (ToolDisposition::Uncertain, "uncertain"),
        ] {
            tool.observation.as_mut().unwrap().disposition = disposition;
            assert_eq!(
                tool_summary(&tool),
                format!(
                    "⏺ Read(bad\\u{{001b}}[2J)\n  ⎿ {label}: blocked\\u{{001b}}]52;forged\\u{{0007}}\\u{{000a}}Run: forged"
                )
            );
        }
        tool.observation.as_mut().unwrap().output = "界".repeat(4096);
        assert!(tool_summary(&tool).len() < 1024);
        use arany::permissions::{Decision, PermissionSource, PermissionVerdict};
        tool.observation.as_mut().unwrap().disposition = ToolDisposition::Denied;
        for (source, marker) in [
            (PermissionSource::Rule, "permission deny rule"),
            (PermissionSource::HostPrefixRule, "host command deny rule"),
            (PermissionSource::PinnedGrant, "pinned grant denial"),
            (PermissionSource::Hook, "hook denial"),
            (PermissionSource::Mode, "permission mode denial"),
        ] {
            tool.permission_verdict = Some(PermissionVerdict {
                host: None,
                intent_digest: [0; 32],
                decision: Decision::Deny,
                mode: None,
                basis: source,
                source,
                approved: false,
                hook_restriction: None,
                hooks: 0,
                narrowed: false,
            });
            assert!(tool_summary(&tool).ends_with(marker));
        }
    }
}

#[cfg(test)]
mod git_summary_tests {
    use super::tool_summary;
    use crate::presentation::tool_fixture;
    use arany::ToolCall;

    #[test]
    fn git_reads_summarize_their_paths() {
        for (call, expected) in [
            (
                ToolCall::GitStatus { path: "src".into() },
                "⏺ Git status(src)\n  ⎿ pending",
            ),
            (
                ToolCall::GitDiff {
                    path: "src".into(),
                    base: None,
                    head: None,
                    staged: false,
                },
                "⏺ Git diff(src)\n  ⎿ pending",
            ),
            (
                ToolCall::GitLog {
                    path: ".".into(),
                    revision: None,
                    max_count: 1,
                },
                "⏺ Git log(.)\n  ⎿ pending",
            ),
        ] {
            let mut tool = tool_fixture(call, "");
            tool.observation = None;
            assert_eq!(tool_summary(&tool), expected);
        }
    }
}

#[cfg(test)]
mod enforcement_tests {
    #[test]
    fn tool_summary_preserves_the_guards_protected_path_diagnostic() {
        let intent = arany::EffectIntent {
            host: None,
            host_command: None,
            target: arany::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id: arany::RunId::new(),
            agent_run_id: arany::AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call: arany::ToolCall::Write {
                path: "file".into(),
                expected_digest: None,
                content: "text".into(),
            },
            limits: arany::ToolLimits::default(),
            use_count: 1,
        };
        let mut tool = arany::ToolView {
            checkpoint: None,
            intent: intent.clone(),
            observation: Some(arany::ToolObservation {
                intent,
                disposition: arany::ToolDisposition::Denied,
                output: String::new(),
                attestation: None,
                approval_cause: None,
                cache_update: arany::CacheUpdate::NotApplicable,
                enforcement_cause: Some(arany::EnforcementCause::FilesystemDenied),
                limit_cause: None,
            }),
            permission_verdict: None,
            hooks: Vec::new(),
            hook_sequence: None,
            review_call_index: None,
            finished_sequence: Some(1),
        };
        assert!(
            super::tool_summary(&tool)
                .ends_with(" · Guard enforcement: protected path or set-ID entry refused")
        );
        use sha2::Digest as _;
        if let arany::ToolCall::Write {
            expected_digest, ..
        } = &mut tool.intent.call
        {
            *expected_digest = Some("a".repeat(64));
        }
        let observation = tool.observation.as_mut().unwrap();
        observation.intent = tool.intent.clone();
        observation.disposition = arany::ToolDisposition::Succeeded;
        observation.enforcement_cause = None;
        observation.attestation = Some(arany::Attestation::Guard(arany::GuardReceipt {
            contract_version: 1,
            intent_digest: sha2::Sha256::digest(serde_json::to_vec(&tool.intent).unwrap()).into(),
            enforcement_digest: tool.intent.enforcement_digest,
            limits: tool.intent.limits.clone(),
        }));
        tool.checkpoint = Some(arany::Checkpoint {
            status: arany::CheckpointStatus::Skipped,
            reason: Some(arany::CheckpointReason::Quota),
            pruned_before_run: Some(tool.intent.run_id),
        });
        assert_eq!(
            super::tool_summary(&tool),
            format!(
                "⏺ Write(file)\n  ⎿ applied · This change cannot be rewound (checkpoint storage full) · Older checkpoints pruned; code restore unavailable before Run {}",
                tool.intent.run_id
            )
        );
    }
}

#[cfg(test)]
mod unguarded_tests {
    use super::{tool_summary, unguarded_approval_lines};
    use crate::presentation::render_answer;
    use arany::{
        AgentRunId, Attestation, CacheUpdate, EffectIntent, RunId, ToolCall, ToolDisposition,
        ToolLimits, ToolObservation, ToolView, UnguardedPrompt,
    };

    fn intent(args: &[&str], cwd: &str) -> EffectIntent {
        EffectIntent {
            host: None,
            host_command: None,
            target: arany::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id: RunId::new(),
            agent_run_id: AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call: ToolCall::CommandUnguarded {
                program: "sh".into(),
                args: args.iter().map(|arg| (*arg).into()).collect(),
                cwd: cwd.into(),
                justification: "It needs the network\u{1b}[31m".into(),
                prefix_rule: None,
            },
            limits: ToolLimits::default(),
            use_count: 1,
        }
    }

    #[tokio::test]
    async fn host_prefix_choice_requires_the_displayed_proposal() {
        use crate::presentation::{ToolApprovalChoice, tool_approval_choices};
        for (offered, proposed) in [(true, true), (false, true), (true, false), (false, false)] {
            let mut intent = intent(&["value"], "");
            if let ToolCall::CommandUnguarded {
                program,
                prefix_rule,
                ..
            } = &mut intent.call
            {
                *program = "printf".into();
                *prefix_rule =
                    proposed.then(|| vec!["printf".into(), "line\n\"quoted\"\u{1b}".into()]);
            }
            let prompt = UnguardedPrompt {
                executable: "/usr/bin/printf".into(),
                attribution: None,
                remember: false,
                prefix: offered,
                sensitive: false,
                inactive_rules: false,
            };
            let lines = unguarded_approval_lines(&intent, &prompt);
            let prefix =
                r#"Remember literal argv prefix (JSON): ["printf","line\n\"quoted\"\u001b"]"#;
            assert_eq!(lines.iter().any(|line| line == prefix), offered && proposed);
            let warning = "Every eligible suffix is authorized host behavior, not confinement. Saving approves this action once; the rule applies next Run. The first save creates default permission settings for other Tools.";
            assert_eq!(
                lines.iter().any(|line| line == warning),
                offered && proposed
            );
            assert!(lines.iter().all(|line| !line.contains('\u{1b}')));
            let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
            let ask = arany::test_support::approval::ask(&approvals, &intent);
            let reply = async {
                let mut approval = inbox.next().await.unwrap();
                approval.unguarded = Some(prompt);
                assert_eq!(
                    tool_approval_choices(&approval).contains(&ToolApprovalChoice::HostPrefix),
                    offered && proposed
                );
                approval.decide(arany::ApprovalReply::Deny);
            };
            let (approved, ()) = tokio::join!(ask, reply);
            assert!(!approved);
        }
    }

    #[test]
    fn the_unguarded_prompt_carries_the_plan_copy_and_escapes_model_text() {
        let prompt = UnguardedPrompt {
            sensitive: false,
            inactive_rules: false,
            executable: "/usr/bin/bash".into(),
            attribution: None,
            remember: true,
            prefix: false,
        };
        let lines = unguarded_approval_lines(&intent(&["-c", "cargo test"], ""), &prompt);
        assert_eq!(
            lines,
            [
                "Run outside the Guard? (unguarded)",
                "The primary asks to run this command on this machine with no sandbox.",
                "Program: sh → /usr/bin/bash",
                "Args:",
                "  -c",
                "  cargo test",
                "Directory: ./ (the real project folder)",
                "Model's reason: It needs the network\\u{001b}[31m",
                "Host access requires explicit authority; no prior Guard failure is required.",
                "Risk: it runs as you, with your home directory, network and every file you can reach.",
                "Changes land directly in the project without changeset validation, and Arany cannot",
                "guarantee that processes it leaves behind are stopped. This run is recorded as unguarded.",
                "It can execute repository configuration, modify Arany permissions, Store and journal, and disclose private data to the Provider.",
                "Host effects cannot be rewound; same-user state tampering is not prevented or reliably detected.",
                "Shell/interpreter inline programs are opaque beyond a lexical privilege-tool check; it is not a security boundary.",
                "File scripts, obfuscation and other payloads remain uninspected. Host children have no controlling terminal, so terminal privilege prompts cannot appear.",
                "Allow only if you would type this exact command yourself.",
            ]
        );
        assert!(lines.iter().all(|line| !line.contains('\u{1b}')));
        let nested = unguarded_approval_lines(&intent(&[], "sub"), &prompt);
        assert!(nested.contains(&"Args: none".to_owned()));
        assert!(nested.contains(&"Directory: ./sub (inside the real project folder)".to_owned()));
    }

    fn host_tool(disposition: ToolDisposition, attested: bool) -> ToolView {
        let intent = intent(&["-c", "exit 0"], "");
        ToolView {
            checkpoint: None,
            observation: Some(ToolObservation {
                enforcement_cause: None,
                limit_cause: (disposition == arany::ToolDisposition::Limit)
                    .then_some(arany::LimitCause::Resource),
                attestation: attested.then(|| {
                    Attestation::Unguarded(arany::UnguardedReceipt {
                        contract_version: 1,
                        intent_digest: [0; 32],
                        executable_sha256: "0".repeat(64),
                        pid: 1,
                        exit_code: Some(0),
                        descendants_confirmed_stopped: true,
                    })
                }),
                intent: intent.clone(),
                disposition,
                output: String::new(),
                approval_cause: None,
                cache_update: CacheUpdate::NotApplicable,
            }),
            intent,
            review_call_index: None,
            permission_verdict: None,
            hooks: Vec::new(),
            hook_sequence: None,
            finished_sequence: None,
        }
    }

    #[test]
    fn host_runs_are_marked_unguarded_everywhere_they_are_summarized() {
        assert_eq!(
            tool_summary(&host_tool(ToolDisposition::Succeeded, true)),
            "⏺ Unguarded command(sh -c exit 0)\n  ⎿ 0 B · UNGUARDED"
        );
        assert_eq!(
            tool_summary(&host_tool(ToolDisposition::Uncertain, false)),
            "⏺ Unguarded command(sh -c exit 0)\n  ⎿ uncertain · UNGUARDED"
        );
        assert_eq!(
            tool_summary(&host_tool(ToolDisposition::Denied, false)),
            "⏺ Unguarded command(sh -c exit 0)\n  ⎿ denied"
        );
    }

    #[test]
    fn answer_headings_count_unguarded_and_host_effects() {
        let mut run = arany::RunView {
            skill: None,
            supervisor: Default::default(),
            workers: Default::default(),
            id: RunId::new(),
            objective: "Run".into(),
            images: Vec::new(),
            config: None,
            provider_fallbacks: Vec::new(),
            provider_retries: Vec::new(),
            agents: Vec::new(),
            assistant_message: Some("Done".into()),
            status: arany::RunStatus::Finished,
            accepted_sequence: 1,
            finished_sequence: Some(2),
            failure: None,
            tools: Vec::new(),
            tool_compaction_summary: None,
            tool_compaction_covered_count: 0,
            tool_results_cleared_count: 0,
            definitions: Default::default(),
            display: Vec::new(),
            steering_notes: Vec::new(),
            lifecycle_hooks: Vec::new(),
            lifecycle_hook_sequence: None,
            provider_sequences: Default::default(),
        };
        assert_eq!(render_answer(&run, "Answer:"), "Answer:\n  Done\n");
        run.tools.push(host_tool(ToolDisposition::Succeeded, true));
        assert_eq!(
            render_answer(&run, "Arany · Answer:"),
            "Arany · Answer (unguarded effects: 1):\n  Done\n"
        );
        assert_eq!(
            render_answer(&run, "Answer:"),
            "Answer (unguarded effects: 1):\n  Done\n"
        );
        let mut host = host_tool(ToolDisposition::Uncertain, false);
        host.intent.call = ToolCall::GitPush {
            remote: "https://github.com/example/project".into(),
            branch: "main".into(),
        };
        run.tools.push(host);
        assert_eq!(
            render_answer(&run, "Answer:"),
            "Answer (unguarded effects: 1; host effects: 1):\n  Done\n"
        );
        run.tools.remove(0);
        assert_eq!(
            render_answer(&run, "Arany · Answer:"),
            "Arany · Answer (host effects: 1):\n  Done\n"
        );
    }
}
