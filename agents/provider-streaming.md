# Provider streaming and provisional display

Load for shared SSE framing, incremental result extraction or Provider preview publication. [Provider](./provider.md), [Engine](./engine.md), [security](./security.md) and [execution](../docs/specs/execution.md) remain authoritative.

- A preview is a bounded candidate, never an accepted outcome, reasoning channel, Tool argument or authority source. Extract only the exact final-result path or explicitly public commentary. Structured JSON field order is not a Finish guarantee.
- Decode JSON escapes, UTF-8 and surrogate pairs across fragments. Check known selected credentials before exposing or recording text; retain any suffix that can complete a credential. Cancellation must not flush that ambiguous suffix.
- One latest snapshot coalesces display updates; its complete semantic text and commentary stay within the terminal contract's bounds. Complete response/model/usage/outcome checks still decide acceptance.
- Concrete adapters own their wire protocols. Shared framing admits at most 1 MiB and 8,192 events, requires duplicate-free event JSON capped at 4,096 nodes and depth 32, and label/type agreement, and never silently switches media or response encoding.
- Checkpoints belong to the Engine and canonical Store, not the adapter. Child work, Tool review, compaction and headless execution have no conversation preview sink.
- Offline fixtures prove fragmentation, hostile controls, cross-fragment credentials, bounds and strict closure. Real native/custom/ChatGPT streaming compatibility still requires independently activated live evidence.
