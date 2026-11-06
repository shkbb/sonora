//! Watch reports for YouTube Music: what the YouTube Music page tells YouTube while a track
//! plays, so the play lands in the account's history and its watch time counts.
//!
//! One [`Watch`] lives per track that plays. It sends the playback report when the track
//! starts, then watchtime reports with the stretches heard: on a timer while the track plays,
//! on every pause and seek, and a last one when the track stops or another one starts. The
//! reports are only sent for a signed-in session, since a guest's plays belong to nobody.

use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tokio::task::AbortHandle;
use ytmusic::{Tracking, Watched, YtMusic};

use crate::Report;

/// When the timed watchtime reports go out, counted from the start of the watch: three close
/// together at first, as the YouTube Music page sends them, then one every [`STEADY`].
const EARLY: [Duration; 3] = [
    Duration::from_secs(10),
    Duration::from_secs(20),
    Duration::from_secs(30),
];
const STEADY: Duration = Duration::from_secs(40);
/// How far, in seconds, a reported position may sit from where the clock puts playback before
/// the report counts as a seek.
const DRIFT: f64 = 2.0;

/// Reports the plays of one signed-in YouTube Music session.
pub(crate) struct Stats {
    api: Arc<YtMusic>,
    watch: Mutex<Option<Watch>>,
    /// This reporter, for the timer task, which must not keep it alive.
    me: Weak<Stats>,
}

/// One track playing, from its first report to its last.
struct Watch {
    id: String,
    /// Where to report this watch. `None` when YouTube named nowhere, which leaves the watch
    /// silent rather than asking again on every pause.
    tracking: Option<Tracking>,
    begun: Instant,
    /// Where the stretch playing now began and when, `None` while paused.
    running: Option<(f64, Instant)>,
    /// The stretches heard since the last watchtime report.
    heard: Vec<(f64, f64)>,
    timer: Option<AbortHandle>,
}

impl Stats {
    pub(crate) fn new(api: Arc<YtMusic>) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            api,
            watch: Mutex::new(None),
            me: me.clone(),
        })
    }

    /// Follows the player: `report` says what the track `id` is doing at `position`. A track
    /// other than the one being watched ends that watch first.
    pub(crate) async fn report(&self, id: &str, report: Report, position: Duration) {
        let at = position.as_secs_f64();
        let mut slot = self.watch.lock().await;
        if let Some(watch) = slot.take_if(|watch| watch.id != id) {
            self.finish(watch, None).await;
        }
        match (report, slot.as_mut()) {
            (Report::Playing, None) => *slot = Some(self.begin(id, at).await),
            (Report::Playing, Some(watch)) => {
                if let Some(expected) = watch.position() {
                    if (expected - at).abs() <= DRIFT {
                        return;
                    }
                    watch.close(expected);
                }
                watch.running = Some((at, Instant::now()));
            }
            (Report::Paused, Some(watch)) => {
                let was_running = watch.running.is_some();
                watch.close(at);
                if was_running {
                    self.send(watch, at, true, false).await;
                }
            }
            (Report::Paused, None) => {}
            (Report::Stopped, _) => {
                if let Some(watch) = slot.take() {
                    self.finish(watch, Some(at)).await;
                }
            }
        }
    }

    /// Starts watching `id` from `at`: asks where to report it, sends the playback report and
    /// starts the timer.
    async fn begin(&self, id: &str, at: f64) -> Watch {
        let tracking = self
            .api
            .playback_tracking(id)
            .await
            .inspect_err(|error| log::warn!("stats: {id} will not be reported: {error:#}"))
            .ok();
        let mut watch = Watch {
            id: id.to_owned(),
            tracking,
            begun: Instant::now(),
            running: Some((at, Instant::now())),
            heard: Vec::new(),
            timer: None,
        };
        let Some(tracking) = &watch.tracking else {
            return watch;
        };
        match tracking.started(at, 0.) {
            Ok(url) => self.post(id, &url).await,
            Err(error) => log::warn!("stats: cannot build the playback report: {error:#}"),
        }
        log::debug!("stats: watching {id} as {}", tracking.cpn());
        watch.timer = Some(self.time(id.to_owned()));
        watch
    }

    /// Sends the last report of a watch, the stretch playing now ending at `at`, or where the
    /// clock puts it when the player gave no position.
    async fn finish(&self, mut watch: Watch, at: Option<f64>) {
        let Some(end) = at.or_else(|| watch.position()) else {
            if !watch.heard.is_empty() {
                let at = watch.heard.last().map_or(0., |stretch| stretch.1);
                self.send(&mut watch, at, true, true).await;
            }
            return;
        };
        watch.close(end);
        self.send(&mut watch, end, true, true).await;
    }

    /// The timed report: what has played since the last one, for a watch still playing.
    async fn tick(&self, id: &str) {
        let mut slot = self.watch.lock().await;
        let Some(watch) = slot.as_mut().filter(|watch| watch.id == id) else {
            return;
        };
        let Some(at) = watch.position() else {
            return;
        };
        watch.close(at);
        watch.running = Some((at, Instant::now()));
        self.send(watch, at, false, false).await;
    }

    /// Sends a watchtime report of the stretches heard, then forgets them.
    async fn send(&self, watch: &mut Watch, at: f64, paused: bool, last: bool) {
        let heard = std::mem::take(&mut watch.heard);
        let Some(tracking) = &watch.tracking else {
            return;
        };
        let url = tracking.watched(&Watched {
            heard: &heard,
            position: at,
            elapsed: watch.begun.elapsed().as_secs_f64(),
            paused,
            last,
        });
        match url {
            Ok(url) => self.post(&watch.id, &url).await,
            Err(error) => log::warn!("stats: cannot build the watchtime report: {error:#}"),
        }
    }

    async fn post(&self, id: &str, url: &str) {
        if let Err(error) = self.api.send_stats(url).await {
            log::warn!("stats: cannot report {id}: {error:#}");
        }
    }

    /// Starts the timer that sends the timed reports of `id`. It ends with the watch, which
    /// aborts it, or with the reporter.
    fn time(&self, id: String) -> AbortHandle {
        let me = self.me.clone();
        tokio::spawn(async move {
            let begun = tokio::time::Instant::now();
            let steady = (1..).map(|count| EARLY[2] + STEADY * count);
            for mark in EARLY.into_iter().chain(steady) {
                tokio::time::sleep_until(begun + mark).await;
                let Some(stats) = me.upgrade() else {
                    return;
                };
                stats.tick(&id).await;
            }
        })
        .abort_handle()
    }
}

impl Watch {
    /// Where the clock puts playback now, `None` while paused.
    fn position(&self) -> Option<f64> {
        self.running
            .map(|(from, since)| from + since.elapsed().as_secs_f64())
    }

    /// Ends the stretch playing now at `at` and keeps it for the next report. A stretch that
    /// ran backwards, as a seek landing early can make it, is dropped.
    fn close(&mut self, at: f64) {
        let Some((from, _)) = self.running.take() else {
            return;
        };
        if at > from {
            self.heard.push((from, at));
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        if let Some(timer) = self.timer.take() {
            timer.abort();
        }
    }
}
