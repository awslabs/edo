use crate::storage::Compression;
use async_compression::tokio::write::{
    BzDecoder, BzEncoder, GzipDecoder, GzipEncoder, Lz4Decoder, Lz4Encoder, LzmaDecoder,
    LzmaEncoder, XzDecoder, XzEncoder, ZstdDecoder, ZstdEncoder,
};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use tokio::io::AsyncWrite;

/// An async writer wrapper that computes a SHA256 hash of all bytes written.
///
/// Implements [`AsyncWrite`]. After writing is complete call [`Writer::finish`]
/// to obtain the hex-encoded content digest.
#[derive(Clone)]
pub struct Writer {
    inner: Arc<Mutex<Inner>>,
}

impl Writer {
    /// Wrap an async writer with a target name and start a fresh SHA256 hash.
    pub fn new(target: String, writer: impl AsyncWrite + Send + Sync + 'static) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                writer: Box::pin(writer),
                hash: Sha256::new(),
                digest: None,
                size: 0,
                target,
            })),
        }
    }

    /// Wrap an async writer with compression enabled
    pub fn with_compression(
        target: String,
        writer: impl AsyncWrite + Send + Sync + 'static,
        compression: &Compression,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                writer: match compression {
                    Compression::Bzip2 => Box::pin(BzEncoder::new(writer)),
                    Compression::Gzip => Box::pin(GzipEncoder::new(writer)),
                    Compression::Lz4 => Box::pin(Lz4Encoder::new(writer)),
                    Compression::Lzma => Box::pin(LzmaEncoder::new(writer)),
                    Compression::Xz => Box::pin(XzEncoder::new(writer)),
                    Compression::Zstd => Box::pin(ZstdEncoder::new(writer)),
                    Compression::None => Box::pin(writer),
                },
                hash: Sha256::new(),
                digest: None,
                size: 0,
                target,
            })),
        }
    }

    /// Wrap an async writer with compression enabled
    pub fn with_decompression(
        target: String,
        writer: impl AsyncWrite + Send + Sync + 'static,
        compression: &Compression,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                writer: match compression {
                    Compression::Bzip2 => Box::pin(BzDecoder::new(writer)),
                    Compression::Gzip => Box::pin(GzipDecoder::new(writer)),
                    Compression::Lz4 => Box::pin(Lz4Decoder::new(writer)),
                    Compression::Lzma => Box::pin(LzmaDecoder::new(writer)),
                    Compression::Xz => Box::pin(XzDecoder::new(writer)),
                    Compression::Zstd => Box::pin(ZstdDecoder::new(writer)),
                    Compression::None => Box::pin(writer),
                },
                hash: Sha256::new(),
                digest: None,
                size: 0,
                target,
            })),
        }
    }

    /// Return the total number of bytes written so far.
    pub fn size(&self) -> usize {
        self.inner.lock().size
    }

    /// Override the computed digest with a predetermined value.
    pub fn set_digest(&self, digest: &str) {
        self.inner.lock().digest = Some(digest.to_string());
    }

    /// Return the target name supplied at construction time.
    pub fn target(&self) -> String {
        self.inner.lock().target.clone()
    }

    /// Finalize the hash and return the hex-encoded SHA256 digest.
    ///
    /// If a digest was set manually via [`Writer::set_digest`], that value is
    /// returned instead.
    pub async fn finish(&self) -> String {
        let lock = self.inner.lock();
        let hash = lock.hash.clone().finalize();
        let digest = base16::encode_lower(hash.as_slice());

        lock.digest.clone().unwrap_or(digest)
    }
}

struct Inner {
    writer: Pin<Box<dyn AsyncWrite + Send + Sync>>,
    hash: Sha256,
    digest: Option<String>,
    size: usize,
    target: String,
}

impl AsyncWrite for Writer {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        let this = self.get_mut();
        let mut lock = this.inner.lock();
        match lock.writer.as_mut().poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                lock.hash.update(&buf[..n]);
                lock.size += n;
                Poll::Ready(Ok(n))
            }
            value => value,
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        self.get_mut().inner.lock().writer.as_mut().poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        self.get_mut().inner.lock().writer.as_mut().poll_flush(cx)
    }
}
