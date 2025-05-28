use super::{error, Result, CHUNK_SIZE};
use aws_sdk_s3::primitives::AggregatedBytes;
use aws_sdk_s3::Client;
use futures::future::BoxFuture;
use snafu::ResultExt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use tokio::io::AsyncRead;

pub struct ObjectReader {
    client: Arc<Client>,
    bucket: String,
    key: String,
    buffer: Vec<u8>,
    position: u64,
    size: u64,
    active: Option<BoxFuture<'static, Result<AggregatedBytes>>>,
}

impl ObjectReader {
    pub async fn new(client: Arc<Client>, bucket: &str, key: &str) -> Result<Self> {
        let head_object = client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .context(error::CheckSnafu)?;
        let size = head_object.content_length().unwrap_or(0) as u64;
        Ok(Self {
            client: client.clone(),
            bucket: bucket.into(),
            buffer: Vec::new(),
            key: key.into(),
            position: 0,
            size,
            active: None,
        })
    }

    fn start_request(&mut self, size: usize) {
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let key = self.key.clone();
        let position = self.position;

        self.active = Some(Box::pin(async move {
            let output = client
                .get_object()
                .bucket(bucket)
                .key(key)
                .range(format!(
                    "bytes={}-{}",
                    position,
                    position as usize + size - 1
                ))
                .send()
                .await
                .context(error::GetSnafu)?;
            Ok(output.body.collect().await.unwrap())
        }));
    }
}

impl AsyncRead for ObjectReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();

        if let Some(request) = this.active.as_mut() {
            match request.as_mut().poll(cx) {
                Poll::Ready(Ok(stream)) => {
                    this.active = None;
                    let mut data = stream.to_vec();
                    this.position += data.len() as u64;
                    this.buffer.append(&mut data);
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                Poll::Ready(Err(e)) => Poll::Ready(Err(std::io::Error::other(e))),
                Poll::Pending => {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
        } else {
            // Check if we have a buffer and the request lives within the buffer
            if this.buffer.len() > buf.remaining() {
                let buffer = this.buffer.clone();
                let (segment, remainder) = buffer.split_at(buf.remaining());
                this.buffer = remainder.to_vec();
                buf.put_slice(segment);
                return Poll::Ready(Ok(()));
            }
            // Check if we are done reading if
            if this.position >= this.size {
                if !this.buffer.is_empty() {
                    // If the buffer is still full of data it needs to be written
                    buf.put_slice(this.buffer.as_slice());
                    this.buffer.clear();
                }
                return Poll::Ready(Ok(()));
            }

            // Otherwise we need to get another buffer load
            let chunk_size = std::cmp::min(CHUNK_SIZE as u64, this.size - this.position);
            this.start_request(chunk_size as usize);
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}
