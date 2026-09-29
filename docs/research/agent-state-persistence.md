# Agent-state persistence for the minimum harness

> **Session amendment — 2026-09-29:** The selected SQLite rollback-journal architecture remains. The canonical Events table now requires `session_id` and optional Run/AgentRun scopes so it can replay durable multi-Run Sessions, fork/default/compaction facts, and ordered bounded teams. SessionView/RunView remain reductions; no duplicate canonical entity tables are added. The [system overview](../architecture/system-overview.md) owns the current schema.

**Status:** research and implementation recommendation  
**Research date:** 2026-09-29  
**Scope:** one local Rust CLI process, one authoritative writer, durable Sessions, one primary plus an ordered budget-bounded `0..N` collection of direct children per Run, append/replay Events, no daemon, no effectful Tools
**Question:** Is SQLite the smallest honest persistence mechanism for this harness, and what exact durability contract should the demonstrator implement?

> **Security amendment — 2026-09-29:** [Harness security lessons](./harness-security-lessons-and-controls.md) and D-14 in the [decision register](./next-step-decision-register.md) make no-follow open, defensive mode, owner/ACL validation, outside-Workspace local storage, data-only replay, a 256 MiB page cap, and 4 MiB Run-admission headroom release requirements rather than optional hardening.

## Reading guide

This report uses four labels:

- **Fact:** directly supported by a primary source.
- **Inference:** a conclusion from those facts and this repository's workload.
- **Recommendation:** the design to implement here.
- **Gate:** evidence that must exist before the recommendation changes.

“Application Event” means a domain fact such as `AgentSpawned`. SQLite's rollback journal or WAL is database recovery machinery, not the Harness Event journal.

## Executive answer

**Recommendation: keep SQLite, but do not enable SQLite WAL for the minimum demonstrator.** Use one bundled SQLite database, one connection, the existing append-only `events` table, rollback journal mode `DELETE`, and `synchronous=EXTRA`. Commit each logical state transition before exposing it as durable feedback. `Run` and `AgentRun` remain replayed state; do not add canonical tables for them.

SQLite is not selected because SQL is fashionable or because the first schema is relationally complex. It is selected because this tiny workload still needs a surprisingly difficult combination:

1. an acknowledged transition must be all-or-nothing across process or machine failure;
2. replay must observe one ordered committed prefix;
3. cancellation and multi-Event transitions must be committed atomically;
4. reopening after an unclean exit must not require Harness-authored repair logic;
5. schema changes, inspection, export, and corruption diagnosis need ordinary tools; and
6. a later read-only process should not force a storage rewrite.

SQLite already owns those mechanisms and crash-tests them. Its official test suite injects I/O errors, reorders unsynchronized writes, simulates crashes and power loss, and verifies that transactions are wholly present or absent and the database remains structurally valid ([SQLite testing](https://www.sqlite.org/testing.html)). A JSONL or directory format can meet the same contract only after the Harness implements record framing, partial-write detection, checksums, locking, `fsync`, directory synchronization, recovery, compaction, indexing, migrations, and crash tests. That is a storage engine hidden inside “simple files.”

The choice is workload-specific:

- **In-memory only** is the first test implementation, not persistence; it cannot satisfy `arany show <run-id>` after exit.
- **JSONL** is the best export format, not the authoritative store.
- **Directory-per-run files** are useful later for large Artifacts, not atomic lifecycle state.
- **redb** is the strongest pure-Rust alternative, but the Harness would own key encoding, secondary indexes, value-version migrations, query tooling, and backup/export behavior. Stable redb is also exclusive across processes by default; its multiprocess modes are currently behind an experimental feature ([redb API](https://docs.rs/redb/latest/redb/), [redb feature manifest](https://docs.rs/crate/redb/latest/source/Cargo.toml.orig)).
- **Fjall** and **RocksDB** solve high-throughput LSM workloads the demonstrator does not have and add background maintenance and more application-owned structure. RocksDB also adds C++/FFI and a much larger build surface.
- **PostgreSQL** is the correct later choice for multiple authoritative hosts/writers or operational requirements such as replication and centralized backup, but it would turn a zero-configuration CLI into a client/server deployment now.

The narrower correction to the previous research is important: **SQLite remains the answer; SQLite WAL is deferred.** WAL permits readers and a writer to proceed concurrently but still allows only one writer, requires same-host shared memory, and makes the `-wal` file part of persistent state ([SQLite WAL](https://www.sqlite.org/wal.html)). The demonstrator renders live state from its Engine and reopens the database only after exit, so it has no concurrent database reader to benefit from WAL. Rollback mode produces fewer operational states to explain and a closed database is one copyable file.

## 1. The persistence contract

### 1.1 Required behavior

The minimum architecture requires:

- one process to append Session and Run state transitions;
- one primary plus at most the admitted bounded number of active child `AgentRun`s, all scheduled by the same Engine;
- a global order sufficient to replay one durable Session or an individual Run;
- reconstruction after normal exit, `Ctrl-C`, panic, or forced process death;
- the same facts in human terminal output and JSONL;
- no background service and no external database installation; and
- no effectful Tool recovery, distributed lease, or cross-host coordination.

The expected write rate is tiny. Even a verbose demonstrator should emit tens or hundreds of semantic Events per Run, not one durable row per generated token. Provider streaming deltas may be transient and coalesced; lifecycle changes become durable Events.

### 1.2 Acknowledgement rule

**Recommendation:** an Event is durable only after its SQLite transaction commits successfully. The Engine then reduces and publishes that committed Event to `RunView` and JSONL.

This ordering prevents a terminal from showing “finished” when replay after a crash would show “running.” It also means database failure may delay or reject a state transition, but it cannot create two competing truths.

Transient provider text may be shown before it is an Application Event only if the renderer labels it as transient and does not use it to change `RunView` state. The current proof can avoid that distinction by rendering only committed semantic Events.

### 1.3 Consistency unit

The unit of durability is a **logical Engine transition**, not necessarily one row:

- `RunStarted`: one row and one commit;
- delegating an admitted team: the primary state update and all child-spawn Events in one transaction;
- a worker completion: its final state and summary in one Event, one transaction;
- primary completion: primary `AgentFinished` and `RunFinished` in one transaction; and
- graceful cancellation: all terminal cancellation Events in one transaction.

SQLite states that changes inside one transaction are atomic, including across application, OS, and power failure subject to the filesystem and hardware honoring its sync and locking contracts ([SQLite transactional guarantee](https://www.sqlite.org/transactional.html), [atomic commit assumptions](https://www.sqlite.org/atomiccommit.html)).

### 1.4 Explicit non-guarantees

The demonstrator does **not** guarantee:

- automatic continuation of a Run after an unclean exit;
- exactly-once provider billing or generation;
- recovery of a provider response received but not yet committed locally;
- survival of malicious database-file modification, disk destruction, or a storage device that lies about flush completion;
- safe operation from NFS, SMB, cloud-synced folders, or another network filesystem;
- simultaneous authoritative writers; or
- encrypted state at rest.

Those exclusions are not SQLite defects. Every local option relies on OS and device persistence behavior. POSIX specifies that `fsync` requests synchronized completion but leaves some physical-storage details implementation-defined; on Linux a file `fsync` also does not make its directory entry durable, which requires syncing the directory separately ([POSIX `fsync`](https://pubs.opengroup.org/onlinepubs/9699919799/functions/fsync.html), [Linux `fsync(2)`](https://man7.org/linux/man-pages/man2/fsync.2.html)). SQLite implements and tests the required ordering, but it cannot repair broken locking, a rogue writer, or dishonest hardware ([how SQLite can be corrupted](https://www.sqlite.org/howtocorrupt.html)).

## 2. Decision criteria

The options are judged in this order:

1. **Correctness owned by mature code:** atomic commit, ordered replay, and crash recovery.
2. **Small operational surface:** no service, account, daemon, administrator, or additional process.
3. **Small Harness surface:** no custom log recovery, index, migration, or locking subsystem.
4. **Debuggability:** a developer can inspect a failed Run without writing a bespoke decoder.
5. **Evolution:** Event envelopes and indexes can change without rewriting the orchestration model.
6. **Portability:** Linux, macOS, and Windows builds and files behave predictably.
7. **Latency:** persistence does not create user-visible delay relative to provider generation.
8. **Future fit:** a read-only observer, snapshots, FTS, or export can be added without replacing canonical history.

Raw write throughput is intentionally lower in the ordering. The demonstrator is not a telemetry firehose, cache, vector database, or multi-tenant service.

## 3. Comparison at a glance

| Option | Crash-safe transaction | Replay/query | Concurrency | Build/operations | Application-owned correctness | Decision |
|---|---|---|---|---|---|---|
| In-memory only | No persistence | Excellent until exit | Process-local | Minimal | Low | Use for the first fake test only |
| JSONL append file | Only after custom framing, sync, and recovery | Sequential scan; grep-friendly | Custom locking | Small dependency surface | Very high | Export/debug format, not truth |
| Directory per Run | Only per file; cross-file atomicity is custom | Easy single-Run browsing; poor global queries | Custom locking | Many files and migration paths | Very high | Later Artifacts, not lifecycle state |
| SQLite rollback journal | Yes, built-in | SQL, indexes, standard CLI | One writer; readers coordinated | Embedded C library, no service | Low | **Selected** |
| SQLite WAL | Yes with the right sync policy | Same SQL | Concurrent readers + one writer | Extra persistent sidecars/checkpoints | Low-medium | Triggered optimization |
| redb | ACID and crash-safe by default | Ordered KV ranges; custom indexes/tools | Concurrent readers + one writer in process; stable default is cross-process exclusive | Pure Rust, embedded | Medium | Best fallback if native C is forbidden |
| Fjall | Requires explicit persistence policy; optional transactions | Ordered KV ranges | Thread-safe; no parallel process opens | Pure safe Rust; LSM maintenance | Medium-high | Not justified by this workload |
| sled | Beta and pre-1.0 format migration | Ordered KV ranges | In-process concurrency | Pure Rust | High | Reject; its own README recommends SQLite for reliability |
| RocksDB | WAL/atomic batch with explicit sync policy | Ordered KV; custom indexes/tools | One process opens a DB; rich thread concurrency | C++/FFI, compaction, directory of files | Medium-high | Throughput solution without a throughput problem |
| PostgreSQL | Mature server WAL/transactions | Full SQL and operations ecosystem | Many clients/writers | Server lifecycle, auth, upgrades, backup | Low in application; high operationally | Later distributed/operational boundary |

## 4. Option analysis

### 4.1 In-memory only

An in-memory `Vec<Event>` is the smallest way to prove the reducer, loop, join rule, and cancellation propagation. It is deterministic and removes storage failures from early tests.

It fails the product proof because process exit destroys every Event. Re-running a provider is not reconstruction and can change output, cost money, or fail. In-memory state therefore remains a test fixture behind private Engine construction, not a `Store` adapter or production mode.

**Recommendation:** make `team_run` pass in memory first as the architecture already requires, then replace the private append/replay implementation with SQLite. Do not offer `--memory` as a public durability mode.

### 4.2 JSONL append-only file

JSONL looks perfectly matched to Events: one readable record per line and append in order. It also makes export, fixtures, diffs, and piping easy.

The gap is between `write()` and a transactional append log:

- a write may be short or interrupted;
- a process can die halfway through a JSON value or UTF-8 sequence;
- buffered writer flush is not durable storage synchronization;
- `sync_all`/`fsync` errors must change acknowledgement behavior;
- the last complete newline does not prove the prior record reached stable storage;
- several records forming one logical transition are not atomic;
- concurrent append and compaction need a lock protocol;
- repair must distinguish a torn tail from corruption in the middle;
- records need length limits, versioning, and possibly checksums;
- listing or filtering Runs requires a full scan or a second index whose consistency is now another transaction problem; and
- replacing/compacting a file durably requires syncing the new file, atomically renaming it, and syncing the directory.

Rust's `File::sync_all` asks the OS to synchronize content and metadata, while `sync_data` may omit metadata not needed to recover content ([Rust `File`](https://doc.rust-lang.org/std/fs/struct.File.html#method.sync_all)). POSIX rename gives atomic namespace replacement, but directory operations are not automatically durable; the POSIX rationale describes the temporary-file, file-sync, rename, directory-sync sequence ([POSIX rename](https://pubs.opengroup.org/onlinepubs/9799919799/functions/rename.html), [POSIX durability rationale](https://pubs.opengroup.org/onlinepubs/9799919799/xrat/V4_xbd_chap01.html)). Windows exposes different primitives such as `FlushFileBuffers`, whose documentation also calls out caching and performance implications ([Microsoft `FlushFileBuffers`](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers)).

Implementing all of that well would be more code than the Harness Event model. SQLite's recovery journal exists precisely to own it.

**Recommendation:** provide JSONL as a deterministic logical export produced by replay, and accept JSONL fixtures in tests. Do not make it canonical state.

### 4.3 Directory-per-run files

A layout such as this is attractive:

```text
runs/<run-id>/
├── run.json
├── events.jsonl
├── agents/<agent-id>.json
└── result.md
```

It offers human discoverability, easy whole-Run copying, and natural isolation of damage. It also creates four representations that can disagree. A crash between updating `events.jsonl` and `run.json` leaves ambiguous truth. Atomic rename solves replacement of one file, not a transaction across the directory. Global `harness list`, retention, schema upgrades, and collision-safe ID creation require walking and coordinating many paths. Antivirus, cloud sync, and user editors add interference.

One file per Event avoids partial shared-file appends but creates inode and directory-scaling costs, still needs durable filename publication, and makes ordering depend on carefully encoded names rather than the filesystem's directory enumeration.

**Recommendation:** use directories only after large immutable Artifacts exist. The database stores Artifact metadata and content hashes; the filesystem stores bounded content objects whose write/publish protocol can be tested independently. Do not split lifecycle truth by Run now.

### 4.4 SQLite

SQLite is an embedded library, not another service. It provides transactions, constraints, indexes, locking, recovery, integrity checks, online backup, and a stable cross-platform file format. SQLite documents its database format as backwards compatible since 3.0.0 and portable across architectures ([file-format compatibility](https://www.sqlite.org/formatchng.html), [SQLite as an application format](https://www.sqlite.org/appfileformat.html)).

Its concurrency model matches the current workload: it serializes writers, permits several connections, and supplies serializable isolation by allowing only one writer at a time ([SQLite isolation](https://www.sqlite.org/isolation.html)). The Harness already has exactly one authoritative writer.

Its largest cost is a native C/FFI dependency. The Rust wrapper `rusqlite` uses `libsqlite3-sys`; its `bundled` feature compiles and statically links a known SQLite source version and avoids depending on an old or absent system library ([rusqlite repository](https://github.com/rusqlite/rusqlite)). This repository's security rules require review of that native boundary, but writing a storage engine in safe Rust is not automatically lower risk than consuming SQLite's extensively fault-injected implementation.

SQLite is also inspectable. A developer can query Event kinds, ordering, payloads, and Run membership with the standard CLI. The online Backup API creates a consistent snapshot even while the source changes; `VACUUM INTO` is another consistent-copy mechanism ([SQLite backup API](https://www.sqlite.org/backup.html), [`VACUUM INTO`](https://www.sqlite.org/lang_vacuum.html)).

**Recommendation:** use `rusqlite` with its bundled SQLite feature for reproducible CLI releases. Keep all SQL and connection setup private in `store.rs`; do not create a generic repository trait.

### 4.5 redb

redb is a serious alternative, not a toy. Its current documentation describes a pure-Rust, copy-on-write B-tree store with ACID transactions, MVCC, concurrent readers and one writer, crash safety, savepoints, integrity checking, and immediate durability as the default ([redb crate](https://docs.rs/redb/latest/redb/), [redb durability](https://docs.rs/redb/latest/redb/enum.Durability.html), [redb database API](https://docs.rs/redb/latest/redb/struct.Database.html)). Its recent changelog and releases show active maintenance ([redb changelog](https://docs.rs/crate/redb/latest/source/CHANGELOG.md), [redb releases](https://github.com/cberner/redb/releases)).

For an event log it could use a composite big-endian key such as `(run_id, sequence)` and a versioned serialized Event value. That would append and range-scan efficiently.

The costs move into Harness code:

- encode keys so lexical order equals replay order;
- version and migrate serialized values;
- maintain any Run, status, timestamp, or Agent secondary index transactionally;
- create inspection/export tooling because generic SQL tools cannot query domain fields;
- specify online-copy/backup behavior;
- expose corruption diagnostics in Harness commands; and
- revisit multiprocess access if a live observer arrives.

Stable redb opens a database for an exclusive writer by default. The current source exposes single-writer and multi-writer process sharing only through `experimental-multiprocess`, explicitly allowed to change or disappear ([redb concurrency source](https://docs.rs/redb/latest/src/redb/db.rs.html), [redb feature declaration](https://docs.rs/crate/redb/latest/source/Cargo.toml.orig)). This does not hurt today's one process, but it is weaker than SQLite for the likely next local-reader step.

redb can require a full repair walk after an unclean close unless quick repair is enabled; quick repair trades slower commits for fast recovery by persisting allocator state and using two-phase commit ([redb transaction source](https://docs.rs/redb/latest/src/redb/transactions.rs.html)). That is a valid design, but it introduces another policy to benchmark and own.

**Recommendation:** keep redb as the fallback only if a concrete distribution target forbids C/FFI or SQLite compilation. It is not simpler at the product level merely because it is pure Rust.

### 4.6 Fjall and sled

Fjall is an actively developed safe-Rust LSM-tree. It supports ranges, multiple keyspaces, optional serializable transactional databases, compression, and background maintenance. Its default writes reach OS buffers; callers explicitly request a durable journal sync, and one database cannot be loaded by separate processes ([Fjall repository](https://github.com/fjall-rs/fjall)).

Those are useful controls for write-heavy storage, but the Harness would need to select the transactional API and persistence mode correctly, manage compaction behavior, encode indexes, and build inspection/migration tooling. Compression and LSM compaction do not solve a current bottleneck.

sled is even easier to reject for canonical history. Its own README calls the database beta, warns that the README is out of sync with an in-progress rewrite, says the on-disk format can require manual migrations before 1.0, and explicitly recommends SQLite when reliability is the primary constraint ([sled repository](https://github.com/spacejam/sled)).

**Recommendation:** do not benchmark Fjall or sled for the demonstrator. Reconsider a Rust LSM only if a measured, sustained write workload makes B-tree persistence the limiting factor and the workload can accept the maintenance model.

### 4.7 RocksDB

RocksDB is a C++ LSM engine designed for high write throughput, batches, column families, compression, and background compaction. A `WriteBatch` atomically updates multiple keys, while the transaction variants add conflict handling ([RocksDB transactions](https://github.com/facebook/rocksdb/wiki/Transactions)).

Durability is an explicit configuration responsibility: the default `WriteOptions.sync=false` does not synchronize the WAL to disk; `sync=true` fsyncs before returning ([RocksDB WAL performance](https://github.com/facebook/rocksdb/wiki/WAL-Performance)). RocksDB uses a directory containing WAL, manifest, and SST files, and its checkpoint API builds a consistent directory snapshot ([RocksDB checkpoints](https://github.com/facebook/rocksdb/wiki/Checkpoints)). Only one process may open a database, although threads within it can operate concurrently ([RocksDB basic operations](https://github.com/facebook/rocksdb/wiki/Basic-Operations/8b0db11192422ae154253ae6e76123f28b09488a)).

For Rust this also means a community wrapper over the C API and a `librocksdb-sys` C++ build. It adds FFI invariants, toolchain and cross-compilation burden, a large binary/build cache, tuning, background threads, and compaction tail latency. It still lacks SQL-level ad hoc queries and application indexes.

**Recommendation:** RocksDB is over-engineering for an event rate that fits in a handful of SQLite transactions per provider turn. Do not add it without benchmark evidence that SQLite's single-writer queue, not Event serialization or provider latency, is the limiting resource.

### 4.8 PostgreSQL

PostgreSQL supplies server-managed transactions, WAL recovery, MVCC, many concurrent clients, mature logical/physical backup, replication, and point-in-time recovery. Its WAL rule flushes log records before the corresponding data pages, allowing crash recovery by replay ([PostgreSQL WAL](https://www.postgresql.org/docs/current/wal-intro.html)). Default synchronous commit waits for local WAL flush; disabling it can lose recently acknowledged transactions even though the database remains consistent ([PostgreSQL WAL configuration](https://www.postgresql.org/docs/current/runtime-config-wal.html)).

Those guarantees come with a server process, connection protocol, authentication, data directory, service lifecycle, upgrades, and operational backup policy. PostgreSQL explicitly uses a client/server architecture and creates backend processes for clients ([PostgreSQL architecture](https://www.postgresql.org/docs/current/tutorial-arch.html)). Its backup system offers SQL dumps, filesystem backups, and continuous WAL archiving, each with operational choices ([PostgreSQL backup](https://www.postgresql.org/docs/current/backup.html)).

For a single-user executable, requiring PostgreSQL would make installation and failure handling dominate the feature being demonstrated. An embedded PostgreSQL launched by the CLI would merely hide, not remove, those responsibilities.

**Recommendation:** migrate when the product, not speculation, creates a service boundary: multiple hosts need authoritative writes, remote tenants require access control, high write concurrency violates the SQLite SLO, or replication/PITR/centralized operations become product requirements.

## 5. Why rollback journal, not WAL

### 5.1 What WAL buys

In WAL mode, writers append frames and readers retain an end mark, so readers and the writer can operate concurrently. There is still only one writer. A checkpoint later transfers committed pages into the main file. The WAL index uses shared memory, so all readers must be on the same host and WAL is unsuitable for a network filesystem ([SQLite WAL](https://www.sqlite.org/wal.html)).

WAL can reduce write latency because commits append sequentially. With `synchronous=FULL`, the WAL is synchronized on every commit. With `NORMAL`, most transactions do not issue a sync and may be lost after power failure even though database consistency is preserved ([SQLite synchronous pragma](https://www.sqlite.org/pragma.html#pragma_synchronous)).

### 5.2 What WAL costs

- The `database-wal` file is part of committed persistent state. Copying the main file without it can lose committed transactions or corrupt the copy.
- Checkpoint timing becomes another latency and disk-growth concern.
- A long reader can prevent checkpoint completion and allow WAL growth.
- Backup and move procedures must be checkpoint-aware or use the Backup API.
- It solves reader/writer overlap, not multiple-writer throughput.

Version choice is also part of a future WAL decision. SQLite documents a rare WAL-reset corruption race that affected releases through 3.51.2 when two or more connections wrote or checkpointed concurrently; it is fixed in 3.51.3 and later, with selected backports ([SQLite WAL-reset bug](https://www.sqlite.org/wal.html#the_wal_reset_bug)). Rollback-journal V1 does not exercise that path. Any later WAL benchmark or release must verify the bundled SQLite source contains the fix rather than relying only on the Rust wrapper version.

### 5.3 Why the demonstrator gets no benefit

The active CLI receives Events from the Engine, not by polling SQLite. `arany show` is required to reconstruct after process exit, not concurrently with an active Run. One connection performs all writes. Therefore rollback journal reader blocking is irrelevant.

**Recommendation:** explicitly set and verify:

```sql
PRAGMA journal_mode = DELETE;
PRAGMA synchronous = EXTRA;
PRAGMA trusted_schema = OFF;
PRAGMA busy_timeout = 250;
PRAGMA page_size = 4096;
PRAGMA max_page_count = 65536;
```

`DELETE` keeps the default rollback-journal mechanism explicit. `EXTRA` includes the `FULL` syncs and additionally syncs the containing directory after unlinking a rollback journal, which SQLite recommends when power-loss durability is desired in rollback mode ([SQLite synchronous pragma](https://www.sqlite.org/pragma.html#pragma_synchronous), [SQLite 3.11.1 release note](https://sqlite.org/releaselog/3_11_1.html)). `trusted_schema=OFF` follows SQLite's recommendation to prevent a hostile schema from invoking unsafe application functions or virtual tables ([SQLite defensive guidance](https://www.sqlite.org/security.html), [trusted schema pragma](https://www.sqlite.org/pragma.html#pragma_trusted_schema)).

Open read-write/create with `SQLITE_OPEN_NOFOLLOW`, enable `SQLITE_DBCONFIG_DEFENSIVE`, and check the returned `journal_mode`; do not assume requests succeeded. Apply and verify `synchronous`, `trusted_schema`, page size, page cap, and the busy timeout before preparing application statements. Migrations run before normal operations and set `user_version` in the same migration transaction where possible; connection setup must never overwrite a newer version. SQLite reports the synchronous modes numerically through the pragma API, so the configuration test accepts `EXTRA` only when the returned value is `3`.

Do not set `synchronous=OFF` or `NORMAL` for canonical Events. Do not expose a durability tuning flag in V1. Tests may use an in-memory database; benchmarks must use the production policy.

### 5.4 WAL activation gate

Benchmark `WAL + FULL` only when one of these becomes true:

- a second live local process must read while the Run writes;
- rollback-mode commit latency breaches the durable-append budget;
- renderer or evaluation work genuinely reads through a separate SQLite connection during writes; or
- a future daemon has concurrent read clients.

If activated, keep one writer, use local storage only, retain `synchronous=FULL`, verify a SQLite release containing the WAL-reset fix, leave the default automatic checkpoint initially, measure checkpoint stalls and WAL bytes, and use the Backup API. Do not use `NORMAL` merely to win a benchmark by weakening the acknowledgement contract.

## 6. Minimum schema

### 6.1 Recommended schema

```sql
CREATE TABLE events (
    sequence       INTEGER PRIMARY KEY,
    session_id     TEXT NOT NULL,
    run_id         TEXT,
    agent_run_id   TEXT,
    kind           TEXT NOT NULL,
    event_version  INTEGER NOT NULL CHECK (event_version >= 1),
    payload        TEXT NOT NULL CHECK (json_valid(payload)),
    created_at_ms  INTEGER NOT NULL
) STRICT;

CREATE INDEX events_by_session
    ON events (session_id, sequence);

CREATE INDEX events_by_run
    ON events (run_id, sequence)
    WHERE run_id IS NOT NULL;

PRAGMA user_version = 1;
```

Changes from the earlier sketch are deliberate:

- remove `AUTOINCREMENT`;
- add an explicit `event_version`;
- validate canonical JSON text;
- make the table `STRICT`; and
- retain exactly one canonical application table with rebuildable Session and Run indexes.

SQLite says `AUTOINCREMENT` adds CPU, memory, disk-space, and disk-I/O overhead and is usually unnecessary. A plain `INTEGER PRIMARY KEY` normally assigns one more than the largest row ID. Reuse is possible only after deleting the largest row or exhausting the signed 64-bit range ([SQLite autoincrement](https://www.sqlite.org/autoinc.html)). V1 never deletes canonical Events, so non-reuse does not justify the feature.

`STRICT` makes SQLite reject values that cannot be losslessly converted to the declared type ([SQLite STRICT tables](https://sqlite.org/stricttables.html)). `json_valid` rejects malformed RFC 8259 JSON text, and JSON functions are built in by default in modern SQLite ([SQLite JSON](https://www.sqlite.org/json1.html)). The Engine still validates and bounds the typed payload before serialization; the database check is defense in depth, not the primary parser boundary.

### 6.2 Meaning of each column

- `sequence`: authoritative global commit order and replay cursor inside this database.
- `run_id`: selection key for `arany show`; created by the Engine before `RunStarted`.
- `agent_run_id`: nullable because Run-level Events do not have an Agent cause.
- `kind`: stable semantic discriminator, never a Rust type name produced implicitly.
- `event_version`: decoder version for this kind's payload.
- `payload`: bounded canonical JSON generated from a typed Event payload.
- `created_at_ms`: wall-clock display metadata; never used for ordering or causality.

The Engine must order by `sequence`, never timestamp. Wall clocks can move backward, have low resolution, or be corrected.

### 6.3 Payload bounds

SQLite's compiled limits are intentionally generous and can be lowered per connection, but application limits should reject excessive allocation before SQL execution ([SQLite limits](https://www.sqlite.org/limits.html)).

**Recommendation:** the Event encoder accepts at most 64 KiB of serialized JSON per Event in V1. Provider text larger than that is rejected as a bounded result failure; later it activates the Artifact trigger rather than increasing the row indefinitely. Objectives and summaries have narrower typed limits chosen by their own input research.

Also cap:

- kind at 64 ASCII bytes;
- identifier strings at 128 bytes;
- one transition at 8 Events; and
- one append transaction at 256 KiB serialized payload.

The exact constants should live in code and tests, not configuration. They are generous relative to the proof and make overflow behavior deterministic.

### 6.4 Why no `runs` table

`Run` is already the reduction of Events. A canonical `runs` row would duplicate objective, status, result, and timestamps. Every append would then need to keep the journal and row synchronized. The database transaction could do that safely, but the reducer and projection would still need separate migration and invariant tests.

The proof requires `show <run-id>`, not an optimized Run list. The `(run_id, sequence)` index makes replay direct. Querying distinct Run IDs is adequate at demonstrator scale.

**Trigger:** add a rebuildable `runs` projection only when a measured `harness list` query or startup budget fails. It is derived cache, never a second source of truth.

### 6.5 Why no `agent_runs` table

The same reasoning applies. `AgentSpawned`, `AgentUpdated`, and `AgentFinished` already contain the necessary lifecycle. The reducer enforces parentage, root-only delegation, join-all completion, and terminal-state rules. A table would duplicate those facts without serving a current query.

**Trigger:** add a derived Agent projection only when cross-Run analytics or a real query cannot be served within the replay budget.

### 6.6 Why no metadata table

`PRAGMA user_version` supplies the single database schema version. Per-Event `event_version` supplies payload evolution. That is enough for one schema migration chain and five Event kinds.

Do not add `schema_migrations`, settings, locks, leases, counters, snapshots, or outbox tables. Add a migration table only if branching or independently shipped migrations become real. Use `application_id` only after obtaining or deliberately managing a collision-safe identifier; `user_version` is sufficient for V1 ([SQLite file header fields](https://www.sqlite.org/fileformat.html)).

## 7. Append and replay semantics

### 7.1 Append algorithm

For each logical transition:

1. Hold the Engine's in-process transition lock.
2. Load or retain the current reduced state.
3. Validate the proposed transition against domain rules.
4. Serialize bounded typed Event payloads.
5. Begin an explicit SQLite write transaction.
6. Insert every Event in semantic order.
7. Commit.
8. Read assigned sequences or use `RETURNING` as supported by the bundled version.
9. Apply the committed Events to the in-memory reducer.
10. Publish them to terminal/JSONL observers.

With one connection and one writer, a deferred transaction whose first statement is an insert is sufficient. `BEGIN IMMEDIATE` is also acceptable and makes lock acquisition happen at the start; SQLite documents that it starts a write transaction immediately and may return `SQLITE_BUSY` before application changes occur ([SQLite transactions](https://www.sqlite.org/lang_transaction.html)).

**Recommendation:** use `BEGIN IMMEDIATE` so an unexpected second process fails or waits before work is performed. Configure a short, fixed busy timeout of 250 ms. On timeout, report “state database is already in use”; do not wait indefinitely and do not retry a whole transition automatically.

### 7.2 Replay algorithm

1. Open the database read-only for `show` when no migration is required.
2. Verify supported `user_version`.
3. Select the Run's rows ordered by `sequence ASC`.
4. Reject unknown Event kinds or unsupported versions.
5. Deserialize each bounded payload into a typed Event.
6. Apply the reducer, validating every state transition.
7. Require exactly one `RunStarted` and at most one `RunFinished`.
8. Return a `RunView` or a precise corruption/unsupported-version error.

Do not skip unknown Events. Silent skipping could turn a completed, denied, or cancelled action into a different history.

### 7.3 Incomplete prefixes

After `SIGKILL`, panic abort, OS crash, or power loss, the committed history may end with active Agents and no `RunFinished`. That is a valid committed prefix, not database corruption.

**Recommendation:** replay derives `Interrupted` as a presentation state when it sees a nonterminal Run and no live owning process. It does not append a fictional cancellation Event during read-only `show`. V1 does not resume or call the Provider automatically.

A graceful `Ctrl-C` attempts one bounded cancellation transaction. If it commits, replay shows `Cancelled`. If the process is killed before commit, replay shows `Interrupted`. This distinction is truthful.

### 7.4 External provider uncertainty

No local database can atomically commit with a remote model API. These sequences are possible:

1. provider completes, process dies, completion Event is absent;
2. request is sent, connection fails, provider outcome or billing is unknown; or
3. completion Event commits, terminal rendering fails.

V1 should not resume incomplete Runs, so it never hides a duplicate provider call behind “recovery.” The user can start a new Run explicitly. If future automatic resume is added, provider request IDs, response IDs, idempotency support, and an `Uncertain` state become required before replay can dispatch anything.

## 8. Failure model and response

| Failure | Committed truth | Required behavior |
|---|---|---|
| Validation/serialization fails | No new Event | Reject transition; keep prior state |
| Process exits before transaction | No new Event | Replay prior prefix |
| Process dies during commit | Whole transaction present or absent | SQLite recovery; replay one prefix |
| Commit succeeds, process dies before render | Event present | `show` reveals committed transition |
| Disk full | Commit fails with `SQLITE_FULL` | Publish no durable state; report storage failure |
| I/O error/read-only filesystem | Commit fails | Stop Run; preserve database; no blind retry |
| Another writer holds lock | `SQLITE_BUSY` | Bound wait, then explicit “database in use” error |
| Corrupt/not-a-database file | Open/replay fails | Fail closed; preserve file; offer diagnosis/export path later |
| Unsupported schema/Event version | Data may be valid but unreadable | Refuse downgrade; require compatible binary/migration |
| Graceful Ctrl-C | Cancellation transaction when possible | Cancel all Agents, commit terminals, then exit |
| Forced kill | Last complete prefix only | Derive `Interrupted`; never pretend `Cancelled` |
| User copies open DB incorrectly | Copy may be inconsistent | Unsupported; use closed copy or Backup API |
| Network filesystem lock failure | Possible corruption | Unsupported storage location |

SQLite exposes stable result classes such as `BUSY`, `CORRUPT`, `FULL`, `IOERR`, `READONLY`, and `NOTADB`, with extended codes for diagnosis ([SQLite result codes](https://sqlite.org/rescode.html)). Preserve their safe classification in errors, but do not print paths, payloads, objectives, prompts, or secrets by default.

### 8.1 Corruption policy

- Never mutate the only copy in an automatic “repair.”
- On `CORRUPT`/`NOTADB`, close the connection and report the database path plus a non-sensitive error class.
- Preserve the original for explicit recovery.
- Provide `PRAGMA quick_check` through a future `harness doctor`; use `integrity_check` for deeper diagnosis. SQLite documents the structural checks each performs ([SQLite pragmas](https://www.sqlite.org/pragma.html#pragma_integrity_check)).
- Restore from a known-good copy or logical export; do not reconstruct missing canonical Events from terminal logs.
- Treat semantic reducer violations as logical corruption even when SQLite's page structure is valid.

Running a full integrity check on every CLI start is unnecessary and makes startup proportional to database size. Recovery journals are processed automatically on open.

### 8.2 Filesystem and path policy

Resolve the database root before repository input and store it in the platform's per-user application data/state directory, never inside the Workspace. Open the root no-follow; verify local-filesystem policy, expected type, current-user ownership, and private permissions (`0700` directory plus `0600` database/sidecars on Unix) or an equivalent effective Windows ACL. Refuse ambiguity instead of silently weakening or repairing the policy. The Harness and future Tools must not inherit direct access merely because the user can run the CLI.

Open the resolved database path with `SQLITE_OPEN_NOFOLLOW`; SQLite defines that flag specifically for `sqlite3_open_v2` ([SQLite open flags](https://www.sqlite.org/c3ref/open.html)). Do not accept a repository- or model-supplied state path.

Support local filesystems only. SQLite explicitly warns that buggy network-filesystem locking can permit simultaneous writes and corruption ([appropriate uses for SQLite](https://www.sqlite.org/whentouse.html)).

### 8.3 Confidentiality

The database can contain objectives, selected instruction contents or digests, model summaries, and results. SQLite, redb, JSONL, and filesystem layouts do not encrypt those bytes by default.

V1 should:

- store only the bounded data required for replay;
- keep provider credentials and authorization headers out of Events;
- avoid raw provider envelopes;
- create user-private files/directories;
- rely on platform full-disk/user-home encryption for at-rest protection; and
- document that copying/exporting the database copies sensitive content.

Do not add SQLCipher until an explicit at-rest threat model, key source, recovery story, and distribution test exist. Encryption without key management would create the appearance of safety rather than a complete control.

## 9. Concurrency and async integration

### 9.1 One owner

Keep exactly one writable SQLite connection per Harness process. Root and workers never write directly. They send semantic outcomes to the Engine, and the Engine serializes accepted transitions.

This preserves deterministic sequencing without exposing database locks to agent tasks. SQLite's default serialized threading mode is safe, but using one owner is a simpler application invariant ([SQLite threading](https://sqlite.org/threadsafe.html)).

### 9.2 Synchronous library inside an async Engine

`rusqlite` is synchronous. A durable commit can block while the OS flushes, so the V1 Engine gives the sole connection to one named `std::thread`. The async coordinator sends private requests through a bounded channel and receives each result through a one-shot reply. Accepted transactions run to commit or rollback even if their async caller is cancelled; the Engine publishes no Event until the reply confirms commit.

This worker is an internal ownership mechanism, not a Store trait, actor framework, daemon, pool, or second process. Its bounded queue applies backpressure, and no transaction is held across provider I/O, rendering, user interaction, or an async `.await` inside the storage thread.

Do not introduce an asynchronous connection pool. SQLx's pool manages multiple asynchronous connections and internal runtime tasks ([SQLx pool](https://docs.rs/sqlx/latest/sqlx/pool/)); none of that serves a one-connection, one-writer CLI. If a blocking worker becomes necessary, `rusqlite` remains smaller and makes transaction ownership clear.

### 9.3 Lock policy

- One process owns the write connection during a Run.
- `show` is supported after the owner exits in V1.
- A concurrent second writer receives a bounded `BUSY` error.
- A future live read process is the trigger to evaluate WAL, not a reason to add an application lock file now.
- Never disable SQLite locking with `nolock=1`.

## 10. Schema and Event evolution

### 10.1 Database migrations

On writable open:

1. read `PRAGMA user_version`;
2. reject a version newer than the binary understands;
3. run each forward migration in order inside an exclusive migration transaction;
4. update `user_version` only with the successful schema change; and
5. reopen normal prepared statements after migration.

V1 has only migration `0 -> 1`. Do not create a migration framework dependency; a short Rust `match` is sufficient.

Downgrade writes are unsupported. An older binary must fail without changing the file.

### 10.2 Event migrations

Each `(kind, event_version)` maps to one typed decoder. Prefer decoding old payloads and reducing them into the current in-memory model. Rewrite historical Events only when continued compatibility becomes materially expensive and only through an explicit backup-then-migrate command.

Do not use Rust enum variant names or default Serde layouts as the durable contract. Give kinds and fields explicit names and test captured V1 fixtures.

### 10.3 Queryability and debugging

The selected schema immediately supports:

```sql
-- Replay one Run within its Session.
SELECT sequence, agent_run_id, kind, event_version, payload, created_at_ms
FROM events
WHERE run_id = ?1
ORDER BY sequence;

-- Inspect lifecycle kinds without decoding full state.
SELECT kind, count(*)
FROM events
WHERE run_id = ?1
GROUP BY kind
ORDER BY kind;

-- Find nonterminal histories in application code after ordered replay.
SELECT DISTINCT run_id
FROM events;
```

Do not add JSON expression indexes before a real query uses them. Do not place status in a column just to make debugging easier; status is a reducer result.

## 11. Backup, export, deletion, and portability

### 11.1 Demonstrator backup

Because there is no daemon and rollback `DELETE` mode removes the journal after successful commit, the minimum documented backup is:

1. ensure no Harness process is using the database;
2. copy the database file; and
3. optionally open the copy and run `quick_check`.

Do not implement a backup command merely to satisfy the proof.

### 11.2 Live backup trigger

Once a daemon or long Run makes “close then copy” unacceptable, use SQLite's online Backup API or `VACUUM INTO`. Both produce a consistent snapshot; the Backup API is incremental and better for a live database, while `VACUUM INTO` produces a compact copy ([SQLite Backup API](https://www.sqlite.org/backup.html), [SQLite VACUUM](https://www.sqlite.org/lang_vacuum.html)).

Never copy an active WAL database's main file alone. SQLite states that the WAL file is part of the persistent state and separation can lose committed transactions or corrupt the database ([SQLite WAL files](https://www.sqlite.org/wal.html#the_wal_file)).

### 11.3 Logical export

JSONL is the stable user-facing export:

- one versioned envelope per Event;
- ordered by sequence;
- explicit UTF-8;
- no SQLite implementation metadata; and
- optional redaction chosen by a future explicit export policy.

An export is not automatically a backup until an import/round-trip test proves it preserves every replayed fact.

### 11.4 Retention and deletion

V1 does not delete canonical Events automatically. It has no retention scheduler, tombstones, vacuum policy, or compaction.

When explicit whole-Run deletion becomes a product requirement, define whether audit history must remain, delete all rows for that Run in one transaction, and test sequence non-reuse assumptions. The current plain `INTEGER PRIMARY KEY` remains safe because sequence is database-local ordering, not a durable globally shared identity.

## 12. Verification plan

### 12.1 Schema tests

- A new database has `user_version=1`, one `events` table, and the canonical Session and Run indexes.
- `journal_mode` returns `delete`; `synchronous` returns `3` (`EXTRA`).
- The table is `STRICT`.
- malformed JSON, unknown application kinds, empty/oversized identifiers, payloads over the application limit, and version zero are rejected at the correct boundary.
- an unsupported future `user_version` opens neither read-write nor with automatic downgrade.

### 12.2 Transaction tests

- a valid multi-Event transition commits all rows in order;
- an injected failure on the second insert leaves none of that transition's rows;
- `RunFinished` cannot commit before every admitted child reaches a terminal state;
- committed Events are reduced before rendering and uncommitted Events are never rendered as durable;
- a concurrent second writer hits the bounded busy behavior;
- disk-full and read-only errors leave the prior prefix replayable.

### 12.3 Process-death matrix

Drive the scripted fake in a child process. Add deterministic failpoints around:

1. before `BEGIN`;
2. after `BEGIN`;
3. after each insert in a multi-Event transition;
4. immediately before commit;
5. immediately after commit returns;
6. after reduction but before terminal render; and
7. during graceful cancellation.

At each failpoint, kill the child with the platform's uncatchable termination mechanism where available. Reopen from a fresh process, run `quick_check`, replay, and assert that the result equals either the previous committed state or the full new transition—never a subset. Repeat enough times to cover journal creation, growth, synchronization, and deletion timing.

Application process-kill tests do not prove physical power-loss behavior. The durability claim also relies on SQLite's crash-testing evidence and the supported filesystem/device honoring sync. Record the test machine, filesystem, mount options, and storage type for release benchmarks.

### 12.4 Reducer and migration tests

- replay the same fixture from memory and SQLite and compare `RunView` exactly;
- replay each Event kind/version from captured JSON fixtures;
- reject unknown kinds and unsupported versions without partial state;
- migrate a V0 fixture to V1 and replay it;
- refuse a future-version fixture without modifying its bytes; and
- replay an incomplete prefix as `Interrupted`, not `Cancelled` or `Failed`.

### 12.5 Corruption tests

- truncate a copy of the database and require a safe open/replay error;
- flip bytes in copies covering header, table pages, and payload text;
- distinguish structural `CORRUPT`/`NOTADB` from semantic reducer violations;
- preserve the corrupt fixture rather than repairing it in place; and
- verify errors do not contain payload text or credentials.

SQLite's own crash suite is not a replacement for these integration tests; these verify Harness acknowledgement, reducer, and error semantics.

### 12.6 Cross-platform tests

Run the schema, replay, graceful cancellation, abrupt-kill, long path, non-ASCII path, read-only, and second-process lock tests on Linux, macOS, and Windows. The `rusqlite` bundled build must be part of each release target.

## 13. Performance and benchmark gates

Measure the production policy on local SSD storage with cold and warm caches. Never compare SQLite `EXTRA` to an unsynchronized KV/file write and call the latter faster; compare equal acknowledgement guarantees.

Generate representative histories at 100, 1,000, 10,000, and 100,000 Events with the actual payload distribution. Record p50/p95/p99/max, database bytes, CPU, and peak RSS for:

- database open and schema check;
- one-Event durable transition;
- three-Event durable transition;
- full replay by Run;
- `RunView` reduction;
- closed-file copy and logical JSONL export; and
- recovery/open after forced process death.

Initial acceptance gates on each supported reference machine:

| Measure | Gate | Consequence if missed |
|---|---:|---|
| One-Event durable commit, warm, p95 | ≤ 15 ms | Profile filesystem; compare WAL+FULL with equal durability |
| One-Event durable commit, warm, p99 | ≤ 40 ms | Keep state correct; batch logical transition; investigate stalls |
| Three-Event transaction, warm, p95 | ≤ 20 ms | Verify one commit, prepared statements, payload size |
| Open + replay 1,000 Events, warm, p95 | ≤ 50 ms | Profile decoding/reducer before changing storage |
| Open + replay 10,000 Events, warm, p95 | ≤ 250 ms | Consider snapshot trigger only after profiling |
| Replay 100,000 Events, warm, p95 | ≤ 2 s | Snapshot is earned; backend replacement is not yet earned |
| Recovery/open after forced kill, p95 | ≤ 250 ms at 10,000 Events | Diagnose journal/recovery and filesystem |
| Storage time in realistic provider Run | < 1% of Run wall time | Tune only if user-visible |
| Acknowledged Event loss after fault tests | 0 | Release blocker |
| Partial logical transition after fault tests | 0 | Release blocker |

These are product budgets, not claims about SQLite. Adjust them only from measured user expectations, not to make a chosen backend pass.

Optimization order after a miss:

1. confirm the benchmark uses semantic Events, not token deltas;
2. shrink oversized payloads and remove accidental duplicate content;
3. prepare and reuse insert/select statements;
4. group rows belonging to one transition into one transaction;
5. keep transactions free of provider, rendering, and async waits;
6. compare rollback `EXTRA` against WAL `FULL` on the same filesystem;
7. add a private blocking worker if executor stalls are the problem;
8. add a rebuildable snapshot if replay, not append, is the problem; and
9. change storage only when the selected backend still violates the gate.

## 14. Triggers that change the choice

| Evidence | Change |
|---|---|
| Live second process must read during a Run | Benchmark SQLite WAL+FULL; keep one writer |
| Durable rollback commits breach latency budget, WAL+FULL passes | Adopt WAL with checkpoint metrics and Backup API |
| Replay breaches budget while append remains healthy | Add rebuildable Run snapshots, not a new database |
| Large payloads dominate DB/replay | Add content-addressed Artifacts and persist references |
| `harness list` or analytics query breaches budget | Add a rebuildable SQL projection/index |
| Lexical search becomes real | Add FTS5 as a derived index |
| C/FFI is forbidden by a target/platform policy | Spike redb with identical crash, migration, and replay tests |
| Multiple local authoritative writer processes exist | Prefer one daemon owning SQLite; do not let clients contend directly |
| Multiple hosts need authoritative writes | Move canonical storage behind PostgreSQL/service boundary |
| Tenant isolation, replication, PITR, centralized backup, or operator access is required | PostgreSQL becomes justified |
| Sustained write workload exceeds SQLite after batching and WAL | Benchmark redb/Fjall/RocksDB with equal durability and full application indexes |
| Tamper evidence is a product requirement | Threat-model signatures/hash chains and protected keys; checksums alone are insufficient |
| At-rest confidentiality is required beyond OS encryption | Design key management and evaluate SQLCipher/encrypted service storage |
| Automatic Run resume is required | Add explicit recovery state, provider idempotency/reconciliation, and possibly an outbox |

Database size alone is not a migration trigger. SQLite's official guidance prefers it for device-local, low-writer-concurrency storage and recommends client/server systems when data is remote or many writers must proceed concurrently ([appropriate uses](https://www.sqlite.org/whentouse.html)). The Harness should move for an observed access-pattern or operational requirement, not because a file crossed an arbitrary number of megabytes.

## 15. What not to build

For the demonstrator, do not add:

- a `Store` trait or pluggable backend registry;
- separate canonical `runs` or `agent_runs` tables;
- SQLite WAL/checkpoint code;
- a connection pool;
- a daemon-owned writer actor;
- snapshots;
- outbox/effect tables;
- FTS or vector indexes;
- event hash chains;
- SQLCipher;
- automatic repair;
- compression;
- retention/vacuum jobs;
- per-Run databases;
- custom JSONL recovery;
- a PostgreSQL feature flag; or
- benchmarks for every available embedded database.

One internal `store.rs` with connection setup, migration `0 -> 1`, append-transition, replay-Run, and test fault injection is sufficient.

## 16. Final decision

**Decision:** SQLite remains the minimum honest persistence choice for the Rust CLI Harness.

**Exact V1 shape:**

- `rusqlite` with bundled SQLite;
- database in a private, owner/ACL-verified, outside-Workspace per-user local application directory;
- one writable connection owned by a dedicated standard-library thread inside the Engine;
- local filesystem only;
- read-write/create open with `SQLITE_OPEN_NOFOLLOW` and `SQLITE_DBCONFIG_DEFENSIVE`;
- rollback `journal_mode=DELETE`;
- `synchronous=EXTRA`;
- `trusted_schema=OFF`;
- 4 KiB pages, a 65,536-page/256 MiB maximum, and 4 MiB admission headroom for a new Run;
- short `BEGIN IMMEDIATE` transactions;
- 250 ms busy timeout;
- one `STRICT` Events table and `(run_id, sequence)` index;
- plain `INTEGER PRIMARY KEY`, not `AUTOINCREMENT`;
- database `user_version=1` and per-Event `event_version=1`;
- typed, bounded JSON payloads with a 64 KiB hard maximum;
- commit before durable feedback;
- strict replay that validates structure and treats every stored string as data, never instructions or authority;
- replay incomplete committed prefixes as `Interrupted`; and
- no automatic resume.

**Reason:** it minimizes the amount of persistence correctness that this repository must invent while preserving zero-configuration local execution, transparent debugging, transactional multi-Event transitions, cross-process reopening, and a clear path to later readers, snapshots, search, and export.

**Trade-off:** the binary gains a native C/FFI dependency and every durable transition pays a real synchronization cost. Those are visible and testable costs. The alternatives either fail the required persistence behavior, move substantially more correctness into Harness code, optimize a workload that does not exist, or add a server boundary before the product earns one.

**First implementation evidence required:** the SQLite-backed portion of `tests/team_run.rs`, the forced-process-death matrix, identical in-memory/SQLite replay, and the benchmark gates above. Until they pass, “durable replay” is a design intention rather than a demonstrated property.

## Primary source map

- SQLite: [transactional guarantees](https://www.sqlite.org/transactional.html), [atomic commit](https://www.sqlite.org/atomiccommit.html), [testing](https://www.sqlite.org/testing.html), [isolation](https://www.sqlite.org/isolation.html), [transactions](https://www.sqlite.org/lang_transaction.html), [WAL](https://www.sqlite.org/wal.html), [pragmas](https://www.sqlite.org/pragma.html), [corruption causes](https://www.sqlite.org/howtocorrupt.html), [result codes](https://www.sqlite.org/rescode.html), [backup](https://www.sqlite.org/backup.html), [`VACUUM INTO`](https://www.sqlite.org/lang_vacuum.html), [autoincrement](https://www.sqlite.org/autoinc.html), [STRICT tables](https://www.sqlite.org/stricttables.html), [JSON](https://www.sqlite.org/json1.html), [limits](https://www.sqlite.org/limits.html), [file compatibility](https://www.sqlite.org/formatchng.html), [application file format](https://www.sqlite.org/appfileformat.html), [appropriate uses](https://www.sqlite.org/whentouse.html), [threading](https://www.sqlite.org/threadsafe.html), and [open flags](https://www.sqlite.org/c3ref/open.html).
- Rust integration: [rusqlite](https://github.com/rusqlite/rusqlite), [Rust file synchronization](https://doc.rust-lang.org/std/fs/struct.File.html#method.sync_all), and [SQLx pools](https://docs.rs/sqlx/latest/sqlx/pool/).
- Filesystem semantics: [POSIX `fsync`](https://pubs.opengroup.org/onlinepubs/9699919799/functions/fsync.html), [POSIX rename](https://pubs.opengroup.org/onlinepubs/9799919799/functions/rename.html), [POSIX durability rationale](https://pubs.opengroup.org/onlinepubs/9799919799/xrat/V4_xbd_chap01.html), [Linux `fsync`](https://man7.org/linux/man-pages/man2/fsync.2.html), and [Windows `FlushFileBuffers`](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers).
- Rust-native KV: [redb crate](https://docs.rs/redb/latest/redb/), [redb durability](https://docs.rs/redb/latest/redb/enum.Durability.html), [redb database API](https://docs.rs/redb/latest/redb/struct.Database.html), [redb design](https://github.com/cberner/redb/blob/master/docs/design.md), [redb changelog](https://docs.rs/crate/redb/latest/source/CHANGELOG.md), [Fjall](https://github.com/fjall-rs/fjall), and [sled](https://github.com/spacejam/sled).
- RocksDB: [overview](https://github.com/facebook/rocksdb/wiki/RocksDB-Overview), [WAL performance](https://github.com/facebook/rocksdb/wiki/WAL-Performance), [transactions](https://github.com/facebook/rocksdb/wiki/Transactions), [checkpoints](https://github.com/facebook/rocksdb/wiki/Checkpoints), and [basic operations](https://github.com/facebook/rocksdb/wiki/Basic-Operations/8b0db11192422ae154253ae6e76123f28b09488a).
- PostgreSQL: [architecture](https://www.postgresql.org/docs/current/tutorial-arch.html), [MVCC](https://www.postgresql.org/docs/current/mvcc-intro.html), [WAL](https://www.postgresql.org/docs/current/wal-intro.html), [WAL configuration](https://www.postgresql.org/docs/current/runtime-config-wal.html), and [backup](https://www.postgresql.org/docs/current/backup.html).
