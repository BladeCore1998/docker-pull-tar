use crate::error::AppError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
#[cfg(windows)]
use std::iter::once;
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
            read_credential(&format!("DockerPullTar:{service}:{credential_ref}"))
        })
        .await
        .map_err(|_| AppError::CredentialStoreFailed)?
    }

    async fn set(&self, credential_ref: &str, credential: &Credential) -> CredentialResult<()> {
        let service = self.service.clone();
        let credential_ref = credential_ref.to_owned();
        let credential = credential.clone();
        tokio::task::spawn_blocking(move || {
            let json =
                serde_json::to_string(&credential).map_err(|_| AppError::CredentialStoreFailed)?;
            let target = format!("DockerPullTar:{service}:{credential_ref}");
            write_credential(&target, &json)?;
            match read_credential(&target)? {
                Some(stored) if stored == credential => Ok(()),
                _ => Err(AppError::CredentialStoreFailed),
            }
        })
        .await
        .map_err(|_| AppError::CredentialStoreFailed)?
    }

    async fn delete(&self, credential_ref: &str) -> CredentialResult<()> {
        let service = self.service.clone();
        let credential_ref = credential_ref.to_owned();
        tokio::task::spawn_blocking(move || {
            delete_credential(&format!("DockerPullTar:{service}:{credential_ref}"))
        })
        .await
        .map_err(|_| AppError::CredentialStoreFailed)?
    }
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(once(0)).collect()
}

#[cfg(windows)]
fn read_credential(target: &str) -> CredentialResult<Option<Credential>> {
    use std::mem::MaybeUninit;
    use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, GetLastError};
    use windows_sys::Win32::Security::Credentials::{
        CRED_TYPE_GENERIC, CREDENTIALW, CredFree, CredReadW,
    };

    let target = wide(target);
    let mut raw = MaybeUninit::<*mut CREDENTIALW>::uninit();
    if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, raw.as_mut_ptr()) } == 0 {
        return if unsafe { GetLastError() } == ERROR_NOT_FOUND {
            Ok(None)
        } else {
            Err(AppError::CredentialStoreFailed)
        };
    }
    let raw = unsafe { raw.assume_init() };
    let bytes = unsafe {
        std::slice::from_raw_parts((*raw).CredentialBlob, (*raw).CredentialBlobSize as usize)
    };
    let result = serde_json::from_slice(bytes)
        .map(Some)
        .map_err(|_| AppError::CredentialStoreFailed);
    unsafe { CredFree(raw.cast()) };
    result
}

#[cfg(windows)]
fn write_credential(target: &str, payload: &str) -> CredentialResult<()> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::Security::Credentials::{
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredWriteW,
    };

    let mut target = wide(target);
    let mut username = wide("Docker Pull Tar");
    let mut payload = payload.as_bytes().to_vec();
    let credential = CREDENTIALW {
        Flags: 0,
        Type: CRED_TYPE_GENERIC,
        TargetName: target.as_mut_ptr(),
        Comment: std::ptr::null_mut(),
        LastWritten: FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        },
        CredentialBlobSize: payload
            .len()
            .try_into()
            .map_err(|_| AppError::CredentialStoreFailed)?,
        CredentialBlob: payload.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        AttributeCount: 0,
        Attributes: std::ptr::null_mut(),
        TargetAlias: std::ptr::null_mut(),
        UserName: username.as_mut_ptr(),
    };
    if unsafe { CredWriteW(&credential, 0) } == 0 {
        return Err(AppError::CredentialStoreFailed);
    }
    Ok(())
}

#[cfg(windows)]
fn delete_credential(target: &str) -> CredentialResult<()> {
    use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, GetLastError};
    use windows_sys::Win32::Security::Credentials::{CRED_TYPE_GENERIC, CredDeleteW};

    let target = wide(target);
    if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } != 0
        || unsafe { GetLastError() } == ERROR_NOT_FOUND
    {
        Ok(())
    } else {
        Err(AppError::CredentialStoreFailed)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn windows_store_round_trips_credentials_with_an_explicit_target() {
        let store = WindowsCredentialStore::new("docker-pull-tar-test");
        let credential_ref = format!("test:{}", uuid::Uuid::new_v4());
        let expected = Credential {
            username: Some("test-user".into()),
            password: "test-password".into(),
        };

        store.set(&credential_ref, &expected).await.unwrap();
        assert_eq!(store.get(&credential_ref).await.unwrap(), Some(expected));
        store.delete(&credential_ref).await.unwrap();
        assert_eq!(store.get(&credential_ref).await.unwrap(), None);
    }
}
