//! Hold a message at a given viewport Y using this frame's layout.

use std::{cell::Cell, rc::Rc};

use gpui::{AnyElement, App, Bounds, IntoElement, LayoutId, Pixels, ScrollHandle, Window, px};

use crate::{frame_log::SharedFrameLog, present};

/// A message's index and its top relative to the scroll viewport.
#[derive(Clone, Copy)]
pub(super) struct ScrollAnchor {
    pub index: usize,
    pub y: Pixels,
}

impl ScrollAnchor {
    /// Capture the first message with a visible part, including a clipped top.
    pub fn capture(scroll: &ScrollHandle) -> Option<Self> {
        let view = scroll.bounds();
        // GPUI clamps wheel deltas during prepaint. Capture the position
        // the wheel would have drawn with the old content.
        let raw_offset = scroll.offset().y;
        let offset = clamped_offset(scroll);
        // top_item uses the raw offset, which can be past the last visible
        // message if the wheel overshot the bottom.
        let start = if raw_offset < offset {
            0
        } else {
            scroll.top_item()
        };
        (start..scroll.children_count()).find_map(|index| {
            let item = scroll.bounds_for_item(index)?;
            visible_top(view, offset, item).map(|y| Self { index, y })
        })
    }

    /// Capture a selected message, which can be below the first visible one.
    fn capture_message(scroll: &ScrollHandle, index: usize) -> Option<Self> {
        let item = scroll.bounds_for_item(index)?;
        visible_top(scroll.bounds(), clamped_offset(scroll), item).map(|y| Self { index, y })
    }
}

fn clamped_offset(scroll: &ScrollHandle) -> Pixels {
    scroll.offset().y.clamp(-scroll.max_offset().height, px(0.))
}

/// A short, untouched conversation may reveal its newest message after a
/// body download. Reader input takes over until another thread opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reveal {
    Waiting,
    /// The next frame lays out the bodies.
    Laying,
    /// The new message bounds can now be read.
    Ready,
    /// Hold a selected message after message navigation, or the first visible
    /// message after scrolling.
    Reading {
        selected: bool,
    },
}

impl Reveal {
    pub fn pending(self) -> bool {
        matches!(self, Self::Laying | Self::Ready)
    }

    pub fn body_changed(&mut self, previously_fit: bool) {
        if self.pending() || (*self == Self::Waiting && previously_fit) {
            *self = Self::Laying;
        }
    }

    pub fn cancel_pending(&mut self) {
        if self.pending() {
            *self = Self::Waiting;
        }
    }

    pub fn body_anchor(self, scroll: &ScrollHandle, cursor: usize) -> Option<ScrollAnchor> {
        if self.pending() {
            return None;
        }
        if let Self::Reading { selected } = self {
            if selected && let Some(anchor) = ScrollAnchor::capture_message(scroll, cursor) {
                return Some(anchor);
            }
        } else if present::conversation_fits(f32::from(scroll.max_offset().height)) {
            return None;
        }
        ScrollAnchor::capture(scroll)
    }
}

fn visible_top(view: Bounds<Pixels>, offset: Pixels, item: Bounds<Pixels>) -> Option<Pixels> {
    let top = item.top() + offset;
    (view.size.height > px(0.) && item.bottom() + offset > view.top() && top < view.bottom())
        .then(|| top - view.top())
}

fn offset_to_hold(view: Bounds<Pixels>, item: Bounds<Pixels>, y: Pixels) -> Pixels {
    (y - (item.top() - view.top())).min(px(0.))
}

pub(super) type LayoutSlot = Rc<Cell<Option<LayoutId>>>;

/// Apply the offset before the scroll div prepaints its children. GPUI's
/// `scroll_to_top_of_item` uses the previous frame's viewport bounds, which
/// can put the message in the wrong place for a frame if the header moves.
pub(super) struct LandingHold {
    pub inner: AnyElement,
    /// The layout of the reader's message, from [`LayoutProbe`].
    pub probe: LayoutSlot,
    pub scroll: ScrollHandle,
    pub y: Pixels,
    /// `FELL_FRAME_LOG`: each offset set here is a `landing` note.
    pub frames: Option<SharedFrameLog>,
}

impl IntoElement for LandingHold {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl gpui::Element for LandingHold {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.inner.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let before = self.scroll.offset();
        if let Some(item) = self.probe.get() {
            // Both bounds come from this frame before the scroll offset.
            // The scroll div clamps the result against its new content height.
            let item = window.layout_bounds(item);
            let mut offset = before;
            offset.y = offset_to_hold(bounds, item, self.y);
            self.scroll.set_offset(offset);
        }
        self.inner.prepaint(window, cx);
        // What the scroll area drew at, once clamped, if it moved.
        let after = f32::from(self.scroll.offset().y);
        if (after - f32::from(before.y)).abs() > 0.5
            && let Some(log) = &self.frames
        {
            log.borrow_mut().note("landing", after);
        }
    }

    fn paint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inner.paint(window, cx);
    }
}

/// Records the layout of the element it wraps, for [`LandingHold`].
pub(super) struct LayoutProbe {
    pub inner: AnyElement,
    pub slot: LayoutSlot,
}

impl IntoElement for LayoutProbe {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl gpui::Element for LayoutProbe {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let id = self.inner.request_layout(window, cx);
        self.slot.set(Some(id));
        (id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inner.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inner.paint(window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size};

    fn vertical_bounds(y: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(0.), px(y)), size(px(100.), px(height)))
    }

    #[test]
    fn only_an_untouched_conversation_can_reveal_a_new_body() {
        let mut reveal = Reveal::Waiting;
        reveal.body_changed(false);
        assert!(!reveal.pending());
        reveal.body_changed(true);
        assert_eq!(reveal, Reveal::Laying);
        reveal = Reveal::Ready;
        reveal.body_changed(false);
        assert_eq!(reveal, Reveal::Laying, "remeasure a second batch");

        let reading = Reveal::Reading { selected: true };
        reveal = reading;
        reveal.cancel_pending();
        reveal.body_changed(true);
        assert_eq!(reveal, reading, "input persists across body batches");
    }

    #[test]
    fn repeated_body_growth_holds_a_partly_visible_message() {
        let old_view = vertical_bounds(50., 300.);
        let message = vertical_bounds(200., 180.);
        let y = visible_top(old_view, px(-170.), message).unwrap();
        assert_eq!(y, px(-20.));

        // Two batches grow earlier messages. The second frame also moves the
        // viewport down as the subject wraps.
        let view = vertical_bounds(75., 275.);
        let first_batch = vertical_bounds(340., 180.);
        let second_batch = vertical_bounds(575., 180.);
        let offset = offset_to_hold(old_view, first_batch, y);
        assert_eq!(offset, px(-310.));
        let next_y = visible_top(old_view, offset, first_batch).unwrap();
        let offset = offset_to_hold(view, second_batch, next_y);
        assert_eq!(offset, px(-520.));
        assert_eq!(visible_top(view, offset, second_batch), Some(y));
    }

    #[test]
    fn only_messages_with_area_inside_the_view_can_be_anchors() {
        let view = vertical_bounds(50., 300.);
        let offset = px(-80.);
        assert_eq!(visible_top(view, offset, vertical_bounds(50., 80.)), None);
        assert_eq!(
            visible_top(view, offset, vertical_bounds(130., 80.)),
            Some(px(0.))
        );
        assert_eq!(visible_top(view, offset, vertical_bounds(430., 40.)), None);
    }

    #[test]
    fn a_message_at_the_scroll_top_stays_there() {
        let view = vertical_bounds(50., 300.);
        let first = vertical_bounds(50., 200.);
        assert_eq!(offset_to_hold(view, first, px(0.)), px(0.));
        assert_eq!(offset_to_hold(view, first, px(20.)), px(0.));
    }
}
