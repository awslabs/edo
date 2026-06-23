use async_compression::tokio::bufread::{
    BzDecoder, BzEncoder, GzipDecoder, GzipEncoder, Lz4Decoder, Lz4Encoder, LzmaDecoder,
    LzmaEncoder, XzDecoder, XzEncoder, ZstdDecoder, ZstdEncoder,
};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use tokio::io::{AsyncRead, AsyncReadExt, BufReader};

use crate::storage::Compression;

/// An async reader wrapper that computes a SHA256 hash of all bytes read.
///
/// Implements both [`AsyncRead`] and [`std::io::Read`] (blocking via the
/// current tokio runtime). Use [`Reader::finish`] after all data has been
/// consumed to obtain the hex-encoded digest.
#[derive(Clone)]
pub struct Reader {
    inner: Arc<Mutex<Inner>>,
}

impl Reader {
    /// Wrap an async reader, starting a fresh SHA256 hash.
    pub fn new(reader: impl AsyncRead + Send + 'static) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                reader: Box::pin(reader),
                hash: Sha256::new(),
                pos: 0,
            })),
        }
    }

    /// Wrap an async reader with compression enabled
    pub fn with_compression(
        reader: impl AsyncRead + Send + 'static,
        compression: &Compression,
    ) -> Self {
        let buffered = BufReader::new(reader);
        Self {
            inner: Arc::new(Mutex::new(Inner {
                reader: match compression {
                    Compression::Bzip2 => Box::pin(BzEncoder::new(buffered)),
                    Compression::Gzip => Box::pin(GzipEncoder::new(buffered)),
                    Compression::Lz4 => Box::pin(Lz4Encoder::new(buffered)),
                    Compression::Lzma => Box::pin(LzmaEncoder::new(buffered)),
                    Compression::Xz => Box::pin(XzEncoder::new(buffered)),
                    Compression::Zstd => Box::pin(ZstdEncoder::new(buffered)),
                    Compression::None => Box::pin(buffered),
                },
                hash: Sha256::new(),
                pos: 0,
            })),
        }
    }

    /// Wrap an async reader with decompression enabled
    pub fn with_decompression(
        reader: impl AsyncRead + Send + 'static,
        compression: &Compression,
    ) -> Self {
        let buffered = BufReader::new(reader);
        Self {
            inner: Arc::new(Mutex::new(Inner {
                reader: match compression {
                    Compression::Bzip2 => Box::pin(BzDecoder::new(buffered)),
                    Compression::Gzip => Box::pin(GzipDecoder::new(buffered)),
                    Compression::Lz4 => Box::pin(Lz4Decoder::new(buffered)),
                    Compression::Lzma => Box::pin(LzmaDecoder::new(buffered)),
                    Compression::Xz => Box::pin(XzDecoder::new(buffered)),
                    Compression::Zstd => Box::pin(ZstdDecoder::new(buffered)),
                    Compression::None => Box::pin(buffered),
                },
                hash: Sha256::new(),
                pos: 0,
            })),
        }
    }

    /// Finalize the hash and return the hex-encoded SHA256 digest of all bytes read so far.
    pub fn finish(&self) -> String {
        let lock = self.inner.lock();
        let hash = lock.hash.clone().finalize();

        base16::encode_lower(hash.as_slice())
    }
}

struct Inner {
    reader: Pin<Box<dyn AsyncRead + Send>>,
    hash: Sha256,
    pos: usize,
}

impl std::io::Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut lock = self.inner.lock();
        let handle = tokio::runtime::Handle::current();
        handle.block_on(lock.reader.read(buf))
    }
}

impl AsyncRead for Reader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let mut lock = this.inner.lock();
        match lock.reader.as_mut().poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                if !buf.filled().is_empty() {
                    let segment = buf.filled();
                    lock.pos += segment.len();
                    if !segment.is_empty() {
                        lock.hash.update(segment);
                    }
                }
                Poll::Ready(Ok(()))
            }
            value => value,
        }
    }
}
