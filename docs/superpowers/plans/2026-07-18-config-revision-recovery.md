# Revisioned Config Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the ambiguous v2 config recovery behavior with a v3 revisioned two-snapshot protocol that remains consistent across Windows replacement failures, process crashes, credential changes, and legacy migration.

**Architecture:** `storage/recovery.rs` owns snapshot parsing, candidate selection, durable recovery commit, and non-destructive main promotion. `storage.rs` owns the serialized configuration transaction, assigns monotonic revisions, and implements pending-first credential workflows. A synced recovery snapshot is the logical commit point; main promotion is maintenance and cannot reverse a committed command result.

**Tech Stack:** Rust 2024, Tokio, Serde JSON, UUID, `windows-sys` `MoveFileExW`, tempfile, existing `CredentialStore` and storage contract fixtures.

---

## File Map

- Create `frontend/src-tauri/src/storage/recovery.rs`: candidate parsing/selection, v1/v2/v3 recovery rules, durable recovery commit, main promotion, platform replacement barriers, fault-injection seams.
- Modify `frontend/src-tauri/src/storage.rs`: v3 fields and migration, monotonic revision allocation, authoritative in-memory swap, credential prepare/activate ordering, active-reference normalization, removal of the old compensation/recovery protocol.
- Modify `frontend/src-tauri/tests/storage_contracts.rs`: public-behavior and real-reopen tests for migration, crash recovery, credential intent, post-commit results, concurrency, and redaction.
- Keep the existing `windows-sys` `Win32_Storage_FileSystem` dependency unchanged. Do not add a database, async logger, or general transaction framework.
- Modify `docs/superpowers/plans/2026-07-12-tauri-rust-windows-client.md`: record that this recovery plan supersedes Task 3's simple atomic-save algorithm before Task 8 proceeds.

## Task 1: Parse and Select Versioned Snapshot Candidates

**Files:**
- Create: `frontend/src-tauri/src/storage/recovery.rs`
- Modify: `frontend/src-tauri/src/storage.rs`
- Test: `frontend/src-tauri/src/storage/recovery.rs`

- [ ] **Step 1: Add v3 metadata to the private configuration model**

Add serde-defaulted storage metadata without changing command/frontend models:

```rust
const CONFIG_VERSION: &str = "3.0";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ConfigData {
    version: String,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    transaction_id: String,
    settings: AppSettings,
    #[serde(default)]
    mirrors: Vec<StoredMirror>,
    #[serde(default)]
    history: Vec<HistoryItem>,
    #[serde(default)]
    pending_credential_cleanup: Vec<String>,
}
```

Update existing test fixtures to specify revision/transaction fields only where construction requires it. Do not change behavior yet.

- [ ] **Step 2: Write failing candidate-selection tests**

In `storage/recovery.rs`, define tests before selection code. Cover at least these exact assertions:

```rust
#[test]
fn newer_recovery_wins_over_older_main() {
    let selected = select_candidates(v3_main(41, "main"), v3_recovery(42, "recovery")).unwrap();
    assert_eq!(selected.source, SnapshotSource::Recovery);
    assert_eq!(selected.data.revision, 42);
}

#[test]
fn equal_revision_with_different_transaction_is_corrupt() {
    let error = select_candidates(v3_main(42, "a"), v3_recovery(42, "b")).unwrap_err();
    assert!(matches!(error, AppError::ConfigCorrupted));
}

#[test]
fn same_transaction_cannot_span_revisions() {
    let error = select_candidates(v3_main(41, "same"), v3_recovery(42, "same")).unwrap_err();
    assert!(matches!(error, AppError::ConfigCorrupted));
}

#[test]
fn different_valid_v2_candidates_are_ambiguous() {
    let error = select_candidates(v2_main("amd64"), v2_recovery("arm64")).unwrap_err();
    assert!(matches!(error, AppError::ConfigCorrupted));
}

#[test]
fn migration_recovery_v3_wins_over_legacy_main() {
    let selected = select_candidates(v2_main("amd64"), v3_recovery(1, "migration")).unwrap();
    assert_eq!(selected.source, SnapshotSource::Recovery);
}
```

Also cover: main only; recovery only; main newer than stale recovery; equal revision/transaction/content; equal transaction with different normalized content; v3 main plus v2 recovery; v3 main plus v1 recovery; valid main plus corrupt recovery; corrupt main plus valid recovery; mixed v1/v2 candidates.

- [ ] **Step 3: Run the tests and confirm RED**

Run:

```powershell
cargo test --offline --manifest-path frontend/src-tauri/Cargo.toml --lib storage::recovery -- --nocapture
```

Expected: compilation fails because `select_candidates`, `Candidate`, `SelectedSnapshot`, and `SnapshotSource` do not exist.

- [ ] **Step 4: Implement candidate parsing and pure selection**

Create these module-local contracts:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SnapshotSource { Main, Recovery, Defaults }

pub(super) struct SelectedSnapshot {
    pub data: ConfigData,
    pub bytes: Vec<u8>,
    pub source: SnapshotSource,
    pub needs_v3_rewrite: bool,
}

pub(super) fn load_and_select(path: &Path) -> StorageResult<SelectedSnapshot>;
```

Parse both files before changing either. Compare canonical `serde_json::Value` values produced after `normalize_config`, not Rust derived equality or JSON formatting. Enforce the complete matrix in design section 4 and migration section 6. Preserve conflicting files and return `ConfigCorrupted`; do not use timestamps.

- [ ] **Step 5: Verify GREEN and commit**

Run:

```powershell
cargo fmt --manifest-path frontend/src-tauri/Cargo.toml -- --check
cargo test --offline --manifest-path frontend/src-tauri/Cargo.toml --lib storage::recovery -- --nocapture
```

Expected: all recovery selection tests pass.

Commit:

```powershell
git add frontend/src-tauri/src/storage.rs frontend/src-tauri/src/storage/recovery.rs
git commit -m "feat: select revisioned config snapshots"
```

## Task 2: Commit Recovery Durably and Promote Main Non-Destructively

**Files:**
- Modify: `frontend/src-tauri/src/error.rs`
- Modify: `frontend/src-tauri/src/storage/recovery.rs`
- Test: `frontend/src-tauri/tests/domain_contracts.rs`
- Test: `frontend/src-tauri/src/storage/recovery.rs`

- [ ] **Step 1: Write failing durability-boundary tests**

Use a small module-local `SnapshotFs` trait with a real implementation and deterministic failing test implementation. Write tests for each boundary:

```rust
#[test]
fn recovery_barrier_failure_is_indeterminate_after_replacement() {
    let fs = FailingSnapshotFs::fail_at(FaultPoint::RecoveryBarrier);
    let result = commit_recovery_with(&fs, paths(), snapshot_bytes(42));
    assert!(matches!(result, Err(AppError::ConfigCommitIndeterminate)));
    assert_eq!(fs.recovery_revision(), Some(42));
}

#[test]
fn failed_main_promotion_preserves_committed_recovery() {
    let fs = FailingSnapshotFs::fail_at(FaultPoint::MainReplace);
    commit_recovery_with(&fs, paths(), snapshot_bytes(42)).unwrap();
    assert!(promote_main_with(&fs, paths(), snapshot_bytes(42)).is_err());
    assert_eq!(fs.recovery_revision(), Some(42));
}

#[test]
fn recovery_is_deleted_only_after_main_is_durable_and_verified() {
    let fs = FailingSnapshotFs::fail_at(FaultPoint::MainVerify);
    commit_recovery_with(&fs, paths(), snapshot_bytes(42)).unwrap();
    assert!(promote_main_with(&fs, paths(), snapshot_bytes(42)).is_err());
    assert_eq!(fs.recovery_revision(), Some(42));
}
```

Add `SnapshotFs` faults for recovery parent-directory and staging-file creation. Add real-tempdir tests that use parent/file/directory conflicts, without sleeps, for recovery staging creation failure, recovery replacement failure, main path absent/directory/regular, redundant equal recovery, and restart after promotion failure. Every pre-replacement failure preserves the previous recovery bytes; regular `.recovery.tmp`/`.tmp` files are absent or safely overwritten on retry, while externally created non-file conflicts are preserved until the test explicitly resolves them. Add a real-filesystem barrier-failure test proving the returned error is `CONFIG_COMMIT_INDETERMINATE` even though a real reopen can select the newly installed revision.

Add a `CommandError` contract test for `CONFIG_COMMIT_INDETERMINATE`: `retryable` is `false` and `details` is empty.

- [ ] **Step 2: Run the tests and confirm RED**

Run:

```powershell
cargo test --offline --manifest-path frontend/src-tauri/Cargo.toml --lib storage::recovery::tests -- --nocapture
```

Expected: fails because the indeterminate error contract and the new creation fault points are missing, and because recovery barrier failure is still mapped to `CONFIG_IO_FAILED`.

- [ ] **Step 3: Implement the exact commit point**

Implement these operations in `storage/recovery.rs`:

```rust
pub(super) fn commit_recovery(path: &Path, bytes: &[u8]) -> StorageResult<()>;
pub(super) fn promote_main(path: &Path, bytes: &[u8]) -> StorageResult<()>;
```

`commit_recovery` writes `config.json.recovery.tmp`, flushes and syncs it, atomically replaces `config.json.recovery`, then performs the post-replacement durability barrier. `promote_main` writes independent `config.json.tmp` bytes, syncs and replaces main, performs the barrier, parses/verifies main, then removes recovery. It never renames or moves recovery to main.

Failures before recovery replacement return `CONFIG_IO_FAILED`. Once replacement has installed the new recovery bytes, a barrier failure returns non-retryable `CONFIG_COMMIT_INDETERMINATE` with no details. Do not convert this window into success, ordinary I/O failure, or a claimed rollback: the next real reopen may legitimately select the new revision.

For Windows, keep the existing scoped dependency and call:

```rust
MoveFileExW(
    source.as_ptr(),
    destination.as_ptr(),
    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
)
```

For non-Windows, rename then sync the parent directory. Keep all blocking filesystem calls inside `spawn_blocking` at async call sites.

- [ ] **Step 4: Verify GREEN, platform code, and commit**

Run:

```powershell
cargo fmt --manifest-path frontend/src-tauri/Cargo.toml -- --check
cargo test --offline --manifest-path frontend/src-tauri/Cargo.toml --lib storage::recovery -- --nocapture
cargo clippy --offline --manifest-path frontend/src-tauri/Cargo.toml --lib -- -D warnings
```

Expected: recovery tests pass; Clippy has no new warning in the recovery module. If unrelated existing warnings still block `-D warnings`, run a module-targeted lint and record the baseline exactly.

Commit:

```powershell
git add frontend/src-tauri/src/error.rs frontend/src-tauri/src/storage/recovery.rs frontend/src-tauri/tests/domain_contracts.rs docs/superpowers/specs/2026-07-18-config-revision-recovery-design.md docs/superpowers/plans/2026-07-18-config-revision-recovery.md
git commit -m "fix: report indeterminate recovery commits"
```

## Task 3: Integrate Monotonic Revisions into ConfigStore

**Files:**
- Modify: `frontend/src-tauri/src/storage.rs`
- Modify: `frontend/src-tauri/tests/storage_contracts.rs`

- [ ] **Step 1: Write failing public-contract tests**

Add real reopen tests that invoke `ConfigStore`, not recovery helpers:

```rust
#[tokio::test]
async fn every_committed_mutation_increments_revision() {
    let (store, path) = store_with_path().await;
    let initial = read_revision(&path).await;
    store.update_settings(SettingsPatch { default_arch: Some("arm64".into()), ..Default::default() }).await.unwrap();
    let after_settings = read_authoritative_revision(&path).await;
    store.append_history(history("one")).await.unwrap();
    let after_history = read_authoritative_revision(&path).await;
    assert_eq!(after_settings, initial + 1);
    assert_eq!(after_history, after_settings + 1);
}

#[tokio::test]
async fn committed_recovery_is_returned_when_main_promotion_fails() {
    let fixture = fail_main_promotion_after_recovery_commit().await;
    let settings = fixture.store.update_settings(arm64_patch()).await.unwrap();
    assert_eq!(settings.default_arch, "arm64");
    drop(fixture.store);
    assert_eq!(reopen(&fixture.path).await.settings().await.default_arch, "arm64");
}
```

Also test strictly increasing concurrent settings/mirror/history mutations, transaction-ID uniqueness, v1-to-v3 and v2-to-v3 migration, legacy-main/v3-recovery restart, conflicting v2 candidates, corrupt recovery preservation, and revision overflow.

- [ ] **Step 2: Run tests and confirm RED**

Run:

```powershell
cargo test --offline --manifest-path frontend/src-tauri/Cargo.toml --test storage_contracts revision -- --nocapture
cargo test --offline --manifest-path frontend/src-tauri/Cargo.toml --test storage_contracts migration -- --nocapture
```

Expected: fails because the existing store writes version 2.0 directly to main and has no revision contract.

- [ ] **Step 3: Replace `persist_and_swap_raw` with one revision commit path**

All config mutations must call one internal function:

```rust
async fn commit_next(&self, mut next: ConfigData) -> StorageResult<ConfigData> {
    let current = self.inner.data.read().await.clone();
    next.version = CONFIG_VERSION.into();
    next.revision = current.revision.checked_add(1).ok_or(AppError::ConfigCorrupted)?;
    next.transaction_id = Uuid::new_v4().to_string();
    let bytes = serde_json::to_vec_pretty(&next).map_err(|_| AppError::ConfigIoFailed)?;
    recovery::commit_recovery_async(&self.inner.path, bytes.clone()).await?;
    *self.inner.data.write().await = next.clone();
    if recovery::promote_main_async(&self.inner.path, bytes).await.is_err() {
        self.record_maintenance_warning(StorageMaintenanceWarning::MainPromotionDeferred).await;
    }
    Ok(next)
}
```

No fallible work may occur between the durable recovery commit and the in-memory swap; acquiring the in-memory write guard is allowed because it cannot return an application error. A promotion failure does not change the successful command result.

`commit_next` must handle `CONFIG_COMMIT_INDETERMINATE` separately from ordinary pre-commit failures. Before returning it, atomically poison the current `ConfigStore` instance. Every later mutation on that instance returns `CONFIG_COMMIT_INDETERMINATE` without writing anything; only dropping and reopening the store clears the poison by running candidate selection against disk. The original command remains an error even if reopen selects its newly installed revision. Add a real-reopen contract test for this sequence and a no-further-writes test for the poisoned instance.

Add a bounded internal warning queue:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageMaintenanceWarning {
    MainPromotionDeferred,
    CredentialCleanupDeferred,
}

// ConfigStoreInner
maintenance_warnings: Mutex<Vec<StorageMaintenanceWarning>>,
```

Cap it at 32 entries, discard the oldest entry on overflow, and expose `pub async fn take_maintenance_warnings(&self) -> Vec<StorageMaintenanceWarning>` for the later Tauri command/event layer. Tests assert enum values only; warnings contain no paths, usernames, credential references, or payloads.

`ConfigStore::open` loads `recovery::load_and_select`, performs required migration/normalization by committing a new revision, swaps the selected authoritative snapshot, then attempts promotion. Remove `atomic_save`, `recover_committed_config`, `promote_committed_recovery`, and boolean compensation paths made obsolete by the single commit point.

- [ ] **Step 4: Normalize active credential references before cleanup**

Before calling credential deletion:

```rust
let active: HashSet<_> = snapshot.mirrors.iter()
    .filter_map(|mirror| mirror.credential_ref.clone())
    .collect();
let mut normalized = snapshot.clone();
normalized.pending_credential_cleanup.retain(|reference| !active.contains(reference));
if normalized.pending_credential_cleanup != snapshot.pending_credential_cleanup {
    snapshot = self.commit_next(normalized).await?;
}
```

The cleanup loop must repeat the active-set guard immediately before every delete. Add a real reopen test where an active reference is present in pending and assert its password remains retrievable.

- [ ] **Step 5: Verify GREEN and commit**

Run:

```powershell
cargo test --offline --manifest-path frontend/src-tauri/Cargo.toml --test storage_contracts -- --nocapture
cargo test --offline --manifest-path frontend/src-tauri/Cargo.toml --lib storage -- --nocapture
cargo fmt --manifest-path frontend/src-tauri/Cargo.toml -- --check
```

Expected: migration, revision, concurrency, reopen, and existing storage contracts pass.

Commit:

```powershell
git add frontend/src-tauri/src/storage.rs frontend/src-tauri/tests/storage_contracts.rs
git commit -m "feat: commit monotonic config revisions"
```

## Task 4: Make Credential Changes Pending-First

**Files:**
- Modify: `frontend/src-tauri/src/storage.rs`
- Modify: `frontend/src-tauri/tests/storage_contracts.rs`

- [ ] **Step 1: Write failing crash and post-commit tests**

Add deterministic tests with the existing `FailingCredentialStore`:

```rust
#[tokio::test]
async fn crash_after_new_credential_write_keeps_durable_cleanup_intent() {
    let fixture = crash_after_credential_set_before_activation().await;
    let raw = read_authoritative_json(&fixture.path).await;
    assert_eq!(raw["pendingCredentialCleanup"], json!([fixture.new_ref]));
    assert!(!json_active_refs(&raw).contains(&fixture.new_ref));
    reopen(&fixture.path, fixture.credentials.clone()).await.unwrap();
    assert!(!fixture.credentials.values().await.contains_key(&fixture.new_ref));
}

#[tokio::test]
async fn old_credential_delete_failure_does_not_reverse_committed_update() {
    let fixture = fail_old_delete().await;
    let updated = fixture.store.update_mirror(&fixture.id, fixture.patch).await.unwrap();
    assert_eq!(updated.username.as_deref(), Some("bob"));
    assert!(read_pending(&fixture.path).await.contains(&fixture.old_ref));
    assert_eq!(fixture.store.take_maintenance_warnings().await, vec![StorageMaintenanceWarning::CredentialCleanupDeferred]);
}

#[tokio::test]
async fn active_reference_is_never_deleted_when_also_pending() {
    let fixture = reopen_with_active_ref_in_pending().await;
    assert_eq!(fixture.store.mirror(&fixture.id).await.unwrap().password.as_deref(), Some("secret"));
    assert!(!read_pending(&fixture.path).await.contains(&fixture.active_ref));
}
```

Cover add authenticated mirror, update username/password, registry change, credential removal, mirror deletion, credential set failure before activation, activation commit failure, crash after set, old delete failure, successful delete plus pending-clear commit failure, and real reopen.

- [ ] **Step 2: Run tests and confirm RED**

Run:

```powershell
cargo test --offline --manifest-path frontend/src-tauri/Cargo.toml --test storage_contracts credential -- --nocapture
```

Expected: existing implementation writes the credential before durable discoverability or converts post-commit cleanup errors into command failures.

- [ ] **Step 3: Implement prepare, activate, and cleanup phases**

Under one transaction mutex for the whole command:

```rust
async fn prepare_credential_ref(&self, reference: &str) -> StorageResult<()> {
    let mut prepared = self.inner.data.read().await.clone();
    enqueue_cleanup(&mut prepared, reference);
    self.commit_next(prepared).await?;
    Ok(())
}
```

Then set the credential. Activation commits one revision that removes the new active ref from pending, changes/adds the mirror, and enqueues the old inactive ref. Never overwrite an active credential reference in place; allocate a UUID-based reference for every password/username credential change.

Mirror deletion commits mirror removal plus pending old ref before deleting the credential. Cleanup deletion and pending removal are post-commit maintenance. Their failure retains pending and returns the already committed mirror/settings result.

Remove `RegistryUpdateCompensation`, `InactiveCredentialCompensation`, old config rollback restoration, and any path that deletes an old credential before its inactive reference is durably pending.

- [ ] **Step 4: Verify GREEN, secrecy, and commit**

Run:

```powershell
cargo test --offline --manifest-path frontend/src-tauri/Cargo.toml --test storage_contracts credential -- --nocapture
cargo test --offline --manifest-path frontend/src-tauri/Cargo.toml --test storage_contracts -- --nocapture
cargo test --offline --manifest-path frontend/src-tauri/Cargo.toml --lib storage -- --nocapture
cargo fmt --manifest-path frontend/src-tauri/Cargo.toml -- --check
```

Expected: all credential crash/reopen tests pass; raw JSON contains no password; active refs never remain pending.

Commit:

```powershell
git add frontend/src-tauri/src/storage.rs frontend/src-tauri/tests/storage_contracts.rs
git commit -m "fix: make credential changes crash discoverable"
```

## Task 5: Remove Superseded Recovery Paths and Restore the Rust Baseline

**Files:**
- Modify: `frontend/src-tauri/src/storage.rs`
- Modify: `frontend/src-tauri/src/storage/recovery.rs`
- Modify: `frontend/src-tauri/tests/storage_contracts.rs`
- Modify: `docs/superpowers/plans/2026-07-12-tauri-rust-windows-client.md`

- [ ] **Step 1: Add source-contract tests for one persistence protocol**

Add a focused test that rejects reintroduction of the retired helpers and direct main writes:

```rust
#[test]
fn config_store_has_one_revisioned_commit_protocol() {
    let source = include_str!("../src/storage.rs");
    assert!(!source.contains("recover_committed_config"));
    assert!(!source.contains("promote_committed_recovery"));
    assert!(!source.contains("RegistryUpdateCompensation"));
    assert!(source.contains("commit_next"));
}
```

This is a narrow architecture guard, not a substitute for behavior tests.

- [ ] **Step 2: Run the guard and confirm RED before cleanup**

Run:

```powershell
cargo test --offline --manifest-path frontend/src-tauri/Cargo.toml --test storage_contracts config_store_has_one_revisioned_commit_protocol -- --exact --nocapture
```

Expected: fails while old helper/state-machine names remain.

- [ ] **Step 3: Remove dead code and update the parent plan**

Delete only helpers, enums, imports, and tests superseded by Tasks 1-4. Preserve mirror CRUD, history cap/fallback, corruption backups, and credential redaction. Add this note before Task 8 in the parent plan:

```markdown
> Storage recovery prerequisite: execute and verify
> `docs/superpowers/plans/2026-07-18-config-revision-recovery.md` before Task 8.
```

- [ ] **Step 4: Run all storage and Rust gates**

Run:

```powershell
cargo fmt --manifest-path frontend/src-tauri/Cargo.toml -- --check
cargo test --offline --manifest-path frontend/src-tauri/Cargo.toml --test storage_contracts -- --nocapture
cargo test --offline --manifest-path frontend/src-tauri/Cargo.toml --all-targets
cargo clippy --offline --manifest-path frontend/src-tauri/Cargo.toml --all-targets -- -D warnings
git diff --check
```

Expected: storage contracts and all Rust tests pass. Strict Clippy must pass before Task 8; fix only concrete reported warnings with behavior-preserving edits and targeted tests where behavior could change.

- [ ] **Step 5: Commit**

```powershell
git add frontend/src-tauri/src/storage.rs frontend/src-tauri/src/storage/recovery.rs frontend/src-tauri/tests/storage_contracts.rs docs/superpowers/plans/2026-07-12-tauri-rust-windows-client.md
git commit -m "refactor: retire ambiguous config recovery"
```

## Completion Evidence

- [ ] Every configuration commit has version 3.0, a strictly increasing revision, and a fresh transaction ID.
- [ ] A synced recovery snapshot is the only logical commit point; promotion cannot consume it early.
- [ ] Recovery barrier failure is non-retryable `CONFIG_COMMIT_INDETERMINATE`; the current store is poisoned until reopen, and real reopen may select the installed revision.
- [ ] Candidate conflicts never use timestamps or implicit path priority.
- [ ] V1/v2 migration crash states reopen deterministically or fail explicitly without deleting evidence.
- [ ] New credential bytes are never written before a durable cleanup reference exists.
- [ ] Old credentials are never deleted before their inactive reference is durably pending.
- [ ] Post-commit maintenance failures return the committed command result.
- [ ] Active credential references are normalized out of pending before cleanup.
- [ ] Raw JSON contains no password, token, or authorization value.
- [ ] `storage_contracts`, all Rust targets, formatting, strict Clippy, and diff checks pass.
