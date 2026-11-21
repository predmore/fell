//! The inbox window's running transitions, built on [`crate::motion`].
//!
//! Most of them start from what changed between two frames, not from the
//! code that changed it: [`InboxMotion::observe`] compares the session with
//! what the last frame drew, so every path that opens the palette, shows a
//! notice or switches panes gets the same transition. The list and the
//! scroll glides start where the view changes them, because they need the
//! positions from before the change.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::keymap::KeyId;
use crate::model::{FolderIntent, Overlay, Session, Toast};
use crate::motion::{self, Ghost, Glide, ListShift, Motion, Transition};
use crate::present::{ConvMessage, Pane, ThreadRow};

/// The old selection's highlight starts its fade from this much, so on a
/// slow frame two rows never look selected.
pub const SELECTION_FROM: f32 = 0.6;

/// The folder label and count leaving the header as a pair.
pub struct FolderHeaderFade {
    pub folder: String,
    pub label: Option<String>,
    pub unread: usize,
    fade: Transition,
    folder_from: f32,
    /// The account name and email can keep different opacities.
    account_from: [f32; 2],
    account_changes: [bool; 2],
}

impl FolderHeaderFade {
    /// Which half of the fade is showing, and its opacity before scaling.
    fn phase(&self, now: Instant) -> (bool, f32) {
        let time = self.fade.linear(now);
        if time < 0.5 {
            (true, motion::fade_out(2.0 * time))
        } else {
            (false, motion::ease_out_quad(2.0 * time - 1.0))
        }
    }

    pub fn label_opacities(&self, now: Instant) -> (f32, f32) {
        let (leaving, opacity) = self.phase(now);
        if leaving {
            (self.folder_from * opacity, 0.0)
        } else {
            (0.0, opacity)
        }
    }

    fn account_opacities(&self, leaving: bool, opacity: f32) -> [f32; 2] {
        std::array::from_fn(|part| {
            let midpoint = if self.account_changes[part] { 0.0 } else { 1.0 };
            if leaving {
                self.account_from[part] * opacity + midpoint * (1.0 - opacity)
            } else {
                motion::lerp(midpoint, 1.0, opacity)
            }
        })
    }

    /// Record which parts of the account fit change and sample the fit to draw.
    pub fn account_frame(&mut self, changes: [bool; 2], now: Instant) -> (bool, [f32; 2]) {
        self.account_changes = changes;
        let (leaving, opacity) = self.phase(now);
        (leaving, self.account_opacities(leaving, opacity))
    }
}

/// The status bar's notice and key hints as this frame draws them.
pub struct StatusMotion {
    /// The notice on screen and its opacity.
    pub notice: Option<(Toast, f32)>,
    /// A notice leaving, fading where it was: a key cleared it, or another
    /// notice replaced it.
    pub leaving: Option<(Toast, f32)>,
    /// How far below its place the notice is drawn, rising in.
    pub notice_dy: f32,
    /// How far above its place the leaving notice is drawn, rising out.
    pub leaving_dy: f32,
    /// How much of the key hints shows. A notice takes their room, so they
    /// fade out as it comes and back in as it leaves.
    pub hints: f32,
}

/// What an overlay or which-key showed, kept from the frame before it
/// closed: the key that closed it may already have cleared its query, so
/// it fades out as it was, not as the session is now.
#[derive(Clone, Debug)]
pub struct Layer {
    overlay: Overlay,
    chord: Vec<KeyId>,
    palette_query: String,
    palette_index: usize,
    folder_query: String,
    folder_index: usize,
    folder_intent: FolderIntent,
}

impl Layer {
    fn of(session: &Session) -> Self {
        Self {
            overlay: session.overlay.clone(),
            chord: session.chord.clone(),
            palette_query: session.palette_query.clone(),
            palette_index: session.palette_index,
            folder_query: session.folder_query.clone(),
            folder_index: session.folder_index,
            folder_intent: session.folder_intent,
        }
    }

    /// Trade this layer's state with the session's. Call it once to draw
    /// the layer as it was, and once more to put the session back.
    pub fn swap(&mut self, session: &mut Session) {
        std::mem::swap(&mut self.overlay, &mut session.overlay);
        std::mem::swap(&mut self.chord, &mut session.chord);
        std::mem::swap(&mut self.palette_query, &mut session.palette_query);
        std::mem::swap(&mut self.palette_index, &mut session.palette_index);
        std::mem::swap(&mut self.folder_query, &mut session.folder_query);
        std::mem::swap(&mut self.folder_index, &mut session.folder_index);
        std::mem::swap(&mut self.folder_intent, &mut session.folder_intent);
    }
}

/// A pane that just came into view, sliding in from its side.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaneEntry {
    pub opacity: f32,
    /// Offset from its place, in pixels (negative: from the left).
    pub dx: f32,
}

/// A narrow window swapping one pane for the other, as one frame draws it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaneSwap {
    /// The pane going out, drawn beneath for paint only.
    pub leaving: Pane,
    pub out: PaneEntry,
    /// The pane coming in, drawn on top.
    pub incoming: PaneEntry,
}

impl PaneSwap {
    /// Left edge and width of the opaque fill under the incoming pane, in
    /// a row `width` pixels wide: the pane's own place as drawn (its slide
    /// rounded as the view rounds it). The old pane shows only in the rest
    /// of the row, the strip the slide uncovers.
    pub fn cover(&self, width: f32) -> (f32, f32) {
        (self.incoming.dx.round(), width)
    }
}

pub struct InboxMotion {
    pub motion: Motion,
    /// The time every transition here runs on.
    pub clock: motion::Clock,
    /// Rows moving after an archive, undo, sync or folder switch.
    pub list: Option<ListShift<ThreadRow>>,
    /// The list viewport gliding to the selection (`Ctrl-d`, `gg`, `G`).
    pub list_glide: Option<Glide>,
    /// The conversation gliding to a page or a message.
    pub conv_glide: Option<Glide>,
    toast_in: Option<Transition>,
    /// The opacity a notice fading in starts from: what a notice still
    /// leaving has already lost, so the two never show less than one.
    toast_from: f32,
    /// The notice that is leaving and its fade.
    toast_out: Option<(Toast, Transition)>,
    last_toast: Option<Toast>,
    /// The key hints fading out as a notice takes their room, and how
    /// much of them showed when it started.
    hints_out: Option<Transition>,
    hints_from: f32,
    /// A chord was pending last frame: the bar showed its prefix, not the hints.
    last_chord: bool,
    which: Option<Transition>,
    which_out: Option<(Layer, Transition)>,
    last_which: bool,
    overlay: Option<Transition>,
    overlay_out: Option<(Layer, Transition)>,
    last_overlay: Overlay,
    /// The overlay or which-key as last drawn, for its fade-out.
    last_layer: Option<Layer>,
    /// The pane that came into view and the side it comes from.
    pane: Option<(Transition, Pane)>,
    /// In a narrow window, the pane it replaced, fading out beneath it.
    pane_out: Option<(Pane, Transition)>,
    last_panes: (bool, bool),
    last_layout: (Pane, bool),
    /// The conversation's focus edge changing colour.
    focus: Option<Transition>,
    /// The row the selection just left, its highlight fading out.
    selection_out: Option<(String, Transition)>,
    last_selected: Option<String>,
    /// Another thread's messages just arrived in the conversation.
    arrival: Option<Transition>,
    /// They arrived since the last frame: the fade starts with the frame
    /// that first draws them, so a late frame cannot eat it.
    arrival_pending: bool,
    /// Bodies settling into the open thread, by message id.
    bodies: HashMap<String, Transition>,
    /// Where a glide a key interrupted was drawn, so the next glide starts
    /// there instead of snapping to the old target first.
    list_from: Option<f32>,
    conv_from: Option<f32>,
    /// The palette or folder picker's height: it fits its items, and eases
    /// to the new fit when typing changes how many there are.
    card_to: Option<f32>,
    card_glide: Option<Glide>,
    /// The folder and unread count from the last observed page.
    last_unread: Option<(String, Option<String>, usize)>,
    /// The count it replaced in the same folder, fading out under the new one.
    unread_out: Option<(usize, Transition)>,
    folder_out: Option<FolderHeaderFade>,
    quotes: DisclosureMotion,
    recipients: DisclosureMotion,
}

/// How open each moving disclosure is, from 0 to 1, by message id.
#[derive(Default)]
pub struct Disclosures {
    pub quotes: HashMap<String, f32>,
    pub recipients: HashMap<String, f32>,
}

#[derive(Default)]
struct DisclosureMotion {
    open: HashSet<String>,
    transitions: HashMap<String, (Transition, bool)>,
}

impl DisclosureMotion {
    fn sample(
        &mut self,
        open: &HashSet<String>,
        motion: &Motion,
        now: Instant,
    ) -> HashMap<String, f32> {
        if self.open != *open {
            for (changed, opening) in [
                (open.difference(&self.open), true),
                (self.open.difference(open), false),
            ] {
                let duration = if opening { motion::SLIDE } else { motion::FADE };
                if let Some(transition) = motion.start(duration, now) {
                    for id in changed {
                        self.transitions.insert(id.clone(), (transition, opening));
                    }
                }
            }
            self.open = open.clone();
        }
        self.transitions
            .iter()
            .map(|(id, (transition, opening))| {
                let shown = if *opening {
                    transition.progress(now)
                } else {
                    transition.remaining(now)
                };
                (id.clone(), shown)
            })
            .collect()
    }

    fn tick(&mut self, now: Instant) -> bool {
        self.transitions
            .retain(|_, (transition, _)| transition.running(now));
        !self.transitions.is_empty()
    }
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
            clock: motion::Clock::default(),
            list: None,
            list_glide: None,
            conv_glide: None,
            toast_in: None,
            toast_from: 0.0,
            toast_out: None,
            last_toast: None,
            hints_out: None,
            hints_from: 1.0,
            last_chord: false,
            which: None,
            which_out: None,
            last_which: false,
            overlay: None,
            overlay_out: None,
            last_overlay: Overlay::None,
            last_layer: None,
            pane: None,
            pane_out: None,
            last_panes: (true, true),
            last_layout: (Pane::List, false),
            focus: None,
            selection_out: None,
            last_selected: None,
            arrival: None,
            arrival_pending: false,
            list_from: None,
            conv_from: None,
            card_to: None,
            card_glide: None,
            bodies: HashMap::new(),
            quotes: DisclosureMotion::default(),
            recipients: DisclosureMotion::default(),
            last_unread: None,
            unread_out: None,
            folder_out: None,
        }
    }

    /// Start the transitions for what changed since the last frame.
    /// `panes`: which panes this frame shows (list, conversation).
    pub fn observe(&mut self, session: &Session, panes: (bool, bool), now: Instant) {
        let start = |duration| self.motion.start(duration, now);
        if std::mem::take(&mut self.arrival_pending) {
            self.arrival = start(motion::QUICK);
        }
        let notice = session.status_notice();
        if notice != self.last_toast.as_ref() {
            match (notice, self.last_toast.take()) {
                (Some(toast), old) => {
                    // The same notice again (a second `e` in a burst): it
                    // stays up instead of blinking out and back.
                    let returning = self
                        .toast_out
                        .as_ref()
                        .is_some_and(|(leaving, _)| leaving == toast);
                    if returning {
                        self.toast_in = None;
                        self.toast_out = None;
                    } else {
                        self.toast_in = start(motion::FADE);
                        self.toast_from = 0.0;
                        match old {
                            // Another notice replaced this one: cross-fade,
                            // out as fast as the new one comes in.
                            Some(old) => {
                                self.toast_out = start(motion::FADE).map(|fade| (old, fade));
                            }
                            // The hints make room as the notice comes, from
                            // what shows of them now. A notice still leaving
                            // keeps fading under the new one.
                            // A chord's prefix held the hints' place: the
                            // hints were not showing, so they stay hidden.
                            None => {
                                // A notice that comes a few frames after the
                                // key cleared the last one (an archive is
                                // shown when the store has it) picks up
                                // where that one fades.
                                if let Some((_, fade)) = &self.toast_out {
                                    self.toast_from = 1.0 - fade.remaining(now);
                                }
                                self.hints_from = match &self.toast_out {
                                    _ if self.last_chord => 0.0,
                                    Some((_, fade)) => 1.0 - fade.remaining(now),
                                    None => 1.0,
                                };
                                self.hints_out = start(motion::FADE);
                            }
                        }
                    }
                }
                (None, Some(old)) => {
                    self.toast_in = None;
                    self.hints_out = None;
                    self.toast_out = start(motion::FADE).map(|fade| (old, fade));
                }
                (None, None) => {}
            }
            self.last_toast = notice.cloned();
        }

        let which =
            session.which_visible && session.overlay == Overlay::None && !session.chord.is_empty();
        if which != self.last_which {
            if which {
                self.which = start(motion::FADE);
                self.which_out = None;
            } else {
                self.which = None;
                self.which_out = self
                    .last_layer
                    .clone()
                    .filter(|layer| !layer.chord.is_empty())
                    .and_then(|layer| start(motion::FADE).map(|fade| (layer, fade)));
            }
        }
        self.last_which = which;
        self.last_chord = !session.chord.is_empty();

        if session.overlay != self.last_overlay {
            if session.overlay == Overlay::None {
                // Gone for input at once; only its pixels fade.
                self.overlay = None;
                self.overlay_out = self
                    .last_layer
                    .clone()
                    .filter(|layer| layer.overlay != Overlay::None)
                    .and_then(|layer| start(motion::FADE).map(|fade| (layer, fade)));
            } else {
                self.overlay = start(motion::FADE);
                self.overlay_out = None;
                // A card that opens is as tall as its items at once.
                self.card_to = None;
                self.card_glide = None;
            }
            self.last_overlay = session.overlay.clone();
        }
        self.last_layer = (which || session.overlay != Overlay::None).then(|| Layer::of(session));

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
                // A narrow window swaps one pane for the other: the old one
                // fades out under the new one, so no frame shows neither.
                let leaving = match (self.last_panes, panes) {
                    ((true, false), (false, true)) => Some(Pane::List),
                    ((false, true), (true, false)) => Some(Pane::Conversation),
                    _ => None,
                };
                self.pane_out = leaving.and_then(|pane| start(motion::FADE).map(|t| (pane, t)));
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

    /// The open card should be `fit` pixels tall to hold its items; `None`
    /// when no card is open, and one fading out keeps the height it had.
    /// A key never settles this: nothing acts on the card's bottom edge,
    /// and typing quickly keeps easing from where the edge is drawn.
    pub fn fit_card(&mut self, fit: Option<f32>, now: Instant) {
        let Some(fit) = fit else { return };
        if let Some(to) = self.card_to.filter(|to| (to - fit).abs() >= 0.5) {
            let from = self.card_glide.map_or(to, |glide| glide.value(now));
            self.card_glide = Glide::start(&self.motion, from, fit, 0.0, motion::FADE, now);
        }
        self.card_to = Some(fit);
    }

    /// How tall the card is drawn now.
    pub fn card_height(&self, now: Instant) -> Option<f32> {
        self.card_glide
            .map(|glide| glide.value(now))
            .or(self.card_to)
    }

    /// Put another thread's messages in the conversation. They fade in
    /// with the next frame however they were read: a thread from the
    /// store, or a compose draft in the Drafts list from memory. Every
    /// path that replaces the conversation goes through here, so none
    /// pops in.
    pub fn show_thread(&mut self, session: &mut Session, messages: Vec<ConvMessage>) {
        session.set_messages(messages);
        self.arrival = None;
        self.arrival_pending = true;
        self.bodies.clear();
    }

    /// Jump every transition to its end state: a key acts on what the
    /// reader will see, never on a frame in between. Glides return their
    /// targets for the view to apply; where they were drawn is kept for a
    /// glide the same key starts ([`Self::glide_from_list`]) and for the
    /// rows a list change animates from ([`Self::painted_list_offset`]).
    /// Rows still sliding keep sliding: the next list change starts from
    /// where they are drawn, and the list the key acts on is already final.
    /// What is leaving (a notice, an overlay) keeps fading: no key acts on
    /// it. The exception is the pane a narrow window swapped out: the
    /// pane that replaced it is now in place and covers it, so it goes.
    pub fn settle(&mut self, now: Instant) -> Settled {
        let list = self.list_glide.take();
        let conversation = self.conv_glide.take();
        self.list_from = list.map(|glide| glide.value(now));
        self.conv_from = conversation.map(|glide| glide.value(now));
        self.toast_in = None;
        self.hints_out = None;
        self.which = None;
        self.overlay = None;
        self.pane = None;
        self.pane_out = None;
        self.focus = None;
        self.selection_out = None;
        self.arrival = None;
        self.arrival_pending = false;
        self.bodies.clear();
        self.quotes.transitions.clear();
        self.recipients.transitions.clear();
        self.unread_out = None;
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

    /// The list's offset as the last frame painted it, when a key settled
    /// a glide and moved the offset to its target before the next paint.
    pub fn painted_list_offset(&self) -> Option<f32> {
        self.list_from
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
        fn keep_with<T>(fade: &mut Option<(T, Transition)>, now: Instant) -> bool {
            if fade.as_ref().is_some_and(|(_, t)| !t.running(now)) {
                *fade = None;
            }
            fade.is_some()
        }
        let mut running = false;
        running |= keep(&mut self.toast_in, now);
        running |= keep_with(&mut self.toast_out, now);
        running |= keep(&mut self.hints_out, now);
        running |= keep(&mut self.which, now);
        running |= keep_with(&mut self.which_out, now);
        running |= keep(&mut self.overlay, now);
        running |= keep_with(&mut self.overlay_out, now);
        running |= keep(&mut self.focus, now);
        running |= keep(&mut self.arrival, now);
        self.bodies.retain(|_, fade| fade.running(now));
        running |= self.quotes.tick(now);
        running |= self.recipients.tick(now);
        running |= !self.bodies.is_empty();
        if self.card_glide.is_some_and(|glide| !glide.running(now)) {
            self.card_glide = None;
        }
        running |= self.card_glide.is_some();
        running |= keep_with(&mut self.selection_out, now);
        running |= keep_with(&mut self.unread_out, now);
        if self
            .folder_out
            .as_ref()
            .is_some_and(|old| !old.fade.running(now))
        {
            self.folder_out = None;
        }
        running |= self.folder_out.is_some();
        if self.pane.is_some_and(|(t, _)| !t.running(now)) {
            self.pane = None;
        }
        running |= self.pane.is_some();
        running |= keep_with(&mut self.pane_out, now);
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

    /// Rows were put above: rows that left move down with their neighbours.
    pub fn move_ghosts(&mut self, dy: f32) {
        if let Some(shift) = &mut self.list {
            shift.move_ghosts(dy);
        }
    }

    /// Where row `id`, whose slot starts at viewport `top`, is drawn
    /// relative to it right now.
    pub fn row_offset(&self, id: &str, top: f32, now: Instant) -> f32 {
        self.list
            .as_ref()
            .map_or(0.0, |shift| shift.offset_of(id, top, now))
    }

    /// Observe the folder whose page actually supplied the count.
    pub fn observe_header(
        &mut self,
        folder: &str,
        label: Option<&str>,
        unread: usize,
        now: Instant,
    ) {
        let Some((previous, previous_label, count)) = &mut self.last_unread else {
            self.last_unread = Some((folder.to_owned(), label.map(str::to_owned), unread));
            return;
        };
        if previous != folder || previous_label.as_deref() != label {
            let old_folder = std::mem::replace(previous, folder.to_owned());
            let old_label = std::mem::replace(previous_label, label.map(str::to_owned));
            let old = self.folder_out.take();
            self.folder_out = self.motion.start(motion::FADE, now).map(|fade| {
                let mut next = FolderHeaderFade {
                    folder: old_folder,
                    label: old_label,
                    unread: *count,
                    fade,
                    folder_from: 1.0,
                    account_from: [1.0; 2],
                    account_changes: [false; 2],
                };
                if let Some(old) = old {
                    let (leaving, opacity) = old.phase(now);
                    next.account_from = old.account_opacities(leaving, opacity);
                    next.folder_from = opacity;
                    if leaving {
                        next.folder = old.folder;
                        next.label = old.label;
                        next.unread = old.unread;
                        next.folder_from *= old.folder_from;
                    }
                }
                next
            });
            self.unread_out = None;
        } else if *count != unread && self.folder_out.is_none() {
            self.unread_out = self
                .motion
                .start(motion::FADE, now)
                .map(|fade| (*count, fade));
        }
        *count = unread;
    }

    pub fn folder_header(&mut self) -> Option<&mut FolderHeaderFade> {
        self.folder_out.as_mut()
    }

    /// The header's unread count being replaced: the old count and how far
    /// the new one has faded in over it.
    pub fn unread_count(&self, now: Instant) -> Option<(usize, f32)> {
        self.unread_out
            .as_ref()
            .map(|(old, fade)| (*old, fade.progress(now)))
    }

    pub fn status(&self, session: &Session, now: Instant) -> StatusMotion {
        let rising = motion::progress(self.toast_in.as_ref(), now);
        let notice = session
            .status_notice()
            .cloned()
            .map(|toast| (toast, motion::lerp(self.toast_from, 1.0, rising)));
        let leaving = self
            .toast_out
            .as_ref()
            .map(|(toast, fade)| (toast.clone(), fade.remaining(now)));
        let gone = self
            .toast_out
            .as_ref()
            .map_or(0.0, |(_, fade)| motion::ease_out(fade.linear(now)));
        let hints = match (&notice, &leaving) {
            (Some(_), _) => {
                self.hints_from * self.hints_out.map_or(0.0, |fade| fade.remaining(now))
            }
            (None, Some((_, opacity))) => 1.0 - opacity,
            (None, None) => 1.0,
        };
        StatusMotion {
            notice,
            leaving,
            notice_dy: motion::NOTICE_RISE_PX * (1.0 - rising),
            leaving_dy: -motion::NOTICE_RISE_PX * gone,
            hints,
        }
    }

    /// Opacity and rise (pixels still to travel up) of the which-key popup.
    pub fn which(&self, now: Instant) -> (f32, f32) {
        let p = motion::progress(self.which.as_ref(), now);
        (p, motion::RISE_PX * (1.0 - p))
    }

    /// Opacity and drop (pixels still to travel down) of an overlay.
    pub fn overlay(&self, now: Instant) -> (f32, f32) {
        let p = motion::progress(self.overlay.as_ref(), now);
        (p, motion::RISE_PX * (1.0 - p))
    }

    /// An overlay that just closed, as it was, and its opacity.
    pub fn closing_overlay(&mut self, now: Instant) -> Option<(&mut Layer, f32)> {
        self.overlay_out
            .as_mut()
            .map(|(layer, fade)| (layer, fade.remaining(now)))
    }

    /// The which-key popup that just closed, as it was, and its opacity.
    pub fn closing_which(&mut self, now: Instant) -> Option<(&mut Layer, f32)> {
        self.which_out
            .as_mut()
            .map(|(layer, fade)| (layer, fade.remaining(now)))
    }

    pub fn pane_entry(&self, pane: Pane, now: Instant) -> PaneEntry {
        match self.pane {
            Some((transition, entering)) if entering == pane => {
                let p = transition.progress(now);
                let side = if pane == Pane::List { -1.0 } else { 1.0 };
                PaneEntry {
                    // From ARRIVE_FROM, like any content that replaces other
                    // content: never from empty.
                    opacity: motion::lerp(motion::ARRIVE_FROM, 1.0, p),
                    dx: side * motion::PANE_SLIDE_PX * (1.0 - p),
                }
            }
            _ => PaneEntry {
                opacity: 1.0,
                dx: 0.0,
            },
        }
    }

    /// The pane a narrow window just swapped out, still fading where it
    /// was as it moves a little towards its own side (the list left, the
    /// conversation right): the way the new one came in, continued.
    pub fn pane_leaving(&self, now: Instant) -> Option<(Pane, PaneEntry)> {
        let (pane, fade) = self
            .pane_out
            .as_ref()
            .filter(|(_, fade)| fade.running(now))?;
        let shows = fade.remaining(now);
        let side = if *pane == Pane::List { -1.0 } else { 1.0 };
        Some((
            *pane,
            PaneEntry {
                opacity: shows,
                dx: side * motion::PANE_SLIDE_PX * (1.0 - shows),
            },
        ))
    }

    /// The pane swap a narrow window is drawing, if any. `shown`: which
    /// panes this frame shows (list, conversation).
    pub fn pane_swap(&self, shown: (bool, bool), now: Instant) -> Option<PaneSwap> {
        let (leaving, out) = self.pane_leaving(now)?;
        let incoming = match (leaving, shown) {
            (Pane::List, (false, true)) => Pane::Conversation,
            (Pane::Conversation, (true, false)) => Pane::List,
            _ => return None,
        };
        Some(PaneSwap {
            leaving,
            out,
            incoming: self.pane_entry(incoming, now),
        })
    }

    /// How much of the selection highlight row `id` still shows, when the
    /// selection just left it.
    pub fn selection_left(&self, id: &str, now: Instant) -> Option<f32> {
        self.selection_out
            .as_ref()
            .filter(|(old, _)| old == id)
            .map(|(_, fade)| SELECTION_FROM * (1.0 - fade.progress(now)))
    }

    /// Opacity of the conversation's subject and messages: another
    /// thread's arrive from [`motion::ARRIVE_FROM`], so the pane never
    /// blinks to empty.
    pub fn arrival(&self, now: Instant) -> f32 {
        motion::lerp(
            motion::ARRIVE_FROM,
            1.0,
            motion::progress(self.arrival.as_ref(), now),
        )
    }

    /// A message's HTML body replaced the room kept for it, or its stored
    /// text, under a header already drawn: it settles in over [`motion::QUICK`].
    pub fn body_arrived(&mut self, id: &str, now: Instant) {
        if let Some(fade) = self.motion.start(motion::QUICK, now) {
            self.bodies.insert(id.to_string(), fade);
        }
    }

    /// Opacity of each body still settling in, by message id.
    pub fn body_alphas(&self, now: Instant) -> HashMap<String, f32> {
        self.bodies
            .iter()
            .map(|(id, fade)| {
                let alpha = motion::lerp(motion::ARRIVE_FROM, 1.0, fade.progress(now));
                (id.clone(), alpha)
            })
            .collect()
    }

    /// History and recipients can open together with `q` or separately by
    /// click. Both use the same timing and reduced-motion setting.
    pub fn disclosures(&mut self, session: &Session, now: Instant) -> Disclosures {
        Disclosures {
            quotes: self.quotes.sample(&session.quotes_open, &self.motion, now),
            recipients: self
                .recipients
                .sample(&session.recipients_open, &self.motion, now),
        }
    }

    /// How far the conversation is towards "focused": its edge and the
    /// message card under the cursor follow this.
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

    fn notice_opacity(motion: &InboxMotion, session: &Session, now: Instant) -> Option<f32> {
        motion
            .status(session, now)
            .notice
            .map(|(_, opacity)| opacity)
    }

    #[test]
    fn the_unread_count_cross_fades_and_a_key_settles_it() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        session.unread_count = 3;
        let now = Instant::now();
        motion.observe_header(&session.folder, None, session.unread_count, now);
        assert_eq!(motion.unread_count(now), None);
        // Opened a thread: 3 → 2 in the same folder.
        session.unread_count = 2;
        motion.observe_header(&session.folder, None, session.unread_count, now);
        assert_eq!(motion.unread_count(now), Some((3, 0.0)));
        let (old, mid) = motion.unread_count(later(now, 60)).unwrap();
        assert_eq!(old, 3);
        assert!(mid > 0.5 && mid < 1.0, "{mid}");
        assert!(!motion.tick(later(now, 120)));
        assert_eq!(motion.unread_count(later(now, 120)), None);
        // A key mid-fade shows the new count alone.
        session.unread_count = 1;
        motion.observe_header(&session.folder, None, session.unread_count, later(now, 200));
        assert!(motion.unread_count(later(now, 210)).is_some());
        motion.settle(later(now, 210));
        assert_eq!(motion.unread_count(later(now, 210)), None);
        // On a folder switch the old name and its count fade as a pair.
        session.folder = "Archive".into();
        session.unread_count = 7;
        motion.observe_header(&session.folder, None, session.unread_count, later(now, 300));
        assert_eq!(motion.unread_count(later(now, 300)), None);
        let fade = motion.folder_header().unwrap();
        assert_eq!((fade.folder.as_str(), fade.unread), ("INBOX", 1));
        assert_eq!(fade.label_opacities(later(now, 300)), (1.0, 0.0));
        motion.tick(later(now, 420));
        assert!(motion.folder_header().is_none());
        // Nor with motion reduced.
        let mut reduced = InboxMotion::new(Motion::new(false));
        reduced.observe_header(&session.folder, None, session.unread_count, now);
        session.unread_count = 6;
        reduced.observe_header(&session.folder, None, session.unread_count, now);
        assert_eq!(reduced.unread_count(now), None);
    }

    #[test]
    fn a_switch_during_the_header_exit_keeps_the_outgoing_folder() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let now = Instant::now();
        motion.observe_header("INBOX", None, 3, now);
        motion.observe_header("Archive", None, 7, now);

        let interrupted = now + motion::FADE / 4;
        let fade = motion.folder_header().unwrap();
        let before = fade.label_opacities(interrupted);
        assert!((before.0 - 0.25).abs() <= f32::EPSILON);
        assert_eq!(before.1, 0.0);
        let account = fade.account_frame([false, true], interrupted);
        assert_eq!(account, (true, [1.0, before.0]));

        motion.observe_header("Spam", None, 5, interrupted);
        let fade = motion.folder_header().unwrap();
        assert_eq!((fade.folder.as_str(), fade.unread), ("INBOX", 3));
        assert_eq!(fade.label_opacities(interrupted), before);
        assert_eq!(fade.account_frame([true, true], interrupted), account);
        let (outgoing, incoming) = fade.label_opacities(interrupted + motion::FADE / 4);
        assert!(0.0 < outgoing && outgoing < before.0);
        assert_eq!(incoming, 0.0);
    }

    #[test]
    fn a_switch_during_the_header_arrival_fades_out_the_arriving_folder() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let now = Instant::now();
        motion.observe_header("INBOX", None, 3, now);
        motion.observe_header("Archive", None, 7, now);

        let interrupted = now + 3 * motion::FADE / 4;
        let fade = motion.folder_header().unwrap();
        let before = fade.label_opacities(interrupted);
        assert_eq!(before.0, 0.0);
        assert!((before.1 - 0.75).abs() <= f32::EPSILON);
        let account = fade.account_frame([true, true], interrupted);
        assert_eq!(account, (false, [before.1; 2]));

        motion.observe_header("Spam", None, 5, interrupted);
        let fade = motion.folder_header().unwrap();
        assert_eq!((fade.folder.as_str(), fade.unread), ("Archive", 7));
        assert_eq!(fade.label_opacities(interrupted), (before.1, 0.0));
        assert_eq!(
            fade.account_frame([false, true], interrupted),
            (true, account.1)
        );
        let (outgoing, incoming) = fade.label_opacities(interrupted + motion::FADE / 4);
        assert!(0.0 < outgoing && outgoing < before.1);
        assert_eq!(incoming, 0.0);
        assert_eq!(fade.label_opacities(interrupted + motion::FADE), (0.0, 1.0));
    }

    #[test]
    fn a_notice_fades_in_then_out_after_a_key() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        motion.observe(&session, (true, true), now);
        session.show_toast("Archived · u", false);
        motion.observe(&session, (true, true), now);
        assert_eq!(notice_opacity(&motion, &session, now), Some(0.0));
        assert!(motion.tick(later(now, 40)));
        let shown = later(now, 200);
        assert!(!motion.tick(shown));
        assert_eq!(notice_opacity(&motion, &session, shown), Some(1.0));
        session.clear_toast();
        motion.observe(&session, (true, true), shown);
        let status = motion.status(&session, shown);
        assert!(status.notice.is_none());
        assert_eq!(status.leaving.as_ref().unwrap().1, 1.0);
        assert_eq!(status.hints, 0.0);
        let mid = motion.status(&session, later(shown, 80));
        assert!(mid.leaving.unwrap().1 < 1.0);
        assert!(
            mid.hints > 0.0 && mid.hints < 1.0,
            "hints come back as it leaves"
        );
        assert!(!motion.tick(later(shown, 200)));
        let gone = motion.status(&session, later(shown, 200));
        assert!(gone.leaving.is_none());
        assert_eq!(gone.hints, 1.0);
    }

    #[test]
    fn the_hints_fade_out_as_a_notice_fades_in() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        motion.observe(&session, (true, true), now);
        session.show_toast("Archived · u", false);
        motion.observe(&session, (true, true), now);
        assert_eq!(motion.status(&session, now).hints, 1.0, "no pop");
        let mid = motion.status(&session, later(now, 40));
        assert!(mid.hints > 0.0 && mid.hints < 1.0);
        let sum = mid.hints + mid.notice.unwrap().1;
        assert!((1.0..1.2).contains(&sum), "never a dip: {sum}");
        let done = motion.status(&session, later(now, 120));
        assert_eq!(done.hints, 0.0, "gone after FADE");
        assert_eq!(done.notice.unwrap().1, 1.0);
    }

    #[test]
    fn hints_a_chord_replaced_do_not_come_back_to_fade() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        session.chord = vec![KeyId::Space];
        motion.observe(&session, (true, true), now);
        // `space s` with no account: the chord ends in a notice.
        session.chord.clear();
        session.show_toast("No account is set up", true);
        motion.observe(&session, (true, true), now);
        assert_eq!(motion.status(&session, now).hints, 0.0);
    }

    #[test]
    fn a_notice_replacing_another_cross_fades() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        session.show_toast("Undone", false);
        motion.observe(&session, (true, true), now);
        motion.settle(now);
        session.show_toast("Archived · u", false);
        let swap = later(now, 300);
        motion.observe(&session, (true, true), swap);
        let status = motion.status(&session, swap);
        assert_eq!(status.notice.as_ref().unwrap().0.text, "Archived · u");
        assert_eq!(status.notice.unwrap().1, 0.0);
        let (old, opacity) = status.leaving.unwrap();
        assert_eq!(old.text, "Undone");
        assert_eq!(opacity, 1.0);
        assert_eq!(status.hints, 0.0);
        // A true cross-fade: one goes as the other comes, never a cut.
        for ms in [17, 33, 50, 80] {
            let mid = motion.status(&session, later(swap, ms));
            let (old, new) = (mid.leaving.unwrap().1, mid.notice.unwrap().1);
            assert!((1.0..1.2).contains(&(old + new)), "{ms} ms: {old} + {new}");
        }
        assert!(motion.status(&session, later(swap, 17)).leaving.unwrap().1 > 0.7);
        // A swap, not a smear: the new one rises in from just below its
        // place as the old one rises out above it, over the same fade.
        assert_eq!(
            (status.notice_dy, status.leaving_dy),
            (motion::NOTICE_RISE_PX, 0.0)
        );
        let mut last = (status.notice_dy, status.leaving_dy);
        for ms in [17, 33, 50, 80] {
            let mid = motion.status(&session, later(swap, ms));
            assert!(mid.notice_dy < last.0 && mid.notice_dy > 0.0, "{ms} ms");
            assert!(mid.leaving_dy < last.1 && mid.leaving_dy > -motion::NOTICE_RISE_PX);
            last = (mid.notice_dy, mid.leaving_dy);
        }
        let after = motion.status(&session, later(swap, 120));
        assert!(after.leaving.is_none_or(|(_, o)| o == 0.0), "out over FADE");
        assert_eq!(after.notice.unwrap().1, 1.0);
        assert_eq!(after.notice_dy, 0.0, "in its place");
    }

    #[test]
    fn a_notice_that_comes_late_picks_up_where_the_last_one_fades() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        session.show_toast("Undone", false);
        motion.observe(&session, (true, true), now);
        motion.settle(now);
        // `e`: the key clears "Undone" at once ...
        let key = later(now, 300);
        session.clear_toast();
        motion.observe(&session, (true, true), key);
        // ... and the store has the archive three frames later.
        let shown = later(key, 50);
        session.show_toast("Archived · u", false);
        motion.observe(&session, (true, true), shown);
        for ms in [0, 17, 33, 60] {
            let status = motion.status(&session, later(shown, ms));
            let new = status.notice.unwrap().1;
            let old = status.leaving.map_or(0.0, |(_, o)| o);
            // Within a few percent of one whole notice: no visible dip.
            assert!(old + new >= 0.95, "{ms} ms: {old} + {new}");
        }
        assert_eq!(
            motion.status(&session, later(shown, 120)).notice.unwrap().1,
            1.0
        );
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
        let status = motion.status(&session, back);
        assert_eq!(
            status.notice.unwrap().1,
            1.0,
            "no blink between two archives"
        );
        assert!(status.leaving.is_none());
        assert_eq!(status.hints, 0.0);
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
        assert_eq!(motion.overlay(now), (1.0, 0.0));
        assert!(!motion.tick(now));
        // A list change the same key makes starts from the painted rows.
        assert_eq!(motion.painted_list_offset(), Some(0.0));
        // A glide the same key starts begins where the old one was drawn.
        assert_eq!(motion.glide_from_list(-680.0), 0.0);
        assert_eq!(motion.glide_from_list(-680.0), -680.0);
        motion.end_key();
        assert_eq!(motion.painted_list_offset(), None);
    }

    #[test]
    fn a_closed_overlay_fades_out_as_it_was() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        session.overlay = Overlay::Palette;
        session.palette_query = "arch".into();
        motion.observe(&session, (true, true), now);
        // Enter runs the item and closes the palette in one key.
        motion.settle(now);
        session.overlay = Overlay::None;
        session.palette_query.clear();
        motion.observe(&session, (true, true), now);
        let (layer, opacity) = motion.closing_overlay(now).unwrap();
        assert_eq!(opacity, 1.0);
        assert_eq!(layer.overlay, Overlay::Palette);
        layer.swap(&mut session);
        assert_eq!(session.palette_query, "arch", "drawn as it was");
        assert_eq!(session.overlay, Overlay::Palette);
        layer.swap(&mut session);
        assert_eq!(session.overlay, Overlay::None, "the session is put back");
        assert!(session.palette_query.is_empty());
        // A key during the fade does not cut it off, and it ends in FADE.
        motion.settle(later(now, 40));
        assert!(motion.closing_overlay(later(now, 40)).unwrap().1 < 1.0);
        assert!(
            motion.closing_overlay(later(now, 17)).unwrap().1 > 0.7,
            "no cut on the first frame"
        );
        assert!(motion.tick(later(now, 80)));
        assert!(!motion.tick(later(now, 120)));
        assert!(motion.closing_overlay(later(now, 120)).is_none());
    }

    #[test]
    fn a_new_overlay_replaces_one_still_fading_out() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        session.overlay = Overlay::Help;
        motion.observe(&session, (true, true), now);
        session.overlay = Overlay::None;
        motion.observe(&session, (true, true), now);
        assert!(motion.closing_overlay(now).is_some());
        session.overlay = Overlay::Palette;
        motion.observe(&session, (true, true), later(now, 20));
        assert!(motion.closing_overlay(later(now, 20)).is_none());
    }

    #[test]
    fn which_key_fades_out_with_its_chord() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        session.chord = vec![KeyId::Char('g')];
        session.which_visible = true;
        motion.observe(&session, (true, true), now);
        session.chord.clear();
        session.which_visible = false;
        motion.observe(&session, (true, true), now);
        let (layer, opacity) = motion.closing_which(now).unwrap();
        assert_eq!(opacity, 1.0);
        layer.swap(&mut session);
        assert_eq!(session.chord, vec![KeyId::Char('g')]);
        layer.swap(&mut session);
        assert!(session.chord.is_empty());
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
        assert_eq!(motion.selection_left("a", now), Some(SELECTION_FROM));
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
        assert_eq!(entry.opacity, motion::ARRIVE_FROM, "never from empty");
        assert!(entry.dx > 0.0, "from the right");
        assert_eq!(motion.pane_entry(Pane::List, now).opacity, 1.0);
        // The list fades out under it, so a frame never shows neither.
        for ms in [0, 16, 33, 50, 80, 119] {
            let at = later(now, ms);
            let (pane, out) = motion.pane_leaving(at).expect("the list is leaving");
            assert_eq!(pane, Pane::List);
            assert!(out.dx <= 0.0, "towards the left");
            let coming = motion.pane_entry(Pane::Conversation, at).opacity;
            assert!(
                coming + out.opacity >= 0.95,
                "{ms} ms: {coming} + {}",
                out.opacity
            );
        }
        assert!(
            motion.tick(later(now, 130)),
            "the conversation still slides"
        );
        assert!(motion.pane_leaving(later(now, 130)).is_none());
        session.pane = Pane::List;
        let back = later(now, 300);
        motion.observe(&session, (true, false), back);
        assert!(
            motion.pane_entry(Pane::List, back).dx < 0.0,
            "from the left"
        );
        assert_eq!(
            motion.pane_entry(Pane::List, back).opacity,
            motion::ARRIVE_FROM
        );
        let (pane, _) = motion
            .pane_leaving(back)
            .expect("the conversation is leaving");
        assert_eq!(pane, Pane::Conversation);
        // A key mid-swap puts the list in place: nothing of the
        // conversation is drawn under it any more.
        motion.settle(later(back, 40));
        assert!(motion.pane_leaving(later(back, 40)).is_none());
        assert_eq!(motion.pane_entry(Pane::List, later(back, 40)).dx, 0.0);
    }

    #[test]
    fn a_compose_draft_fades_in_as_a_synced_one_does() {
        // #169 round 6: `j` to a compose draft in Drafts set its messages
        // from memory without the arrival fade, so its title and body
        // popped in, while a synced draft's faded in.
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        motion.observe(&session, (true, true), now);
        assert_eq!(motion.arrival(now), 1.0);
        let draft = fell_core::compose::DraftSummary {
            id: "d1".into(),
            from: "a@x".into(),
            to: vec!["sam@example.com".into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Notes".into(),
            body: "## Agenda\n\nA **short** list\n".into(),
            owner: String::new(),
            updated_unix: 1_000,
            maybe_sent: false,
            reply_context: None,
            forwarded: None,
            in_reply_to: None,
            known_names: Default::default(),
        };
        let shown = crate::present::draft_conversation(&draft, "a@x", 2_000);
        motion.show_thread(&mut session, shown);
        assert_eq!(session.messages.len(), 1);
        // The frame that first draws it starts the fade, from ARRIVE_FROM:
        // the pane never blinks to empty.
        let drawn = later(now, 500);
        motion.observe(&session, (true, true), drawn);
        assert_eq!(motion.arrival(drawn), motion::ARRIVE_FROM);
        assert!(motion.tick(later(drawn, 40)));
        let mid = motion.arrival(later(drawn, 40));
        assert!(mid > motion::ARRIVE_FROM && mid < 1.0, "{mid}");
        assert!(!motion.tick(later(drawn, 200)));
        assert_eq!(motion.arrival(later(drawn, 200)), 1.0);
        // Off with reduce_motion: it is simply there.
        let mut still = InboxMotion::new(Motion::new(false));
        let shown = crate::present::draft_conversation(&draft, "a@x", 2_000);
        still.show_thread(&mut session, shown);
        still.observe(&session, (true, true), drawn);
        assert_eq!(still.arrival(drawn), 1.0);
    }

    #[test]
    fn a_pane_swap_shows_the_old_pane_only_in_the_uncovered_strip() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        let width = 700.0;
        motion.observe(&session, (true, false), now);
        session.pane = Pane::Conversation;
        motion.observe(&session, (false, true), now);
        // Enter: the conversation covers all but a strip on the left from
        // its first frame, and the strip closes as it slides in.
        let mut strip = f32::INFINITY;
        for ms in [0, 16, 33, 50, 80, 119] {
            let swap = motion
                .pane_swap((false, true), later(now, ms))
                .expect("swapping");
            assert_eq!(swap.leaving, Pane::List);
            let (left, cover) = swap.cover(width);
            assert_eq!(cover, width);
            assert!(
                (0.0..=motion::PANE_SLIDE_PX).contains(&left),
                "{ms} ms: {left}"
            );
            assert!(left <= strip, "the strip never widens");
            strip = left;
            if ms == 0 {
                assert_eq!(left, motion::PANE_SLIDE_PX, "covered from the first frame");
                assert_eq!(swap.incoming.opacity, motion::ARRIVE_FROM);
            }
        }
        // A frame that shows both panes is not a swap.
        assert!(motion.pane_swap((true, true), now).is_none());
        // Esc: the list comes from the left; the strip is on the right.
        session.pane = Pane::List;
        let back = later(now, 300);
        motion.observe(&session, (true, false), back);
        let swap = motion
            .pane_swap((true, false), back)
            .expect("swapping back");
        assert_eq!(swap.leaving, Pane::Conversation);
        let (left, cover) = swap.cover(width);
        assert_eq!(left, -motion::PANE_SLIDE_PX);
        assert!(left + cover < width, "a strip shows on the right");
        assert!(motion.pane_swap((true, false), later(back, 200)).is_none());
    }

    #[test]
    fn the_palette_eases_to_fit_its_items() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        session.overlay = Overlay::Palette;
        motion.observe(&session, (true, true), now);
        motion.fit_card(Some(400.0), now);
        assert_eq!(motion.card_height(now), Some(400.0), "opens at its fit");
        // Typing filters it down to two items.
        let typed = later(now, 200);
        motion.fit_card(Some(126.0), typed);
        assert_eq!(motion.card_height(typed), Some(400.0), "no jump");
        let mid = motion.card_height(later(typed, 40)).unwrap();
        assert!(mid < 400.0 && mid > 126.0);
        // Another key mid-ease starts from where the edge is drawn.
        motion.settle(later(typed, 40));
        motion.fit_card(Some(90.0), later(typed, 40));
        assert_eq!(motion.card_height(later(typed, 40)), Some(mid));
        assert!(!motion.tick(later(typed, 200)));
        assert_eq!(motion.card_height(later(typed, 200)), Some(90.0));
        // Closed and opened again: at its fit at once.
        session.overlay = Overlay::None;
        motion.observe(&session, (true, true), later(typed, 300));
        motion.fit_card(None, later(typed, 300));
        assert_eq!(
            motion.card_height(later(typed, 300)),
            Some(90.0),
            "fades out as it was"
        );
        session.overlay = Overlay::Folders;
        motion.observe(&session, (true, true), later(typed, 400));
        motion.fit_card(Some(200.0), later(typed, 400));
        assert_eq!(motion.card_height(later(typed, 400)), Some(200.0));
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
        assert_eq!(motion.overlay(now), (1.0, 0.0));
        let status = motion.status(&session, now);
        assert_eq!(status.notice.unwrap().1, 1.0);
        assert_eq!(status.hints, 0.0);
        assert_eq!(motion.pane_entry(Pane::Conversation, now).opacity, 1.0);
        assert_eq!(motion.focus_edge(Pane::Conversation, now), 1.0);
        session.overlay = Overlay::None;
        session.clear_toast();
        motion.observe(&session, (false, true), now);
        assert!(motion.closing_overlay(now).is_none());
        assert!(motion.status(&session, now).leaving.is_none());
        motion.body_arrived("a", now);
        session.quotes_open.insert("a".into());
        session.recipients_open.insert("a".into());
        assert!(motion.body_alphas(now).is_empty());
        let shown = motion.disclosures(&session, now);
        assert!(shown.quotes.is_empty() && shown.recipients.is_empty());
        assert!(!motion.tick(now));
    }

    #[test]
    fn history_and_recipients_move_together_and_a_key_settles_them() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        motion.body_arrived("a", now);
        session.quotes_open.insert("b".into());
        session.recipients_open.insert("b".into());
        assert_eq!(motion.body_alphas(now)["a"], motion::ARRIVE_FROM);
        let opening = motion.disclosures(&session, now);
        assert_eq!(opening.quotes["b"], 0.0);
        assert_eq!(opening.recipients["b"], 0.0);

        let mid = later(now, 40);
        let alpha = motion.body_alphas(mid)["a"];
        assert!(alpha > motion::ARRIVE_FROM && alpha < 1.0, "{alpha}");
        assert!(motion.tick(mid));
        let shown = motion.disclosures(&session, mid);
        assert_eq!(shown.quotes, shown.recipients);
        assert!((0.0..1.0).contains(&shown.quotes["b"]));

        // The body is in after QUICK; both disclosures keep moving until SLIDE.
        let after = later(now, 90);
        assert!(motion.tick(after));
        assert!(!motion.body_alphas(after).contains_key("a"));
        assert!(motion.disclosures(&session, after).quotes["b"] > 0.9);

        let close = later(now, 130);
        assert!(!motion.tick(close));
        session.quotes_open.clear();
        session.recipients_open.clear();
        let closing = motion.disclosures(&session, close);
        assert_eq!(closing.quotes["b"], 1.0);
        assert_eq!(closing.recipients["b"], 1.0);
        let next = later(close, 20);
        let shown = motion.disclosures(&session, next);
        assert_eq!(shown.quotes, shown.recipients);
        assert!((0.0..1.0).contains(&shown.quotes["b"]));

        motion.settle(next);
        let end = motion.disclosures(&session, next);
        assert!(end.quotes.is_empty() && end.recipients.is_empty());
        assert!(!motion.tick(next));
    }
}
