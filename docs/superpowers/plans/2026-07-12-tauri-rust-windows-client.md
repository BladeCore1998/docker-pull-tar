# Tauri/Rust Windows Client Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace Electron and the Python/FastAPI backend with a tested Tauri 2/Rust core while preserving the Vue workflow and configurable mirror sources, then produce a Windows x64 portable `DockerPull.exe`.

**Architecture:** Vue invokes typed Tauri commands and consumes task events. A Rust `TaskManager` owns the single download state machine and coordinates `RegistryClient`, resumable blob storage, Docker archive generation, atomic configuration storage, Windows credentials, and complete history records. No localhost HTTP server, WebSocket, Python process, or bundled Chromium remains.

**Tech Stack:** Vue 3, Pinia, TypeScript, Vite, Element Plus, Tauri 2, Rust 2024, Tokio, reqwest/rustls, serde, sha2, flate2, tar, keyring, tokio-util, wiremock, Vitest.

---

## File Map

Create these focused Rust modules under `frontend/src-tauri/src/`:

- `lib.rs`: Tauri builder, managed state, command registration.
- `main.rs`: Windows executable entry only.
- `commands.rs`: serializable Tauri command boundary.
- `domain.rs`: shared request, settings, mirror, history, task, progress, manifest types.
- `error.rs`: `AppError`, stable error codes, command serialization.
- `image_ref.rs`: image-reference parsing and normalization.
- `storage.rs`: one locked configuration store, schema migration, atomic JSON replacement.
- `credentials.rs`: `CredentialStore` trait and Windows Credential Manager adapter.
- `registry.rs`: challenge parsing, token acquisition, manifest/platform selection.
- `blob.rs`: resumable downloads, Range handling, digest validation, progress callbacks.
- `archive.rs`: layer decoding and Docker-compatible tar creation.
- `task_manager.rs`: single-task lifecycle, cancellation, snapshots, history finalization.
- `events.rs`: Tauri event names and `EventSink` abstraction.

Create or modify these frontend files:

- `frontend/src/api/tauri.ts`: all `invoke` wrappers.
- `frontend/src/types/api.d.ts`: command/event contract matching Rust camelCase payloads.
- `frontend/src/stores/download.ts`: snapshot/event-driven task state.
- `frontend/src/stores/settings.ts`: command-backed settings and configurable mirrors.
- `frontend/src/views/Home.vue`: Tauri directory selection and download handling.
- `frontend/src/views/History.vue`: command-backed history and safe parent-directory opening.
- `frontend/src/views/Settings.vue`: preserve mirror CRUD/default/credentials behavior.
- `frontend/src/components/DownloadProgress.vue`: reactive store refs and new states.

Create tests beside Rust modules using `#[cfg(test)]`, integration fixtures under `frontend/src-tauri/tests/`, and frontend tests under `frontend/src/**/*.spec.ts`.

## Task 1: Establish the Tauri 2 Skeleton and Test Baseline

**Files:**
- Create: `frontend/src-tauri/Cargo.toml`
- Create: `frontend/src-tauri/build.rs`
- Create: `frontend/src-tauri/tauri.conf.json`
- Create: `frontend/src-tauri/capabilities/default.json`
- Create: `frontend/src-tauri/src/main.rs`
- Create: `frontend/src-tauri/src/lib.rs`
- Modify: `frontend/package.json`
- Modify: `frontend/vite.config.mts`
- Modify: `package.json`

- [ ] **Step 1: Add a failing Rust smoke test**

Put this in `frontend/src-tauri/src/lib.rs` before defining `app_name`:

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn exposes_stable_app_name() {
        assert_eq!(super::app_name(), "Docker Pull Tar");
    }
}
```

- [ ] **Step 2: Run the test and confirm the missing symbol**

Run: `cargo test --manifest-path frontend/src-tauri/Cargo.toml exposes_stable_app_name`

Expected: compilation fails because `app_name` is not defined.

- [ ] **Step 3: Create the minimal Tauri crate and portable configuration**

Define `pub fn app_name() -> &'static str { "Docker Pull Tar" }`, a `run()` Tauri builder, and `main.rs` calling `docker_pull_tar_lib::run()`. Configure `beforeDevCommand` as `pnpm dev`, `beforeBuildCommand` as `pnpm build`, `devUrl` as `http://localhost:5173`, `frontendDist` as `../dist`, one 1000x700 non-resizable window, CSP `default-src 'self'; img-src 'self' asset: data:; style-src 'self' 'unsafe-inline'`, and `bundle.active: false`.

Add these dependency groups to `Cargo.toml`:

```toml
[dependencies]
tauri = { version = "2", features = [] }
tauri-plugin-dialog = "2"
tauri-plugin-opener = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"

[build-dependencies]
tauri-build = { version = "2", features = [] }
```

Add scripts `tauri:dev`, `tauri:build`, `test`, and `typecheck` to `frontend/package.json`; root `dev` and `build` delegate to these Tauri scripts.

- [ ] **Step 4: Verify the skeleton**

Run: `cargo test --manifest-path frontend/src-tauri/Cargo.toml`

Expected: the smoke test passes.

Run: `pnpm --filter frontend typecheck`

Expected: existing Vue code type-checks before migration, or every pre-existing failure is recorded before proceeding.

- [ ] **Step 5: Commit**

```bash
git add package.json frontend/package.json frontend/vite.config.mts frontend/src-tauri
git commit -m "build: scaffold Tauri Windows client"
```

## Task 2: Define Domain Contracts, Errors, and Image Parsing

**Files:**
- Create: `frontend/src-tauri/src/domain.rs`
- Create: `frontend/src-tauri/src/error.rs`
- Create: `frontend/src-tauri/src/image_ref.rs`
- Modify: `frontend/src-tauri/src/lib.rs`
- Test: module tests in the three new files

- [ ] **Step 1: Write failing parser and serialization tests**

Cover these exact cases:

```rust
#[test]
fn parses_short_name() {
    let value = parse_image_reference("nginx:1.27", "registry-1.docker.io").unwrap();
    assert_eq!(value.registry, "registry-1.docker.io");
    assert_eq!(value.repository, "library/nginx");
    assert_eq!(value.reference, "1.27");
}

#[test]
fn parses_registry_with_port() {
    let value = parse_image_reference("registry.local:5000/team/app:v2", "ignored").unwrap();
    assert_eq!(value.registry, "registry.local:5000");
    assert_eq!(value.repository, "team/app");
}

#[test]
fn command_error_uses_stable_shape() {
    let json = serde_json::to_value(CommandError::from(AppError::TaskAlreadyRunning)).unwrap();
    assert_eq!(json["code"], "TASK_ALREADY_RUNNING");
    assert_eq!(json["retryable"], false);
}
```

Also test empty input, invalid registry/repository separators, digest references, and `arm/v7` platform serialization.

- [ ] **Step 2: Run targeted tests and verify failure**

Run: `cargo test --manifest-path frontend/src-tauri/Cargo.toml image_ref error domain`

Expected: missing modules/types/functions.

- [ ] **Step 3: Implement stable domain types**

Define `AppSettings`, `SettingsPatch`, `Mirror`, `MirrorInput`, `MirrorPatch`, `DownloadStartRequest`, `DownloadStatus`, `LayerStatus`, `LayerProgress`, `DownloadProgress`, `DownloadSnapshot`, `HistoryItem`, `LogEvent`, `Platform`, and `ImageReference`. Use `#[serde(rename_all = "camelCase")]`; use `Option<T>` only for patches or semantically optional values.

Define `AppError` variants for every code in design section 6.3 and implement conversion to:

```rust
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandError {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
    pub details: BTreeMap<String, String>,
}
```

Implement parsing without regex-based ad hoc URL rewriting: split the first path component as registry only when it contains `.` or `:` or equals `localhost`; split tag on the last colon after registry removal; preserve `@sha256:` digest references; prepend `library/` only for single-component repositories.

- [ ] **Step 4: Run tests and formatting**

Run: `cargo fmt --manifest-path frontend/src-tauri/Cargo.toml -- --check`

Run: `cargo test --manifest-path frontend/src-tauri/Cargo.toml image_ref`

Expected: all parser and error-shape tests pass.

- [ ] **Step 5: Commit**

```bash
git add frontend/src-tauri/src
git commit -m "feat: define Rust domain contracts"
```

## Task 3: Implement Atomic Configuration, Credentials, and Configurable Mirrors

**Files:**
- Create: `frontend/src-tauri/src/storage.rs`
- Create: `frontend/src-tauri/src/credentials.rs`
- Modify: `frontend/src-tauri/src/domain.rs`
- Modify: `frontend/src-tauri/Cargo.toml`
- Test: `frontend/src-tauri/src/storage.rs`

- [ ] **Step 1: Write failing storage and mirror behavior tests**

Use `tempfile::TempDir` and an in-memory credential adapter. Test:

```rust
#[tokio::test]
async fn preserves_fields_during_partial_update() {
    let store = test_store().await;
    store.update_settings(SettingsPatch { default_arch: Some("arm64".into()), ..Default::default() }).await.unwrap();
    let settings = store.settings().await;
    assert_eq!(settings.default_arch, "arm64");
    assert_eq!(settings.download_workers, 4);
}

#[tokio::test]
async fn mirror_crud_keeps_exactly_one_default() { /* add two, set second, delete first, assert second default */ }

#[tokio::test]
async fn credentials_are_not_written_to_json() { /* add authenticated mirror and assert raw password absent */ }

#[tokio::test]
async fn concurrent_updates_do_not_lose_data() { /* join settings patch and mirror add */ }
```

Also test migration of the current `version: "1.0"` JSON, maximum 100 history entries, corrupt-file backup, rejecting deletion of the only/default mirror unless another default is selected, and history redownload fallback to the current default registry.

- [ ] **Step 2: Verify the tests fail**

Run: `cargo test --manifest-path frontend/src-tauri/Cargo.toml storage`

Expected: `ConfigStore` and `CredentialStore` are missing.

- [ ] **Step 3: Implement one locked store and credential abstraction**

Define:

```rust
#[async_trait::async_trait]
pub trait CredentialStore: Send + Sync {
    async fn get(&self, mirror_id: &str) -> Result<Option<Credentials>, AppError>;
    async fn set(&self, mirror_id: &str, value: &Credentials) -> Result<(), AppError>;
    async fn delete(&self, mirror_id: &str) -> Result<(), AppError>;
}

pub struct ConfigStore {
    path: PathBuf,
    data: tokio::sync::RwLock<ConfigData>,
    credentials: Arc<dyn CredentialStore>,
}
```

Implement mirror add/edit/delete/default with generated UUID IDs, registry normalization, duplicate registry rejection, and exactly one default. Store `credentialRef` in JSON and use `keyring` on Windows. Atomic save writes `config.json.tmp`, calls `sync_all`, then replaces `config.json`; preserve a `.corrupt-<timestamp>` file on parse failure.

- [ ] **Step 4: Run storage tests**

Run: `cargo test --manifest-path frontend/src-tauri/Cargo.toml storage -- --nocapture`

Expected: migration, mirror CRUD/default, credentials, history cap, and concurrent update tests pass.

- [ ] **Step 5: Commit**

```bash
git add frontend/src-tauri/Cargo.toml frontend/src-tauri/src/domain.rs frontend/src-tauri/src/storage.rs frontend/src-tauri/src/credentials.rs
git commit -m "feat: persist settings and configurable mirrors"
```

## Task 4: Implement Registry Authentication and Platform Selection

**Files:**
- Create: `frontend/src-tauri/src/registry.rs`
- Modify: `frontend/src-tauri/src/domain.rs`
- Modify: `frontend/src-tauri/Cargo.toml`
- Test: `frontend/src-tauri/src/registry.rs`
- Test: `frontend/src-tauri/tests/registry.rs`

- [ ] **Step 1: Write failing challenge and manifest tests**

Test Bearer parameters in arbitrary order, quoted commas, anonymous token, Basic-auth token, Docker Manifest List, OCI Index, `amd64`, `arm64`, and `arm/v7`. Assert missing platform returns `PLATFORM_NOT_FOUND`, 401 returns `REGISTRY_AUTH_FAILED`, 404 returns `REGISTRY_NOT_FOUND`, and an invalid certificate is never accepted.

Use a local `wiremock::MockServer` for HTTP behavior; keep TLS rejection as a `reqwest` client unit/configuration test if the mock server lacks TLS support.

- [ ] **Step 2: Run and verify failure**

Run: `cargo test --manifest-path frontend/src-tauri/Cargo.toml registry`

Expected: `RegistryClient` and challenge parser are absent.

- [ ] **Step 3: Implement the Registry V2 client**

Build one `reqwest::Client` with Rustls, connect/read timeouts, bounded retry for GET/HEAD on 429/5xx, and no invalid-certificate override. Accept these media types:

```rust
const MANIFEST_ACCEPT: &str = concat!(
    "application/vnd.docker.distribution.manifest.v2+json, ",
    "application/vnd.docker.distribution.manifest.list.v2+json, ",
    "application/vnd.oci.image.manifest.v1+json, ",
    "application/vnd.oci.image.index.v1+json"
);
```

Return a `ResolvedImage` containing the exact manifest digest, config descriptor, ordered layer descriptors, repository, tag/digest reference, and selected platform. Redact authorization values from all errors and logs.

- [ ] **Step 4: Run registry tests**

Run: `cargo test --manifest-path frontend/src-tauri/Cargo.toml registry -- --nocapture`

Expected: all auth, media type, platform, and error mapping tests pass.

- [ ] **Step 5: Commit**

```bash
git add frontend/src-tauri/Cargo.toml frontend/src-tauri/src/domain.rs frontend/src-tauri/src/registry.rs frontend/src-tauri/tests/registry.rs
git commit -m "feat: resolve registry manifests securely"
```

## Task 5: Implement Resumable and Verified Blob Downloads

**Files:**
- Create: `frontend/src-tauri/src/blob.rs`
- Modify: `frontend/src-tauri/src/domain.rs`
- Test: `frontend/src-tauri/src/blob.rs`
- Test: `frontend/src-tauri/tests/resume.rs`

- [ ] **Step 1: Write failing resume tests**

Cover exact scenarios: fresh 200 download, valid 206 append, server ignores Range with 200 and restarts, 416 with already-complete valid file, wrong `Content-Range`, cancellation preserving `.part`, digest mismatch quarantining/removing corrupt content, Manifest Digest change invalidating resume metadata, and progress counting validated pre-existing bytes without adding them to network speed.

The central assertion for append is:

```rust
assert_eq!(received_header("range"), "bytes=5-");
assert_eq!(tokio::fs::read(&completed).await.unwrap(), b"hello world");
assert_eq!(result.resumed_bytes, 5);
```

- [ ] **Step 2: Verify failures**

Run: `cargo test --manifest-path frontend/src-tauri/Cargo.toml resume`

Expected: resume metadata and downloader types are missing.

- [ ] **Step 3: Implement streaming resume and SHA-256 verification**

Define versioned `ResumeMetadata` keyed by normalized image, platform, and manifest digest. Stream response chunks to `*.part`, update SHA-256 incrementally, and check `CancellationToken` between chunks. For resumed files, hash the existing prefix before append. Apply the 200/206/416 rules from design section 8 exactly. Rename a valid completed part to its digest-addressed blob path only after verification.

- [ ] **Step 4: Run resume tests**

Run: `cargo test --manifest-path frontend/src-tauri/Cargo.toml resume -- --nocapture`

Expected: all resume, cancellation, and digest tests pass.

- [ ] **Step 5: Commit**

```bash
git add frontend/src-tauri/src/blob.rs frontend/src-tauri/src/domain.rs frontend/src-tauri/tests/resume.rs
git commit -m "feat: add resumable verified blob downloads"
```

## Task 6: Generate a Docker-Compatible Archive

**Files:**
- Create: `frontend/src-tauri/src/archive.rs`
- Create: `frontend/src-tauri/tests/fixtures/`
- Test: `frontend/src-tauri/src/archive.rs`
- Test: `frontend/src-tauri/tests/archive.rs`

- [ ] **Step 1: Write failing archive tests**

Create tiny deterministic config, gzip layer, and uncompressed layer fixtures. Assert tar entries are relative and include config JSON, `manifest.json`, `repositories`, every `<layer-id>/json`, and every `<layer-id>/layer.tar`. Assert layer order matches Manifest order, parent IDs form a chain, unknown media type returns `UNSUPPORTED_LAYER_MEDIA_TYPE`, cancellation leaves no final tar, and an existing final tar is not overwritten.

- [ ] **Step 2: Verify failure**

Run: `cargo test --manifest-path frontend/src-tauri/Cargo.toml --test archive`

Run: `cargo test --manifest-path frontend/src-tauri/Cargo.toml --lib`

Expected: archive builder is missing.

- [ ] **Step 3: Implement streaming archive creation**

Use `flate2::read::GzDecoder` only for gzip media types and direct copy for uncompressed layers. Reject zstd or unknown media types explicitly in this release. Generate Docker legacy layer IDs deterministically from parent ID and blob digest. Write to a same-directory temporary tar, flush/sync, then atomically rename to a collision-safe final name.

- [ ] **Step 4: Run archive tests**

Run: `cargo test --manifest-path frontend/src-tauri/Cargo.toml --test archive -- --nocapture`

Run: `cargo test --manifest-path frontend/src-tauri/Cargo.toml --lib -- --nocapture`

Run: `cargo test --manifest-path frontend/src-tauri/Cargo.toml`

Expected: all content, path safety, media type, cancellation, and collision tests pass.

- [ ] **Step 5: Commit**

```bash
git add frontend/src-tauri/src/archive.rs frontend/src-tauri/tests
git commit -m "feat: build verified Docker image archives"
```

## Task 7: Build the Single-Source Task Manager and Events

**Files:**
- Create: `frontend/src-tauri/src/events.rs`
- Create: `frontend/src-tauri/src/task_manager.rs`
- Modify: `frontend/src-tauri/src/domain.rs`
- Modify: `frontend/src-tauri/src/storage.rs`
- Test: `frontend/src-tauri/src/task_manager.rs`

- [ ] **Step 1: Write failing state-machine tests**

Use fake registry/blob/archive services and a recording event sink. Test legal state order, atomic rejection of two simultaneous starts, cancellation from preparing/downloading/verifying/packing, panic/error convergence to failed, exactly one history row per terminal state, no completed state before archive commit, and snapshot/event equality.

Define the injection point before implementation:

```rust
#[async_trait::async_trait]
pub trait DownloadPipeline: Send + Sync {
    async fn run(&self, request: DownloadStartRequest, ctx: TaskContext) -> Result<CompletedDownload, AppError>;
}
```

- [ ] **Step 2: Verify failures**

Run: `cargo test --manifest-path frontend/src-tauri/Cargo.toml task_manager`

Expected: manager and event sink types are missing.

- [ ] **Step 3: Implement task ownership and terminal finalization**

Store `Arc<RwLock<DownloadSnapshot>>`, an active task handle, and `CancellationToken` behind one manager mutex. Reserve task ID before spawning. Emit `download://state`, throttled `download://progress`, and sanitized `download://log`. Put terminal transition and history append in one finalizer guarded by task ID so late work cannot alter a newer task.

- [ ] **Step 4: Run task tests**

Run: `cargo test --manifest-path frontend/src-tauri/Cargo.toml task_manager -- --nocapture`

Expected: all concurrency, transition, cancellation, event, and history tests pass.

- [ ] **Step 5: Commit**

```bash
git add frontend/src-tauri/src/domain.rs frontend/src-tauri/src/events.rs frontend/src-tauri/src/task_manager.rs frontend/src-tauri/src/storage.rs
git commit -m "feat: manage downloads with one state machine"
```

> Storage recovery prerequisite: execute and verify
> `docs/superpowers/plans/2026-07-18-config-revision-recovery.md` before Task 8.

## Task 8: Expose Tauri Commands and Minimal Windows Capabilities

**Files:**
- Create: `frontend/src-tauri/src/commands.rs`
- Modify: `frontend/src-tauri/src/lib.rs`
- Modify: `frontend/src-tauri/capabilities/default.json`
- Test: `frontend/src-tauri/src/commands.rs`

- [ ] **Step 1: Write failing command tests**

Call command implementation functions directly with test state. Verify all commands from design section 6.1, especially mirror CRUD/default/credential retrieval, partial settings updates, history clear/delete, task-already-running error shape, cancelled directory dialog returning null, and `path_open_parent` rejecting a nonexistent path.

- [ ] **Step 2: Verify failures**

Run: `cargo test --manifest-path frontend/src-tauri/Cargo.toml commands`

Expected: command functions are missing.

- [ ] **Step 3: Implement and register commands**

Keep each `#[tauri::command]` as a thin adapter returning `Result<T, CommandError>`. Register plugins and commands in `lib.rs`. Capability JSON grants only core window access, dialog directory selection, opener access needed for a validated local path, and the registered commands; do not grant arbitrary shell execution.

- [ ] **Step 4: Run the complete Rust suite**

Run: `cargo fmt --manifest-path frontend/src-tauri/Cargo.toml -- --check`

Run: `cargo clippy --manifest-path frontend/src-tauri/Cargo.toml --all-targets -- -D warnings`

Run: `cargo test --manifest-path frontend/src-tauri/Cargo.toml --all-targets`

Expected: formatting, clippy, and all Rust tests pass.

- [ ] **Step 5: Commit**

```bash
git add frontend/src-tauri/src/commands.rs frontend/src-tauri/src/lib.rs frontend/src-tauri/capabilities/default.json
git commit -m "feat: expose secure Tauri command API"
```

## Task 9: Replace Axios/WebSocket with Typed Tauri APIs

**Files:**
- Create: `frontend/src/api/tauri.ts`
- Create: `frontend/src/api/tauri.spec.ts`
- Modify: `frontend/src/types/api.d.ts`
- Modify: `frontend/package.json`
- Delete later: `frontend/src/api/index.ts`

- [ ] **Step 1: Write failing API wrapper tests**

Mock `@tauri-apps/api/core` and assert exact command names and camelCase payloads. Include mirror operations:

```ts
expect(invoke).toHaveBeenCalledWith('mirror_add', {
  input: { name: 'Private', registry: 'registry.local:5000', username: 'u', password: 'p' }
})
```

Also test `CommandError` propagation without converting it to an Axios-shaped error.

- [ ] **Step 2: Run and verify failure**

Run: `pnpm --filter frontend test -- src/api/tauri.spec.ts`

Expected: module or wrapper functions are missing.

- [ ] **Step 3: Implement typed wrappers**

Export objects `downloadApi`, `settingsApi`, `mirrorsApi`, `historyApi`, and `systemApi` with the same conceptual grouping used today, but return payloads directly rather than `AxiosResponse`. Add `DownloadSnapshot`, `CommandError`, `SettingsPatch`, new statuses `verifying/packing`, layer statuses `verifying/cancelled`, task IDs, and resume fields to TypeScript types.

- [ ] **Step 4: Run frontend API tests and typecheck**

Run: `pnpm --filter frontend test -- src/api/tauri.spec.ts`

Run: `pnpm --filter frontend typecheck`

Expected: wrapper tests pass; remaining type errors point only to stores/views not yet migrated in Tasks 10-11.

- [ ] **Step 5: Commit**

```bash
git add frontend/package.json frontend/src/api/tauri.ts frontend/src/api/tauri.spec.ts frontend/src/types/api.d.ts
git commit -m "feat: add typed Tauri frontend API"
```

## Task 10: Migrate Settings, Mirrors, and History UI

**Files:**
- Modify: `frontend/src/stores/settings.ts`
- Modify: `frontend/src/views/Settings.vue`
- Modify: `frontend/src/views/History.vue`
- Create: `frontend/src/stores/settings.spec.ts`
- Create: `frontend/src/views/History.spec.ts`

- [ ] **Step 1: Write failing configurable-mirror regression tests**

Test loading settings plus mirrors in parallel; adding/editing/deleting a custom mirror; setting default; preserving per-mirror username/password command payloads; rejecting deletion errors without mutating local state; and re-download fallback when a history mirror no longer exists.

The fallback assertion must be:

```ts
expect(router.push).toHaveBeenCalledWith(expect.objectContaining({
  query: expect.objectContaining({ mirror: 'registry-1.docker.io' })
}))
```

- [ ] **Step 2: Verify tests fail against Axios stores**

Run: `pnpm --filter frontend test -- src/stores/settings.spec.ts src/views/History.spec.ts`

Expected: Tauri direct-return API assumptions fail.

- [ ] **Step 3: Migrate store and views**

Replace `res.data` usage with direct command results. Keep all existing mirror controls and add validation for trimmed name/registry. Do not disable Registry address editing unless the Rust command contract forbids it; changing registry must invalidate old credentials only after the update succeeds. Replace `window.electronAPI.openDirectory` with `systemApi.openParent`.

- [ ] **Step 4: Run tests and typecheck**

Run: `pnpm --filter frontend test -- src/stores/settings.spec.ts src/views/History.spec.ts`

Run: `pnpm --filter frontend typecheck`

Expected: mirror and history tests pass; no errors in migrated files.

- [ ] **Step 5: Commit**

```bash
git add frontend/src/stores/settings.ts frontend/src/stores/settings.spec.ts frontend/src/views/Settings.vue frontend/src/views/History.vue frontend/src/views/History.spec.ts
git commit -m "feat: preserve configurable mirror workflows"
```

## Task 11: Migrate Download State and UI to Snapshots and Events

**Files:**
- Modify: `frontend/src/stores/download.ts`
- Modify: `frontend/src/views/Home.vue`
- Modify: `frontend/src/components/DownloadProgress.vue`
- Modify: `frontend/src/components/LogConsole.vue`
- Create: `frontend/src/stores/download.spec.ts`
- Delete: `frontend/src/api/index.ts`
- Delete: `frontend/src/types/global.d.ts` Electron API declarations, replacing with Vite-only declarations if needed

- [ ] **Step 1: Write failing event convergence tests**

Mock Tauri `listen`. Test listeners register before `download_status`, snapshot initialization, current taskId filtering, state event as the only terminal source, progress not generating text logs, cancellation returning cancelled snapshot, max 500 logs, and listener cleanup only on application teardown.

- [ ] **Step 2: Verify failures**

Run: `pnpm --filter frontend test -- src/stores/download.spec.ts`

Expected: current WebSocket/polling store fails the new contract.

- [ ] **Step 3: Implement snapshot/event store and reactive components**

Remove WebSocket fields, reconnection, polling, Axios, and frontend terminal-state inference. Add `initialize()`, `dispose()`, `applySnapshot()`, and event handlers. In components use `storeToRefs(downloadStore)` or computed access rather than setup-time primitive copies. Add visible `verifying` and `packing` labels. Home uses `systemApi.selectDirectory` and displays structured `CommandError.message`.

- [ ] **Step 4: Run the frontend suite**

Run: `pnpm --filter frontend test`

Run: `pnpm --filter frontend typecheck`

Run: `pnpm --filter frontend build`

Expected: all frontend tests, Vue typecheck, and Vite production build pass.

- [ ] **Step 5: Commit**

```bash
git add frontend/src
git commit -m "feat: switch download UI to Tauri events"
```

## Task 12: Remove Legacy Runtimes and Build the Portable Windows EXE

**Files:**
- Delete: `backend/`
- Delete: `backend.spec`
- Delete: `startup.config.json`
- Delete: `frontend/electron/`
- Delete: `frontend/electron-builder.json`
- Modify: `frontend/package.json`
- Modify: `package.json`
- Modify: `pnpm-lock.yaml`
- Modify: `.gitignore`
- Modify: `frontend/src-tauri/Cargo.toml`
- Modify: `README.md`
- Create: `scripts/build-portable.ps1`

- [ ] **Step 1: Add a failing legacy-reference check**

Create `scripts/build-portable.ps1` with a preflight that fails if source/build configuration still references `electron`, `backend.exe`, `FastAPI`, `127.0.0.1:8000`, or `ws://127.0.0.1` outside historical design documents:

```powershell
$legacy = rg -n "electron|backend\.exe|FastAPI|127\.0\.0\.1:8000|ws://127\.0\.0\.1" package.json frontend/src frontend/package.json
if ($LASTEXITCODE -eq 0) { throw "Legacy runtime references remain:`n$legacy" }
```

Run: `powershell -ExecutionPolicy Bypass -File scripts/build-portable.ps1`

Expected: fails and lists current Electron/Python references.

- [ ] **Step 2: Delete the old runtime and dependency chain**

Remove Electron, electron-builder, Axios, concurrently, cross-env, wait-on, Python source, PyInstaller config, and startup config. Keep the historical design document. Update root scripts to Tauri commands and regenerate `pnpm-lock.yaml` with `pnpm install`.

- [ ] **Step 3: Complete the portable build script**

After preflight, define `[profile.release]` with `panic = "unwind"` (required so task-boundary panics converge to `failed`; never use the abort strategy), then run frontend tests/typecheck, Rust fmt/clippy/tests, and `pnpm --filter frontend tauri build --no-bundle`. Copy `frontend/src-tauri/target/release/docker-pull-tar.exe` to `release/portable/DockerPull.exe`. Print executable byte size and SHA-256 using `Get-FileHash`.

- [ ] **Step 4: Update documentation**

README must describe Tauri/Rust architecture, WebView2 prerequisite, configurable mirror sources and per-source authentication, source build prerequisites, portable output path, absence of Docker/Python requirement, true resume behavior, and commands:

```text
pnpm install
pnpm dev
pnpm test
pnpm build
```

- [ ] **Step 5: Run all release gates**

Run: `powershell -ExecutionPolicy Bypass -File scripts/build-portable.ps1`

Expected:

- no legacy reference failure;
- frontend tests and typecheck pass;
- Rust fmt, clippy, and tests pass;
- Tauri release build succeeds;
- `release/portable/DockerPull.exe` exists;
- script prints size and SHA-256.

On a clean Windows test account, launch the EXE and manually verify: no Python/Node/Docker installed, no localhost listener, mirror add/edit/delete/default, per-mirror login, download, cancel, resume, history, redownload fallback, directory selection, and open parent directory.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: ship portable Tauri Windows client"
```

## Final Verification

- [ ] Run `git status --short` and confirm only intentional generated release files remain untracked or ignored.
- [ ] Run `cargo test --manifest-path frontend/src-tauri/Cargo.toml --all-targets` and record the test count.
- [ ] Run `pnpm --filter frontend test` and record the test count.
- [ ] Run `pnpm --filter frontend typecheck` and confirm zero errors.
- [ ] Run `powershell -ExecutionPolicy Bypass -File scripts/build-portable.ps1` and record EXE size and SHA-256.
- [ ] Compare every acceptance criterion in `docs/superpowers/specs/2026-07-12-tauri-rust-windows-client-design.md` section 16 with evidence from tests or the Windows smoke checklist.
