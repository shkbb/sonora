//! The Now Playing tab of the right sidebar, laid out the way Spotify's is: the song's cover,
//! or its music video while the switch on it says Video, then the song and its artists, a few
//! lines of its lyrics and a card about the artist. `state::Video` decodes the video only while
//! this tab shows it.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui::accesskit::Role;
use gpui::prelude::*;
use gpui::{
    AnyElement, Bounds, Context, Entity, MouseUpEvent, ObjectFit, Pixels, Render, SharedString,
    Window, canvas, div, img, px,
};
use i18n::t;
use input::{ShowAudio, ShowVideo, ToggleFullscreen, ToggleLyrics};
use router::{Destination, navigate};
use state::{AppSettings, Lyrics, Playback, PlaybackState, SongDetail, Sonora, Video, Viewer};
use ui::{
    ActiveTheme as _, Artwork, Button, Scrollbar, Scroller, Scrubber, ScrubberState, TabBar, Text,
    clock, eyebrow, heading, snapped,
};

use crate::shared::about::AboutArtist;
use crate::shared::picture::{self, Picture};
use crate::shared::transport::{fraction, like};

/// The group the video's controls show with while the pointer is over it.
const HOVER: &str = "now-playing-screen";
/// How large the cover is asked for, past what the panel is ever wide on a dense screen.
const COVER_EDGE: u32 = 1200;
/// How bright the choice not taken reads on the Audio | Video switch.
const UNCHOSEN: f32 = 0.7;
/// How many lines of the lyrics the preview shows.
const PREVIEW_LINES: usize = 3;

pub(crate) struct NowPlaying {
    video: Entity<Video>,
    playback: Entity<Playback>,
    settings: Entity<AppSettings>,
    lyrics: Entity<Lyrics>,
    /// The song page of the track playing, for its artist's profile.
    detail: Entity<SongDetail>,
    scrollbar: Entity<Scrollbar>,
    picture: Picture,
    seek: ScrubberState,
    /// Where the position is being dragged to, applied on release.
    pending: Option<f32>,
    shown: bool,
    /// The width the content had last frame, which the cover and the artist's photo fill.
    width: Rc<Cell<Pixels>>,
}

impl NowPlaying {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let sonora = Sonora::global(cx);
        let (video, playback, settings, lyrics, session) = (
            sonora.video.clone(),
            sonora.playback.clone(),
            sonora.settings.clone(),
            sonora.lyrics.clone(),
            sonora.session.clone(),
        );
        let io = state::Io::global(cx);
        let detail = cx.new(|cx| SongDetail::new(session, io, cx));
        cx.observe(&video, |_, _, cx| cx.notify()).detach();
        cx.observe(&lyrics, |_, _, cx| cx.notify()).detach();
        cx.observe(&detail, |_, _, cx| cx.notify()).detach();
        cx.observe(&playback, |this, _, cx| {
            this.follow(cx);
            cx.notify();
        })
        .detach();
        cx.observe(&settings, |this, _, cx| {
            this.watch(cx);
            cx.notify();
        })
        .detach();
        // The bar shows only while the panel scrolls, so it never sits beside the cover.
        let scrollbar = cx.new(|_| Scrollbar::new(gpui::ScrollHandle::new()));
        Self {
            video,
            playback,
            settings,
            lyrics,
            detail,
            scrollbar,
            picture: Picture::default(),
            seek: ScrubberState::new("now-playing-seek"),
            pending: None,
            shown: false,
            width: Rc::new(Cell::new(Pixels::ZERO)),
        }
    }

    /// Says whether the tab is on screen. The song page loads and the video decodes only
    /// while it is.
    pub fn set_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        self.shown = shown;
        self.watch(cx);
        self.follow(cx);
    }

    /// Asks for the video while the tab shows and the switch says Video.
    fn watch(&mut self, cx: &mut Context<Self>) {
        let wanted = self.shown && self.settings.read(cx).video_mode();
        self.video
            .update(cx, |video, cx| video.watch(Viewer::Panel, wanted, cx));
    }

    /// Loads the song page of the track playing while the tab shows.
    fn follow(&mut self, cx: &mut Context<Self>) {
        if !self.shown {
            return;
        }
        let Some(id) = self
            .playback
            .read(cx)
            .shown_track(cx)
            .and_then(|track| track.id)
        else {
            return;
        };
        self.detail.update(cx, |detail, cx| detail.open(&id, cx));
    }

    fn commit_seek(&mut self, cx: &mut Context<Self>) {
        let Some(fraction) = self.pending.take() else {
            return;
        };
        self.playback
            .update(cx, |playback, cx| playback.seek_fraction(fraction, cx));
    }

    /// The Audio | Video switch over the cover, for a track that is a music video.
    fn switch(&self, video: bool, cx: &mut Context<Self>) -> TabBar {
        let theme = *cx.theme();
        let option = |id: &'static str, label: SharedString, choice: bool| {
            let chosen = video == choice;
            Button::new(id)
                .ghost()
                .small()
                .label(label)
                .selected(chosen)
                .rounded(theme.radius)
                // The switch sits on the cover, so the other choice stays bright enough to
                // read over any picture.
                .tint(match chosen {
                    true => theme.foreground,
                    false => theme.foreground.opacity(UNCHOSEN),
                })
                .on_click(move |_, window, cx| {
                    let action: Box<dyn gpui::Action> = match choice {
                        true => Box::new(ShowVideo),
                        false => Box::new(ShowAudio),
                    };
                    window.dispatch_action(action, cx);
                })
        };
        TabBar::new("now-playing-mode")
            .name(t!("player-mode"))
            .blurred()
            .items([
                option("now-playing-audio", t!("player-mode-audio"), false),
                option("now-playing-video", t!("player-mode-video"), true),
            ])
    }

    /// The music video with its controls, which show while the pointer is over it.
    fn screen(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.theme();
        let frame = self.picture.frame(&self.video, &self.playback, window, cx);
        let playback = self.playback.read(cx);
        let playing = *playback.state() == PlaybackState::Playing;
        let (elapsed, total) = playback.shown_time(cx);
        let progress = self.pending.unwrap_or_else(|| fraction(elapsed, total));
        let shown_at = self.pending.map_or(elapsed, |at| total.mul_f32(at));
        let playback = self.playback.clone();
        let time = |value: Duration| {
            div()
                .flex_none()
                .text_size(theme.text(Text::Tiny))
                .text_color(theme.foreground)
                .child(clock(value))
        };

        div()
            .id("now-playing-screen")
            .group(HOVER)
            .relative()
            .size_full()
            .bg(gpui::black())
            .on_click(|event, window, cx| {
                if event.click_count() == 2 {
                    window.dispatch_action(Box::new(ToggleFullscreen), cx);
                }
            })
            // The screen stays black until the first frame is decoded.
            .when_some(frame, |this, frame| {
                this.child(img(frame).size_full().object_fit(ObjectFit::Contain))
            })
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .justify_end()
                    .opacity(0.)
                    .group_hover(HOVER, |this| this.opacity(1.))
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                Button::new("now-playing-play")
                                    .ghost()
                                    .frosted()
                                    .icon(match playing {
                                        true => "icons/pause.svg",
                                        false => "icons/play.svg",
                                    })
                                    .tooltip(match playing {
                                        true => "play-pause",
                                        false => "play-resume",
                                    })
                                    .on_click(move |_, _, cx| {
                                        playback.update(cx, |playback, cx| playback.toggle_play(cx))
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .relative()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .pb_1()
                            .bg(gpui::black().opacity(0.45))
                            .child(time(shown_at))
                            .child(
                                div().flex_1().min_w_0().child(
                                    Scrubber::new(&self.seek, progress)
                                        .name(t!("song-position"))
                                        .value(clock(elapsed))
                                        .colors(
                                            theme.foreground,
                                            theme.foreground.opacity(0.3),
                                            theme.foreground,
                                        )
                                        .on_move(cx.listener(|this, at: &f32, _, cx| {
                                            this.pending = Some(*at);
                                            cx.notify();
                                        }))
                                        .on_release(cx.listener(
                                            |this, _: &MouseUpEvent, _, cx| this.commit_seek(cx),
                                        )),
                                ),
                            )
                            .child(time(total))
                            .child(
                                Button::new("now-playing-expand")
                                    .ghost()
                                    .small()
                                    .icon("icons/maximize.svg")
                                    .tooltip("player-fullscreen")
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(Box::new(ToggleFullscreen), cx)
                                    }),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// A few lines of the lyrics around the one being sung, which open the whole lyrics.
    fn preview(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.theme();
        let hit = self.lyrics.read(cx).current()?;
        let lines: Vec<(String, bool)> = match &hit.lyrics {
            music::Lyrics::Synced { lines } => {
                let at = self.playback.read(cx).shown_time(cx).0;
                let active = music::lyrics::active(lines, at);
                let from = active.unwrap_or(0);
                lines
                    .iter()
                    .enumerate()
                    .skip(from)
                    .filter(|(_, line)| !line.text.trim().is_empty())
                    .take(PREVIEW_LINES)
                    .map(|(index, line)| (line.text.clone(), Some(index) == active))
                    .collect()
            }
            music::Lyrics::Plain { text, .. } => text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .take(PREVIEW_LINES)
                .map(|line| (line.to_owned(), false))
                .collect(),
        };
        if lines.is_empty() {
            return None;
        }
        let fill = theme
            .tint
            .map(|tint| theme.primary_of(tint).opacity(0.35))
            .unwrap_or(theme.secondary);
        Some(
            div()
                .id("now-playing-lyrics")
                .role(Role::Button)
                .aria_label(t!("now-playing-lyrics"))
                .flex()
                .flex_col()
                .gap_2()
                .p_4()
                .rounded(theme.radius * 2.)
                .bg(fill)
                .cursor_pointer()
                .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleLyrics), cx))
                .child(eyebrow(t!("now-playing-lyrics"), cx))
                .children(lines.into_iter().map(|(line, active)| {
                    div()
                        .text_size(theme.text(Text::Title))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(match active {
                            true => theme.foreground,
                            false => theme.foreground.opacity(0.55),
                        })
                        .child(line)
                }))
                .into_any_element(),
        )
    }
}

impl Render for NowPlaying {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .h(snapped(theme.metrics.header, window))
            .px_2()
            .border_b_1()
            .border_color(theme.border)
            .child(eyebrow(t!("now-playing-title"), cx));
        let root = div()
            .id("now-playing")
            .role(Role::Complementary)
            .aria_label(t!("now-playing-title"))
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .min_w_0()
            .child(header);

        let Some(track) = self.playback.read(cx).shown_track(cx) else {
            return root.child(
                ui::vacant(t!("player-nothing-playing"), cx)
                    .id("now-playing-empty")
                    .role(Role::Label)
                    .aria_value(t!("player-nothing-playing"))
                    .flex_1(),
            );
        };

        let width = self.width.get();
        let has_video = self.video.read(cx).has_video();
        let video = has_video && self.settings.read(cx).video_mode();
        let media = match video {
            true => self.screen(window, cx),
            false => {
                // The cover is cut square only once the panel's width is known.
                Artwork::new(
                    track
                        .cover
                        .as_deref()
                        .map(|cover| music::artwork::larger(cover, COVER_EDGE)),
                )
                .size(width.max(px(1.)))
                .into_any_element()
            }
        };
        let shape = match video {
            true => picture::shape(&self.video, cx),
            false => 1.,
        };
        let artist = self.detail.read(cx).artist().cloned();
        let artist_id = track
            .artist_refs
            .first()
            .and_then(|artist| artist.id.clone());
        let measure = self.width.clone();
        // A song with a music video of its own switches to it, as a music video switches to
        // its song, so the switch shows for either half.
        let paired = self.video.read(cx).counterpart().is_some();
        let switch = (has_video || paired).then(|| self.switch(video, cx));
        let preview = self.preview(cx);

        root.child(
            div().flex_1().min_h_0().child(
                Scroller::new("now-playing-scroll", &self.scrollbar).child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .p_3()
                        .child(
                            canvas(
                                move |bounds: Bounds<Pixels>, _, _| measure.set(bounds.size.width),
                                |_, _, _, _| {},
                            )
                            .w_full()
                            .h(px(0.)),
                        )
                        .child(
                            div()
                                .id("now-playing-media")
                                .relative()
                                .w_full()
                                .aspect_ratio(shape)
                                .rounded(theme.radius * 2.)
                                .overflow_hidden()
                                .child(media)
                                .when_some(switch, |this, switch| {
                                    this.child(div().absolute().top_2().left_2().child(switch))
                                }),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .min_w_0()
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .flex_1()
                                        .min_w_0()
                                        .child(heading(track.name.clone(), cx).truncate())
                                        .child(
                                            crate::shared::cells::artist_links(
                                                "now-playing-artists",
                                                track.artist_refs.clone(),
                                                track.artists.clone(),
                                                theme.muted_foreground,
                                            )
                                            .truncate(),
                                        ),
                                )
                                .child(like(Some(&track), &self.playback, cx)),
                        )
                        .children(preview)
                        .when_some(artist, |this, artist| {
                            this.child(
                                AboutArtist::new("now-playing-artist", artist.name.clone())
                                    .cover(artist.banner.clone().or(artist.cover_large.clone()))
                                    .square(artist.banner.is_none())
                                    .biography(artist.biography.clone())
                                    .stacked(width)
                                    .when_some(artist_id, |card, id| {
                                        card.on_open(move |_, _, cx| {
                                            navigate(Destination::Artist(id.clone().into()), cx)
                                        })
                                    }),
                            )
                        }),
                ),
            ),
        )
    }
}
