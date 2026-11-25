use super::*;

pub(super) struct RegroupMotion {
    pub transition: Transition,
    removed_prefix: f32,
    old_title: String,
    arriving: HashSet<String>,
}

pub(super) struct RegroupFrame {
    pub gap: f32,
    pub old_title: String,
    pub progress: f32,
    pub arriving: HashSet<String>,
}

impl RegroupMotion {
    pub fn frame(&self, now: Instant) -> RegroupFrame {
        let progress = self.transition.progress(now);
        RegroupFrame {
            gap: self.removed_prefix * (1. - progress),
            old_title: self.old_title.clone(),
            progress,
            arriving: self.arriving.clone(),
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
                            })
                            .collect();
                    }
                    Err(err) => session.show_toast(err.to_string(), true),
                }
                cx.notify();
            });
        })
        .detach();
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
                let (destination, follow, notice) = match edit {
                    ThreadEdit::Split { thread, message } => {
                        let destination = store.split_thread(
                            &account,
                            &ThreadId(thread),
                            &fell_core::MessageId(message),
                        )?;
                        (
                            destination,
                            true,
                            "Split into its own conversation · u".to_owned(),
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
                        store.merge_threads(&account, &sources, &target)?;
                        let subject = present::display_safe(&subject).replace(['“', '”'], "'");
                        (target, follow, format!("Merged into “{subject}” · u"))
                    }
                };
                let key = follow
                    .then(|| {
                        store
                            .threading_list_key(
                                &account,
                                &destination,
                                &reading.0,
                                reading.1.as_deref(),
                            )
                            .ok()
                            .flatten()
                    })
                    .flatten();
                Ok((destination, key, follow, notice))
            },
            move |inbox, result, cx| match result {
                Ok((destination, key, follow, notice)) => {
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
                    inbox.session.show_toast(notice, false);
                    inbox.expire_action_notice(cx);
                    Some(ReadIntent {
                        changed: true,
                        refresh_open: true,
                        hold_selection: follow,
                        select: key.filter(|_| follow),
                        ..ReadIntent::default()
                    })
                }
                Err(err) => {
                    inbox.session.show_toast(err.to_string(), true);
                    inbox.expire_action_notice(cx);
                    None
                }
            },
            cx,
        );
    }

    /// Membership changed while the reader's selected message stayed in
    /// view. Reuse its disclosure state, focus and last drawn scroll anchor.
    pub(super) fn regroup_conversation(&mut self, view: Vec<ConvMessage>, generation: u64) {
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
        let removed_prefix = if anchor.is_some() {
            self.session.messages[..self.session.message_cursor]
                .iter()
                .enumerate()
                .filter(|(_, old)| view.iter().all(|message| message.id != old.id))
                .filter_map(|(index, _)| {
                    self.conv_scroll
                        .bounds_for_item(index + usize::from(self.conv_note_slot))
                })
                .map(|bounds| f32::from(bounds.size.height))
                .sum()
        } else {
            0.
        };
        let arriving: HashSet<_> = view
            .iter()
            .filter(|message| self.session.messages.iter().all(|old| old.id != message.id))
            .map(|message| message.id.clone())
            .collect();
        self.invalidate_conversation_layout();
        self.motion.conv_glide = None;
        self.regroup_motion = self
            .motion
            .motion
            .start(motion::FADE, self.motion.clock.now())
            .map(|transition| RegroupMotion {
                transition,
                removed_prefix,
                old_title: self.open_subject.clone(),
                arriving,
            });
        self.session.refresh_messages(view, false);
        self.sent_arrival_anchor = anchor.map(|mut anchor| {
            anchor.index = self.session.message_cursor;
            anchor
        });
        self.reveal = Reveal::Reading {
            selected: self.session.pane == Pane::Conversation,
        };
        self.finish_body_refresh(generation, true);
    }
}
