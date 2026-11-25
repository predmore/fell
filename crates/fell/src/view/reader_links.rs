use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
    time::Instant,
};

use gpui::{App, Context, ElementId, FocusHandle, SharedString, Window};

use super::{Inbox, Overlay, Pane};
use crate::motion::{self, Motion, Transition};

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct LinkPreview {
    pub target: SharedString,
    pub mismatch: bool,
}

pub(crate) struct Placement {
    pub line: Cell<(f32, f32)>,
    pub visible: Cell<bool>,
    pub preview: Option<LinkPreview>,
}

#[derive(Default)]
pub(super) struct ReaderLinks {
    placements: RefCell<HashMap<ElementId, Weak<Placement>>>,
    hovered: Option<FocusHandle>,
    pub(super) pending: Option<String>,
    current: Option<LinkPreview>,
    outgoing: Option<LinkPreview>,
    fade: Option<Transition>,
}

pub(super) type PreviewFrame = [(Option<LinkPreview>, f32); 2];

impl ReaderLinks {
    fn placement(&self, handle: &FocusHandle) -> Option<Rc<Placement>> {
        self.placements.borrow().get(&handle.into())?.upgrade()
    }

    pub(super) fn clear_hover(&mut self) {
        self.hovered = None;
    }

    pub(super) fn frame(
        &mut self,
        root: &FocusHandle,
        show: bool,
        motion: Motion,
        now: Instant,
        window: &Window,
        cx: &App,
    ) -> (PreviewFrame, bool) {
        self.placements
            .borrow_mut()
            .retain(|_, place| place.strong_count() > 0);
        let hovered = self.hovered.as_ref().filter(|h| {
            root.contains(h, window) && self.placement(h).is_some_and(|place| place.visible.get())
        });
        let next = if show {
            hovered
                .cloned()
                .or_else(|| window.focused(cx))
                .filter(|handle| root.contains(handle, window))
                .and_then(|handle| self.placement(&handle)?.preview.clone())
        } else {
            None
        };
        if next != self.current {
            self.outgoing = std::mem::replace(&mut self.current, next);
            self.fade = motion.start(motion::FADE, now);
        }
        if self.fade.is_some_and(|fade| !fade.running(now)) {
            self.fade = None;
            self.outgoing = None;
        }
        let progress = motion::progress(self.fade.as_ref(), now);
        (
            [
                (self.current.clone(), progress),
                (self.outgoing.clone(), 1.0 - progress),
            ],
            self.fade.is_some(),
        )
    }
}

pub(crate) fn register(window: &Window, cx: &App, handle: &FocusHandle, place: &Rc<Placement>) {
    if let Some(inbox) = window.root::<Inbox>().flatten() {
        inbox
            .read(cx)
            .reader_links
            .placements
            .borrow_mut()
            .insert(handle.into(), Rc::downgrade(place));
    }
}

pub(crate) fn hover(window: &mut Window, cx: &mut App, handle: &FocusHandle, hovered: bool) {
    if let Some(inbox) = window.root::<Inbox>().flatten() {
        inbox.update(cx, |inbox, cx| {
            if hovered && inbox.reader_links.hovered.as_ref() != Some(handle) {
                inbox.reader_links.hovered = Some(handle.clone());
                cx.notify();
            } else if !hovered && inbox.reader_links.hovered.as_ref() == Some(handle) {
                inbox.reader_links.clear_hover();
                cx.notify();
            }
        });
    }
}

pub(crate) fn clicked(window: &mut Window, cx: &mut App) {
    if let Some(inbox) = window.root::<Inbox>().flatten() {
        inbox.update(cx, |inbox, cx| {
            if inbox.session.pane == Pane::List {
                inbox.focus_message(inbox.session.message_cursor);
                cx.notify();
            }
        });
    }
}

pub(crate) fn notice(window: &mut Window, cx: &mut App, text: &'static str, alert: bool) {
    if let Some(inbox) = window.root::<Inbox>().flatten() {
        inbox.update(cx, |inbox, cx| {
            inbox.session.show_toast(text, alert);
            inbox.expire_action_notice(cx);
            cx.notify();
        });
    }
}

impl Inbox {
    pub(super) fn focus_link(
        &mut self,
        backward: bool,
        visible_first: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.reader_links.clear_hover();
        if backward {
            window.focus_prev();
        } else {
            window.focus_next();
        }
        let Some(mut focus) = window.focused(cx) else {
            return false;
        };
        let Some(mut place) = self.reader_links.placement(&focus) else {
            return false;
        };
        if visible_first && !place.visible.get() {
            let first = focus.clone();
            let first_place = place.clone();
            loop {
                window.focus_next();
                let Some(next) = window.focused(cx) else {
                    break;
                };
                let Some(next_place) = self.reader_links.placement(&next) else {
                    break;
                };
                if next == first {
                    break;
                }
                if next_place.visible.get() {
                    focus = next;
                    place = next_place;
                    break;
                }
            }
            if !place.visible.get() {
                focus = first;
                place = first_place;
            }
            window.focus(&focus);
        }
        self.reader_links.pending = None;
        let (top, bottom) = place.line.get();
        let view = self.conv_scroll.bounds();
        let offset = f32::from(self.conv_scroll.offset().y);
        let max = f32::from(self.conv_scroll.max_offset().height);
        let delta = if top < f32::from(view.top()) {
            f32::from(view.top()) - top
        } else if bottom > f32::from(view.bottom()) {
            f32::from(view.bottom()) - bottom
        } else {
            0.0
        };
        if delta.abs() > 0.5 {
            self.reader_took_over(false);
            self.glide_conversation(offset, (offset + delta).clamp(-max, 0.0));
        }
        cx.notify();
        true
    }

    pub(super) fn focus_pending_link(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.reader_links.pending.is_none()
            || self.reader_links.pending != self.session.cursor_id
        {
            self.reader_links.pending = None;
            return;
        }
        if self.session.pane != Pane::Conversation
            || self.session.overlay != Overlay::None
            || self.open_thread != self.session.cursor_id
            || !self.search_conversation_live()
        {
            return;
        }
        let inbox = cx.weak_entity();
        window.on_next_frame(move |window, cx| {
            let _ = inbox.update(cx, |inbox, cx| {
                if inbox.reader_links.pending.is_some()
                    && inbox.reader_links.pending == inbox.open_thread
                    && inbox.session.pane == Pane::Conversation
                    && inbox.session.overlay == Overlay::None
                {
                    inbox.focus_link(false, true, window, cx);
                }
            });
        });
    }
}
