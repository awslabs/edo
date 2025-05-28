use futures::future::BoxFuture;
use futures::Future;
use std::io::{self, Read};
use std::pin::Pin;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use tokio::io::{AsyncRead, ReadBuf};

// Define the static VTABLE for our no-op waker
static VTABLE: RawWakerVTable = RawWakerVTable::new(
    |data| RawWaker::new(data, &VTABLE), // clone
    |_| {},                              // wake
    |_| {},                              // wake_by_ref
    |_| {},                              // drop
);

/// A wrapper that allows synchronous reading from an AsyncRead source
pub struct SyncReader<R> {
    reader: Pin<Box<R>>,
}

impl<R: AsyncRead> SyncReader<R> {
    /// Creates a new SyncReader with the given AsyncRead source
    pub fn new(reader: R) -> Self {
        Self {
            reader: Box::pin(reader),
        }
    }

    // Create a no-op waker that does nothing when woken
    fn create_waker() -> Waker {
        // Safety: This implements a no-op waker that does nothing when woken
        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
    }

    /// Synchronously reads from the async source into the provided buffer
    pub fn read_sync(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let waker = Self::create_waker();
        let mut cx = Context::from_waker(&waker);
        let mut readbuf = ReadBuf::new(buf);
        // Poll until we get data or EOF
        loop {
            match self.reader.as_mut().poll_read(&mut cx, &mut readbuf) {
                Poll::Ready(Ok(_)) => {
                    return Ok(readbuf.filled().len());
                }
                Poll::Ready(Err(e)) => return Err(e),
                Poll::Pending => continue, // Spin until data is available
            }
        }
    }
}

pub fn sync<R>(future: &mut BoxFuture<R>) -> R {
    // Safety: This implements a no-op waker that does nothing when woken
    let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
    let mut cx = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(result) => {
                return result;
            }
            Poll::Pending => continue,
        }
    }
}

pub fn sync_fn<R>(block_: impl AsyncFn() -> R) -> R {
    // Safety: This implements a no-op waker that does nothing when woken
    let mut pl = Box::pin(block_());
    let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
    let mut cx = Context::from_waker(&waker);
    loop {
        match pl.as_mut().poll(&mut cx) {
            Poll::Ready(result) => {
                return result;
            }
            Poll::Pending => continue,
        }
    }
}

impl<R: AsyncRead> Read for SyncReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_sync(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_sync_reader() {
        // Create a cursor with some test data
        let test_data = b"Hello, World!".to_vec();
        let cursor = Cursor::new(test_data.clone());

        // Create our sync reader
        let mut sync_reader = SyncReader::new(cursor);

        // Read using the sync interface
        let mut output = Vec::new();
        sync_reader.read_to_end(&mut output).unwrap();

        // Verify the output matches our input
        assert_eq!(output, test_data);
    }

    #[test]
    fn test_partial_reads() {
        let test_data = b"Hello, World!".to_vec();
        let cursor = Cursor::new(test_data.clone());
        let mut sync_reader = SyncReader::new(cursor);

        // Read in small chunks
        let mut buf = [0u8; 5];
        let n = sync_reader.read(&mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf, b"Hello");

        let n = sync_reader.read(&mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf, b", Wor");

        let n = sync_reader.read(&mut buf).unwrap();
        assert_eq!(n, 3);
        assert_eq!(&buf[..n], b"ld!");
    }
}
