use crate::error::AppError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use tokio::sync::RwLock;

pub type CredentialResult<T> = Result<T, AppError>;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Credential {
    pub username: Option<String>,
    pub password: String,
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

#[async_trait]
pub trait CredentialStore: Send + Sync {
    async fn get(&self, credential_ref: &str) -> CredentialResult<Option<Credential>>;
    async fn set(&self, credential_ref: &str, credential: &Credential) -> CredentialResult<()>;
    async fn delete(&self, credential_ref: &str) -> CredentialResult<()>;
}

#[derive(Default)]
pub struct MemoryCredentialStore {
    values: RwLock<HashMap<String, Credential>>,
}

#[async_trait]
impl CredentialStore for MemoryCredentialStore {
    async fn get(&self, credential_ref: &str) -> CredentialResult<Option<Credential>> {
        Ok(self.values.read().await.get(credential_ref).cloned())
    }

    async fn set(&self, credential_ref: &str, credential: &Credential) -> CredentialResult<()> {
        self.values
            .write()
            .await
            .insert(credential_ref.to_owned(), credential.clone());
        Ok(())
    }

    async fn delete(&self, credential_ref: &str) -> CredentialResult<()> {
        self.values.write().await.remove(credential_ref);
        Ok(())
    }
}

pub struct WindowsCredentialStore {
    service: String,
}

impl WindowsCredentialStore {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }
}

#[async_trait]
impl CredentialStore for WindowsCredentialStore {
    async fn get(&self, credential_ref: &str) -> CredentialResult<Option<Credential>> {
        let service = self.service.clone();
        let credential_ref = credential_ref.to_owned();
        tokio::task::spawn_blocking(move || {
            let entry = keyring::Entry::new(&service, &credential_ref)
                .map_err(|_| AppError::CredentialStoreFailed)?;
            match entry.get_password() {
                Ok(json) => serde_json::from_str(&json)
                    .map(Some)
                    .map_err(|_| AppError::CredentialStoreFailed),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(_) => Err(AppError::CredentialStoreFailed),
            }
        })
        .await
        .map_err(|_| AppError::CredentialStoreFailed)?
    }

    async fn set(&self, credential_ref: &str, credential: &Credential) -> CredentialResult<()> {
        let service = self.service.clone();
        let credential_ref = credential_ref.to_owned();
        let credential = credential.clone();
        tokio::task::spawn_blocking(move || {
            let entry = keyring::Entry::new(&service, &credential_ref)
                .map_err(|_| AppError::CredentialStoreFailed)?;
            let json =
                serde_json::to_string(&credential).map_err(|_| AppError::CredentialStoreFailed)?;
            entry
                .set_password(&json)
                .map_err(|_| AppError::CredentialStoreFailed)
        })
        .await
        .map_err(|_| AppError::CredentialStoreFailed)?
    }

    async fn delete(&self, credential_ref: &str) -> CredentialResult<()> {
        let service = self.service.clone();
        let credential_ref = credential_ref.to_owned();
        tokio::task::spawn_blocking(move || {
            let entry = keyring::Entry::new(&service, &credential_ref)
                .map_err(|_| AppError::CredentialStoreFailed)?;
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(_) => Err(AppError::CredentialStoreFailed),
            }
        })
        .await
        .map_err(|_| AppError::CredentialStoreFailed)?
    }
}
