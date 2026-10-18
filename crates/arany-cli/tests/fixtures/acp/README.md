# ACP v1 conformance fixture

`schema.json` is the unmodified upstream `schema/v1/schema.json` from
[agentclientprotocol/agent-client-protocol](https://github.com/agentclientprotocol/agent-client-protocol/tree/2c8270462725d07d9d7cd845beb21dc99c9eae68),
commit `2c8270462725d07d9d7cd845beb21dc99c9eae68` (schema package 1.12.0,
protocol version 1). SHA-256:
`4bcd1d2051ff7ed379bd9b234fb9b09c5ab9245478698e9fab805bd9b52a33a6`.
The adjacent `LICENSE` preserves its Apache-2.0 license. This fixture is used
only by tests; no upstream ACP package is a runtime dependency.

`basic.jsonl` is the manually maintained core text-turn transcript. Only the
Session UUID, Workspace directory and package version are canonicalized.
