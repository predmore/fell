//! Conversation policy and snapshots for received HTML.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;

use fell_render::{RenderMode, WebMail};
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Styled,
    Window, div, prelude::*, px, rgb,
};

use super::{ConvMessage, Inbox, Pane, Rich, Theme};
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
    cache: VecDeque<(Key, Option<Snapshot>, usize)>,
    shown: Option<Shown>,
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
    id: String,
    image: Option<Snapshot>,
    outgoing: Option<Snapshot>,
    fade: Option<Transition>,
}

pub(super) struct Frame {
    pub image: Option<Snapshot>,
    pub outgoing: Option<Snapshot>,
    pub progress: f32,
    pub prompt: Option<Prompt>,
}

#[derive(Clone, Copy)]
pub(super) enum Prompt {
    Blocked { verified: bool },
    Loading,
    AllowedOnce,
    AllowedSender,
    Unavailable { always: bool },
}

fn sender_address(address: &str) -> Option<String> {
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

    fn prefers_web(&self, key: &Key, suggested: RenderMode) -> bool {
        self.show_images_for.as_deref() == Some(&key.id)
            || self
                .known_sender(key.sender.as_deref())
                .0
                .unwrap_or(suggested == RenderMode::Web)
    }

    fn entry(&self, key: &Key) -> Option<&(Key, Option<Snapshot>, usize)> {
        self.cache.iter().find(|(cached, _, _)| cached == key)
    }

    fn image(&self, key: &Key) -> Option<Snapshot> {
        self.entry(key)
            .or_else(|| {
                // Show the blocked frame while the explicit fetch is in flight.
                // Only a blocked frame may be reused across permission changes.
                key.images
                    .then(|| {
                        self.cache.iter().find(|(cached, _, _)| {
                            !cached.images
                                && cached.id == key.id
                                && Arc::ptr_eq(&cached.mail, &key.mail)
                                && cached.width == key.width
                                && cached.scheme == key.scheme
                                && cached.background == key.background
                        })
                    })
                    .flatten()
            })
            .and_then(|(_, picture, _)| picture.clone())
    }

    fn evict_sender(&mut self, sender: &str, cx: &mut Context<Inbox>) {
        self.pending.take();
        self.shown = None;
        self.cache.retain(|(key, picture, _)| {
            let remove = key.images && key.sender.as_deref() == Some(sender);
            if remove && let Some(picture) = picture {
                cx.drop_image(picture.image.clone(), None);
            }
            !remove
        });
    }

    pub(super) fn stop(&mut self) {
        self.pending = None;
        self.shown = None;
    }

    pub(super) fn release(&mut self, app: &mut gpui::App) {
        self.pending.take();
        self.shown = None;
        for (_, picture, _) in self.cache.drain(..) {
            if let Some(picture) = picture {
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
            let (html, count) = cx
                .background_spawn(async move {
                    if fetch.images {
                        remote_images::inline_page(&fetch.mail, fetch.scheme, || {
                            permission.load(Ordering::Acquire)
                        })
                    } else {
                        (fetch.mail.page(fetch.scheme, |_| None), 0)
                    }
                })
                .await;
            let picture = webview::render(WebRequest {
                html,
                width,
                background,
                settle_images: count > 0,
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
                reader.cache.push_back((install, picture, count));
                reader.pending = None;
                if reader.cache.len() > CACHE_SIZE
                    && let Some((_, Some(old), _)) = reader.cache.pop_front()
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

    pub(super) fn frames(
        &mut self,
        messages: &[ConvMessage],
        cursor: usize,
        theme: &Theme,
        width: f32,
        (motion, now): (Motion, Instant),
        cx: &mut Context<Inbox>,
    ) -> HashMap<String, Frame> {
        let width = width.round().clamp(1., 1600.) as i32;
        let target = messages
            .get(cursor)
            .filter(|message| message.expanded)
            .and_then(|message| {
                let Rich::Html(body) = &message.rich else {
                    return None;
                };
                let key = self.key(message, body, theme, width);
                self.prefers_web(&key, body.suggested).then_some(key)
            });
        if let Some(key) = target.filter(|key| self.entry(key).is_none()) {
            self.start(key, cx);
        } else {
            self.pending = None;
        }
        if self.shown.as_ref().is_some_and(|shown| {
            Some(shown.id.as_str()) != messages.get(cursor).map(|message| message.id.as_str())
        }) {
            self.shown = None;
        }
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
            let web = self.prefers_web(&key, body.suggested);
            let image = web.then(|| self.image(&key)).flatten();
            let mut frame = Frame {
                image,
                outgoing: None,
                progress: 1.0,
                prompt: None,
            };
            if body.web.has_remote_images() {
                let sender = self.verified(body, key.sender.as_deref());
                let always = sender && self.known_sender(key.sender.as_deref()).1;
                frame.prompt = Some(if !key.images {
                    Prompt::Blocked { verified: sender }
                } else if !web {
                    if always {
                        Prompt::AllowedSender
                    } else {
                        Prompt::AllowedOnce
                    }
                } else {
                    match self.entry(&key) {
                        None => Prompt::Loading,
                        Some((_, None, _)) | Some((_, _, 0)) => Prompt::Unavailable { always },
                        Some(_) if always => Prompt::AllowedSender,
                        Some(_) => Prompt::AllowedOnce,
                    }
                });
            }
            if index == cursor {
                let shown = self.shown.get_or_insert_with(|| Shown {
                    id: key.id.clone(),
                    image: None,
                    outgoing: None,
                    fade: None,
                });
                let current = shown.image.as_ref().map(|pic| pic.image.id);
                let next = frame.image.as_ref().map(|pic| pic.image.id);
                if current != next {
                    shown.outgoing = shown.image.take();
                    shown.image = frame.image.clone();
                    shown.fade = motion.start(motion::FADE, now);
                }
                if shown.fade.is_some_and(|fade| !fade.running(now)) || !motion.enabled() {
                    shown.fade = None;
                    shown.outgoing = None;
                }
                frame.progress = motion::progress(shown.fade.as_ref(), now);
                frame.outgoing = shown.outgoing.clone();
            }
            if frame.image.is_some() || frame.outgoing.is_some() || frame.prompt.is_some() {
                frames.insert(message.id.clone(), frame);
            }
        }
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
            let key = self.web_mail.key(message, body, &self.theme, 1);
            let next = !self.web_mail.prefers_web(&key, body.suggested);
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
                .cache
                .retain(|(key, _, count)| key.id != id || !key.images || *count > 0);
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
    prompt: Prompt,
    theme: &Theme,
    index: usize,
    live: bool,
    cx: &mut Context<Inbox>,
) -> AnyElement {
    let (label, once, sender) = match prompt {
        Prompt::Blocked { verified } => (
            "Remote images blocked",
            Some(("Load once", Action::AllowMessageImages)),
            verified.then_some(("Always allow", Action::AllowSenderImages)),
        ),
        Prompt::Loading => ("Loading remote images…", None, None),
        Prompt::AllowedOnce => ("Remote images allowed for this message", None, None),
        Prompt::AllowedSender => (
            "Remote images allowed for this sender",
            None,
            Some(("Revoke", Action::BlockSenderImages)),
        ),
        Prompt::Unavailable { always } => (
            "Remote images unavailable",
            Some(("Retry", Action::AllowMessageImages)),
            always.then_some(("Revoke", Action::BlockSenderImages)),
        ),
    };
    let button = |(label, action): (&'static str, Action), cx: &mut Context<Inbox>| {
        div()
            .id(action.label())
            .flex_none()
            .px(px(6.))
            .rounded_sm()
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgb(theme.accent))
            .when(live, |button| {
                button
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(theme.background)))
                    .on_click(cx.listener(move |inbox, _, window: &mut Window, cx| {
                        if !inbox.search_conversation_live() {
                            return;
                        }
                        window.focus(&inbox.focus);
                        inbox.session.pane = Pane::Conversation;
                        inbox.focus_message(index);
                        inbox.html_reading_action(action, cx);
                    }))
            })
            .child(label)
    };
    div()
        .h(px(30.))
        .mb(px(8.))
        .flex()
        .items_center()
        .gap(px(5.))
        .px(px(7.))
        .bg(rgb(theme.recipient_surface))
        .rounded_sm()
        .text_size(px(theme.ui_px - 1.))
        .text_color(rgb(theme.foreground))
        .child(div().min_w_0().flex_1().truncate().child(label))
        .children(once.map(|action| button(action, cx)))
        .children(sender.map(|action| button(action, cx)))
        .into_any_element()
}
