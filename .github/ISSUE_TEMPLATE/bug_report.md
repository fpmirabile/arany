---
name: Bug report
about: Report a problem with enough detail to reproduce and verify a fix.
---

<!--
For humans and agents: write in English for readers with limited attention.
TL;DR first; aim for 1–3 short bullets per section, except numbered repro steps.
Replace placeholders. Use "unknown" for missing facts; do not guess.
Link or collapse longer evidence. Remove secrets, personal data, and private
prompts/payloads from commands, logs, and screenshots before sharing.
Agents: read AGENTS.md and the affected contract before implementing a fix.
-->

## Summary

<!-- One sentence: what fails, when, and how it affects the user. -->

<problem and impact>

## Where and environment

<!-- Include relevant details only; provider/model, terminal, and configuration
may matter. File/function locations are helpful when known, never required. -->

- **Area:** <command/workflow; file or function if known>
- **Version:** <Arany version or commit>
- **Environment:** <OS/version and relevant setup>

## Reproduction

<!-- Smallest reproducible example: setup, exact command/input, then trigger.
If you cannot reproduce reliably, say so and state the observed frequency. -->

1. <preconditions or minimal setup>
2. <command or action>
3. <action that triggers the failure>

## Expected vs. actual

- **Expected:** <observable result>
- **Actual:** <observable result, including error or exit code when relevant>

## Evidence

<!-- A short sanitized excerpt, screenshot, or failing test is enough.
Separate observed facts from hypotheses. Link related issues or the relevant
contract/scenario when known. Missing evidence is "unknown", not invented. -->

- <evidence or link>
