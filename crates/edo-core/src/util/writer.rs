use parking_lot::Mutex;
use std::pin::Pin;
use std::rc::Rc;
use std::task::Poll;
use tokio::io::AsyncWrite;

#[derive(Clone)]
pub struct Writer {
    inner: Rc<Mutex<Inner>>,
}

impl Writer {
    pub fn new(target: String, writer: impl AsyncWrite + Send + Sync + 'static) -> Self {
        Self {
            inner: Rc::new(Mutex::new(Inner {
                writer: Box::pin(writer),
                hash: blake3::Hasher::new(),
                digest: None,
                size: 0,
                target,
            })),
        }
    }

    pub fn size(&self) -> usize {
        self.inner.lock().size
    }

    pub fn set_digest(&self, digest: &str) {
        self.inner.lock().digest = Some(digest.to_string());
    }

    pub fn target(&self) -> String {
        self.inner.lock().target.clone()
    }

    pub async fn finish(&self) -> String {
        let lock = self.inner.lock();
        let hash = lock.hash.finalize();
        let digest = base16::encode_lower(hash.as_bytes());

        lock.digest.clone().unwrap_or(digest)
    }
}

unsafe impl Send for Writer {}
unsafe impl Sync for Writer {}

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
        self.get_mut().inner.lock().writer.as_mut().poll_flush(cx)
    }
}
