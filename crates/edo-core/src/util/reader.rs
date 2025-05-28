use parking_lot::Mutex;
use std::pin::Pin;
use std::rc::Rc;
use std::task::Poll;
use tokio::io::{AsyncRead, AsyncReadExt};

#[derive(Clone)]
pub struct Reader {
    inner: Rc<Mutex<Inner>>,
}

impl Reader {
    pub fn new(reader: impl AsyncRead + 'static) -> Self {
        Self {
            inner: Rc::new(Mutex::new(Inner {
                reader: Box::pin(reader),
                hash: blake3::Hasher::new(),
                pos: 0,
            })),
        }
    }

    pub fn finish(&self) -> String {
        let lock = self.inner.lock();
        let hash = lock.hash.finalize();
        let digest = base16::encode_lower(hash.as_bytes());
        digest
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
