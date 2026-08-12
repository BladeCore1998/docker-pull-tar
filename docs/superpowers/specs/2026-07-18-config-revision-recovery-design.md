# Revisioned Config Recovery Design

**Status:** Approved approach A on 2026-07-18; written specification ready for user review.

## 1. Purpose

The client stores settings, mirrors, credential references, pending credential cleanup, and history in one JSON configuration. A Windows file lock or interrupted replacement can leave both `config.json` and `config.json.recovery` present. The current files do not carry generation information, so startup cannot determine which valid file is authoritative.

This design keeps the lightweight JSON store and adds an explicit generation protocol. It does not introduce SQLite, a background service, or a general-purpose transaction journal.

## 2. Persisted Envelope

The configuration schema becomes `version: "3.0"` and adds private storage metadata:

```json
{
  "version": "3.0",
  "revision": 42,
  "transactionId": "5db6d59c-52ff-4d82-a293-92a138eb1211",
  "settings": {},
  "mirrors": [],
  "history": [],
  "pendingCredentialCleanup": []
}
```

- `revision` is a monotonically increasing unsigned 64-bit integer.
- `transactionId` is a fresh UUID for each logical configuration commit.
- Both fields are internal. Tauri commands and frontend types do not expose them.
- Every mutation runs under the existing transaction mutex. It allocates `current.revision + 1` with checked arithmetic and creates one transaction ID.
- All bytes for one revision are serialized once. The same bytes are used for the recovery snapshot and main-file promotion.
- One `transactionId` identifies exactly one revision and one normalized configuration value. Reusing it with another revision or content is corruption.

Normalized content means every parsed version, revision, transaction, settings, mirror, history, and pending-cleanup field after migration defaults and collection normalization are applied; JSON whitespace and object-key order are irrelevant. An overflow, duplicate revision with conflicting transaction identity, or duplicate transaction identity with another revision or normalized content is `CONFIG_CORRUPTED`. The store never guesses between conflicting states.

## 3. Commit Protocol

`config.json.recovery` is a committed redo snapshot, not an untrusted temporary file. `config.json.recovery.tmp` remains an uncommitted staging file.

For revision `N + 1`:

1. Build and validate the complete next `ConfigData` while holding the transaction mutex.
2. Serialize it once to bytes.
3. Write the bytes to `config.json.recovery.tmp` in the same directory.
4. Flush and `sync_all` the staging file.
5. Atomically replace `config.json.recovery` with the staging file.
6. Persist the replacement's directory entry. Unix-like targets sync the parent directory after replacement. Windows uses `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH` and verifies the resulting recovery path before success.
7. **Commit point:** after step 6 succeeds, revision `N + 1` is durable and authoritative.
8. Swap the in-memory snapshot to revision `N + 1`.
9. Attempt to promote the committed recovery bytes without consuming their only authoritative copy: write the same bytes to `config.json.tmp`, flush and sync it, atomically replace `config.json`, persist the main directory entry, and verify the main candidate.
10. Delete recovery only after the main candidate is durable and verified. A deletion failure may leave two identical snapshots and is safe.

Failure before recovery replacement returns `CONFIG_IO_FAILED` and leaves the in-memory revision unchanged. If recovery replacement installs the new bytes but its post-replacement durability barrier fails, the result is `CONFIG_COMMIT_INDETERMINATE`: the caller cannot prove whether revision `N + 1` survived durably, must not report success or attempt to pretend the old recovery was restored, and a real reopen may select revision `N + 1`. Credential preparation is compensated as described in section 5.

Failure after the commit point does not roll back the logical transaction. The command returns the committed result, while a redacted maintenance warning records the failed main-file promotion. The recovery snapshot remains authoritative, and startup or a later maintenance attempt may promote it. This rule prevents a command from reporting failure while a later restart loads that same mutation as committed.

The main file is therefore a promoted copy of the latest committed state. The recovery file is authoritative only when it contains a higher committed revision.

## 4. Startup Selection

Startup reads and validates the main and recovery candidates independently before mutating either path.

| Main candidate | Recovery candidate | Result |
| --- | --- | --- |
| Valid, recovery absent | Load main |
| Missing/non-file, recovery valid | Load recovery; attempt promotion |
| Valid `M`, valid `R`, `R.revision > M.revision` | Load recovery; attempt promotion |
| Valid `M`, valid `R`, `M.revision > R.revision` | Main wins; stale recovery must be removed before normal operation |
| Same revision, same transaction ID, same normalized content | Load either; recovery is redundant |
| Same revision with different transaction IDs or content | Return `CONFIG_CORRUPTED`; preserve both files |
| Valid main, corrupt recovery | Return `CONFIG_CORRUPTED`; preserve both files because recovery may represent a newer commit |
| Corrupt main, valid recovery | Back up corrupt main, load recovery, attempt promotion |
| Corrupt main, recovery absent | Preserve existing behavior: back up main, create defaults, surface recovery warning |

If a higher valid recovery cannot be promoted because the main path is locked, the store may open using the recovery snapshot. It must not load the older main snapshot. If a stale lower recovery cannot be removed, opening fails with `CONFIG_IO_FAILED`; silently retaining a lower fallback could roll configuration backward if the main file later disappears.

Promotion is maintenance, not a second commit. It never renames or moves the authoritative recovery file into place. Any failure before the main candidate is durable and verified preserves recovery unchanged. Once main is verified, failure to remove an identical recovery is a redacted maintenance warning; equal snapshots remain unambiguous.

## 5. Credential Transaction Ordering

Passwords remain exclusively in Windows Credential Manager. JSON stores only `credentialRef`.

For a new or changed credential, the reference is made durably discoverable before the secret is written:

1. Allocate a fresh credential reference. Credential changes never overwrite an active reference in place.
2. Commit a preparation revision that leaves the current mirror state unchanged and adds the fresh, inactive reference to `pendingCredentialCleanup`.
3. Write the new credential under that reference.
4. Commit the activation revision that points the mirror to the new reference, removes the new active reference from pending cleanup, and adds any newly inactive old reference to pending cleanup.
5. If credential preparation or the activation commit fails before its commit point, the preparation revision remains authoritative. Startup or the next maintenance pass deletes the inactive new reference idempotently.

The command holds the configuration transaction mutex across preparation, credential write, and activation. An in-process cleanup pass cannot consume the prepared reference between steps 2 and 4. A process crash releases that ownership; startup then treats the still-inactive pending reference as an orphan and cleans it before accepting new mutations.

If the process crashes after the credential write but before activation, the durable preparation revision still identifies the orphan. No password or credential payload is written to JSON.

For an old credential that becomes inactive, cleanup is pending-first:

1. Add the inactive reference to `pendingCredentialCleanup` in the same revision that removes its active reference.
2. Commit that revision before deleting the credential.
3. Attempt deletion.
4. On confirmed deletion, commit a later revision removing the pending entry.
5. If deletion or the cleanup-only commit fails, retain the pending entry. Retrying deletion is idempotent.

This ordering guarantees that a crash cannot permanently lose knowledge of an inactive credential that may still exist.

Once the request's activation/removal revision reaches its commit point, the command returns the committed result. Old-credential deletion and removal of a completed pending entry are post-commit maintenance: their failures emit redacted, retryable maintenance warnings and leave the pending entry durable. They do not convert the committed command result into a credential error. A credential error is returned only when preparation fails before the requested state is committed.

Before any cleanup pass, the store builds the set of active credential references from the selected snapshot. Cleanup never deletes a reference in that set. If `activeCredentialRefs` intersects `pendingCredentialCleanup`, startup first commits a normalization revision that removes active references from pending, then starts cleanup. This safe normalization applies to migrated, manually restored, and current snapshots; a real reopen test must prove the active credential remains usable.

## 6. Migration

- Existing `version: "1.0"` data follows the current password-to-Credential-Manager migration, then becomes version 3.0.
- Existing `version: "2.0"` data preserves settings, mirrors, credential references, pending cleanup, and history.
- A version 1.0 main file has no recovery protocol. It is treated as revision 0 and rewritten as revision 1 after credential migration and normalization.
- A sole valid version 2.0 main or recovery candidate is treated as revision 0.
- If valid version 2.0 main and recovery candidates both exist, they may be selected only when their normalized content is identical. Different valid contents return `CONFIG_CORRUPTED` and preserve both files because version 2.0 has no generation metadata.
- A corrupt version 2.0 recovery alongside a valid main file returns `CONFIG_CORRUPTED` and preserves both candidates. The store does not guess whether the recovery represented a newer commit.
- Mixed version 1.0/version 2.0 candidates, or a version 1.0 recovery candidate, return `CONFIG_CORRUPTED`; these combinations were never produced by a valid legacy protocol.
- A valid version 3.0 recovery alongside a version 1.0 or 2.0 main is the committed migration result and wins. Startup loads it and completes main promotion.
- A valid version 3.0 main alongside a valid version 2.0 recovery wins; the version 2.0 recovery is stale and must be removed before normal mutation. A version 1.0 recovery remains `CONFIG_CORRUPTED` in every combination because no valid version 1.0 protocol created that file.
- A corrupt version 3.0 recovery alongside a legacy main returns `CONFIG_CORRUPTED` and preserves both files.
- The selected legacy state is rewritten as version 3.0 revision 1 with a fresh transaction ID before normal mutations continue. Crashing after this commit point and before promotion is recovered by the mixed legacy-main/v3-recovery rule.
- Migration rollback tests must verify that no password is serialized and no active credential reference enters pending cleanup.

## 7. Error Handling

- `CONFIG_IO_FAILED`: the requested state revision was not committed, or safe removal of a stale lower revision failed. A credential preparation revision may already be durable; it is cleanup intent, not the requested active state.
- `CONFIG_COMMIT_INDETERMINATE`: recovery replacement installed the requested revision, but the post-replacement durability barrier failed, so the commit result cannot be proven. This error is non-retryable and has no details. The command returns this error even when a real reopen subsequently selects the new revision. The current `ConfigStore` instance is poisoned and rejects further mutations until it is dropped and reopened; retrying against the stale in-memory revision could duplicate or overwrite the installed mutation.
- `CONFIG_CORRUPTED`: candidates conflict, a recognized recovery candidate is malformed, revision arithmetic overflows, or a transaction identity is internally inconsistent.
- Credential errors retain their existing stable codes and redaction rules.

Errors and logs may include candidate paths and revision numbers. They must not include passwords, authorization headers, or credential payloads.

## 8. Verification Matrix

Tests use deterministic fault injection at every persistence boundary:

1. Failure creating the recovery parent directory or staging file.
2. Failure during recovery staging write, flush, or sync; the previous recovery bytes remain authoritative and any regular staging file is absent or safely overwritable. An externally created non-file conflict is preserved and must be removed before retry.
3. Failure replacing the previous recovery snapshot.
4. Crash after recovery commit but before in-memory swap.
5. Failure after recovery replacement but before its parent-directory durability barrier returns exactly `CONFIG_COMMIT_INDETERMINATE`; a real reopen may select the newly installed revision, and the current store rejects further mutations until reopen.
6. Windows recovery replacement and promotion with write-through semantics.
7. Failure while copying committed recovery to main staging, replacing main, persisting the main directory entry, or verifying main; recovery remains authoritative in every case.
8. Restart with main older than recovery.
9. Restart with main newer than stale recovery.
10. Equal revision and identical transaction/content.
11. Equal revision with conflicting transaction/content; same transaction with another revision/content.
12. Valid main plus corrupt recovery; corrupt main plus valid recovery.
13. Version 1.0 and 2.0 migration, including mixed legacy versions, v3 main plus v1 recovery, two different valid v2 candidates, and real restart after v1/v2-to-v3 commit but before promotion.
14. Pending-first old-credential deletion across failure and restart.
15. Crash after the new credential write but before activation, followed by real reopen and orphan cleanup.
16. Post-commit deletion/cleanup failure returns the committed result and retains a retryable pending entry.
17. Active credential references present in pending cleanup are normalized before cleanup; real restart proves the active credential remains usable.
18. Concurrent settings, mirror, history, preparation, and cleanup writes proving strictly increasing revisions.

The existing storage contract suite remains mandatory. Real-tempdir tests use deterministic parent/file/directory conflicts rather than timing or sleeps for staging and replacement failures, preserve unknown non-file conflicts, and verify old recovery evidence plus reusable-or-removed regular `.recovery.tmp`/`.tmp` staging files after the conflict is resolved. A real reopen is required for every crash-recovery test; inspecting only the in-memory snapshot is insufficient.

## 9. Non-Goals

- General multi-file ACID transactions.
- SQLite or a database migration in this release.
- Exposing storage revisions to the frontend.
- Automatic guessing or last-write-time selection when snapshots conflict.
