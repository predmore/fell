use super::*;

pub(super) struct RegroupMotion {
    pub transition: Transition,
    removed_prefix: f32,
    arriving: HashSet<String>,
}

pub(super) struct RegroupFrame {
    pub gap: f32,
    pub progress: f32,
    pub arriving: HashSet<String>,
}

impl RegroupMotion {
    pub fn frame(&self, now: Instant) -> RegroupFrame {
        let progress = self.transition.progress(now);
        RegroupFrame {
            gap: self.removed_prefix * (1. - progress),
            progress,
            arriving: self.arriving.clone(),
        }
    }
}

impl Inbox {
    pub(super) fn search_merge_targets(&mut self, cx: &mut Context<Self>) {
        let account = self.account_id.clone();
        let generation = self.session.merge_search;
        let sources = self.session.merge_sources.clone();
        let query = self.session.folder_query.clone();
        let reply = self
            .store
            .call(move |store| store.merge_candidates(&account, &sources, &query));
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
                            .map(|(id, subject, sender, date)| PickerChoice {
                                name: id.0,
                                title: if subject.trim().is_empty() {
                                    "(no subject)".into()
                                } else {
                                    subject
                                },
                                detail: Some(format!(
                                    "{} · {}",
                                    present::display_safe(&sender),
                                    present::relative_date(now_unix(), date)
                                )),
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
                        (destination, true, "Conversation split")
                    }
                    ThreadEdit::Merge { sources, target } => {
                        let follow = select
                            .as_ref()
                            .is_some_and(|id| sources.contains(id) || id == &target);
                        let sources: Vec<_> = sources.into_iter().map(ThreadId).collect();
                        let target = ThreadId(target);
                        store.merge_threads(&account, &sources, &target)?;
                        (target, follow, "Conversations merged")
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
