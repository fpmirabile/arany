# Provider MCP argument admission

## Status and scope

Fixed in the current uncommitted beta implementation; local regression evidence passes. No live account, credential, or prior private journal was inspected. The finding concerns selected-credential exclusion before canonical Tool persistence, not a demonstrated Guard escape.

## Failure and evidence

An MCP Tool outcome carries arguments as a JSON string. The former validity check bounded that string but did not parse it. Provider credential scanning used a separate `serde_json::Value` decoder: duplicate object keys overwrote earlier values, and malformed JSON made the scan return false. An encoded credential echo in an overwritten argument value could therefore survive Provider admission and enter the serialized `ToolStarted` intent before the Guard's stricter argument parser rejected execution. A raw-byte credential check does not catch a JSON-escaped echo.

The two existing native decoded-reflection corpus owners reproduced the overwritten-escaped-value defect and failed before the repair. Both pass after the repair, covering encoded values, arrays and keys, duplicate/encoded-duplicate keys, malformed JSON and non-object controls. These are hand-authored synthetic responses, not captured credentials or user requests.

## Repair and ownership

`ToolCall` owns one crate-private MCP argument admission operation backed by the existing strict JSON parser: at most 8 KiB, depth 32 and 4,096 nodes, one object, no duplicate decoded keys. Typed Tool validity, Provider credential scanning and Guard MCP execution share it. Invalid parse is rejection, never evidence that a credential is absent. JSON Schema compilation remains inside the killable Guard; no new parser dependency, billing route, endpoint or public API was added.

Shared Provider acceptance and check/admission contracts were versioned accordingly. Previously accepted malformed, duplicate-key or non-object MCP intents now fail strict replay; valid historical encoding remains unchanged. No migration or rewrite was performed, and replay never dispatches a stored Tool.

## Security non-claims and handoff

This excludes the exact selected credential in the covered decoded fields; it is not general secret detection, encryption, secure erasure, or proof about every prior saved history. No existing user data was rewritten or deleted. Actual account turns and native platform/service validation remain the user's final checklist, not inferred from synthetic regression success.
