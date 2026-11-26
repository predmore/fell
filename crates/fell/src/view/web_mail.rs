//! Conversation policy and snapshots for received HTML.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;

use fell_render::{RenderMode, WebMail};
use gpui::{
    AnyElement, Context, ElementId, FontWeight, InteractiveElement, IntoElement, ParentElement,
    Styled, Window, div, prelude::*, px, rgb,
};

use super::{ConvMessage, FontOpt, Inbox, Pane, Rich, Theme};
use crate::keymap::Action;
use crate::mail_body::HtmlBody;
use crate::motion::{self, Motion, Transition};
use crate::render::remote_images;
use crate::render::webview::{self, Snapshot, WebRequest};

const CACHE_SIZE: usize = 12;

#[derive(Default)]
pub(super) struct Reader {
    pub trusted_delivery: bool,
    pub preferences: HashMap<String, (Option<bool>, bool)>,
    allowed_once: HashMap<String, (Option<String>, Weak<WebMail>)>,
    show_images_for: Option<String>,
    pending: Option<Pending>,
    cache: VecDeque<Cached>,
    shown: HashMap<String, Shown>,
}

#[derive(Clone)]
struct Key {
    id: String,
    sender: Option<String>,
    mail: Arc<WebMail>,
    width: i32,
    scheme: &'static str,
    background: u32,
    images: bool,
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && Arc::ptr_eq(&self.mail, &other.mail)
            && self.width == other.width
            && self.scheme == other.scheme
            && self.background == other.background
            && self.images == other.images
    }
}

struct Cached {
    key: Key,
    picture: Option<Snapshot>,
    image_failure: Option<remote_images::Failure>,
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
    body: Body,
    outgoing: Option<Body>,
    fade: Option<Transition>,
    height: Rc<Cell<Option<f32>>>,
    from_height: Option<f32>,
    prompt: Option<Prompt>,
    outgoing_prompts: Vec<(Prompt, f32)>,
    prompt_from: f32,
    prompt_fade: Option<Transition>,
    reserves_tag: bool,
}

impl Shown {
    fn new(
        mail: Weak<WebMail>,
        image: Option<Snapshot>,
        waiting: bool,
        prompt: Option<Prompt>,
    ) -> Self {
        let body = image.map_or(
            if waiting {
                Body::Placeholder
            } else {
                Body::Native
            },
            Body::Web,
        );
        Self {
            mail,
            body,
            outgoing: None,
            fade: None,
            height: Rc::default(),
            from_height: None,
            prompt,
            outgoing_prompts: Vec::new(),
            prompt_from: 0.,
            prompt_fade: None,
            reserves_tag: false,
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
            self.outgoing = Some(std::mem::replace(&mut self.body, body));
            self.from_height = self.height.get();
            self.fade = motion.start(motion::FADE, now);
        }
        if !motion.enabled() || self.fade.is_none_or(|fade| !fade.running(now)) {
            self.fade = None;
            self.outgoing = None;
            self.from_height = None;
        }
        if self.prompt != prompt {
            // A quick download can finish before its loading label has faded in.
            // Continue from the opacities on screen when that happens.
            let prior = std::mem::replace(&mut self.prompt, prompt);
            let progress = motion::progress(self.prompt_fade.as_ref(), now);
            let fade = motion.start(motion::FADE, now);
            for (_, alpha) in &mut self.outgoing_prompts {
                *alpha *= 1.0 - progress;
            }
            if let Some(prior) = prior {
                self.outgoing_prompts
                    .push((prior, motion::lerp(self.prompt_from, 1.0, progress)));
            }
            self.prompt_from = 0.;
            self.outgoing_prompts.retain(|(label, alpha)| {
                if Some(label) == self.prompt.as_ref() {
                    self.prompt_from = *alpha;
                    return false;
                }
                *alpha > 0.0
            });
            self.prompt_fade = fade;
        }
        if !motion.enabled() || self.prompt_fade.is_none_or(|fade| !fade.running(now)) {
            self.prompt_fade = None;
            self.outgoing_prompts.clear();
        }
    }

    fn frame(&self, now: Instant) -> Frame {
        let progress = motion::progress(self.fade.as_ref(), now);
        let current_tag = f32::from(u8::from(self.body.is_web()));
        let previous_tag = self
            .outgoing
            .as_ref()
            .map_or(current_tag, |body| f32::from(u8::from(body.is_web())));
        Frame {
            body: self.body.clone(),
            outgoing: self.outgoing.clone(),
            progress,
            prompt: self.prompt.clone().map(|incoming| {
                let progress = motion::progress(self.prompt_fade.as_ref(), now);
                PromptFrame {
                    incoming,
                    incoming_alpha: motion::lerp(self.prompt_from, 1.0, progress),
                    outgoing: self
                        .outgoing_prompts
                        .iter()
                        .map(|(label, alpha)| (label.clone(), alpha * (1.0 - progress)))
                        .collect(),
                    progress,
                }
            }),
            web_tag: self
                .reserves_tag
                .then(|| motion::lerp(previous_tag, current_tag, progress)),
            height: Some(self.height.clone()),
            from_height: self.from_height,
        }
    }
}

#[derive(Clone)]
pub(super) enum Body {
    Placeholder,
    Native,
    Web(Snapshot),
}

impl Body {
    fn same_as(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Placeholder, Self::Placeholder) | (Self::Native, Self::Native) => true,
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
    pub progress: f32,
    pub prompt: Option<PromptFrame>,
    pub web_tag: Option<f32>,
    pub height: Option<Rc<Cell<Option<f32>>>>,
    pub from_height: Option<f32>,
}

impl Frame {
    pub(super) fn owns_body(&self) -> bool {
        !matches!(self.body, Body::Native) || self.outgoing.is_some()
    }

    pub(super) fn animating(&self) -> bool {
        self.progress < 1.0
            || self
                .prompt
                .as_ref()
                .is_some_and(|prompt| prompt.progress < 1.0)
    }
}

pub(super) struct PromptFrame {
    incoming: Prompt,
    incoming_alpha: f32,
    outgoing: Vec<(Prompt, f32)>,
    progress: f32,
}

#[derive(Clone, PartialEq, Eq)]
enum Prompt {
    Blocked {
        verified: bool,
    },
    Loading,
    AllowedOnce,
    AllowedSender,
    Native {
        always: bool,
    },
    Unavailable {
        always: bool,
        failure: Option<remote_images::Failure>,
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
        // Bulk layouts commonly specify light table backgrounds and inherit
        // the text color. Keep those colors readable in a dark inbox.
        let light_paper = body.suggested == RenderMode::Web;
        Key {
            id: message.id.clone(),
            sender,
            mail: body.web.clone(),
            width,
            scheme: if light_paper || theme.is_light() {
                "light"
            } else {
                "dark"
            },
            background: if light_paper { 0xffffff } else { theme.surface },
            images,
        }
    }

    fn prefers_web(&self, id: &str, sender: Option<&str>, suggested: RenderMode) -> bool {
        self.show_images_for.as_deref() == Some(id)
            || self
                .known_sender(sender)
                .0
                .unwrap_or(suggested == RenderMode::Web)
    }

    pub(super) fn prefers_message_web(&self, message: &ConvMessage) -> bool {
        matches!(&message.rich, Rich::Html(body) if self.prefers_web(
            &message.id, sender_address(&message.email).as_deref(), body.suggested,
        ))
    }

    fn entry(&self, key: &Key) -> Option<&Cached> {
        self.cache.iter().find(|cached| &cached.key == key)
    }

    fn image(&self, key: &Key) -> Option<Snapshot> {
        self.entry(key)
            .and_then(|cached| cached.picture.clone())
            .or_else(|| {
                // Only a blocked frame may be reused across permission changes.
                if !key.images {
                    return None;
                }
                self.cache.iter().find_map(|cached| {
                    (!cached.key.images
                        && cached.key.id == key.id
                        && Arc::ptr_eq(&cached.key.mail, &key.mail)
                        && cached.key.width == key.width
                        && cached.key.scheme == key.scheme
                        && cached.key.background == key.background)
                        .then(|| cached.picture.clone())
                        .flatten()
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

    fn retry_images(&mut self, id: &str, mut release: impl FnMut(Arc<gpui::RenderImage>)) {
        self.cache.retain(|cached| {
            let retry = cached.key.id == id
                && cached.key.images
                && (cached.picture.is_none() || cached.image_failure.is_some());
            if retry && let Some(picture) = &cached.picture {
                release(picture.image.clone());
            }
            !retry
        });
    }

    pub(super) fn stop(&mut self) {
        self.pending = None;
        self.shown.clear();
    }

    pub(super) fn release(&mut self, app: &mut gpui::App) {
        self.pending.take();
        self.shown.clear();
        for cached in self.cache.drain(..) {
            if let Some(picture) = cached.picture {
                app.drop_image(picture.image, None);
            }
        }
    }

    fn start(&mut self, key: Key, cx: &mut Context<Inbox>) {
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
                        let (html, result) =
                            remote_images::inline_page(&fetch.mail, fetch.scheme, || {
                                permission.load(Ordering::Acquire)
                            });
                        (html, result.err())
                    } else {
                        (fetch.mail.page(fetch.scheme, |_| None), None)
                    }
                })
                .await;
            let picture = webview::render(WebRequest {
                html,
                width,
                background,
                settle_images: install.images && image_failure.is_none(),
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
                reader.pending = None;
                if reader.cache.len() > CACHE_SIZE
                    && let Some(Cached {
                        picture: Some(old), ..
                    }) = reader.cache.pop_front()
                {
                    cx.drop_image(old.image, None);
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
        self.prefers_web(&key.id, key.sender.as_deref(), body.suggested)
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
        let width = width.round().clamp(1., 1600.) as i32;
        let key = message
            .and_then(|message| self.snapshot_key(message, theme, width))
            .map(|key| Key {
                images: false,
                ..key
            });
        self.request_snapshot(key, cx);
    }

    fn request_snapshot(&mut self, key: Option<Key>, cx: &mut Context<Inbox>) {
        if let Some(key) = key.filter(|key| self.entry(key).is_none()) {
            self.start(key, cx);
        } else {
            self.pending = None;
        }
    }

    pub(super) fn frames(
        &mut self,
        messages: &[ConvMessage],
        cursor: Option<usize>,
        theme: &Theme,
        width: f32,
        (motion, now): (Motion, Instant),
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
        let selected = messages.get(cursor).filter(|message| message.expanded);
        self.request_snapshot(
            selected.and_then(|message| self.snapshot_key(message, theme, width)),
            cx,
        );
        let mut frames = HashMap::new();
        for (index, message) in messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message.expanded)
        {
            let Rich::Html(body) = &message.rich else {
                continue;
            };
            let key = self.key(message, body, theme, width);
            let web = self.prefers_web(&key.id, key.sender.as_deref(), body.suggested);
            let image = web.then(|| self.image(&key)).flatten();
            let entry = self.entry(&key);
            let waiting = web && index == cursor && entry.is_none();
            let prompt = body.web.has_remote_images().then(|| {
                let verified = self.verified(body, key.sender.as_deref());
                let always = verified && self.known_sender(key.sender.as_deref()).1;
                if !key.images {
                    Prompt::Blocked { verified }
                } else if !web {
                    Prompt::Native { always }
                } else if let Some(cached) = entry {
                    if cached.picture.is_none() || cached.image_failure.is_some() {
                        Prompt::Unavailable {
                            always,
                            failure: cached.picture.as_ref().and(cached.image_failure.clone()),
                        }
                    } else if always {
                        Prompt::AllowedSender
                    } else {
                        Prompt::AllowedOnce
                    }
                } else if waiting {
                    Prompt::Loading
                } else {
                    Prompt::Native { always }
                }
            });

            let mail = Arc::downgrade(&key.mail);
            let shown = self.shown.entry(message.id.clone()).or_insert_with(|| {
                Shown::new(mail.clone(), image.clone(), waiting, prompt.clone())
            });
            if !Weak::ptr_eq(&shown.mail, &mail) {
                *shown = Shown::new(mail, image.clone(), waiting, prompt.clone());
            }
            let next = if let Some(image) = image {
                Body::Web(image)
            } else if waiting {
                shown.body.clone()
            } else {
                Body::Native
            };
            shown.update(next, prompt, web, motion, now);
            frames.insert(message.id.clone(), shown.frame(now));
        }
        self.shown.retain(|id, _| frames.contains_key(id));
        frames
    }
}

impl Inbox {
    pub(super) fn html_reading_action(&mut self, action: Action, cx: &mut Context<Self>) {
        if !self.search_conversation_live() || self.session.pane != Pane::Conversation {
            return;
        }
        let Some(message) = self.session.messages.get(self.session.message_cursor) else {
            return;
        };
        let Rich::Html(body) = &message.rich else {
            self.session
                .show_toast("This message has no formatted body", false);
            return;
        };
        let sender = sender_address(&message.email);
        let id = message.id.clone();
        if action != Action::ToggleWebView
            && action != Action::BlockSenderImages
            && !body.web.has_remote_images()
        {
            return;
        }
        if action == Action::ToggleWebView {
            let next = !self
                .web_mail
                .prefers_web(&id, sender.as_deref(), body.suggested);
            self.web_mail.show_images_for = None;
            if let Some(sender) = sender {
                self.web_mail
                    .preferences
                    .entry(sender.clone())
                    .or_default()
                    .0 = Some(next);
                self.persist_html_choice(sender, action, next, cx);
            } else if next {
                self.web_mail.show_images_for = Some(id);
            }
        } else if action == Action::AllowMessageImages {
            self.web_mail
                .allowed_once
                .retain(|_, (_, mail)| mail.strong_count() > 0);
            self.web_mail
                .allowed_once
                .insert(id.clone(), (sender, Arc::downgrade(&body.web)));
            self.web_mail.show_images_for = Some(id.clone());
            self.web_mail
                .retry_images(&id, |image| cx.drop_image(image, None));
        } else if let Some(sender) = sender {
            let allow = action == Action::AllowSenderImages;
            if allow && !self.web_mail.verified(body, Some(&sender)) {
                self.session.show_toast(
                    "Sender verification unavailable · Allow images for this message instead",
                    false,
                );
                return;
            }
            self.web_mail
                .preferences
                .entry(sender.clone())
                .or_default()
                .1 = allow;
            if allow {
                self.web_mail.show_images_for = Some(id);
            } else {
                self.web_mail
                    .allowed_once
                    .retain(|_, (address, _)| address.as_deref() != Some(&sender));
                if self.web_mail.show_images_for.as_deref() == Some(&id) {
                    self.web_mail.show_images_for = None;
                }
                self.web_mail.evict_sender(&sender, cx);
            }
            self.persist_html_choice(sender, action, allow, cx);
        }
        self.session.messages[self.session.message_cursor].expanded = true;
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
    live: bool,
    cx: &mut Context<Inbox>,
) -> AnyElement {
    div()
        .id(ElementId::from((
            ElementId::from("web-image-prompt"),
            message_id.to_owned(),
        )))
        .relative()
        .h(px(30.))
        .mb(px(8.))
        .bg(rgb(theme.recipient_surface))
        .rounded_sm()
        .children(
            frame
                .outgoing
                .iter()
                .enumerate()
                .map(|(index_out, (outgoing, alpha))| {
                    prompt_layer(outgoing, theme, message_id, false, cx)
                        .id(("outgoing-prompt", index_out))
                        .opacity(*alpha)
                }),
        )
        .child(
            prompt_layer(&frame.incoming, theme, message_id, live, cx)
                .id("incoming-prompt")
                .opacity(frame.incoming_alpha),
        )
        .into_any_element()
}

fn prompt_layer(
    prompt: &Prompt,
    theme: &Theme,
    message_id: &str,
    live: bool,
    cx: &mut Context<Inbox>,
) -> gpui::Div {
    let (label, once, sender): (gpui::SharedString, _, _) = match prompt {
        Prompt::Blocked { verified } => (
            "Remote images blocked".into(),
            Some(("Load images", Action::AllowMessageImages, Some("i"))),
            verified.then_some(("Always allow", Action::AllowSenderImages, Some("⇧I"))),
        ),
        Prompt::Loading => ("Loading remote images…".into(), None, None),
        Prompt::AllowedOnce => ("Remote images allowed for this message".into(), None, None),
        Prompt::AllowedSender => (
            "Remote images allowed for this sender".into(),
            None,
            Some(("Revoke", Action::BlockSenderImages, None)),
        ),
        Prompt::Native { always } => (
            "Images show in web view".into(),
            Some(("", Action::ToggleWebView, Some("w"))),
            always.then_some(("Revoke", Action::BlockSenderImages, None)),
        ),
        Prompt::Unavailable { always, failure } => (
            failure.as_ref().map_or_else(
                || "Couldn’t display the web view".into(),
                |failure| failure.label().into(),
            ),
            Some(("Retry", Action::AllowMessageImages, Some("i"))),
            always.then_some(("Revoke", Action::BlockSenderImages, None)),
        ),
    };
    let button =
        |(label, action, key): (&'static str, Action, Option<&str>), cx: &mut Context<Inbox>| {
            div()
                .id(action.label())
                .flex_none()
                .flex()
                .items_center()
                .gap(px(4.))
                .px(px(6.))
                .rounded_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(theme.accent))
                .when(live, |button| {
                    let message_id = message_id.to_owned();
                    button
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(theme.background)))
                        .on_click(cx.listener(move |inbox, _, window: &mut Window, cx| {
                            if !inbox.search_conversation_live()
                                || inbox.open_thread != inbox.session.cursor_id
                            {
                                return;
                            }
                            let Some(index) =
                                inbox.session.messages.iter().position(|message| {
                                    message.id == message_id && message.expanded
                                })
                            else {
                                return;
                            };
                            window.focus(&inbox.focus);
                            inbox.focus_message(index);
                            inbox.html_reading_action(action, cx);
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
                        .text_color(rgb(theme.dimmed))
                        .font_family_opt(theme.mono_family.clone())
                        .child(key.to_owned())
                }))
        };
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .gap(px(5.))
        .px(px(7.))
        .text_size(px(theme.ui_px - 1.))
        .text_color(rgb(match prompt {
            Prompt::AllowedOnce | Prompt::AllowedSender | Prompt::Native { .. } => {
                theme.recipient_text.1
            }
            _ => theme.recipient_text.0,
        }))
        .child(div().min_w_0().flex_1().truncate().child(label))
        .children(once.map(|action| button(action, cx)))
        .children(sender.map(|action| button(action, cx)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        Snapshot {
            image: Arc::new(gpui::RenderImage::new(smallvec::smallvec![
                image::Frame::new(image::RgbaImage::new(1, 1),)
            ])),
            width: 1.,
            height: 1.,
        }
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
        reader.retry_images(&allowed.id, |_| {
            panic!("the blocked snapshot must stay cached")
        });
        assert!(reader.entry(&allowed).is_none());
        assert!(reader.entry(&blocked).is_some());
        assert!(reader.image(&allowed).is_some());
    }

    #[test]
    fn an_expanded_native_body_crossfades_from_its_painted_height() {
        let mut shown = Shown::new(Weak::new(), None, false, None);
        shown.height.set(Some(350.));
        let now = Instant::now();
        shown.update(Body::Web(snapshot()), None, true, Motion::new(true), now);
        let frame = shown.frame(now);
        assert_eq!(frame.from_height, Some(350.));
        assert!(matches!(frame.outgoing, Some(Body::Native)));
        assert_eq!(frame.web_tag, Some(0.));
        let frame = shown.frame(now + motion::FADE / 2);
        assert_eq!(frame.web_tag, Some(frame.progress));
    }

    #[test]
    fn a_fast_download_fades_from_the_current_prompt_mix() {
        let blocked = Prompt::Blocked { verified: false };
        let mut shown = Shown::new(Weak::new(), None, true, Some(blocked.clone()));
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
        assert!(allowed.outgoing[1].0 == Prompt::Loading);
        assert_eq!(allowed.outgoing[1].1, loading.incoming_alpha);
    }
}
