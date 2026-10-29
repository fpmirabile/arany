# Canonical state: storage choice and trade-offs

**Research baseline:** 2026-09-29.

[Execution spec](../specs/execution.md#admission-persistence-and-recovery) owns persistence, admission and format checks;
[architecture](../architecture/system-overview.md) owns the schema. This report
explains the storage choice and the evidence that could justify changing it.

## Choose the smallest durability owner

The workload needs local transactional transitions, ordered replay and one
logical writer, without a service. Correctness owned by mature storage code
matters more than raw append throughput.

| Option | Benefit | Application or operational cost |
|---|---|---|
| In-memory state | Simple pure reducer tests | No recovery after exit |
| JSONL append file | Readable export | Framing, torn-tail recovery, locking, sync, transactions and indexes become harness code |
| Directory per Session | Convenient browsing | Cross-file atomicity and summary consistency become harness code |
| SQLite rollback journal | Transactions, recovery, queries and inspection in one embedded library | Native dependency and filesystem assumptions |
| SQLite WAL | Reader/writer overlap | Persistent sidecars, checkpoint policy and backup coordination; still one writer |
| Rust KV stores | Avoid SQLite's native implementation | Application indexes, query/inspection tools and equivalent recovery evidence |
| RocksDB | High sustained write throughput | Native C++ build, compaction and operational tuning |
| PostgreSQL | Multi-client/server operations | Service lifecycle, authentication, upgrades and backup ownership |

SQLite keeps semantic transactions and inspection local. A single canonical
Events table does not forbid derived evidence caches or indexes; those cannot
become another authority for Session state. The concrete schema belongs in its
owner rather than a SQL sketch that ages separately.

## Why rollback journaling fits

The active CLI observes acknowledged Engine facts rather than polling through a
second database connection. The writer is explicitly owned. WAL's reader/writer
overlap therefore does not automatically improve this path, while checkpoints
and sidecar-aware recovery add work.

`DELETE + EXTRA` preserves the selected acknowledgement policy. SQLite's
`EXTRA` rollback mode includes directory synchronization after journal removal.
Durability still depends on the filesystem and hardware honoring synchronization;
a successful unit test cannot establish power-loss behavior everywhere.

A future WAL proposal must compare equal durability, include checkpoint stalls
and storage growth, and review the actual bundled SQLite version. The original
investigation identified [SQLite's WAL-reset concurrency defect](https://www.sqlite.org/wal.html#the_wal_reset_bug)
as a reason to review native engine fixes rather than only the Rust wrapper.
Do not weaken synchronization merely to improve a benchmark.

## Recovery and confidentiality limits

[Execution](../specs/execution.md#incompatible-history) owns upgrades, archival and
strict replay. SQLite structural validity and legal semantic history are separate
checks. Private OS storage is not encryption; adding encryption needs key and
recovery ownership. A copy of an open database is not automatically a consistent
backup, and a valid snapshot alone does not prove it includes the final
acknowledged commit at cutover.

## Reconsider only for an observed constraint

| Evidence | Candidate change |
|---|---|
| A real concurrent reader or durable commit bottleneck | Benchmark WAL with equal sync guarantees and bounded checkpoint ownership |
| Replay is slow while append remains healthy | Rebuildable projections or snapshots |
| Large retained payloads dominate state | Separately scoped content storage; [context research](./context-memory-and-compaction.md) covers retention implications |
| C/native storage is prohibited | Compare a Rust KV store with the same crash and replay corpus |
| Multiple hosts need canonical writes | A service/database boundary with a new operational threat model |
| At-rest confidentiality exceeds OS protection | A key-management and recovery design before encryption |

Database size alone does not justify a new engine. Validate the access pattern,
full durability policy and cost of application-owned indexes. Current unresolved
storage work belongs in [next steps](../../NEXT_STEPS.md); verification procedures
belong in [testing rules](../../agents/testing.md).

## Primary sources

- SQLite: [atomic commit](https://www.sqlite.org/atomiccommit.html), [transactions](https://www.sqlite.org/lang_transaction.html), [synchronization](https://www.sqlite.org/pragma.html#pragma_synchronous), [WAL](https://www.sqlite.org/wal.html)
- SQLite: [appropriate uses](https://www.sqlite.org/whentouse.html), [corruption causes](https://www.sqlite.org/howtocorrupt.html), [backup API](https://www.sqlite.org/backup.html), [defensive guidance](https://www.sqlite.org/security.html)
- [Rusqlite](https://github.com/rusqlite/rusqlite), [Rust file synchronization](https://doc.rust-lang.org/std/fs/struct.File.html#method.sync_all)
- [Redb durability](https://docs.rs/redb/latest/redb/enum.Durability.html), [Fjall](https://github.com/fjall-rs/fjall), [RocksDB overview](https://github.com/facebook/rocksdb/wiki/RocksDB-Overview)
- [PostgreSQL architecture](https://www.postgresql.org/docs/current/tutorial-arch.html)
