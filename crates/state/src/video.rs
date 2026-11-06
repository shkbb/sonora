//! The picture of the music video playing, kept in step with its sound.
//!
//! [`Video`] follows the track playing. When the track is a music video and the picture is
//! wanted, a tokio task downloads its video stream and a worker thread decodes it a few frames
//! ahead. A view asks [`Video::frame`] for the picture at the sound's position on every frame
//! it draws; the sound's clock is the only clock, so the picture never runs ahead of it or
//! drags behind. A jump in the position tells the worker to seek.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use gpui::{App, Context, Entity, RenderImage, Task};
use video::fmp4::{self, Fragment, Track};

use crate::{Io, Playback, PlaybackState, Session, join};

/// How many decoded frames the worker keeps ready ahead of the sound.
const AHEAD: usize = 8;
/// How far the position may move between two draws before it counts as a seek rather than
/// playback moving on.
const JUMP: Duration = Duration::from_millis(1500);
/// How long the worker waits for bytes or room before it looks at its messages again.
const IDLE: Duration = Duration::from_millis(10);

/// Follows the playing track and keeps its picture decoded ahead of the sound.
pub struct Video {
    playback: Entity<Playback>,
    session: Entity<Session>,
    io: Io,
    /// The views showing the picture. Nothing is decoded while there are none, so a track
    /// plays without a picture nobody sees.
    viewers: u8,
    /// The track whose picture is loaded or being looked for.
    following: Option<String>,
    pipeline: Option<Pipeline>,
    opening: Option<Task<()>>,
}

/// A view that shows the picture. Each is a bit in [`Video::viewers`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Viewer {
    /// The Video tab of the right sidebar.
    Panel = 1,
    /// The fullscreen player.
    Fullscreen = 2,
}

/// One video being downloaded and decoded.
struct Pipeline {
    shelf: Arc<Mutex<Shelf>>,
    to_worker: Sender<Feed>,
    /// The position the last frame was asked for at, to tell a seek from playback.
    last: Option<Duration>,
    /// The frame on show, which stays up until a later one is due.
    shown: Option<(Duration, Arc<RenderImage>)>,
    /// The download, aborted when the pipeline drops.
    download: tokio::task::AbortHandle,
}

/// What the worker hands to the view: the frames decoded ahead, oldest first.
#[derive(Default)]
struct Shelf {
    frames: VecDeque<(Duration, Arc<RenderImage>)>,
    width: u32,
    height: u32,
}

/// What reaches the worker.
enum Feed {
    Bytes(Bytes),
    /// Every byte has arrived.
    End,
    /// Playback moved to this position.
    Seek(Duration),
}

impl Video {
    pub fn new(
        playback: Entity<Playback>,
        session: Entity<Session>,
        io: Io,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&playback, |this, _, cx| this.follow(cx))
            .detach();
        Self {
            playback,
            session,
            io,
            viewers: 0,
            following: None,
            pipeline: None,
            opening: None,
        }
    }

    /// Says whether `viewer` shows the picture. Nothing is downloaded or decoded while no
    /// view does.
    pub fn watch(&mut self, viewer: Viewer, watching: bool, cx: &mut Context<Self>) {
        let viewers = match watching {
            true => self.viewers | viewer as u8,
            false => self.viewers & !(viewer as u8),
        };
        if (viewers == 0) == (self.viewers == 0) {
            self.viewers = viewers;
            return;
        }
        self.viewers = viewers;
        self.following = None;
        self.follow(cx);
    }

    /// Whether the picture of the track playing is still being looked for.
    pub fn is_opening(&self) -> bool {
        self.opening.is_some()
    }

    /// Whether the track playing has a picture loaded to show.
    pub fn has_picture(&self) -> bool {
        self.pipeline.is_some()
    }

    /// The picture's size in pixels, once its first frame is decoded.
    pub fn size(&self) -> Option<(u32, u32)> {
        let shelf = self.pipeline.as_ref()?.shelf.lock().ok()?;
        (shelf.width > 0).then_some((shelf.width, shelf.height))
    }

    /// The frame to show now: the last one due at or before the sound's position. A view
    /// calls it on every frame it draws while the picture is up.
    pub fn frame(&mut self, cx: &App) -> Option<Arc<RenderImage>> {
        let position = self.playback.read(cx).live_position();
        let pipeline = self.pipeline.as_mut()?;
        let jumped = pipeline
            .last
            .is_some_and(|last| position + JUMP < last || position > last + JUMP);
        pipeline.last = Some(position);
        if jumped {
            pipeline.shown = None;
            if let Ok(mut shelf) = pipeline.shelf.lock() {
                shelf.frames.clear();
            }
            pipeline.to_worker.send(Feed::Seek(position)).ok();
        }
        if let Ok(mut shelf) = pipeline.shelf.lock() {
            // Frames far ahead were decoded for where playback was before a seek back.
            shelf.frames.retain(|(at, _)| *at <= position + JUMP);
            while let Some((at, _)) = shelf.frames.front() {
                if *at > position {
                    break;
                }
                pipeline.shown = shelf.frames.pop_front();
            }
        }
        pipeline.shown.as_ref().map(|(_, image)| image.clone())
    }

    /// Loads the picture of the track playing when one is wanted, and drops any other.
    fn follow(&mut self, cx: &mut Context<Self>) {
        let playback = self.playback.read(cx);
        let id = playback
            .track()
            .and_then(|track| track.id.clone())
            .filter(|_| self.viewers != 0)
            .filter(|id| !music::is_local_id(id));
        let stopped = matches!(
            playback.state(),
            PlaybackState::Idle | PlaybackState::Failed(_)
        );
        if id == self.following && !stopped {
            return;
        }
        self.following = id.clone();
        self.pipeline = None;
        self.opening = None;
        cx.notify();
        let Some(id) = id.filter(|_| !stopped) else {
            return;
        };
        let Some(client) = self.session.read(cx).client() else {
            return;
        };
        let opening = self.io.spawn({
            let id = id.clone();
            async move { client.open_video(&id).await }
        });
        let io = self.io.clone();
        self.opening = Some(cx.spawn(async move |this, cx| {
            let opened = join(opening).await;
            this.update(cx, |this, cx| {
                this.opening = None;
                if this.following.as_deref() != Some(id.as_str()) {
                    return;
                }
                match opened {
                    Ok(Some(stream)) => {
                        // The track may have played a while before its picture opened.
                        let position = this.playback.read(cx).live_position();
                        let pipeline = Pipeline::start(stream, &io);
                        pipeline.to_worker.send(Feed::Seek(position)).ok();
                        this.pipeline = Some(pipeline);
                        cx.notify();
                    }
                    Ok(None) => log::debug!("video: {id} has no picture of its own"),
                    Err(error) => log::warn!("video: cannot open the picture of {id}: {error:#}"),
                }
            })
            .ok();
        }));
    }
}

impl Pipeline {
    /// Starts downloading `stream` and the worker that decodes it.
    fn start(stream: music::video::VideoStream, io: &Io) -> Self {
        let shelf = Arc::new(Mutex::new(Shelf::default()));
        let (to_worker, from_feed) = mpsc::channel();
        let feed = to_worker.clone();
        let mut body = stream.body;
        let download = io
            .spawn(async move {
                loop {
                    match body.chunk().await {
                        Ok(Some(bytes)) => {
                            if feed.send(Feed::Bytes(bytes)).is_err() {
                                return;
                            }
                        }
                        Ok(None) => break,
                        Err(error) => {
                            log::warn!("video: the download broke off: {error:#}");
                            break;
                        }
                    }
                }
                feed.send(Feed::End).ok();
            })
            .abort_handle();
        let worker_shelf = shelf.clone();
        std::thread::Builder::new()
            .name("video".into())
            .spawn(move || {
                if let Err(error) = Worker::new(worker_shelf).run(from_feed) {
                    log::warn!("video: decoding stopped: {error:#}");
                }
            })
            .ok();
        Self {
            shelf,
            to_worker,
            last: None,
            shown: None,
            download,
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.download.abort();
    }
}

/// The decoding thread's state: the bytes so far, the fragments found in them and where
/// decoding has got to.
struct Worker {
    shelf: Arc<Mutex<Shelf>>,
    data: Vec<u8>,
    ended: bool,
    track: Option<Track>,
    decoder: Option<video::Decoder>,
    fragments: Vec<Fragment>,
    /// Where the next fragment starts once its bytes arrive.
    scanned: usize,
    /// The next sample to decode, as fragment and sample index.
    cursor: (usize, usize),
    /// Frames before this are decoded only to reach it and never shown.
    skip_before: Duration,
}

impl Worker {
    fn new(shelf: Arc<Mutex<Shelf>>) -> Self {
        Self {
            shelf,
            data: Vec::new(),
            ended: false,
            track: None,
            decoder: None,
            fragments: Vec::new(),
            scanned: 0,
            cursor: (0, 0),
            skip_before: Duration::ZERO,
        }
    }

    /// Decodes until the feed hangs up, which is when the pipeline drops.
    fn run(mut self, feed: Receiver<Feed>) -> anyhow::Result<()> {
        loop {
            let busy = self.room() && self.decode_one()?;
            let message = match busy {
                true => feed.try_recv().map_err(|error| match error {
                    mpsc::TryRecvError::Empty => RecvTimeoutError::Timeout,
                    mpsc::TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
                }),
                false => feed.recv_timeout(IDLE),
            };
            match message {
                Ok(Feed::Bytes(bytes)) => {
                    self.data.extend_from_slice(&bytes);
                    self.scan()?;
                }
                Ok(Feed::End) => self.ended = true,
                Ok(Feed::Seek(to)) => self.seek(to)?,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    }

    /// Whether the shelf has room for another frame.
    fn room(&self) -> bool {
        self.shelf
            .lock()
            .map(|shelf| shelf.frames.len() < AHEAD)
            .unwrap_or(false)
    }

    /// Reads the init segment once it is in, then every fragment that has fully arrived.
    fn scan(&mut self) -> anyhow::Result<()> {
        if self.track.is_none() {
            let Ok(track) = fmp4::read_init(&self.data) else {
                return Ok(());
            };
            self.scanned = track.fragments_at;
            self.decoder = Some(video::Decoder::new(&track)?);
            if let Ok(mut shelf) = self.shelf.lock() {
                shelf.width = track.width;
                shelf.height = track.height;
            }
            self.track = Some(track);
        }
        while let Some(fragment) = fmp4::read_fragment(&self.data, self.scanned)? {
            self.scanned = fragment.next;
            self.fragments.push(fragment);
        }
        Ok(())
    }

    /// Decodes the next sample, when it has arrived. Answers whether there was one.
    fn decode_one(&mut self) -> anyhow::Result<bool> {
        let (Some(track), Some(decoder)) = (self.track.as_ref(), self.decoder.as_mut()) else {
            return Ok(false);
        };
        // A seek target the bytes had not reached yet is jumped to once they have.
        let target = (self.skip_before.as_secs_f64() * track.timescale as f64) as u64;
        if let Some(jump) = self.fragments.iter().rposition(|fragment| {
            fragment
                .samples
                .first()
                .is_some_and(|first| first.show <= target)
        }) && jump > self.cursor.0
        {
            decoder.flush()?;
            self.cursor = (jump, 0);
        }
        let (fragment, sample) = self.cursor;
        let Some(found) = self.fragments.get(fragment) else {
            if self.ended && fragment == self.fragments.len() && fragment > 0 {
                let frames = decoder.drain()?;
                self.cursor = (fragment + 1, 0);
                self.shelve(frames);
            }
            return Ok(false);
        };
        let Some(coded) = found.samples.get(sample) else {
            self.cursor = (fragment + 1, 0);
            return Ok(true);
        };
        let seconds = |ticks: u64| Duration::from_secs_f64(ticks as f64 / track.timescale as f64);
        let nal = fmp4::annex_b(track, coded, &self.data)?;
        let frames = decoder.push(
            &nal,
            seconds(coded.show),
            seconds(coded.duration as u64),
            coded.key,
        )?;
        self.cursor = (fragment, sample + 1);
        self.shelve(frames);
        Ok(true)
    }

    /// Puts decoded frames on the shelf, leaving out those before a seek's target.
    fn shelve(&mut self, frames: Vec<video::Frame>) {
        let Ok(mut shelf) = self.shelf.lock() else {
            return;
        };
        for frame in frames {
            // The frame just before the target still shows until the target's own is due.
            if frame.at + Duration::from_millis(100) < self.skip_before {
                continue;
            }
            let Some(buffer) = image::RgbaImage::from_raw(frame.width, frame.height, frame.bgra)
            else {
                continue;
            };
            let image = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
            shelf.frames.push_back((frame.at, image));
        }
    }

    /// Restarts decoding from the key frame at or before `to`.
    fn seek(&mut self, to: Duration) -> anyhow::Result<()> {
        self.skip_before = to;
        let Some(track) = self.track.as_ref() else {
            return Ok(());
        };
        let ticks = (to.as_secs_f64() * track.timescale as f64) as u64;
        let fragment = self
            .fragments
            .iter()
            .rposition(|fragment| {
                fragment
                    .samples
                    .first()
                    .is_some_and(|first| first.show <= ticks)
            })
            .unwrap_or(0);
        if let Some(decoder) = self.decoder.as_mut() {
            decoder.flush()?;
        }
        if let Ok(mut shelf) = self.shelf.lock() {
            shelf.frames.clear();
        }
        self.cursor = (fragment, 0);
        Ok(())
    }
}
