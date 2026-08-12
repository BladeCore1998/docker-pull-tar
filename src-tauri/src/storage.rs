use crate::credentials::{Credential, CredentialStore};
use crate::domain::{
    AppSettings, HistoryItem, Mirror, MirrorInput, MirrorPatch, SettingsPatch, normalize_registry,
};
use crate::error::AppError;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

mod recovery;

#[async_trait::async_trait]
pub trait HistoryWriter: Send + Sync {
    /// Appends at most one row for an ID. An error guarantees this call committed no row.
    async fn append_history(&self, item: HistoryItem) -> StorageResult<HistoryAppendOutcome>;
}

const CONFIG_VERSION: &str = "3.0";
const HISTORY_LIMIT: usize = 100;
const MAINTENANCE_WARNING_LIMIT: usize = 32;

pub type StorageResult<T> = Result<T, AppError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryAppendOutcome {
    Committed,
    CommitIndeterminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigCommitOutcome {
    Committed,
    CommitIndeterminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageMaintenanceWarning {
    MainPromotionDeferred,
    CredentialCleanupDeferred,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredMirror {
    id: String,
    name: String,
    registry: String,
    is_default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential_ref: Option<String>,
    #[serde(default, skip_serializing)]
    password: Option<String>,
}

impl std::fmt::Debug for StoredMirror {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredMirror")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("registry", &self.registry)
            .field("is_default", &self.is_default)
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigData {
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

impl std::fmt::Debug for ConfigData {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConfigData")
            .field("version", &self.version)
            .field("revision", &self.revision)
            .field("transaction_id", &self.transaction_id)
            .field("settings", &self.settings)
            .field("mirrors", &self.mirrors)
            .field("history_len", &self.history.len())
            .field(
                "pending_credential_cleanup_len",
                &self.pending_credential_cleanup.len(),
            )
            .finish()
    }
}

struct ConfigStoreInner {
    path: PathBuf,
    data: RwLock<ConfigData>,
    transaction: Mutex<()>,
    credentials: Arc<dyn CredentialStore>,
    poisoned: AtomicBool,
    maintenance_warnings: Mutex<VecDeque<StorageMaintenanceWarning>>,
    persistence: Arc<dyn ConfigPersistence>,
    #[cfg(test)]
    cleanup_delete_barrier: Mutex<Option<Arc<CredentialCleanupBarrier>>>,
}

#[cfg(test)]
struct CredentialCleanupBarrier {
    reached: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

#[async_trait::async_trait]
trait ConfigPersistence: Send + Sync {
    async fn commit_recovery(&self, path: &Path, bytes: Vec<u8>) -> StorageResult<()>;
    async fn promote_main(&self, path: &Path, bytes: Vec<u8>) -> StorageResult<()>;
    async fn remove_recovery(&self, path: &Path) -> StorageResult<()>;
}

struct RealConfigPersistence;

#[async_trait::async_trait]
impl ConfigPersistence for RealConfigPersistence {
    async fn commit_recovery(&self, path: &Path, bytes: Vec<u8>) -> StorageResult<()> {
        recovery::commit_recovery(path, bytes).await
    }

    async fn promote_main(&self, path: &Path, bytes: Vec<u8>) -> StorageResult<()> {
        recovery::promote_main(path, bytes).await
    }

    async fn remove_recovery(&self, path: &Path) -> StorageResult<()> {
        recovery::remove_recovery(path).await
    }
}

#[derive(Clone)]
pub struct ConfigStore {
    inner: Arc<ConfigStoreInner>,
}

impl std::fmt::Debug for ConfigStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConfigStore")
            .field("path", &self.inner.path)
            .finish()
    }
}

impl ConfigStore {
    pub async fn open(
        path: impl Into<PathBuf>,
        default_settings: AppSettings,
        credentials: Arc<dyn CredentialStore>,
    ) -> StorageResult<Self> {
        Self::open_with_persistence(
            path.into(),
            default_settings,
            credentials,
            Arc::new(RealConfigPersistence),
        )
        .await
    }

    async fn open_with_persistence(
        path: PathBuf,
        default_settings: AppSettings,
        credentials: Arc<dyn CredentialStore>,
        persistence: Arc<dyn ConfigPersistence>,
    ) -> StorageResult<Self> {
        let selected = recovery::load_and_select(&path).await?;
        preserve_main_disposition(&path, &selected.main_disposition).await?;
        let recovery_cleanup_deferred = match selected.recovery_disposition {
            recovery::RecoveryDisposition::RemoveStale => {
                persistence.remove_recovery(&path).await?;
                false
            }
            recovery::RecoveryDisposition::RemoveRedundant => {
                persistence.remove_recovery(&path).await.is_err()
            }
            recovery::RecoveryDisposition::None | recovery::RecoveryDisposition::Promote => false,
        };
        let mut migrated_credentials = Vec::new();
        let recovery_disposition = selected.recovery_disposition;
        let selected_bytes = selected.bytes;
        let needs_v3_rewrite = selected.needs_v3_rewrite;
        let mut data = selected
            .data
            .unwrap_or_else(|| ConfigData::new(default_settings));

        if data.version == "1.0" {
            for mirror in &mut data.mirrors {
                if let Some(password) = mirror.password.take() {
                    let credential_ref = mirror
                        .credential_ref
                        .clone()
                        .unwrap_or_else(|| format!("mirror:{}", mirror.id));
                    let before = match credentials.get(&credential_ref).await {
                        Ok(value) => value,
                        Err(error) => {
                            rollback_migration(credentials.as_ref(), &migrated_credentials).await;
                            return Err(error);
                        }
                    };
                    if let Err(error) = credentials
                        .set(
                            &credential_ref,
                            &Credential {
                                username: mirror.username.clone(),
                                password,
                            },
                        )
                        .await
                    {
                        let failed = vec![(credential_ref, before)];
                        rollback_migration(credentials.as_ref(), &failed).await;
                        rollback_migration(credentials.as_ref(), &migrated_credentials).await;
                        return Err(error);
                    }
                    migrated_credentials.push((credential_ref.clone(), before));
                    mirror.credential_ref = Some(credential_ref);
                }
            }
            data.version = CONFIG_VERSION.into();
        } else if data.version == "2.0" {
            data.version = CONFIG_VERSION.into();
        }

        let store = Self {
            inner: Arc::new(ConfigStoreInner {
                path,
                data: RwLock::new(data),
                transaction: Mutex::new(()),
                credentials,
                poisoned: AtomicBool::new(false),
                maintenance_warnings: Mutex::new(VecDeque::new()),
                persistence,
                #[cfg(test)]
                cleanup_delete_barrier: Mutex::new(None),
            }),
        };
        if recovery_cleanup_deferred {
            store
                .record_warning(StorageMaintenanceWarning::MainPromotionDeferred)
                .await;
        }
        if needs_v3_rewrite {
            let snapshot = store.inner.data.read().await.clone();
            if let Err(error) = store.commit_next_locked(snapshot).await {
                if !matches!(error, AppError::ConfigCommitIndeterminate) {
                    rollback_migration(store.inner.credentials.as_ref(), &migrated_credentials)
                        .await;
                }
                return Err(error);
            }
        } else if recovery_disposition == recovery::RecoveryDisposition::Promote {
            let bytes = selected_bytes.ok_or(AppError::ConfigCorrupted)?;
            if store
                .inner
                .persistence
                .promote_main(&store.inner.path, bytes)
                .await
                .is_err()
            {
                store
                    .record_warning(StorageMaintenanceWarning::MainPromotionDeferred)
                    .await;
            }
        }
        store.retry_pending_credential_cleanup().await?;
        Ok(store)
    }

    pub async fn settings(&self) -> AppSettings {
        self.inner.data.read().await.settings.clone()
    }

    pub async fn has_mirrors(&self) -> bool {
        !self.inner.data.read().await.mirrors.is_empty()
    }

    pub async fn take_maintenance_warnings(&self) -> Vec<StorageMaintenanceWarning> {
        self.inner
            .maintenance_warnings
            .lock()
            .await
            .drain(..)
            .collect()
    }

    pub async fn update_settings(&self, mut patch: SettingsPatch) -> StorageResult<AppSettings> {
        let _transaction = self.inner.transaction.lock().await;
        self.ensure_mutable()?;
        let mut next = self.inner.data.read().await.clone();
        if let Some(registry) = patch.default_mirror.take() {
            let registry = normalize_registry(&registry);
            if registry.is_empty() {
                return Err(AppError::InvalidMirror);
            }
            if !next
                .mirrors
                .iter()
                .any(|mirror| mirror.registry == registry)
            {
                return Err(AppError::MirrorNotFound);
            }
            for mirror in &mut next.mirrors {
                mirror.is_default = mirror.registry == registry;
            }
            next.settings.default_mirror = registry;
        }
        next.settings.apply_patch(patch);
        self.commit_next_and_cleanup_locked(next.clone()).await?;
        Ok(next.settings)
    }

    pub async fn mirrors(&self) -> StorageResult<Vec<Mirror>> {
        let stored = self.inner.data.read().await.mirrors.clone();
        let mut mirrors = Vec::with_capacity(stored.len());
        for mirror in stored {
            mirrors.push(self.resolve_mirror(&mirror).await?);
        }
        Ok(mirrors)
    }

    pub async fn mirror(&self, id: &str) -> StorageResult<Mirror> {
        let stored = self
            .inner
            .data
            .read()
            .await
            .mirrors
            .iter()
            .find(|mirror| mirror.id == id)
            .cloned()
            .ok_or(AppError::MirrorNotFound)?;
        self.resolve_mirror(&stored).await
    }

    pub async fn add_mirror(&self, input: MirrorInput) -> StorageResult<Mirror> {
        let _transaction = self.inner.transaction.lock().await;
        self.ensure_mutable()?;
        let registry = normalize_registry(&input.registry);
        if registry.is_empty() {
            return Err(AppError::InvalidMirror);
        }
        let mut next = self.inner.data.read().await.clone();
        if next
            .mirrors
            .iter()
            .any(|mirror| mirror.registry == registry)
        {
            return Err(AppError::MirrorConflict);
        }
        let id = Uuid::new_v4().to_string();
        let credential = input.password.as_ref().map(|password| Credential {
            username: input.username.clone(),
            password: password.clone(),
        });
        let credential_ref = credential.as_ref().map(|_| fresh_credential_reference(&id));
        if let (Some(reference), Some(credential)) = (&credential_ref, &credential) {
            self.prepare_credential_ref_locked(reference).await?;
            self.inner.credentials.set(reference, credential).await?;
            next = self.inner.data.read().await.clone();
        }
        let is_default = input.is_default || next.mirrors.is_empty();
        if is_default {
            for mirror in &mut next.mirrors {
                mirror.is_default = false;
            }
            next.settings.default_mirror = registry.clone();
        }
        let stored = StoredMirror {
            id,
            name: input.name.trim().to_owned(),
            registry,
            is_default,
            username: input.username,
            credential_ref: credential_ref.clone(),
            password: None,
        };
        next.mirrors.push(stored.clone());
        if let Some(reference) = &credential_ref {
            remove_cleanup(&mut next, reference);
        }
        self.commit_next_locked(next).await?;
        self.retry_pending_credential_cleanup_locked().await?;
        Ok(resolved_mirror(&stored, credential))
    }

    pub async fn update_mirror(&self, id: &str, patch: MirrorPatch) -> StorageResult<Mirror> {
        let _transaction = self.inner.transaction.lock().await;
        self.ensure_mutable()?;
        let mut next = self.inner.data.read().await.clone();
        let index = next
            .mirrors
            .iter()
            .position(|m| m.id == id)
            .ok_or(AppError::MirrorNotFound)?;
        if patch.is_default == Some(false) && next.mirrors[index].is_default {
            return Err(AppError::DefaultMirrorRequired);
        }
        let current_registry = next.mirrors[index].registry.clone();
        let normalized_registry = patch.registry.as_deref().map(normalize_registry);
        let registry_changed = normalized_registry
            .as_deref()
            .is_some_and(|registry| registry != current_registry);
        let credential_changed = patch.username.is_some() || patch.password.is_some();
        if let Some(registry) = normalized_registry {
            if registry.is_empty() {
                return Err(AppError::InvalidMirror);
            }
            if next
                .mirrors
                .iter()
                .enumerate()
                .any(|(i, m)| i != index && m.registry == registry)
            {
                return Err(AppError::MirrorConflict);
            }
            next.mirrors[index].registry = registry;
            if next.mirrors[index].is_default {
                next.settings.default_mirror = next.mirrors[index].registry.clone();
            }
        }
        if let Some(name) = patch.name {
            next.mirrors[index].name = name.trim().to_owned();
        }
        if patch.is_default == Some(true) {
            for mirror in &mut next.mirrors {
                mirror.is_default = false;
            }
            next.mirrors[index].is_default = true;
            next.settings.default_mirror = next.mirrors[index].registry.clone();
        }
        let old_ref = next.mirrors[index].credential_ref.clone();
        if let Some(username) = patch.username.clone() {
            next.mirrors[index].username = username;
        }
        if !registry_changed && !credential_changed {
            let stored = next.mirrors[index].clone();
            self.commit_next_locked(next).await?;
            self.retry_pending_credential_cleanup_locked().await?;
            return self.resolve_mirror(&stored).await;
        }

        let old_credential = match (
            &old_ref,
            patch.password.is_none() && patch.username.is_some(),
        ) {
            (Some(reference), true) => self.inner.credentials.get(reference).await?,
            _ => None,
        };
        let desired_credential = match patch.password {
            Some(Some(password)) => Some(Credential {
                username: next.mirrors[index].username.clone(),
                password,
            }),
            Some(None) => None,
            None if patch.username.is_some() => {
                old_credential.as_ref().map(|credential| Credential {
                    username: next.mirrors[index].username.clone(),
                    password: credential.password.clone(),
                })
            }
            None => None,
        };
        let new_ref = desired_credential
            .as_ref()
            .map(|_| fresh_credential_reference(id));
        if let (Some(reference), Some(credential)) = (&new_ref, &desired_credential) {
            self.prepare_credential_ref_locked(reference).await?;
            self.inner.credentials.set(reference, credential).await?;
            next.pending_credential_cleanup = self
                .inner
                .data
                .read()
                .await
                .pending_credential_cleanup
                .clone();
        }
        next.mirrors[index].credential_ref = new_ref.clone();
        if let Some(reference) = &new_ref {
            remove_cleanup(&mut next, reference);
        }
        if let Some(reference) = &old_ref {
            enqueue_cleanup(&mut next, reference);
        }
        let stored = next.mirrors[index].clone();
        self.commit_next_locked(next).await?;
        self.retry_pending_credential_cleanup_locked().await?;
        Ok(resolved_mirror(&stored, desired_credential))
    }

    pub async fn set_default_mirror(&self, id: &str) -> StorageResult<()> {
        let _transaction = self.inner.transaction.lock().await;
        self.ensure_mutable()?;
        let mut next = self.inner.data.read().await.clone();
        if !next.mirrors.iter().any(|mirror| mirror.id == id) {
            return Err(AppError::MirrorNotFound);
        }
        for mirror in &mut next.mirrors {
            mirror.is_default = mirror.id == id;
        }
        next.settings.default_mirror = next
            .mirrors
            .iter()
            .find(|mirror| mirror.id == id)
            .expect("mirror existence checked")
            .registry
            .clone();
        self.commit_next_and_cleanup_locked(next).await
    }

    pub async fn delete_mirror(&self, id: &str) -> StorageResult<()> {
        let _transaction = self.inner.transaction.lock().await;
        self.ensure_mutable()?;
        let mut next = self.inner.data.read().await.clone();
        let index = next
            .mirrors
            .iter()
            .position(|mirror| mirror.id == id)
            .ok_or(AppError::MirrorNotFound)?;
        if next.mirrors[index].is_default {
            return Err(AppError::DefaultMirrorRequired);
        }
        let removed = next.mirrors.remove(index);
        if let Some(reference) = &removed.credential_ref {
            enqueue_cleanup(&mut next, reference);
        }
        self.commit_next_locked(next).await?;
        self.retry_pending_credential_cleanup_locked().await?;
        Ok(())
    }

    pub async fn history(&self) -> Vec<HistoryItem> {
        self.inner.data.read().await.history.clone()
    }

    pub async fn add_history(&self, item: HistoryItem) -> StorageResult<()> {
        let _transaction = self.inner.transaction.lock().await;
        self.ensure_mutable()?;
        let mut next = self.inner.data.read().await.clone();
        next.history.insert(0, item);
        next.history.truncate(HISTORY_LIMIT);
        self.commit_next_and_cleanup_locked(next).await
    }

    pub async fn delete_history(&self, id: &str) -> StorageResult<()> {
        let _transaction = self.inner.transaction.lock().await;
        self.ensure_mutable()?;
        let mut next = self.inner.data.read().await.clone();
        next.history.retain(|item| item.id != id);
        self.commit_next_and_cleanup_locked(next).await
    }

    pub async fn clear_history(&self) -> StorageResult<()> {
        let _transaction = self.inner.transaction.lock().await;
        self.ensure_mutable()?;
        let mut next = self.inner.data.read().await.clone();
        next.history.clear();
        self.commit_next_and_cleanup_locked(next).await
    }

    async fn resolve_mirror(&self, stored: &StoredMirror) -> StorageResult<Mirror> {
        let credential = match &stored.credential_ref {
            Some(reference) => self.inner.credentials.get(reference).await?,
            None => None,
        };
        Ok(Mirror {
            id: stored.id.clone(),
            name: stored.name.clone(),
            registry: stored.registry.clone(),
            is_default: stored.is_default,
            username: credential
                .as_ref()
                .and_then(|c| c.username.clone())
                .or_else(|| stored.username.clone()),
            password: credential.map(|c| c.password),
        })
    }

    async fn commit_next_and_cleanup_locked(&self, next: ConfigData) -> StorageResult<()> {
        self.commit_next_locked(next).await?;
        self.retry_pending_credential_cleanup_locked().await
    }

    async fn retry_pending_credential_cleanup(&self) -> StorageResult<()> {
        let _transaction = self.inner.transaction.lock().await;
        self.retry_pending_credential_cleanup_locked().await
    }

    async fn prepare_credential_ref_locked(&self, reference: &str) -> StorageResult<()> {
        let mut prepared = self.inner.data.read().await.clone();
        enqueue_cleanup(&mut prepared, reference);
        self.commit_next_locked(prepared).await
    }

    async fn retry_pending_credential_cleanup_locked(&self) -> StorageResult<()> {
        self.ensure_mutable()?;
        let mut snapshot = self.inner.data.read().await.clone();
        let active: HashSet<_> = snapshot
            .mirrors
            .iter()
            .filter_map(|mirror| mirror.credential_ref.clone())
            .collect();
        let mut normalized = snapshot.clone();
        normalized
            .pending_credential_cleanup
            .retain(|reference| !active.contains(reference));
        if normalized.pending_credential_cleanup != snapshot.pending_credential_cleanup {
            self.commit_next_locked(normalized).await?;
            snapshot = self.inner.data.read().await.clone();
        }
        let pending = snapshot.pending_credential_cleanup;
        if pending.is_empty() {
            return Ok(());
        }
        let active: HashSet<_> = snapshot
            .mirrors
            .iter()
            .filter_map(|mirror| mirror.credential_ref.clone())
            .collect();
        let mut remaining = Vec::new();
        let mut cleanup_deferred = false;
        for reference in pending {
            #[cfg(test)]
            if let Some(barrier) = self.inner.cleanup_delete_barrier.lock().await.clone() {
                barrier.reached.notify_one();
                barrier.resume.notified().await;
            }
            if active.contains(&reference) || self.is_active_credential_reference(&reference).await
            {
                continue;
            }
            let deletion = self.inner.credentials.delete(&reference).await;
            cleanup_deferred |= deletion.is_err();
            if !credential_absent(self.inner.credentials.as_ref(), &reference).await {
                remaining.push(reference);
                cleanup_deferred = true;
            }
        }
        let mut next = self.inner.data.read().await.clone();
        if next.pending_credential_cleanup == remaining {
            if cleanup_deferred {
                self.record_warning(StorageMaintenanceWarning::CredentialCleanupDeferred)
                    .await;
            }
            return Ok(());
        }
        next.pending_credential_cleanup = remaining;
        if self.commit_next_locked(next).await.is_err() {
            cleanup_deferred = true;
        }
        if cleanup_deferred {
            self.record_warning(StorageMaintenanceWarning::CredentialCleanupDeferred)
                .await;
        }
        Ok(())
    }

    async fn commit_next_locked(&self, next: ConfigData) -> StorageResult<()> {
        match self.commit_next_outcome_locked(next).await? {
            ConfigCommitOutcome::Committed => Ok(()),
            ConfigCommitOutcome::CommitIndeterminate => Err(AppError::ConfigCommitIndeterminate),
        }
    }

    async fn commit_next_outcome_locked(
        &self,
        mut next: ConfigData,
    ) -> StorageResult<ConfigCommitOutcome> {
        self.ensure_mutable()?;
        let current_revision = self.inner.data.read().await.revision;
        next.version = CONFIG_VERSION.into();
        next.revision = current_revision
            .checked_add(1)
            .ok_or(AppError::ConfigCorrupted)?;
        next.transaction_id = Uuid::new_v4().to_string();
        let bytes = serde_json::to_vec_pretty(&next).map_err(|_| AppError::ConfigIoFailed)?;

        match self
            .inner
            .persistence
            .commit_recovery(&self.inner.path, bytes.clone())
            .await
        {
            Ok(()) => {
                *self.inner.data.write().await = next;
            }
            Err(AppError::ConfigCommitIndeterminate) => {
                *self.inner.data.write().await = next;
                self.inner.poisoned.store(true, Ordering::Release);
                return Ok(ConfigCommitOutcome::CommitIndeterminate);
            }
            Err(error) => return Err(error),
        }
        if self
            .inner
            .persistence
            .promote_main(&self.inner.path, bytes)
            .await
            .is_err()
        {
            self.record_warning(StorageMaintenanceWarning::MainPromotionDeferred)
                .await;
        }
        Ok(ConfigCommitOutcome::Committed)
    }

    fn ensure_mutable(&self) -> StorageResult<()> {
        if self.inner.poisoned.load(Ordering::Acquire) {
            Err(AppError::ConfigCommitIndeterminate)
        } else {
            Ok(())
        }
    }

    async fn record_warning(&self, warning: StorageMaintenanceWarning) {
        let mut warnings = self.inner.maintenance_warnings.lock().await;
        if warnings.len() == MAINTENANCE_WARNING_LIMIT {
            warnings.pop_front();
        }
        warnings.push_back(warning);
    }

    async fn is_active_credential_reference(&self, reference: &str) -> bool {
        self.inner
            .data
            .read()
            .await
            .mirrors
            .iter()
            .any(|mirror| mirror.credential_ref.as_deref() == Some(reference))
    }
}

#[async_trait::async_trait]
impl HistoryWriter for ConfigStore {
    async fn append_history(&self, item: HistoryItem) -> StorageResult<HistoryAppendOutcome> {
        let _transaction = self.inner.transaction.lock().await;
        self.ensure_mutable()?;
        let mut next = self.inner.data.read().await.clone();
        if next.history.iter().any(|existing| existing.id == item.id) {
            return Ok(HistoryAppendOutcome::Committed);
        }
        next.history.insert(0, item);
        next.history.truncate(HISTORY_LIMIT);
        match self.commit_next_outcome_locked(next).await? {
            ConfigCommitOutcome::Committed => Ok(HistoryAppendOutcome::Committed),
            ConfigCommitOutcome::CommitIndeterminate => {
                Ok(HistoryAppendOutcome::CommitIndeterminate)
            }
        }
    }
}

impl ConfigData {
    fn new(settings: AppSettings) -> Self {
        Self {
            version: CONFIG_VERSION.into(),
            revision: 0,
            transaction_id: String::new(),
            settings,
            mirrors: vec![],
            history: vec![],
            pending_credential_cleanup: vec![],
        }
    }
}

pub fn history_redownload_registry(
    item: &HistoryItem,
    mirrors: &[Mirror],
    default_registry: &str,
) -> String {
    mirrors
        .iter()
        .find(|mirror| mirror.registry == item.mirror || mirror.id == item.mirror)
        .map(|mirror| mirror.registry.clone())
        .unwrap_or_else(|| default_registry.to_owned())
}

fn normalize_config(data: &mut ConfigData) -> bool {
    let mut changed = false;
    if data.history.len() > HISTORY_LIMIT {
        data.history.truncate(HISTORY_LIMIT);
        changed = true;
    }
    let active: HashSet<_> = data
        .mirrors
        .iter()
        .filter_map(|mirror| mirror.credential_ref.as_deref())
        .collect();
    let pending_len = data.pending_credential_cleanup.len();
    data.pending_credential_cleanup
        .retain(|reference| !active.contains(reference.as_str()));
    changed |= pending_len != data.pending_credential_cleanup.len();
    if data.mirrors.is_empty() {
        return changed;
    }
    let default = data
        .mirrors
        .iter()
        .position(|mirror| mirror.is_default)
        .unwrap_or(0);
    for (index, mirror) in data.mirrors.iter_mut().enumerate() {
        let registry = normalize_registry(&mirror.registry);
        let is_default = index == default;
        changed |= mirror.registry != registry || mirror.is_default != is_default;
        mirror.registry = registry;
        mirror.is_default = is_default;
    }
    let default_registry = data.mirrors[default].registry.clone();
    changed |= data.settings.default_mirror != default_registry;
    data.settings.default_mirror = default_registry;
    changed
}

fn config_recovery_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.recovery",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config.json")
    ))
}

async fn preserve_main_disposition(
    path: &Path,
    disposition: &recovery::MainDisposition,
) -> StorageResult<()> {
    match disposition {
        recovery::MainDisposition::None | recovery::MainDisposition::Keep { .. } => Ok(()),
        recovery::MainDisposition::BackupCorrupt { bytes } => {
            let timestamp = Utc::now().format("%Y%m%d%H%M%S%3f");
            let backup = path.with_file_name(format!(
                "{}.corrupt-{timestamp}",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("config.json")
            ));
            tokio::fs::write(backup, bytes)
                .await
                .map_err(|_| AppError::ConfigIoFailed)
        }
        recovery::MainDisposition::ReplaceNonFile => tokio::fs::remove_dir(path)
            .await
            .map_err(|_| AppError::ConfigIoFailed),
    }
}

async fn apply_credential(
    store: &dyn CredentialStore,
    reference: Option<&str>,
    credential: Option<&Credential>,
) -> StorageResult<()> {
    match (reference, credential) {
        (Some(reference), Some(value)) => store.set(reference, value).await,
        (Some(reference), None) => store.delete(reference).await,
        (None, None) => Ok(()),
        (None, Some(_)) => Err(AppError::CredentialStoreFailed),
    }
}

async fn rollback_migration(store: &dyn CredentialStore, changes: &[(String, Option<Credential>)]) {
    for (reference, before) in changes.iter().rev() {
        let _ = apply_credential(store, Some(reference), before.as_ref()).await;
    }
}

async fn credential_absent(store: &dyn CredentialStore, reference: &str) -> bool {
    matches!(store.get(reference).await, Ok(None))
}

fn enqueue_cleanup(data: &mut ConfigData, reference: &str) {
    if !data
        .pending_credential_cleanup
        .iter()
        .any(|pending| pending == reference)
    {
        data.pending_credential_cleanup.push(reference.to_owned());
    }
}

fn remove_cleanup(data: &mut ConfigData, reference: &str) {
    data.pending_credential_cleanup
        .retain(|pending| pending != reference);
}

fn fresh_credential_reference(mirror_id: &str) -> String {
    format!("mirror:{mirror_id}:{}", Uuid::new_v4())
}

fn resolved_mirror(stored: &StoredMirror, credential: Option<Credential>) -> Mirror {
    Mirror {
        id: stored.id.clone(),
        name: stored.name.clone(),
        registry: stored.registry.clone(),
        is_default: stored.is_default,
        username: credential
            .as_ref()
            .and_then(|value| value.username.clone())
            .or_else(|| stored.username.clone()),
        password: credential.map(|value| value.password),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        CompletedDownload, DownloadSnapshot, DownloadStartRequest, DownloadStatus,
    };
    use crate::events::EventSink;
    use crate::task_manager::{DownloadPipeline, TaskContext, TaskManager};
    use serde_json::Value;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    fn restart_settings() -> AppSettings {
        AppSettings {
            default_output_dir: "downloads".into(),
            default_arch: "amd64".into(),
            default_mirror: "registry.example.com".into(),
            download_workers: 4,
            last_package_name: None,
        }
    }

    enum PersistenceFault {
        IndeterminateCommit,
        DeferredPromotion,
        RecoveryRemoval,
    }

    struct FaultPersistence {
        fault: PersistenceFault,
        commit_calls: AtomicUsize,
    }

    #[derive(Default)]
    struct RecordingPersistence {
        commits: Mutex<Vec<Vec<u8>>>,
    }

    #[derive(Clone, Copy)]
    enum SequencedCommitFault {
        Io(usize),
        Indeterminate(usize),
    }

    struct SequencedPersistence {
        fault: SequencedCommitFault,
        commit_calls: AtomicUsize,
    }

    #[derive(Default)]
    struct RecordingCredentialStore {
        values: Mutex<std::collections::HashMap<String, Credential>>,
        set_refs: Mutex<Vec<String>>,
        delete_refs: Mutex<Vec<String>>,
    }

    impl RecordingCredentialStore {
        async fn insert(&self, reference: &str, credential: Credential) {
            self.values
                .lock()
                .await
                .insert(reference.into(), credential);
        }
    }

    struct CompletingPipeline;

    #[async_trait::async_trait]
    impl DownloadPipeline for CompletingPipeline {
        async fn run(
            &self,
            _request: DownloadStartRequest,
            context: TaskContext,
        ) -> Result<CompletedDownload, AppError> {
            for status in [
                DownloadStatus::Downloading,
                DownloadStatus::Verifying,
                DownloadStatus::Packing,
            ] {
                context.transition(status).await?;
            }
            Ok(CompletedDownload {
                output_path: "C:/output/nginx.tar".into(),
                size_bytes: 4096,
            })
        }
    }

    struct SilentEventSink;

    impl EventSink for SilentEventSink {
        fn emit(&self, _name: &'static str, _payload: Value) {}
    }

    struct CountingConfigHistory {
        store: Arc<ConfigStore>,
        append_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl HistoryWriter for CountingConfigHistory {
        async fn append_history(&self, item: HistoryItem) -> StorageResult<HistoryAppendOutcome> {
            self.append_calls.fetch_add(1, Ordering::SeqCst);
            HistoryWriter::append_history(self.store.as_ref(), item).await
        }
    }

    fn download_request() -> DownloadStartRequest {
        DownloadStartRequest {
            package_name: "library/nginx".into(),
            tag: "latest".into(),
            arch: "amd64".into(),
            mirror: "registry.example.com".into(),
            output_dir: "C:/output".into(),
            username: None,
            password: None,
        }
    }

    async fn wait_for_task_terminal(manager: &TaskManager) -> DownloadSnapshot {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = manager.snapshot().await;
                if matches!(
                    snapshot.status,
                    DownloadStatus::Completed | DownloadStatus::Failed | DownloadStatus::Cancelled
                ) {
                    return snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    #[async_trait::async_trait]
    impl ConfigPersistence for FaultPersistence {
        async fn commit_recovery(&self, path: &Path, bytes: Vec<u8>) -> StorageResult<()> {
            self.commit_calls.fetch_add(1, Ordering::SeqCst);
            match self.fault {
                PersistenceFault::IndeterminateCommit => {
                    recovery::commit_recovery(path, bytes).await?;
                    Err(AppError::ConfigCommitIndeterminate)
                }
                PersistenceFault::DeferredPromotion | PersistenceFault::RecoveryRemoval => {
                    recovery::commit_recovery(path, bytes).await
                }
            }
        }

        async fn promote_main(&self, _path: &Path, _bytes: Vec<u8>) -> StorageResult<()> {
            match self.fault {
                PersistenceFault::IndeterminateCommit => {
                    panic!("promotion must not follow an indeterminate commit")
                }
                PersistenceFault::DeferredPromotion => Err(AppError::ConfigIoFailed),
                PersistenceFault::RecoveryRemoval => recovery::promote_main(_path, _bytes).await,
            }
        }

        async fn remove_recovery(&self, _path: &Path) -> StorageResult<()> {
            match self.fault {
                PersistenceFault::RecoveryRemoval => Err(AppError::ConfigIoFailed),
                _ => Ok(()),
            }
        }
    }

    #[async_trait::async_trait]
    impl ConfigPersistence for RecordingPersistence {
        async fn commit_recovery(&self, _path: &Path, bytes: Vec<u8>) -> StorageResult<()> {
            self.commits.lock().await.push(bytes);
            Ok(())
        }

        async fn promote_main(&self, _path: &Path, _bytes: Vec<u8>) -> StorageResult<()> {
            Ok(())
        }

        async fn remove_recovery(&self, _path: &Path) -> StorageResult<()> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl ConfigPersistence for SequencedPersistence {
        async fn commit_recovery(&self, path: &Path, bytes: Vec<u8>) -> StorageResult<()> {
            let call = self.commit_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if matches!(self.fault, SequencedCommitFault::Io(fault) if fault == call) {
                return Err(AppError::ConfigIoFailed);
            }
            recovery::commit_recovery(path, bytes).await?;
            if matches!(self.fault, SequencedCommitFault::Indeterminate(fault) if fault == call) {
                return Err(AppError::ConfigCommitIndeterminate);
            }
            Ok(())
        }

        async fn promote_main(&self, path: &Path, bytes: Vec<u8>) -> StorageResult<()> {
            recovery::promote_main(path, bytes).await
        }

        async fn remove_recovery(&self, path: &Path) -> StorageResult<()> {
            recovery::remove_recovery(path).await
        }
    }

    #[async_trait::async_trait]
    impl CredentialStore for RecordingCredentialStore {
        async fn get(&self, key: &str) -> StorageResult<Option<Credential>> {
            Ok(self.values.lock().await.get(key).cloned())
        }

        async fn set(&self, key: &str, value: &Credential) -> StorageResult<()> {
            self.set_refs.lock().await.push(key.into());
            self.values.lock().await.insert(key.into(), value.clone());
            Ok(())
        }

        async fn delete(&self, key: &str) -> StorageResult<()> {
            self.delete_refs.lock().await.push(key.into());
            self.values.lock().await.remove(key);
            Ok(())
        }
    }

    fn store_with_persistence(
        path: PathBuf,
        data: ConfigData,
        persistence: Arc<dyn ConfigPersistence>,
    ) -> ConfigStore {
        store_with_dependencies(
            path,
            data,
            Arc::new(crate::credentials::MemoryCredentialStore::default()),
            persistence,
        )
    }

    fn store_with_dependencies(
        path: PathBuf,
        data: ConfigData,
        credentials: Arc<dyn CredentialStore>,
        persistence: Arc<dyn ConfigPersistence>,
    ) -> ConfigStore {
        ConfigStore {
            inner: Arc::new(ConfigStoreInner {
                path,
                data: RwLock::new(data),
                transaction: Mutex::new(()),
                credentials,
                poisoned: AtomicBool::new(false),
                maintenance_warnings: Mutex::new(VecDeque::new()),
                persistence,
                cleanup_delete_barrier: Mutex::new(None),
            }),
        }
    }

    #[tokio::test]
    async fn indeterminate_commit_exposes_next_snapshot_and_poisons_all_future_mutations() {
        let dir = tempfile::TempDir::new().unwrap();
        let persistence = Arc::new(FaultPersistence {
            fault: PersistenceFault::IndeterminateCommit,
            commit_calls: AtomicUsize::new(0),
        });
        let mut data = ConfigData::new(restart_settings());
        data.revision = 7;
        data.transaction_id = "existing".into();
        let store =
            store_with_persistence(dir.path().join("config.json"), data, persistence.clone());

        let error = store
            .update_settings(SettingsPatch {
                download_workers: Some(9),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(error, AppError::ConfigCommitIndeterminate));
        assert_eq!(store.settings().await.download_workers, 9);
        assert_eq!(store.inner.data.read().await.revision, 8);
        let committed_path = config_recovery_path(&store.inner.path);
        let committed_bytes = tokio::fs::read(&committed_path).await.unwrap();

        let history_error = store
            .add_history(HistoryItem {
                id: "blocked".into(),
                package_name: "busybox".into(),
                tag: "latest".into(),
                arch: "amd64".into(),
                mirror: "registry.example.com".into(),
                full_image: "busybox:latest".into(),
                output_path: None,
                size: None,
                status: crate::domain::DownloadStatus::Completed,
                downloaded_at: None,
                error: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(history_error, AppError::ConfigCommitIndeterminate));
        let mirror_error = store
            .add_mirror(MirrorInput {
                name: "blocked".into(),
                registry: "blocked.example".into(),
                is_default: true,
                username: None,
                password: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(mirror_error, AppError::ConfigCommitIndeterminate));
        assert_eq!(persistence.commit_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            tokio::fs::read(&committed_path).await.unwrap(),
            committed_bytes
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&committed_bytes).unwrap()["revision"],
            8
        );
        drop(store);

        let reopened = ConfigStore::open(
            dir.path().join("config.json"),
            restart_settings(),
            Arc::new(crate::credentials::MemoryCredentialStore::default()),
        )
        .await
        .unwrap();
        assert_eq!(reopened.settings().await.download_workers, 9);
        assert_eq!(reopened.inner.data.read().await.revision, 8);
    }

    #[tokio::test]
    async fn task_manager_keeps_completed_terminal_when_history_recovery_commit_is_indeterminate() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        let persistence = Arc::new(FaultPersistence {
            fault: PersistenceFault::IndeterminateCommit,
            commit_calls: AtomicUsize::new(0),
        });
        let mut data = ConfigData::new(restart_settings());
        data.revision = 7;
        data.transaction_id = "existing".into();
        let store = Arc::new(store_with_persistence(
            path.clone(),
            data,
            persistence.clone(),
        ));
        let history = Arc::new(CountingConfigHistory {
            store: store.clone(),
            append_calls: AtomicUsize::new(0),
        });
        let manager = TaskManager::new(
            Arc::new(CompletingPipeline),
            Arc::new(SilentEventSink),
            history.clone(),
        );

        let task_id = manager.start(download_request()).await.unwrap();
        let terminal = wait_for_task_terminal(&manager).await;
        assert_eq!(terminal.task_id, task_id);
        assert_eq!(terminal.status, DownloadStatus::Completed);
        assert_eq!(terminal.last_error, None);
        let in_memory = store.history().await;
        assert_eq!(in_memory.len(), 1);
        assert_eq!(in_memory[0].id, task_id);
        assert_eq!(in_memory[0].status, DownloadStatus::Completed);
        assert_eq!(history.append_calls.load(Ordering::SeqCst), 1);
        assert_eq!(persistence.commit_calls.load(Ordering::SeqCst), 1);

        let mut rejected = in_memory[0].clone();
        rejected.id = "after-poison".into();
        let error = HistoryWriter::append_history(store.as_ref(), rejected)
            .await
            .unwrap_err();
        assert!(matches!(error, AppError::ConfigCommitIndeterminate));
        assert_eq!(store.history().await.len(), 1);
        assert_eq!(persistence.commit_calls.load(Ordering::SeqCst), 1);

        manager.snapshot().await;
        assert_eq!(history.append_calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.history().await.len(), 1);
        assert_eq!(persistence.commit_calls.load(Ordering::SeqCst), 1);
        drop(manager);
        drop(store);

        let reopened = ConfigStore::open(
            path,
            restart_settings(),
            Arc::new(crate::credentials::MemoryCredentialStore::default()),
        )
        .await
        .unwrap();
        let reopened_history = reopened.history().await;
        assert_eq!(reopened_history.len(), 1);
        assert_eq!(reopened_history[0].id, task_id);
        assert_eq!(reopened_history[0].status, DownloadStatus::Completed);
    }

    #[tokio::test]
    async fn stale_recovery_removal_failure_prevents_open() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        let recovery_path = config_recovery_path(&path);
        let mut main = ConfigData::new(restart_settings());
        main.revision = 8;
        main.transaction_id = "new-main".into();
        let mut stale = main.clone();
        stale.revision = 7;
        stale.transaction_id = "stale-recovery".into();
        tokio::fs::write(&path, serde_json::to_vec(&main).unwrap())
            .await
            .unwrap();
        tokio::fs::write(&recovery_path, serde_json::to_vec(&stale).unwrap())
            .await
            .unwrap();

        let error = ConfigStore::open_with_persistence(
            path,
            restart_settings(),
            Arc::new(crate::credentials::MemoryCredentialStore::default()),
            Arc::new(FaultPersistence {
                fault: PersistenceFault::RecoveryRemoval,
                commit_calls: AtomicUsize::new(0),
            }),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, AppError::ConfigIoFailed));
        assert!(recovery_path.exists());
    }

    #[tokio::test]
    async fn equal_recovery_removal_failure_allows_open_with_redacted_warning() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        let recovery_path = config_recovery_path(&path);
        let mut snapshot = ConfigData::new(restart_settings());
        snapshot.revision = 8;
        snapshot.transaction_id = "same-transaction".into();
        let bytes = serde_json::to_vec(&snapshot).unwrap();
        tokio::fs::write(&path, &bytes).await.unwrap();
        tokio::fs::write(&recovery_path, &bytes).await.unwrap();

        let opened = ConfigStore::open_with_persistence(
            path,
            restart_settings(),
            Arc::new(crate::credentials::MemoryCredentialStore::default()),
            Arc::new(FaultPersistence {
                fault: PersistenceFault::RecoveryRemoval,
                commit_calls: AtomicUsize::new(0),
            }),
        )
        .await
        .unwrap();

        assert_eq!(
            opened.take_maintenance_warnings().await,
            vec![StorageMaintenanceWarning::MainPromotionDeferred]
        );
        assert!(recovery_path.exists());
    }

    #[tokio::test]
    async fn deferred_promotion_returns_success_and_reopen_selects_committed_recovery() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        let persistence = Arc::new(FaultPersistence {
            fault: PersistenceFault::DeferredPromotion,
            commit_calls: AtomicUsize::new(0),
        });
        let mut data = ConfigData::new(restart_settings());
        data.revision = 1;
        data.transaction_id = "existing".into();
        let store = store_with_persistence(path.clone(), data, persistence);

        store
            .update_settings(SettingsPatch {
                default_arch: Some("arm64".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            store.take_maintenance_warnings().await,
            vec![StorageMaintenanceWarning::MainPromotionDeferred]
        );
        drop(store);

        let reopened = ConfigStore::open(
            path,
            restart_settings(),
            Arc::new(crate::credentials::MemoryCredentialStore::default()),
        )
        .await
        .unwrap();
        assert_eq!(reopened.settings().await.default_arch, "arm64");
        assert_eq!(reopened.inner.data.read().await.revision, 2);
    }

    #[tokio::test]
    async fn maintenance_warning_queue_is_bounded_and_take_clears_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = store_with_persistence(
            dir.path().join("config.json"),
            ConfigData::new(restart_settings()),
            Arc::new(RealConfigPersistence),
        );
        for _ in 0..40 {
            store
                .record_warning(StorageMaintenanceWarning::CredentialCleanupDeferred)
                .await;
        }
        assert_eq!(store.take_maintenance_warnings().await.len(), 32);
        assert!(store.take_maintenance_warnings().await.is_empty());
    }

    #[tokio::test]
    async fn authenticated_add_commits_unique_prepare_and_activation_snapshots() {
        let dir = tempfile::TempDir::new().unwrap();
        let persistence = Arc::new(RecordingPersistence::default());
        let store = store_with_persistence(
            dir.path().join("config.json"),
            ConfigData::new(restart_settings()),
            persistence.clone(),
        );

        let added = store
            .add_mirror(MirrorInput {
                name: "Private".into(),
                registry: "registry.example.com".into(),
                is_default: true,
                username: Some("alice".into()),
                password: Some("secret".into()),
            })
            .await
            .unwrap();

        let commits = persistence.commits.lock().await;
        assert_eq!(commits.len(), 2);
        let prepare: serde_json::Value = serde_json::from_slice(&commits[0]).unwrap();
        let activation: serde_json::Value = serde_json::from_slice(&commits[1]).unwrap();
        assert_eq!(prepare["revision"], 1);
        assert_eq!(activation["revision"], 2);
        assert_ne!(prepare["transactionId"], activation["transactionId"]);
        assert!(prepare["mirrors"].as_array().unwrap().is_empty());
        let prepared_ref = prepare["pendingCredentialCleanup"][0].as_str().unwrap();
        assert_eq!(activation["mirrors"][0]["id"], added.id);
        assert_eq!(activation["mirrors"][0]["credentialRef"], prepared_ref);
        assert!(
            activation["pendingCredentialCleanup"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            commits
                .iter()
                .all(|bytes| !bytes.windows(6).any(|part| part == b"secret"))
        );
    }

    fn seeded_authenticated_config(reference: &str) -> ConfigData {
        let mut data = ConfigData::new(restart_settings());
        data.revision = 1;
        data.transaction_id = "seeded".into();
        data.mirrors.push(StoredMirror {
            id: "private".into(),
            name: "Private".into(),
            registry: "registry.example.com".into(),
            is_default: true,
            username: Some("alice".into()),
            credential_ref: Some(reference.into()),
            password: None,
        });
        data
    }

    fn replacement_credential_patch() -> MirrorPatch {
        MirrorPatch {
            username: Some(Some("bob".into())),
            password: Some(Some("new-secret".into())),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn credential_prepare_io_failure_never_calls_secret_set() {
        let dir = tempfile::TempDir::new().unwrap();
        let credentials = Arc::new(RecordingCredentialStore::default());
        let persistence = Arc::new(SequencedPersistence {
            fault: SequencedCommitFault::Io(1),
            commit_calls: AtomicUsize::new(0),
        });
        let store = store_with_dependencies(
            dir.path().join("config.json"),
            ConfigData::new(restart_settings()),
            credentials.clone(),
            persistence.clone(),
        );

        let error = store
            .add_mirror(MirrorInput {
                name: "Private".into(),
                registry: "registry.example.com".into(),
                is_default: true,
                username: Some("alice".into()),
                password: Some("secret".into()),
            })
            .await
            .unwrap_err();

        assert!(matches!(error, AppError::ConfigIoFailed));
        assert_eq!(persistence.commit_calls.load(Ordering::SeqCst), 1);
        assert!(credentials.set_refs.lock().await.is_empty());
        assert!(credentials.values.lock().await.is_empty());
        assert!(
            store
                .inner
                .data
                .read()
                .await
                .pending_credential_cleanup
                .is_empty()
        );
    }

    #[tokio::test]
    async fn credential_prepare_indeterminate_never_sets_secret_and_reopen_cleans_pending_ref() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        let credentials = Arc::new(RecordingCredentialStore::default());
        let persistence = Arc::new(SequencedPersistence {
            fault: SequencedCommitFault::Indeterminate(1),
            commit_calls: AtomicUsize::new(0),
        });
        let store = store_with_dependencies(
            path.clone(),
            ConfigData::new(restart_settings()),
            credentials.clone(),
            persistence.clone(),
        );

        let error = store
            .add_mirror(MirrorInput {
                name: "Private".into(),
                registry: "registry.example.com".into(),
                is_default: true,
                username: Some("alice".into()),
                password: Some("secret".into()),
            })
            .await
            .unwrap_err();

        assert!(matches!(error, AppError::ConfigCommitIndeterminate));
        assert!(credentials.set_refs.lock().await.is_empty());
        assert!(credentials.values.lock().await.is_empty());
        let recovery_bytes = tokio::fs::read(config_recovery_path(&path)).await.unwrap();
        let recovery_json: serde_json::Value = serde_json::from_slice(&recovery_bytes).unwrap();
        let prepared_ref = recovery_json["pendingCredentialCleanup"][0]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(recovery_json["mirrors"].as_array().unwrap().is_empty());
        assert!(
            !String::from_utf8(recovery_bytes)
                .unwrap()
                .contains("secret")
        );
        let poisoned = store
            .update_settings(SettingsPatch {
                download_workers: Some(9),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(poisoned, AppError::ConfigCommitIndeterminate));
        assert_eq!(persistence.commit_calls.load(Ordering::SeqCst), 1);
        drop(store);

        let reopened = ConfigStore::open(path.clone(), restart_settings(), credentials.clone())
            .await
            .unwrap();
        assert!(reopened.mirrors().await.unwrap().is_empty());
        assert!(credentials.values.lock().await.is_empty());
        assert_eq!(
            credentials.delete_refs.lock().await.as_slice(),
            [prepared_ref]
        );
        let reopened_json: serde_json::Value =
            serde_json::from_slice(&tokio::fs::read(path).await.unwrap()).unwrap();
        assert!(
            reopened_json["pendingCredentialCleanup"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn credential_activation_indeterminate_exposes_new_active_and_never_cleans_old_in_process()
     {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        let old_ref = "mirror:private:old";
        let data = seeded_authenticated_config(old_ref);
        tokio::fs::write(&path, serde_json::to_vec_pretty(&data).unwrap())
            .await
            .unwrap();
        let credentials = Arc::new(RecordingCredentialStore::default());
        credentials
            .insert(
                old_ref,
                Credential {
                    username: Some("alice".into()),
                    password: "old-secret".into(),
                },
            )
            .await;
        let persistence = Arc::new(SequencedPersistence {
            fault: SequencedCommitFault::Indeterminate(2),
            commit_calls: AtomicUsize::new(0),
        });
        let store =
            store_with_dependencies(path.clone(), data, credentials.clone(), persistence.clone());

        let error = store
            .update_mirror("private", replacement_credential_patch())
            .await
            .unwrap_err();

        assert!(matches!(error, AppError::ConfigCommitIndeterminate));
        assert_eq!(persistence.commit_calls.load(Ordering::SeqCst), 2);
        let current = store.inner.data.read().await.clone();
        let new_ref = current.mirrors[0].credential_ref.clone().unwrap();
        assert_ne!(new_ref, old_ref);
        assert_eq!(current.pending_credential_cleanup, vec![old_ref]);
        assert!(credentials.values.lock().await.contains_key(old_ref));
        assert!(credentials.values.lock().await.contains_key(&new_ref));
        assert!(credentials.delete_refs.lock().await.is_empty());
        let recovery_bytes = tokio::fs::read(config_recovery_path(&path)).await.unwrap();
        let recovery_json: serde_json::Value = serde_json::from_slice(&recovery_bytes).unwrap();
        assert_eq!(recovery_json["mirrors"][0]["credentialRef"], new_ref);
        assert_eq!(
            recovery_json["pendingCredentialCleanup"],
            serde_json::json!([old_ref])
        );
        assert!(
            !String::from_utf8(recovery_bytes)
                .unwrap()
                .contains("secret")
        );
        let poisoned = store
            .update_settings(SettingsPatch {
                download_workers: Some(9),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(poisoned, AppError::ConfigCommitIndeterminate));
        assert_eq!(persistence.commit_calls.load(Ordering::SeqCst), 2);
        drop(store);

        let reopened = ConfigStore::open(path, restart_settings(), credentials.clone())
            .await
            .unwrap();
        let active = reopened.mirror("private").await.unwrap();
        assert_eq!(active.username.as_deref(), Some("bob"));
        assert_eq!(active.password.as_deref(), Some("new-secret"));
        assert!(!credentials.values.lock().await.contains_key(old_ref));
        assert!(credentials.values.lock().await.contains_key(&new_ref));
    }

    #[tokio::test]
    async fn credential_pending_clear_indeterminate_returns_success_warns_and_reopens_active() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        let old_ref = "mirror:private:old";
        let data = seeded_authenticated_config(old_ref);
        tokio::fs::write(&path, serde_json::to_vec_pretty(&data).unwrap())
            .await
            .unwrap();
        let credentials = Arc::new(RecordingCredentialStore::default());
        credentials
            .insert(
                old_ref,
                Credential {
                    username: Some("alice".into()),
                    password: "old-secret".into(),
                },
            )
            .await;
        let persistence = Arc::new(SequencedPersistence {
            fault: SequencedCommitFault::Indeterminate(3),
            commit_calls: AtomicUsize::new(0),
        });
        let store =
            store_with_dependencies(path.clone(), data, credentials.clone(), persistence.clone());

        let updated = store
            .update_mirror("private", replacement_credential_patch())
            .await
            .unwrap();

        assert_eq!(updated.username.as_deref(), Some("bob"));
        assert_eq!(updated.password.as_deref(), Some("new-secret"));
        assert_eq!(persistence.commit_calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            store.take_maintenance_warnings().await,
            vec![StorageMaintenanceWarning::CredentialCleanupDeferred]
        );
        let current = store.inner.data.read().await.clone();
        let new_ref = current.mirrors[0].credential_ref.clone().unwrap();
        assert!(current.pending_credential_cleanup.is_empty());
        assert!(!credentials.values.lock().await.contains_key(old_ref));
        assert!(credentials.values.lock().await.contains_key(&new_ref));
        let recovery_bytes = tokio::fs::read(config_recovery_path(&path)).await.unwrap();
        let recovery_json: serde_json::Value = serde_json::from_slice(&recovery_bytes).unwrap();
        assert_eq!(recovery_json["mirrors"][0]["credentialRef"], new_ref);
        assert!(
            recovery_json["pendingCredentialCleanup"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            !String::from_utf8(recovery_bytes)
                .unwrap()
                .contains("secret")
        );
        let poisoned = store
            .update_settings(SettingsPatch {
                download_workers: Some(9),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(poisoned, AppError::ConfigCommitIndeterminate));
        assert_eq!(persistence.commit_calls.load(Ordering::SeqCst), 3);
        drop(store);

        let reopened = ConfigStore::open(path.clone(), restart_settings(), credentials.clone())
            .await
            .unwrap();
        let active = reopened.mirror("private").await.unwrap();
        assert_eq!(active.username.as_deref(), Some("bob"));
        assert_eq!(active.password.as_deref(), Some("new-secret"));
        let reopened_bytes = tokio::fs::read(path).await.unwrap();
        assert!(
            !String::from_utf8(reopened_bytes)
                .unwrap()
                .contains("secret")
        );
    }

    #[tokio::test]
    async fn cleanup_live_guard_blocks_delete_when_reference_becomes_active_after_snapshot() {
        let dir = tempfile::TempDir::new().unwrap();
        let reference = "mirror:reactivated";
        let mut data = ConfigData::new(restart_settings());
        enqueue_cleanup(&mut data, reference);
        let store = Arc::new(store_with_persistence(
            dir.path().join("config.json"),
            data,
            Arc::new(RecordingPersistence::default()),
        ));
        store
            .inner
            .credentials
            .set(
                reference,
                &Credential {
                    username: Some("alice".into()),
                    password: "secret".into(),
                },
            )
            .await
            .unwrap();
        let barrier = Arc::new(CredentialCleanupBarrier {
            reached: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
        });
        *store.inner.cleanup_delete_barrier.lock().await = Some(barrier.clone());

        let cleanup_store = store.clone();
        let cleanup =
            tokio::spawn(async move { cleanup_store.retry_pending_credential_cleanup().await });
        barrier.reached.notified().await;
        {
            let mut current = store.inner.data.write().await;
            current.mirrors.push(StoredMirror {
                id: "reactivated".into(),
                name: "Private".into(),
                registry: "registry.example.com".into(),
                is_default: true,
                username: Some("alice".into()),
                credential_ref: Some(reference.into()),
                password: None,
            });
        }
        barrier.resume.notify_one();
        cleanup.await.unwrap().unwrap();

        assert!(
            store
                .inner
                .credentials
                .get(reference)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .inner
                .data
                .read()
                .await
                .pending_credential_cleanup
                .is_empty()
        );
    }
}
