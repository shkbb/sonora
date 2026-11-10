use gpui::accesskit::Role;
use gpui::prelude::*;
use gpui::{Div, SharedString, Stateful};

use crate::{Destination, navigate, reveal};

pub trait Link: Sized {
    fn link(self, to: Destination) -> Self;
    /// A link that also picks out the row holding `row` on the page it opens.
    fn link_row(self, to: Destination, row: SharedString) -> Self;
}

impl Link for Stateful<Div> {
    fn link(self, to: Destination) -> Self {
        // The role alone makes the link a node; its name stays with the
        // caller, which owns the label the link wraps.
        self.role(Role::Link)
            .cursor_pointer()
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                navigate(to.clone(), cx);
            })
    }

    fn link_row(self, to: Destination, row: SharedString) -> Self {
        self.role(Role::Link)
            .cursor_pointer()
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                reveal(to.clone(), row.clone(), cx);
            })
    }
}
