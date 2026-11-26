//! The inbox window's running transitions, built on [`crate::motion`].
//!
//! Most of them start from what changed between two frames, not from the
//! code that changed it: [`InboxMotion::observe`] compares the session with
//! what the last frame drew, so every path that opens the palette, shows a
//! notice or switches panes gets the same transition. The list and the
//! scroll glides start where the view changes them, because they need the
//! positions from before the change.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui::{Pixels, Size};

use crate::keymap::KeyId;
use crate::model::{Banner, FolderIntent, Overlay, PickerChoice, Session, Toast};
use crate::motion::{self, Ghost, Glide, ListShift, Motion, Transition};
use crate::present::{ConvMessage, Pane, ThreadRow};
use crate::text_render::RenderedBody;
use crate::worker::Worker;

/// The old selection's highlight starts its fade from this much, so on a
/// slow frame two rows never look selected.
pub const SELECTION_FROM: f32 = 0.6;

const STATUS_SWAP_OUT: Duration = Duration::from_millis(60);

#[derive(Clone, Debug)]
pub struct ThreadGhost {
    pub row: ThreadRow,
    pub selected: bool,
}

/// The folder label and count leaving the header as a pair.
pub struct FolderHeaderFade {
    pub folder: String,
    pub unread: Option<usize>,
    banner: Option<Banner>,
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
    pub sync_opacity: f32,
    /// The notice on screen and its opacity.
    pub notice: Option<(Toast, f32)>,
    /// A notice leaving, fading where it was: a key cleared it, or another
    /// notice replaced it.
    pub leaving: Option<(Toast, f32)>,
    /// How much of the link destination the notice slot covers.
    pub notice_slot: f32,
    /// How far below its place the notice is drawn, rising in.
    pub notice_dy: f32,
    /// How far above its place the leaving notice is drawn, rising out.
    pub leaving_dy: f32,
    /// How much of the key hints shows. A notice takes their room, so they
    /// fade out as it comes and back in as it leaves.
    pub hints: f32,
    /// Sync text shares the notice's space on a narrow window.
    pub sync: f32,
}

struct BodyArrival {
    fade: Transition,
    height: Option<(f32, Transition)>,
    /// Whether the outgoing pending label said the body was downloading.
    placeholder: Option<bool>,
    text: Option<RenderedBody>,
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
    snooze: Option<crate::model::snooze::Picker>,
    agents: Option<crate::model::agents::Agents>,
    activity: Option<crate::model::activity::Activity>,
    palette_origin: Overlay,
    settings: Option<crate::model::settings::Nav>,
    pub folder_prompt: Cow<'static, str>,
    pub folder_target: Option<String>,
    pub picker_choices: Vec<PickerChoice>,
    pub label_marks: f32,
}

impl Layer {
    fn of(session: &Session, overlay: Overlay, label_marks: f32) -> Self {
        let picker_choices = if overlay == Overlay::Folders {
            session.picker_choices()
        } else {
            Vec::new()
        };
        Self {
            overlay,
            chord: session.chord.clone(),
            palette_query: session.palette_query.clone(),
            palette_index: session.palette_index,
            folder_query: session.folder_query.clone(),
            folder_index: session.folder_index,
            folder_intent: session.folder_intent,
            snooze: (session.overlay == Overlay::Snooze).then(|| {
                let mut picker = session.snooze.clone();
                picker.ids.clear();
                picker
            }),
            agents: (session.overlay == Overlay::Agents
                || session.palette_origin == Overlay::Agents)
                .then(|| session.agents.clone()),
            activity: (session.overlay == Overlay::AgentActivity
                || session.palette_origin == Overlay::AgentActivity)
                .then(|| session.activity.clone()),
            palette_origin: session.palette_origin.clone(),
            settings: (session.overlay == Overlay::Settings
                || session.palette_origin == Overlay::Settings)
                .then(|| session.settings_nav.clone()),
            folder_prompt: session.folder_prompt(),
            folder_target: session.folder_target(),
            picker_choices,
            label_marks,
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
        if let Some(picker) = &mut self.snooze {
            std::mem::swap(picker, &mut session.snooze);
        }
        if let Some(agents) = &mut self.agents {
            std::mem::swap(agents, &mut session.agents);
        }
        if let Some(activity) = &mut self.activity {
            std::mem::swap(activity, &mut session.activity);
        }
        if let Some(settings) = &mut self.settings {
            std::mem::swap(settings, &mut session.settings_nav);
        }
        std::mem::swap(&mut self.palette_origin, &mut session.palette_origin);
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
    pub list: Option<ListShift<ThreadGhost>>,
    /// The list viewport gliding to the selection (`Ctrl-d`, `gg`, `G`).
    pub list_glide: Option<Glide>,
    /// The conversation gliding to a page or a message.
    pub conv_glide: Option<Glide>,
    last_worker_outage: Option<Worker>,
    sync_in: Option<Transition>,
    /// Sync and notices share a cell in a narrow status bar.
    pub narrow_status: bool,
    toast_in: Option<Transition>,
    /// The notice that is leaving and its fade.
    toast_out: Option<((Toast, f32), Transition)>,
    last_toast: Option<Toast>,
    notice_slot_fade: Option<(f32, Transition)>,
    sync_fade: Option<(f32, Transition)>,
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
    /// Where the reading column was when its current slide started.
    reading_slide: Option<(f32, Transition)>,
    reading_list_fade: Option<(f32, Transition)>,
    reading_reflow: Option<([f32; 2], Transition)>,
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
    bodies: HashMap<String, BodyArrival>,
    attachment_fades: HashMap<String, (Transition, f32)>,
    /// Sent cards already laid out but hidden until the compose hand-off.
    messages_pending: HashSet<String>,
    message_fades: HashMap<String, Transition>,
    /// The pending cards are painted at their starting opacity for this viewport.
    arrival_frame: Option<((Size<Pixels>, f32), u64)>,
    arrival_frame_id: u64,
    /// The inbox's viewport before compose could tile over it.
    uncovered_size: Option<Size<Pixels>>,
    uncover_started: Option<Instant>,
    /// Where a glide a key interrupted was drawn, so the next glide starts
    /// there instead of snapping to the old target first.
    list_from: Option<f32>,
    conv_from: Option<f32>,
    /// The palette or folder picker's height: it fits its items, and eases
    /// to the new fit when typing changes how many there are.
    card_to: Option<f32>,
    card_glide: Option<Glide>,
    /// The folder and unread count from the last observed page.
    last_unread: Option<(String, Option<String>, Option<usize>)>,
    /// The count it replaced in the same folder, fading out under the new one.
    unread_fade: motion::NumberFade<Option<usize>>,
    folder_out: Option<FolderHeaderFade>,
    /// A label scope changes its heading in the same frame as its rows.
    label_header_in: Option<Transition>,
    last_banner: Option<Banner>,
    banner_out: Option<(Banner, Transition)>,
    /// The rows and reader leave before the empty message comes in.
    empty_list: Option<EmptyList>,
    label_marks: Option<Transition>,
    quotes: DisclosureMotion,
    recipients: DisclosureMotion,
}

struct EmptyList {
    exit: Transition,
    enter: Option<Transition>,
    replaces_reader: bool,
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
            last_worker_outage: None,
            sync_in: None,
            narrow_status: false,
            toast_in: None,
            toast_out: None,
            last_toast: None,
            notice_slot_fade: None,
            sync_fade: None,
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
            reading_slide: None,
            reading_list_fade: None,
            reading_reflow: None,
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
            attachment_fades: HashMap::new(),
            messages_pending: HashSet::new(),
            message_fades: HashMap::new(),
            arrival_frame: None,
            arrival_frame_id: 0,
            uncovered_size: None,
            uncover_started: None,
            quotes: DisclosureMotion::default(),
            recipients: DisclosureMotion::default(),
            last_unread: None,
            unread_fade: motion::NumberFade::default(),
            folder_out: None,
            label_header_in: None,
            last_banner: None,
            banner_out: None,
            empty_list: None,
            label_marks: None,
        }
    }

    /// Start the transitions for what changed since the last frame.
    /// `panes`: which panes this frame shows (list, conversation).
    pub fn observe(&mut self, session: &Session, panes: (bool, bool), now: Instant) {
        self.observe_notice(session.status_notice(), now);
        let start = |duration| self.motion.start(duration, now);
        if std::mem::take(&mut self.arrival_pending) {
            self.arrival = start(motion::QUICK);
        }
        let outage = session.worker_outage();
        if outage != self.last_worker_outage {
            self.last_worker_outage = outage;
            self.sync_in = start(motion::QUICK);
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

        let overlay = session.visible_overlay();
        if overlay != self.last_overlay {
            let replacing_agent_card = matches!(
                (&self.last_overlay, &overlay),
                (Overlay::Agents, Overlay::AgentActivity)
                    | (Overlay::AgentActivity, Overlay::Agents)
            );
            if overlay == Overlay::None || replacing_agent_card {
                // Gone for input at once; only its pixels fade.
                self.overlay_out = self
                    .last_layer
                    .clone()
                    .filter(|layer| layer.overlay != Overlay::None)
                    .and_then(|layer| start(motion::FADE).map(|fade| (layer, fade)));
                if replacing_agent_card {
                    self.overlay = start(motion::FADE);
                } else {
                    self.overlay = None;
                    self.card_to = self.card_height(now);
                    self.card_glide = None;
                }
            } else {
                self.overlay = start(motion::FADE);
                self.overlay_out = None;
                // A card that opens is as tall as its items at once.
                self.card_to = None;
                self.card_glide = None;
            }
            self.last_overlay = overlay.clone();
        }
        self.last_layer = (which || overlay != Overlay::None)
            .then(|| Layer::of(session, overlay, self.label_marks(now)));

        // Only a key moves the panes; a window resized across the narrow
        // threshold shows its panes at once.
        let layout = (session.pane, session.full_width);
        if layout != self.last_layout {
            if self.empty_list.is_none()
                && ((layout == (Pane::Conversation, true))
                    != (self.last_layout == (Pane::Conversation, true)))
            {
                self.reading_slide =
                    start(motion::SLIDE).map(|slide| (self.reading_progress(now), slide));
                self.reading_list_fade =
                    start(motion::FADE).map(|fade| (self.reading_list_opacity(now), fade));
                let alpha = self.reading_reflow(now).map_or_else(
                    || {
                        if self.reading_target() {
                            [0.0, 1.0]
                        } else {
                            [1.0, 0.0]
                        }
                    },
                    |reflow| reflow.alpha,
                );
                self.reading_reflow = start(motion::READING_REFLOW).map(|fade| (alpha, fade));
            }
            if panes != self.last_panes && self.empty_list.is_none() {
                let entering = match (self.last_panes, panes) {
                    ((_, false), (_, true)) => Some(Pane::Conversation),
                    // The conversation and list already move with the
                    // split as full-width reading mode opens or closes.
                    ((false, true), (true, true)) => None,
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

    /// The window owns the outgoing account picker once its inbox goes away.
    pub fn settle_account_switch(&mut self, now: Instant) -> Option<(Layer, Option<f32>)> {
        let card = self.card_height(now);
        let picker = self
            .overlay_out
            .take()
            .map(|(layer, _)| layer)
            .or_else(|| self.last_layer.take())
            .filter(|layer| {
                layer.overlay == Overlay::Folders && layer.folder_intent == FolderIntent::Account
            });
        self.last_layer = None;
        self.last_overlay = Overlay::None;
        self.last_which = false;
        self.last_chord = false;
        self.overlay = None;
        self.which = None;
        self.which_out = None;
        self.card_to = None;
        self.card_glide = None;
        picker.map(|layer| (layer, card))
    }

    pub fn reading_progress(&self, now: Instant) -> f32 {
        let target = if self.reading_target() { 1.0 } else { 0.0 };
        self.reading_slide.map_or(target, |(from, slide)| {
            motion::lerp(from, target, motion::progress(Some(&slide), now))
        })
    }

    pub fn reading_target(&self) -> bool {
        self.last_layout == (Pane::Conversation, true)
    }

    pub fn reading_sliding(&self) -> bool {
        self.reading_slide.is_some()
    }

    pub fn reading_reflow(&self, now: Instant) -> Option<motion::ReadingReflow> {
        let (from, fade) = self.reading_reflow?;
        if !fade.running(now) {
            return None;
        }
        Some(motion::ReadingReflow {
            alpha: motion::ReadingReflow::opacities(from, self.reading_target(), fade, now),
            height: self.reading_progress(now),
        })
    }

    pub fn reading_list_opacity(&self, now: Instant) -> f32 {
        let target = if self.reading_target() { 0.0 } else { 1.0 };
        self.reading_list_fade.map_or(target, |(from, fade)| {
            motion::lerp(target, from, fade.remaining(now))
        })
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
        self.attachment_fades.clear();
        self.message_fades.clear();
        self.messages_pending.clear();
        self.arrival_frame = None;
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
        let sync_leaving =
            self.narrow_status && self.last_toast.is_some() && self.sync_opacity(now) > 0.0;
        if self.toast_out.is_none() && !sync_leaving {
            self.toast_in = None;
            if self.last_toast.is_some() {
                self.notice_slot_fade = None;
            }
        }
        self.sync_in = None;
        self.hints_out = None;
        self.which = None;
        self.overlay = None;
        self.pane = None;
        self.pane_out = None;
        self.focus = None;
        self.selection_out = None;
        self.arrival = None;
        self.arrival_pending = false;
        self.finish_empty_list_entry();
        self.bodies.clear();
        self.attachment_fades.clear();
        self.message_fades.clear();
        self.messages_pending.clear();
        self.arrival_frame = None;
        self.quotes.transitions.clear();
        self.recipients.transitions.clear();
        self.unread_fade = motion::NumberFade::default();
        self.label_header_in = None;
        self.banner_out = None;
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
        running |= keep(&mut self.sync_in, now);
        // Keep the outgoing marker through a replacement so a key cannot
        // finish the incoming fade early.
        if keep(&mut self.toast_in, now) {
            running = true;
        } else {
            running |= keep_with(&mut self.toast_out, now);
        }
        running |= keep_with(&mut self.notice_slot_fade, now);
        running |= keep_with(&mut self.sync_fade, now);
        running |= keep(&mut self.hints_out, now);
        running |= keep(&mut self.which, now);
        running |= keep_with(&mut self.which_out, now);
        running |= keep(&mut self.overlay, now);
        running |= keep_with(&mut self.overlay_out, now);
        running |= keep(&mut self.focus, now);
        running |= keep(&mut self.arrival, now);
        self.bodies.retain(|_, body| {
            body.fade.running(now) || body.height.is_some_and(|(_, growth)| growth.running(now))
        });
        running |= self.quotes.tick(now);
        running |= self.recipients.tick(now);
        self.message_fades.retain(|_, fade| fade.running(now));
        self.attachment_fades
            .retain(|_, (fade, _)| fade.running(now));
        running |= !self.bodies.is_empty()
            || !self.message_fades.is_empty()
            || !self.attachment_fades.is_empty();
        if self.card_glide.is_some_and(|glide| !glide.running(now)) {
            self.card_glide = None;
        }
        running |= self.card_glide.is_some();
        running |= keep_with(&mut self.selection_out, now);
        running |= self.unread_fade.frame(now).is_some();
        if self
            .folder_out
            .as_ref()
            .is_some_and(|old| !old.fade.running(now))
        {
            self.folder_out = None;
        }
        running |= self.folder_out.is_some();
        running |= keep_with(&mut self.banner_out, now);
        running |= keep(&mut self.label_header_in, now);
        running |= keep(&mut self.label_marks, now);
        if self
            .empty_list
            .as_ref()
            .is_some_and(|list| !list.enter.unwrap_or(list.exit).running(now))
        {
            self.empty_list = None;
        }
        running |= self.empty_list.is_some();
        if self.pane.is_some_and(|(t, _)| !t.running(now)) {
            self.pane = None;
        }
        running |= self.pane.is_some();
        running |= keep_with(&mut self.pane_out, now);
        running |= keep_with(&mut self.reading_slide, now);
        running |= keep_with(&mut self.reading_list_fade, now);
        running |= keep_with(&mut self.reading_reflow, now);
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
    pub fn carried_ghosts(&mut self) -> Vec<Ghost<ThreadGhost>> {
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

    /// Keep the tail of a shrinking list sliding even beyond the old
    /// snapshot. The first unseen row starts below the clip and below the
    /// last tracked row. Giving the tail one offset keeps its rows spaced.
    pub fn batch_bottom_arrival_offset(
        viewport: f32,
        previous_bottom: f32,
        first_slot: f32,
    ) -> f32 {
        (viewport.max(previous_bottom) - first_slot).max(0.0)
    }

    /// The prefix ends at or above the clip and before the first tracked
    /// row. Every row in the prefix shares this upward starting offset.
    pub fn batch_top_arrival_offset(tracked_top: f32, tracked_slot: f32) -> f32 {
        (tracked_top.min(0.0) - tracked_slot).min(0.0)
    }

    /// Observe the folder whose page actually supplied the count.
    pub fn observe_header(
        &mut self,
        folder: &str,
        label: Option<&str>,
        unread: Option<usize>,
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
            if old_label.is_some() || label.is_some() {
                self.label_header_in = self.motion.start(motion::FADE, now);
                self.unread_fade = motion::NumberFade::default();
                *count = unread;
                return;
            }
            self.label_header_in = None;
            self.folder_out = self.motion.start(motion::FADE, now).map(|fade| {
                let mut next = FolderHeaderFade {
                    folder: old_folder,
                    unread: *count,
                    banner: self.last_banner.clone(),
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
                        next.unread = old.unread;
                        next.banner = old.banner;
                        next.folder_from *= old.folder_from;
                    }
                }
                next
            });
            self.unread_fade = motion::NumberFade::default();
        } else if *count != unread && self.folder_out.is_none() {
            self.unread_fade.change(*count, self.motion, now);
        }
        *count = unread;
    }

    pub fn folder_header(&mut self) -> Option<&mut FolderHeaderFade> {
        self.folder_out.as_mut()
    }

    pub fn label_header_opacity(&self, now: Instant) -> f32 {
        self.label_header_in.map_or(1.0, |fade| {
            motion::lerp(motion::ARRIVE_FROM, 1.0, fade.progress(now))
        })
    }

    /// Start the last-row exit and report whether the reader occupied the window.
    pub fn list_emptied(&mut self, now: Instant) -> bool {
        self.pane = None;
        self.pane_out = None;
        self.reading_slide = None;
        self.reading_list_fade = None;
        self.reading_reflow = None;
        // The snapshot owns the exit. A key arriving before the next paint
        // can land the list without starting the usual reading-mode slide.
        self.last_layout = (Pane::List, false);
        let replaces_reader = self.last_panes == (false, true);
        self.empty_list = self.motion.start(motion::FADE, now).map(|exit| EmptyList {
            exit,
            enter: self.motion.start(motion::FADE, now + motion::FADE),
            replaces_reader,
        });
        replaces_reader
    }

    pub fn finish_empty_list_entry(&mut self) {
        if let Some(list) = &mut self.empty_list {
            list.enter = None;
        }
    }

    pub fn empty_list_exit(&self, now: Instant) -> f32 {
        self.empty_list
            .as_ref()
            .map_or(0.0, |list| list.exit.remaining(now))
    }

    pub fn empty_list_pane(&self, same_scope: bool, now: Instant) -> f32 {
        self.empty_list
            .as_ref()
            .filter(|list| list.replaces_reader && same_scope)
            .and_then(|list| list.enter)
            .map_or(1.0, |enter| enter.progress(now))
    }

    pub fn reset_label_marks(&mut self) {
        self.label_marks = None;
    }

    pub fn fade_label_marks(&mut self, now: Instant) {
        self.label_marks = self.motion.start(motion::FADE, now);
    }

    pub fn label_marks(&self, now: Instant) -> f32 {
        self.label_marks.map_or(1.0, |fade| fade.progress(now))
    }

    /// Keep an empty banner in step with the scope the header is showing.
    pub fn list_banner(&mut self, banner: Option<Banner>, now: Instant) -> Option<(Banner, f32)> {
        if self.last_banner != banner {
            let old = std::mem::replace(&mut self.last_banner, banner.clone());
            self.banner_out = old
                .filter(|_| {
                    banner.is_some() && self.folder_out.is_none() && self.empty_list.is_none()
                })
                .zip(self.motion.start(motion::FADE, now));
        }
        let banner = banner?;
        if let Some(fade) = &self.folder_out {
            let (leaving, opacity) = fade.phase(now);
            if leaving {
                return fade
                    .banner
                    .clone()
                    .map(|old| (old, opacity * fade.folder_from));
            }
            return Some((banner, opacity));
        }
        if let Some((_, fade)) = &self.banner_out {
            return Some((
                banner,
                motion::lerp(motion::ARRIVE_FROM, 1., fade.progress(now)),
            ));
        }
        Some((
            banner,
            self.empty_list
                .as_ref()
                .filter(|list| !list.replaces_reader)
                .and_then(|list| list.enter)
                .map_or(1.0, |enter| enter.progress(now)),
        ))
    }

    pub fn leaving_banner(&self, now: Instant) -> Option<(Banner, f32)> {
        let (banner, fade) = self.banner_out.as_ref()?;
        fade.running(now)
            .then(|| (banner.clone(), 1. - fade.progress(now)))
    }

    /// The header's unread count being replaced: the old count and how far
    /// the new one has faded in over it.
    pub fn header_unread(&self) -> Option<usize> {
        self.last_unread.as_ref().and_then(|(_, _, count)| *count)
    }

    pub fn unread_count(&self, now: Instant) -> Option<(Option<usize>, f32)> {
        self.unread_fade
            .frame(now)
            .map(|(old, progress)| (*old, progress))
    }

    fn observe_notice(&mut self, notice: Option<&Toast>, now: Instant) {
        if notice == self.last_toast.as_ref() {
            return;
        }
        let sync = self.sync_opacity(now);
        let slot = self.notice_slot(now);
        let replacing = self.last_toast.is_some() || slot > 0.0;
        let incoming = motion::progress(self.toast_in.as_ref(), now);
        let old = self
            .last_toast
            .take()
            .map(|toast| (toast, incoming))
            .filter(|&(_, opacity)| opacity > 0.0)
            .or_else(|| {
                self.toast_out.take().and_then(|((toast, from), fade)| {
                    let opacity = from * fade.remaining(now);
                    (opacity > 0.0).then_some((toast, opacity))
                })
            });
        self.toast_in = None;
        self.toast_out = None;
        self.sync_fade = None;
        let start = |duration| self.motion.start(duration, now);
        match (notice, old) {
            (Some(toast), Some(old)) if *toast != old.0 => {
                // Narrow layouts share one cell. Finish the old text before
                // the next can rise into it, and keep sync out of the handoff.
                self.toast_out = start(STATUS_SWAP_OUT).map(|fade| (old, fade));
                self.toast_in = self.motion.start(motion::FADE, now + STATUS_SWAP_OUT);
            }
            (Some(_), Some(_)) => {
                // Repeating the same notice keeps it visible.
            }
            (Some(_), None) => {
                let (sync_out, notice_delay, notice_in) = if self.narrow_status && sync > 0.0 {
                    (STATUS_SWAP_OUT, STATUS_SWAP_OUT, motion::QUICK)
                } else {
                    (motion::FADE, Duration::ZERO, motion::FADE)
                };
                self.sync_fade = start(sync_out).map(|fade| (sync, fade));
                self.toast_in = self.motion.start(notice_in, now + notice_delay);
                self.hints_from = if self.last_chord { 0.0 } else { 1.0 };
                self.hints_out = start(motion::FADE);
            }
            (None, old) => {
                self.hints_out = None;
                let delay = if let Some(old) = old {
                    self.toast_out = start(motion::FADE).map(|fade| (old, fade));
                    motion::FADE
                } else {
                    Duration::ZERO
                };
                self.sync_fade = self
                    .motion
                    .start(motion::FADE, now + delay)
                    .map(|fade| (sync, fade));
            }
        }
        // The whole slot stays covered while notices take turns fading.
        self.notice_slot_fade = match (notice.is_some(), replacing) {
            (true, true) => None,
            (true, false) => self.toast_in.map(|fade| (0.0, fade)),
            (false, _) => start(motion::FADE)
                .filter(|_| slot > 0.0)
                .map(|fade| (slot, fade)),
        };
        self.last_toast = notice.cloned();
    }

    fn notice_slot(&self, now: Instant) -> f32 {
        if self.last_toast.is_some() {
            motion::progress(self.notice_slot_fade.as_ref().map(|(_, fade)| fade), now)
        } else {
            self.notice_slot_fade
                .map_or(0.0, |(from, fade)| from * fade.remaining(now))
        }
    }

    fn sync_opacity(&self, now: Instant) -> f32 {
        let target = if self.last_toast.is_some() { 0.0 } else { 1.0 };
        self.sync_fade.map_or(target, |(from, fade)| {
            motion::lerp(from, target, fade.progress(now))
        })
    }

    pub fn status(&self, session: &Session, now: Instant) -> StatusMotion {
        let rising = motion::progress(self.toast_in.as_ref(), now);
        let notice = session
            .status_notice()
            .cloned()
            .map(|toast| (toast, rising));
        let leaving = self
            .toast_out
            .as_ref()
            .map(|((toast, from), fade)| (toast.clone(), from * fade.remaining(now)));
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
            sync_opacity: motion::lerp(
                motion::ARRIVE_FROM,
                1.0,
                motion::progress(self.sync_in.as_ref(), now),
            ),
            notice,
            leaving,
            notice_slot: self.notice_slot(now),
            notice_dy: motion::NOTICE_RISE_PX * (1.0 - rising),
            leaving_dy: -motion::NOTICE_RISE_PX * gone,
            hints,
            sync: self.sync_opacity(now),
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
        let drop = if self.overlay_out.is_some() {
            0.0
        } else {
            motion::RISE_PX * (1.0 - p)
        };
        (p, drop)
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

    /// A list change put the selection directly on its new row.
    pub fn land_selection(&mut self, selected: Option<String>) {
        self.last_selected = selected;
        self.selection_out = None;
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

    /// Fade a body in under an already drawn header. When its card changes
    /// height, both the text and the height share the full fade.
    pub fn body_arrived(
        &mut self,
        id: &str,
        height: Option<f32>,
        placeholder: Option<bool>,
        now: Instant,
    ) {
        let duration = if height.is_some() || placeholder.is_some() {
            motion::FADE
        } else {
            motion::QUICK
        };
        self.start_body_arrival(id, height, placeholder, None, duration, now);
    }

    pub fn reader_body_arrived(
        &mut self,
        id: &str,
        height: Option<f32>,
        text: Option<RenderedBody>,
        now: Instant,
    ) {
        self.start_body_arrival(id, height, None, text, motion::FADE, now);
    }

    fn start_body_arrival(
        &mut self,
        id: &str,
        height: Option<f32>,
        placeholder: Option<bool>,
        text: Option<RenderedBody>,
        duration: Duration,
        now: Instant,
    ) {
        if !self.message_fades.contains_key(id)
            && let Some(fade) = self.motion.start(duration, now)
        {
            let height = height.map(|height| (height, fade));
            self.bodies.insert(
                id.to_string(),
                BodyArrival {
                    fade,
                    height,
                    placeholder,
                    text,
                },
            );
        }
    }

    pub fn attachments_arrived(&mut self, id: &str, with_body: bool, now: Instant) {
        let fade = if with_body && let Some(body) = self.bodies.get(id) {
            let from = if body.text.is_some() {
                0.
            } else {
                motion::ARRIVE_FROM
            };
            Some((body.fade, from))
        } else {
            self.motion
                .start(motion::FADE, now)
                .map(|fade| (fade, if with_body { motion::ARRIVE_FROM } else { 0. }))
        };
        if let Some(fade) = fade {
            self.attachment_fades.insert(id.to_owned(), fade);
        }
    }

    pub fn attachment_alphas(&self, now: Instant) -> HashMap<String, f32> {
        self.attachment_fades
            .iter()
            .map(|(id, (fade, from))| (id.clone(), motion::lerp(*from, 1., fade.progress(now))))
            .collect()
    }

    pub fn replacing_body_text(&self) -> HashMap<String, RenderedBody> {
        self.bodies
            .iter()
            .filter_map(|(id, body)| Some((id.clone(), body.text.clone()?)))
            .collect()
    }

    /// Fade in the whole card when a newly sent reply joins the open thread.
    pub fn message_arrived(&mut self, id: &str) {
        self.messages_pending.insert(id.to_string());
        self.arrival_frame = None;
    }

    pub fn has_pending_messages(&self) -> bool {
        !self.messages_pending.is_empty()
    }

    /// Wait for the inbox geometry to return before exposing the sent card.
    /// Compose can close before the compositor configures the expanded tile.
    /// A changed desktop layout may settle at a different size.
    pub fn waiting_for_uncover(
        &mut self,
        size: Size<Pixels>,
        composing: bool,
        now: Instant,
    ) -> bool {
        if composing || !self.has_pending_messages() {
            self.uncover_started = None;
            if !composing {
                self.uncovered_size = Some(size);
            }
            return false;
        }
        self.uncovered_size.is_some_and(|previous| previous != size)
            && now.duration_since(*self.uncover_started.get_or_insert(now)) < motion::FADE
    }

    /// Pin the first visible frame and return its token once per viewport.
    pub fn stage_arriving_messages(
        &mut self,
        viewport: Option<(Size<Pixels>, f32)>,
    ) -> Option<u64> {
        let Some(viewport) = viewport.filter(|_| self.has_pending_messages()) else {
            self.arrival_frame = None;
            return None;
        };
        if self.arrival_frame.is_some_and(|(last, _)| last == viewport) {
            return None;
        }
        self.arrival_frame_id = self.arrival_frame_id.wrapping_add(1);
        self.arrival_frame = Some((viewport, self.arrival_frame_id));
        Some(self.arrival_frame_id)
    }

    pub fn reveal_arriving_messages(
        &mut self,
        viewport: (Size<Pixels>, f32),
        frame: u64,
        now: Instant,
    ) -> bool {
        if self.arrival_frame != Some((viewport, frame)) {
            return false;
        }
        self.arrival_frame = None;
        if let Some(fade) = self.motion.start(motion::FADE, now) {
            for id in self.messages_pending.drain() {
                self.bodies.remove(&id);
                self.message_fades.insert(id, fade);
            }
        } else {
            self.messages_pending.clear();
        }
        true
    }

    /// Opacity of each body still settling in, by message id.
    pub fn body_alphas(&self, now: Instant) -> HashMap<String, f32> {
        self.bodies
            .iter()
            .map(|(id, body)| {
                let shown = if body.height.is_some() {
                    motion::ease_out_quad(body.fade.linear(now))
                } else {
                    body.fade.progress(now)
                };
                let alpha = if body.placeholder.is_some() || body.text.is_some() {
                    shown
                } else {
                    motion::lerp(motion::ARRIVE_FROM, 1.0, shown)
                };
                (id.clone(), alpha)
            })
            .collect()
    }

    pub fn card_alphas(&self, now: Instant) -> HashMap<String, f32> {
        let pending_alpha = match self.arrival_frame {
            Some(_) if self.motion.enabled() => motion::ARRIVE_FROM,
            Some(_) => 1.0,
            None => 0.0,
        };
        self.message_fades
            .iter()
            .map(|(id, fade)| {
                (
                    id.clone(),
                    motion::lerp(motion::ARRIVE_FROM, 1.0, fade.progress(now)),
                )
            })
            .chain(
                self.messages_pending
                    .iter()
                    .map(|id| (id.clone(), pending_alpha)),
            )
            .collect()
    }

    pub fn body_placeholders(&self) -> HashMap<String, bool> {
        self.bodies
            .iter()
            .filter_map(|(id, body)| Some((id.clone(), body.placeholder?)))
            .collect()
    }

    pub fn body_heights(&self, now: Instant) -> HashMap<String, (f32, f32)> {
        self.bodies
            .iter()
            .filter_map(|(id, body)| {
                let (from, growth) = body.height?;
                // A sync batch can grow several cards above the reader at
                // once. Use the same gentler curve as a long scroll glide.
                Some((
                    id.clone(),
                    (from, motion::ease_out_quad(growth.linear(now))),
                ))
            })
            .collect()
    }

    pub fn has_body_growth(&self) -> bool {
        self.bodies.values().any(|body| body.height.is_some())
    }

    pub fn body_growth_running(&self, now: Instant) -> bool {
        self.bodies
            .values()
            .any(|body| body.height.is_some_and(|(_, growth)| growth.running(now)))
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

    #[test]
    fn the_last_label_rows_leave_before_the_empty_message_fades_in() {
        let now = Instant::now();
        let mut motions = InboxMotion::new(Motion::new(true));
        let mut session = session();
        session.pane = Pane::Conversation;
        session.full_width = true;
        motions.observe(&session, (false, true), now);
        assert!(motions.reading_sliding());
        assert!(motions.list_emptied(now));
        session.pane = Pane::List;
        session.full_width = false;
        motions.observe(&session, (true, true), now);
        assert!(!motions.reading_sliding());
        assert!(motions.pane_swap((true, true), now).is_none());
        let banner = Banner {
            title: "No conversations in Work".into(),
            hint: String::new(),
        };
        let half = now + motion::FADE / 2;
        let entered = now + motion::FADE;
        assert_eq!(motions.empty_list_exit(now), 1.0);
        assert!(motions.empty_list_exit(half) > 0.0);
        assert_eq!(motions.empty_list_pane(true, half), 0.0);
        assert_eq!(motions.empty_list_pane(false, half), 1.0);
        assert_eq!(motions.empty_list_exit(entered), 0.0);
        assert_eq!(motions.empty_list_pane(true, entered), 0.0);
        let halfway_in = entered + motion::FADE / 2;
        assert!((0.0..1.0).contains(&motions.empty_list_pane(true, halfway_in)));
        assert_eq!(
            motions
                .list_banner(Some(banner.clone()), halfway_in)
                .unwrap()
                .1,
            1.0
        );
        assert!(motions.tick(halfway_in));
        let done = entered + motion::FADE;
        assert!(!motions.tick(done));
        assert_eq!(motions.list_banner(Some(banner), done).unwrap().1, 1.0);
    }

    #[test]
    fn slash_during_the_last_reader_exit_shows_search_immediately() {
        let now = Instant::now();
        let mut motions = InboxMotion::new(Motion::new(true));
        let mut session = session();
        session.label_scope = Some("Work".into());
        session.pane = Pane::Conversation;
        session.full_width = true;
        motions.observe(&session, (false, true), now);
        motions.list_emptied(now);
        session.pane = Pane::List;
        session.full_width = false;

        let halfway = now + motion::FADE / 2;
        let opacity = motions.empty_list_exit(halfway);
        assert!(opacity > 0.0);
        motions.settle(halfway);
        session.on_key(KeyId::Char('/'), 10);
        motions.observe(&session, (true, true), halfway);

        assert!(session.search_editing());
        assert_eq!(motions.empty_list_exit(halfway), opacity);
        assert!(motions.empty_list_exit(halfway + motion::FADE / 4) < opacity);
        assert_eq!(motions.empty_list_exit(now + motion::FADE), 0.0);
        assert_eq!(motions.empty_list_pane(false, halfway), 1.0);
        assert_eq!(motions.reading_progress(halfway), 0.0);
        assert_eq!(motions.reading_list_opacity(halfway), 1.0);
    }

    #[test]
    fn enter_during_the_last_reader_exit_leaves_the_empty_list_focused() {
        let now = Instant::now();
        let mut motions = InboxMotion::new(Motion::new(true));
        let mut session = session();
        session.label_scope = Some("Work".into());
        session.pane = Pane::Conversation;
        session.full_width = true;
        motions.observe(&session, (false, true), now);
        motions.list_emptied(now);
        session.pane = Pane::List;
        session.full_width = false;
        motions.observe(&session, (true, true), now);

        let halfway = now + motion::FADE / 2;
        let opacity = motions.empty_list_exit(halfway);
        assert!(opacity > 0.0);
        motions.settle(halfway);
        session.on_key(KeyId::Enter, 10);

        assert_eq!(session.pane, Pane::List);
        assert!(!session.full_width);
        assert_eq!(motions.empty_list_exit(halfway), opacity);
        assert!(motions.empty_list_exit(halfway + motion::FADE / 4) < opacity);
        assert_eq!(motions.empty_list_exit(now + motion::FADE), 0.0);
        assert_eq!(motions.empty_list_pane(true, halfway), 1.0);
    }

    #[test]
    fn late_picker_marks_fade_for_the_shared_duration() {
        let now = Instant::now();
        let mut motions = InboxMotion::new(Motion::new(true));
        assert_eq!(motions.label_marks(now), 1.0);
        motions.fade_label_marks(now);
        assert_eq!(motions.label_marks(now), 0.0);
        let halfway = now + motion::FADE / 2;
        motions.settle(halfway);
        assert!((0.0..1.0).contains(&motions.label_marks(halfway)));
        assert!(motions.tick(halfway));
        assert_eq!(motions.label_marks(now + motion::FADE), 1.0);
        assert!(!motions.tick(now + motion::FADE));

        let mut reduced = InboxMotion::new(Motion::new(false));
        reduced.fade_label_marks(now);
        assert_eq!(reduced.label_marks(now), 1.0);
    }

    #[test]
    fn narrow_sync_text_fades_out_before_the_first_notice_rises() {
        let now = Instant::now();
        let mut motions = InboxMotion::new(Motion::new(true));
        motions.narrow_status = true;
        let mut session = session();
        session.show_toast("Archived · u", false);
        motions.observe(&session, (true, false), now);

        let halfway = later(now, 30);
        motions.settle(halfway);
        for ms in [0, 17, 30, 59, 60, 61, 77, 139, 140] {
            let status = motions.status(&session, later(now, ms));
            let (_, opacity) = status.notice.unwrap();
            assert_eq!(status.sync > 0.0, ms < 60, "{ms} ms");
            assert_eq!(opacity > 0.0, ms > 60, "{ms} ms");
            assert_eq!(status.notice_dy == motion::NOTICE_RISE_PX, ms <= 60);
        }
        assert_eq!(
            notice_opacity(&motions, &session, later(now, 140)),
            Some(1.0)
        );
    }

    #[test]
    fn narrow_sync_text_returns_after_the_last_notice_fades_out() {
        let now = Instant::now();
        let mut motions = InboxMotion::new(Motion::new(true));
        motions.narrow_status = true;
        let mut session = session();
        assert_eq!(motions.status(&session, now).sync, 1.0);
        session.show_toast("Removed a label", false);
        motions.observe(&session, (true, false), now);
        assert_eq!(motions.status(&session, now).sync, 1.0);

        let entered = now + STATUS_SWAP_OUT + motion::QUICK;
        assert_eq!(motions.status(&session, entered).sync, 0.0);

        session.clear_toast();
        motions.observe(&session, (true, false), entered);
        assert_eq!(motions.status(&session, entered).sync, 0.0);
        let fading = motions.status(&session, entered + motion::FADE / 2);
        assert_eq!(fading.sync, 0.0);
        assert!(fading.leaving.unwrap().1 > 0.0);
        let gone = entered + motion::FADE;
        assert_eq!(motions.status(&session, gone).sync, 0.0);
        let returning = motions.status(&session, gone + motion::FADE / 2);
        assert!(returning.sync > 0.5 && returning.sync < 1.0);
        assert_eq!(motions.status(&session, gone + motion::FADE).sync, 1.0);

        let mut reduced = InboxMotion::new(Motion::new(false));
        reduced.narrow_status = true;
        session.show_toast("Removed a label", false);
        reduced.observe(&session, (true, false), now);
        assert_eq!(reduced.status(&session, now).sync, 0.0);
        session.clear_toast();
        reduced.observe(&session, (true, false), now);
        assert_eq!(reduced.status(&session, now).sync, 1.0);
    }

    fn notice_opacity(motion: &InboxMotion, session: &Session, now: Instant) -> Option<f32> {
        motion
            .status(session, now)
            .notice
            .map(|(_, opacity)| opacity)
    }

    #[test]
    fn the_first_unread_count_fades_in_with_no_outgoing_count() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let now = Instant::now();
        motion.observe_header("INBOX", None, None, now);
        assert_eq!(motion.header_unread(), None);
        motion.observe_header("INBOX", None, Some(51), now);
        assert_eq!(motion.unread_count(now), Some((None, 0.)));
        motion.observe_header("INBOX", None, Some(52), later(now, 30));
        assert_eq!(motion.header_unread(), Some(52));
        assert_eq!(
            motion.unread_count(later(now, 60)),
            Some((None, motion::ease_out(0.5)))
        );
        assert!(!motion.tick(now + motion::FADE));
        assert_eq!(motion.header_unread(), Some(52));
        assert_eq!(motion.unread_count(now + motion::FADE), None);

        let burst = later(now, 300);
        motion.observe_header("INBOX", None, Some(127), burst);
        motion.observe_header("INBOX", None, Some(207), later(burst, 40));
        assert_eq!(motion.header_unread(), Some(207));
        assert_eq!(
            motion.unread_count(later(burst, 60)),
            Some((Some(52), motion::ease_out(0.5)))
        );
        assert_eq!(motion.unread_count(burst + motion::FADE), None);
    }

    #[test]
    fn the_unread_count_cross_fades_and_a_key_settles_it() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        session.unread_count = 3;
        let now = Instant::now();
        motion.observe_header(&session.folder, None, Some(session.unread_count), now);
        assert_eq!(motion.unread_count(now), None);
        // Opened a thread: 3 → 2 in the same folder.
        session.unread_count = 2;
        motion.observe_header(&session.folder, None, Some(session.unread_count), now);
        assert_eq!(motion.unread_count(now), Some((Some(3), 0.0)));
        let (old, mid) = motion.unread_count(later(now, 60)).unwrap();
        assert_eq!(old, Some(3));
        assert!(mid > 0.5 && mid < 1.0, "{mid}");
        assert!(!motion.tick(later(now, 120)));
        assert_eq!(motion.unread_count(later(now, 120)), None);
        // A key mid-fade shows the new count alone.
        session.unread_count = 1;
        motion.observe_header(
            &session.folder,
            None,
            Some(session.unread_count),
            later(now, 200),
        );
        assert!(motion.unread_count(later(now, 210)).is_some());
        motion.settle(later(now, 210));
        assert_eq!(motion.unread_count(later(now, 210)), None);
        // On a folder switch the old name and its count fade as a pair.
        session.folder = "Archive".into();
        session.unread_count = 7;
        motion.observe_header(
            &session.folder,
            None,
            Some(session.unread_count),
            later(now, 300),
        );
        assert_eq!(motion.unread_count(later(now, 300)), None);
        let fade = motion.folder_header().unwrap();
        assert_eq!((fade.folder.as_str(), fade.unread), ("INBOX", Some(1)));
        assert_eq!(fade.label_opacities(later(now, 300)), (1.0, 0.0));
        motion.tick(later(now, 420));
        assert!(motion.folder_header().is_none());
        // Nor with motion reduced.
        let mut reduced = InboxMotion::new(Motion::new(false));
        reduced.observe_header(&session.folder, None, Some(session.unread_count), now);
        session.unread_count = 6;
        reduced.observe_header(&session.folder, None, Some(session.unread_count), now);
        assert_eq!(reduced.unread_count(now), None);
    }

    #[test]
    fn a_label_scope_hands_the_header_to_the_arriving_rows() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let now = Instant::now();
        motion.observe_header("INBOX", Some("Work"), Some(0), now);
        motion.observe_header("INBOX", None, Some(5), now);

        assert!(motion.folder_header().is_none());
        assert_eq!(motion.label_header_opacity(now), motion::ARRIVE_FROM);
        let midway = motion.label_header_opacity(now + motion::FADE / 2);
        assert!(midway > motion::ARRIVE_FROM && midway < 1.0);
        assert_eq!(motion.label_header_opacity(now + motion::FADE), 1.0);
    }

    #[test]
    fn a_switch_during_the_header_exit_keeps_the_outgoing_folder() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let now = Instant::now();
        motion.observe_header("INBOX", None, Some(3), now);
        motion.observe_header("Archive", None, Some(7), now);

        let interrupted = now + motion::FADE / 4;
        let fade = motion.folder_header().unwrap();
        let before = fade.label_opacities(interrupted);
        assert!((before.0 - 0.25).abs() <= f32::EPSILON);
        assert_eq!(before.1, 0.0);
        let account = fade.account_frame([false, true], interrupted);
        assert_eq!(account, (true, [1.0, before.0]));

        motion.observe_header("Spam", None, Some(5), interrupted);
        let fade = motion.folder_header().unwrap();
        assert_eq!((fade.folder.as_str(), fade.unread), ("INBOX", Some(3)));
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
        motion.observe_header("INBOX", None, Some(3), now);
        motion.observe_header("Archive", None, Some(7), now);

        let interrupted = now + 3 * motion::FADE / 4;
        let fade = motion.folder_header().unwrap();
        let before = fade.label_opacities(interrupted);
        assert_eq!(before.0, 0.0);
        assert!((before.1 - 0.75).abs() <= f32::EPSILON);
        let account = fade.account_frame([true, true], interrupted);
        assert_eq!(account, (false, [before.1; 2]));

        motion.observe_header("Spam", None, Some(5), interrupted);
        let fade = motion.folder_header().unwrap();
        assert_eq!((fade.folder.as_str(), fade.unread), ("Archive", Some(7)));
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
    fn a_notice_and_its_details_hint_fade_together_after_a_key() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        motion.observe(&session, (true, true), now);
        let mut notice =
            Toast::with_hint("Couldn't save attachments", "space a R".into(), " to retry");
        notice.details = true;
        session.set_toast(notice.clone());
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
        let (leaving, opacity) = mid.leaving.unwrap();
        assert_eq!(leaving, notice);
        assert!(opacity > 0.0 && opacity < 1.0);
        assert!(
            mid.hints > 0.0 && mid.hints < 1.0,
            "hints come back as it leaves"
        );
        let gone_at = shown + 2 * motion::FADE;
        assert!(!motion.tick(gone_at));
        let gone = motion.status(&session, gone_at);
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
    fn replacement_notices_use_the_status_cell_in_sequence() {
        for gap in [None, Some(50)] {
            let mut motion = InboxMotion::new(Motion::new(true));
            let mut session = session();
            let now = Instant::now();
            session.show_toast("Archived · u", false);
            motion.observe(&session, (true, true), now);

            let key = later(now, 300);
            let swap = if let Some(gap) = gap {
                session.clear_toast();
                motion.observe(&session, (true, true), key);
                let reply = later(key, gap);
                assert_eq!(motion.status(&session, reply).sync, 0.0);
                reply
            } else {
                key
            };
            session.show_toast("Undone", false);
            motion.observe(&session, (true, true), swap);

            for ms in [0, 17, 33, 59, 60, 61, 77, 179, 180, 300] {
                let status = motion.status(&session, later(swap, ms));
                let (incoming, opacity) = status.notice.unwrap();
                assert_eq!(incoming.text, "Undone");
                let leaving = status.leaving.map_or(0.0, |(old, opacity)| {
                    assert_eq!(old.text, "Archived · u");
                    opacity
                });
                assert_eq!(leaving > 0.0, ms < 60, "{ms} ms");
                assert_eq!(opacity > 0.0, ms > 60, "{ms} ms");
                assert_eq!(opacity == 1.0, ms >= 180, "{ms} ms");
                assert_eq!(status.sync, 0.0, "{ms} ms");
                assert_eq!(status.notice_dy == motion::NOTICE_RISE_PX, ms <= 60);
                assert_eq!(status.notice_dy == 0.0, ms >= 180);
            }
        }
    }

    #[test]
    fn notice_replacement_keeps_the_texts_apart() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        session.show_toast("Reply · Avery Lane", false);
        motion.observe(&session, (true, true), now);
        let swap = later(now, 200);
        session.show_toast("Sent “Field test notes”", false);
        motion.observe(&session, (true, true), swap);

        let first = motion.status(&session, swap);
        assert_eq!(first.leaving.unwrap().0.text, "Reply · Avery Lane");
        assert_eq!(first.notice.unwrap().1, 0.0);
        assert_eq!(first.hints, 0.0);
        let leaving = motion.status(&session, later(swap, 30));
        assert!(leaving.leaving.unwrap().1 > 0.0);
        assert_eq!(leaving.notice.unwrap().1, 0.0);
        let arriving = motion.status(&session, later(swap, 100));
        assert_eq!(arriving.leaving.unwrap().1, 0.0);
        assert!((0.0..1.0).contains(&arriving.notice.unwrap().1));
        assert_eq!(
            notice_opacity(&motion, &session, later(swap, 180)),
            Some(1.0)
        );
    }

    #[test]
    fn the_notice_slot_covers_link_destinations_through_replacements() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        assert_eq!(motion.status(&session, now).notice_slot, 0.0);
        session.show_toast("Copied link", false);
        motion.observe(&session, (true, true), now);
        let entering = motion.status(&session, later(now, 30));
        assert!((0.0..1.0).contains(&entering.notice_slot));
        assert_eq!(entering.notice_slot, entering.notice.unwrap().1);

        let swap = later(now, 200);
        session.show_toast("Couldn't open link", true);
        motion.observe(&session, (true, true), swap);
        let gap = motion.status(&session, later(swap, 60));
        assert_eq!(gap.notice.unwrap().1, 0.0);
        assert_eq!(gap.leaving.unwrap().1, 0.0);
        for ms in [0, 30, 59, 60, 61, 100, 180] {
            assert_eq!(motion.status(&session, later(swap, ms)).notice_slot, 1.0);
        }

        let exit = later(swap, 200);
        session.clear_toast();
        motion.observe(&session, (true, true), exit);
        assert_eq!(motion.status(&session, exit).notice_slot, 1.0);
        let leaving = motion.status(&session, later(exit, 30));
        assert!((0.0..1.0).contains(&leaving.notice_slot));
        assert_eq!(leaving.notice_slot, leaving.leaving.unwrap().1);
        assert_eq!(
            motion.status(&session, exit + motion::FADE).notice_slot,
            0.0
        );
    }

    #[test]
    fn a_sent_notice_yields_before_the_retry_notice_fades_in() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        session.show_toast("Sent “Re: Field test notes”", false);
        motion.observe(&session, (true, true), now);
        let swapped = later(now, 180);
        session.show_toast("Sent · retrying server copy", false);
        motion.observe(&session, (true, true), swapped);
        let exiting = motion.status(&session, later(swapped, 30));
        assert!(exiting.leaving.unwrap().1 > 0.0);
        assert_eq!(exiting.notice.unwrap().1, 0.0);
        assert!(motion.tick(later(swapped, 60)));
        let arriving = motion.status(&session, later(swapped, 100));
        assert_eq!(arriving.leaving.unwrap().1, 0.0);
        let (notice, opacity) = arriving.notice.unwrap();
        assert!(!notice.alert);
        assert!((0.0..1.0).contains(&opacity));
        assert_eq!(
            notice_opacity(&motion, &session, later(swapped, 180)),
            Some(1.0)
        );
    }

    #[test]
    fn the_old_tile_stays_hidden_until_the_inbox_restores_its_size() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let now = Instant::now();
        let tiled = gpui::size(gpui::px(420.), gpui::px(780.));
        let uncovered = gpui::size(gpui::px(840.), gpui::px(780.));
        assert!(!motion.waiting_for_uncover(uncovered, false, now));
        motion.message_arrived("reply");
        assert!(!motion.waiting_for_uncover(tiled, true, now));
        assert!(motion.waiting_for_uncover(tiled, false, now));
        assert!(motion.waiting_for_uncover(tiled, false, later(now, 80)));
        assert_eq!(motion.card_alphas(later(now, 80))["reply"], 0.0);
        assert!(!motion.waiting_for_uncover(uncovered, false, later(now, 80)));

        // A different arrangement of windows may give the inbox new bounds.
        assert!(!motion.waiting_for_uncover(tiled, false, later(now, 120)));
    }

    #[test]
    fn a_sent_card_stays_pinned_until_the_callback_for_its_resized_frame() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let now = Instant::now();
        let tiled = (gpui::size(gpui::px(600.), gpui::px(780.)), 1.0);
        let uncovered = (gpui::size(gpui::px(1200.), gpui::px(780.)), 1.0);
        motion.message_arrived("reply");
        assert!(motion.stage_arriving_messages(None).is_none());
        assert_eq!(motion.card_alphas(now)["reply"], 0.0);

        let old_frame = motion.stage_arriving_messages(Some(tiled)).unwrap();
        let resized_frame = motion.stage_arriving_messages(Some(uncovered)).unwrap();
        assert!(motion.stage_arriving_messages(Some(uncovered)).is_none());
        let presented = later(now, 500);
        assert!(!motion.reveal_arriving_messages(uncovered, old_frame, presented));
        assert!(!motion.tick(presented));
        assert_eq!(motion.card_alphas(presented)["reply"], motion::ARRIVE_FROM);

        let callback = later(presented, 40);
        assert!(motion.reveal_arriving_messages(uncovered, resized_frame, callback));
        assert_eq!(motion.card_alphas(callback)["reply"], motion::ARRIVE_FROM);
        let halfway = later(callback, 60);
        assert!(motion.tick(halfway));
        assert!((motion::ARRIVE_FROM..1.0).contains(&motion.card_alphas(halfway)["reply"]));
        assert!(!motion.tick(later(callback, 120)));
    }

    #[test]
    fn a_notice_that_comes_late_waits_for_the_last_one_to_leave() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        session.show_toast("Undone", false);
        motion.observe(&session, (true, true), now);
        motion.settle(now);
        let key = later(now, 300);
        session.clear_toast();
        motion.observe(&session, (true, true), key);

        let shown = later(key, 50);
        let before = motion.status(&session, shown).leaving.unwrap().1;
        session.show_toast("Archived · u", false);
        motion.observe(&session, (true, true), shown);
        let start = motion.status(&session, shown);
        assert_eq!(start.leaving.unwrap().1, before);
        assert_eq!(start.notice.unwrap().1, 0.0);
        let incoming = motion.status(&session, later(shown, 100));
        assert_eq!(incoming.leaving.unwrap().1, 0.0);
        assert!((0.0..1.0).contains(&incoming.notice.unwrap().1));
        assert_eq!(
            notice_opacity(&motion, &session, later(shown, 180)),
            Some(1.0)
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
    fn agent_cards_crossfade_at_the_same_gliding_height() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        session.overlay = Overlay::Agents;
        motion.observe(&session, (true, true), now);
        motion.fit_card(Some(300.), now);
        motion.settle(later(now, 200));

        let switched = later(now, 220);
        session.overlay = Overlay::AgentActivity;
        motion.observe(&session, (true, true), switched);
        motion.fit_card(Some(510.), switched);
        let (leaving, opacity) = motion.closing_overlay(switched).unwrap();
        assert_eq!(leaving.overlay, Overlay::Agents);
        assert_eq!(opacity, 1.);
        assert_eq!(motion.overlay(switched), (0., 0.));
        assert_eq!(motion.card_height(switched), Some(300.));

        let midpoint = later(switched, 60);
        assert!(motion.closing_overlay(midpoint).unwrap().1 > 0.);
        assert!(motion.overlay(midpoint).0 > 0.);
        assert!((300. ..510.).contains(&motion.card_height(midpoint).unwrap()));
        motion.tick(later(switched, 120));
        assert!(motion.closing_overlay(later(switched, 120)).is_none());
        assert_eq!(motion.card_height(later(switched, 120)), Some(510.));
    }

    #[test]
    fn unseen_batch_arrivals_start_at_or_below_the_bottom_edge() {
        let viewport = 200.0;
        let first_slot = 68.0;
        for (previous_bottom, start_top) in [(0.0, 200.0), (150.0, 200.0), (220.0, 220.0)] {
            let dy =
                InboxMotion::batch_bottom_arrival_offset(viewport, previous_bottom, first_slot);
            assert_eq!(first_slot + dy, start_top);

            let now = Instant::now();
            let mut shift = ListShift::start(
                &Motion::new(true),
                vec![
                    ("a".into(), 0.0),
                    ("b".into(), 68.0),
                    ("c".into(), start_top),
                    ("d".into(), start_top + 68.0),
                ],
                vec![("a", 0.0), ("b", 68.0)],
                vec![],
                viewport,
                false,
                now,
            )
            .unwrap();
            for elapsed in [0, 16, 60, 120] {
                let at = later(now, elapsed);
                let c = shift.row("c", first_slot, 68.0, None, at);
                let d = shift.row("d", first_slot + 68.0, 68.0, None, at);
                assert_eq!(c, d);
                assert_eq!(c.opacity, 1.0);
                if elapsed == 0 {
                    assert_eq!(c.dy, dy);
                } else if elapsed == 120 {
                    assert_eq!(c.dy, 0.0);
                } else {
                    assert!((0.0..dy).contains(&c.dy));
                }
            }
        }
    }

    #[test]
    fn unseen_batch_arrivals_slide_down_from_above_the_top_edge() {
        let tracked_slot = 136.0;
        for tracked_top in [-68.0, 0.0, 12.0] {
            let now = Instant::now();
            let dy = InboxMotion::batch_top_arrival_offset(tracked_top, tracked_slot);
            let mut shift = ListShift::start(
                &Motion::new(true),
                vec![
                    ("a".into(), dy),
                    ("b".into(), 68.0 + dy),
                    ("c".into(), tracked_top),
                    ("d".into(), 68.0),
                    ("e".into(), 136.0),
                ],
                vec![("d", 68.0), ("e", 136.0)],
                vec![],
                600.0,
                false,
                now,
            )
            .unwrap();
            assert!(68.0 + dy + 68.0 <= tracked_top.min(0.0));
            for elapsed in [0, 16, 60, 120] {
                let at = later(now, elapsed);
                let a = shift.row("a", 0.0, 68.0, None, at);
                let b = shift.row("b", 68.0, 68.0, None, at);
                let c = shift.row("c", tracked_slot, 68.0, None, at);
                assert_eq!(a, b);
                assert_eq!(a.opacity, 1.0);
                assert!(b.dy <= c.dy, "the incoming rows stay above the tracked row");
                if elapsed == 0 {
                    assert_eq!(a.dy, dy);
                } else if elapsed == 120 {
                    assert_eq!(a.dy, 0.0);
                } else {
                    assert!((dy..0.0).contains(&a.dy));
                }
            }
        }
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
    fn reading_mode_reverses_from_its_current_position_and_respects_reduced_motion() {
        let mut session = session();
        session.pane = Pane::Conversation;
        let now = Instant::now();
        let mut motion = InboxMotion::new(Motion::new(true));
        session.full_width = true;
        motion.observe(&session, (false, true), now);
        assert_eq!(motion.reading_progress(now), 0.0);
        let reflow = motion.reading_reflow(now).unwrap();
        assert_eq!((reflow.alpha, reflow.height), ([1.0, 0.0], 0.0));
        assert_eq!(motion.reading_reflow(later(now, 40)).unwrap().alpha[1], 0.0);
        assert_eq!(motion.reading_reflow(later(now, 60)).unwrap().alpha[0], 0.0);
        assert_eq!(motion.pane_entry(Pane::Conversation, now).dx, 0.0);
        let mid = later(now, 48);
        let drawn = motion.reading_progress(mid);
        assert!(drawn > 0.0 && drawn < 1.0);
        let alpha = motion.reading_reflow(mid).unwrap().alpha;
        assert!(alpha.iter().all(|alpha| *alpha > 0.0 && *alpha < 1.0));

        motion.settle(mid);
        session.full_width = false;
        motion.observe(&session, (true, true), mid);
        assert_eq!(motion.reading_progress(mid), drawn);
        assert_eq!(motion.reading_reflow(mid).unwrap().alpha, alpha);
        assert_eq!(
            motion.reading_reflow(mid + motion::SLIDE).unwrap().height,
            0.0
        );
        assert!(
            motion
                .reading_reflow(mid + motion::READING_REFLOW)
                .is_none()
        );
        assert_eq!(motion.reading_progress(mid + motion::SLIDE), 0.0);

        let mut reduced = InboxMotion::new(Motion::new(false));
        session.full_width = true;
        reduced.observe(&session, (false, true), now);
        assert_eq!(reduced.reading_progress(now), 1.0);
        assert!(reduced.reading_reflow(now).is_none());
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
        motion.body_arrived("a", None, None, now);
        session.quotes_open.insert("a".into());
        session.recipients_open.insert("a".into());
        assert!(motion.body_alphas(now).is_empty());
        let shown = motion.disclosures(&session, now);
        assert!(shown.quotes.is_empty() && shown.recipients.is_empty());
        assert!(!motion.tick(now));
    }

    #[test]
    fn reader_body_fades_in_without_clipping_an_empty_card() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let now = Instant::now();
        motion.reader_body_arrived("a", None, None, now);
        assert_eq!(motion.body_alphas(now)["a"], motion::ARRIVE_FROM);
        assert!(motion.body_heights(now).is_empty());
        assert!(motion.body_alphas(later(now, 100))["a"] < 1.0);
        assert_eq!(motion.body_alphas(later(now, 120))["a"], 1.0);
    }

    #[test]
    fn history_and_recipients_move_together_and_a_key_settles_them() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        motion.body_arrived("a", None, None, now);
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
