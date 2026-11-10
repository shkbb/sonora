//! The picture of a track that is a music video, for a provider that has one. Its sound plays
//! through the engine like any track's, so this is only ever the video-only stream.

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;

/// A video-only H.264 stream in fragmented MP4, arriving in file order.
pub struct VideoStream {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// The length of the whole body in bytes, when the provider said.
    pub total: Option<u64>,
    pub body: Box<dyn VideoBody>,
}

/// Where the bytes of a [`VideoStream`] come from.
#[async_trait]
pub trait VideoBody: Send {
    /// The next part of the body, or `None` once it has all arrived.
    async fn chunk(&mut self) -> Result<Option<Bytes>>;

    /// Moves to the byte `offset` of the file, so the next chunk starts there.
    fn seek(&mut self, offset: u64);
}
