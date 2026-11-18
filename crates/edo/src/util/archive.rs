//! Tar archiving and extraction over async streams.
//!
//! The `tar` crate is synchronous, so every operation runs as a whole on a
//! blocking-pool thread and reaches the async stream through
//! [`SyncIoBridge`]. The bridge blocks on the runtime handle, which panics on a
//! runtime worker thread: never drive a `tar::Archive`/`tar::Builder` over a
//! bridge outside `spawn_blocking`.
//!
//! Blocking tasks cannot be aborted. A cancelled caller stops awaiting, but the
//! archive operation runs until it completes or its stream errors.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::io::SyncIoBridge;

/// Extract a tar stream into `dest`.
///
/// Entries that would land outside `dest` (via `..`, absolute paths, or
/// previously extracted symlinks) are refused by `tar::Archive::unpack`.
pub async fn unpack<R>(reader: R, dest: &Path, preserve_permissions: bool) -> io::Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
{
    let bridge = SyncIoBridge::new(reader);
    let dest = dest.to_path_buf();
    blocking(move || {
        let mut archive = tar::Archive::new(bridge);
        archive.set_preserve_permissions(preserve_permissions);
        archive.set_preserve_mtime(true);
        archive.set_overwrite(true);
        archive.unpack(&dest)
    })
    .await
}

/// Archive the contents of `src` (rooted at `.`) into `writer`, then write the
/// end-of-archive marker and flush. Returns the writer.
///
/// Symlinks are stored as links rather than followed, so a link pointing at
/// the host filesystem never pulls host files into an artifact.
pub async fn pack_dir<W>(writer: W, src: &Path) -> io::Result<W>
where
    W: AsyncWrite + Send + Unpin + 'static,
{
    let bridge = SyncIoBridge::new(writer);
    let src: PathBuf = src.to_path_buf();
    blocking(move || {
        let mut builder = tar::Builder::new(bridge);
        builder.mode(tar::HeaderMode::Complete);
        builder.follow_symlinks(false);
        builder.append_dir_all(".", &src)?;
        let mut bridge = builder.into_inner()?;
        bridge.flush()?;
        Ok(bridge.into_inner())
    })
    .await
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::tempdir;

    async fn pack(src: &Path) -> Vec<u8> {
        let out = tempdir().unwrap();
        let path = out.path().join("a.tar");
        let file = tokio::fs::File::create(&path).await.unwrap();
        pack_dir(file, src).await.unwrap();
        std::fs::read(path).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn round_trip_through_pipe_on_runtime() {
        let src = tempdir().unwrap();
        let long = "d".repeat(80);
        std::fs::create_dir_all(src.path().join(&long).join(&long)).unwrap();
        std::fs::write(src.path().join(&long).join(&long).join("f.txt"), b"deep").unwrap();
        std::fs::write(src.path().join("top"), b"hello").unwrap();
        std::os::unix::fs::symlink("top", src.path().join("link")).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", src.path().join("abs")).unwrap();

        // Pack and unpack concurrently over an in-memory pipe, the shape that
        // panicked when the bridge ran on a worker thread.
        let (w, r) = tokio::io::duplex(1024);
        let dst = tempdir().unwrap();
        let (packed, unpacked) = tokio::join!(pack_dir(w, src.path()), unpack(r, dst.path(), true));
        drop(packed.unwrap());
        unpacked.unwrap();

        let d = dst.path();
        assert_eq!(std::fs::read(d.join("top")).unwrap(), b"hello");
        assert_eq!(
            std::fs::read(d.join(&long).join(&long).join("f.txt")).unwrap(),
            b"deep"
        );
        assert_eq!(
            std::fs::read_link(d.join("link")).unwrap(),
            Path::new("top")
        );
        assert_eq!(
            std::fs::read_link(d.join("abs")).unwrap(),
            Path::new("/etc/hostname")
        );
    }

    #[tokio::test]
    async fn preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let src = tempdir().unwrap();
        let exe = src.path().join("run.sh");
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o750)).unwrap();
        let bytes = pack(src.path()).await;

        let dst = tempdir().unwrap();
        unpack(Cursor::new(bytes), dst.path(), true).await.unwrap();
        let mode = std::fs::metadata(dst.path().join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o750);
    }

    fn entry(
        builder: &mut tar::Builder<Vec<u8>>,
        name: &[u8],
        kind: tar::EntryType,
        link: Option<&str>,
        data: &[u8],
    ) {
        let mut header = tar::Header::new_old();
        // Write the raw name: `set_path` refuses `..`, which is the point here.
        header.as_old_mut().name[..name.len()].copy_from_slice(name);
        header.set_entry_type(kind);
        header.set_mode(0o644);
        header.set_size(data.len() as u64);
        if let Some(link) = link {
            header.set_link_name(link).unwrap();
        }
        header.set_cksum();
        builder.append(&header, data).unwrap();
    }

    #[tokio::test]
    async fn refuses_parent_dir_entries() {
        let root = tempdir().unwrap();
        let dst = root.path().join("dst");
        std::fs::create_dir(&dst).unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        entry(
            &mut builder,
            b"../evil",
            tar::EntryType::Regular,
            None,
            b"x",
        );
        let bytes = builder.into_inner().unwrap();

        // tar skips (or errors on) such entries; either way nothing escapes.
        let _ = unpack(Cursor::new(bytes), &dst, false).await;
        assert!(!root.path().join("evil").exists());
    }

    #[tokio::test]
    async fn refuses_writes_through_symlinks() {
        let outside = tempdir().unwrap();
        let dst = tempdir().unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        let target = outside.path().to_str().unwrap();
        entry(
            &mut builder,
            b"link",
            tar::EntryType::Symlink,
            Some(target),
            b"",
        );
        entry(
            &mut builder,
            b"link/pwned",
            tar::EntryType::Regular,
            None,
            b"x",
        );
        let bytes = builder.into_inner().unwrap();

        assert!(unpack(Cursor::new(bytes), dst.path(), false).await.is_err());
        assert!(!outside.path().join("pwned").exists());
    }
}
