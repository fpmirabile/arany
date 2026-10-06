# Arany

Arany is a CLI-first runtime for durable conversations whose Runs execute bounded single-agent or team work. The executable is `arany`; the repository directory name is not a product identifier. This glossary names only the concepts required by the beta.

## Language

**Arany**:
The complete CLI product that maintains Sessions, coordinates bounded agent work, and presents durable results.
_Avoid_: App, wrapper, agent framework

**Harness**:
The product category, used generically for an agent runtime rather than as Arany's proper name.
_Avoid_: Using Harness as the product or executable name

**Engine**:
The policy-owning module that advances a Run and owns its lifecycle invariants.
_Avoid_: Core, backend, server

**Workspace**:
The authorized read-only project scope available to a Run.
_Avoid_: Repository, working directory, project root

**Run**:
One bounded execution attempt for an accepted user objective within a Session. A Run may contain only the primary AgentRun or a bounded team.
_Avoid_: Session, conversation, job

**Session**:
A durable ordered conversation containing authored Messages and their Runs. Session history is canonical local state, not a provider-owned conversation object.
_Avoid_: Run, replay, chat buffer

**Message**:
An authored user input or assistant output in a Session, associated with the Run that accepted or produced it.
_Avoid_: Event, prompt fragment, terminal line

**Team**:
The AgentRuns participating in one Run: one accountable primary and zero or more bounded read-only children selected by the Run's agent mode and limits.
_Avoid_: Session, fixed worker pair, peer swarm

**AgentRun**:
One accountable execution within a Run. The primary AgentRun owns the iterative Tool/delegation/synthesis loop; each child AgentRun makes one bounded read-only Provider call.
_Avoid_: Agent, worker process, task

**Event**:
An immutable fact about a Run or AgentRun transition.
_Avoid_: Message, notification, log line

**RunView**:
The current Run and AgentRun state reconstructed from Events for terminal rendering.
_Avoid_: Projection set, dashboard, UI state

**SessionView**:
The ordered Messages and Runs, active RunView, and compaction provenance reconstructed for one Session.
_Avoid_: Provider conversation, terminal transcript, mutable chat state

**PresentationModel**:
A bounded semantic view derived purely from a SessionView and its active RunView. Terminal presentation may select and clip semantic fields by available width; geometry remains outside Engine state.
_Avoid_: UI state, terminal buffer, second projection store

**Provider**:
An external model-inference implementation used by the Engine through its only substitutable behavior seam.
_Avoid_: Model, LLM service

**ProviderProfile**:
A trusted user-selected binding of a protocol family, endpoint, credential reference, model, and verified capability claims. It never grants a repository authority to choose egress or secrets.
_Avoid_: Base URL, compatibility mode, Provider

**Provider Account**:
A saved connection to a Provider owned by one operating-system user and selectable across that user's Sessions. A Session references an account but does not own its credentials.
_Avoid_: Machine-global account, Session credential

**Telemetry**:
An optional, lossy operational trace projection derived from safe Engine facts after canonical Event commits.
_Avoid_: Event journal, Run history, logging subsystem
