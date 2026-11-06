//! The Video tab of the right sidebar: the music video playing, at the width of the panel,
//! beside the rest of the app, with a way into the fullscreen player to watch it large.
//! `state::Video` downloads and decodes it only while this tab or the fullscreen player shows.

use gpui::accesskit::Role;
use gpui::prelude::*;
use gpui::{Context, Entity, ObjectFit, Render, SharedString, Window, div, img};
use i18n::t;
use input::ToggleFullscreen;
use state::{Playback, Sonora, Video, Viewer};
use ui::{ActiveTheme as _, Button, Text, eyebrow, snapped, vacant};

use crate::shared::picture::{self, Picture};

pub(crate) struct VideoPanel {
    video: Entity<Video>,
    playback: Entity<Playback>,
    picture: Picture,
}

impl VideoPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let video = Sonora::global(cx).video.clone();
        let playback = Sonora::global(cx).playback.clone();
        cx.observe(&video, |_, _, cx| cx.notify()).detach();
        cx.observe(&playback, |_, _, cx| cx.notify()).detach();
        Self {
            video,
            playback,
            picture: Picture::default(),
        }
    }

    /// Says whether the tab is on screen, so the picture is decoded only while it is.
    pub fn set_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        self.video
            .update(cx, |video, cx| video.watch(Viewer::Panel, shown, cx));
    }
}

impl Render for VideoPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .h(snapped(theme.metrics.header, window))
            .px_2()
            .border_b_1()
            .border_color(theme.border)
            .child(eyebrow(t!("video-title"), cx))
            .child(
                Button::new("video-expand")
                    .ghost()
                    .small()
                    .icon("icons/maximize.svg")
                    .tooltip("player-fullscreen")
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(ToggleFullscreen), cx)
                    }),
            );
        let root = div()
            .id("video-panel")
            .role(Role::Complementary)
            .aria_label(t!("video-title"))
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .min_w_0()
            .child(header);

        let track = self.playback.read(cx).shown_track(cx);
        let frame = self.picture.frame(&self.video, &self.playback, window, cx);
        let (has_picture, opening) = {
            let video = self.video.read(cx);
            (video.has_picture(), video.is_opening())
        };
        let Some(track) = track.filter(|_| has_picture || opening) else {
            return root.child(
                vacant(t!("video-none"), cx)
                    .id("video-none")
                    .role(Role::Label)
                    .aria_value(t!("video-none"))
                    .flex_1(),
            );
        };

        let screen = div()
            .id("video-screen")
            .role(Role::Image)
            .aria_label(SharedString::from(format!(
                "{} — {}",
                track.name, track.artists
            )))
            .relative()
            .w_full()
            .aspect_ratio(picture::shape(&self.video, cx))
            .rounded(theme.radius * 2.)
            .overflow_hidden()
            .bg(gpui::black())
            .cursor_pointer()
            .on_click(|event, window, cx| {
                if event.click_count() == 2 {
                    window.dispatch_action(Box::new(ToggleFullscreen), cx);
                }
            })
            // The screen stays black until the first frame is decoded.
            .when_some(frame, |this, frame| {
                this.child(img(frame).size_full().object_fit(ObjectFit::Contain))
            });

        root.child(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .p_2()
                .min_w_0()
                .child(screen)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .child(
                            div()
                                .text_size(theme.text(Text::Title))
                                .text_color(theme.foreground)
                                .truncate()
                                .child(track.name.clone()),
                        )
                        .child(
                            div()
                                .text_size(theme.text(Text::Body))
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(track.artists.clone()),
                        ),
                ),
        )
    }
}
