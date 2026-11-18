//! The inbox window's running transitions, built on [`crate::motion`].
//!
//! Most of them start from what changed between two frames, not from the
//! code that changed it: [`InboxMotion::observe`] compares the session with
//! what the last frame drew, so every path that opens the palette, shows a
//! notice or switches panes gets the same transition. The list and the
//! scroll glides start where the view changes them, because they need the
//! positions from before the change.

use std::time::Instant;

use crate::model::{Overlay, Session, Toast};
use crate::motion::{self, Ghost, Glide, ListShift, Motion, Transition};
use crate::present::{Pane, ThreadRow};

/// A notice as the status bar draws it this frame.
pub struct NoticeView {
    pub toast: Toast,
    pub opacity: f32,
    /// Cleared by a key and fading out: the bar lays out as without a notice.
    pub leaving: bool,
}

/// A pane that just came into view, sliding in from its side.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaneEntry {
    pub opacity: f32,
    /// Offset from its place, in pixels (negative: from the left).
    pub dx: f32,
}

pub struct InboxMotion {
    pub motion: Motion,
    /// Rows moving after an archive, undo, sync or folder switch.
    pub list: Option<ListShift<ThreadRow>>,
    /// The list viewport gliding to the selection (`Ctrl-d`, `gg`, `G`).
    pub list_glide: Option<Glide>,
    /// The conversation gliding to a page or a message.
    pub conv_glide: Option<Glide>,
    toast_in: Option<Transition>,
    toast_out: Option<(Toast, Transition)>,
    last_toast: Option<Toast>,
    which: Option<Transition>,
    last_which: bool,
    overlay: Option<Transition>,
    last_overlay: Overlay,
    /// The pane that came into view and the side it comes from.
    pane: Option<(Transition, Pane)>,
    last_panes: (bool, bool),
    last_layout: (Pane, bool),
    /// The conversation's focus edge changing colour.
    focus: Option<Transition>,
    /// The row the selection just left, its highlight fading out.
    selection_out: Option<(String, Transition)>,
    last_selected: Option<String>,
    /// Another thread's messages just arrived in the conversation.
    arrival: Option<Transition>,
    /// Where a glide a key interrupted was drawn, so the next glide starts
    /// there instead of snapping to the old target first.
    list_from: Option<f32>,
    conv_from: Option<f32>,
}

/// Scroll offsets a settle jumped to, for the view to apply.
#[derive(Default)]
pub struct Settled {
    pub list: Option<f32>,
    pub conversation: Option<f32>,
}

impl InboxMotion {
    pub fn new(motion: Motion) -> Self {
        Self {
            motion,
            list: None,
            list_glide: None,
            conv_glide: None,
            toast_in: None,
            toast_out: None,
            last_toast: None,
            which: None,
            last_which: false,
            overlay: None,
            last_overlay: Overlay::None,
            pane: None,
            last_panes: (true, true),
            last_layout: (Pane::List, false),
            focus: None,
            selection_out: None,
            last_selected: None,
            arrival: None,
            list_from: None,
            conv_from: None,
        }
    }

    /// Start the transitions for what changed since the last frame.
    /// `panes`: which panes this frame shows (list, conversation).
    pub fn observe(&mut self, session: &Session, panes: (bool, bool), now: Instant) {
        let start = |duration| self.motion.start(duration, now);
        if session.toast != self.last_toast {
            match (&session.toast, self.last_toast.take()) {
                (Some(toast), _) => {
                    // The same notice again (a second `e` in a burst): it
                    // stays up instead of blinking out and back.
                    let returning = self
                        .toast_out
                        .as_ref()
                        .is_some_and(|(leaving, _)| leaving == toast);
                    self.toast_in = if returning { None } else { start(motion::FADE) };
                    self.toast_out = None;
                }
                (None, Some(old)) => {
                    self.toast_in = None;
                    self.toast_out = start(motion::FADE_OUT).map(|fade| (old, fade));
                }
                (None, None) => {}
            }
            self.last_toast = session.toast.clone();
        }

        let which =
            session.which_visible && session.overlay == Overlay::None && !session.chord.is_empty();
        if which && !self.last_which {
            self.which = start(motion::FADE);
        }
        self.last_which = which;

        if session.overlay != self.last_overlay {
            self.overlay = if session.overlay == Overlay::None {
                None
            } else {
                start(motion::FADE)
            };
            self.last_overlay = session.overlay.clone();
        }

        // Only a key moves the panes; a window resized across the narrow
        // threshold shows its panes at once.
        let layout = (session.pane, session.full_width);
        if layout != self.last_layout {
            if panes != self.last_panes {
                let entering = match (self.last_panes, panes) {
                    ((_, false), (_, true)) | ((true, true), (false, true)) => {
                        Some(Pane::Conversation)
                    }
                    ((false, _), (true, _)) => Some(Pane::List),
                    _ => None,
                };
                self.pane = entering.and_then(|pane| start(motion::PANE).map(|t| (t, pane)));
            }
            if layout.0 != self.last_layout.0 {
                self.focus = start(motion::QUICK);
            }
            self.last_layout = layout;
        }
        self.last_panes = panes;

        if session.cursor_id != self.last_selected {
            // The new row is highlighted at once; only the old one fades.
            self.selection_out = self
                .last_selected
                .take()
                .and_then(|old| start(motion::QUICK).map(|fade| (old, fade)));
            self.last_selected = session.cursor_id.clone();
        }
    }

    /// Another thread's messages replaced the conversation.
    pub fn thread_arrived(&mut self, now: Instant) {
        self.arrival = self.motion.start(motion::QUICK, now);
    }

    /// Jump every transition to its end state: a key acts on what the
    /// reader will see, never on a frame in between. Glides return their
    /// targets for the view to apply; where they were drawn is kept for a
    /// glide the same key starts ([`Self::glide_from_list`]). Rows still
    /// sliding keep sliding: the next list change starts from where they
    /// are drawn, and the list the key acts on is already final.
    pub fn settle(&mut self, now: Instant) -> Settled {
        let list = self.list_glide.take();
        let conversation = self.conv_glide.take();
        self.list_from = list.map(|glide| glide.value(now));
        self.conv_from = conversation.map(|glide| glide.value(now));
        self.toast_in = None;
        self.toast_out = None;
        self.which = None;
        self.overlay = None;
        self.pane = None;
        self.focus = None;
        self.selection_out = None;
        self.arrival = None;
        Settled {
            list: list.map(|glide| glide.to),
            conversation: conversation.map(|glide| glide.to),
        }
    }

    /// Where a list glide should start: where the one a key interrupted
    /// was drawn, else `offset`.
    pub fn glide_from_list(&mut self, offset: f32) -> f32 {
        self.list_from.take().unwrap_or(offset)
    }

    pub fn glide_from_conversation(&mut self, offset: f32) -> f32 {
        self.conv_from.take().unwrap_or(offset)
    }

    /// The key is handled: a glide it did not restart stays at its target.
    pub fn end_key(&mut self) {
        self.list_from = None;
        self.conv_from = None;
    }

    /// The wheel took over: stop gliding where the viewport is.
    pub fn cancel_glides(&mut self) {
        self.list_glide = None;
        self.conv_glide = None;
    }

    /// Drop what finished. True while anything still needs frames.
    pub fn tick(&mut self, now: Instant) -> bool {
        fn keep(transition: &mut Option<Transition>, now: Instant) -> bool {
            if transition.is_some_and(|t| !t.running(now)) {
                *transition = None;
            }
            transition.is_some()
        }
        let mut running = false;
        running |= keep(&mut self.toast_in, now);
        if self
            .toast_out
            .as_ref()
            .is_some_and(|(_, fade)| !fade.running(now))
        {
            self.toast_out = None;
        }
        running |= self.toast_out.is_some();
        running |= keep(&mut self.which, now);
        running |= keep(&mut self.overlay, now);
        running |= keep(&mut self.focus, now);
        running |= keep(&mut self.arrival, now);
        if self
            .selection_out
            .as_ref()
            .is_some_and(|(_, fade)| !fade.running(now))
        {
            self.selection_out = None;
        }
        running |= self.selection_out.is_some();
        if self.pane.is_some_and(|(t, _)| !t.running(now)) {
            self.pane = None;
        }
        running |= self.pane.is_some();
        if self
            .list
            .as_ref()
            .is_some_and(|shift| !shift.running(now) || shift.is_still())
        {
            self.list = None;
        }
        running |= self.list.is_some();
        // Glides end in the view, which sets their last offset.
        running |= self.list_glide.is_some() || self.conv_glide.is_some();
        running
    }

    /// Hand the ghosts of a shift still running to the next one.
    pub fn carried_ghosts(&mut self) -> Vec<Ghost<ThreadRow>> {
        self.list
            .as_mut()
            .map(ListShift::take_ghosts)
            .unwrap_or_default()
    }

    /// Where row `id` is drawn relative to its slot right now.
    pub fn row_offset(&self, id: &str, now: Instant) -> f32 {
        self.list
            .as_ref()
            .map_or(0.0, |shift| shift.offset_of(id, now))
    }

    pub fn notice(&self, session: &Session, now: Instant) -> Option<NoticeView> {
        if let Some(toast) = &session.toast {
            return Some(NoticeView {
                toast: toast.clone(),
                opacity: motion::progress(self.toast_in.as_ref(), now),
                leaving: false,
            });
        }
        self.toast_out.as_ref().map(|(toast, fade)| NoticeView {
            toast: toast.clone(),
            opacity: 1.0 - fade.progress(now),
            leaving: true,
        })
    }

    /// Opacity and rise (pixels still to travel up) of the which-key popup.
    pub fn which(&self, now: Instant) -> (f32, f32) {
        let p = motion::progress(self.which.as_ref(), now);
        (p, motion::RISE_PX * (1.0 - p))
    }

    /// Opacity, drop (pixels still to travel) and width scale of an overlay.
    pub fn overlay(&self, now: Instant) -> (f32, f32, f32) {
        let p = motion::progress(self.overlay.as_ref(), now);
        (
            p,
            motion::RISE_PX * (1.0 - p),
            motion::lerp(motion::OPEN_SCALE, 1.0, p),
        )
    }

    pub fn pane_entry(&self, pane: Pane, now: Instant) -> PaneEntry {
        match self.pane {
            Some((transition, entering)) if entering == pane => {
                let p = transition.progress(now);
                let side = if pane == Pane::List { -1.0 } else { 1.0 };
                PaneEntry {
                    opacity: p,
                    dx: side * motion::PANE_SLIDE_PX * (1.0 - p),
                }
            }
            _ => PaneEntry {
                opacity: 1.0,
                dx: 0.0,
            },
        }
    }

    /// How much of the selection highlight row `id` still shows, when the
    /// selection just left it.
    pub fn selection_left(&self, id: &str, now: Instant) -> Option<f32> {
        self.selection_out
            .as_ref()
            .filter(|(old, _)| old == id)
            .map(|(_, fade)| 1.0 - fade.progress(now))
    }

    /// Opacity of the conversation's messages: another thread's arrive
    /// from [`motion::ARRIVE_FROM`], so the pane never blinks to empty.
    pub fn arrival(&self, now: Instant) -> f32 {
        motion::lerp(
            motion::ARRIVE_FROM,
            1.0,
            motion::progress(self.arrival.as_ref(), now),
        )
    }

    /// How far the conversation's focus edge is towards "focused".
    pub fn focus_edge(&self, pane: Pane, now: Instant) -> f32 {
        let p = motion::progress(self.focus.as_ref(), now);
        if pane == Pane::Conversation {
            p
        } else {
            1.0 - p
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Keymap;
    use crate::model::SyncKind;
    use std::time::Duration;

    fn session() -> Session {
        Session::new("A", "a@x", SyncKind::LocalOffline, Keymap::default())
    }

    fn later(now: Instant, ms: u64) -> Instant {
        now + Duration::from_millis(ms)
    }

    #[test]
    fn a_notice_fades_in_then_out_after_a_key() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        motion.observe(&session, (true, true), now);
        session.show_toast("Archived · u", false);
        motion.observe(&session, (true, true), now);
        assert_eq!(motion.notice(&session, now).unwrap().opacity, 0.0);
        assert!(motion.tick(later(now, 40)));
        let shown = later(now, 200);
        assert!(!motion.tick(shown));
        assert_eq!(motion.notice(&session, shown).unwrap().opacity, 1.0);
        session.clear_toast();
        motion.observe(&session, (true, true), shown);
        let leaving = motion.notice(&session, shown).unwrap();
        assert!(leaving.leaving);
        assert_eq!(leaving.opacity, 1.0);
        assert!(motion.notice(&session, later(shown, 80)).unwrap().opacity < 1.0);
        assert!(!motion.tick(later(shown, 200)));
        assert!(motion.notice(&session, later(shown, 200)).is_none());
    }

    #[test]
    fn the_same_notice_again_stays_up() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        session.show_toast("Archived · u", false);
        motion.observe(&session, (true, true), now);
        motion.settle(now);
        session.clear_toast();
        motion.observe(&session, (true, true), now);
        session.show_toast("Archived · u", false);
        let back = later(now, 10);
        motion.observe(&session, (true, true), back);
        let notice = motion.notice(&session, back).unwrap();
        assert_eq!(notice.opacity, 1.0, "no blink between two archives");
        assert!(!notice.leaving);
    }

    #[test]
    fn a_key_jumps_overlays_and_glides_to_their_end() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        session.overlay = Overlay::Palette;
        motion.observe(&session, (true, true), now);
        assert_eq!(motion.overlay(now).0, 0.0);
        motion.list_glide = Glide::start(&motion.motion, 0.0, -680.0, 0.0, motion::SLIDE, now);
        let settled = motion.settle(now);
        assert_eq!(settled.list, Some(-680.0));
        assert_eq!(motion.overlay(now), (1.0, 0.0, 1.0));
        assert!(!motion.tick(now));
        // A glide the same key starts begins where the old one was drawn.
        assert_eq!(motion.glide_from_list(-680.0), 0.0);
        assert_eq!(motion.glide_from_list(-680.0), -680.0);
    }

    #[test]
    fn the_old_selection_fades_while_the_new_one_shows_at_once() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        session.cursor_id = Some("a".into());
        motion.observe(&session, (true, true), now);
        session.cursor_id = Some("b".into());
        motion.observe(&session, (true, true), now);
        assert_eq!(motion.selection_left("a", now), Some(1.0));
        assert_eq!(motion.selection_left("b", now), None);
        assert!(!motion.tick(later(now, 100)));
        assert_eq!(motion.selection_left("a", later(now, 100)), None);
    }

    #[test]
    fn the_conversation_slides_in_when_it_replaces_the_list() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        motion.observe(&session, (true, false), now);
        session.pane = Pane::Conversation;
        motion.observe(&session, (false, true), now);
        let entry = motion.pane_entry(Pane::Conversation, now);
        assert_eq!(entry.opacity, 0.0);
        assert!(entry.dx > 0.0, "from the right");
        assert_eq!(motion.pane_entry(Pane::List, now).opacity, 1.0);
        session.pane = Pane::List;
        let back = later(now, 300);
        motion.observe(&session, (true, false), back);
        assert!(
            motion.pane_entry(Pane::List, back).dx < 0.0,
            "from the left"
        );
    }

    #[test]
    fn reduced_motion_draws_end_states() {
        let mut motion = InboxMotion::new(Motion::new(false));
        let mut session = session();
        let now = Instant::now();
        session.overlay = Overlay::Palette;
        session.show_toast("Undone", false);
        session.pane = Pane::Conversation;
        motion.observe(&session, (false, true), now);
        assert_eq!(motion.overlay(now), (1.0, 0.0, 1.0));
        assert_eq!(motion.notice(&session, now).unwrap().opacity, 1.0);
        assert_eq!(motion.pane_entry(Pane::Conversation, now).opacity, 1.0);
        assert_eq!(motion.focus_edge(Pane::Conversation, now), 1.0);
        assert!(!motion.tick(now));
    }
}
