use super::{ConfigData, StorageResult, config_recovery_path, normalize_config};
use crate::error::AppError;
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SnapshotSource {
    Main,
    Recovery,
    Defaults,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecoveryDisposition {
    None,
    RemoveStale,
    RemoveRedundant,
    Promote,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) enum MainDisposition {
    None,
    Keep { bytes: Vec<u8> },
    BackupCorrupt { bytes: Vec<u8> },
    ReplaceNonFile,
}

impl std::fmt::Debug for MainDisposition {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => formatter.write_str("None"),
            Self::Keep { bytes } => formatter
                .debug_struct("Keep")
                .field("byte_len", &bytes.len())
                .finish(),
            Self::BackupCorrupt { bytes } => formatter
                .debug_struct("BackupCorrupt")
                .field("byte_len", &bytes.len())
                .finish(),
            Self::ReplaceNonFile => formatter.write_str("ReplaceNonFile"),
        }
    }
}

pub(super) struct SelectedSnapshot {
    pub(super) data: Option<ConfigData>,
    pub(super) bytes: Option<Vec<u8>>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) source: SnapshotSource,
    pub(super) needs_v3_rewrite: bool,
    pub(super) main_disposition: MainDisposition,
    pub(super) recovery_disposition: RecoveryDisposition,
}

struct Candidate {
    data: ConfigData,
    bytes: Vec<u8>,
    normalized: serde_json::Value,
    normalized_changed: bool,
    version: SnapshotVersion,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SnapshotVersion {
    V1,
    V2,
    V3,
}

enum ParsedCandidate {
    Missing,
    NonFile,
    Valid(Box<Candidate>),
    Corrupt { bytes: Vec<u8> },
    Invalid { bytes: Vec<u8> },
}

enum CandidateInput {
    Missing,
    NonFile,
    File(Vec<u8>),
}

pub(super) async fn load_and_select(path: &Path) -> StorageResult<SelectedSnapshot> {
    let recovery_path = config_recovery_path(path);
    let (main, recovery) = tokio::join!(read_candidate(path), read_candidate(&recovery_path));
    let main = parse_candidate(main?);
    let recovery = parse_candidate(recovery?);
    select_candidates(main, recovery)
}

trait SnapshotFs {
    type File: Write;

    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    fn create_file(&self, path: &Path) -> io::Result<Self::File>;
    fn flush_file(&self, file: &mut Self::File) -> io::Result<()>;
    fn sync_file(&self, file: &Self::File) -> io::Result<()>;
    fn durable_replace(&self, source: &Path, target: &Path) -> io::Result<()>;
    fn replacement_barrier(&self, source: &Path, target: &Path) -> io::Result<()>;
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
}

struct RealSnapshotFs;

impl SnapshotFs for RealSnapshotFs {
    type File = fs::File;

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::create_dir_all(platform_path(path)?)
    }

    fn create_file(&self, path: &Path) -> io::Result<Self::File> {
        fs::File::create(platform_path(path)?)
    }

    fn flush_file(&self, file: &mut Self::File) -> io::Result<()> {
        file.flush()
    }

    fn sync_file(&self, file: &Self::File) -> io::Result<()> {
        file.sync_all()
    }

    fn durable_replace(&self, source: &Path, target: &Path) -> io::Result<()> {
        durable_replace(source, target)
    }

    fn replacement_barrier(&self, source: &Path, target: &Path) -> io::Result<()> {
        replacement_barrier(source, target)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(platform_path(path)?)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(platform_path(path)?)
    }
}

pub(super) async fn commit_recovery(path: &Path, bytes: Vec<u8>) -> StorageResult<()> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || commit_recovery_with(&RealSnapshotFs, &path, &bytes))
        .await
        .map_err(|_| AppError::ConfigIoFailed)?
}

pub(super) async fn promote_main(path: &Path, bytes: Vec<u8>) -> StorageResult<()> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || promote_main_with(&RealSnapshotFs, &path, &bytes))
        .await
        .map_err(|_| AppError::ConfigIoFailed)?
}

pub(super) async fn remove_recovery(path: &Path) -> StorageResult<()> {
    let recovery = config_recovery_path(path);
    tokio::task::spawn_blocking(move || match RealSnapshotFs.remove_file(&recovery) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(AppError::ConfigIoFailed),
    })
    .await
    .map_err(|_| AppError::ConfigIoFailed)?
}

fn commit_recovery_with<F: SnapshotFs>(fs: &F, path: &Path, bytes: &[u8]) -> StorageResult<()> {
    let staging = recovery_temp_path(path);
    let recovery = config_recovery_path(path);
    write_synced(fs, &staging, bytes)?;
    fs.durable_replace(&staging, &recovery)
        .map_err(|_| AppError::ConfigIoFailed)?;
    fs.replacement_barrier(&staging, &recovery)
        .map_err(|_| AppError::ConfigCommitIndeterminate)
}

fn promote_main_with<F: SnapshotFs>(fs: &F, path: &Path, bytes: &[u8]) -> StorageResult<()> {
    let staging = main_temp_path(path);
    write_synced(fs, &staging, bytes)?;
    fs.durable_replace(&staging, path)
        .map_err(|_| AppError::ConfigIoFailed)?;
    fs.replacement_barrier(&staging, path)
        .map_err(|_| AppError::ConfigIoFailed)?;
    let promoted = fs.read(path).map_err(|_| AppError::ConfigIoFailed)?;
    verify_promoted_snapshot(bytes, &promoted)?;
    fs.remove_file(&config_recovery_path(path))
        .map_err(|_| AppError::ConfigIoFailed)
}

fn write_synced<F: SnapshotFs>(fs: &F, path: &Path, bytes: &[u8]) -> StorageResult<()> {
    if let Some(parent) = path.parent() {
        fs.create_dir_all(parent)
            .map_err(|_| AppError::ConfigIoFailed)?;
    }
    let mut file = fs.create_file(path).map_err(|_| AppError::ConfigIoFailed)?;
    file.write_all(bytes)
        .map_err(|_| AppError::ConfigIoFailed)?;
    fs.flush_file(&mut file)
        .map_err(|_| AppError::ConfigIoFailed)?;
    fs.sync_file(&file).map_err(|_| AppError::ConfigIoFailed)
}

fn verify_promoted_snapshot(expected: &[u8], actual: &[u8]) -> StorageResult<()> {
    let ParsedCandidate::Valid(expected) = parse_candidate(CandidateInput::File(expected.to_vec()))
    else {
        return Err(AppError::ConfigCorrupted);
    };
    let ParsedCandidate::Valid(actual) = parse_candidate(CandidateInput::File(actual.to_vec()))
    else {
        return Err(AppError::ConfigCorrupted);
    };
    if expected.version != SnapshotVersion::V3
        || actual.version != SnapshotVersion::V3
        || expected.data.revision != actual.data.revision
        || expected.data.transaction_id != actual.data.transaction_id
        || expected.normalized != actual.normalized
    {
        return Err(AppError::ConfigCorrupted);
    }
    Ok(())
}

fn recovery_temp_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.recovery.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config.json")
    ))
}

fn main_temp_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config.json")
    ))
}

#[cfg(not(windows))]
fn platform_path(path: &Path) -> io::Result<PathBuf> {
    Ok(path.to_owned())
}

#[cfg(windows)]
fn platform_path(path: &Path) -> io::Result<PathBuf> {
    windows_extended_path(path)
}

#[cfg(not(windows))]
fn durable_replace(source: &Path, target: &Path) -> io::Result<()> {
    fs::rename(source, target)
}

#[cfg(windows)]
fn durable_replace(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = windows_extended_path(source)?;
    let target = windows_extended_path(target)?;
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replacement_barrier(source: &Path, target: &Path) -> io::Result<()> {
    verify_replaced_paths(source, target)?;
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::other("snapshot path has no parent"))?;
    fs::File::open(parent)?.sync_all()
}

#[cfg(windows)]
fn replacement_barrier(source: &Path, target: &Path) -> io::Result<()> {
    verify_replaced_paths(source, target)
}

fn verify_replaced_paths(source: &Path, target: &Path) -> io::Result<()> {
    if !fs::metadata(platform_path(target)?)?.is_file() {
        return Err(io::Error::other("snapshot replacement is not a file"));
    }
    match fs::symlink_metadata(platform_path(source)?) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(io::Error::other("snapshot staging path still exists")),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn windows_extended_path(path: &Path) -> io::Result<PathBuf> {
    use std::{
        ffi::OsString,
        os::windows::ffi::{OsStrExt, OsStringExt},
    };

    const VERBATIM_PREFIX: &[u16] = &[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    const UNC_PREFIX: &[u16] = &[b'\\' as u16, b'\\' as u16];
    const VERBATIM_UNC: &[u16] = &[
        b'\\' as u16,
        b'\\' as u16,
        b'?' as u16,
        b'\\' as u16,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        b'\\' as u16,
    ];

    let absolute = std::path::absolute(path)?;
    let wide = absolute.as_os_str().encode_wide().collect::<Vec<_>>();
    if wide.starts_with(VERBATIM_PREFIX) {
        return Ok(absolute);
    }
    let mut extended = Vec::with_capacity(wide.len() + VERBATIM_UNC.len());
    if wide.starts_with(UNC_PREFIX) {
        extended.extend_from_slice(VERBATIM_UNC);
        extended.extend_from_slice(&wide[UNC_PREFIX.len()..]);
    } else {
        extended.extend_from_slice(VERBATIM_PREFIX);
        extended.extend_from_slice(&wide);
    }
    Ok(PathBuf::from(OsString::from_wide(&extended)))
}

async fn read_candidate(path: &Path) -> StorageResult<CandidateInput> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CandidateInput::Missing);
        }
        Err(_) => return Err(AppError::ConfigIoFailed),
    };
    if !metadata.file_type().is_file() {
        return Ok(CandidateInput::NonFile);
    }
    tokio::fs::read(path)
        .await
        .map(CandidateInput::File)
        .map_err(|_| AppError::ConfigIoFailed)
}

fn parse_candidate(input: CandidateInput) -> ParsedCandidate {
    let bytes = match input {
        CandidateInput::Missing => return ParsedCandidate::Missing,
        CandidateInput::NonFile => return ParsedCandidate::NonFile,
        CandidateInput::File(bytes) => bytes,
    };
    let Ok(raw) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return ParsedCandidate::Corrupt { bytes };
    };
    let raw_version = raw.get("version").and_then(serde_json::Value::as_str);
    if raw_version != Some("1.0") && contains_plaintext_password_key(&raw) {
        return ParsedCandidate::Invalid { bytes };
    }
    let Ok(mut data) = serde_json::from_value::<ConfigData>(raw) else {
        return ParsedCandidate::Corrupt { bytes };
    };
    let version = match data.version.as_str() {
        "1.0" if data.revision == 0 && data.transaction_id.is_empty() => SnapshotVersion::V1,
        "2.0" if data.revision == 0 && data.transaction_id.is_empty() => SnapshotVersion::V2,
        "3.0" if data.revision >= 1 && !data.transaction_id.trim().is_empty() => {
            SnapshotVersion::V3
        }
        _ => return ParsedCandidate::Invalid { bytes },
    };
    let normalized_changed = normalize_config(&mut data);
    let Ok(normalized) = serde_json::to_value(&data) else {
        return ParsedCandidate::Corrupt { bytes };
    };
    ParsedCandidate::Valid(Box::new(Candidate {
        data,
        bytes,
        normalized,
        normalized_changed,
        version,
    }))
}

fn contains_plaintext_password_key(raw: &serde_json::Value) -> bool {
    raw.get("mirrors")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|mirrors| {
            mirrors.iter().any(|mirror| {
                mirror
                    .as_object()
                    .is_some_and(|object| object.contains_key("password"))
            })
        })
}

fn select_candidates(
    main: ParsedCandidate,
    recovery: ParsedCandidate,
) -> StorageResult<SelectedSnapshot> {
    if matches!(recovery, ParsedCandidate::NonFile) {
        return Err(AppError::ConfigIoFailed);
    }
    if matches!(main, ParsedCandidate::Invalid { .. })
        || matches!(recovery, ParsedCandidate::Invalid { .. })
    {
        return Err(AppError::ConfigCorrupted);
    }
    let main_disposition = main_disposition(&main);
    match (main, recovery) {
        (ParsedCandidate::Missing, ParsedCandidate::Missing) => Ok(SelectedSnapshot {
            data: None,
            bytes: None,
            source: SnapshotSource::Defaults,
            needs_v3_rewrite: true,
            main_disposition,
            recovery_disposition: RecoveryDisposition::None,
        }),
        (ParsedCandidate::Corrupt { .. } | ParsedCandidate::NonFile, ParsedCandidate::Missing) => {
            Ok(SelectedSnapshot {
                data: None,
                bytes: None,
                source: SnapshotSource::Defaults,
                needs_v3_rewrite: true,
                main_disposition,
                recovery_disposition: RecoveryDisposition::None,
            })
        }
        (ParsedCandidate::Valid(main), ParsedCandidate::Missing) => {
            select_single(main, SnapshotSource::Main, main_disposition)
        }
        (
            ParsedCandidate::Missing | ParsedCandidate::NonFile | ParsedCandidate::Corrupt { .. },
            ParsedCandidate::Valid(recovery),
        ) => select_single(recovery, SnapshotSource::Recovery, main_disposition),
        (_, ParsedCandidate::Corrupt { .. }) => Err(AppError::ConfigCorrupted),
        (_, ParsedCandidate::Invalid { .. }) | (ParsedCandidate::Invalid { .. }, _) => {
            Err(AppError::ConfigCorrupted)
        }
        (ParsedCandidate::Valid(main), ParsedCandidate::Valid(recovery)) => {
            select_valid_pair(main, recovery, main_disposition)
        }
        (_, ParsedCandidate::NonFile) => Err(AppError::ConfigIoFailed),
    }
}

fn main_disposition(main: &ParsedCandidate) -> MainDisposition {
    match main {
        ParsedCandidate::Missing => MainDisposition::None,
        ParsedCandidate::NonFile => MainDisposition::ReplaceNonFile,
        ParsedCandidate::Valid(candidate) => MainDisposition::Keep {
            bytes: candidate.bytes.clone(),
        },
        ParsedCandidate::Corrupt { bytes } => MainDisposition::BackupCorrupt {
            bytes: bytes.clone(),
        },
        ParsedCandidate::Invalid { bytes } => MainDisposition::BackupCorrupt {
            bytes: bytes.clone(),
        },
    }
}

fn select_single(
    candidate: Box<Candidate>,
    source: SnapshotSource,
    main_disposition: MainDisposition,
) -> StorageResult<SelectedSnapshot> {
    if source == SnapshotSource::Recovery && candidate.version == SnapshotVersion::V1 {
        return Err(AppError::ConfigCorrupted);
    }
    let recovery_disposition = if source == SnapshotSource::Recovery {
        RecoveryDisposition::Promote
    } else {
        RecoveryDisposition::None
    };
    Ok(selected(
        candidate,
        source,
        main_disposition,
        recovery_disposition,
    ))
}

fn select_valid_pair(
    main: Box<Candidate>,
    recovery: Box<Candidate>,
    main_disposition: MainDisposition,
) -> StorageResult<SelectedSnapshot> {
    match (main.version, recovery.version) {
        (SnapshotVersion::V3, SnapshotVersion::V3) => {
            select_v3_pair(main, recovery, main_disposition)
        }
        (SnapshotVersion::V2, SnapshotVersion::V2) if main.normalized == recovery.normalized => {
            Ok(selected(
                main,
                SnapshotSource::Main,
                main_disposition,
                RecoveryDisposition::RemoveRedundant,
            ))
        }
        (SnapshotVersion::V1 | SnapshotVersion::V2, SnapshotVersion::V3) => Ok(selected(
            recovery,
            SnapshotSource::Recovery,
            main_disposition,
            RecoveryDisposition::Promote,
        )),
        (SnapshotVersion::V3, SnapshotVersion::V2) => Ok(selected(
            main,
            SnapshotSource::Main,
            main_disposition,
            RecoveryDisposition::RemoveStale,
        )),
        _ => Err(AppError::ConfigCorrupted),
    }
}

fn select_v3_pair(
    main: Box<Candidate>,
    recovery: Box<Candidate>,
    main_disposition: MainDisposition,
) -> StorageResult<SelectedSnapshot> {
    if main.data.transaction_id == recovery.data.transaction_id {
        if main.data.revision != recovery.data.revision || main.normalized != recovery.normalized {
            return Err(AppError::ConfigCorrupted);
        }
        return Ok(selected(
            main,
            SnapshotSource::Main,
            main_disposition,
            RecoveryDisposition::RemoveRedundant,
        ));
    }
    if main.data.revision == recovery.data.revision {
        return Err(AppError::ConfigCorrupted);
    }
    if main.data.revision > recovery.data.revision {
        Ok(selected(
            main,
            SnapshotSource::Main,
            main_disposition,
            RecoveryDisposition::RemoveStale,
        ))
    } else {
        Ok(selected(
            recovery,
            SnapshotSource::Recovery,
            main_disposition,
            RecoveryDisposition::Promote,
        ))
    }
}

fn selected(
    candidate: Box<Candidate>,
    source: SnapshotSource,
    main_disposition: MainDisposition,
    recovery_disposition: RecoveryDisposition,
) -> SelectedSnapshot {
    let needs_v3_rewrite = candidate.version != SnapshotVersion::V3 || candidate.normalized_changed;
    let Candidate { data, bytes, .. } = *candidate;
    SelectedSnapshot {
        data: Some(data),
        bytes: Some(bytes),
        source,
        needs_v3_rewrite,
        main_disposition,
        recovery_disposition,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::{
        collections::BTreeMap,
        io::{self, Write},
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    fn settings() -> Value {
        json!({
            "defaultOutputDir": "downloads",
            "defaultArch": "amd64",
            "defaultMirror": "registry.example.com",
            "downloadWorkers": 4,
            "lastPackageName": null
        })
    }

    fn snapshot(
        version: &str,
        revision: Option<u64>,
        transaction_id: Option<&str>,
        marker: &str,
    ) -> Vec<u8> {
        let mut value = json!({
            "version": version,
            "settings": settings(),
            "mirrors": [{
                "id": "mirror",
                "name": marker,
                "registry": "registry.example.com",
                "isDefault": true
            }],
            "history": [],
            "pendingCredentialCleanup": []
        });
        if let Some(revision) = revision {
            value["revision"] = json!(revision);
        }
        if let Some(transaction_id) = transaction_id {
            value["transactionId"] = json!(transaction_id);
        }
        serde_json::to_vec_pretty(&value).unwrap()
    }

    fn v1(marker: &str) -> Vec<u8> {
        snapshot("1.0", None, None, marker)
    }

    fn v2(marker: &str) -> Vec<u8> {
        snapshot("2.0", None, None, marker)
    }

    fn v3(revision: u64, transaction_id: &str, marker: &str) -> Vec<u8> {
        snapshot("3.0", Some(revision), Some(transaction_id), marker)
    }

    async fn write_candidates(
        main: Option<&[u8]>,
        recovery: Option<&[u8]>,
    ) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        if let Some(bytes) = main {
            tokio::fs::write(&path, bytes).await.unwrap();
        }
        if let Some(bytes) = recovery {
            tokio::fs::write(super::super::config_recovery_path(&path), bytes)
                .await
                .unwrap();
        }
        (dir, path)
    }

    async fn select(
        main: Option<&[u8]>,
        recovery: Option<&[u8]>,
    ) -> StorageResult<SelectedSnapshot> {
        let (_dir, path) = write_candidates(main, recovery).await;
        load_and_select(&path).await
    }

    fn assert_selected(
        selected: SelectedSnapshot,
        source: SnapshotSource,
        revision: u64,
        rewrite: bool,
    ) {
        assert_eq!(selected.source, source);
        assert_eq!(selected.data.as_ref().unwrap().revision, revision);
        assert_eq!(selected.needs_v3_rewrite, rewrite);
        assert!(selected.bytes.is_some());
    }

    fn assert_corrupted(result: StorageResult<SelectedSnapshot>) {
        assert!(matches!(result, Err(AppError::ConfigCorrupted)));
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Fault {
        RecoveryCreateDir,
        RecoveryCreateFile,
        RecoveryWrite,
        RecoveryFlush,
        RecoverySync,
        RecoveryReplace,
        RecoveryBarrier,
        MainWrite,
        MainFlush,
        MainSync,
        MainReplace,
        MainBarrier,
        MainVerify,
        RecoveryDelete,
    }

    #[derive(Default)]
    struct FakeState {
        files: BTreeMap<PathBuf, Vec<u8>>,
        fault: Option<Fault>,
    }

    #[derive(Clone, Default)]
    struct FakeSnapshotFs {
        state: Arc<Mutex<FakeState>>,
    }

    impl FakeSnapshotFs {
        fn with_fault(fault: Fault) -> Self {
            Self {
                state: Arc::new(Mutex::new(FakeState {
                    fault: Some(fault),
                    ..FakeState::default()
                })),
            }
        }

        fn seed(&self, path: &Path, bytes: &[u8]) {
            self.state
                .lock()
                .unwrap()
                .files
                .insert(path.to_owned(), bytes.to_vec());
        }

        fn bytes(&self, path: &Path) -> Option<Vec<u8>> {
            self.state.lock().unwrap().files.get(path).cloned()
        }

        fn fault(&self) -> Option<Fault> {
            self.state.lock().unwrap().fault
        }

        fn clear_fault(&self) {
            self.state.lock().unwrap().fault = None;
        }
    }

    struct RecoveryBarrierFailingRealFs;

    impl SnapshotFs for RecoveryBarrierFailingRealFs {
        type File = fs::File;

        fn create_dir_all(&self, path: &Path) -> io::Result<()> {
            RealSnapshotFs.create_dir_all(path)
        }

        fn create_file(&self, path: &Path) -> io::Result<Self::File> {
            RealSnapshotFs.create_file(path)
        }

        fn flush_file(&self, file: &mut Self::File) -> io::Result<()> {
            RealSnapshotFs.flush_file(file)
        }

        fn sync_file(&self, file: &Self::File) -> io::Result<()> {
            RealSnapshotFs.sync_file(file)
        }

        fn durable_replace(&self, source: &Path, target: &Path) -> io::Result<()> {
            RealSnapshotFs.durable_replace(source, target)
        }

        fn replacement_barrier(&self, source: &Path, target: &Path) -> io::Result<()> {
            if is_recovery_path(target) {
                Err(io::Error::other("injected durability barrier failure"))
            } else {
                RealSnapshotFs.replacement_barrier(source, target)
            }
        }

        fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
            RealSnapshotFs.read(path)
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            RealSnapshotFs.remove_file(path)
        }
    }

    struct FakeWriter {
        fs: FakeSnapshotFs,
        path: PathBuf,
    }

    impl Write for FakeWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let fault = if is_recovery_temp(&self.path) {
                Fault::RecoveryWrite
            } else {
                Fault::MainWrite
            };
            if self.fs.fault() == Some(fault) {
                return Err(io::Error::other("injected write failure"));
            }
            self.fs
                .state
                .lock()
                .unwrap()
                .files
                .entry(self.path.clone())
                .or_default()
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl SnapshotFs for FakeSnapshotFs {
        type File = FakeWriter;

        fn create_dir_all(&self, _path: &Path) -> io::Result<()> {
            if self.fault() == Some(Fault::RecoveryCreateDir) {
                Err(io::Error::other("injected directory creation failure"))
            } else {
                Ok(())
            }
        }

        fn create_file(&self, path: &Path) -> io::Result<Self::File> {
            if self.fault() == Some(Fault::RecoveryCreateFile) {
                return Err(io::Error::other("injected file creation failure"));
            }
            self.state
                .lock()
                .unwrap()
                .files
                .insert(path.to_owned(), Vec::new());
            Ok(FakeWriter {
                fs: self.clone(),
                path: path.to_owned(),
            })
        }

        fn flush_file(&self, file: &mut Self::File) -> io::Result<()> {
            let fault = if is_recovery_temp(&file.path) {
                Fault::RecoveryFlush
            } else {
                Fault::MainFlush
            };
            if self.fault() == Some(fault) {
                Err(io::Error::other("injected flush failure"))
            } else {
                Ok(())
            }
        }

        fn sync_file(&self, file: &Self::File) -> io::Result<()> {
            let fault = if is_recovery_temp(&file.path) {
                Fault::RecoverySync
            } else {
                Fault::MainSync
            };
            if self.fault() == Some(fault) {
                Err(io::Error::other("injected sync failure"))
            } else {
                Ok(())
            }
        }

        fn durable_replace(&self, source: &Path, target: &Path) -> io::Result<()> {
            let fault = if is_recovery_path(target) {
                Fault::RecoveryReplace
            } else {
                Fault::MainReplace
            };
            if self.fault() == Some(fault) {
                return Err(io::Error::other("injected replace failure"));
            }
            let mut state = self.state.lock().unwrap();
            let bytes = state
                .files
                .remove(source)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing source"))?;
            state.files.insert(target.to_owned(), bytes);
            Ok(())
        }

        fn replacement_barrier(&self, _source: &Path, target: &Path) -> io::Result<()> {
            let fault = if is_recovery_path(target) {
                Fault::RecoveryBarrier
            } else {
                Fault::MainBarrier
            };
            if self.fault() == Some(fault) {
                Err(io::Error::other("injected durability barrier failure"))
            } else {
                Ok(())
            }
        }

        fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
            if self.fault() == Some(Fault::MainVerify) && !is_recovery_path(path) {
                return Ok(b"not-json".to_vec());
            }
            self.bytes(path)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing file"))
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            if self.fault() == Some(Fault::RecoveryDelete) && is_recovery_path(path) {
                return Err(io::Error::other("injected recovery delete failure"));
            }
            self.state
                .lock()
                .unwrap()
                .files
                .remove(path)
                .map(|_| ())
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing file"))
        }
    }

    fn is_recovery_temp(path: &Path) -> bool {
        path.file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(".recovery.tmp"))
    }

    fn is_recovery_path(path: &Path) -> bool {
        path.file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            name.ends_with(".recovery") && !name.ends_with(".recovery.tmp")
        })
    }

    fn fake_paths() -> (PathBuf, PathBuf) {
        let main = PathBuf::from("state/config.json");
        let recovery = super::super::config_recovery_path(&main);
        (main, recovery)
    }

    #[test]
    fn recovery_staging_boundaries_do_not_commit() {
        let (main, recovery) = fake_paths();
        let old = v3(41, "tx-41", "old");
        let next = v3(42, "tx-42", "next");

        for fault in [
            Fault::RecoveryWrite,
            Fault::RecoveryFlush,
            Fault::RecoverySync,
            Fault::RecoveryReplace,
        ] {
            let fs = FakeSnapshotFs::with_fault(fault);
            fs.seed(&recovery, &old);
            let result = commit_recovery_with(&fs, &main, &next);
            assert!(matches!(result, Err(AppError::ConfigIoFailed)), "{fault:?}");
            assert_eq!(
                fs.bytes(&recovery).as_deref(),
                Some(old.as_slice()),
                "{fault:?}"
            );

            fs.clear_fault();
            commit_recovery_with(&fs, &main, &next).unwrap();
            assert_eq!(fs.bytes(&recovery).as_deref(), Some(next.as_slice()));
            assert!(fs.bytes(&recovery_temp_path(&main)).is_none());
        }
    }

    #[test]
    fn recovery_directory_and_file_creation_failures_do_not_commit() {
        let (main, recovery) = fake_paths();
        let old = v3(41, "tx-41", "old");
        let next = v3(42, "tx-42", "next");

        for fault in [Fault::RecoveryCreateDir, Fault::RecoveryCreateFile] {
            let fs = FakeSnapshotFs::with_fault(fault);
            fs.seed(&recovery, &old);

            let result = commit_recovery_with(&fs, &main, &next);

            assert!(matches!(result, Err(AppError::ConfigIoFailed)), "{fault:?}");
            assert_eq!(
                fs.bytes(&recovery).as_deref(),
                Some(old.as_slice()),
                "{fault:?}"
            );
            assert!(fs.bytes(&recovery_temp_path(&main)).is_none(), "{fault:?}");

            fs.clear_fault();
            commit_recovery_with(&fs, &main, &next).unwrap();
            assert_eq!(fs.bytes(&recovery).as_deref(), Some(next.as_slice()));
            assert!(fs.bytes(&recovery_temp_path(&main)).is_none());
        }
    }

    #[test]
    fn recovery_barrier_failure_is_indeterminate_after_replacement() {
        let (main, recovery) = fake_paths();
        let next = v3(42, "tx-42", "next");
        let fs = FakeSnapshotFs::with_fault(Fault::RecoveryBarrier);

        let result = commit_recovery_with(&fs, &main, &next);

        assert!(matches!(result, Err(AppError::ConfigCommitIndeterminate)));
        assert_eq!(fs.bytes(&recovery).as_deref(), Some(next.as_slice()));
    }

    #[tokio::test]
    async fn real_reopen_selects_new_revision_after_indeterminate_recovery_barrier() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        let old = v3(41, "tx-41", "old");
        let next = v3(42, "tx-42", "next");
        std::fs::write(&path, &old).unwrap();

        let result = commit_recovery_with(&RecoveryBarrierFailingRealFs, &path, &next);

        assert!(matches!(result, Err(AppError::ConfigCommitIndeterminate)));
        assert_eq!(std::fs::read(&path).unwrap(), old);
        assert_eq!(
            std::fs::read(super::super::config_recovery_path(&path)).unwrap(),
            next
        );
        assert!(!recovery_temp_path(&path).exists());
        let selected = load_and_select(&path).await.unwrap();
        assert_eq!(selected.source, SnapshotSource::Recovery);
        assert_eq!(selected.data.unwrap().revision, 42);
    }

    #[tokio::test]
    async fn real_recovery_staging_directory_conflict_preserves_old_recovery() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        let recovery = super::super::config_recovery_path(&path);
        let staging = recovery_temp_path(&path);
        let old = v3(41, "tx-41", "old");
        let next = v3(42, "tx-42", "next");
        std::fs::write(&recovery, &old).unwrap();
        std::fs::create_dir(&staging).unwrap();

        let result = commit_recovery(&path, next.clone()).await;

        assert!(matches!(result, Err(AppError::ConfigIoFailed)));
        assert_eq!(std::fs::read(&recovery).unwrap(), old);
        assert!(staging.is_dir());
        let selected = load_and_select(&path).await.unwrap();
        assert_eq!(selected.source, SnapshotSource::Recovery);
        assert_eq!(selected.data.unwrap().revision, 41);

        std::fs::remove_dir(&staging).unwrap();
        commit_recovery(&path, next.clone()).await.unwrap();
        assert_eq!(std::fs::read(&recovery).unwrap(), next);
        assert!(!staging.exists());
    }

    #[tokio::test]
    async fn real_recovery_directory_replace_conflict_preserves_evidence_and_reusable_staging() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        let recovery = super::super::config_recovery_path(&path);
        let staging = recovery_temp_path(&path);
        let evidence = recovery.join("previous-snapshot.json");
        let old = v3(41, "tx-41", "old");
        let next = v3(42, "tx-42", "next");
        std::fs::create_dir(&recovery).unwrap();
        std::fs::write(&evidence, &old).unwrap();

        let result = commit_recovery(&path, next.clone()).await;

        assert!(matches!(result, Err(AppError::ConfigIoFailed)));
        assert_eq!(std::fs::read(&evidence).unwrap(), old);
        assert_eq!(std::fs::read(&staging).unwrap(), next);
        assert!(matches!(
            load_and_select(&path).await,
            Err(AppError::ConfigIoFailed)
        ));

        std::fs::remove_file(&evidence).unwrap();
        std::fs::remove_dir(&recovery).unwrap();
        commit_recovery(&path, next.clone()).await.unwrap();
        assert_eq!(std::fs::read(&recovery).unwrap(), next);
        assert!(!staging.exists());
        let selected = load_and_select(&path).await.unwrap();
        assert_eq!(selected.source, SnapshotSource::Recovery);
        assert_eq!(selected.data.unwrap().revision, 42);
    }

    #[test]
    fn committed_recovery_preserves_exact_revision_bytes() {
        let (main, recovery) = fake_paths();
        let bytes = v3(42, "tx-42", "next");
        let fs = FakeSnapshotFs::default();

        commit_recovery_with(&fs, &main, &bytes).unwrap();

        assert_eq!(fs.bytes(&recovery).as_deref(), Some(bytes.as_slice()));
        assert!(fs.bytes(&recovery_temp_path(&main)).is_none());
    }

    #[test]
    fn every_main_promotion_failure_preserves_authoritative_recovery() {
        let (main, recovery) = fake_paths();
        let bytes = v3(42, "tx-42", "next");

        for fault in [
            Fault::MainWrite,
            Fault::MainFlush,
            Fault::MainSync,
            Fault::MainReplace,
            Fault::MainBarrier,
            Fault::MainVerify,
        ] {
            let fs = FakeSnapshotFs::with_fault(fault);
            fs.seed(&recovery, &bytes);
            let result = promote_main_with(&fs, &main, &bytes);
            assert!(result.is_err(), "{fault:?}");
            assert_eq!(
                fs.bytes(&recovery).as_deref(),
                Some(bytes.as_slice()),
                "{fault:?}"
            );
        }
    }

    #[test]
    fn recovery_is_removed_only_after_main_is_durable_and_verified() {
        let (main, recovery) = fake_paths();
        let bytes = v3(42, "tx-42", "next");
        let fs = FakeSnapshotFs::default();
        fs.seed(&recovery, &bytes);

        promote_main_with(&fs, &main, &bytes).unwrap();

        assert_eq!(fs.bytes(&main).as_deref(), Some(bytes.as_slice()));
        assert!(fs.bytes(&recovery).is_none());
    }

    #[test]
    fn recovery_delete_failure_leaves_equal_snapshots_and_reports_maintenance_error() {
        let (main, recovery) = fake_paths();
        let bytes = v3(42, "tx-42", "next");
        let fs = FakeSnapshotFs::with_fault(Fault::RecoveryDelete);
        fs.seed(&recovery, &bytes);

        let result = promote_main_with(&fs, &main, &bytes);

        assert!(matches!(result, Err(AppError::ConfigIoFailed)));
        assert_eq!(fs.bytes(&main).as_deref(), Some(bytes.as_slice()));
        assert_eq!(fs.bytes(&recovery).as_deref(), Some(bytes.as_slice()));
    }

    #[tokio::test]
    async fn real_promotion_handles_missing_and_regular_main_paths() {
        for existing in [None, Some(v3(41, "tx-41", "old"))] {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("config.json");
            if let Some(existing) = existing {
                tokio::fs::write(&path, existing).await.unwrap();
            }
            let bytes = v3(42, "tx-42", "next");

            commit_recovery(&path, bytes.clone()).await.unwrap();
            promote_main(&path, bytes.clone()).await.unwrap();

            assert_eq!(tokio::fs::read(&path).await.unwrap(), bytes);
            assert!(
                !tokio::fs::try_exists(super::super::config_recovery_path(&path))
                    .await
                    .unwrap()
            );
        }
    }

    #[tokio::test]
    async fn real_directory_main_failure_preserves_recovery_for_restart_selection() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        tokio::fs::create_dir(&path).await.unwrap();
        let bytes = v3(42, "tx-42", "next");
        commit_recovery(&path, bytes.clone()).await.unwrap();

        assert!(promote_main(&path, bytes.clone()).await.is_err());
        assert_eq!(
            tokio::fs::read(super::super::config_recovery_path(&path))
                .await
                .unwrap(),
            bytes
        );
        let selected = load_and_select(&path).await.unwrap();
        assert_eq!(selected.source, SnapshotSource::Recovery);
        assert_eq!(selected.data.unwrap().revision, 42);

        let staging = main_temp_path(&path);
        assert_eq!(tokio::fs::read(&staging).await.unwrap(), bytes);
        tokio::fs::remove_dir(&path).await.unwrap();
        promote_main(&path, tokio::fs::read(&staging).await.unwrap())
            .await
            .unwrap();
        assert!(!tokio::fs::try_exists(&staging).await.unwrap());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn locked_main_failure_preserves_recovery() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        tokio::fs::write(&path, v3(41, "tx-41", "old"))
            .await
            .unwrap();
        let bytes = v3(42, "tx-42", "next");
        commit_recovery(&path, bytes.clone()).await.unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();

        assert!(promote_main(&path, bytes.clone()).await.is_err());
        assert_eq!(
            tokio::fs::read(super::super::config_recovery_path(&path))
                .await
                .unwrap(),
            bytes
        );
        drop(lock);
        let selected = load_and_select(&path).await.unwrap();
        assert_eq!(selected.source, SnapshotSource::Recovery);
        assert_eq!(selected.data.unwrap().revision, 42);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn real_commit_and_promotion_support_paths_beyond_max_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut nested = dir.path().to_owned();
        while nested.as_os_str().to_string_lossy().len() <= 280 {
            nested.push("revisioned-config-segment");
        }
        std::fs::create_dir_all(windows_extended_path(&nested).unwrap()).unwrap();
        let path = nested.join("config.json");
        let bytes = v3(42, "tx-42", "next");

        commit_recovery(&path, bytes.clone()).await.unwrap();
        promote_main(&path, bytes.clone()).await.unwrap();

        assert_eq!(
            std::fs::read(windows_extended_path(&path).unwrap()).unwrap(),
            bytes
        );
    }

    #[tokio::test]
    async fn selects_main_when_it_is_the_only_candidate() {
        let main = v3(1, "tx-main", "main");
        assert_selected(
            select(Some(&main), None).await.unwrap(),
            SnapshotSource::Main,
            1,
            false,
        );
    }

    #[tokio::test]
    async fn selects_recovery_when_it_is_the_only_candidate() {
        let recovery = v3(1, "tx-recovery", "recovery");
        assert_selected(
            select(None, Some(&recovery)).await.unwrap(),
            SnapshotSource::Recovery,
            1,
            false,
        );
    }

    #[tokio::test]
    async fn selects_recovery_with_higher_revision() {
        let main = v3(41, "tx-41", "old");
        let recovery = v3(42, "tx-42", "new");
        let selected = select(Some(&main), Some(&recovery)).await.unwrap();
        assert_eq!(selected.recovery_disposition, RecoveryDisposition::Promote);
        assert_selected(selected, SnapshotSource::Recovery, 42, false);
    }

    #[tokio::test]
    async fn selects_main_with_higher_revision() {
        let main = v3(42, "tx-42", "new");
        let recovery = v3(41, "tx-41", "old");
        let selected = select(Some(&main), Some(&recovery)).await.unwrap();
        assert_eq!(
            selected.recovery_disposition,
            RecoveryDisposition::RemoveStale
        );
        assert_selected(selected, SnapshotSource::Main, 42, false);
    }

    #[tokio::test]
    async fn equal_revision_transaction_and_normalized_content_are_equivalent() {
        let main = v3(42, "same-tx", "same");
        let mut recovery_value: Value = serde_json::from_slice(&main).unwrap();
        recovery_value["mirrors"][0]["registry"] = json!("registry.example.com");
        let recovery = serde_json::to_vec(&recovery_value).unwrap();
        let selected = select(Some(&main), Some(&recovery)).await.unwrap();
        assert_eq!(
            selected.recovery_disposition,
            RecoveryDisposition::RemoveRedundant
        );
        assert_selected(selected, SnapshotSource::Main, 42, false);
    }

    #[tokio::test]
    async fn equal_revision_with_different_transactions_is_corrupt() {
        let main = v3(42, "tx-a", "same");
        let recovery = v3(42, "tx-b", "same");
        assert_corrupted(select(Some(&main), Some(&recovery)).await);
    }

    #[tokio::test]
    async fn same_transaction_across_revisions_is_corrupt() {
        let main = v3(41, "same-tx", "old");
        let recovery = v3(42, "same-tx", "new");
        assert_corrupted(select(Some(&main), Some(&recovery)).await);
    }

    #[tokio::test]
    async fn same_revision_and_transaction_with_different_normalized_content_is_corrupt() {
        let main = v3(42, "same-tx", "one");
        let recovery = v3(42, "same-tx", "two");
        assert_corrupted(select(Some(&main), Some(&recovery)).await);
    }

    #[tokio::test]
    async fn different_v2_candidates_are_corrupt() {
        let main = v2("one");
        let recovery = v2("two");
        assert_corrupted(select(Some(&main), Some(&recovery)).await);
    }

    #[tokio::test]
    async fn equivalent_v2_candidates_can_be_selected_for_rewrite() {
        let main = v2("same");
        let mut recovery_value: Value = serde_json::from_slice(&main).unwrap();
        recovery_value["mirrors"][0]["registry"] = json!("registry.example.com");
        let recovery = serde_json::to_vec(&recovery_value).unwrap();
        assert_selected(
            select(Some(&main), Some(&recovery)).await.unwrap(),
            SnapshotSource::Main,
            0,
            true,
        );
    }

    #[tokio::test]
    async fn v3_recovery_wins_over_legacy_main_after_migration_commit() {
        for main in [v1("legacy"), v2("legacy")] {
            let recovery = v3(1, "migration-tx", "migrated");
            assert_selected(
                select(Some(&main), Some(&recovery)).await.unwrap(),
                SnapshotSource::Recovery,
                1,
                false,
            );
        }
    }

    #[tokio::test]
    async fn v3_main_wins_over_valid_v2_recovery() {
        let main = v3(2, "current-tx", "current");
        let recovery = v2("stale");
        assert_selected(
            select(Some(&main), Some(&recovery)).await.unwrap(),
            SnapshotSource::Main,
            2,
            false,
        );
    }

    #[tokio::test]
    async fn v3_main_with_v1_recovery_is_corrupt() {
        let main = v3(2, "current-tx", "current");
        let recovery = v1("invalid-recovery");
        assert_corrupted(select(Some(&main), Some(&recovery)).await);
    }

    #[tokio::test]
    async fn valid_main_with_corrupt_recovery_is_corrupt() {
        let main = v3(2, "current-tx", "current");
        assert_corrupted(select(Some(&main), Some(b"not-json")).await);
    }

    #[tokio::test]
    async fn corrupt_main_with_valid_recovery_selects_recovery() {
        let recovery = v3(2, "recovery-tx", "current");
        let selected = select(Some(b"not-json"), Some(&recovery)).await.unwrap();
        assert_eq!(
            selected.main_disposition,
            MainDisposition::BackupCorrupt {
                bytes: b"not-json".to_vec()
            }
        );
        assert_selected(selected, SnapshotSource::Recovery, 2, false);
    }

    #[tokio::test]
    async fn corrupt_main_without_recovery_selects_defaults_and_preserves_backup_evidence() {
        let corrupt = b"not-json";
        let selected = select(Some(corrupt), None).await.unwrap();

        assert_eq!(selected.source, SnapshotSource::Defaults);
        assert!(selected.data.is_none());
        assert!(selected.bytes.is_none());
        assert!(selected.needs_v3_rewrite);
        assert_eq!(
            selected.main_disposition,
            MainDisposition::BackupCorrupt {
                bytes: corrupt.to_vec()
            }
        );
    }

    #[tokio::test]
    async fn main_directory_with_valid_recovery_selects_recovery_for_non_file_replacement() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        tokio::fs::create_dir(&path).await.unwrap();
        let recovery = v3(2, "recovery-tx", "current");
        tokio::fs::write(super::super::config_recovery_path(&path), &recovery)
            .await
            .unwrap();

        let selected = load_and_select(&path).await.unwrap();
        assert_eq!(selected.source, SnapshotSource::Recovery);
        assert_eq!(selected.main_disposition, MainDisposition::ReplaceNonFile);
        assert_eq!(selected.bytes.as_deref(), Some(recovery.as_slice()));
    }

    #[tokio::test]
    async fn recovery_directory_is_an_io_failure() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        tokio::fs::write(&path, v3(2, "main-tx", "current"))
            .await
            .unwrap();
        tokio::fs::create_dir(super::super::config_recovery_path(&path))
            .await
            .unwrap();

        let result = load_and_select(&path).await;
        assert!(matches!(result, Err(AppError::ConfigIoFailed)));
    }

    #[tokio::test]
    async fn mixed_v1_v2_candidates_are_corrupt() {
        let main = v1("legacy-one");
        let recovery = v2("legacy-two");
        assert_corrupted(select(Some(&main), Some(&recovery)).await);
    }

    #[tokio::test]
    async fn v1_recovery_is_corrupt_even_without_a_main_candidate() {
        let recovery = v1("invalid-recovery");
        assert_corrupted(select(None, Some(&recovery)).await);
    }

    #[tokio::test]
    async fn conflicting_candidates_are_preserved_byte_for_byte() {
        let main = v3(42, "tx-a", "main");
        let recovery = v3(42, "tx-b", "recovery");
        let (_dir, path) = write_candidates(Some(&main), Some(&recovery)).await;

        assert_corrupted(load_and_select(&path).await);
        assert_eq!(tokio::fs::read(&path).await.unwrap(), main);
        assert_eq!(
            tokio::fs::read(super::super::config_recovery_path(&path))
                .await
                .unwrap(),
            recovery
        );
    }

    #[tokio::test]
    async fn sole_legacy_candidates_use_revision_zero_and_require_rewrite() {
        for (source, main, recovery) in [
            (SnapshotSource::Main, Some(v1("v1")), None),
            (SnapshotSource::Main, Some(v2("v2")), None),
            (SnapshotSource::Recovery, None, Some(v2("v2"))),
        ] {
            let main = main.as_deref();
            let recovery = recovery.as_deref();
            assert_selected(select(main, recovery).await.unwrap(), source, 0, true);
        }
    }

    #[tokio::test]
    async fn malformed_v3_metadata_is_corrupt() {
        for bytes in [v3(0, "tx", "bad-revision"), v3(1, "", "bad-transaction")] {
            assert_corrupted(select(Some(&bytes), None).await);
        }
    }

    #[tokio::test]
    async fn v2_and_v3_reject_any_plaintext_password_key() {
        for (version, revision, transaction_id) in
            [("2.0", None, None), ("3.0", Some(1), Some("tx"))]
        {
            for password in [Value::Null, json!(""), json!("leaked")] {
                let mut value: Value =
                    serde_json::from_slice(&snapshot(version, revision, transaction_id, "mirror"))
                        .unwrap();
                value["mirrors"][0]["password"] = password;
                let bytes = serde_json::to_vec(&value).unwrap();
                let (_dir, path) = write_candidates(Some(&bytes), None).await;
                assert_corrupted(load_and_select(&path).await);
                assert_eq!(tokio::fs::read(path).await.unwrap(), bytes);
            }
        }
    }

    #[tokio::test]
    async fn v1_plaintext_password_remains_available_for_credential_migration() {
        let mut value: Value = serde_json::from_slice(&v1("legacy")).unwrap();
        value["mirrors"][0]["password"] = json!("legacy-secret");
        let bytes = serde_json::to_vec(&value).unwrap();

        let selected = select(Some(&bytes), None).await.unwrap();
        assert_eq!(selected.source, SnapshotSource::Main);
        assert!(selected.needs_v3_rewrite);
        assert_eq!(
            selected.data.as_ref().unwrap().mirrors[0]
                .password
                .as_deref(),
            Some("legacy-secret")
        );
    }

    #[tokio::test]
    async fn password_only_difference_cannot_hide_in_canonical_v3_content() {
        let main = v3(42, "same-tx", "same");
        let mut value: Value = serde_json::from_slice(&main).unwrap();
        value["mirrors"][0]["password"] = json!("leaked");
        let recovery = serde_json::to_vec(&value).unwrap();
        let (_dir, path) = write_candidates(Some(&main), Some(&recovery)).await;

        assert_corrupted(load_and_select(&path).await);
        assert_eq!(tokio::fs::read(&path).await.unwrap(), main);
        assert_eq!(
            tokio::fs::read(super::super::config_recovery_path(&path))
                .await
                .unwrap(),
            recovery
        );
    }

    #[tokio::test]
    async fn v3_registry_normalization_marks_rewrite_and_preserves_raw_bytes() {
        let mut value: Value = serde_json::from_slice(&v3(1, "tx", "mirror")).unwrap();
        value["mirrors"][0]["registry"] = json!("HTTPS://REGISTRY.EXAMPLE.COM/");
        let raw = serde_json::to_vec_pretty(&value).unwrap();

        let selected = select(Some(&raw), None).await.unwrap();
        assert!(selected.needs_v3_rewrite);
        assert_eq!(
            selected.data.as_ref().unwrap().mirrors[0].registry,
            "registry.example.com"
        );
        assert_eq!(selected.bytes.as_deref(), Some(raw.as_slice()));
    }

    #[tokio::test]
    async fn v3_default_repair_marks_rewrite_and_returns_normalized_data() {
        let mut value: Value = serde_json::from_slice(&v3(1, "tx", "mirror")).unwrap();
        value["mirrors"][0]["isDefault"] = json!(false);
        value["settings"]["defaultMirror"] = json!("wrong.example.com");
        let raw = serde_json::to_vec(&value).unwrap();

        let selected = select(Some(&raw), None).await.unwrap();
        let data = selected.data.as_ref().unwrap();
        assert!(selected.needs_v3_rewrite);
        assert!(data.mirrors[0].is_default);
        assert_eq!(data.settings.default_mirror, "registry.example.com");
        assert_eq!(selected.bytes.as_deref(), Some(raw.as_slice()));
    }

    #[tokio::test]
    async fn v3_history_cap_marks_rewrite_and_returns_normalized_data() {
        let mut value: Value = serde_json::from_slice(&v3(1, "tx", "mirror")).unwrap();
        value["history"] = Value::Array(
            (0..101)
                .map(|index| {
                    json!({
                        "id": format!("history-{index}"),
                        "packageName": "library/alpine",
                        "tag": "latest",
                        "arch": "amd64",
                        "mirror": "registry.example.com",
                        "fullImage": "registry.example.com/library/alpine:latest",
                        "outputPath": null,
                        "size": null,
                        "status": "completed",
                        "downloadedAt": null,
                        "error": null
                    })
                })
                .collect(),
        );
        let raw = serde_json::to_vec(&value).unwrap();

        let selected = select(Some(&raw), None).await.unwrap();
        assert!(selected.needs_v3_rewrite);
        assert_eq!(selected.data.as_ref().unwrap().history.len(), 100);
        assert_eq!(selected.bytes.as_deref(), Some(raw.as_slice()));
    }

    #[tokio::test]
    async fn missing_candidates_select_defaults_without_creating_files() {
        let (dir, path) = write_candidates(None, None).await;
        let selected = load_and_select(&path).await.unwrap();
        assert_eq!(selected.source, SnapshotSource::Defaults);
        assert!(selected.data.is_none());
        assert!(selected.bytes.is_none());
        assert!(selected.needs_v3_rewrite);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn main_disposition_debug_redacts_evidence_bytes() {
        let disposition = MainDisposition::BackupCorrupt {
            bytes: b"plaintext-secret".to_vec(),
        };

        assert_eq!(format!("{disposition:?}"), "BackupCorrupt { byte_len: 16 }");
    }

    #[tokio::test]
    async fn selected_v1_config_debug_redacts_legacy_credentials() {
        let mut value: Value = serde_json::from_slice(&v1("legacy")).unwrap();
        value["mirrors"][0]["username"] = json!("private-user-marker");
        value["mirrors"][0]["credentialRef"] = json!("private-reference-marker");
        value["mirrors"][0]["password"] = json!("plaintext-secret-marker");
        let bytes = serde_json::to_vec(&value).unwrap();

        let selected = select(Some(&bytes), None).await.unwrap();
        let debug = format!("{:?}", selected.data);
        let debug_lower = debug.to_ascii_lowercase();
        assert!(!debug_lower.contains("password"), "{debug}");
        assert!(!debug.contains("plaintext-secret-marker"), "{debug}");
        assert!(!debug.contains("private-user-marker"), "{debug}");
        assert!(!debug.contains("private-reference-marker"), "{debug}");
    }
}
