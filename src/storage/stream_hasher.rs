use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use bytes::Bytes;
use futures::Stream;
use sha2::{Digest, Sha256};

/// Shared hash state accessible from both the streaming body and the caller.
pub struct HashHandle {
    inner: Mutex<HashInner>,
}

struct HashInner {
    hasher: Sha256,
    bytes_hashed: u64,
}

impl HashHandle {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashInner {
                hasher: Sha256::new(),
                bytes_hashed: 0,
            }),
        }
    }

    /// Consume the internal hasher and return the hex-encoded SHA-256 digest.
    pub fn finalize(&self) -> String {
        let mut guard = self.inner.lock().unwrap();
        let old = std::mem::replace(&mut guard.hasher, Sha256::new());
        hex::encode(old.finalize())
    }

    /// Number of bytes hashed so far.
    pub fn bytes_hashed(&self) -> u64 {
        self.inner.lock().unwrap().bytes_hashed
    }

    fn update(&self, data: &[u8]) {
        let mut guard = self.inner.lock().unwrap();
        guard.hasher.update(data);
        guard.bytes_hashed += data.len() as u64;
    }
}

/// Wraps a `Stream<Item = Result<Bytes, E>>`, computing a running SHA-256
/// as bytes pass through.  The caller keeps an `Arc<HashHandle>` that yields
/// the final digest after the stream is fully consumed.
pub struct HashingStream<S> {
    inner: S,
    hash_handle: Arc<HashHandle>,
}

impl<S> HashingStream<S> {
    /// Build a hashing stream and return a handle that can be queried for the
    /// final hash once the stream is exhausted.
    pub fn new(inner: S) -> (Self, Arc<HashHandle>) {
        let handle = Arc::new(HashHandle::new());
        let stream = HashingStream {
            inner,
            hash_handle: handle.clone(),
        };
        (stream, handle)
    }
}

impl<S, E> Stream for HashingStream<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Item = Result<Bytes, Box<dyn std::error::Error + Send + Sync>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                self.hash_handle.update(&bytes);
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e.into()))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    #[tokio::test]
    async fn test_hashing_stream_etag() {
        let data: Vec<Bytes> = vec![
            Bytes::from("hello "),
            Bytes::from("world"),
        ];
        let input_stream = stream::iter(data.into_iter().map(Ok::<_, anyhow::Error>));
        let (hashing, handle) = HashingStream::new(input_stream);

        use futures::StreamExt;
        let result: Vec<Bytes> = hashing
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(result.concat(), b"hello world");

        let etag = handle.finalize();
        let expected = hex::encode(Sha256::digest(b"hello world"));
        assert_eq!(etag, expected);
    }

    #[tokio::test]
    async fn test_hashing_stream_empty() {
        let data: Vec<Bytes> = vec![];
        let input_stream = stream::iter(data.into_iter().map(Ok::<_, anyhow::Error>));
        let (hashing, handle) = HashingStream::new(input_stream);

        use futures::StreamExt;
        let result: Vec<Bytes> = hashing
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert!(result.is_empty());
        let expected = hex::encode(Sha256::digest(b""));
        assert_eq!(handle.finalize(), expected);
    }

    #[tokio::test]
    async fn test_hashing_stream_large() {
        let chunk = Bytes::from(vec![0xABu8; 65536]);
        let chunks: Vec<_> = (0..100).map(|_| Ok::<_, anyhow::Error>(chunk.clone())).collect();
        let input_stream = stream::iter(chunks);
        let (hashing, handle) = HashingStream::new(input_stream);

        use futures::StreamExt;
        let result: Vec<Bytes> = hashing
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(result.len(), 100);

        let mut expected_hasher = Sha256::new();
        for b in &result {
            expected_hasher.update(b);
        }
        let expected = hex::encode(expected_hasher.finalize());
        assert_eq!(handle.finalize(), expected);
        assert_eq!(handle.bytes_hashed(), 6553600);
    }
}
