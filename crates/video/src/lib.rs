//! Video decoding for music videos: frames out of an H.264 stream, timed so the player can
//! show each one when the sound reaches it. The sound itself is never decoded here.
//!
//! On Windows the work is Media Foundation's, which decodes H.264 in hardware on any GPU and
//! ships with the system, so no codec library comes with the app.

use std::time::Duration;

pub mod fmp4;
#[cfg(windows)]
mod mf;

#[cfg(windows)]
pub use mf::Decoder;

/// One decoded picture, in BGRA rows from the top, and when in the video it is shown.
pub struct Frame {
    pub at: Duration,
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}
