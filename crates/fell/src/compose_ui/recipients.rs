use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use fell_core::Address;
use fell_core::compose::email_of;
use gpui::{Context, Deferred, MouseButton, Window, deferred, div, prelude::*, px};

use super::{ComposeView, Field, Palette};
use crate::motion::Motion;
use crate::recipient_menu::{self, Menu};
use crate::store_actor::StoreClient;

pub(crate) struct RecipientStore(pub StoreClient);

impl gpui::Global for RecipientStore {}

pub(super) struct Completion {
    key: Option<(gpui::SharedString, Field, String)>,
    generation: Arc<AtomicU64>,
    motion: Motion,
    menu: Menu,
    selected: usize,
    anchor: Field,
    ready: bool,
}

impl Default for Completion {
    fn default() -> Self {
        Self {
            key: None,
            generation: Arc::default(),
            motion: Motion::from_env(),
            menu: Menu::default(),
            selected: 0,
            anchor: Field::To,
            ready: false,
        }
    }
}

impl Completion {
    pub(super) fn selected(&self) -> usize {
        self.selected
    }
    pub(super) fn has_matches(&self) -> bool {
        self.ready && !self.menu.rows().is_empty()
    }
    pub(super) fn animating(&self) -> bool {
        self.menu.animating(Instant::now())
    }
}

impl ComposeView {
    fn recipient_line(&self) -> Option<&str> {
        match self.field {
            Field::To => Some(&self.to_line),
            Field::Cc => Some(&self.cc_line),
            Field::Bcc => Some(&self.bcc_line),
            _ => None,
        }
    }

    pub(super) fn update_recipient_completion(&mut self, cx: &mut Context<Self>) {
        let next = self
            .recipient_line()
            .filter(|_| !self.closing && !self.discarding && !self.preview)
            .and_then(prefix)
            .map(|prefix| (self.draft_id.clone(), self.field, prefix.to_owned()));
        let completion = &mut self.recipients;
        if completion.key == next {
            return;
        }
        if completion
            .key
            .as_ref()
            .map(|(draft, field, _)| (draft, field))
            != next.as_ref().map(|(draft, field, _)| (draft, field))
        {
            completion
                .menu
                .update(Vec::new(), completion.motion, Instant::now());
        }
        completion.key = next.clone();
        completion.ready = false;
        let generation = Arc::clone(&completion.generation);
        let revision = generation.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let Some((draft, field, prefix)) = next else {
            return;
        };
        let Some(store) = cx
            .try_global::<RecipientStore>()
            .map(|store| store.0.clone())
        else {
            return;
        };
        let Some(account) = self
            .hub
            .with_session(draft.as_ref(), |session| {
                session
                    .send_lock()
                    .is_none()
                    .then(|| session.account_id().clone())
            })
            .flatten()
        else {
            return;
        };
        let latest = Arc::clone(&generation);
        let reply = store.call(move |store| {
            if latest.load(Ordering::Relaxed) != revision {
                return Vec::new();
            }
            let mut people = store
                .correspondent_suggestions(&account, &prefix)
                .unwrap_or_default();
            if people
                .iter()
                .any(|person| person.email.eq_ignore_ascii_case(&prefix))
            {
                people.clear();
            }
            people
        });
        cx.spawn(async move |this, cx| {
            let rows = reply.await.unwrap_or_default();
            let _ = this.update(cx, |compose, cx| {
                if generation.load(Ordering::Relaxed) != revision {
                    return;
                }
                let completion = &mut compose.recipients;
                let previous = completion.menu.rows().get(completion.selected);
                completion.selected = previous
                    .and_then(|person| {
                        rows.iter()
                            .position(|row| row.email.eq_ignore_ascii_case(&person.email))
                    })
                    .unwrap_or(0);
                completion.anchor = field;
                completion
                    .menu
                    .update(rows, completion.motion, Instant::now());
                completion.ready = true;
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn recipient_key(&mut self, key: &str, modifiers: &gpui::Modifiers) -> bool {
        self.recipients.menu.finish_opening();
        let completion = &mut self.recipients;
        let len = completion.menu.rows().len();
        if !completion.ready || len == 0 {
            return false;
        }
        match (key, modifiers.control, modifiers.shift) {
            ("n", true, false) => completion.selected = (completion.selected + 1) % len,
            ("p", true, false) => completion.selected = (completion.selected + len - 1) % len,
            ("tab" | "enter", false, false) => {
                return self.accept_recipient(self.recipients.selected);
            }
            _ => return false,
        }
        true
    }

    pub(super) fn accept_recipient(&mut self, index: usize) -> bool {
        if !self.recipients.ready {
            return false;
        }
        let Some(person) = self.recipients.menu.rows().get(index) else {
            return false;
        };
        let line = match self.field {
            Field::To => &mut self.to_line,
            Field::Cc => &mut self.cc_line,
            Field::Bcc => &mut self.bcc_line,
            _ => return false,
        };
        if prefix(line).is_none() {
            return false;
        }
        let start = token_start(line);
        line.truncate(start);
        if start > 0 {
            line.push(' ');
        }
        line.push_str(&mailbox(person));
        line.push_str(", ");
        true
    }

    pub(super) fn recipient_overlay(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        colors: Palette,
    ) -> Option<(Field, Deferred)> {
        let width = window.viewport_size().width;
        let completion = &mut self.recipients;
        let selected = completion.ready.then_some(completion.selected);
        let (people, alpha, inset) = completion.menu.visible(window, Instant::now());
        if people.is_empty() || self.preview || self.palette.is_some() {
            return None;
        }
        let left = if width < px(520.) { 6. } else { 118. };
        Some((
            completion.anchor,
            deferred(
                div()
                    .absolute()
                    .left(px(left))
                    .top(gpui::relative(1.))
                    .mt(px(inset))
                    .child(recipient_menu::popup(
                        people,
                        selected,
                        recipient_menu::Colors {
                            surface: colors.panel,
                            border: colors.rule,
                            text: colors.fg,
                            dim: colors.dim,
                            selection: colors.hunk_focus,
                            accent: colors.accent,
                        },
                        alpha,
                        (width - px(left + 38.)).min(px(480.)),
                        |index, row| {
                            if selected.is_some() {
                                row.id(("recipient", index))
                                    .cursor_pointer()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |compose, _, _, cx| {
                                            if compose.accept_recipient(index) {
                                                compose.update_recipient_completion(cx);
                                                cx.notify();
                                            }
                                        }),
                                    )
                                    .into_any_element()
                            } else {
                                row.into_any_element()
                            }
                        },
                    )),
            ),
        ))
    }
}

/// The last top-level comma. A comma in a quoted name belongs to that name.
fn token_start(line: &str) -> usize {
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (i, ch) in line.char_indices() {
        if escaped {
            escaped = false;
        } else {
            match ch {
                '\\' if quoted => escaped = true,
                '"' => quoted = !quoted,
                ',' if !quoted => start = i + 1,
                _ => {}
            }
        }
    }
    start
}

fn prefix(line: &str) -> Option<&str> {
    let token = line[token_start(line)..].trim();
    if token.is_empty() || (token.contains('<') && email_of(token).is_some()) {
        return None;
    }
    let typed = token.rsplit_once('<').map_or(token, |(_, email)| email);
    let typed = typed.trim_matches('"').trim();
    (!typed.is_empty()).then_some(typed)
}

fn mailbox(person: &Address) -> String {
    if let Some(name) = &person.name {
        let name = crate::present::display_safe(name).replace(['<', '>'], "");
        let quoted = name.contains([',', '"', '@', '\\']);
        let name = if quoted {
            format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
        } else {
            name
        };
        let candidate = format!("{name} <{}>", person.email);
        if email_of(&candidate).is_some() {
            return candidate;
        }
    }
    person.email.clone()
}
