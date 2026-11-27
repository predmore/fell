use fell_core::store::ThreadEditOutcome;

use super::*;
use fell_core::store::{MUTED, SNOOZED};

fn hidden_destination(
    store: &fell_core::Store,
    account: &AccountId,
    thread: &ThreadId,
) -> Option<String> {
    if store
        .folder_key_of(account, MUTED, thread)
        .ok()
        .flatten()
        .is_some()
    {
        return Some("muted".into());
    }
    // The Snoozed list stores negative deadlines so the next wake sorts first.
    let key = store.folder_key_of(account, SNOOZED, thread).ok()??;
    let at = crate::model::snooze::toast_time(now_unix(), -key.date_unix);
    Some(format!("snoozed until {at}"))
}

const MERGE_FEEDBACK_DELAY: Duration = Duration::from_millis(120);
const TITLE_EXIT: Duration = Duration::from_millis(60);

pub(super) struct RegroupMotion {
    pub transition: Transition,
    pub anchor: Option<ScrollAnchor>,
    departing: Arc<HashMap<usize, Vec<DepartingMessage>>>,
    title: Option<(String, LayerSwap)>,
    arriving: Arc<HashSet<String>>,
    previous_markers: Arc<HashMap<String, Option<String>>>,
}

pub(super) struct RegroupFrame {
    pub departing: Arc<HashMap<usize, Vec<DepartingMessage>>>,
    pub title: Option<(String, motion::SwapFrame)>,
    pub progress: f32,
    pub arriving: Arc<HashSet<String>>,
    pub previous_markers: Arc<HashMap<String, Option<String>>>,
}

pub(super) struct DepartingMessage {
    pub message: ConvMessage,
    pub height: f32,
}

impl RegroupMotion {
    pub fn running(&self, now: Instant) -> bool {
        self.transition.running(now)
            || self
                .title
                .as_ref()
                .is_some_and(|(_, swap)| swap.frame(now).is_some())
    }

    pub fn frame(&self, now: Instant) -> RegroupFrame {
        let progress = self.transition.progress(now);
        RegroupFrame {
            departing: self.departing.clone(),
            title: self
                .title
                .as_ref()
                .and_then(|(title, swap)| swap.frame(now).map(|frame| (title.clone(), frame))),
            progress,
            arriving: self.arriving.clone(),
            previous_markers: self.previous_markers.clone(),
        }
    }
}

pub(super) struct RegroupSelection {
    pub selected: String,
    pub previous: ThreadRow,
    pub fade: Transition,
}

impl Inbox {
    pub(super) fn fade_regroup_selection(&mut self, previous: Option<ThreadRow>, now: Instant) {
        self.regroup_selection = previous
            .zip(self.session.cursor_thread())
            .filter(|(previous, current)| previous != *current)
            .and_then(|(previous, current)| {
                self.motion
                    .motion
                    .start(motion::FADE, now)
                    .map(|fade| RegroupSelection {
                        selected: current.id.clone(),
                        previous,
                        fade,
                    })
            });
    }

    pub(super) fn search_merge_targets(&mut self, cx: &mut Context<Self>) {
        let account = self.account_id.clone();
        let generation = self.session.merge_search;
        let me = self.session.account_email.clone();
        let sources = self.session.merge_sources.clone();
        let query = self.session.folder_query.clone();
        self.merge_feedback_at = Some(Instant::now() + MERGE_FEEDBACK_DELAY);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(MERGE_FEEDBACK_DELAY).await;
            let _ = this.update(cx, |inbox, cx| {
                if inbox.merge_query_loading() && inbox.session.merge_search == generation {
                    cx.notify();
                }
            });
        })
        .detach();
        let reply = self
            .store
            .call(move |store| store.merge_candidates(&account, &sources, &query, &me));
        cx.spawn(async move |this, cx| {
            let result = reply
                .await
                .unwrap_or_else(|_| Err(fell_core::Error::Action("Mailbox store stopped".into())));
            let _ = this.update(cx, |inbox, cx| {
                let session = &mut inbox.session;
                if session.overlay != Overlay::Folders
                    || session.folder_intent != FolderIntent::Merge
                    || session.merge_search != generation
                {
                    return;
                }
                session.merge_loading = false;
                inbox.merge_feedback_at = None;
                let before = inbox.before_step();
                let session = &mut inbox.session;
                match result {
                    Ok(rows) => {
                        session.merge_choices = rows
                            .into_iter()
                            .map(|row| PickerChoice {
                                name: row.thread.0,
                                title: if row.subject.trim().is_empty() {
                                    "(no subject)".into()
                                } else {
                                    row.subject
                                },
                                detail: Some(present::display_name(
                                    &row.sender_name,
                                    &row.sender_email,
                                    &session.account_email,
                                )),
                                date: Some(present::relative_date(now_unix(), row.date_unix)),
                                mark: None,
                                divider: false,
                                unavailable: false,
                            })
                            .collect();
                        let step = session.confirm_pending_merge();
                        if step.threading.is_some()
                            && let Some(log) = &inbox.frames
                        {
                            log.borrow_mut().note("merge_confirm_after_search", 1.);
                        }
                        inbox.apply_step(step, before, cx);
                    }
                    Err(err) => {
                        session.merge_choices.clear();
                        session.show_toast(err.to_string(), true);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn merge_query_loading(&self) -> bool {
        self.session.overlay == Overlay::Folders
            && self.session.folder_intent == FolderIntent::Merge
            && self.session.merge_loading
    }

    pub(super) fn merge_feedback(&self, now: Instant) -> (f32, bool) {
        if self.merge_query_loading()
            && let Some(start) = self.merge_feedback_at
            && now >= start
        {
            let fade = self.motion.motion.start(motion::QUICK, start);
            return (
                motion::progress(fade.as_ref(), now),
                fade.is_some_and(|fade| fade.running(now)),
            );
        }
        (0., false)
    }

    pub(super) fn edit_threading(&mut self, edit: ThreadEdit, cx: &mut Context<Self>) {
        let current = self.session.cursor_id.clone();
        if let ThreadEdit::Split { thread, .. } = &edit
            && self.open_thread.as_ref() != Some(thread)
        {
            self.session
                .show_toast("The conversation is still loading", false);
            return;
        }
        self.session.cancel_read_delay();
        let account = self.account_id.clone();
        let folder = self.session.folder.clone();
        let label = self.session.label_scope.clone();
        let reading = (folder.clone(), label.clone());
        let select = current.clone();
        self.change_mailbox(
            move |store| {
                let (outcome, follow, applied_notice, grouped_notice, mut departed) = match edit {
                    ThreadEdit::Split { message, .. } => {
                        let outcome =
                            store.split_thread(&account, &fell_core::MessageId(message))?;
                        (
                            outcome,
                            true,
                            "Split into its own conversation".to_owned(),
                            "This message and its replies already make up the conversation",
                            Vec::new(),
                        )
                    }
                    ThreadEdit::Merge {
                        sources,
                        target,
                        subject,
                    } => {
                        let follow = select
                            .as_ref()
                            .is_some_and(|id| sources.contains(id) || id == &target);
                        let sources: Vec<_> = sources.into_iter().map(ThreadId).collect();
                        let target = ThreadId(target);
                        let outcome = store.merge_threads(&account, &sources, &target)?;
                        let participants = sources.into_iter().chain([target]).map(|id| id.0);
                        let subject = present::display_safe(&subject).replace(['“', '”'], "'");
                        (
                            outcome,
                            follow,
                            format!("Merged into “{subject}”"),
                            "Already in one conversation",
                            participants.collect(),
                        )
                    }
                };
                let (destination, mut notice, applied) = match outcome {
                    ThreadEditOutcome::Applied(thread) => (thread, applied_notice, true),
                    ThreadEditOutcome::AlreadyGrouped(thread) => {
                        (thread, grouped_notice.into(), false)
                    }
                };
                let key = store.threading_list_key(
                    &account,
                    &destination,
                    &reading.0,
                    reading.1.as_deref(),
                );
                if matches!(key, Ok(None)) {
                    if let Some(detail) = hidden_destination(store, &account, &destination) {
                        notice.push_str(" · ");
                        notice.push_str(&detail);
                    }
                    if !departed.is_empty() && !departed.contains(&destination.0) {
                        departed.push(destination.0.clone());
                    }
                } else {
                    departed.clear();
                }
                Ok((
                    destination,
                    key.ok().flatten(),
                    follow,
                    notice,
                    applied,
                    departed,
                ))
            },
            move |inbox, result, cx| match result {
                Ok((destination, key, follow, notice, applied, mut departed)) => {
                    let follow = follow
                        && inbox.session.cursor_id == current
                        && inbox.session.folder == folder
                        && inbox.session.label_scope == label;
                    let choose = follow && (key.is_some() || inbox.session.search.is_some());
                    if choose {
                        inbox.regroup_to = Some(destination.clone());
                        if inbox.session.search.is_some() {
                            inbox.search_view.prefer = vec![destination];
                        }
                    }
                    inbox.session.picked.clear();
                    inbox.session.visual = false;
                    inbox.session.anchor_id = None;
                    inbox.note_generation = inbox.note_generation.wrapping_add(1);
                    if follow
                        && !choose
                        && !departed.is_empty()
                        && inbox.session.list_folder == folder
                        && inbox.session.list_label == label
                        && let Some(current) = &current
                    {
                        // Remove other departing rows first so the cursor goes to
                        // the next surviving row, including across a page boundary.
                        departed.retain(|id| id != current);
                        departed.push(current.clone());
                        inbox.remove_list_rows(&departed, cx);
                    }
                    if applied {
                        inbox.session.set_toast(Toast::with_hint(
                            notice,
                            crate::keymap::binding_text(&inbox.session.keymap, Action::Undo),
                            "",
                        ));
                    } else {
                        inbox.session.show_toast(notice, false);
                    }
                    inbox.expire_action_notice(cx);
                    Some(ReadIntent {
                        changed: true,
                        refresh_open: true,
                        hold_selection: follow && key.is_some(),
                        select: key.filter(|_| follow),
                        ..ReadIntent::default()
                    })
                }
                Err(err) => {
                    inbox.session.show_toast(err.to_string(), true);
                    inbox.expire_action_notice(cx);
                    matches!(
                        err,
                        fell_core::Error::UnknownThread | fell_core::Error::Action(_)
                    )
                    .then_some(ReadIntent {
                        refresh_open: true,
                        ..ReadIntent::default()
                    })
                }
            },
            cx,
        );
    }

    /// Membership changed while the reader's selected message stayed in
    /// view. Reuse its disclosure state, focus and last drawn scroll anchor.
    pub(super) fn regroup_conversation(
        &mut self,
        view: Vec<ConvMessage>,
        target: &str,
        generation: u64,
    ) {
        let anchor = self
            .conv_layout_current
            .then(|| {
                ScrollAnchor::capture_message(
                    &self.conv_scroll,
                    self.session.message_cursor,
                    usize::from(self.conv_note_slot),
                )
            })
            .flatten();
        let now = self.motion.clock.now();
        let transition = self.motion.motion.start(motion::FADE, now);
        let mut departing = HashMap::<_, Vec<_>>::new();
        let mut arriving = HashSet::new();
        let mut previous_markers = HashMap::new();
        if transition.is_some() {
            let indices: HashMap<_, _> = view
                .iter()
                .enumerate()
                .map(|(index, message)| (message.id.as_str(), index))
                .collect();
            let mut next_slot = view.len();
            let mut retained = HashSet::new();
            for (index, old) in self.session.messages.iter().enumerate().rev() {
                if let Some(&slot) = indices.get(old.id.as_str()) {
                    next_slot = slot;
                    retained.insert(old.id.as_str());
                    let previous = old.grouping_marker(None);
                    if previous != view[slot].grouping_marker(None) {
                        previous_markers.insert(old.id.clone(), previous);
                    }
                } else if self.conv_layout_current
                    && let Some(bounds) = self
                        .conv_scroll
                        .bounds_for_item(index + usize::from(self.conv_note_slot))
                {
                    let gap = if old.expanded { MESSAGE_GAP_PX } else { 0. };
                    departing
                        .entry(next_slot)
                        .or_default()
                        .push(DepartingMessage {
                            message: old.clone(),
                            height: f32::from(bounds.size.height) + gap,
                        });
                }
            }
            for cards in departing.values_mut() {
                cards.reverse();
            }
            arriving.extend(
                view.iter()
                    .filter(|message| !retained.contains(message.id.as_str()))
                    .map(|message| message.id.clone()),
            );
        }
        self.invalidate_conversation_layout();
        self.motion.conv_glide = None;
        self.session.refresh_messages(view, false);
        let anchor = anchor.map(|mut anchor| {
            anchor.index = self.session.message_cursor;
            anchor
        });
        let title = self
            .session
            .threads
            .iter()
            .find(|row| row.id == target)
            .filter(|row| !self.open_subject.is_empty() && row.subject != self.open_subject)
            .and_then(|_| LayerSwap::start(self.motion.motion, now, TITLE_EXIT, motion::QUICK))
            .map(|swap| (self.open_subject.clone(), swap));
        self.regroup_motion = transition.map(|transition| RegroupMotion {
            transition,
            anchor,
            departing: Arc::new(departing),
            title,
            arriving: Arc::new(arriving),
            previous_markers: Arc::new(previous_markers),
        });
        self.sent_arrival_anchor = anchor.filter(|_| transition.is_none());
        self.reveal = Reveal::Reading {
            selected: self.session.pane == Pane::Conversation,
        };
        self.finish_body_refresh(generation, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{fixture_account, seed_if_empty};
    use fell_core::{Store, test_support::in_isolated_process};
    use gpui::TestAppContext;

    #[test]
    fn merging_into_a_hidden_conversation_advances_inbox() {
        let Some(root) = in_isolated_process() else {
            return;
        };
        for muted in [false, true] {
            for already_grouped in [false, true] {
                let path = root.join(format!("{muted}-{already_grouped}.db"));
                let store = Store::open(&path).unwrap();
                let account = fixture_account();
                store.upsert_account(&account).unwrap();
                seed_if_empty(&store, &account).unwrap();
                let inbox_folder = "INBOX";
                let thread = |id| {
                    store
                        .thread_for_message(&account.id, &MessageId::new(id))
                        .unwrap()
                        .unwrap()
                };
                let target = thread("tickets@fell.test");
                let source = thread("tiles@fell.test");
                let marked_above = thread("dinner@fell.test");
                let until = now_unix() + 86_400;
                if muted {
                    store
                        .toggle_thread_mutes(&account.id, std::slice::from_ref(&target))
                        .unwrap();
                } else {
                    store
                        .set_thread_snoozes(&account.id, std::slice::from_ref(&target), Some(until))
                        .unwrap();
                }

                let mut app = TestAppContext::single();
                let view = app.new(|cx| {
                    Inbox::with_startup(
                        mailbox::Startup {
                            keymap: Default::default(),
                            split: 0.42,
                            read_dwell_ms: 0,
                            error: None,
                            account_missing: true,
                            config: None,
                            path: Ok(path.clone()),
                        },
                        true,
                        None,
                        cx,
                    )
                });
                let wait_for = |app: &mut TestAppContext, ready: &dyn Fn(&Inbox) -> bool| {
                    let deadline = Instant::now() + Duration::from_secs(30);
                    loop {
                        app.run_until_parked();
                        if view.read_with(app, |inbox, _| ready(inbox)) {
                            break;
                        }
                        assert!(
                            Instant::now() < deadline,
                            "muted={muted}, already_grouped={already_grouped}"
                        );
                        std::thread::sleep(Duration::from_millis(1));
                    }
                };
                wait_for(&mut app, &|inbox| {
                    inbox.session.threads.iter().any(|row| row.id == source.0)
                });
                let next = view.read_with(&app, |inbox, _| {
                    let rows = &inbox.session.threads;
                    let source_at = rows.iter().position(|row| row.id == source.0).unwrap();
                    assert!(rows[..source_at].iter().any(|row| row.id == marked_above.0));
                    assert!(!rows.iter().any(|row| row.id == target.0));
                    rows[source_at + 1].id.clone()
                });
                view.update(&mut app, |inbox, cx| {
                    // Exercise the next row when it is beyond the loaded page as well.
                    if muted {
                        let mut rows = inbox.session.threads.clone();
                        rows.truncate(rows.iter().position(|row| row.id == source.0).unwrap() + 1);
                        inbox
                            .session
                            .set_page(rows, false, true, inbox.session.unread_count);
                    }
                    assert!(inbox.session.select_if_present(source.as_str()));
                    inbox.request_messages(true, cx);
                });
                wait_for(&mut app, &|inbox| {
                    inbox.open_thread.as_ref() == Some(&source.0)
                });

                if already_grouped {
                    // Leave header changes queued so the merge itself sees the automatic join.
                    rusqlite::Connection::open(&path)
                        .unwrap()
                        .execute(
                            "UPDATE messages SET in_reply_to = 'tickets@fell.test'
                             WHERE message_id IN ('tiles@fell.test', 'dinner@fell.test')",
                            [],
                        )
                        .unwrap();
                }
                view.update(&mut app, |inbox, cx| {
                    inbox.edit_threading(
                        ThreadEdit::Merge {
                            sources: vec![marked_above.0, source.0.clone()],
                            target: target.0,
                            subject: "Tickets for Friday".into(),
                        },
                        cx,
                    );
                });
                wait_for(&mut app, &|inbox| {
                    !inbox.read_in_flight
                        && inbox.session.status_notice().is_some()
                        && inbox.open_thread.as_ref().is_some_and(|id| id != &source.0)
                });
                view.read_with(&app, |inbox, _| {
                    assert_eq!(inbox.session.list_folder, inbox_folder);
                    assert_eq!(inbox.session.cursor_id.as_deref(), Some(next.as_str()));
                    assert_eq!(inbox.open_thread.as_deref(), Some(next.as_str()));
                    assert!(inbox.regroup_selection.is_none());
                    let notice = inbox.session.status_notice().unwrap();
                    let action = if already_grouped {
                        "Already in one conversation"
                    } else {
                        "Merged into “Tickets for Friday”"
                    };
                    let detail = if muted {
                        "muted".into()
                    } else {
                        let time = crate::model::snooze::toast_time(now_unix(), until);
                        format!("snoozed until {time}")
                    };
                    assert_eq!(notice.text, format!("{action} · {detail}"));
                    assert_eq!(notice.hint, (!already_grouped).then(|| ("u".into(), "")));
                });
            }
        }
    }
}
