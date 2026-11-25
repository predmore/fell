use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
    time::Instant,
};

use gpui::{App, Context, ElementId, FocusHandle, SharedString, Window};

use super::{Inbox, Overlay, Pane, Rich};
use crate::motion::{self, Motion, Transition};

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct LinkPreview {
    pub destination: SharedString,
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
    pub(super) pending: Option<(String, bool)>,
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
    pub(super) fn queue_link_focus(&mut self, backward: bool) {
        self.reader_links.pending = self.session.cursor_id.clone().map(|id| (id, backward));
    }

    fn focus_link(
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
        let (top, bottom) = place.line.get();
        let view = self.conv_scroll.bounds();
        let offset = f32::from(self.conv_scroll.offset().y);
        let max = f32::from(self.conv_scroll.max_offset().height);
        let margin = 48.0_f32.min((f32::from(view.size.height) - (bottom - top)).max(0.0) / 2.0);
        let safe_top = f32::from(view.top()) + margin;
        let safe_bottom = f32::from(view.bottom()) - margin;
        let delta = if top < safe_top {
            safe_top - top
        } else if bottom > safe_bottom {
            safe_bottom - bottom
        } else {
            0.0
        };
        let target = (offset + delta).clamp(-max, 0.0);
        if (target - offset).abs() > 0.5 {
            self.reader_took_over(false);
            self.glide_conversation(offset, target);
        }
        cx.notify();
        true
    }

    fn link_view_ready(&self) -> bool {
        self.session.pane == Pane::Conversation
            && self.session.overlay == Overlay::None
            && self.open_thread.is_some()
            && self.open_thread == self.session.cursor_id
            && self.search_conversation_live()
            && self.body_refresh.is_empty()
            && self
                .session
                .messages
                .iter()
                .filter(|message| message.expanded)
                .all(|message| match message.rich {
                    Rich::Html(_) | Rich::PlainPart(_) => true,
                    Rich::Pending => false,
                    Rich::Plain | Rich::Missing => {
                        !self.body_loads.awaiting(&message.id)
                            && !self.reader_bodies.active.as_ref().is_some_and(|request| {
                                request.ids.iter().any(|id| id.as_str() == message.id)
                            })
                    }
                })
    }

    fn pending_link_direction(&self) -> Option<bool> {
        let (thread, backward) = self.reader_links.pending.as_ref()?;
        (Some(thread) == self.session.cursor_id.as_ref() && self.link_view_ready())
            .then_some(*backward)
    }

    pub(super) fn focus_message_links(
        &mut self,
        backward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.focus.is_focused(window) && !self.link_view_ready() {
            self.queue_link_focus(backward);
            cx.notify();
            return;
        }
        self.reader_links.pending = None;
        self.focus_or_notice(backward, window, cx);
    }

    fn focus_or_notice(&mut self, backward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let first = self.focus.is_focused(window) && !backward;
        if !self.focus_link(backward, first, window, cx) {
            self.session.show_toast("No links", false);
            self.expire_action_notice(cx);
            cx.notify();
        }
    }

    pub(super) fn focus_pending_link(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self
            .reader_links
            .pending
            .as_ref()
            .is_some_and(|(thread, _)| Some(thread) != self.session.cursor_id.as_ref())
        {
            self.reader_links.pending = None;
        }
        if self.pending_link_direction().is_none() {
            return;
        }
        // The incoming body registers its stops when this frame is drawn.
        window.request_animation_frame();
        let inbox = cx.weak_entity();
        window.on_next_frame(move |window, cx| {
            let _ = inbox.update(cx, |inbox, cx| {
                inbox.apply_pending_link_focus(window, cx);
            });
        });
    }

    fn apply_pending_link_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(backward) = self.pending_link_direction() {
            self.reader_links.pending = None;
            self.focus_or_notice(backward, window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn list_tab_waits_for_formatted_links_and_activates_the_settled_target() {
        let (mut app, inbox) = super::super::tests::pending_inbox();
        let window_handle: gpui::AnyWindowHandle = inbox.into();
        let draw = |app: &mut gpui::TestAppContext| {
            window_handle
                .update(app, |_, window, cx| window.draw(cx).clear())
                .unwrap();
        };
        inbox
            .update(&mut app, |inbox, window, cx| {
                let message = inbox.session.messages.last_mut().unwrap();
                message.expanded = true;
                message.body_state = fell_core::BodyState::Stored;
                message.reader_parts = fell_core::ReaderParts::Ready;
                message.rich = Rich::Pending;
                message.body = crate::text_render::render_stored_body(
                    Some("https://example.test/stored"),
                    "",
                    "Release notes",
                );
                inbox
                    .reader_bodies
                    .fallback_since
                    .insert(message.id.clone(), Instant::now());
                window.focus(&inbox.focus);
                let step = inbox.session.on_key(crate::keymap::KeyId::Tab, 10);
                assert_eq!(step.action, Some(crate::keymap::Action::FocusLinks));
                inbox.queue_link_focus(false);
                cx.notify();
            })
            .unwrap();
        draw(&mut app);
        inbox
            .update(&mut app, |inbox, window, cx| {
                inbox.apply_pending_link_focus(window, cx);
                assert!(inbox.focus.is_focused(window));
                assert!(inbox.reader_links.pending.is_some());
                let message = inbox.session.messages.last().unwrap();
                inbox.body_refresh.rich.insert(
                    message.id.clone(),
                    Rich::Html(Arc::new(crate::mail_body::prepare_html(
                        r#"<p><a href="https://example.test/formatted">Formatted release notes</a></p>"#,
                        &message.mail_headers,
                    ))),
                );
                cx.notify();
            })
            .unwrap();
        draw(&mut app);
        inbox
            .update(&mut app, |inbox, window, cx| {
                inbox.apply_pending_link_focus(window, cx);
                assert!(!inbox.focus.is_focused(window));
                assert!(inbox.reader_links.pending.is_none());
            })
            .unwrap();
        draw(&mut app);
        app.simulate_keystrokes(window_handle, "y");
        assert_eq!(
            app.read_from_clipboard()
                .and_then(|clipboard| clipboard.text())
                .as_deref(),
            Some("https://example.test/formatted"),
        );
    }
}
