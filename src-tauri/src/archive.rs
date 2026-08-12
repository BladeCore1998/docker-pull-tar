use crate::{domain::ResolvedImage, error::AppError};
use flate2::read::GzDecoder;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
use tar::{Builder, Header};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const COPY_BUFFER_SIZE: usize = 64 * 1024;
const DOCKER_GZIP: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";
const OCI_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
const DOCKER_TAR: &str = "application/vnd.docker.image.rootfs.diff.tar";
const OCI_TAR: &str = "application/vnd.oci.image.layer.v1.tar";

#[derive(Debug, Clone)]
pub struct ArchiveRequest {
    pub image: ResolvedImage,
    pub config_path: PathBuf,
    pub layer_paths: Vec<PathBuf>,
    pub output_dir: PathBuf,
    pub output_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveResult {
    pub path: PathBuf,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayerEncoding {
    Gzip,
    Uncompressed,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ManifestEntry<'a> {
    config: &'a str,
    repo_tags: Option<Vec<String>>,
    layers: Vec<String>,
}

#[derive(Serialize)]
struct LegacyLayer<'a> {
    id: &'a str,
    parent: Option<&'a str>,
}

pub async fn build_archive(
    request: ArchiveRequest,
    cancellation: &CancellationToken,
) -> Result<ArchiveResult, AppError> {
    validate_request(&request)?;
    check_cancelled(cancellation)?;
    let cancellation = cancellation.clone();
    tokio::task::spawn_blocking(move || build_archive_sync(request, &cancellation))
        .await
        .map_err(|_| AppError::OutputNotWritable)?
}

pub async fn estimate_unpacked_size(
    image: &ResolvedImage,
    layer_paths: &[PathBuf],
    cancellation: &CancellationToken,
) -> Result<u64, AppError> {
    validate_layers(image, layer_paths)?;
    check_cancelled(cancellation)?;
    let image = image.clone();
    let layer_paths = layer_paths.to_vec();
    let cancellation = cancellation.clone();
    tokio::task::spawn_blocking(move || {
        estimate_unpacked_size_sync(&image, &layer_paths, &cancellation)
    })
    .await
    .map_err(|_| AppError::OutputNotWritable)?
}

fn build_archive_sync(
    request: ArchiveRequest,
    cancellation: &CancellationToken,
) -> Result<ArchiveResult, AppError> {
    check_cancelled(cancellation)?;
    std::fs::create_dir_all(&request.output_dir).map_err(map_io)?;
    let temporary = request
        .output_dir
        .join(format!(".docker-pull-tar-{}.tmp", Uuid::new_v4()));
    let result = build_temporary(&request, &temporary, cancellation)
        .and_then(|()| promote(&temporary, &request.output_dir, &request.output_name));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn build_temporary(
    request: &ArchiveRequest,
    temporary: &Path,
    cancellation: &CancellationToken,
) -> Result<(), AppError> {
    let layer_ids = legacy_layer_ids(&request.image);
    let config_name = format!("{}.json", digest_hex(&request.image.config.digest)?);
    let layers = layer_ids
        .iter()
        .map(|id| format!("{id}/layer.tar"))
        .collect::<Vec<_>>();
    let tag = (!is_digest_reference(&request.image.reference))
        .then(|| format!("{}:{}", request.image.repository, request.image.reference));
    let manifest = serde_json::to_vec(&vec![ManifestEntry {
        config: &config_name,
        repo_tags: tag.map(|tag| vec![tag]),
        layers,
    }])
    .map_err(map_json)?;
    let repositories = if is_digest_reference(&request.image.reference) {
        BTreeMap::new()
    } else {
        BTreeMap::from([(
            request.image.repository.clone(),
            BTreeMap::from([(
                request.image.reference.clone(),
                layer_ids.last().cloned().unwrap_or_default(),
            )]),
        )])
    };
    let repositories = serde_json::to_vec(&repositories).map_err(map_json)?;

    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(temporary)
        .map_err(map_io)?;
    let mut archive = Builder::new(file);
    append_file(
        &mut archive,
        &config_name,
        &request.config_path,
        cancellation,
    )?;
    append_bytes(&mut archive, "manifest.json", &manifest)?;
    append_bytes(&mut archive, "repositories", &repositories)?;

    for (index, layer_id) in layer_ids.iter().enumerate() {
        check_cancelled(cancellation)?;
        let parent = index
            .checked_sub(1)
            .map(|parent| layer_ids[parent].as_str());
        let metadata = serde_json::to_vec(&LegacyLayer {
            id: layer_id,
            parent,
        })
        .map_err(map_json)?;
        append_bytes(&mut archive, &format!("{layer_id}/json"), &metadata)?;
        append_layer(
            &mut archive,
            &format!("{layer_id}/layer.tar"),
            &request.layer_paths[index],
            media_encoding(&request.image.layers[index].media_type)?,
            cancellation,
        )?;
    }

    check_cancelled(cancellation)?;
    archive.finish().map_err(map_io)?;
    let mut file = archive.into_inner().map_err(map_io)?;
    file.flush().map_err(map_io)?;
    file.sync_all().map_err(map_io)?;
    check_cancelled(cancellation)
}

fn append_bytes(builder: &mut Builder<File>, path: &str, bytes: &[u8]) -> Result<(), AppError> {
    validate_archive_entry_path(path)?;
    let mut header = regular_header(bytes.len() as u64);
    builder
        .append_data(&mut header, path, bytes)
        .map_err(map_io)
}

fn append_file(
    builder: &mut Builder<File>,
    archive_path: &str,
    source: &Path,
    cancellation: &CancellationToken,
) -> Result<(), AppError> {
    validate_archive_entry_path(archive_path)?;
    let size = std::fs::metadata(source).map_err(map_io)?.len();
    let file = File::open(source).map_err(map_io)?;
    let mut reader = CancellationReader::new(file, cancellation);
    let mut header = regular_header(size);
    builder
        .append_data(&mut header, archive_path, &mut reader)
        .map_err(map_reader_io)
}

fn append_layer(
    builder: &mut Builder<File>,
    archive_path: &str,
    source: &Path,
    encoding: LayerEncoding,
    cancellation: &CancellationToken,
) -> Result<(), AppError> {
    validate_archive_entry_path(archive_path)?;
    match encoding {
        LayerEncoding::Uncompressed => append_file(builder, archive_path, source, cancellation),
        LayerEncoding::Gzip => {
            let size = decompressed_size(source, cancellation)?;
            let file = File::open(source).map_err(map_io)?;
            let decoder = GzDecoder::new(CancellationReader::new(file, cancellation));
            let mut reader = CancellationReader::new(decoder, cancellation);
            let mut header = regular_header(size);
            builder
                .append_data(&mut header, archive_path, &mut reader)
                .map_err(map_reader_io)
        }
    }
}

fn decompressed_size(source: &Path, cancellation: &CancellationToken) -> Result<u64, AppError> {
    let file = File::open(source).map_err(map_io)?;
    let decoder = GzDecoder::new(CancellationReader::new(file, cancellation));
    let mut reader = CancellationReader::new(decoder, cancellation);
    io::copy(&mut reader, &mut io::sink()).map_err(map_reader_io)
}

fn estimate_unpacked_size_sync(
    image: &ResolvedImage,
    layer_paths: &[PathBuf],
    cancellation: &CancellationToken,
) -> Result<u64, AppError> {
    image
        .layers
        .iter()
        .zip(layer_paths)
        .try_fold(0_u64, |total, (layer, path)| {
            check_cancelled(cancellation)?;
            let size = match media_encoding(&layer.media_type)? {
                LayerEncoding::Gzip => decompressed_size(path, cancellation)?,
                LayerEncoding::Uncompressed => std::fs::metadata(path).map_err(map_io)?.len(),
            };
            total.checked_add(size).ok_or(AppError::OutputNotWritable)
        })
}

fn regular_header(size: u64) -> Header {
    let mut header = Header::new_gnu();
    header.set_size(size);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    header
}

fn promote(
    temporary: &Path,
    output_dir: &Path,
    output_name: &str,
) -> Result<ArchiveResult, AppError> {
    promote_with_hook(temporary, output_dir, output_name, |_| {})
}

fn promote_with_hook<F>(
    temporary: &Path,
    output_dir: &Path,
    output_name: &str,
    mut before_promote: F,
) -> Result<ArchiveResult, AppError>
where
    F: FnMut(&Path),
{
    for suffix in 0u32.. {
        let filename = if suffix == 0 {
            format!("{output_name}.tar")
        } else {
            format!("{output_name}-{suffix}.tar")
        };
        let destination = output_dir.join(filename);
        before_promote(&destination);
        match atomic_promote_no_replace(temporary, &destination) {
            Ok(()) => {
                let size = promoted_metadata(&destination).map_err(map_io)?.len();
                return Ok(ArchiveResult {
                    path: destination,
                    size,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(map_io(error)),
        }
    }
    unreachable!("u32 archive suffix space exhausted")
}

fn validate_request(request: &ArchiveRequest) -> Result<(), AppError> {
    validate_output_name(&request.output_name)?;
    digest_hex(&request.image.config.digest)?;
    validate_layers(&request.image, &request.layer_paths)
}

fn validate_layers(image: &ResolvedImage, layer_paths: &[PathBuf]) -> Result<(), AppError> {
    if image.layers.len() != layer_paths.len() {
        return Err(AppError::OutputNotWritable);
    }
    for layer in &image.layers {
        digest_hex(&layer.digest)?;
        media_encoding(&layer.media_type)?;
    }
    Ok(())
}

fn validate_output_name(name: &str) -> Result<(), AppError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(AppError::OutputNotWritable);
    }
    Ok(())
}

fn validate_archive_entry_path(path: &str) -> Result<(), AppError> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || !path.is_relative()
        || path.to_string_lossy().contains(['\\', ':'])
        || !path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
    {
        return Err(AppError::OutputNotWritable);
    }
    Ok(())
}

#[cfg(unix)]
fn atomic_promote_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::hard_link(source, destination)?;
    if let Err(error) = std::fs::remove_file(source) {
        let _ = std::fs::remove_file(destination);
        return Err(error);
    }
    Ok(())
}

#[cfg(windows)]
fn atomic_promote_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    let source = windows_extended_path(source)?;
    let destination = windows_extended_path(destination)?;
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if result != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(80) | Some(183)) {
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "archive destination already exists",
        ))
    } else {
        Err(error)
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

#[cfg(windows)]
fn promoted_metadata(path: &Path) -> io::Result<std::fs::Metadata> {
    std::fs::metadata(windows_extended_path(path)?)
}

#[cfg(not(windows))]
fn promoted_metadata(path: &Path) -> io::Result<std::fs::Metadata> {
    std::fs::metadata(path)
}

#[cfg(not(any(unix, windows)))]
fn atomic_promote_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::hard_link(source, destination)?;
    if let Err(error) = std::fs::remove_file(source) {
        let _ = std::fs::remove_file(destination);
        return Err(error);
    }
    Ok(())
}

fn legacy_layer_ids(image: &ResolvedImage) -> Vec<String> {
    let mut parent = String::new();
    image
        .layers
        .iter()
        .map(|layer| {
            let id = format!(
                "{:x}",
                Sha256::digest(format!("{}\n{}\n", parent, layer.digest))
            );
            parent.clone_from(&id);
            id
        })
        .collect()
}

fn digest_hex(digest: &str) -> Result<&str, AppError> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return Err(AppError::DigestMismatch);
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::DigestMismatch);
    }
    Ok(hex)
}

fn is_digest_reference(reference: &str) -> bool {
    digest_hex(reference).is_ok()
}

fn media_encoding(media_type: &str) -> Result<LayerEncoding, AppError> {
    match media_type {
        DOCKER_GZIP | OCI_GZIP => Ok(LayerEncoding::Gzip),
        DOCKER_TAR | OCI_TAR => Ok(LayerEncoding::Uncompressed),
        _ => Err(AppError::UnsupportedLayerMediaType),
    }
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), AppError> {
    if cancellation.is_cancelled() {
        Err(AppError::TaskCancelled)
    } else {
        Ok(())
    }
}

struct CancellationReader<'a, R> {
    inner: R,
    cancellation: &'a CancellationToken,
}

impl<'a, R> CancellationReader<'a, R> {
    fn new(inner: R, cancellation: &'a CancellationToken) -> Self {
        Self {
            inner,
            cancellation,
        }
    }
}

impl<R: Read> Read for CancellationReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.cancellation.is_cancelled() {
            return Err(cancelled_io());
        }
        let limit = buffer.len().min(COPY_BUFFER_SIZE);
        let read = self.inner.read(&mut buffer[..limit])?;
        if self.cancellation.is_cancelled() {
            return Err(cancelled_io());
        }
        Ok(read)
    }
}

fn cancelled_io() -> io::Error {
    io::Error::other(ArchiveCancelled)
}

fn map_reader_io(error: io::Error) -> AppError {
    if error
        .get_ref()
        .and_then(|source| source.downcast_ref::<ArchiveCancelled>())
        .is_some()
    {
        AppError::TaskCancelled
    } else {
        map_io(error)
    }
}

#[derive(Debug)]
struct ArchiveCancelled;

impl std::fmt::Display for ArchiveCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("archive creation cancelled")
    }
}

impl std::error::Error for ArchiveCancelled {}

fn map_io(_: io::Error) -> AppError {
    AppError::OutputNotWritable
}

fn map_json(_: serde_json::Error) -> AppError {
    AppError::OutputNotWritable
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn cancellation_read_error_is_not_retryable_by_io_copy() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let mut reader = CancellationReader::new(io::Cursor::new(b"layer"), &cancellation);

        let error = reader.read(&mut [0; 8]).unwrap_err();

        assert_ne!(error.kind(), io::ErrorKind::Interrupted);
        assert!(matches!(map_reader_io(error), AppError::TaskCancelled));
    }

    #[test]
    fn promotion_retries_when_candidate_appears_after_selection() {
        let dir = TempDir::new().unwrap();
        let temporary = dir.path().join("archive.tmp");
        std::fs::write(&temporary, b"new archive").unwrap();
        let first = dir.path().join("image.tar");
        let mut injected = false;
        let result = promote_with_hook(&temporary, dir.path(), "image", |candidate| {
            if !injected {
                injected = true;
                std::fs::write(candidate, b"existing archive").unwrap();
            }
        })
        .unwrap();

        assert_eq!(std::fs::read(&first).unwrap(), b"existing archive");
        assert_eq!(result.path, dir.path().join("image-1.tar"));
        assert_eq!(std::fs::read(result.path).unwrap(), b"new archive");
    }

    #[test]
    fn archive_entry_paths_reject_absolute_prefix_and_parent_components() {
        for path in [
            "/absolute",
            "../parent",
            "nested/../parent",
            r"C:\absolute",
            r"\\server\share\archive",
            "C:relative",
            "./current",
        ] {
            assert!(
                validate_archive_entry_path(path).is_err(),
                "accepted unsafe path {path:?}"
            );
        }
        assert!(validate_archive_entry_path("layer-id/layer.tar").is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_are_absolute_extended_and_never_double_prefixed() {
        assert_eq!(
            windows_extended_path(Path::new(r"C:\images\archive.tmp")).unwrap(),
            PathBuf::from(r"\\?\C:\images\archive.tmp")
        );
        assert_eq!(
            windows_extended_path(Path::new(r"\\server\share\archive.tmp")).unwrap(),
            PathBuf::from(r"\\?\UNC\server\share\archive.tmp")
        );
        assert_eq!(
            windows_extended_path(Path::new(r"\\?\C:\images\archive.tmp")).unwrap(),
            PathBuf::from(r"\\?\C:\images\archive.tmp")
        );
        assert_eq!(
            windows_extended_path(Path::new(r"\\?\UNC\server\share\archive.tmp")).unwrap(),
            PathBuf::from(r"\\?\UNC\server\share\archive.tmp")
        );

        let relative = windows_extended_path(Path::new(r".\archive-test\..\archive.tmp"))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(relative.starts_with(r"\\?\"));
        assert!(relative.ends_with(r"\archive.tmp"));
        assert!(!relative.contains(r"\..\"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_promotion_supports_paths_beyond_max_path() {
        let dir = TempDir::new().unwrap();
        let mut output_dir = dir.path().to_owned();
        while output_dir.as_os_str().len() < 280 {
            output_dir.push("long-archive-directory");
        }
        std::fs::create_dir_all(windows_extended_path(&output_dir).unwrap()).unwrap();
        let temporary = output_dir.join("archive.tmp");
        std::fs::write(windows_extended_path(&temporary).unwrap(), b"archive").unwrap();

        let result = promote(&temporary, &output_dir, "image").unwrap();

        assert!(result.path.as_os_str().len() > 260);
        assert_eq!(
            std::fs::read(windows_extended_path(&result.path).unwrap()).unwrap(),
            b"archive"
        );
    }
}
