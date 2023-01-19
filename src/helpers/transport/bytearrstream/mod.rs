mod aligned;

pub use aligned::ByteArrStream as AlignedByteArrStream;

use crate::error::BoxError;
use axum::extract::BodyStream;
use futures::Stream;
use futures_util::{stream, TryStreamExt};
use hyper::body::Bytes;
use std::pin::Pin;
use std::task::{Context, Poll};

/// represents the item of an underlying stream
type Item = Result<Bytes, BoxError>;
/// represents the underlying stream
type PinnedStream = Pin<Box<dyn Stream<Item = Item> + Send>>;

pub struct ByteArrStream {
    stream: PinnedStream,
}

impl ByteArrStream {
    #[must_use]
    pub fn new(stream: PinnedStream) -> Self {
        Self { stream }
    }

    #[must_use]
    #[allow(clippy::missing_panics_doc)] // `size_in_bytes` known to be small
    pub fn align(self, size_in_bytes: usize) -> AlignedByteArrStream {
        AlignedByteArrStream::new(self.stream, u32::try_from(size_in_bytes).unwrap())
    }
}

impl From<BodyStream> for ByteArrStream {
    fn from(stream: BodyStream) -> Self {
        ByteArrStream::new(Box::pin(stream.map_err(<BoxError>::from)) as PinnedStream)
    }
}

impl From<Vec<u8>> for ByteArrStream {
    fn from(vec: Vec<u8>) -> Self {
        ByteArrStream::new(Box::pin(stream::iter(std::iter::once(Ok(Bytes::from(
            vec,
        ))))))
    }
}

#[cfg(test)]
impl From<&[u8]> for ByteArrStream {
    fn from(slice: &[u8]) -> Self {
        Self::from(slice.to_vec())
    }
}

impl Stream for ByteArrStream {
    type Item = Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.as_mut().poll_next(cx)
    }
}
