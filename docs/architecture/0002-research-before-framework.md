# ADR-0002: Research before framework commitment

Status: Accepted as research baseline
Date: 2026-08-14

## Decision

The initialization workspace uses the Rust standard library only. No graph library, async runtime, serialization framework, plugin ABI, or persistence system is selected yet.

A dependency is introduced when an experiment demonstrates a concrete need and records the trade-off.

## Rationale

The project is evaluating architecture boundaries. Premature framework selection would make it difficult to distinguish domain requirements from framework-shaped requirements.

## K1 physical durability selection

K1 selects `redb 2.6.3` as the first embedded physical backend for the typed
`DurableStore` contract. The version is pinned because it supports the
workspace Rust 1.85 MSRV. `redb` supplies the physical transaction, file
coordination, copy-on-write pages, and storage checksums; `workflow-recovery`
continues to own CAS validation, idempotency, typed fact invariants, and
backend-neutral error classification.

The durable value is a version-4 `postcard 1.1.3` snapshot of
`DurableRunState`, prefixed with a Kernis format magic, schema version, and
deterministic payload checksum. The snapshot carries an ordered commit ledger
as the replay authority; its idempotency map is only a materialized lookup
index and must match the ledger exactly. The checksum detects logical snapshot
mutation before deserialization; redb's page checksums remain a separate
physical integrity boundary. `serde` derives are applied only to the stable
durable fact types. Runtime objects such as Fibers, capability handles,
streams, registries, disposers, and effect closures remain outside the
snapshot. The shared `kernis-core::Id` primitive is included because it is the
identifier field inside those stable facts; it is not a serialization
commitment for process-local runtime objects. Older snapshot versions fail
closed rather than being migrated.

### Focused comparison

| Candidate | Result | Reason |
| --- | --- | --- |
| `redb` | Selected | Embedded ACID transactions and cross-process file coordination fit the synchronous store port without introducing a SQL schema. |
| SQLite | Rejected for K1 | A valid embedded option, but its SQL/schema surface is broader than this typed fact contract and the earlier durability research explicitly deferred a SQL adapter. |
| Custom file snapshot/log | Rejected for K1 | Would require Kernis to re-prove portable file locking, atomic replacement, crash recovery, and corruption handling that the embedded backend already supplies. |

The adapter opens the physical redb handle for each operation while exposing a
logical `FileDurableStore` connection. This lets separately opened store values
observe the same file and exercise expected-revision CAS/idempotency without
retaining a process-local database handle. A redb physical lock contention is
reported as `StoreError::BackendUnavailable` rather than being confused with a
domain revision conflict. Active bootstrap lock contention reports
`BackendUnavailable`; after the bounded bootstrap wait, a persistent redb file
with no Kernis table reports `DataCorruption`, as does a populated file with an
incompatible table.

K1 claims atomic redb commit transactions, typed schema/version/checksum
rejection, lineage validation, CAS, idempotent replay, and successful reopen
after normal process termination. It does not claim `fsync`/power-loss
guarantees, arbitrary schema migration, distributed ownership, or persistence
of the runtime object graph.
