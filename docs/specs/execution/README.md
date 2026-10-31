# Run execution

## Scope and related contracts

This contract owns bounded Run coordination, delegation, accepted outcomes and history-capacity rejection. The [Goals contract](../goals.md) owns durable task lifecycle, Goal budgets, plan contracts and recovery; every accepted task links its Runs to one Goal. [Tools](../tools/README.md) owns grants and effect continuations; [terminal](../terminal/README.md) owns attached controls and agent inspection. Session lifecycle and Provider admission stay in the [Session](../session.md) and [Provider](../provider/README.md) contracts. [Next steps](../../../NEXT_STEPS.md) owns remaining native and live checks.

Editing constraints: [security rules](../../../agents/security.md).

Topic files:

- [Collaboration and agent definitions](./collaboration.md): Collaboration; Researcher children; Agent definitions.
- [Events, worker journals and outcomes](./events-and-outcomes.md): Committed Event stream; Worker journal protocol; Accepted outcomes.
- [Admission and recovery](./admission-and-recovery.md): Admission, persistence and recovery.
- [Incompatible state and persistence bounds](./persistence.md): Incompatible history; Incompatible records; Workspace rewind Events; Persistence bounds.
