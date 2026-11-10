//! The picture of the music video playing, kept in step with its sound.
//!
//! [`Video`] follows the track playing. When the track is a music video and the picture is
//! wanted, a tokio task downloads its video stream and a worker thread decodes it a few frames
//! ahead. A view asks [`Video::frame`] for the picture at the sound's position on every frame
//! it draws; the sound's clock is the only clock, so the picture never runs ahead of it or
//! drags behind. A jump in the position tells the worker to seek.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use gpui::{App, Context, Entity, RenderImage, Task};
use music::Track as Song;
use video::fmp4::{self, Fragment, Segment, Track};

use crate::{Io, Playback, PlaybackState, Session, join};

/// How many decoded frames the worker keeps ready ahead of the sound.
const AHEAD: usize = 8;
/// How far the position may move between two draws before it counts as a seek rather than
/// playback moving on.
const JUMP: Duration = Duration::from_millis(1500);
/// How long the worker waits for bytes or room before it looks at its messages again.
const IDLE: Duration = Duration::from_millis(10);
/// How long a download that is far enough ahead rests before it looks again.
const PAUSE: Duration = Duration::from_millis(100);
/// How far past the bytes in hand a seek target may sit and still be waited for. Past this,
/// the download starts over at the target instead.
const NEAR: usize = 2 * 1024 * 1024;
/// How many bytes of the file the download keeps ahead of decoding.
const AHEAD_BYTES: usize = 12 * 1024 * 1024;
/// How many decoded segments are kept behind playback, for a short seek back.
const BEHIND: usize = 2;

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
    /// The track followed itself, which becomes the other half once playback swaps to its pair.
    followed: Option<Song>,
    /// Whether a view shows the switch between a song and its video, so the other half is
    /// kept ready for it.
    attentive: bool,
    /// The picture of the other half, opened ahead so a switch to it shows at once.
    standby: Option<Standby>,
    /// Whether the track followed is a music video, `None` while that is being asked.
    kind: Option<bool>,
    /// The other half of the track followed, its music video or its song, once found.
    counterpart: Option<Song>,
    pairing: Option<Task<()>>,
    checking: Option<Task<()>>,
    /// Whether opening the picture of the track followed failed, so it is not asked again.
    failed: bool,
    pipeline: Option<Pipeline>,
    opening: Option<Task<()>>,
}

/// The picture of a track that is not playing yet, opened ahead.
struct Standby {
    id: String,
    /// The stream once it has opened. It reads nothing more until a pipeline takes it.
    stream: Option<music::video::VideoStream>,
    _opening: Task<()>,
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
    /// The next bytes of the download numbered by its lap, in file order from where it began.
    Bytes(u64, Bytes),
    /// The download of this lap reached the end of the file.
    End(u64),
    /// Playback moved to this position.
    Seek(Duration),
}

/// Where the worker sends the download next: the number of the new lap and the byte of the
/// file it starts at.
type Moves = tokio::sync::mpsc::UnboundedSender<(u64, u64)>;

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
            followed: None,
            attentive: false,
            standby: None,
            kind: None,
            counterpart: None,
            pairing: None,
            checking: None,
            failed: false,
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
        self.viewers = viewers;
        self.open(cx);
    }

    /// Says whether a view shows the switch between a song and its video. While one does, the
    /// other half is kept ready: its sound fetched ahead and its picture opened, so the switch
    /// is heard and seen at once.
    pub fn attend(&mut self, attentive: bool, cx: &mut Context<Self>) {
        if self.attentive == attentive {
            return;
        }
        self.attentive = attentive;
        match attentive {
            true => self.ready(cx),
            false => self.standby = None,
        }
    }

    /// Gets the other half of the track followed ready for a switch to it.
    fn ready(&mut self, cx: &mut Context<Self>) {
        if !self.attentive {
            return;
        }
        let (Some(other), Some(is_video)) = (self.counterpart.clone(), self.kind) else {
            return;
        };
        self.playback
            .update(cx, |playback, _| playback.preload(&other));
        // Only a song's other half has a picture to open.
        let Some(id) = other.id.filter(|_| !is_video) else {
            return;
        };
        if self
            .standby
            .as_ref()
            .is_some_and(|standby| standby.id == id)
        {
            return;
        }
        let Some(client) = self.session.read(cx).client() else {
            return;
        };
        let opening = self.io.spawn({
            let id = id.clone();
            async move { client.open_video(&id).await }
        });
        let task = cx.spawn({
            let id = id.clone();
            async move |this, cx| {
                let opened = join(opening).await;
                this.update(cx, |this, _| {
                    let Some(standby) = this.standby.as_mut().filter(|held| held.id == id) else {
                        return;
                    };
                    match opened {
                        Ok(stream) => standby.stream = stream,
                        Err(error) => {
                            log::debug!("video: cannot open {id} ahead: {error:#}");
                            this.standby = None;
                        }
                    }
                })
                .ok();
            }
        });
        self.standby = Some(Standby {
            id,
            stream: None,
            _opening: task,
        });
    }

    /// Whether the track playing is a music video, so it can be watched.
    pub fn has_video(&self) -> bool {
        self.kind == Some(true)
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
    /// Follows the track playing: learns whether it is a music video, and opens its picture
    /// once it is one and a view wants it. A picture already open stays open for as long as its
    /// track plays, so a view that hides and comes back shows it again at once.
    fn follow(&mut self, cx: &mut Context<Self>) {
        let playback = self.playback.read(cx);
        let track = playback
            .track()
            .filter(|track| {
                track
                    .id
                    .as_deref()
                    .is_some_and(|id| !music::is_local_id(id))
            })
            .cloned();
        let stopped = matches!(
            playback.state(),
            PlaybackState::Idle | PlaybackState::Failed(_)
        );
        let track = track.filter(|_| !stopped);
        let id = track.as_ref().and_then(|track| track.id.clone());
        if id != self.following {
            // A swap to the other half needs nothing asked: what it is and what pairs with it
            // are the reverse of what was known.
            let swapped = match (&self.counterpart, self.kind, &id) {
                (Some(other), Some(is_video), Some(id)) if other.id.as_ref() == Some(id) => {
                    self.followed.take().map(|previous| (previous, !is_video))
                }
                _ => None,
            };
            self.following = id.clone();
            self.followed = track;
            self.failed = false;
            self.pipeline = None;
            self.opening = None;
            self.checking = None;
            self.pairing = None;
            self.standby
                .take_if(|standby| Some(&standby.id) != id.as_ref());
            match (swapped, id) {
                (Some((previous, is_video)), Some(_)) => {
                    self.kind = Some(is_video);
                    self.counterpart = Some(previous);
                    cx.notify();
                    self.open(cx);
                    self.ready(cx);
                }
                (_, id) => {
                    self.kind = None;
                    self.counterpart = None;
                    cx.notify();
                    if let Some(id) = id {
                        self.check(id.clone(), cx);
                        self.pair(id, cx);
                    }
                }
            }
            return;
        }
        self.open(cx);
    }

    /// The other half of the track playing, its music video or its song, once found.
    pub fn counterpart(&self) -> Option<&Song> {
        self.counterpart.as_ref()
    }

    /// Looks for the other half of `id`, so the switch can move between a song and its video.
    fn pair(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(client) = self.session.read(cx).client() else {
            return;
        };
        let asking = self.io.spawn({
            let id = id.clone();
            async move { client.counterpart(&id).await }
        });
        self.pairing = Some(cx.spawn(async move |this, cx| {
            let answer = join(asking).await;
            this.update(cx, |this, cx| {
                this.pairing = None;
                if this.following.as_deref() != Some(id.as_str()) {
                    return;
                }
                match answer {
                    Ok(found) => this.counterpart = found,
                    Err(error) => log::debug!("video: no other half of {id}: {error:#}"),
                }
                cx.notify();
                this.ready(cx);
            })
            .ok();
        }));
    }

    /// Asks the provider whether `id` is a music video.
    fn check(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(client) = self.session.read(cx).client() else {
            return;
        };
        let asking = self.io.spawn({
            let id = id.clone();
            async move { client.has_video(&id).await }
        });
        self.checking = Some(cx.spawn(async move |this, cx| {
            let answer = join(asking).await;
            this.update(cx, |this, cx| {
                this.checking = None;
                if this.following.as_deref() != Some(id.as_str()) {
                    return;
                }
                this.kind = Some(answer.unwrap_or_else(|error| {
                    log::warn!("video: cannot tell whether {id} is a video: {error:#}");
                    false
                }));
                cx.notify();
                this.open(cx);
                this.ready(cx);
            })
            .ok();
        }));
    }

    /// Opens the picture of the track followed, when it has one, a view wants it, and it is not
    /// open, opening or refused already.
    fn open(&mut self, cx: &mut Context<Self>) {
        let wanted = self.viewers != 0 && self.kind == Some(true) && !self.failed;
        if !wanted || self.pipeline.is_some() || self.opening.is_some() {
            return;
        }
        let Some(id) = self.following.clone() else {
            return;
        };
        // A picture opened ahead starts at once.
        let ahead = self
            .standby
            .take_if(|standby| standby.id == id && standby.stream.is_some())
            .and_then(|standby| standby.stream);
        if let Some(stream) = ahead {
            let position = self.playback.read(cx).live_position();
            let pipeline = Pipeline::start(stream, &self.io);
            pipeline.to_worker.send(Feed::Seek(position)).ok();
            self.pipeline = Some(pipeline);
            cx.notify();
            return;
        }
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
                    }
                    Ok(None) => {
                        log::debug!("video: {id} has no picture of its own");
                        this.failed = true;
                    }
                    Err(error) => {
                        log::warn!("video: cannot open the picture of {id}: {error:#}");
                        this.failed = true;
                    }
                }
                cx.notify();
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
        let (moves, moved) = tokio::sync::mpsc::unbounded_channel();
        let limit = Arc::new(AtomicU64::new(u64::MAX));
        let download = io
            .spawn(download(
                stream.body,
                to_worker.clone(),
                moved,
                limit.clone(),
            ))
            .abort_handle();
        let worker_shelf = shelf.clone();
        std::thread::Builder::new()
            .name("video".into())
            .spawn(move || {
                if let Err(error) = Worker::new(worker_shelf, moves, limit).run(from_feed) {
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

/// Reads the body in file order and hands it to the worker. It starts over wherever the worker
/// sends it, and rests while it is `limit` bytes into the file, which is as far ahead of
/// decoding as the worker wants it.
async fn download(
    mut body: Box<dyn music::video::VideoBody>,
    feed: Sender<Feed>,
    mut moved: tokio::sync::mpsc::UnboundedReceiver<(u64, u64)>,
    limit: Arc<AtomicU64>,
) {
    let (mut lap, mut offset, mut ended) = (0u64, 0u64, false);
    loop {
        let resting = ended || offset >= limit.load(Ordering::Relaxed);
        tokio::select! {
            biased;
            to = moved.recv() => {
                let Some((next, at)) = to else {
                    return;
                };
                body.seek(at);
                (lap, offset, ended) = (next, at, false);
            }
            _ = tokio::time::sleep(PAUSE), if resting => {}
            chunk = body.chunk(), if !resting => match chunk {
                Ok(Some(bytes)) => {
                    offset += bytes.len() as u64;
                    if feed.send(Feed::Bytes(lap, bytes)).is_err() {
                        return;
                    }
                }
                Ok(None) => {
                    ended = true;
                    if feed.send(Feed::End(lap)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    log::warn!("video: the download broke off: {error:#}");
                    ended = true;
                }
            },
        }
    }
}

/// The decoding thread's state: the stretch of the file in hand, the fragments found in it and
/// where decoding has got to.
struct Worker {
    shelf: Arc<Mutex<Shelf>>,
    moves: Moves,
    /// How far into the file the download may read, kept a little ahead of decoding.
    limit: Arc<AtomicU64>,
    /// The number of the download whose bytes are taken. Bytes of an earlier one that were
    /// still on their way when it was sent elsewhere are dropped.
    lap: u64,
    /// The byte of the file `data` starts at.
    base: usize,
    data: Vec<u8>,
    ended: bool,
    track: Option<Track>,
    decoder: Option<video::Decoder>,
    /// The file's own index of its segments, empty when it has none.
    index: Vec<Segment>,
    /// Which segment of the index `fragments` starts at.
    first: usize,
    fragments: Vec<Fragment>,
    /// Where in `data` the next fragment starts once its bytes arrive.
    scanned: usize,
    /// The next sample to decode, as fragment and sample index.
    cursor: (usize, usize),
    /// Frames before this are decoded only to reach it and never shown.
    skip_before: Duration,
    /// When the picture was last asked for somewhere new, until its first frame is out.
    asked: Option<std::time::Instant>,
}

impl Worker {
    fn new(shelf: Arc<Mutex<Shelf>>, moves: Moves, limit: Arc<AtomicU64>) -> Self {
        Self {
            shelf,
            moves,
            limit,
            lap: 0,
            base: 0,
            data: Vec::new(),
            ended: false,
            track: None,
            decoder: None,
            index: Vec::new(),
            first: 0,
            fragments: Vec::new(),
            scanned: 0,
            cursor: (0, 0),
            skip_before: Duration::ZERO,
            asked: Some(std::time::Instant::now()),
        }
    }

    /// Decodes until the feed hangs up, which is when the pipeline drops.
    fn run(mut self, feed: Receiver<Feed>) -> anyhow::Result<()> {
        loop {
            let busy = self.room() && self.decode_one()?;
            self.pace();
            let message = match busy {
                true => feed.try_recv().map_err(|error| match error {
                    mpsc::TryRecvError::Empty => RecvTimeoutError::Timeout,
                    mpsc::TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
                }),
                false => feed.recv_timeout(IDLE),
            };
            match message {
                Ok(Feed::Bytes(lap, bytes)) if lap == self.lap => {
                    self.data.extend_from_slice(&bytes);
                    self.scan()?;
                }
                Ok(Feed::End(lap)) if lap == self.lap => self.ended = true,
                Ok(Feed::Bytes(..) | Feed::End(_)) => {}
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

    /// Reads the init segment and the index once they are in, then every fragment that has
    /// fully arrived.
    fn scan(&mut self) -> anyhow::Result<()> {
        if self.track.is_none() {
            let Ok(track) = fmp4::read_init(&self.data) else {
                return Ok(());
            };
            let Some(index) = fmp4::read_index(&self.data, track.fragments_at)? else {
                return Ok(());
            };
            self.scanned = track.fragments_at;
            let mut decoder = video::Decoder::new(&track)?;
            decoder.set_floor(self.skip_before);
            self.decoder = Some(decoder);
            if let Ok(mut shelf) = self.shelf.lock() {
                shelf.width = track.width;
                shelf.height = track.height;
            }
            self.track = Some(track);
            self.index = index;
            // Playback is usually somewhere into the video by now, and the index says where
            // in the file that is.
            let lap = self.lap;
            self.aim();
            if self.lap != lap {
                return Ok(());
            }
        }
        while let Some(fragment) = fmp4::read_fragment(&self.data, self.scanned)? {
            self.scanned = fragment.next;
            self.fragments.push(fragment);
        }
        Ok(())
    }

    /// Points decoding at the segment that covers the seek target. When the bytes in hand do
    /// not reach it, the download starts over there rather than reading its way to it.
    fn aim(&mut self) {
        let target = self.skip_before;
        let Some(want) = self
            .index
            .iter()
            .rposition(|segment| segment.start <= target)
        else {
            // No index to go by, so the target is found among the fragments in hand, and
            // waited for when it is past them.
            let ticks = self.ticks(target);
            let fragment = self.fragments.iter().rposition(|fragment| {
                fragment
                    .samples
                    .first()
                    .is_some_and(|first| first.show <= ticks)
            });
            self.cursor = (fragment.unwrap_or(0), 0);
            return;
        };
        let offset = self.index[want].offset;
        let reach = self.base + self.data.len() + NEAR;
        if want < self.first || offset < self.base || offset > reach {
            return self.restart(want);
        }
        self.cursor = (want - self.first, 0);
    }

    /// Starts the download over at segment `want` of the index and forgets the bytes in hand.
    fn restart(&mut self, want: usize) {
        let offset = self.index[want].offset;
        self.lap += 1;
        self.base = offset;
        self.first = want;
        self.data.clear();
        self.fragments.clear();
        self.scanned = 0;
        self.cursor = (0, 0);
        self.ended = false;
        self.limit
            .store((offset + AHEAD_BYTES) as u64, Ordering::Relaxed);
        self.moves.send((self.lap, offset as u64)).ok();
    }

    /// Keeps the download a little ahead of decoding and lets go of the segments well behind
    /// it, so a long video is never held whole.
    fn pace(&mut self) {
        if self.cursor.0 > BEHIND && self.cursor.0 <= self.fragments.len() {
            let gone = self.cursor.0 - BEHIND;
            let cut = self.fragments[gone - 1].next.min(self.data.len());
            self.data.drain(..cut);
            self.fragments.drain(..gone);
            for fragment in &mut self.fragments {
                fragment.next -= cut;
                for sample in &mut fragment.samples {
                    sample.bytes = sample.bytes.start - cut..sample.bytes.end - cut;
                }
            }
            self.scanned -= cut;
            self.base += cut;
            self.first += gone;
            self.cursor.0 -= gone;
        }
        let decoded = match self.cursor.0.checked_sub(1) {
            Some(before) => self
                .fragments
                .get(before)
                .map_or(self.data.len(), |fragment| fragment.next),
            None => 0,
        };
        self.limit.store(
            (self.base + decoded + AHEAD_BYTES) as u64,
            Ordering::Relaxed,
        );
    }

    /// A time in the track's own ticks.
    fn ticks(&self, at: Duration) -> u64 {
        let timescale = self.track.as_ref().map_or(1, |track| track.timescale);
        (at.as_secs_f64() * timescale as f64) as u64
    }

    /// Decodes the next sample, when it has arrived. Answers whether there was one.
    fn decode_one(&mut self) -> anyhow::Result<bool> {
        // Without an index, a seek target the bytes had not reached is jumped to once they have.
        if self.index.is_empty() && self.track.is_some() {
            let target = self.ticks(self.skip_before);
            let jump = self.fragments.iter().rposition(|fragment| {
                fragment
                    .samples
                    .first()
                    .is_some_and(|first| first.show <= target)
            });
            if let Some(jump) = jump.filter(|jump| *jump > self.cursor.0) {
                if let Some(decoder) = self.decoder.as_mut() {
                    decoder.flush()?;
                }
                self.cursor = (jump, 0);
            }
        }
        let (Some(track), Some(decoder)) = (self.track.as_ref(), self.decoder.as_mut()) else {
            return Ok(false);
        };
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
        // A fragment is known from its header on, ahead of the pictures it names.
        if coded.bytes.end > self.data.len() {
            return Ok(false);
        }
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

    /// Puts decoded frames on the shelf.
    fn shelve(&mut self, frames: Vec<video::Frame>) {
        let Ok(mut shelf) = self.shelf.lock() else {
            return;
        };
        for frame in frames {
            if let Some(asked) = self.asked.take() {
                log::debug!(
                    "video: the frame at {:?} is out {:?} after it was asked for",
                    frame.at,
                    asked.elapsed()
                );
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
        self.asked.get_or_insert_with(std::time::Instant::now);
        if let Some(decoder) = self.decoder.as_mut() {
            decoder.flush()?;
            decoder.set_floor(to);
        }
        if let Ok(mut shelf) = self.shelf.lock() {
            shelf.frames.clear();
        }
        if self.track.is_some() {
            self.aim();
        }
        Ok(())
    }
}
