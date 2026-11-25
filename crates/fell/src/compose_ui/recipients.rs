use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use fell_core::Address;
use fell_core::compose::email_of;
use gpui::{Context, Deferred, MouseButton, Window, deferred, div, prelude::*, px};

use super::{ComposeView, Field, recipient_rows::tokens};
use crate::motion::Motion;
use crate::recipient_menu::{self, Menu};
use crate::store_actor::StoreClient;

pub(crate) struct RecipientStore(pub StoreClient);

impl gpui::Global for RecipientStore {}

pub(super) struct Completion {
    key: Option<Lookup>,
    generation: Arc<AtomicU64>,
    motion: Motion,
    menu: Menu,
    selected: usize,
    anchor: Field,
    ready: bool,
    accept_pending: bool,
}

#[derive(Clone, PartialEq)]
struct Lookup {
    draft: gpui::SharedString,
    field: Field,
    prefix: String,
    excluded: HashSet<String>,
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
            ready: true,
            accept_pending: false,
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
            .and_then(|line| {
                let caret = match &self.recipient_selection {
                    Some(selection) if !selection.is_empty() => return None,
                    Some(caret) => caret.start,
                    None => line.len(),
                };
                let token = token_at(line, caret);
                Some(Lookup {
                    draft: self.draft_id.clone(),
                    field: self.field,
                    prefix: prefix(&line[token.start..caret])?.to_owned(),
                    excluded: self.excluded_recipients(token),
                })
            });
        let completion = &mut self.recipients;
        if completion.key == next {
            return;
        }
        if completion
            .key
            .as_ref()
            .map(|lookup| (&lookup.draft, lookup.field))
            != next.as_ref().map(|lookup| (&lookup.draft, lookup.field))
        {
            completion.menu.dismiss(completion.motion, Instant::now());
            completion.selected = 0;
        }
        completion.key = next.clone();
        completion.ready = true;
        completion.accept_pending = false;
        let generation = Arc::clone(&completion.generation);
        let revision = generation.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let Some(Lookup {
            draft,
            field,
            prefix,
            excluded,
        }) = next
        else {
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
        completion.anchor = field;
        completion.ready = false;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(recipient_menu::QUERY_DELAY)
                .await;
            if generation.load(Ordering::Relaxed) != revision {
                return;
            }
            let latest = Arc::clone(&generation);
            let reply = store.call(move |store| {
                if latest.load(Ordering::Relaxed) != revision {
                    return Vec::new();
                }
                let mut people = store
                    .correspondent_suggestions_excluding(&account, &prefix, &excluded)
                    .unwrap_or_default();
                if people
                    .iter()
                    .any(|person| person.email.eq_ignore_ascii_case(&prefix))
                {
                    people.clear();
                }
                people
            });
            let timer = cx
                .background_executor()
                .timer(recipient_menu::LOADING_DELAY);
            let rows = recipient_menu::wait_for_lookup(reply, timer, || {
                let _ = this.update(cx, |compose, cx| {
                    if generation.load(Ordering::Relaxed) == revision {
                        let completion = &mut compose.recipients;
                        completion.menu.loading(
                            completion.selected,
                            completion.motion,
                            Instant::now(),
                        );
                        cx.notify();
                    }
                });
            })
            .await
            .unwrap_or_default();
            let _ = this.update(cx, |compose, cx| {
                if generation.load(Ordering::Relaxed) != revision {
                    return;
                }
                let completion = &mut compose.recipients;
                let accept = std::mem::take(&mut completion.accept_pending);
                completion.selected = completion.menu.show(
                    rows,
                    completion.selected,
                    completion.motion,
                    Instant::now(),
                );
                completion.ready = true;
                if accept && compose.accept_recipient(compose.recipients.selected) {
                    compose.update_recipient_completion(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn excluded_recipients(&self, active_token: Range<usize>) -> HashSet<String> {
        [
            (Field::To, self.to_line.as_str()),
            (Field::Cc, self.cc_line.as_str()),
            (Field::Bcc, self.bcc_line.as_str()),
        ]
        .into_iter()
        .flat_map(|(field, line)| {
            tokens(line)
                .filter(move |(start, _)| field != self.field || *start != active_token.start)
                .take(recipient_menu::EXCLUSION_LIMIT)
                .map(|(_, raw)| raw)
        })
        .filter_map(email_of)
        .map(|email| email.to_lowercase())
        .collect()
    }

    pub(super) fn recipient_key(&mut self, key: &str, modifiers: &gpui::Modifiers) -> bool {
        let completion = &mut self.recipients;
        if matches!(key, "tab" | "enter")
            && !modifiers.control
            && !modifiers.shift
            && !modifiers.platform
            && !modifiers.alt
            && !completion.ready
        {
            completion.accept_pending = true;
            return true;
        }
        let len = completion.menu.rows().len();
        if !completion.ready || len == 0 || modifiers.platform || modifiers.alt {
            return false;
        }
        let previous = completion.selected;
        match (key, modifiers.control, modifiers.shift) {
            ("n", true, false) => completion.selected = (completion.selected + 1) % len,
            ("p", true, false) => completion.selected = (completion.selected + len - 1) % len,
            ("tab" | "enter", false, false) => {
                return self.accept_recipient(self.recipients.selected);
            }
            _ => return false,
        }
        completion.menu.move_selection(
            previous,
            completion.selected,
            completion.motion,
            Instant::now(),
        );
        true
    }

    pub(super) fn dismiss_recipient_menu(&mut self) -> bool {
        let completion = &mut self.recipients;
        if !completion.menu.dismiss(completion.motion, Instant::now()) && completion.ready {
            return false;
        }
        completion.generation.fetch_add(1, Ordering::Relaxed);
        completion.accept_pending = false;
        completion.ready = true;
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
        let caret = match &self.recipient_selection {
            Some(selection) if !selection.is_empty() => return false,
            Some(caret) => caret.start,
            None => line.len(),
        };
        let token = token_at(line, caret);
        if prefix(&line[token.start..caret]).is_none() {
            return false;
        }
        let mut replacement = mailbox(person);
        if token.start > 0 {
            replacement.insert(0, ' ');
        }
        if token.end == line.len() {
            replacement.push_str(", ");
        }
        let caret = token.start + replacement.len();
        line.replace_range(token, &replacement);
        self.recipient_selection = (caret < line.len()).then_some(caret..caret);
        // Keep an accepted recipient in the draft even if the next token is
        // still unfinished.
        self.commit_field(self.field);
        true
    }

    pub(super) fn recipient_overlay(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        theme: &crate::theme::Theme,
    ) -> Option<(Field, Deferred)> {
        if self.preview || self.palette.is_some() {
            return None;
        }
        let completion = &self.recipients;
        let revision = completion.generation.load(Ordering::Relaxed);
        let popup = completion.menu.popup(
            window,
            completion.selected,
            completion.ready,
            theme,
            |index, row| {
                row.id(("recipient", index))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |compose, _, _, cx| {
                            if compose.recipients.generation.load(Ordering::Relaxed) == revision
                                && compose.accept_recipient(index)
                            {
                                compose.update_recipient_completion(cx);
                                cx.notify();
                            }
                        }),
                    )
                    .into_any_element()
            },
        )?;
        Some((
            completion.anchor,
            deferred(
                div()
                    .absolute()
                    .left_0()
                    .top(gpui::relative(1.))
                    .mt(px(4.))
                    .w_full()
                    .child(popup),
            ),
        ))
    }
}

/// A caret just before a comma belongs to the recipient before it.
fn token_at(line: &str, caret: usize) -> Range<usize> {
    if !line.contains('"') {
        let start = line[..caret].rfind(',').map_or(0, |index| index + 1);
        let end = line[caret..]
            .find(',')
            .map_or(line.len(), |index| caret + index);
        return start..end;
    }
    tokens(line)
        .map(|(start, token)| start..start + token.len())
        .find(|range| (range.start..=range.end).contains(&caret))
        .unwrap_or(caret..caret)
}

fn prefix(token: &str) -> Option<&str> {
    let token = token.trim();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose_ui::{ThemeKind, open_demo};

    #[test]
    fn accepting_a_recipient_saves_it_and_typing_starts_after_it() {
        let Some(root) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        let (hub, id, folder) = open_demo(root.join("demo"), ThemeKind::Dark).unwrap();
        let mut app = gpui::TestAppContext::single();
        let (view, cx) =
            app.add_window_view(|_, cx| ComposeView::new(hub.clone(), id.clone(), folder, cx));
        view.update(cx, |compose, _| {
            compose.field = Field::To;
            compose.to_line = "ar".into();
            compose.recipients.ready = true;
            compose.recipients.menu.show(
                vec![Address {
                    name: Some("Ari Wynn".into()),
                    email: "ari@example.test".into(),
                }],
                0,
                Motion::new(false),
                Instant::now(),
            );
            assert!(compose.accept_recipient(0));
            compose.field_key("b", Some("b"));
            assert_eq!(compose.to_line, "Ari Wynn <ari@example.test>, b");
            let snap = hub.with_session(&id, |session| session.snapshot()).unwrap();
            assert_eq!(snap.to.len(), 1);
            assert_eq!(snap.to[0].email, "ari@example.test");
            compose.field_key("e", Some("ea@example.test"));
            assert!(compose.commit_field(Field::To));
            let snap = hub.with_session(&id, |session| session.snapshot()).unwrap();
            assert_eq!(snap.to[1].email, "bea@example.test");
        });
    }

    #[test]
    fn completion_excludes_addresses_across_the_three_recipient_lines() {
        let Some(root) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        let (hub, id, folder) = open_demo(root.join("demo"), ThemeKind::Dark).unwrap();
        let mut app = gpui::TestAppContext::single();
        let (view, cx) = app.add_window_view(|_, cx| ComposeView::new(hub, id, folder, cx));
        view.update(cx, |compose, _| {
            compose.field = Field::To;
            compose.to_line = r#""Ruben, Test" <RUBEN@example.test>, ru"#.into();
            compose.cc_line = "Rhea <rhea@EXAMPLE.test>".into();
            compose.bcc_line = "ravi@example.test".into();
            assert_eq!(
                compose.excluded_recipients(token_at(&compose.to_line, compose.to_line.len())),
                [
                    "ruben@example.test",
                    "rhea@example.test",
                    "ravi@example.test"
                ]
                .map(str::to_owned)
                .into_iter()
                .collect()
            );
        });
    }

    #[test]
    fn a_five_thousand_address_line_has_a_bounded_exclusion_set() {
        let Some(root) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        let (hub, id, folder) = open_demo(root.join("demo"), ThemeKind::Dark).unwrap();
        let mut app = gpui::TestAppContext::single();
        let (view, cx) = app.add_window_view(|_, cx| ComposeView::new(hub, id, folder, cx));
        view.update(cx, |compose, _| {
            compose.field = Field::To;
            compose.to_line = (0..5_000)
                .map(|i| format!("alex{i}@example.test, "))
                .collect();
            compose.to_line.push_str("alex");
            compose.cc_line = "rhea@example.test".into();
            compose.bcc_line.clear();
            let token = token_at(&compose.to_line, compose.to_line.len());
            assert_eq!(prefix(&compose.to_line[token.clone()]), Some("alex"));
            let excluded = compose.excluded_recipients(token);
            assert_eq!(excluded.len(), recipient_menu::EXCLUSION_LIMIT + 1);
            assert!(excluded.contains("alex199@example.test"));
            assert!(!excluded.contains("alex200@example.test"));
            assert!(excluded.contains("rhea@example.test"));
        });
    }

    #[test]
    fn escape_retains_invalid_text_and_send_focuses_it_for_repair() {
        let root = tempfile::tempdir().unwrap();
        let (hub, id, folder) = open_demo(root.path().to_owned(), ThemeKind::Dark).unwrap();
        let mut app = gpui::TestAppContext::single();
        let (view, cx) =
            app.add_window_view(|_, cx| ComposeView::new(hub.clone(), id.clone(), folder, cx));
        view.update(cx, |compose, cx| {
            for field in [Field::To, Field::Cc, Field::Bcc] {
                compose.focus_header(field);
                let before = compose.header_text().to_owned();
                let line = "ari@example.test, r, avery@example.test";
                for _ in 0..before.len() {
                    compose.field_key("backspace", None);
                }
                compose.field_key("a", Some(line));
                compose.on_key(
                    &gpui::KeyDownEvent {
                        keystroke: gpui::Keystroke::parse("escape").unwrap(),
                        is_held: false,
                    },
                    cx,
                );
                compose.sync_from_hub();
                assert!(compose.field == Field::Body);
                assert_eq!(compose.status.as_ref(), "invalid address `r`");
                assert!(compose.recipient_errors[field.recipient_index().unwrap()].is_some());

                let saved = hub.with_session(&id, |session| session.snapshot()).unwrap();
                let addresses = match field {
                    Field::To => saved.to,
                    Field::Cc => saved.cc,
                    _ => saved.bcc,
                };
                assert_eq!(super::super::join_addrs(&addresses), before);

                compose.send(cx);
                assert!(compose.sending.is_none());
                assert!(compose.field == field);
                assert_eq!(compose.header_text(), line);
                assert_eq!(
                    &compose.header_text()[compose.recipient_selection.clone().unwrap()],
                    "r"
                );
                compose.field_key("r", Some("ruben@example.test"));
                assert!(compose.commit_field(field));
                assert_eq!(
                    compose.header_text(),
                    "ari@example.test, ruben@example.test, avery@example.test"
                );
                assert!(compose.recipient_errors[field.recipient_index().unwrap()].is_none());
            }
        });
    }

    #[test]
    fn palette_undo_restores_focused_to_before_sending() {
        let Some(root) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        let (hub, id, folder) = open_demo(root.join("demo"), ThemeKind::Dark).unwrap();
        hub.with_session(&id, |session| {
            session.set_body_from_user("See you Thursday.\n".into());
            session.end_user_change();
        });
        let mut app = gpui::TestAppContext::single();
        let (view, cx) =
            app.add_window_view(|_, cx| ComposeView::new(hub.clone(), id.clone(), folder, cx));
        view.update(cx, |compose, cx| {
            compose.field = Field::To;
            compose.to_line = "ar".into();
            compose.recipients.menu.show(
                vec![Address {
                    name: Some("Ari Wynn".into()),
                    email: "ari@example.test".into(),
                }],
                0,
                Motion::new(false),
                Instant::now(),
            );
            for key in [
                "enter",
                "ctrl-shift-p",
                "u->u",
                "n->n",
                "d->d",
                "o->o",
                "enter",
            ] {
                compose.on_key(
                    &gpui::KeyDownEvent {
                        keystroke: gpui::Keystroke::parse(key).unwrap(),
                        is_held: false,
                    },
                    cx,
                );
            }
            assert!(compose.field == Field::To);
            assert_eq!(compose.to_line, "Ada <ada@example.com>");
            compose.send(cx);
            assert_eq!(
                compose.sending.as_deref(),
                Some(id.as_str()),
                "{}",
                compose.status
            );
        });
        cx.run_until_parked();
        let store = fell_core::Store::open(root.join("demo/mail.db")).unwrap();
        while store.maintain_correspondents().unwrap() {}
        let account = fell_core::AccountId("local".into());
        let sent_to = store.correspondent_suggestions(&account, "ada").unwrap();
        assert_eq!(sent_to[0].email, "ada@example.com");
        assert!(
            store
                .correspondent_suggestions(&account, "ari")
                .unwrap()
                .is_empty()
        );
    }
}
