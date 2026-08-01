# Cross-Platform Canonical Path Casing

Status: approved, non-normative implementation design

Date: 2026-07-31

`SPEC.md` remains the normative contract for the current v1 implementation.
This document describes the approved v2 direction. The implementation change
must update `SPEC.md` in the same pull request before any v2 behavior ships.

## Summary

Hive Memory stores one canonical inbox that is replicated to Windows, macOS,
and Linux. It therefore needs one path spelling that does not change with the
case behavior of the client currently reading or rewriting it.

Hive will preserve the existing write-ID spelling as canonical. Each artifact's
filename matches its own ID exactly. Paired sidecars point to their same-ID
note; operational events such as promotions use their existing typed source
reference. Filesystem case sensitivity may influence diagnostics, but it must
never influence persisted path spelling.

The change ships as a coordinated manifest-schema migration from v1 to v2. The
migration runs only on a fully materialized local staging copy on a filesystem
that passes Hive's durability and no-clobber capability checks. Direct mutation
through an rclone/FUSE mount or cloud API is forbidden. After local migration,
an operator publishes the verified store to the remote with data first and the
manifest last.

The migration renames case-only path variants, repairs case-only `note_path`
metadata, verifies an exact artifact inventory, and then updates the local
manifest. Existing logical IDs are never rewritten.

## Problem

The v1 path contract lowercases store-relative metadata on filesystems detected
as case-insensitive and preserves case on case-sensitive filesystems. That
makes the same cloud-synchronized store acquire different path spellings
depending on which client last rebuilt an index or rewrote a record.

The background classifier exposed the defect:

1. a case-insensitive client rebuilt an index with a lowercased note path;
2. classification passed that index path into the retag rewrite;
3. the note was rewritten through the lowercased path;
4. the event retained its canonical uppercase write ID and `note_path`;
5. a case-sensitive Linux/rclone client later treated the two spellings as
   different files.

The affected notes are not missing. Their filenames differ from the declared
paths only by the `T` and `Z` casing in the write-ID timestamp.

## Goals

- Give every canonical Hive-owned inbox path one exact spelling across Windows,
  macOS, Linux, and replicated cloud stores.
- Keep each machine's absolute store root local and independent from portable
  store-relative identity.
- Preserve existing logical IDs and all references to them.
- Preserve the invariant that an inbox filename stem equals its record ID.
- Prevent indexes and comparison keys from being reused as mutation paths.
- Detect paths that would collide on a case-insensitive filesystem before
  accepting or migrating a store.
- Provide a preflighted, durably journaled, resumable, and reversible local
  v1-to-v2 migration.
- Provide a checked publication procedure for non-transactional cloud storage.
- Make pre-v2 binaries refuse writes after the migration commits.
- Prevent a stale pre-migration outbox from reintroducing v1 paths.
- Leave the store with valid typed event/note references and no path-casing
  warnings.

## Non-Goals

- Changing the generated write-ID field layout for normally sized components.
- Automatically rewriting unsupported legacy IDs or `supersedes` references.
- Renaming human-authored curated memory files.
- Making cloud storage transactional.
- Running the migrator directly on FUSE, rclone, network, or cloud backends.
- Supporting concurrent writers during migration or publication.
- Providing distributed compare-and-swap for concurrent edits to the same
  existing record on different cloud clients.
- Automatically merging duplicate or conflicting records.
- Turning `hm doctor --fix` into a schema migrator.

## Canonical Path Contract

### Write IDs

The generated write-ID format remains canonical:

```text
YYYYMMDDTHHMMSS.ffffffZ_<host>_<pid>_<agent>_<random>
```

The timestamp uses uppercase `T` and `Z`. Host, agent, and random components
keep their existing sanitization contract. Existing supported IDs and newly
generated IDs are used exactly as produced; path code does not change case.

Manifest schema 2 narrows inbox IDs to a portable filename grammar:

- 1 through 128 ASCII bytes;
- first byte in `[A-Za-z0-9]`;
- remaining bytes in `[A-Za-z0-9._-]`;
- final byte in `[A-Za-z0-9_-]`;
- neither `.` nor `..`;
- the case-insensitive prefix before the first `.` is not `CON`, `PRN`, `AUX`,
  `NUL`, `COM1` through `COM9`, or `LPT1` through `LPT9`;
- no trailing dot or space.

The generator keeps the current field layout and bounds each sanitized host and
agent component to 32 bytes and the random component to 12 bytes. An overlong
component becomes a readable prefix, `-`, and a deterministic eight-hex digest
of the complete original component. Production UUID randomness already fits
the 12-byte budget; the bound also covers the public deterministic ID builder.
With the fixed timestamp, PID, separators, and bounded components, every
generated ID is at most 113 ASCII bytes. One shared encoder and constants own
this rule; configuration and write validation call the same implementation.

Migration preflight rejects any legacy or imported ID outside the 128-byte
grammar with a structured `unsupported_portable_id` error. Renaming such an ID
changes record identity and references, so that remediation is deliberately a
separate, explicit migration rather than a guess made by the casing migration.

### Inbox paths

For a record with ID `id` and UTC creation date `YYYY/MM/DD`, the only canonical
paths are:

```text
inbox/notes/YYYY/MM/DD/<id>.md
inbox/events/YYYY/MM/DD/<id>.json
```

Every note or event filename stem equals that artifact's own `id`, and each
artifact's day partition comes from its own `created_at`.

For paired memory sidecars (`memory.observation`, `memory.correction`,
`memory.task`, `memory.decision`, and `memory.import`), the following values
must agree exactly:

- Markdown front-matter `id`;
- Markdown filename stem;
- JSON event `id`;
- JSON event filename stem;
- JSON event `note_path`;
- note and event day partitions derived from their shared `created_at`.

Operational events have event-type-specific reference rules:

- `memory.promotion` has its own event ID and day partition. Its `note_path`
  references the source inbox note named by `source.ref`; that source note's ID
  and creation date determine the canonical note path. The event ID is not
  compared with the source note ID.
- `memory.compaction` has no paired note and normally omits `note_path`.
  A compaction event with a `note_path` is rejected unless a future normative
  schema defines its meaning.

One typed `EventPathContract` helper owns these rules for writers, inventory,
migration, doctor, and verification. Promotion provenance is never
reconstructed from the promotion event's own ID.

Path serialization converts separators to `/` and preserves case
unconditionally. Hive-owned canonical inbox components are ASCII under schema
2, so Unicode normalization differences cannot create two canonical spellings.

### Portable collision key

Hive computes a separate portable collision key for Hive-owned inbox paths.
The key uses normalized `/` separators and ASCII lowercase. Because schema-2
inbox IDs and fixed path components are ASCII, this models the relevant
cross-platform case equivalence without relying on host filesystem behavior.

The key is used only to detect ambiguity and compare paths for portability. It
must never be:

- serialized into canonical note or event metadata;
- stored in an API field documented as a physical path;
- passed to filesystem read, write, rename, or remove operations;
- returned as the exact observed path in the local triage index.

Two distinct artifacts with the same portable collision key are a hard error.
Hive never chooses one based on directory enumeration order.

## Path Ownership and Interfaces

Path responsibilities have one owner rather than being reimplemented by
writers, indexes, classifiers, doctor, outbox, and migration code.

### Canonical paths

A canonical-path helper accepts an exact logical ID and `created_at` value,
validates the schema-2 ID grammar, and returns the note and event paths.
Writers, outbox creation and flushing, pair validation, and migration use this
helper.

The helper preserves the ID exactly. It does not inspect the local filesystem
or accept a case-sensitivity mode.

### Store root locality

The configured store root is a host-local location, not part of the portable
path contract. The same manifest `store_id` may be reached as, for example, a
Windows drive or UNC path, a macOS volume path, and a Linux mount path. Those
absolute roots need not have matching components, spelling, or case.
Normal reads and writes may use a case-insensitive root; only the one-time
mutating migration has the stricter local case-sensitive staging requirement.
The configured root spelling is neither canonical store data nor a portability
collision input.

Host-specific configuration and local overrides map a store alias to its local
root. `open_store` resolves that root through the existing configuration and
symlink policy, verifies the manifest's stable `store_id`, and holds the trusted
effective root for the command. It never case-folds, compares, or serializes the
absolute root as record identity.

All canonical helpers return store-relative `/`-serialized paths. Notes,
events, outbox metadata, migration inventories, and portable collision keys
contain only those relative paths; a local cache may retain an absolute lookup
hint but it is rebuildable and never authoritative. Filesystem operations join
an opaque validated relative path to the currently open host-local root.

The migration coordinator may temporarily map the same store ID to a local
staging root. Remote identifiers and staging paths remain runbook/configuration
inputs and never enter canonical artifacts.

### Observed paths

Directory scans return paths exactly as filesystem enumeration reports them.
Candidate-path `exists()` calls are not evidence of exact spelling on a
case-insensitive filesystem.

Only the inventory layer can construct an opaque `ObservedInboxPath`. Its
constructor:

- accepts a relative path produced by directory enumeration, never a cache
  string or caller-supplied path;
- rejects absolute paths, `..`, and locations outside canonical inbox
  directories;
- rejects symlinked files or directories;
- verifies the canonical parent remains inside the configured store root;
- records the case-preserved directory-entry name, content hash, and available
  stable file identity.

The local index stores exact observed paths for reads, but the cache remains
private and non-authoritative. On cache lookup, the inventory layer resolves
and revalidates the cached hint before returning an `ObservedInboxPath`.

An opaque `ResolvedRecord` carries:

- exact logical record ID;
- exact observed note and event paths;
- note and event byte hashes;
- available file identities;
- portable collision keys used only for validation.

The cache format is rebuildable, so its internal version is bumped without
migrating cached files.

### Existing-record mutation

Classification and manual retagging operate on a `ResolvedRecord`, not a
normalized or caller-supplied path string. Mutation APIs never accept a bare
store-relative string.

Existing-record mutation must not reconstruct the event path from the ID after
opening the note. A store-local record lock serializes Hive mutations on the
current client. Under that lock, mutation reopens both exact paths without
following symlinks and compares IDs, byte hashes, and available file identities
with the resolved snapshot immediately before replacement. A mismatch returns
`stale_resolved_record` and causes the caller to refresh; matching IDs alone
are insufficient.

This is a local stale-read guard, not distributed compare-and-swap. Generic
cloud storage offers no primitive that atomically couples Hive's validation to
replacement across clients. The operation therefore preserves the existing
no-concurrent-same-record-writers contract and documents that external sync can
still race in the final replacement window. This casing work does not claim to
solve that separate storage-model limitation.

### Store access gate

One `open_store(root, AccessMode)` gate owns manifest compatibility and active
migration checks. Canonical readers and writers receive the resulting store
handle rather than a bare root.

Access modes are:

- `NormalRead` and `NormalWrite`, both refused while a journal is nonterminal;
- `MigrationControl`, restricted to migration commands;
- `DoctorReadOnly`, permitted to inspect and report but never repair;
- `ConfigStatus`, limited to manifest and configuration status.

Every CLI family, hook, classifier, index rebuild, search fallback, and outbox
flush enters through this gate. Table-driven tests cover every family in every
journal phase.

## Configuration

`[storage].case_sensitive` no longer controls canonicalization or metadata
serialization. The key remains accepted during the transition so existing
fleet configuration does not fail to load.

Its remaining use is limited to diagnostics about the mounted filesystem. All
portability collision checks run regardless of its value. The key is documented
as deprecated and may be removed in a later config-schema change.

No configuration setting may opt a store out of the canonical casing contract.

## Store and Artifact Versioning

The migration bumps the store manifest schema from 1 to 2. Note, event, and
outbox artifact schemas remain at 1 because their serialized shapes do not
change. The manifest version records that the complete store satisfies the new
path invariant.

This choice provides the mixed-version safety boundary:

- a v2 binary can inspect and migrate a v1 store;
- before migration starts, a v2 binary may perform normal v1 operations using
  the corrected case-preserving path behavior;
- a v2 binary refuses normal access while a migration journal is active;
- a v1 binary rejects a manifest newer than its supported schema;
- all fleet clients must be upgraded before writers resume.

New stores created by the upgraded binary start at manifest schema 2. The local
config schema remains unchanged.

### Schema-aware outbox flushing

An outbox item's recorded destination paths are hints, not authority. The
migration-capable binary performs this check before publishing to either a v1
or v2 store, so fleet preparation cannot flush an old lowercase destination
immediately before migration. Flush parses the queued note and event, derives
canonical destinations from their exact ID and `created_at`, and checks:

- the ID satisfies the schema-2 portable grammar;
- recorded note and event destinations match the derived paths exactly;
- the event's `note_path` matches the derived note path exactly;
- the queued store ID matches the open store.

A mismatch is refused with `outbox_path_contract_mismatch`; flush never
publishes the recorded v1 path verbatim. A separate local reconciliation step
may atomically upgrade a queued item only after proving the payloads and
derived paths agree. This guard applies even though the outbox artifact schema
remains 1 and protects against an offline pre-migration client returning after
cutover.

## Migration Execution Boundary

`hm stores migrate` mutates only a fully materialized local staging store. It
refuses a root on FUSE, rclone, network, or other remote filesystems. A live
deployment uses a dedicated staging directory on a supported local Linux
filesystem, never a mounted cloud path.

Preflight inventories the mount/device identity and directory-specific flags of
the manifest, journal, every canonical inbox directory, and every artifact
parent. All must remain on the one approved local mount and device; nested bind,
FUSE, network, or other mount boundaries and casefold-enabled directories are
refused.

It then performs a destructive capability probe in a uniquely named private
temporary directory in every affected parent and requires:

- case-sensitive directory entries;
- true no-overwrite rename support;
- atomic same-filesystem replacement;
- file and parent-directory durability synchronization;
- stable enumeration and no symlink traversal.

The migrator rechecks mount/device identity and effective directory flags
immediately before each mutation. Probes clean up only their own uniquely named
files. Unsupported filesystems fail with
`migration_filesystem_unsupported`. There is no weaker non-crash-safe
migration mode.

Restricting mutation to this boundary is intentional: Google Drive can contain
duplicate same-name objects, and an rclone mount can expose cached directory
state and delayed VFS uploads. A mounted view cannot prove the complete,
durable inventory required by the migration protocol.

This boundary follows rclone's documented
[Google Drive duplicate behavior](https://rclone.org/drive/#duplicated-files)
and
[VFS directory-cache and writeback behavior](https://rclone.org/commands/rclone_mount/).

## Migration Command

The migration is implemented by `hm stores migrate`, not `hm doctor --fix`.
The first mutating implementation is deliberately single-store. `--store` is
required for migrate, resume, and rollback:

```text
hm stores migrate --store personal --dry-run
hm stores migrate --store personal
hm stores migrate --store personal --resume
hm stores migrate --store personal --rollback
```

`--dry-run` without `--store` may inspect all configured stores, but it never
creates journals or mutates them. Its result contains one independent status
per store. There is no partially committed multi-store transaction, ambiguous
active journal selection, or implicit continuation after one store fails.

Migration supports `--json`. A single-store result contains `store`,
`store_id`, `plan_id`, `phase`, artifact and operation counts,
`audit_event_id`, and `journal_path`. Multi-store dry-run returns those fields
per store. Errors use the global JSON envelope and exit-code contract with
stable reason codes including:

```text
migration_preflight_failed
migration_filesystem_unsupported
migration_active_attempt
migration_attempt_not_found
migration_interference
migration_manifest_changed
migration_rollback_required
outbox_path_contract_mismatch
unsupported_portable_id
```

Invalid option combinations use exit code 2, schema/preflight failures use 3,
safety refusals use 4, and unavailable backends use 5. A mutation error never
falls through to the next configured store.

Normal execution always repeats preflight before it durably creates the plan.
Once a plan exists, resume and rollback use that immutable plan rather than
performing a fresh migration scan.

### Preflight

Preflight reads the complete local canonical inbox and manifest and produces an
immutable plan. It verifies:

- the store manifest is schema 1;
- no prior incomplete migration requires explicit resume or rollback;
- the filesystem passes every migration capability probe;
- all notes and events parse;
- every note and event ID satisfies the portable grammar;
- every logical ID is unique within its artifact type;
- every paired sidecar agrees with its note on exact logical ID and creation
  date;
- every promotion `note_path` resolves to the canonical source note named by
  `source.ref`, without comparing the promotion event's own ID to that note;
- every operational event satisfies its typed `EventPathContract`;
- every observed path differs from its calculated canonical path by at most
  ASCII case;
- no two distinct artifacts or operations claim the same portable key;
- all directory entries, including non-Hive siblings, are included in
  destination and temporary-name occupancy checks;
- no canonical destination is occupied by unrelated content;
- no conflict-copy filename is present in canonical inbox directories;
- the coordinator's local outbox for the store is empty;
- all contents can be hashed and all containing directories are writable.

Differences beyond path case, malformed records, duplicate IDs, ambiguous
portable keys, or occupied destinations stop preflight. The migrator does not
guess, merge, quarantine, or overwrite.

The dry-run report includes counts and paths but never note bodies, matched
secret values, or other content.

### Exact artifact inventory

The plan inventories notes and events independently. Each entry contains:

```text
artifact type
exact logical ID
exact observed and canonical paths
portable collision key
original byte hash
expected final byte hash
available file identity
planned operation
```

Unchanged artifacts have identical original and expected hashes. For a planned
event rewrite, the plan stores the exact expected output and proves
structurally that only `note_path` differs. The migration audit event is one
explicitly planned addition with a preallocated ID, path, bytes, and hash.

The plan records separate pre-migration note and event counts and exact ID-set
hashes. Event-only operational history is therefore protected rather than
being hidden inside a memory-record count.

### Journal location and contents

Before the first mutation, Hive creates a durable journal under:

```text
<store-root>/.migrations/manifest-v1-to-v2/
  active
  attempts/<plan-id>/
```

The staging store and journal remain on the same local durable filesystem.
Remote publication explicitly excludes `.migrations/`; journal event backups
and coordinator metadata are never uploaded. An independent provider snapshot
or backup remains the recovery boundary after remote publication.

The journal contains:

- store ID, exact starting manifest bytes, and hash;
- immutable inventory and plan hash;
- original, temporary, and canonical path for each operation;
- original and expected hashes for every artifact;
- verbatim backup of each event that will be rewritten;
- preallocated migration audit bytes and expected hash;
- per-operation intent and completion records;
- starting note/event counts and ID-set hashes;
- migration phase and coordinator metadata.

Each immutable plan gets its own attempt directory. `active` is a checksummed,
durably replaced pointer to the sole nonterminal attempt. Creating it is a
no-clobber operation. If pointer validation fails, Hive scans attempt metadata:
exactly one valid nonterminal attempt can repair the pointer, while zero or
multiple candidates stop for manual review.

After an attempt reaches `rolled-back` or `committed`, Hive durably clears the
active pointer and retains the terminal attempt as history. A later v1
preflight creates a new plan ID and attempt without overwriting recovery
evidence. Terminal attempts are retained indefinitely by the first
implementation. Any future cleanup command must be explicit, terminal-only,
and policy-gated; cleanup is never implicit in migrate, resume, or rollback.

The top-level phase is one of:

```text
planned
renaming
rewriting
verifying
committing
committed
rolling-back
rolled-back
```

Any nonterminal journal makes `NormalRead` and `NormalWrite` fail with a message
naming the exact `--resume` or `--rollback` command.

### Durable write-ahead protocol

Migration durability is mandatory and independent of the store's normal
configurable fsync policy. Journal state is never appended in place. Each
attempt alternates between two checksummed state slots containing the plan
hash, a monotonic generation, phase, and complete operation-state map. A state
transition writes and fsyncs the inactive slot through the probed atomic
replacement primitive, syncs its parent, then treats the highest valid
generation as authoritative. An incomplete or corrupt slot is ignored when the
other validates; two invalid slots stop safely.

Every forward and reverse operation uses this order:

1. write the immutable plan, backups, and planned output when first needed;
2. fsync each new file and its parent directory;
3. durably publish a new checksummed state generation containing the operation
   intent;
4. perform the no-clobber rename or atomic content replacement;
5. fsync the changed file when applicable and every affected parent directory;
6. re-enumerate the parent, then verify exact spelling, identity, and hash;
7. durably publish the next checksummed state generation containing completion.

The plan and all required backups are durable before the first data mutation.
An implementation may batch independent operations only if the journal proves
the same ordering for every member. Atomic rename prevents torn names; the
sync barriers establish crash ordering.

Tests terminate the process at every barrier in forward migration and rollback,
and separately inject truncated/corrupt slot bytes to verify generation
selection and safe refusal.

### No-clobber rename

Every affected filename uses two no-overwrite moves, including on Linux:

```text
observed path
  -> sibling .hm-migrate-<plan-id>-<operation-id>.tmp
  -> canonical path
```

A platform abstraction named `rename_noreplace` must provide a true atomic
no-overwrite operation. Check-then-rename is forbidden. The temporary sibling
is included in the plan's full directory occupancy check and is reserved with
`rename_noreplace` immediately before the move. The migrator verifies exact
directory-entry spelling, file identity, and byte hash after each step.

The same primitive and rules apply during rollback. If the capability probe
cannot prove the primitive, migration is refused.

### Metadata rewrite

After all filenames are canonical, the migrator rewrites only event files whose
`note_path` differs from the typed `EventPathContract` result by ASCII case. A
paired sidecar derives that path from its own ID and `created_at`; a promotion
derives it from the source note resolved through `source.ref`. The migrator
never rewrites a promotion path from the promotion event ID.

Rewriting patches a parsed `serde_json::Value`, not the closed `MemoryEvent`
serializer. It preserves unknown members. The plan compares the original and
expected JSON trees after removing `note_path` and requires equality; it also
records the exact expected output hash. Formatting may change for a rewritten
event, but every semantic field other than `note_path` must remain unchanged.
The migration parser rejects duplicate object-member names before conversion
to `Value`, because a map representation cannot preserve ambiguous duplicate
keys.

The migration does not change:

- logical IDs;
- note front matter;
- note bodies;
- event IDs or bodies;
- timestamps;
- scope, project, audience, classification, or supersession metadata;
- unknown event fields.

Promotion `source.ref`, source-note identity, and curated-target provenance are
also unchanged.

Historical outbox archives are recovery evidence rather than canonical inbox
records. The migration does not rename archive snapshots.

If rewriting an event would require any semantic repair beyond path case, the
migration stops.

### Forward and reverse state resolution

Resume and rollback never probe candidate path strings with `exists()`. They
freshly enumerate the parent directory and compare exact case-preserved
`DirEntry` names, file identities, and hashes with the journal.

For a rename operation, forward resume handles:

- exact original entry only with expected identity/hash: start the move;
- exact temporary entry only with expected identity/hash: finish the move;
- exact canonical entry only with expected identity/hash: mark complete;
- no matching entry: stop as data loss;
- multiple distinct matching entries: stop as a collision;
- expected spelling with unexpected identity/hash: stop as interference.

Because migration runs on a case-sensitive local filesystem, these states are
unambiguous. Enumeration-based resolution remains mandatory so the same
algorithm never accidentally treats two differently cased string lookups as
two objects.

Event rewrite, audit creation, and manifest publication each have equivalent
absent/expected/conflicting states based on exact bytes and hashes. Rollback
has an explicit reverse state for every forward state:

- remove the audit event only when its exact expected hash is present;
- restore an event only from its durable backup and only when the current bytes
  are the planned original or expected migrated hash;
- reverse canonical to temporary to original using `rename_noreplace`;
- treat already-restored original content as complete;
- stop on any unplanned path, identity, or hash.

Re-running `--resume` or `--rollback`, including after interruption during
`rolling-back`, is idempotent.

### Commit verification

Before publishing the local v2 manifest, the migrator performs a fresh,
authoritative scan. Commit requires:

```text
before.note_ids == after.note_ids
before.event_ids + planned_audit_id == after.event_ids
before.note_count == after.note_count
before.event_count + 1 == after.event_count
all_unmodified_byte_hashes_match == true
all_planned_event_outputs_match == true
all_paths_are_canonical == true
all_typed_event_path_contracts_hold == true
portable_path_collisions == 0
```

Event-only history participates in these checks. Each rewritten event is also
structurally compared with its original to prove only `note_path` changed.
Promotion verification requires the exact original `source.ref` and canonical
source-note path while preserving the promotion event's independent ID.

### Audit event

The audit artifact remains event schema 1 and is parseable before the manifest
boundary. Its exact discriminator is:

```json
{
  "type": "memory.compaction",
  "source": {
    "kind": "schema.migration",
    "ref": "<plan-hash>"
  }
}
```

The remaining required v1 event fields and aggregate counts are populated
normally. The event contains no memory bodies or sensitive paths. Its ID,
canonical path, complete bytes, and hash are preallocated in the immutable
plan. Resume accepts only absent or exact-expected content; a conflict stops.
Rollback deletes only the exact planned hash.

A crash-window test proves the current v1 parser can read the audit after its
publication and before the manifest update.

### Manifest compare-and-swap

Immediately before manifest publication, the migrator rereads the exact
manifest bytes and requires their hash to equal the starting hash in the
journal. Any mismatch returns `migration_manifest_changed`, leaves the active
journal intact, and requires `--rollback`. Only after rollback reaches
`rolled-back` and durably clears the active pointer may a fresh preflight create
a new attempt; Hive never overwrites a concurrent policy change.

The v2 manifest is derived from the recorded v1 bytes by changing only the
approved schema and update metadata. Hive atomically replaces it, fsyncs the
file and parent directory, rereads it, and verifies the expected hash. The
journal then becomes `committed`.

If the process stops after the manifest write but before the journal update,
`--resume` recognizes the exact expected manifest, verifies the complete v2
store, and records `committed`. Rollback is refused once that compatibility
boundary is durable.

## Remote Publication and Fleet Cutover

Cloud publication is an operator runbook, not part of the local migration
transaction. Hive must not claim a distributed lock or remote atomicity.

The production procedure is:

1. Install the migration-capable binary on every fleet client.
2. Flush every known client outbox.
3. Stop hooks, classifiers, timers, interactive writers, and outbox flushers.
4. Stop the rclone mount cleanly and verify that its VFS cache has no pending
   uploads.
5. Run `rclone dedupe --dedupe-mode list` and provider-authoritative inventory
   to list duplicate objects and case/normalization collisions; abort until
   the remote is unambiguous.
6. Create an independent, recoverable remote snapshot or versioned backup.
7. Materialize the complete remote store into a fresh staging directory on the
   supported local durable filesystem, excluding no canonical artifacts.
8. Verify remote-to-staging content and inventory with an independent rclone
   check before migration.
9. Configure Hive to address the staging root, run `--dry-run`, save the plan
   and health report, then run the local migration.
10. Run doctor, refresh, search, and context checks against staging.
11. Dry-run the remote publication and inspect every create, rename, update,
    and delete. Use rclone's explicit case-fixing behavior where required.
12. Publish all store data except `manifest.toml` and `.migrations/`, using a
    non-overlapping backup directory so replaced or deleted remote objects
    remain recoverable.
13. Re-run provider-authoritative duplicate/collision inventory and verify
    remote data against staging, still excluding the manifest.
14. Publish `manifest.toml` last.
15. Repeat provider-authoritative object-ID and duplicate inventory. Require
    exactly one remote `manifest.toml`, with the expected v2 content hash, and
    no case/normalization collision. Ordinary path-based verification alone is
    insufficient.
16. Verify the complete remote store against staging, excluding only the local
    `.migrations/` journal.
17. Remount on the coordinator, invalidate caches, and run read-only doctor,
    search, and context verification.
18. Remount and verify on Windows, macOS, and Linux before writers resume.

The exact rclone commands, filters, supported rclone version, remote paths,
snapshot method, and expected inventory are generated and reviewed in the
deployment-specific runbook before the live cutover. Private remote names,
paths, and credentials stay out of the public repository. The runbook must use
dry-run for destructive sync, must not use `--ignore-case-sync`, and must
account for Google Drive duplicate-name objects.

The reviewed commands follow rclone's documented
[`sync` deletion and duplicate limitations](https://rclone.org/commands/rclone_sync/),
[`--fix-case` behavior](https://rclone.org/docs/#fix-case), and
[`--backup-dir` recovery semantics](https://rclone.org/docs/#backup-dir-dir).

The remote manifest remains v1 until every canonical data artifact is present
and verified. Publishing it last makes old clients reject the store only after
the data transition has completed. If final provider inventory finds a
duplicate or wrong-hash manifest, writers remain stopped and recovery uses the
reviewed backup procedure. The independent remote backup, not the local
journal, is the rollback boundary after publication.

An unknown offline client may still return later. The schema-aware v2 outbox
guard prevents its old queued destination paths from being published. The
operational upgrade requirement remains because a genuinely old v1 binary
cannot be remotely fenced before it reads the v2 manifest.

## Doctor Behavior

On a v1 store, doctor reports:

- schema migration required;
- count of case-only noncanonical files;
- count of case-only `note_path` values;
- unsupported portable IDs;
- any hard collision or semantic mismatch separately;
- exact staging and dry-run guidance.

It does not report a uniquely matched case-only note as missing.

On a v2 store, any noncanonical inbox path, unsupported ID, portable collision,
or mismatched `note_path` is an error because the manifest asserts that
migration committed successfully. A mismatch is evaluated through the same
typed event-path helper: promotion events are healthy when `note_path` names
the canonical source note identified by `source.ref`, even though their own
event ID differs.

Doctor recognizes active migration journals and reports their phase plus the
exact resume or rollback command. `hm doctor --fix` does not perform or resume
the schema migration. On a direct rclone/FUSE store, doctor is read-only and
points migration requests to the staging runbook.

## Errors and Safety

- Migration failures use structured reason codes; control flow never parses
  display text.
- No migration diagnostic prints note bodies, event bodies, or secret-detector
  matches.
- Opaque observed paths prevent mutation outside the configured store root.
- Symlinked canonical inbox files or directories make preflight fail.
- All migration operations use hash/identity preconditions and exact
  enumerated targets.
- `rename_noreplace` prevents overwriting an unexpected file.
- Migration never deletes canonical content; rollback removes only the exact
  planned audit artifact.
- Conflict copies and provider duplicates block migration/publication until the
  operator resolves them from authoritative inventory.
- Local index and full-text search caches are rebuildable and invalidated, not
  migrated.
- Migration completion invalidates hook/context caches and classifier stamps.

## Testing

### Unit tests

- canonical path derivation preserves exact ID spelling;
- canonical serialization is independent of `PathCase`;
- different absolute Windows, UNC, macOS, and Linux roots produce identical
  store-relative canonical paths and portable keys;
- case variants of a configured root on a case-insensitive host do not alter
  any persisted relative path;
- `open_store` binds each local root to the expected manifest `store_id`;
- no persisted artifact or authoritative index field contains the absolute
  host-local root;
- portable ID grammar covers all boundaries and Windows reserved names;
- generated IDs are exactly bounded under maximum and oversized host, agent,
  and deterministic-random inputs, with stable digest suffixes;
- portable keys fold ASCII case without becoming filesystem paths;
- generated write-ID components satisfy the portable grammar;
- `ObservedInboxPath` rejects absolute, parent, non-inbox, and symlink paths;
- the local stale-read guard rejects changed IDs, hashes, identities, or paths
  after the record lock is acquired;
- typed event paths distinguish paired sidecars from promotion source
  references;
- compaction events without `note_path` are accepted, while compaction events
  with `note_path` are rejected consistently by inventory, migration
  preflight, doctor, and verification;
- event patching preserves unknown JSON members;
- event migration rejects duplicate JSON member names;
- migration-capable outbox flush rejects stale v1 destination spelling against
  both v1 and v2 stores;
- two concurrent local classification/retag mutations serialize on the record
  lock, revalidate after acquiring it, and never overwrite stale content.

### CLI contract tests

Table-driven CLI tests cover:

- `--store` being required for every mutating, resume, and rollback command;
- invalid dry-run, resume, and rollback option combinations;
- exact single-store and multi-store dry-run JSON field sets;
- the global structured error envelope and every documented migration reason
  code;
- exit-code mappings 2 through 5;
- dry-run creating no journal and mutating no store;
- one store's dry-run failure not suppressing independent results for other
  configured stores.

### Migration fixtures

- lowercase note with canonical event;
- lowercase note and lowercase event;
- lowercase event `note_path`;
- already-canonical records;
- filenames differing by more than case;
- files that differ only by case;
- duplicate logical IDs;
- unsupported legacy IDs;
- occupied canonical or temporary destination with identical content;
- occupied canonical or temporary destination with different content;
- destination created concurrently before forward or reverse rename;
- conflict-copy filename present during preflight;
- nonempty coordinator outbox during preflight;
- unreadable artifact content during preflight;
- unwritable containing directory during preflight;
- malformed note or event;
- event-only operational history;
- a promotion event whose independent ID differs from its source note ID;
- unknown event fields;
- duplicate JSON object members;
- wrong day partition;
- symlink in a canonical inbox path;
- changed manifest immediately before compare-and-swap;
- changed same-ID note or event immediately before mutation;
- audit absent, expected, and conflicting;
- interruption at every durability barrier in forward and reverse operations;
- truncated and corrupt journal state slots;
- repeated resume and repeated rollback;
- manifest CAS mismatch, interrupted rollback, completed rollback, and a fresh
  successful attempt;
- stale active-attempt pointer recovery and ambiguous-attempt refusal;
- two simultaneous migration starts, with exactly one active winner, one
  `migration_active_attempt` loser, and no orphan nonterminal attempt;
- rerun after successful migration;
- refusal on FUSE/rclone, network, nested bind mounts, per-directory casefold,
  case-insensitive, and otherwise unsupported filesystems.

Each failure-path test gets its own case rather than being bundled into the
happy path. Every preflight-refusal fixture asserts that no journal was created,
no canonical artifact changed, and the documented structured reason was
returned.

### End-to-end behavior

After migration:

- doctor reports no pairing or canonical-path errors;
- refresh preserves exact note and preexisting event ID sets;
- the sole new event is the planned migration audit;
- unchanged files preserve byte hashes;
- rewritten events differ semantically only in `note_path`;
- promotion provenance retains its event ID, `source.ref`, canonical source-note
  path, and curated target;
- search and context return the same logical memories;
- classification and retagging preserve exact filename spelling;
- a normal outbox flush creates exact canonical note/event pairs;
- an offline v1 outbox returning after commit is refused safely;
- a portability collision is rejected before mutation;
- older manifest support rejects schema 2;
- cache rebuilds preserve exact observed paths;
- relocating the same store to a differently spelled absolute root changes no
  canonical artifact or logical identity.

### Access-gate matrix

Table-driven tests exercise every CLI and background command family against:

- no journal;
- every nonterminal forward phase;
- `rolling-back`;
- `rolled-back`;
- `committed`;
- schema 1 and schema 2 manifests.

Only `MigrationControl`, `DoctorReadOnly`, and `ConfigStatus` receive their
documented exceptions.

### Platform and backend matrix

Canonical path, collision, access-gate, outbox, index, classification, and
retagging tests run on Linux, Windows, and macOS using default filesystem
behavior where available.

Verification has four named ownership tiers:

- required hosted CI: pure capability-detector tests, all deterministic
  migration state fixtures, and Linux/Windows/macOS local-filesystem behavior;
- required release gate: a self-hosted Linux runner on the supported durable
  migration filesystem, including real process termination at every
  write-ahead barrier;
- environment acceptance: actual rclone/FUSE, network, nested-mount, and
  casefold roots proving refusal;
- live-cutover verification: provider-authoritative inventory, backup,
  publication, and post-publication checks from the reviewed deployment
  runbook.

An unavailable environment reports `unverifiable` with the missing capability;
it is never represented as a passing skip. Release criteria name which
environment-acceptance results are required for the backends supported by that
release.

The cloud-sync simulation covers:

- remote duplicate-name inventory blocking publication;
- case-only publication with explicit case fixing;
- data-before-manifest order;
- failed verification before the manifest boundary;
- duplicate or conflicting manifest objects created by final publication;
- stale mounted-directory state;
- delayed writeback detection;
- index invalidation and absence of duplicate canonical objects.

The hot read path gains no additional directory traversal. Exact observed paths,
hashes, and portable keys are computed during the index's existing scan.
Content hashes are rechecked only at mutation boundaries, and warm
context/search performance remains within current budgets.

## Documentation and Rollout

Implementation updates:

- `SPEC.md` path normalization, portable ID, schema migration, writer, outbox,
  doctor, and testing contracts;
- README configuration guidance and migration runbook;
- CLI help for migration, resume, and rollback;
- deployment runbook template for reviewed rclone inventory, staging, backup,
  publication, and verification commands;
- changelog with the required coordinated fleet cutover;
- release notes that tell operators to upgrade all clients before migration.

Live stores are migrated only after the implementation is released and
installed on every participating client. Repository tests use synthetic stores;
development must not mutate a live store.

## Acceptance Criteria

The work is complete when:

- store manifest schema 2 enforces the canonical path and portable ID contract;
- the same `store_id` remains portable across unrelated host-local absolute
  roots without changing stored relative paths;
- v1 stores receive a complete dry-run plan before mutation;
- migration refuses direct cloud/FUSE and unsupported local filesystems;
- every mutation follows the durable write-ahead protocol;
- migration and rollback are idempotently resumable after every tested crash;
- exact preexisting note/event inventories and unrelated semantics are
  preserved;
- promotion events preserve their independent ID and canonical source-note
  provenance;
- indexes never reuse comparison-normalized or untrusted cache paths for
  mutation;
- stale v1 outboxes cannot publish noncanonical paths to a v2 store;
- remote publication keeps manifest v1 until canonical data verifies complete;
- final provider inventory proves exactly one expected-hash v2 manifest;
- all default and new tests pass on the supported platform/backend matrix;
- a reviewed deployment cutover succeeds on the target store;
- `hm doctor` reports zero path-pairing warnings after fleet synchronization.
