use crate::storage::Compression;
use async_compression::tokio::write::{
    BzDecoder, BzEncoder, GzipDecoder, GzipEncoder, Lz4Decoder, Lz4Encoder, LzmaDecoder,
    LzmaEncoder, XzDecoder, XzEncoder, ZstdDecoder, ZstdEncoder,
};
use parking_lot::Mutex;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use tokio::io::AsyncWrite;

/// An async writer wrapper that computes a BLAKE3 hash of all bytes written.
///
/// Implements [`AsyncWrite`]. After writing is complete call [`Writer::finish`]
/// to obtain the hex-encoded content digest.
#[derive(Clone)]
pub struct Writer {
    inner: Arc<Mutex<Inner>>,
}

impl Writer {
    /// Wrap an async writer with a target name and start a fresh BLAKE3 hash.
    pub fn new(target: String, writer: impl AsyncWrite + Send + Sync + 'static) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                writer: Box::pin(writer),
                hash: blake3::Hasher::new(),
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
                hash: blake3::Hasher::new(),
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
                hash: blake3::Hasher::new(),
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

    /// Finalize the hash and return the hex-encoded BLAKE3 digest.
    ///
    /// If a digest was set manually via [`Writer::set_digest`], that value is
    /// returned instead.
    pub async fn finish(&self) -> String {
        let lock = self.inner.lock();
        let hash = lock.hash.finalize();
        let digest = base16::encode_lower(hash.as_bytes());

        lock.digest.clone().unwrap_or(digest)
    }
}

struct Inner {
    writer: Pin<Box<dyn AsyncWrite + Send + Sync>>,
    hash: blake3::Hasher,
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
        // Forward to the inner writer's `poll_shutdown` (not `poll_flush`).
        // Compression encoders (gzip, zstd, xz, bzip2, ...) emit their
        // trailer/footer during shutdown, so flushing alone truncates the
        // stream and produces `UnexpectedEof` on decode.
        self.get_mut()
            .inner
            .lock()
            .writer
            .as_mut()
            .poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::Reader;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Helper: compress with a shared buffer we can inspect after the writer
    // is dropped. The Writer owns its underlying sink, so an Arc<Mutex<Vec>>
    // is the simplest way to observe the encoded output.
    async fn compress_to_vec(compression: &Compression, payload: &[u8]) -> Vec<u8> {
        let sink = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        struct SharedSink(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);
        impl AsyncWrite for SharedSink {
            fn poll_write(
                self: Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                buf: &[u8],
            ) -> Poll<std::io::Result<usize>> {
                self.0.lock().extend_from_slice(buf);
                Poll::Ready(Ok(buf.len()))
            }
            fn poll_flush(
                self: Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
            fn poll_shutdown(
                self: Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
        }
        let mut w = Writer::with_compression("test".into(), SharedSink(sink.clone()), compression);
        w.write_all(payload).await.expect("write");
        w.shutdown().await.expect("shutdown");
        drop(w);
        sink.lock().clone()
    }

    async fn round_trip_ok(compression: Compression, payload: &[u8]) {
        let compressed = compress_to_vec(&compression, payload).await;
        assert!(
            !compressed.is_empty(),
            "compressed output must be non-empty for {compression:?}"
        );

        let cursor = std::io::Cursor::new(compressed);
        let mut r = Reader::with_decompression(cursor, &compression);
        let mut decoded = Vec::new();
        r.read_to_end(&mut decoded).await.expect("decode");
        assert_eq!(decoded, payload, "round trip mismatch for {compression:?}");
    }

    // Regression: prior to the shutdown fix, `poll_shutdown` delegated to
    // `poll_flush`, so encoder trailers were never emitted and decoding
    // returned `UnexpectedEof`.
    #[tokio::test]
    async fn gzip_round_trip() {
        round_trip_ok(Compression::Gzip, b"hello, gzip world!").await;
    }

    #[tokio::test]
    async fn zstd_round_trip() {
        round_trip_ok(Compression::Zstd, b"hello, zstd world!").await;
    }

    #[tokio::test]
    async fn xz_round_trip() {
        round_trip_ok(Compression::Xz, b"hello, xz world!").await;
    }

    #[tokio::test]
    async fn bzip2_round_trip() {
        round_trip_ok(Compression::Bzip2, b"hello, bzip2 world!").await;
    }

    #[tokio::test]
    async fn lz4_round_trip() {
        round_trip_ok(Compression::Lz4, b"hello, lz4 world!").await;
    }

    // Regression: `Compression::Lz` was collapsed for both `.lz4` and
    // `.lzma`, but the encoder only wrapped lz4, so `.lzma` payloads were
    // corrupted. After the split each variant round trips with its own
    // codec.
    #[tokio::test]
    async fn lzma_round_trip() {
        round_trip_ok(Compression::Lzma, b"hello, lzma world!").await;
    }
}
