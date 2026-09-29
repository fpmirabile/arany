# Arany

Arany is a CLI-first runtime that executes one bounded team of agents through one reusable loop. The executable is `arany`; the repository directory name is not a product identifier. This glossary names only the concepts required by the minimum demonstrator.

## Language

**Arany**:
The complete CLI product that accepts an objective, coordinates agent work, and presents the result.
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
One bounded team effort from a user objective to a terminal outcome.
_Avoid_: Session, request, job

**AgentRun**:
One execution of the reusable agent loop within a Run. The root AgentRun orchestrates; child AgentRuns perform bounded objectives.
_Avoid_: Agent, worker process, task

**Event**:
An immutable fact about a Run or AgentRun transition.
_Avoid_: Message, notification, log line

**RunView**:
The current Run and AgentRun state reconstructed from Events for terminal rendering.
_Avoid_: Projection set, dashboard, UI state

**Provider**:
An external model-inference implementation used by the Engine through its only substitutable behavior seam.
_Avoid_: Model, LLM service

**Telemetry**:
An optional, lossy operational trace projection derived from safe Engine facts after canonical Event commits.
_Avoid_: Event journal, Run history, logging subsystem
