/// Container and local environment implementations.
pub mod container;
/// Local environment implementation.
pub mod local;

pub use container::{Container, ContainerConfig, ContainerFarm};
pub use local::{LocalEnv, LocalFarm};

use edo::util::Reader;
use futures::AsyncReadExt as FuturesAsyncReadExt;
use snafu::{ResultExt, Snafu};
use std::path::{Component, Path, PathBuf};
use tokio::io::{AsyncWriteExt, BufReader};

/// Errors produced while extracting a zip archive into an environment.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum ZipError {
    #[snafu(display("failed to read zip archive: {source}"))]
    Read { source: async_zip::error::ZipError },
    #[snafu(display("zip entry filename is not valid utf-8"))]
    Filename { source: async_zip::error::ZipError },
    #[snafu(display("zip entry has unsafe path: {path}"))]
    UnsafePath { path: String },
    #[snafu(display("io error while extracting zip entry: {source}"))]
    Io { source: std::io::Error },
}

/// Stream a zip archive from `reader` and extract every entry below `root`.
///
/// The `Reader` must already be decompressed at the *outer* level (zip stores
/// per-entry compression which `async_zip` handles internally). Symlinks and
/// directory traversal are blocked.
pub async fn extract_zip_stream(root: &Path, reader: Reader) -> Result<(), ZipError> {
    use async_zip::base::read::stream::ZipFileReader;
    let buffered = BufReader::new(reader);
    let mut zip = ZipFileReader::with_tokio(buffered);
    loop {
        let next = zip.next_with_entry().await.context(ReadSnafu)?;
        let Some(mut entry_reader) = next else {
            break;
        };
        let raw_name = entry_reader
            .reader()
            .entry()
            .filename()
            .as_str()
            .context(FilenameSnafu)?
            .to_string();
        let dir = entry_reader.reader().entry().dir().context(ReadSnafu)?;
        let safe_path = sanitize_zip_path(&raw_name).ok_or_else(|| ZipError::UnsafePath {
            path: raw_name.clone(),
        })?;
        let dest = root.join(&safe_path);
        if dir {
            tokio::fs::create_dir_all(&dest).await.context(IoSnafu)?;
        } else {
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await.context(IoSnafu)?;
            }
            let mut file = tokio::fs::File::create(&dest).await.context(IoSnafu)?;
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = entry_reader
                    .reader_mut()
                    .read(&mut buf)
                    .await
                    .context(IoSnafu)?;
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n]).await.context(IoSnafu)?;
            }
            file.flush().await.context(IoSnafu)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Some(mode) = entry_reader.reader().entry().unix_permissions() {
                    let perms = std::fs::Permissions::from_mode(u32::from(mode));
                    let _ = tokio::fs::set_permissions(&dest, perms).await;
                }
            }
        }
        zip = entry_reader.done().await.context(ReadSnafu)?;
    }
    Ok(())
}

/// Reject zip entries whose path escapes the destination root (absolute paths,
/// `..` components, drive prefixes). Returns `None` if the path is unsafe.
fn sanitize_zip_path(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            // Skip leading "./" components.
            Component::CurDir => {}
            // Reject anything that could escape the root.
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if out.as_os_str().is_empty() {
        return None;
    }
    Some(out)
}

impl From<ZipError> for edo::environment::EnvironmentError {
    fn from(value: ZipError) -> Self {
        Self::Implementation {
            source: Box::new(value),
        }
    }
}
