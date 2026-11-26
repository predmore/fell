//! Conversation policy and snapshots for received HTML.

use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use fell_render::{RenderMode, WebMail, WebTextStyle};
use gpui::{
    AnyElement, Context, ElementId, FontWeight, InteractiveElement, IntoElement, ParentElement,
    Styled, Window, div, prelude::*, px, rgb,
};

use super::{ConvMessage, FontOpt, Inbox, Pane, Rich, Theme};
use crate::keymap::{self, Action};
use crate::mail_body::HtmlBody;
use crate::model::Session;
use crate::motion::{self, Motion, Transition};
use crate::render::remote_images;
use crate::render::webview::{self, Snapshot, WebRequest};

const CACHE_SIZE: usize = 12;
const THREAD_HOLD: Duration = Duration::from_millis(150);
const SNAPSHOT_BATCH: Duration = Duration::from_millis(550);
const RENDERER_EXIT: Duration = Duration::from_millis(60);
const RENDERER_CHANGE: Duration = Duration::from_millis(140);
const PROMPT_ENTER: Duration = Duration::from_millis(80);
const PROMPT_SWAP: Duration = RENDERER_EXIT.saturating_add(PROMPT_ENTER);
const RETRY_MINIMUM: Duration = Duration::from_millis(300);
const MEASURED_HEIGHTS: usize = 4 * CACHE_SIZE;
const PROMPT_HEIGHT: f32 = 30.;
const PROMPT_GAP: f32 = 8.;
const COMPACT_PROMPT_WIDTH: f32 = 560.;
const COMPACT_FAILURE_WIDTH: f32 = 480.;

#[derive(Default)]
pub(super) struct Reader {
    pub trusted_delivery: bool,
    pub preferences: HashMap<String, (Option<bool>, bool)>,
    allowed_once: HashMap<String, (Option<String>, Weak<WebMail>)>,
    show_images_for: HashMap<String, (Option<String>, Weak<WebMail>)>,
    show_native_for: HashSet<String>,
    pending: Option<Pending>,
    cache: VecDeque<Cached>,
    measured: VecDeque<MeasuredHeight>,
    opening: Option<Opening>,
    shown: HashMap<String, Shown>,
    retired: Vec<Arc<gpui::RenderImage>>,
}

struct Opening {
    started: Instant,
    cards: HashSet<String>,
    has_layout: bool,
}

struct MeasuredHeight {
    id: String,
    mail: Weak<WebMail>,
    width: i32,
    images: bool,
    text: Option<WebTextStyle>,
    height: f32,
}

impl MeasuredHeight {
    fn matches(&self, key: &Key) -> bool {
        self.id == key.id
            && self.width == key.width
            && self.images == key.images
            && self.text == key.text
            && Weak::ptr_eq(&self.mail, &Arc::downgrade(&key.mail))
    }
}

#[derive(Clone)]
struct Key {
    id: String,
    sender: Option<String>,
    mail: Arc<WebMail>,
    width: i32,
    scheme: &'static str,
    background: u32,
    text: Option<WebTextStyle>,
    images: bool,
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.same_page(other) && self.images == other.images
    }
}

impl Key {
    fn same_page(&self, other: &Self) -> bool {
        self.id == other.id
            && Arc::ptr_eq(&self.mail, &other.mail)
            && self.width == other.width
            && self.scheme == other.scheme
            && self.background == other.background
            && self.text == other.text
    }
}

struct Cached {
    key: Key,
    picture: Option<Snapshot>,
    image_failure: Option<remote_images::UnavailableImages>,
}

struct Pending {
    key: Key,
    allowed: Arc<AtomicBool>,
    _task: gpui::Task<()>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.allowed.store(false, Ordering::Release);
    }
}

struct Shown {
    mail: Weak<WebMail>,
    picture_key: Option<Key>,
    body: Body,
    outgoing: Option<Body>,
    fade: Option<Transition>,
    renderer_painted: Option<Rc<Cell<Option<Instant>>>>,
    snapshot_geometry: Rc<Cell<Option<(f32, f32)>>>,
    continue_at: Option<f32>,
    height: Rc<Cell<Option<f32>>>,
    from_height: Option<f32>,
    placeholder_height: f32,
    prompt: Option<Prompt>,
    outgoing_prompts: Vec<(Prompt, f32)>,
    prompt_from: f32,
    prompt_height_from: f32,
    prompt_fade: Option<Transition>,
    prompt_swap: Option<motion::LayerSwap>,
    retry_until: Option<Instant>,
    reserves_tag: bool,
    image_consent: bool,
}

impl Shown {
    fn new(
        mail: Weak<WebMail>,
        image: Option<Snapshot>,
        waiting: bool,
        stored: bool,
        prompt: Option<Prompt>,
    ) -> Self {
        let body = image.map_or(
            if waiting {
                Body::Placeholder
            } else if stored {
                Body::StoredText
            } else {
                Body::Native
            },
            Body::Web,
        );
        Self {
            mail,
            picture_key: None,
            body,
            outgoing: None,
            fade: None,
            renderer_painted: None,
            snapshot_geometry: Rc::default(),
            continue_at: None,
            height: Rc::default(),
            from_height: None,
            placeholder_height: 100.,
            prompt,
            outgoing_prompts: Vec::new(),
            prompt_from: 0.,
            prompt_height_from: 0.,
            prompt_fade: None,
            prompt_swap: None,
            retry_until: None,
            reserves_tag: false,
            image_consent: false,
        }
    }

    fn update(
        &mut self,
        body: Body,
        prompt: Option<Prompt>,
        web: bool,
        motion: Motion,
        now: Instant,
    ) {
        self.reserves_tag |= web;
        if !self.body.same_as(&body) {
            let renderer_change = self.body.is_web() != body.is_web();
            self.outgoing = Some(std::mem::replace(&mut self.body, body));
            self.from_height = self.height.get();
            self.renderer_painted = (renderer_change && motion.enabled()).then(Rc::default);
            self.fade = (!renderer_change)
                .then(|| motion.start(motion::FADE, now))
                .flatten();
        }
        if !motion.enabled()
            || (self.renderer_painted.is_none() && self.fade.is_none_or(|fade| !fade.running(now)))
        {
            self.fade = None;
            self.renderer_painted = None;
            self.outgoing = None;
            self.from_height = None;
        }
        if self.prompt != prompt {
            // A quick download can finish before its loading label has faded in.
            // Continue from the opacities on screen when that happens.
            let progress = motion::progress(self.prompt_fade.as_ref(), now);
            self.prompt_height_from = self.prompt_height(progress);
            let [incoming, outgoing] = self.prompt_opacities(progress, now);
            let prior = std::mem::replace(&mut self.prompt, prompt);
            for (_, alpha) in &mut self.outgoing_prompts {
                *alpha *= outgoing;
            }
            if let Some(prior) = prior {
                self.outgoing_prompts.push((prior, incoming));
            }
            self.prompt_from = 0.;
            self.outgoing_prompts.retain(|(label, alpha)| {
                if Some(label) == self.prompt.as_ref() {
                    self.prompt_from = *alpha;
                    return false;
                }
                *alpha > 0.0
            });
            let swaps_labels = self.prompt.is_some() && !self.outgoing_prompts.is_empty();
            self.prompt_swap = swaps_labels
                .then(|| motion::LayerSwap::start(motion, now, RENDERER_EXIT, PROMPT_ENTER))
                .flatten();
            self.prompt_fade = motion.start(
                if swaps_labels {
                    PROMPT_SWAP
                } else {
                    motion::FADE
                },
                now,
            );
        }
        if !motion.enabled() || self.prompt_fade.is_none_or(|fade| !fade.running(now)) {
            self.prompt_fade = None;
            self.prompt_swap = None;
            self.outgoing_prompts.clear();
        }
    }

    fn prompt_height(&self, progress: f32) -> f32 {
        motion::lerp(
            self.prompt_height_from,
            f32::from(u8::from(self.prompt.is_some())),
            progress,
        )
    }

    fn prompt_opacities(&self, progress: f32, now: Instant) -> [f32; 2] {
        if let Some(swap) = self.prompt_swap {
            return match swap.frame(now) {
                Some(motion::SwapFrame::Leaving(alpha)) => [0., alpha],
                Some(motion::SwapFrame::Arriving(alpha)) => [alpha, 0.],
                None => [1., 0.],
            };
        }
        [motion::lerp(self.prompt_from, 1., progress), 1. - progress]
    }

    fn start_painted_fade(&mut self, motion: Motion, now: Instant, real: Instant) {
        if let Some(painted) = self
            .renderer_painted
            .as_ref()
            .and_then(|painted| painted.get())
        {
            let elapsed = real
                .saturating_duration_since(painted)
                .min(motion::MAX_STEP);
            self.fade = motion.start(RENDERER_CHANGE, now - elapsed);
            self.renderer_painted = None;
        }
    }

    fn frame(&self, now: Instant) -> Frame {
        let linear = if self.renderer_painted.is_some() {
            0.
        } else {
            self.fade.map_or(1., |fade| fade.linear(now))
        };
        let progress = motion::ease_out(linear);
        let body_alpha = match &self.outgoing {
            Some(outgoing) if outgoing.is_web() != self.body.is_web() => {
                let exit = RENDERER_EXIT.as_secs_f32() / RENDERER_CHANGE.as_secs_f32();
                [
                    (1. - linear / exit).max(0.),
                    motion::ease_out((linear - exit) / (1. - exit)),
                ]
            }
            Some(Body::Placeholder) => [(1. - progress / motion::ease_out(0.4)).max(0.), progress],
            _ => [1., progress],
        };
        let leaving_clipped = matches!(&self.outgoing, Some(Body::Web(picture)) if picture.clipped)
            && matches!(self.body, Body::Native);
        let height_progress = if leaving_clipped {
            // Change both the height and the reader's position at the fade seam.
            f32::from(u8::from(body_alpha[0] == 0.))
        } else {
            motion::ease_out_quad(linear)
        };
        let current_tag = f32::from(u8::from(self.body.is_web()));
        let previous_tag = self
            .outgoing
            .as_ref()
            .map_or(current_tag, |body| f32::from(u8::from(body.is_web())));
        let prompt_progress = motion::progress(self.prompt_fade.as_ref(), now);
        let [incoming_alpha, outgoing_alpha] = self.prompt_opacities(prompt_progress, now);
        Frame {
            body: self.body.clone(),
            outgoing: self.outgoing.clone(),
            body_alpha,
            height_progress,
            fading: linear < 1.,
            leaving_clipped,
            renderer_painted: self.renderer_painted.clone(),
            snapshot_geometry: self.snapshot_geometry.clone(),
            continuation: None,
            prompt: (self.prompt.is_some() || self.prompt_fade.is_some()).then(|| PromptFrame {
                incoming: self.prompt.clone(),
                incoming_alpha,
                outgoing: self
                    .outgoing_prompts
                    .iter()
                    .map(|(label, alpha)| (label.clone(), alpha * outgoing_alpha))
                    .collect(),
                height: self.prompt_height(prompt_progress),
                progress: prompt_progress,
            }),
            web_tag: self
                .reserves_tag
                .then(|| motion::lerp(previous_tag, current_tag, progress)),
            height: Some(self.height.clone()),
            from_height: self.from_height,
            placeholder_height: self.placeholder_height,
            image_consent: self.image_consent,
        }
    }

    fn holds(&self, image: &gpui::RenderImage) -> bool {
        std::iter::once(&self.body)
            .chain(&self.outgoing)
            .any(|body| matches!(body, Body::Web(picture) if picture.image.id == image.id))
    }
}

#[derive(Clone)]
pub(super) enum Body {
    Placeholder,
    StoredText,
    Native,
    Web(Snapshot),
}

impl Body {
    fn same_as(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Placeholder, Self::Placeholder)
            | (Self::StoredText, Self::StoredText)
            | (Self::Native, Self::Native) => true,
            (Self::Web(a), Self::Web(b)) => a.image.id == b.image.id,
            _ => false,
        }
    }

    fn is_web(&self) -> bool {
        matches!(self, Self::Web(_) | Self::Placeholder)
    }
}

pub(super) struct Frame {
    pub body: Body,
    pub outgoing: Option<Body>,
    pub body_alpha: [f32; 2],
    pub height_progress: f32,
    fading: bool,
    pub leaving_clipped: bool,
    pub renderer_painted: Option<Rc<Cell<Option<Instant>>>>,
    pub snapshot_geometry: Rc<Cell<Option<(f32, f32)>>>,
    pub continuation: Option<crate::scroll_anchor::Continuation>,
    pub prompt: Option<PromptFrame>,
    pub web_tag: Option<f32>,
    pub height: Option<Rc<Cell<Option<f32>>>>,
    pub from_height: Option<f32>,
    pub placeholder_height: f32,
    pub image_consent: bool,
}

impl Frame {
    pub(super) fn body_animating(&self) -> bool {
        self.height_progress < 1.0
    }

    pub(super) fn owns_body(&self) -> bool {
        !matches!(self.body, Body::Native) || self.outgoing.is_some() || self.continuation.is_some()
    }

    pub(super) fn animating(&self) -> bool {
        self.fading
            || self
                .prompt
                .as_ref()
                .is_some_and(|prompt| prompt.progress < 1.0)
    }
}

pub(super) struct PromptFrame {
    incoming: Option<Prompt>,
    incoming_alpha: f32,
    outgoing: Vec<(Prompt, f32)>,
    height: f32,
    progress: f32,
}

#[derive(Clone, PartialEq, Eq)]
enum Prompt {
    Blocked {
        verified: bool,
    },
    Loading,
    Retrying,
    AllowedOnce,
    AllowedSender,
    Native {
        always: bool,
    },
    Unavailable {
        verified: bool,
        always: bool,
        images: bool,
        failure: Option<remote_images::UnavailableImages>,
    },
}

pub(super) fn sender_address(address: &str) -> Option<String> {
    let (local, domain) = address.rsplit_once('@')?;
    if address.len() > 254
        || local.is_empty()
        || local.len() > 64
        || domain.is_empty()
        || local.contains('@')
        || address
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
        || !domain
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
    {
        return None;
    }
    Some(format!("{local}@{}", domain.to_ascii_lowercase()))
}

impl Reader {
    pub(super) fn open_thread(
        &mut self,
        messages: &[ConvMessage],
        motion: Motion,
        real: Instant,
        cx: &mut Context<Inbox>,
    ) -> bool {
        self.opening = None;
        if !motion.enabled() {
            return false;
        }
        let cards: HashSet<_> = messages.iter().rev()
            .filter(|message| message.expanded)
            .take(4)
            .filter(|message| matches!(&message.rich, Rich::Html(body)
                if body.web.source_bytes() <= webview::LARGE_HTML_BYTES && self.prefers_message_web(message)))
            .map(|message| message.id.clone())
            .collect();
        if cards.is_empty() {
            return false;
        }
        self.opening = Some(Opening {
            started: real,
            cards,
            has_layout: false,
        });
        cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            for deadline in [real + THREAD_HOLD, real + SNAPSHOT_BATCH] {
                executor
                    .timer(deadline.saturating_duration_since(Instant::now()))
                    .await;
                let _ = this.update(cx, |_, cx| cx.notify());
            }
        })
        .detach();
        true
    }

    pub(super) fn holding_arrival(&self, real: Instant) -> bool {
        self.opening
            .as_ref()
            .is_some_and(|open| real.saturating_duration_since(open.started) < THREAD_HOLD)
    }

    pub(super) fn release_opening(&mut self) {
        self.opening = None;
    }

    fn known_sender(&self, address: Option<&str>) -> (Option<bool>, bool) {
        address
            .and_then(|sender| self.preferences.get(sender).copied())
            .unwrap_or_default()
    }

    fn verified(&self, body: &HtmlBody, sender: Option<&str>) -> bool {
        self.trusted_delivery
            && sender.is_some()
            && body
                .sender_attestation
                .as_deref()
                .and_then(sender_address)
                .as_deref()
                == sender
    }

    fn once(&self, message: &ConvMessage, body: &HtmlBody) -> bool {
        self.allowed_once
            .get(&message.id)
            .is_some_and(|(_, mail)| Weak::ptr_eq(mail, &Arc::downgrade(&body.web)))
    }

    fn key(&self, message: &ConvMessage, body: &HtmlBody, theme: &Theme, width: i32) -> Key {
        let sender = sender_address(&message.email);
        let images = body.web.has_remote_images()
            && (self.once(message, body)
                || self.verified(body, sender.as_deref())
                    && self.known_sender(sender.as_deref()).1);
        let scheme = if theme.is_light() { "light" } else { "dark" };
        let (scheme, background) = body.web.appearance(scheme, theme.surface);
        Key {
            id: message.id.clone(),
            sender,
            mail: body.web.clone(),
            width,
            scheme,
            background,
            text: (!body.web.has_author_colors()).then(|| WebTextStyle {
                family: theme.ui_family.clone(),
                size: theme.ui_px,
                foreground: theme.foreground,
                link: theme.accent,
                inset: body.suggested == RenderMode::Web,
            }),
            images,
        }
    }

    fn prefers_web(&self, id: &str, sender: Option<&str>, body: &HtmlBody) -> bool {
        !self.show_native_for.contains(id)
            && (self
                .show_images_for
                .get(id)
                .is_some_and(|(_, mail)| Weak::ptr_eq(mail, &Arc::downgrade(&body.web)))
                || self
                    .known_sender(sender)
                    .0
                    .unwrap_or(body.suggested == RenderMode::Web))
    }

    pub(super) fn close_thread(&mut self) {
        self.show_native_for.clear();
        for shown in self.shown.values_mut() {
            shown.continue_at = None;
        }
    }

    fn continue_in_native(
        &mut self,
        id: &str,
        viewport: gpui::Bounds<gpui::Pixels>,
        line_height: f32,
    ) -> bool {
        let Some(shown) = self.shown.get_mut(id) else {
            return false;
        };
        let Body::Web(picture) = &shown.body else {
            return false;
        };
        if !picture.clipped {
            return false;
        }
        let Some((top, height)) = shown.snapshot_geometry.get() else {
            return false;
        };
        let row_top = top + height;
        if f32::from(viewport.bottom()) < row_top
            || f32::from(viewport.top()) >= row_top + crate::render::CLIP_NOTICE_HEIGHT
        {
            return false;
        }
        let snapshot_fraction =
            (1. - (crate::render::CLIP_FADE_HEIGHT + line_height) / height).clamp(0., 1.);
        let fraction =
            snapshot_fraction * webview::SNAPSHOT_HEIGHT_LIMIT as f32 / picture.document_height;
        shown.continue_at = Some(fraction);
        self.show_native_for.insert(id.to_owned());
        true
    }

    fn clear_native_override(&mut self, id: &str) {
        self.show_native_for.remove(id);
        if let Some(shown) = self.shown.get_mut(id) {
            shown.continue_at = None;
        }
    }

    fn show_web_for(&mut self, id: String, sender: Option<String>, body: &HtmlBody) {
        self.clear_native_override(&id);
        self.show_images_for
            .retain(|_, (_, mail)| mail.strong_count() > 0);
        self.show_images_for
            .insert(id, (sender, Arc::downgrade(&body.web)));
    }

    pub(super) fn prefers_message_web(&self, message: &ConvMessage) -> bool {
        match &message.rich {
            Rich::PendingWeb => true,
            Rich::Html(body) => {
                self.prefers_web(&message.id, sender_address(&message.email).as_deref(), body)
            }
            _ => false,
        }
    }

    fn entry(&self, key: &Key) -> Option<&Cached> {
        self.cache.iter().find(|cached| &cached.key == key)
    }

    fn image(&self, key: &Key) -> Option<(Snapshot, Key)> {
        self.entry(key)
            .and_then(|cached| Some((cached.picture.clone()?, cached.key.clone())))
            .or_else(|| {
                // Only a blocked frame may be reused across permission changes.
                if !key.images {
                    return None;
                }
                self.cache.iter().find_map(|cached| {
                    if !cached.key.images && cached.key.same_page(key) {
                        Some((cached.picture.clone()?, cached.key.clone()))
                    } else {
                        None
                    }
                })
            })
    }

    fn evict_sender(&mut self, sender: &str, cx: &mut Context<Inbox>) {
        self.pending.take();
        self.shown.clear();
        self.cache.retain(|cached| {
            let remove = cached.key.images && cached.key.sender.as_deref() == Some(sender);
            if remove && let Some(picture) = &cached.picture {
                cx.drop_image(picture.image.clone(), None);
            }
            !remove
        });
    }

    fn retry_images(&mut self, id: &str, images: bool) {
        self.cache.retain(|cached| {
            let retry = cached.key.id == id
                && cached.key.images == images
                && (cached.picture.is_none() || cached.image_failure.is_some());
            if retry && let Some(picture) = &cached.picture {
                self.retired.push(picture.image.clone());
            }
            !retry
        });
    }

    fn prune_cache(&mut self, wanted: &[Key], mut release: impl FnMut(Arc<gpui::RenderImage>)) {
        self.retired.retain(|image| {
            let held = self.shown.values().any(|shown| shown.holds(image));
            if !held {
                release(image.clone());
            }
            held
        });
        while self.cache.len() > CACHE_SIZE {
            let evict = self.cache.iter().position(|cached| {
                !wanted.contains(&cached.key)
                    && cached.picture.as_ref().is_none_or(|picture| {
                        !self.shown.values().any(|shown| shown.holds(&picture.image))
                    })
            });
            let Some(evict) = evict else { break };
            if let Some(picture) = self.cache.remove(evict).and_then(|cached| cached.picture) {
                release(picture.image);
            }
        }
    }

    pub(super) fn stop(&mut self) {
        self.pending = None;
        self.shown.clear();
        self.close_thread();
        self.release_opening();
    }

    pub(super) fn release(&mut self, app: &mut gpui::App) {
        self.pending.take();
        self.shown.clear();
        for cached in self.cache.drain(..) {
            if let Some(picture) = cached.picture {
                app.drop_image(picture.image, None);
            }
        }
        for image in self.retired.drain(..) {
            app.drop_image(image, None);
        }
    }

    fn start(&mut self, key: Key, cx: &mut Context<Inbox>) {
        self.start_with(key, cx, webview::render);
    }

    fn start_with<F: Future<Output = webview::RenderResult> + 'static>(
        &mut self,
        key: Key,
        cx: &mut Context<Inbox>,
        render: impl FnOnce(WebRequest) -> F + 'static,
    ) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.key == key)
        {
            return;
        }
        self.pending = None;
        let allowed = Arc::new(AtomicBool::new(key.images));
        let permission = allowed.clone();
        let fetch = key.clone();
        let install = key.clone();
        let task = cx.spawn(async move |this, cx| {
            let width = fetch.width;
            let background = fetch.background;
            let (html, image_failure) = cx
                .background_spawn(async move {
                    if fetch.images {
                        let (html, result) = remote_images::inline_page(
                            &fetch.mail,
                            fetch.scheme,
                            fetch.text.as_ref(),
                            || permission.load(Ordering::Acquire),
                        );
                        (html, result.err())
                    } else {
                        (
                            fetch
                                .mail
                                .page_with_style(fetch.scheme, fetch.text.as_ref(), |_| None),
                            None,
                        )
                    }
                })
                .await;
            let picture = render(WebRequest {
                html,
                width,
                background,
            })
            .await;
            let _ = this.update(cx, |inbox, cx| {
                let reader = &mut inbox.web_mail;
                if reader
                    .pending
                    .as_ref()
                    .is_none_or(|pending| pending.key != install)
                {
                    return;
                }
                reader.pending = None;
                if let Ok(picture) = picture {
                    if picture.is_some()
                        && let Some(log) = &inbox.frames
                    {
                        log.borrow_mut().note("web_snapshot", 1.0);
                    }
                    reader.cache.push_back(Cached {
                        key: install,
                        picture,
                        image_failure,
                    });
                }
                cx.notify();
            });
        });
        self.pending = Some(Pending {
            key,
            allowed,
            _task: task,
        });
    }

    fn snapshot_key(&self, message: &ConvMessage, theme: &Theme, width: i32) -> Option<Key> {
        let Rich::Html(body) = &message.rich else {
            return None;
        };
        let key = self.key(message, body, theme, width);
        self.prefers_web(&key.id, key.sender.as_deref(), body)
            .then_some(key)
    }

    pub(super) fn prefetch(
        &mut self,
        message: Option<&ConvMessage>,
        theme: &Theme,
        width: f32,
        cx: &mut Context<Inbox>,
    ) {
        self.shown.clear();
        self.close_thread();
        let width = width.round().clamp(1., 1600.) as i32;
        let key = message
            .and_then(|message| self.snapshot_key(message, theme, width))
            .map(|key| Key {
                images: false,
                ..key
            });
        let keys: Vec<_> = key.into_iter().collect();
        self.request_snapshot(&keys, cx);
        self.prune_cache(&keys, |image| cx.drop_image(image, None));
    }

    fn next_snapshot<'a>(&self, keys: &'a [Key]) -> Option<&'a Key> {
        // A failed entry is available for explicit Retry. Keep servicing
        // subsequent cards without asking WebKit to repeat the failed load.
        keys.iter().find(|key| self.entry(key).is_none())
    }

    fn request_snapshot(&mut self, keys: &[Key], cx: &mut Context<Inbox>) {
        if let Some(key) = self.next_snapshot(keys) {
            self.start(key.clone(), cx);
        } else {
            self.pending = None;
        }
    }

    pub(super) fn frames(
        &mut self,
        messages: &[ConvMessage],
        (cursor, visible): (Option<usize>, &[bool]),
        theme: &Theme,
        width: f32,
        (motion, now, real): (Motion, Instant, Instant),
        cx: &mut Context<Inbox>,
    ) -> HashMap<String, Frame> {
        let Some(cursor) = cursor else {
            // The list selection has moved, but the store hasn't returned its
            // conversation yet. Keep the previous reader intact until then.
            self.pending = None;
            return self
                .shown
                .iter()
                .map(|(id, shown)| (id.clone(), shown.frame(now)))
                .collect();
        };
        let width = width.round().clamp(1., 1600.) as i32;
        let mut native_keys = Vec::new();
        let mut queue: Vec<_> = messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message.expanded)
            .filter_map(|(index, message)| {
                let Rich::Html(body) = &message.rich else {
                    return None;
                };
                let key = self.key(message, body, theme, width);
                if !self.prefers_web(&key.id, key.sender.as_deref(), body) {
                    if self.once(message, body) {
                        native_keys.push(key);
                    }
                    return None;
                }
                Some((
                    index != cursor,
                    !visible.get(index).copied().unwrap_or(false),
                    index,
                    key,
                ))
            })
            .collect();
        queue.sort_by_key(|(other, unseen, index, _)| (*other, *unseen, *index));
        let mut keys: Vec<_> = queue.into_iter().map(|(_, _, _, key)| key).collect();
        self.request_snapshot(&keys, cx);
        if let Some(open) = &mut self.opening
            && !open.has_layout
            && visible.contains(&true)
        {
            open.has_layout = true;
            open.cards.retain(|id| {
                messages
                    .iter()
                    .zip(visible)
                    .enumerate()
                    .any(|(index, (message, visible))| {
                        *id == message.id && (*visible || index == cursor)
                    })
            });
        }
        if self.opening.as_ref().is_some_and(|open| {
            real.saturating_duration_since(open.started) >= SNAPSHOT_BATCH
                || keys
                    .iter()
                    .filter(|key| open.cards.contains(&key.id))
                    .all(|key| self.entry(key).is_some())
        }) {
            self.release_opening();
        }
        for shown in self.shown.values() {
            if let (Some(key), Body::Web(picture), None) =
                (&shown.picture_key, &shown.body, shown.fade)
            {
                // The placeholder lays out the link column itself.
                let height = picture.height * key.width as f32 / picture.width
                    + if picture.clipped {
                        crate::render::CLIP_NOTICE_HEIGHT
                    } else {
                        0.
                    };
                if let Some(measured) = self
                    .measured
                    .iter_mut()
                    .find(|measured| measured.matches(key))
                {
                    measured.height = height;
                } else {
                    if self.measured.len() == MEASURED_HEIGHTS {
                        self.measured.pop_front();
                    }
                    self.measured.push_back(MeasuredHeight {
                        id: key.id.clone(),
                        mail: Arc::downgrade(&key.mail),
                        width: key.width,
                        images: key.images,
                        text: key.text.clone(),
                        height,
                    });
                }
            }
        }
        let mut frames = HashMap::new();
        for message in messages.iter().filter(|message| message.expanded) {
            let Rich::Html(body) = &message.rich else {
                continue;
            };
            let key = self.key(message, body, theme, width);
            let web = self.prefers_web(&key.id, key.sender.as_deref(), body);
            let grouped = self
                .opening
                .as_ref()
                .is_some_and(|open| open.cards.contains(&key.id));
            let entry = web.then(|| self.entry(&key)).flatten();
            let retry_until = web
                .then(|| self.retry_until(&key))
                .flatten()
                .filter(|until| real < *until || entry.is_none());
            let failed =
                retry_until.is_none() && entry.is_some_and(|entry| entry.picture.is_none());
            let retrying = retry_until.is_some();
            let image = (web && !failed && !grouped && !retrying)
                .then(|| self.image(&key))
                .flatten();
            let large = key.mail.source_bytes() > webview::LARGE_HTML_BYTES;
            let wait = web && !failed && !large;
            let stored = web && !failed && large;
            let placeholder_height = (wait && image.is_none()).then(|| {
                self.measured
                    .iter()
                    .find(|measured| measured.matches(&key))
                    .map_or_else(
                        || {
                            let height = body
                                .web
                                .estimated_height_with_style(width as f32, key.text.as_ref())
                                .max(48.);
                            if height >= webview::SNAPSHOT_HEIGHT_LIMIT as f32 {
                                height + crate::render::CLIP_NOTICE_HEIGHT
                            } else {
                                height
                            }
                        },
                        |measured| measured.height,
                    )
            });
            let verified = self.verified(body, key.sender.as_deref());
            let always = verified && self.known_sender(key.sender.as_deref()).1;
            let prompt = if retrying {
                Some(Prompt::Retrying)
            } else {
                self.image_prompt(&key, web, verified, always)
            };

            let mail = Arc::downgrade(&key.mail);
            let initial = || {
                let picture = image.as_ref().map(|(image, _)| image.clone());
                Shown::new(mail.clone(), picture, wait, stored, prompt.clone())
            };
            let shown = self
                .shown
                .entry(message.id.clone())
                .or_insert_with(&initial);
            if !Weak::ptr_eq(&shown.mail, &mail) {
                *shown = initial();
            }
            let next = if let Some((image, picture_key)) = image {
                shown.picture_key = Some(picture_key);
                Body::Web(image)
            } else if web
                && !failed
                && shown.picture_key.as_ref().is_some_and(|picture| {
                    picture.same_page(&key) && (!picture.images || key.images)
                })
            {
                shown.body.clone()
            } else if (wait || stored) && !matches!(shown.body, Body::Native) {
                shown.picture_key = None;
                // The current native card can fade directly into its first
                // snapshot when the reader explicitly switches renderers.
                if wait {
                    Body::Placeholder
                } else {
                    Body::StoredText
                }
            } else {
                shown.picture_key = None;
                Body::Native
            };
            if let Some(height) = placeholder_height {
                shown.placeholder_height = height;
            }
            shown.start_painted_fade(motion, now, real);
            shown.update(next, prompt, web, motion, now);
            shown.retry_until = retry_until;
            shown.image_consent = key.images;
            let mut frame = shown.frame(now);
            if matches!(frame.body, Body::Native)
                && (frame.outgoing.is_none() || frame.body_alpha[0] == 0.)
                && let Some(fraction) = shown.continue_at.take()
            {
                frame.continuation = Some(crate::scroll_anchor::Continuation::new(fraction));
            }
            frames.insert(message.id.clone(), frame);
        }
        self.shown.retain(|id, _| frames.contains_key(id));
        // Keep a one-time snapshot eligible for reuse while its card is native.
        keys.extend(native_keys);
        self.prune_cache(&keys, |image| cx.drop_image(image, None));
        frames
    }

    fn image_prompt(&self, key: &Key, web: bool, verified: bool, always: bool) -> Option<Prompt> {
        let has_images = key.mail.has_remote_images();
        let entry = web.then(|| self.entry(key)).flatten();
        if let Some(cached) = entry
            && (cached.picture.is_none() || cached.image_failure.is_some())
        {
            return Some(Prompt::Unavailable {
                verified: verified && has_images,
                always,
                images: key.images,
                failure: cached.picture.as_ref().and(cached.image_failure.clone()),
            });
        }
        has_images.then(|| {
            if !key.images {
                Prompt::Blocked { verified }
            } else if !web {
                Prompt::Native { always }
            } else if always {
                Prompt::AllowedSender
            } else if entry.is_none() {
                Prompt::Loading
            } else {
                Prompt::AllowedOnce
            }
        })
    }

    fn retry_until(&self, key: &Key) -> Option<Instant> {
        self.shown.get(&key.id).and_then(|shown| {
            (shown.image_consent == key.images
                && Weak::ptr_eq(&shown.mail, &Arc::downgrade(&key.mail)))
            .then_some(shown.retry_until)
            .flatten()
        })
    }
}

pub(super) fn reading_key_target(session: &Session) -> Option<&str> {
    let message = if session.pane == Pane::Conversation {
        session.messages.get(session.message_cursor)
    } else {
        session
            .messages
            .iter()
            .rfind(|message| message.expanded && matches!(message.rich, Rich::Html(_)))
    }?;
    Some(&message.id)
}

impl Inbox {
    fn click_image_prompt(&mut self, id: &str, action: Action, cx: &mut Context<Self>) {
        if !self.search_conversation_live() || self.open_thread != self.session.cursor_id {
            return;
        }
        if let Some(index) = self
            .session
            .messages
            .iter()
            .position(|m| m.id == id && m.expanded)
        {
            self.focus_message(index);
            self.html_reading_action(id, action, cx);
            cx.notify();
        }
    }

    pub(super) fn html_reading_key_action(&mut self, action: Action, cx: &mut Context<Self>) {
        if (self.session.pane == Pane::Conversation || self.conv_layout_current)
            && let Some(id) = reading_key_target(&self.session).map(str::to_owned)
        {
            self.html_reading_action(&id, action, cx);
        }
    }

    fn html_reading_action(&mut self, id: &str, action: Action, cx: &mut Context<Self>) {
        if !self.search_conversation_live() || self.open_thread != self.session.cursor_id {
            return;
        }
        let Some(index) = self
            .session
            .messages
            .iter()
            .position(|message| message.id == id)
        else {
            return;
        };
        let message = &self.session.messages[index];
        let Rich::Html(body) = &message.rich else {
            self.session
                .show_toast("This message has no formatted body", false);
            return;
        };
        let sender = sender_address(&message.email);
        if action == Action::AllowMessageImages
            && let Some(shown) = self.web_mail.shown.get_mut(id)
        {
            if shown.retry_until.is_some() {
                return;
            }
            if let Some(Prompt::Unavailable {
                images,
                ref failure,
                ..
            }) = shown.prompt
            {
                if failure
                    .as_ref()
                    .is_none_or(remote_images::UnavailableImages::retryable)
                {
                    let deadline = Instant::now() + RETRY_MINIMUM;
                    shown.retry_until = Some(deadline);
                    self.web_mail.retry_images(id, images);
                    cx.spawn(async move |this, cx| {
                        cx.background_executor()
                            .timer(deadline.saturating_duration_since(Instant::now()))
                            .await;
                        let _ = this.update(cx, |_, cx| cx.notify());
                    })
                    .detach();
                    cx.notify();
                }
                return;
            }
        }
        if action != Action::ToggleWebView
            && action != Action::BlockSenderImages
            && !body.web.has_remote_images()
        {
            return;
        }
        if action == Action::ToggleWebView {
            if self.web_mail.continue_in_native(
                id,
                self.conv_scroll.bounds(),
                self.theme.ui_px * 1.45,
            ) {
                self.reader_took_over(self.message_selected());
                self.motion.conv_glide = None;
                cx.notify();
                return;
            }
            let next = !self.web_mail.prefers_web(id, sender.as_deref(), body);
            self.web_mail.show_images_for.remove(id);
            self.web_mail.clear_native_override(id);
            if let Some(sender) = sender {
                self.web_mail
                    .preferences
                    .entry(sender.clone())
                    .or_default()
                    .0 = Some(next);
                self.persist_html_choice(sender, action, next, cx);
            } else if next {
                self.web_mail.show_web_for(id.to_owned(), None, body);
            } else {
                self.web_mail.show_native_for.insert(id.to_owned());
            }
        } else if action == Action::AllowMessageImages {
            self.web_mail
                .allowed_once
                .retain(|_, (_, mail)| mail.strong_count() > 0);
            self.web_mail
                .allowed_once
                .insert(id.to_owned(), (sender.clone(), Arc::downgrade(&body.web)));
            self.web_mail.show_web_for(id.to_owned(), sender, body);
            self.web_mail.retry_images(id, true);
        } else if let Some(sender) = sender {
            let allow = action == Action::AllowSenderImages;
            if allow && !self.web_mail.verified(body, Some(&sender)) {
                self.session
                    .show_toast("Sender verification unavailable · Use Load images", false);
                return;
            }
            self.web_mail
                .preferences
                .entry(sender.clone())
                .or_default()
                .1 = allow;
            if allow {
                self.web_mail.retry_images(id, true);
                self.web_mail
                    .show_web_for(id.to_owned(), Some(sender.clone()), body);
            } else {
                self.web_mail
                    .allowed_once
                    .retain(|_, (address, _)| address.as_deref() != Some(&sender));
                self.web_mail
                    .show_images_for
                    .retain(|_, (address, _)| address.as_deref() != Some(&sender));
                self.web_mail.evict_sender(&sender, cx);
            }
            self.persist_html_choice(sender, action, allow, cx);
        }
        self.session.messages[index].expanded = true;
        cx.notify();
    }

    fn persist_html_choice(
        &mut self,
        sender: String,
        action: Action,
        value: bool,
        cx: &mut Context<Self>,
    ) {
        let account = self.account_id.clone();
        let reply = self.store.call(move |store| {
            if action == Action::ToggleWebView {
                store.set_sender_web_view(&account, &sender, value)
            } else {
                store.set_sender_remote_images(&account, &sender, value)
            }
        });
        cx.spawn(async move |this, cx| {
            if !matches!(reply.await, Ok(Ok(()))) {
                let _ = this.update(cx, |inbox, cx| {
                    inbox
                        .session
                        .show_toast("Couldn’t save the sender preference", false);
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

pub(super) fn prompt(
    frame: &PromptFrame,
    theme: &Theme,
    message_id: &str,
    (live, target_keys): (bool, Option<&keymap::Keymap>),
    width: f32,
    cx: &mut Context<Inbox>,
) -> AnyElement {
    div()
        .id(ElementId::from((
            ElementId::from("web-image-prompt"),
            message_id.to_owned(),
        )))
        .relative()
        .h(px(PROMPT_HEIGHT * frame.height))
        .mb(px(PROMPT_GAP * frame.height))
        .overflow_hidden()
        .bg(rgb(theme.recipient_surface))
        .rounded_sm()
        .children(
            frame
                .outgoing
                .iter()
                .enumerate()
                .map(|(index_out, (outgoing, alpha))| {
                    prompt_layer(outgoing, theme, message_id, (false, target_keys), width, cx)
                        .id(("outgoing-prompt", index_out))
                        .opacity(*alpha)
                }),
        )
        .children(frame.incoming.as_ref().map(|incoming| {
            prompt_layer(incoming, theme, message_id, (live, target_keys), width, cx)
                .id("incoming-prompt")
                .opacity(frame.incoming_alpha)
        }))
        .into_any_element()
}

fn prompt_layer(
    prompt: &Prompt,
    theme: &Theme,
    message_id: &str,
    (live, target_keys): (bool, Option<&keymap::Keymap>),
    width: f32,
    cx: &mut Context<Inbox>,
) -> gpui::Div {
    let (label, once, sender): (gpui::SharedString, _, _) = match prompt {
        Prompt::Blocked { verified } => (
            if width < COMPACT_PROMPT_WIDTH {
                "Images blocked"
            } else {
                "Remote images blocked"
            }
            .into(),
            Some((
                Action::AllowMessageImages.label(),
                Action::AllowMessageImages,
            )),
            verified.then_some(("Always allow", Action::AllowSenderImages)),
        ),
        Prompt::Loading => ("Loading remote images…".into(), None, None),
        Prompt::Retrying => ("Retrying…".into(), None, None),
        Prompt::AllowedOnce => ("Remote images allowed for this message".into(), None, None),
        Prompt::AllowedSender => (
            "Remote images allowed for this sender".into(),
            None,
            Some(("Revoke", Action::BlockSenderImages)),
        ),
        Prompt::Native { always } => (
            "Images show in web view".into(),
            Some(("Web view", Action::ToggleWebView)),
            always.then_some(("Revoke", Action::BlockSenderImages)),
        ),
        Prompt::Unavailable {
            verified,
            always,
            failure,
            ..
        } => (
            failure.as_ref().map_or_else(
                || "Couldn’t render web view · showing simple view".into(),
                |failure| failure.label(width < COMPACT_FAILURE_WIDTH).into(),
            ),
            failure
                .as_ref()
                .is_none_or(remote_images::UnavailableImages::retryable)
                .then_some(("Retry", Action::AllowMessageImages)),
            if *always {
                Some(("Revoke", Action::BlockSenderImages))
            } else {
                verified.then_some(("Always allow", Action::AllowSenderImages))
            },
        ),
    };
    let button = |(label, action): (&'static str, Action), cx: &mut Context<Inbox>| {
        let key = target_keys
            .filter(|_| action != Action::BlockSenderImages)
            .and_then(|keys| {
                let bindings = keys.bindings_for(action);
                let binding = bindings
                    .iter()
                    .find(|binding| binding.keys.len() == 1)
                    .or(bindings.first())?;
                Some(keymap::prefix_text(&binding.keys, &keys.leader_name))
            })
            .filter(|key| !key.is_empty());
        let label = match (action, width, key.is_some()) {
            (Action::AllowMessageImages, width, _) if width < 420. && label == "Load images" => {
                "Load"
            }
            (Action::AllowSenderImages, width, true) if width < 420. => "",
            _ => label,
        };
        div()
            .id(ElementId::from((
                ElementId::from(action.label()),
                message_id.to_owned(),
            )))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(4.))
            .px(px(6.))
            .rounded_sm()
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgb(crate::theme::ensure_all(
                theme.accent,
                &[theme.recipient_surface, theme.background],
                4.6,
            )))
            .when(live, |button| {
                let message_id = message_id.to_owned();
                button
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(theme.background)))
                    .on_click(cx.listener(move |inbox, _, window: &mut Window, cx| {
                        window.focus(&inbox.focus);
                        inbox.click_image_prompt(&message_id, action, cx);
                    }))
            })
            .children((!label.is_empty()).then_some(label))
            .children(key.map(|key| {
                div()
                    .rounded_sm()
                    .px(px(5.))
                    .bg(rgb(theme.surface))
                    .border_1()
                    .border_color(rgb(theme.border))
                    .text_color(rgb(crate::theme::note_text(theme.dimmed, theme.surface)))
                    .font_family_opt(theme.mono_family.clone())
                    .child(key)
            }))
    };
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .gap(px(8.))
        .px(px(7.))
        .text_size(px(theme.ui_px - 1.))
        .text_color(rgb(match prompt {
            Prompt::AllowedOnce | Prompt::AllowedSender | Prompt::Native { .. } => {
                crate::theme::note_text(theme.recipient_text.1, theme.recipient_surface)
            }
            _ => theme.recipient_text.0,
        }))
        .child(
            // Shape the text after flex allocates the space beside the buttons.
            // GPUI can otherwise reuse an unconstrained nowrap measurement.
            div().relative().min_w_0().flex_1().h_full().child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .truncate()
                    .child(label),
            ),
        )
        .children(once.map(|action| button(action, cx)))
        .children(sender.map(|action| button(action, cx)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrival_hold_expires_at_150_ms_without_a_snapshot() {
        let started = Instant::now();
        let reader = Reader {
            opening: Some(Opening {
                started,
                cards: HashSet::from(["newsletter".into()]),
                has_layout: true,
            }),
            ..Reader::default()
        };
        assert!(reader.holding_arrival(started + Duration::from_millis(149)));
        assert!(!reader.holding_arrival(started + Duration::from_millis(150)));
        assert!(reader.opening.is_some());
        assert!(reader.cache.is_empty());
    }

    fn split_view(inbox: &mut Inbox) {
        let message = inbox.session.messages[0].clone();
        let messages = ["older", "newer"].map(|id| {
            let mut message = message.clone();
            message.id = id.into();
            message.email = format!("{id}@example.test");
            message.expanded = true;
            message.body_state = fell_core::BodyState::Stored;
            message.rich = crate::present::Rich::Html(Arc::new(crate::mail_body::prepare_html(
                r#"<p>Personal note</p><img src="https://example.test/image" alt="Photo">"#,
                &[("X-Fell-Verified-Sender".into(), message.email.clone())],
            )));
            message
        });
        inbox.session.set_messages(messages.into());
        inbox.settle_pending_bodies();
        inbox.session.pane = Pane::List;
        inbox.conv_layout_current = true;
        inbox.web_mail.trusted_delivery = true;
    }

    #[test]
    fn prompt_actions_keep_the_clicked_id_with_list_focus() {
        super::super::tests::with_pending_inbox(|inbox, cx| {
            split_view(inbox);
            assert_eq!(reading_key_target(&inbox.session), Some("newer"));
            // The prompt forwards its own stable ID even when the keyboard
            // still targets the newest card in the split view.
            inbox.html_reading_action("older", Action::AllowMessageImages, cx);
            assert!(inbox.web_mail.allowed_once.contains_key("older"));
            assert!(!inbox.web_mail.allowed_once.contains_key("newer"));
            inbox.html_reading_action("older", Action::AllowSenderImages, cx);
            assert!(inbox.web_mail.known_sender(Some("older@example.test")).1);
            assert!(!inbox.web_mail.known_sender(Some("newer@example.test")).1);
            inbox.html_reading_key_action(Action::AllowMessageImages, cx);
            assert!(inbox.web_mail.allowed_once.contains_key("newer"));
            inbox.click_image_prompt("older", Action::AllowMessageImages, cx);
            assert_eq!(inbox.session.pane, Pane::Conversation);
            assert!(inbox.message_selected());
            assert_eq!(reading_key_target(&inbox.session), Some("older"));
        });
    }

    #[test]
    fn one_time_web_views_survive_actions_on_another_message() {
        super::super::tests::with_pending_inbox(|inbox, cx| {
            split_view(inbox);
            inbox.html_reading_action("older", Action::AllowMessageImages, cx);
            inbox.html_reading_action("newer", Action::AllowMessageImages, cx);
            assert!(
                inbox
                    .session
                    .messages
                    .iter()
                    .all(|message| inbox.web_mail.prefers_message_web(message))
            );
            inbox.html_reading_action("newer", Action::ToggleWebView, cx);
            assert!(
                inbox
                    .web_mail
                    .prefers_message_web(&inbox.session.messages[0])
            );
            assert!(
                !inbox
                    .web_mail
                    .prefers_message_web(&inbox.session.messages[1])
            );
            assert!(inbox.web_mail.allowed_once.contains_key("older"));
        });
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            image: Arc::new(gpui::RenderImage::new(smallvec::smallvec![
                image::Frame::new(image::RgbaImage::new(1, 1),)
            ])),
            width: 1.,
            height: 1.,
            clipped: false,
            document_height: 1.,
            last_row_color: 0,
        }
    }

    #[test]
    fn switching_accounts_reschedules_a_superseded_snapshot() {
        let worker = Rc::new(webview::TestWorker::default());
        use gpui::AppContext as _;

        let mut app = gpui::TestAppContext::single();
        app.background_executor.allow_parking();
        let mut inbox = |id: &str| {
            app.new(|cx| {
                let mut inbox = Inbox::with_startup(
                    crate::mailbox::Startup {
                        keymap: Default::default(),
                        split: 0.4,
                        read_dwell_ms: 0,
                        error: None,
                        account_missing: true,
                        config: None,
                        path: Err("in-memory test".into()),
                    },
                    false,
                    None,
                    cx,
                );
                inbox.account_id.0 = id.into();
                inbox
            })
        };
        let first = inbox("first");
        let second = inbox("second");
        let first_key = Key {
            id: "first-account".into(),
            sender: None,
            mail: Arc::new(WebMail::prepare("<p>First account</p>")),
            width: 440,
            scheme: "light",
            background: 0xffffff,
            text: None,
            images: false,
        };
        let second_key = Key {
            id: "second-account".into(),
            mail: Arc::new(WebMail::prepare("<p>Second account</p>")),
            ..first_key.clone()
        };
        let request = |reader: &mut Reader, key: &Key, cx: &mut Context<Inbox>| {
            let key = reader.next_snapshot(std::slice::from_ref(key)).unwrap();
            let worker = worker.clone();
            reader.start_with(key.clone(), cx, move |request| async move {
                worker.render(request).await
            });
        };

        first.update(&mut app, |inbox, cx| {
            request(&mut inbox.web_mail, &first_key, cx);
        });
        app.run_until_parked();
        worker.start_next();

        second.update(&mut app, |inbox, cx| {
            request(&mut inbox.web_mail, &second_key, cx);
        });
        app.run_until_parked();
        worker.start_next();
        worker.finish(snapshot());
        app.run_until_parked();

        app.run_until_parked();
        first.update(&mut app, |inbox, cx| {
            assert!(inbox.web_mail.pending.is_none());
            assert!(inbox.web_mail.entry(&first_key).is_none());
            request(&mut inbox.web_mail, &first_key, cx);
        });
        app.run_until_parked();
        worker.start_next();
        worker.finish(snapshot());
        app.run_until_parked();

        first.update(&mut app, |inbox, _| {
            assert!(inbox.web_mail.image(&first_key).is_some());
        });
        second.update(&mut app, |inbox, _| {
            assert!(inbox.web_mail.image(&second_key).is_some());
        });
    }

    #[test]
    fn toggle_web_uses_a_message_override_without_a_sender_key() {
        super::super::tests::with_pending_inbox(|inbox, cx| {
            split_view(inbox);
            for email in ["unknown", "a b@example.test", "a@café.test"] {
                let mut messages = inbox.session.messages.as_slice().to_vec();
                messages[0].email = email.into();
                let mut body = crate::mail_body::prepare_html("<p>News</p>", &[]);
                body.suggested = RenderMode::Web;
                messages[0].rich = Rich::Html(Arc::new(body));
                inbox.session.set_messages(messages);

                assert!(
                    inbox
                        .web_mail
                        .prefers_message_web(&inbox.session.messages[0])
                );
                inbox.html_reading_action("older", Action::ToggleWebView, cx);
                assert!(
                    !inbox
                        .web_mail
                        .prefers_message_web(&inbox.session.messages[0])
                );
                inbox.html_reading_action("older", Action::ToggleWebView, cx);
                assert!(
                    inbox
                        .web_mail
                        .prefers_message_web(&inbox.session.messages[0])
                );
                assert!(inbox.web_mail.preferences.is_empty());
                inbox.web_mail.close_thread();
            }
        });
    }

    #[test]
    fn clipped_mail_toggles_the_sender_at_the_top_and_continues_at_the_row() {
        super::super::tests::with_pending_inbox(|inbox, cx| {
            split_view(inbox);
            let sender = "newer@example.test";
            inbox
                .web_mail
                .preferences
                .insert(sender.into(), (Some(true), false));
            let picture = Snapshot {
                clipped: true,
                document_height: 16_384.,
                ..snapshot()
            };
            inbox.web_mail.shown.insert(
                "newer".into(),
                Shown::new(Weak::new(), Some(picture), false, false, None),
            );
            let geometry = inbox.web_mail.shown["newer"].snapshot_geometry.clone();
            let viewport = inbox.conv_scroll.bounds();
            let height = webview::SNAPSHOT_HEIGHT_LIMIT as f32;
            geometry.set(Some((f32::from(viewport.top()), height)));
            inbox.html_reading_action("newer", Action::ToggleWebView, cx);
            assert_eq!(
                inbox.web_mail.known_sender(Some(sender)),
                (Some(false), false)
            );
            assert!(inbox.web_mail.show_native_for.is_empty());
            inbox.html_reading_action("newer", Action::ToggleWebView, cx);

            geometry.set(Some((f32::from(viewport.bottom()) - height, height)));
            inbox.html_reading_action("newer", Action::ToggleWebView, cx);
            assert!(
                !inbox
                    .web_mail
                    .prefers_message_web(&inbox.session.messages[1])
            );
            assert_eq!(
                inbox.web_mail.known_sender(Some(sender)),
                (Some(true), false)
            );
            let before_fade = height - crate::render::CLIP_FADE_HEIGHT - inbox.theme.ui_px * 1.45;
            assert_eq!(
                inbox.web_mail.shown["newer"].continue_at,
                Some(before_fade / 16_384.)
            );
            inbox.web_mail.close_thread();
            assert!(
                inbox
                    .web_mail
                    .prefers_message_web(&inbox.session.messages[1])
            );
        });
    }

    #[test]
    fn switching_renderers_fades_through_while_loading_images_keeps_the_old_snapshot() {
        let now = Instant::now();
        let motion = Motion::new(true);
        for to_web in [true, false] {
            let mut shown = Shown::new(Weak::new(), (!to_web).then(snapshot), false, false, None);
            let target = if to_web {
                Body::Web(snapshot())
            } else {
                Body::Native
            };
            shown.update(target, None, to_web, motion, now);
            let painted = now + Duration::from_millis(200);
            assert_eq!(shown.frame(painted).body_alpha, [1., 0.]);
            shown.renderer_painted.as_ref().unwrap().set(Some(painted));
            shown.start_painted_fade(motion, painted, painted);
            let at = |ms| shown.frame(painted + Duration::from_millis(ms));
            assert_eq!(at(0).body_alpha, [1., 0.]);
            assert!((at(30).body_alpha[0] - 0.5).abs() < 0.001);
            assert_eq!(at(60).body_alpha, [0., 0.]);
            assert!(at(60).body_animating());
            assert_eq!(at(140).body_alpha, [0., 1.]);
            assert!(!at(140).body_animating());
            for ms in 0..=140 {
                let [outgoing, incoming] = at(ms).body_alpha;
                assert_eq!(outgoing * incoming, 0., "{ms} ms");
            }
        }
        let mut shown = Shown::new(Weak::new(), Some(snapshot()), false, false, None);
        shown.update(Body::Web(snapshot()), None, true, motion, now);
        let [outgoing, incoming] = shown.frame(now + motion::FADE / 2).body_alpha;
        assert_eq!(outgoing, 1.);
        assert!(incoming > 0. && incoming < 1.);
    }

    #[test]
    fn retry_evicts_a_failed_snapshot_even_if_the_images_downloaded() {
        let blocked = Key {
            id: "newsletter".into(),
            sender: None,
            mail: Arc::new(WebMail::prepare("<p>News</p>")),
            width: 640,
            scheme: "light",
            background: 0xffffff,
            text: None,
            images: false,
        };
        let allowed = Key {
            images: true,
            ..blocked.clone()
        };
        let mut reader = Reader::default();
        reader.cache.extend([
            Cached {
                key: blocked.clone(),
                picture: Some(snapshot()),
                image_failure: None,
            },
            Cached {
                key: allowed.clone(),
                picture: None,
                image_failure: None,
            },
        ]);
        assert!(reader.entry(&allowed).is_some());
        reader.retry_images(&allowed.id, true);
        assert!(reader.entry(&allowed).is_none());
        assert!(reader.entry(&blocked).is_some());
        assert!(reader.image(&allowed).is_some());
    }

    #[test]
    fn a_failed_blocked_snapshot_offers_retry_without_consent() {
        let key = Key {
            id: "newsletter".into(),
            sender: None,
            mail: Arc::new(WebMail::prepare(r#"<img src="https://example.test/hero">"#)),
            width: 460,
            scheme: "light",
            background: 0xffffff,
            text: None,
            images: false,
        };
        let mut reader = Reader::default();
        reader.cache.push_back(Cached {
            key: key.clone(),
            picture: None,
            image_failure: None,
        });
        assert!(matches!(
            reader.image_prompt(&key, true, true, false),
            Some(Prompt::Unavailable {
                images: false,
                verified: true,
                ..
            })
        ));
        super::super::tests::with_pending_inbox(|inbox, cx| {
            split_view(inbox);
            let message = &inbox.session.messages[0];
            let Rich::Html(body) = &message.rich else {
                panic!("HTML fixture")
            };
            let mut reader = Reader::default();
            let key = reader.key(message, body, &inbox.theme, 460);
            reader
                .preferences
                .insert(message.email.clone(), (Some(true), false));
            reader.cache.push_back(Cached {
                key,
                picture: None,
                image_failure: None,
            });
            let now = Instant::now();
            let frames = reader.frames(
                &inbox.session.messages,
                (Some(0), &[true, true]),
                &inbox.theme,
                460.,
                (Motion::new(true), now, now),
                cx,
            );
            assert!(matches!(frames["older"].body, Body::Native));
            assert!(matches!(
                frames["older"].prompt.as_ref().unwrap().incoming,
                Some(Prompt::Unavailable { images: false, .. })
            ));
        });
        let keys = [key.clone()];
        assert!(reader.next_snapshot(&keys).is_none());
        reader.retry_images(&key.id, false);
        assert!(
            reader
                .next_snapshot(&keys)
                .is_some_and(|retry| !retry.images)
        );
    }

    #[test]
    fn pruning_keeps_a_snapshot_held_by_a_non_cursor_card() {
        let key = Key {
            id: "newsletter".into(),
            sender: None,
            mail: Arc::new(WebMail::prepare("<p>News</p>")),
            width: 460,
            scheme: "light",
            background: 0xffffff,
            text: None,
            images: false,
        };
        let mut reader = Reader::default();
        let held = snapshot();
        reader.shown.insert(
            key.id.clone(),
            Shown::new(
                Arc::downgrade(&key.mail),
                Some(held.clone()),
                false,
                false,
                None,
            ),
        );
        reader.cache.push_back(Cached {
            key: key.clone(),
            picture: Some(held.clone()),
            image_failure: None,
        });
        reader.cache.extend((0..CACHE_SIZE).map(|id| Cached {
            key: Key {
                id: id.to_string(),
                ..key.clone()
            },
            picture: Some(snapshot()),
            image_failure: None,
        }));
        let mut released = Vec::new();
        reader.prune_cache(&[], |image| released.push(image.id));
        assert_eq!(reader.cache.len(), CACHE_SIZE);
        assert_eq!(released.len(), 1);
        assert_ne!(released[0], held.image.id);
        assert!(reader.entry(&key).is_some());
    }

    #[test]
    fn an_expanded_native_body_transitions_from_its_painted_height() {
        let mut shown = Shown::new(Weak::new(), None, false, false, None);
        shown.height.set(Some(350.));
        let now = Instant::now();
        let motion = Motion::new(true);
        shown.update(Body::Web(snapshot()), None, true, motion, now);
        let frame = shown.frame(now);
        assert_eq!(frame.from_height, Some(350.));
        assert!(matches!(frame.outgoing, Some(Body::Native)));
        assert_eq!(frame.web_tag, Some(0.));
        let painted = now + Duration::from_millis(200);
        assert_eq!(shown.frame(painted).web_tag, Some(0.));
        frame.renderer_painted.unwrap().set(Some(painted));
        shown.start_painted_fade(motion, painted, painted);
        let frame = shown.frame(painted + RENDERER_CHANGE / 2);
        assert_eq!(frame.web_tag, Some(motion::ease_out(0.5)));
    }

    #[test]
    fn a_fast_download_replaces_the_unpainted_loading_label() {
        let blocked = Prompt::Blocked { verified: false };
        let mut shown = Shown::new(Weak::new(), None, true, false, Some(blocked.clone()));
        let now = Instant::now();
        let motion = Motion::new(true);
        shown.update(Body::Placeholder, Some(Prompt::Loading), true, motion, now);
        let finished = now + motion::FADE / 4;
        let loading = shown.frame(finished).prompt.unwrap();
        shown.update(
            Body::Placeholder,
            Some(Prompt::AllowedOnce),
            true,
            motion,
            finished,
        );
        let allowed = shown.frame(finished).prompt.unwrap();
        assert_eq!(allowed.incoming_alpha, 0.0);
        assert!(allowed.outgoing[0].0 == blocked);
        assert_eq!(allowed.outgoing[0].1, loading.outgoing[0].1);
        assert_eq!(allowed.outgoing.len(), 1);
        assert_eq!(loading.incoming_alpha, 0.);
        let arriving = shown
            .frame(finished + RENDERER_EXIT + PROMPT_ENTER / 2)
            .prompt
            .unwrap();
        assert!(arriving.incoming_alpha > 0.);
        assert_eq!(arriving.outgoing[0].1, 0.);
    }

    #[test]
    fn a_failure_reveals_the_prompt_and_retry_keeps_its_height() {
        let now = Instant::now();
        let motion = Motion::new(true);
        let mut shown = Shown::new(Weak::new(), None, true, false, None);
        shown.update(
            Body::Native,
            Some(Prompt::Unavailable {
                verified: false,
                always: false,
                images: false,
                failure: None,
            }),
            true,
            motion,
            now,
        );
        let start = shown.frame(now).prompt.unwrap();
        let mid = shown.frame(now + motion::FADE / 3).prompt.unwrap();
        assert_eq!(start.height, 0.);
        assert!(mid.height > 0. && mid.height < 1.);
        assert_eq!(mid.incoming_alpha, mid.height);

        let retry = now + motion::FADE;
        shown.update(Body::Native, Some(Prompt::Retrying), true, motion, retry);
        let frame = shown.frame(retry).prompt.unwrap();
        assert_eq!(frame.height, 1.);
        assert_eq!(frame.incoming_alpha, 0.);
        assert!(matches!(frame.incoming, Some(Prompt::Retrying)));
        let leaving = shown.frame(retry + RENDERER_EXIT / 2).prompt.unwrap();
        assert_eq!(leaving.incoming_alpha, 0.);
        assert!(leaving.outgoing[0].1 > 0.);
        let entering = shown.frame(retry + PROMPT_SWAP * 3 / 4).prompt.unwrap();
        assert!(entering.incoming_alpha > 0. && entering.incoming_alpha < 1.);
        assert_eq!(entering.outgoing[0].1, 0.);
        let entered = shown.frame(retry + PROMPT_SWAP).prompt.unwrap();
        assert_eq!(entered.incoming_alpha, 1.);
    }

    #[test]
    fn a_retry_label_leaves_before_the_failure_arrives() {
        let now = Instant::now();
        let motion = Motion::new(true);
        let mut shown = Shown::new(Weak::new(), None, false, false, Some(Prompt::Retrying));
        shown.update(
            Body::Native,
            Some(Prompt::Unavailable {
                verified: false,
                always: false,
                images: true,
                failure: None,
            }),
            true,
            motion,
            now,
        );
        let halfway_out = shown.frame(now + RENDERER_EXIT / 2).prompt.unwrap();
        assert_eq!(halfway_out.incoming_alpha, 0.);
        assert!(halfway_out.outgoing[0].1 > 0.);
        let seam = shown.frame(now + RENDERER_EXIT).prompt.unwrap();
        assert_eq!(seam.incoming_alpha, 0.);
        assert_eq!(seam.outgoing[0].1, 0.);
        assert_eq!(seam.height, 1.);
        let arriving = shown
            .frame(now + (RENDERER_EXIT + PROMPT_SWAP) / 2)
            .prompt
            .unwrap();
        assert!(arriving.incoming_alpha > 0.);
        assert_eq!(arriving.outgoing[0].1, 0.);
    }

    #[test]
    fn a_prompt_click_finds_its_card_after_an_insertion() {
        super::super::tests::with_pending_inbox(|inbox, cx| {
            split_view(inbox);
            let [first, second] = inbox.session.messages.as_slice() else {
                panic!("two messages")
            };
            let target = second.id.clone();
            let mut inserted = first.clone();
            inserted.id = "inserted".into();
            inserted.email = "inserted@example.test".into();
            for message in [first, second] {
                let Rich::Html(body) = &message.rich else {
                    panic!("HTML fixture")
                };
                let key = inbox.web_mail.key(message, body, &inbox.theme, 440);
                assert!(matches!(
                    inbox.web_mail.image_prompt(&key, false, true, false),
                    Some(Prompt::Blocked { .. })
                ));
            }
            inbox.session.messages.insert(0, inserted);
            inbox.click_image_prompt(&target, Action::AllowMessageImages, cx);
            assert_eq!(inbox.session.message_cursor, 2);
            assert!(inbox.web_mail.allowed_once.contains_key(&target));
            assert_eq!(inbox.web_mail.allowed_once.len(), 1);
        });
    }

    #[test]
    fn native_round_trip_keeps_the_one_time_snapshot_under_cache_pressure() {
        super::super::tests::with_pending_inbox(|inbox, cx| {
            split_view(inbox);
            inbox.html_reading_action("newer", Action::AllowMessageImages, cx);
            let message = &inbox.session.messages[1];
            let Rich::Html(body) = &message.rich else {
                panic!("HTML fixture")
            };
            let key = inbox.web_mail.key(message, body, &inbox.theme, 440);
            assert!(key.images);
            let image = snapshot();
            inbox.web_mail.cache.push_back(Cached {
                key: key.clone(),
                picture: Some(image.clone()),
                image_failure: None,
            });
            for n in 0..CACHE_SIZE {
                let key = Key {
                    id: n.to_string(),
                    ..key.clone()
                };
                inbox.web_mail.cache.push_back(Cached {
                    key,
                    picture: None,
                    image_failure: None,
                });
            }
            for expect_web in [false, true] {
                inbox.html_reading_action("newer", Action::ToggleWebView, cx);
                let now = Instant::now();
                let frames = inbox.web_mail.frames(
                    &inbox.session.messages,
                    (Some(1), &[true, true]),
                    &inbox.theme,
                    440.,
                    (Motion::new(false), now, now),
                    cx,
                );
                assert_eq!(
                    matches!(&frames["newer"].body, Body::Web(shown) if shown.image.id == image.image.id),
                    expect_web
                );
                assert!(
                    inbox
                        .web_mail
                        .next_snapshot(std::slice::from_ref(&key))
                        .is_none()
                );
            }
        });
    }
}
