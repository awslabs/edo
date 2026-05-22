use async_compression::tokio::bufread::{
    BzDecoder, BzEncoder, GzipDecoder, GzipEncoder, LzmaDecoder, LzmaEncoder, XzDecoder, XzEncoder,
    ZstdDecoder, ZstdEncoder,
};
use parking_lot::Mutex;
use std::pin::Pin;
use std::rc::Rc;
use std::task::Poll;
use tokio::io::{AsyncRead, AsyncReadExt, BufReader};

use crate::storage::Compression;

/// An async reader wrapper that computes a BLAKE3 hash of all bytes read.
///
/// Implements both [`AsyncRead`] and [`std::io::Read`] (blocking via the
/// current tokio runtime). Use [`Reader::finish`] after all data has been
/// consumed to obtain the hex-encoded digest.
#[derive(Clone)]
pub struct Reader {
    inner: Rc<Mutex<Inner>>,
}

impl Reader {
    /// Wrap an async reader, starting a fresh BLAKE3 hash.
    pub fn new(reader: impl AsyncRead + 'static) -> Self {
        Self {
            inner: Rc::new(Mutex::new(Inner {
                reader: Box::pin(reader),
                hash: blake3::Hasher::new(),
                pos: 0,
            })),
        }
    }

    /// Wrap an async reader with compression enabled
    pub fn with_compression(reader: impl AsyncRead + 'static, compression: &Compression) -> Self {
        let buffered = BufReader::new(reader);
        Self {
            inner: Rc::new(Mutex::new(Inner {
                reader: match compression {
                    Compression::Bzip2 => Box::pin(BzEncoder::new(buffered)),
                    Compression::Gzip => Box::pin(GzipEncoder::new(buffered)),
                    Compression::Lz => Box::pin(LzmaEncoder::new(buffered)),
                    Compression::Xz => Box::pin(XzEncoder::new(buffered)),
                    Compression::Zstd => Box::pin(ZstdEncoder::new(buffered)),
                    Compression::None => Box::pin(buffered),
                },
                hash: blake3::Hasher::new(),
                pos: 0,
            })),
        }
    }

    /// Wrap an async reader with decompression enabled
    pub fn with_decompression(reader: impl AsyncRead + 'static, compression: &Compression) -> Self {
        let buffered = BufReader::new(reader);
        Self {
            inner: Rc::new(Mutex::new(Inner {
                reader: match compression {
                    Compression::Bzip2 => Box::pin(BzDecoder::new(buffered)),
                    Compression::Gzip => Box::pin(GzipDecoder::new(buffered)),
                    Compression::Lz => Box::pin(LzmaDecoder::new(buffered)),
                    Compression::Xz => Box::pin(XzDecoder::new(buffered)),
                    Compression::Zstd => Box::pin(ZstdDecoder::new(buffered)),
                    Compression::None => Box::pin(buffered),
                },
                hash: blake3::Hasher::new(),
                pos: 0,
            })),
        }
    }

    /// Finalize the hash and return the hex-encoded BLAKE3 digest of all bytes read so far.
    pub fn finish(&self) -> String {
        let lock = self.inner.lock();
        let hash = lock.hash.finalize();

        base16::encode_lower(hash.as_bytes())
    }
}

unsafe impl Send for Reader {}
unsafe impl Sync for Reader {}

struct Inner {
    reader: Pin<Box<dyn AsyncRead>>,
    hash: blake3::Hasher,
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
