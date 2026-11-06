//! Drawing a music video's picture: the frame due now from `state::Video`, the next one asked
//! for while it plays, and every frame that is replaced freed from the GPU, since each frame is
//! an image of its own.

use std::sync::Arc;

use gpui::{App, Entity, RenderImage, Window};
use state::{Playback, PlaybackState, Video};

/// The frame a view drew last, which it frees once a later one replaces it.
#[derive(Default)]
pub(crate) struct Picture {
    shown: Option<Arc<RenderImage>>,
}

impl Picture {
    /// The frame to draw now, `None` while the track has no picture or its first frame is still
    /// decoding. Asks for another draw while one is due.
    pub(crate) fn frame(
        &mut self,
        video: &Entity<Video>,
        playback: &Entity<Playback>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Arc<RenderImage>> {
        let (frame, loaded) = video.update(cx, |video, cx| (video.frame(cx), video.has_picture()));
        let playing = *playback.read(cx).state() == PlaybackState::Playing;
        if loaded && (playing || frame.is_none()) {
            window.request_animation_frame();
        }
        let same = match (&frame, &self.shown) {
            (Some(new), Some(old)) => Arc::ptr_eq(new, old),
            (None, None) => true,
            _ => false,
        };
        if !same {
            if let Some(old) = self.shown.take() {
                window.drop_image(old).ok();
            }
            self.shown = frame.clone();
        }
        frame
    }
}

/// The picture's width over its height, or 16:9 before its first frame says.
pub(crate) fn shape(video: &Entity<Video>, cx: &App) -> f32 {
    let (across, down) = video.read(cx).size().unwrap_or((16, 9));
    across as f32 / down.max(1) as f32
}
