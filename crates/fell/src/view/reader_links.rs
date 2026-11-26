use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{App, Context, ElementId, FocusHandle, SharedString, Window};

use super::{Inbox, Overlay, Pane, Rich};
use crate::motion::{self, Motion, Transition};

pub(crate) const OPEN_FAILED: &str = "Couldn't open link";

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct LinkPreview {
    pub destination: SharedString,
    pub target: SharedString,
    pub mismatch: bool,
}

pub(crate) struct Placement {
    pub line: Cell<(f32, f32)>,
    pub visible: Cell<bool>,
    pub target: Arc<str>,
    pub preview: Option<LinkPreview>,
}

#[derive(Default)]
pub(super) struct ReaderLinks {
    placements: RefCell<HashMap<ElementId, Weak<Placement>>>,
    hovered: Option<FocusHandle>,
    pub(super) pending: Option<(String, bool)>,
    current: Option<LinkPreview>,
    outgoing: Option<(LinkPreview, f32, Transition)>,
    incoming: Option<Transition>,
}

pub(super) type PreviewFrame = Option<(LinkPreview, f32)>;

impl ReaderLinks {
    fn placement(&self, handle: &FocusHandle) -> Option<Rc<Placement>> {
        self.placements.borrow().get(&handle.into())?.upgrade()
    }

    pub(super) fn clear_hover(&mut self) {
        self.hovered = None;
    }

    fn preview_at(&self, now: Instant) -> PreviewFrame {
        if let Some((preview, opacity, fade)) = &self.outgoing
            && fade.running(now)
        {
            return Some((preview.clone(), opacity * fade.remaining(now)));
        }
        self.current
            .clone()
            .map(|preview| (preview, motion::progress(self.incoming.as_ref(), now)))
    }

    fn animate_preview(
        &mut self,
        next: Option<LinkPreview>,
        motion: Motion,
        now: Instant,
    ) -> (PreviewFrame, bool) {
        const SWAP_OUT: Duration = Duration::from_millis(60);
        const SWAP_IN: Duration = Duration::from_millis(100);
        if next != self.current {
            let visible = self.preview_at(now).filter(|(_, alpha)| *alpha > 0.0);
            let replacing = next.is_some() && visible.is_some();
            let outgoing = if replacing { SWAP_OUT } else { motion::FADE };
            self.outgoing = visible
                .and_then(|(preview, alpha)| Some((preview, alpha, motion.start(outgoing, now)?)));
            self.incoming = next.as_ref().and_then(|_| {
                if replacing {
                    motion.start(SWAP_IN, now + SWAP_OUT)
                } else {
                    motion.start(motion::FADE, now)
                }
            });
            self.current = next;
        }
        if self
            .outgoing
            .as_ref()
            .is_some_and(|(_, _, fade)| !fade.running(now))
        {
            self.outgoing = None;
        }
        if self.incoming.is_some_and(|fade| !fade.running(now)) {
            self.incoming = None;
        }
        (
            self.preview_at(now),
            self.outgoing.is_some() || self.incoming.is_some(),
        )
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
        self.animate_preview(next, motion, now)
    }
}

pub(crate) fn register(window: &Window, cx: &App, handle: &FocusHandle, place: &Rc<Placement>) {
    if let Some(inbox) = super::accounts::active_inbox(window, cx) {
        inbox
            .read(cx)
            .reader_links
            .placements
            .borrow_mut()
            .insert(handle.into(), Rc::downgrade(place));
    }
}

pub(crate) fn hover(window: &mut Window, cx: &mut App, handle: &FocusHandle, hovered: bool) {
    if let Some(inbox) = super::accounts::active_inbox(window, cx) {
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
    if let Some(inbox) = super::accounts::active_inbox(window, cx) {
        inbox.update(cx, |inbox, cx| {
            if inbox.session.pane == Pane::List {
                inbox.focus_message(inbox.session.message_cursor);
                cx.notify();
            }
        });
    }
}

pub(crate) fn notice(window: &mut Window, cx: &mut App, text: &'static str, alert: bool) {
    if let Some(inbox) = super::accounts::active_inbox(window, cx) {
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
        preferred: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.reader_links.clear_hover();
        if backward {
            window.focus_prev();
        } else {
            window.focus_next();
        }
        let focused_link = |window: &Window| {
            let focus = window.focused(cx)?;
            let place = self.reader_links.placement(&focus)?;
            Some((focus, place))
        };
        let Some((mut focus, mut place)) = focused_link(window) else {
            if preferred.is_some() {
                window.focus(&self.focus);
            }
            return false;
        };
        if preferred.is_some() || (visible_first && !place.visible.get()) {
            let first = focus.clone();
            let mut next = (focus.clone(), place.clone());
            loop {
                let matches = preferred.map_or_else(
                    || next.1.visible.get(),
                    |target| next.1.target.as_ref() == target,
                );
                if matches {
                    (focus, place) = next;
                    break;
                }
                window.focus_next();
                let Some(candidate) = focused_link(window).filter(|(next, _)| *next != first)
                else {
                    if preferred.is_some() {
                        window.focus(&self.focus);
                        return false;
                    }
                    break;
                };
                next = candidate;
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

    fn link_view_current(&self) -> bool {
        self.session.pane == Pane::Conversation
            && self.session.overlay == Overlay::None
            && self.open_thread.is_some()
            && self.open_thread == self.session.cursor_id
            && self.search_conversation_live()
    }

    fn link_bodies_settled(&self) -> bool {
        self.body_refresh.is_empty()
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
        (Some(thread) == self.session.cursor_id.as_ref() && self.link_view_current())
            .then_some(*backward)
    }

    pub(super) fn focus_message_links(
        &mut self,
        backward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.link_view_current() {
            self.queue_link_focus(backward);
            cx.notify();
            return;
        }
        self.reader_links.pending = None;
        if !self.focus_or_notice(backward, window, cx) {
            self.queue_link_focus(backward);
            cx.notify();
        }
    }

    fn focus_or_notice(
        &mut self,
        backward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let first = self.focus.is_focused(window) && !backward;
        if self.focus_link(backward, first, None, window, cx) {
            return true;
        }
        if !self.link_bodies_settled() {
            return false;
        }
        self.session.show_toast("No links", false);
        self.expire_action_notice(cx);
        cx.notify();
        true
    }

    pub(super) fn update_link_focus(
        &mut self,
        body_replaced: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .reader_links
            .pending
            .as_ref()
            .is_some_and(|(thread, _)| Some(thread) != self.session.cursor_id.as_ref())
        {
            self.reader_links.pending = None;
        }
        // Retain the old handle until the new dispatch tree is available.
        let previous = if body_replaced && self.link_view_current() {
            window.focused(cx).and_then(|handle| {
                let target = self.reader_links.placement(&handle)?.target.clone();
                Some((handle, target))
            })
        } else {
            None
        };
        if previous.is_none() && self.pending_link_direction().is_none() {
            return;
        }
        let inbox = cx.weak_entity();
        let thread = self.session.cursor_id.clone();
        // The draw registers the new stops. Finish before the next input can
        // be dispatched to a link whose body just left the tree.
        window.defer(cx, move |window, cx| {
            let _ = inbox.update(cx, |inbox, cx| {
                if inbox.session.cursor_id != thread || !inbox.link_view_current() {
                    return;
                }
                if let Some((handle, target)) = previous
                    // The inbox focus-lost listener can already have reset
                    // focus to the reader root during this draw.
                    && (handle.is_focused(window) || inbox.focus.is_focused(window))
                    && !inbox.focus.contains(&handle, window)
                {
                    inbox.reader_links.pending = None;
                    window.focus(&inbox.focus);
                    inbox.focus_link(false, false, Some(&target), window, cx);
                } else if let Some(backward) = inbox.pending_link_direction()
                    && inbox.focus_or_notice(backward, window, cx)
                {
                    inbox.reader_links.pending = None;
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handoff_to_formatted_body(html: &str) -> (gpui::TestAppContext, gpui::WindowHandle<Inbox>) {
        let (mut app, inbox) = super::super::tests::pending_inbox();
        let window_handle: gpui::AnyWindowHandle = inbox.into();
        let draw = |app: &mut gpui::TestAppContext| {
            window_handle
                .update(app, |_, window, cx| window.draw(cx).clear())
                .unwrap();
        };
        inbox
            .update(&mut app, |inbox, window, cx| {
                cx.on_focus_lost(window, |inbox, window, _| {
                    window.focus(&inbox.focus);
                })
                .detach();
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
                assert!(!inbox.focus.is_focused(window));
                assert!(inbox.reader_links.pending.is_none());
                let focused = window.focused(cx).unwrap();
                assert_eq!(
                    inbox
                        .reader_links
                        .placement(&focused)
                        .unwrap()
                        .target
                        .as_ref(),
                    "https://example.test/stored"
                );
                let message = inbox.session.messages.last().unwrap();
                inbox.body_refresh.rich.insert(
                    message.id.clone(),
                    Rich::Html(Arc::new(crate::mail_body::prepare_html(
                        html,
                        &message.mail_headers,
                    ))),
                );
                cx.notify();
            })
            .unwrap();
        draw(&mut app);
        (app, inbox)
    }

    #[test]
    fn list_tab_focuses_the_stored_link_and_hands_off_to_the_matching_formatted_link() {
        let (mut app, inbox) = handoff_to_formatted_body(
            r#"<p><a href="https://example.test/other">Another link</a></p>
            <p><a href="https://example.test/stored">Formatted release notes</a></p>"#,
        );
        // Dispatch against the newly drawn tree without waiting for another frame.
        app.simulate_keystrokes(inbox.into(), "y");
        assert_eq!(
            app.read_from_clipboard()
                .and_then(|clipboard| clipboard.text())
                .as_deref(),
            Some("https://example.test/stored"),
        );
    }

    #[test]
    fn a_missing_formatted_link_returns_focus_to_the_reader() {
        let (mut app, inbox) = handoff_to_formatted_body(
            r#"<p><a href="https://example.test/other">Another link</a></p>"#,
        );
        inbox
            .update(&mut app, |inbox, window, _| {
                assert!(inbox.focus.is_focused(window));
                assert!(inbox.reader_links.pending.is_none());
                assert!(inbox.session.status_notice().is_none());
            })
            .unwrap();
        app.simulate_keystrokes(inbox.into(), "y");
        assert!(app.read_from_clipboard().is_none());
    }

    #[test]
    fn regrouping_keeps_focus_on_the_retained_message_and_skips_departing_links() {
        const DEPARTING: &str = "https://example.test/departing";
        const RETAINED: &str = "https://example.test/retained";
        let (mut app, inbox) = super::super::tests::pending_inbox();
        let window_handle: gpui::AnyWindowHandle = inbox.into();
        let draw = |app: &mut gpui::TestAppContext| {
            window_handle
                .update(app, |_, window, cx| window.draw(cx).clear())
                .unwrap();
        };
        inbox
            .update(&mut app, |inbox, window, cx| {
                let mut retained = inbox.session.messages.last().unwrap().clone();
                retained.expanded = true;
                retained.body_state = fell_core::BodyState::Stored;
                retained.reader_parts = fell_core::ReaderParts::Ready;
                retained.rich = Rich::Plain;
                let mut departing = retained.clone();
                departing.id = "departing-message@example.test".into();
                departing.body = crate::text_render::render_stored_body(Some(DEPARTING), "", "");
                retained.body = crate::text_render::render_stored_body(Some(RETAINED), "", "");
                inbox.session.set_messages(vec![departing, retained]);
                window.focus(&inbox.focus);
                inbox.session.on_key(crate::keymap::KeyId::Tab, 10);
                cx.notify();
            })
            .unwrap();
        draw(&mut app);

        let retained_focus = inbox
            .update(&mut app, |inbox, window, cx| {
                assert!(inbox.focus_link(false, false, Some(DEPARTING), window, cx));
                assert!(inbox.focus_link(false, false, Some(RETAINED), window, cx));
                let focused = window.focused(cx).unwrap();
                let retained = inbox.session.messages[1].clone();
                let thread = inbox.session.cursor_id.clone().unwrap();
                inbox.regroup_conversation(vec![retained], &thread, 1);
                let now = inbox.motion.clock.now();
                assert!(
                    inbox
                        .regroup_motion
                        .as_ref()
                        .unwrap()
                        .frame(now)
                        .departing
                        .contains_key(&0)
                );
                cx.notify();
                focused
            })
            .unwrap();
        draw(&mut app);

        inbox
            .update(&mut app, |inbox, window, cx| {
                assert!(retained_focus.is_focused(window));
                assert!(inbox.focus.contains(&retained_focus, window));
                assert!(!inbox.focus_link(false, false, Some(DEPARTING), window, cx));
            })
            .unwrap();
    }

    #[test]
    fn link_previews_take_turns_in_the_status_slot() {
        let preview = |domain: &str| LinkPreview {
            destination: domain.to_owned().into(),
            target: format!("https://{domain}").into(),
            mismatch: false,
        };
        let mut state = ReaderLinks::default();
        let old = preview("hover.example.test");
        let new = preview("focus.example.test");
        let motion = Motion::new(true);
        let start = Instant::now();
        state.animate_preview(Some(old.clone()), motion, start);
        let swap = start + motion::FADE;
        assert!(state.animate_preview(Some(new.clone()), motion, swap).1);
        let frame =
            |state: &ReaderLinks, ms| state.preview_at(swap + Duration::from_millis(ms)).unwrap();
        assert!(frame(&state, 0) == (old.clone(), 1.0));
        assert!(frame(&state, 30) == (old, 0.25));
        assert!(frame(&state, 60) == (new.clone(), 0.0));
        let (entering, alpha) = frame(&state, 90);
        assert!(entering == new && (0.0..1.0).contains(&alpha));
        assert!(frame(&state, 160) == (new, 1.0));
    }
}
