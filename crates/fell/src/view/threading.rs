use super::*;

const MERGE_FEEDBACK_DELAY: Duration = Duration::from_millis(120);
const TITLE_EXIT: Duration = Duration::from_millis(60);

pub(super) struct RegroupMotion {
    pub transition: Transition,
    pub anchor: Option<ScrollAnchor>,
    departing: Arc<HashMap<usize, Vec<DepartingMessage>>>,
    title: Option<(String, LayerSwap)>,
    arriving: HashSet<String>,
    previous_markers: Arc<HashMap<String, Option<String>>>,
}

pub(super) struct RegroupFrame {
    pub departing: Arc<HashMap<usize, Vec<DepartingMessage>>>,
    pub title: Option<(String, motion::SwapFrame)>,
    pub progress: f32,
    pub arriving: HashSet<String>,
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
                let (destination, follow, notice) = match edit {
                    ThreadEdit::Split { message, .. } => {
                        let destination =
                            store.split_thread(&account, &fell_core::MessageId(message))?;
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
                        let target = store.merge_threads(&account, &sources, &target)?;
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
            arriving,
            previous_markers: Arc::new(previous_markers),
        });
        self.sent_arrival_anchor = anchor.filter(|_| transition.is_none());
        self.reveal = Reveal::Reading {
            selected: self.session.pane == Pane::Conversation,
        };
        self.finish_body_refresh(generation, true);
    }
}
