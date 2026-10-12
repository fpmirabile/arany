<!--
For humans and agents: write for readers with limited attention.
Keep everything in English and TL;DR: lead with the point, use plain language,
and aim for 1–3 short bullets per section. Link or collapse longer evidence.
Replace placeholders; use "unknown" for missing facts and "N/A — <reason>"
when a section does not apply. Skip filler and the history of your work.
Agents: read AGENTS.md and follow this structure even when publishing by CLI/API.
Never include secrets, personal data, or raw private prompts and payloads.
-->

## Summary

<!-- Explain the concrete problem and resulting behavior. Link the issue if one exists. -->

- **Problem:** <what happens today and why it matters>
- **Change:** <what happens after this PR; before → after when useful>

## Verification

<!-- Report only checks actually run: command/scenario → result and what it proves.
Name the OS for native evidence. List skipped/blocked checks and the reason.
Use a short before/after excerpt or screenshot when it helps; avoid raw log dumps. -->

- **Checked:** <command or scenario → result; claim verified>
- **Not run:** <check → reason, or N/A with a reason>

## Security

<!-- Check agents/security.md triggers. When triggered, name the affected trust
boundary, attack/failure prevented, and evidence or remaining gap. Otherwise
write N/A with a reason. Do not claim safety from passing tests alone. -->

- <security impact and evidence, or N/A with a reason>

## Risks

<!-- Name what could break, who/what is affected, and any OS limitations.
Explain rollback; flag irreversible effects. Link follow-up work for known gaps. -->

- **Impact:** <concrete risk and affected area, or N/A with a reason>
- **Rollback:** <how to undo this change; any irreversible effects>

## Documentation

<!-- Link changed docs/specs and acceptance scenarios. If unchanged, explain why
the existing contract is adequate or why there is no observable behavior change. -->

- <documentation/contract impact and relevant links, or N/A with a reason>
