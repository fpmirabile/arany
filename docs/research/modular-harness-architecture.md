# Modular harness architecture: reasons and expansion gates

**Research baseline:** 2026-09-29.

This report explains information hiding and evaluates possible future Client
boundaries. [The system overview](../architecture/system-overview.md) owns the
physical architecture; [the decision register](./next-step-decision-register.md)
owns accepted choices. Protocol and crate sketches are not implementation plans.

## Information hiding before topology

Parnas's decomposition criterion groups code around difficult or changeable
choices, rather than stages in a flowchart. Martin's dependency rule keeps
policy independent of external mechanisms. Ousterhout's deep-module criterion
asks whether callers learn substantially less than the implementation hides.
These ideas support one Engine that owns admission, coordination and durable
transitions instead of requiring callers to assemble a sequence of services.

Private Rust modules provide useful boundaries without separate manifests or
published interfaces. A crate split adds public types, conversions and release
obligations. A trait adds a contract that every implementation must satisfy.
Neither follows automatically from a database, renderer or HTTP dependency.

Provider earns a substitutable behavior seam because real model adapters and
the deterministic test Provider differ. SQLite can be exercised directly in a
temporary file; terminal projections can be tested as concrete values. Native
credential and Guard helpers earn process isolation through blocking work or
privilege ownership. These are different reasons for different boundaries.

## Alternatives and their costs

| Alternative | Benefit | Cost or prerequisite |
|---|---|---|
| Private in-process modules | Small interfaces, direct calls, simple lifecycle | Shared process failure domain |
| Multiple Rust crates | Independent dependency, compile or release boundaries | Public interfaces and build-graph maintenance; require demonstrated pressure |
| Local process protocol | Different Client languages, independent UI failure, detached work | Versioning, authentication, replay, cancellation and slow-consumer policy |
| C ABI embedding | Same-address-space integration | Allocator ownership, panic containment, handle lifetimes and native packaging |
| Remote service | Multiple hosts or tenants | New identity, authorization, quota, retention and operational threat model |

A second Client or detached execution can justify a local protocol. A C ABI
needs an actual embedding constraint that makes a process unacceptable. The
Rust ABI is not a stable cross-language contract. Provider latency alone does
not justify either boundary; measure the affected local path first.

## When a process protocol becomes necessary

Choose a transport against the actual Client: framed stdio avoids listener and
discovery work for a supervised child; HTTP with an event stream serves browser
clients; platform local sockets can serve multiple local clients. JSON-RPC,
OpenAPI and Protobuf supply different framing or schema tools, but do not supply
Arany's durable acknowledgement, authority or recovery semantics.

At that point, specify operation identity, version negotiation, bounded
requests, cancellation ownership, event cursors and shutdown. A slow subscriber
must recover from durable history or receive an explicit lag error. It cannot
silently lose lifecycle facts. Generated wire objects stay at the transport
edge rather than becoming Engine or storage types.

ACP is a possible Client-facing integration; MCP is a Tool protocol; LSP is a
precedent for editor/server separation. Their roles do not justify introducing
all three. [The Tool contract](../specs/tools.md) owns admitted MCP behavior.

## Evidence retained from other systems

The original investigation used OpenCode's server, generated SDK and embedded
router as examples of several Clients sharing semantics. That demonstrates a
possible expansion path, not a requirement to copy its package tree. Ports and
Adapters similarly supports several ways to drive an application without
requiring a trait for every concrete internal dependency.

Future protocol or ABI work must prove the same successful, cancellation,
rejection and recovery behavior as the in-process entry point. Current testing
requirements live in [testing rules](../../agents/testing.md); proposed work
belongs in [planning](../../planning/README.md).

## Primary sources

- [Parnas: decomposition into modules](https://dl.acm.org/doi/10.1145/361598.361623)
- [Martin: Clean Architecture](https://blog.cleancoder.com/uncle-bob/2012/08/13/the-clean-architecture.html) and [Single Responsibility Principle](https://blog.cleancoder.com/uncle-bob/2014/05/08/SingleReponsibilityPrinciple.html)
- [Ousterhout: modular design](https://web.stanford.edu/~ouster/cgi-bin/cs190-winter18/lecture.php?topic=modularDesign)
- [Cockburn: Hexagonal Architecture](https://alistair.cockburn.us/hexagonal-architecture/)
- Rust: [visibility](https://doc.rust-lang.org/reference/visibility-and-privacy.html), [workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html), [ABIs](https://doc.rust-lang.org/reference/items/external-blocks.html#abi), [linkage](https://doc.rust-lang.org/reference/linkage.html)
- [JSON-RPC](https://www.jsonrpc.org/specification), [OpenAPI](https://spec.openapis.org/oas/), [Protobuf evolution](https://protobuf.dev/programming-guides/proto3/#updating)
- [ACP schema](https://github.com/agentclientprotocol/agent-client-protocol/blob/main/schema/v1/schema.json), [LSP overview](https://microsoft.github.io/language-server-protocol/)
- [OpenCode source at the reviewed commit](https://github.com/anomalyco/opencode/tree/7f964bbb00e505178847e2c08721b0fff56208f9)
