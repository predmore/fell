//! A bounded window over the configured accounts' Inbox indexes.

use std::collections::{BTreeMap, HashMap};
use std::ops::Bound::{self, Excluded, Included, Unbounded};

use fell_core::{
    AccountId, FlagChange, FolderRole, FolderRoles, FolderSpan, ListKey, Store, ThreadId,
};
use futures::future::join_all;
use gpui::{Context, Entity};

use super::{Inbox, ListRead, ReadIntent, Removal, SyncKind, ThreadRow, ThreadsResult, present};
use crate::model::snooze::{TriageEdit, TriageKind};
use crate::model::{FlagEdit, LabelEdit, MoveEdit, PickerChoice, Toast};

pub(super) const TITLE: &str = "All inboxes";

pub(super) struct AllInboxes {
    pub members: HashMap<String, Entity<Inbox>>,
    pub previous: String,
    pub positions: HashMap<String, ListPosition>,
    reader: Option<Entity<Inbox>>,
    destinations: HashMap<String, Destination>,
    undo: Vec<Vec<UndoEntry>>,
    pending: BTreeMap<u64, Option<Vec<UndoEntry>>>,
    next: u64,
    undo_requested: bool,
}

impl AllInboxes {
    pub fn new(previous: String, members: HashMap<String, Entity<Inbox>>) -> Self {
        Self {
            members,
            previous,
            positions: HashMap::new(),
            reader: None,
            destinations: HashMap::new(),
            undo: Vec::new(),
            pending: BTreeMap::new(),
            next: 0,
            undo_requested: false,
        }
    }

    fn owner(&self, account: &AccountId) -> Option<&Entity<Inbox>> {
        self.members.get(account.as_str())
    }

    pub fn account_for(&self, row: Option<&str>) -> &str {
        row.and_then(split_row_id)
            .map(|(_, account)| account)
            .and_then(|account| {
                self.members
                    .get_key_value(account)
                    .map(|(key, _)| key.as_str())
            })
            .unwrap_or(&self.previous)
    }

    pub fn reader(&self) -> Option<&Entity<Inbox>> {
        self.reader.as_ref()
    }

    pub fn begin_edit(&mut self) -> u64 {
        let serial = self.next;
        self.next += 1;
        self.pending.insert(serial, None);
        serial
    }

    pub fn forget_edit(&mut self, owner: &gpui::WeakEntity<Inbox>, action: i64) {
        for entries in self
            .undo
            .iter_mut()
            .chain(self.pending.values_mut().flatten())
        {
            entries.retain(|entry| entry.owner != *owner || entry.action != action);
        }
        self.undo.retain(|entries| !entries.is_empty());
    }

    pub fn remember_positions(&mut self, cx: &gpui::App) {
        // The account view may have changed a sender choice while this view
        // was away. Rebind before showing any of its message snapshots.
        self.reader = None;
        self.positions = self
            .members
            .iter()
            .filter_map(|(id, member)| {
                let inbox = member.read(cx);
                let cursor = inbox.session.cursor_id.clone()?;
                let top = inbox
                    .session
                    .selected_index()
                    .zip(inbox.list_row_height())
                    .map(|(index, height)| index as f32 * height + inbox.list_offset());
                Some((id.clone(), ListPosition { cursor, top }))
            })
            .collect();
    }
}

#[derive(Clone)]
pub(super) struct ListPosition {
    cursor: String,
    top: Option<f32>,
}

// Reuse the session's list keys and mark handling. The encoded ID sorts by
// thread ID then account ID, including when one thread ID prefixes another.
pub(super) fn row_id(account: &AccountId, thread: &ThreadId) -> String {
    format!(
        "{}\0\0{}",
        thread.0.replace('\0', "\0\u{1}"),
        account.as_str()
    )
}

fn split_row_id(id: &str) -> Option<(&str, &str)> {
    id.rsplit_once("\0\0")
}

pub(super) fn owner_and_thread(id: &str) -> Option<(AccountId, ThreadId)> {
    let (thread, account) = split_row_id(id)?;
    Some((
        AccountId(account.to_owned()),
        ThreadId(thread.replace("\0\u{1}", "\0")),
    ))
}

fn owned_key(account: &AccountId, key: ListKey) -> ListKey {
    ListKey {
        date_unix: key.date_unix,
        thread_id: ThreadId(row_id(account, &key.thread_id)),
    }
}

/// Project the global boundary onto one store. At an equal date and thread
/// ID, whether this store includes the row depends on the account tie-break.
fn local_key(account: &AccountId, key: &ListKey, lower: bool, inclusive: bool) -> Bound<ListKey> {
    let (owner, thread) = owner_and_thread(key.thread_id.as_str()).expect("merged list key");
    let edge = ListKey {
        date_unix: key.date_unix,
        thread_id: thread,
    };
    if (owner == *account && inclusive)
        || (lower && account.as_str() > owner.as_str())
        || (!lower && account.as_str() < owner.as_str())
    {
        Included(edge)
    } else {
        Excluded(edge)
    }
}

struct SourcePage {
    before: Vec<ThreadRow>,
    within: Vec<ThreadRow>,
    after: Vec<ThreadRow>,
    unread: usize,
    followed: Option<ListKey>,
    labels: Vec<String>,
}

#[derive(Clone)]
struct PageRequest {
    span: FolderSpan,
    center: usize,
    follow: Option<(AccountId, ThreadId)>,
    labels: bool,
}

impl PageRequest {
    fn read(
        &self,
        store: &Store,
        account: &AccountId,
        email: &str,
        cue: &str,
    ) -> fell_core::Result<SourcePage> {
        store.read_snapshot(|| {
            let roles = store.folder_roles(account)?;
            let inbox = roles.name(FolderRole::Inbox);
            let prepare = |threads: Vec<_>| {
                let mut rows = present::thread_rows(&threads, email);
                for (row, thread) in rows.iter_mut().zip(threads) {
                    row.id = row_id(account, &thread.thread_id);
                    row.account_label = Some(cue.to_owned());
                }
                rows
            };
            let (before, within, after, unread) = match &self.span {
                FolderSpan::Top { rows } | FolderSpan::Bottom { rows } => {
                    let from_end = matches!(self.span, FolderSpan::Bottom { .. });
                    let span = if from_end {
                        FolderSpan::Bottom { rows: rows + 1 }
                    } else {
                        FolderSpan::Top { rows: rows + 1 }
                    };
                    let page = store.folder_page(account, inbox, &span)?;
                    (Vec::new(), prepare(page.threads), Vec::new(), page.unread)
                }
                FolderSpan::Window {
                    first,
                    last,
                    before,
                    after,
                } => {
                    let first = first.as_ref().map(|key| key.min(last));
                    let start = first.map_or(Unbounded, |key| local_key(account, key, true, true));
                    let end = local_key(account, last, false, true);
                    let earlier =
                        first.map_or(Unbounded, |key| local_key(account, key, false, false));
                    let later = local_key(account, last, true, false);
                    let lead = if first.is_some() {
                        store.folder_slice(
                            account,
                            inbox,
                            (Unbounded, earlier.as_ref()),
                            before + 1,
                            true,
                        )?
                    } else {
                        Vec::new()
                    };
                    let middle = store.folder_slice(
                        account,
                        inbox,
                        (start.as_ref(), end.as_ref()),
                        self.center + 1,
                        false,
                    )?;
                    let tail = store.folder_slice(
                        account,
                        inbox,
                        (later.as_ref(), Unbounded),
                        after + 1,
                        false,
                    )?;
                    let unread = store
                        .folder_page(account, inbox, &FolderSpan::Top { rows: 0 })?
                        .unread;
                    (prepare(lead), prepare(middle), prepare(tail), unread)
                }
            };
            let followed = match self.follow.as_ref() {
                Some((owner, thread)) if owner == account => store
                    .folder_key_of(account, inbox, thread)?
                    .map(|key| owned_key(account, key)),
                _ => None,
            };
            Ok(SourcePage {
                before,
                within,
                after,
                unread,
                followed,
                labels: if self.labels {
                    store.list_labels(account)?
                } else {
                    Vec::new()
                },
            })
        })
    }

    fn merge(&self, pages: Vec<SourcePage>) -> ListRead {
        let mut before = Vec::new();
        let mut within = Vec::new();
        let mut after = Vec::new();
        let mut unread = 0;
        let mut followed = None;
        let mut labels = Vec::new();
        for page in pages {
            before.extend(page.before);
            within.extend(page.within);
            after.extend(page.after);
            unread += page.unread;
            followed = followed.or(page.followed);
            labels.extend(page.labels);
        }
        let order = |a: &ThreadRow, b: &ThreadRow| {
            b.date_unix.cmp(&a.date_unix).then_with(|| a.id.cmp(&b.id))
        };
        within.sort_unstable_by(order);
        let (rows, more_before, more_after) = match self.span {
            FolderSpan::Top { rows } => {
                let more = within.len() > rows;
                within.truncate(rows);
                (within, false, more)
            }
            FolderSpan::Bottom { rows } => {
                let discard = within.len().saturating_sub(rows);
                within.drain(..discard);
                (within, discard > 0, false)
            }
            FolderSpan::Window {
                before: lead,
                after: tail,
                ..
            } => {
                before.sort_unstable_by(order);
                let discard = before.len().saturating_sub(lead);
                before.drain(..discard);
                let capped = within.len() > self.center;
                within.truncate(self.center);
                before.extend(within);
                // Never jump over rows when new mail overflowed the window.
                let more_after = capped || after.len() > tail;
                if !capped {
                    after.sort_unstable_by(order);
                    after.truncate(tail);
                    before.extend(after);
                }
                (before, discard > 0, more_after)
            }
        };
        labels.sort_unstable();
        labels.dedup();
        ListRead {
            rows,
            unread,
            followed,
            more_before,
            more_after,
            names: Vec::new(),
            labels: self.labels.then_some(labels),
            drafts: Vec::new(),
            roles: FolderRoles::default(),
        }
    }
}

impl Inbox {
    pub(super) fn restore_account_position(
        &mut self,
        position: ListPosition,
        cx: &mut Context<Self>,
    ) {
        if self.session.cursor_id.as_ref() == Some(&position.cursor)
            || !self.session.select_if_present(&position.cursor)
        {
            return;
        }
        if let Some((index, height)) = self.session.selected_index().zip(self.list_row_height())
            && let Some(top) = position.top
        {
            self.motion.list_glide = None;
            self.set_list_offset((top - index as f32 * height).min(0.0));
        } else {
            self.scroll_list_to_cursor();
        }
        self.request_messages(true, cx);
    }

    pub(super) fn request_all_inboxes(&mut self, intent: ReadIntent, cx: &mut Context<Self>) {
        let mut span = if let Some(key) = &intent.select {
            FolderSpan::Window {
                first: Some(key.clone()),
                last: key.clone(),
                before: self.session.load_rows / 2,
                after: self.session.load_rows / 2,
            }
        } else {
            self.session.span(intent.fetch, None, self.visible_rows())
        };
        match &mut span {
            FolderSpan::Top { rows } => *rows = (*rows).min(self.session.load_rows),
            FolderSpan::Window { before, after, .. } => {
                // Counts and visual ranges can request more on subsequent pages.
                *before = (*before).min(self.session.load_rows);
                *after = (*after).min(self.session.load_rows);
            }
            FolderSpan::Bottom { .. } => {}
        }
        let request = PageRequest {
            span,
            center: self.session.threads.len() + self.session.load_rows,
            follow: self.session.cursor_id.as_deref().and_then(owner_and_thread),
            labels: self.session.loading || intent.changed,
        };
        let all = self.all_inboxes.as_ref().expect("merged inbox");
        let jobs: Vec<_> = all
            .members
            .values()
            .filter_map(|view| {
                let inbox = view.read(cx);
                if inbox.session.sync == SyncKind::StoreError {
                    return None;
                }
                let account = inbox.account_id.clone();
                let email = inbox.session.account_email.clone();
                let name = &inbox.session.account_name;
                let duplicate = all.members.values().any(|other| {
                    other != view
                        && other
                            .read(cx)
                            .session
                            .account_name
                            .eq_ignore_ascii_case(name)
                });
                let cue = present::display_safe(if name.trim().is_empty() || duplicate {
                    &email
                } else {
                    name
                });
                let request = request.clone();
                let read = move |store: &Store| request.read(store, &account, &email, &cue);
                Some(if intent.is_refresh() {
                    inbox.store.call_background(read)
                } else {
                    inbox.store.call(read)
                })
            })
            .collect();
        let scope = (self.session.folder.clone(), None);
        let generation = self.read_gen;
        let merge = cx.background_executor().spawn(async move {
            let pages = join_all(jobs)
                .await
                .into_iter()
                .map(|result| {
                    result
                        .map_err(|_| "A mailbox store stopped".to_string())?
                        .map_err(|err| err.to_string())
                })
                .collect::<Result<Vec<_>, _>>();
            pages.map(|pages| request.merge(pages))
        });
        cx.spawn(async move |this, cx| {
            let result: ThreadsResult = merge.await;
            let _ = this.update(cx, |inbox, cx| {
                if inbox.read_gen == generation {
                    let log = inbox.frames.clone();
                    super::FrameLog::event(log.as_ref(), "rows", || {
                        inbox.read_landed(scope, result, intent, cx);
                    });
                }
            });
        })
        .detach();
    }

    /// Use the selected row's existing connections for the conversation and
    /// compose. Per-message reader caches must never cross a mailbox boundary.
    pub(super) fn bind_all_reader(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(all) = &mut self.all_inboxes else {
            return true;
        };
        let account = self
            .session
            .cursor_id
            .as_deref()
            .and_then(split_row_id)
            .map_or(all.previous.as_str(), |(_, account)| account);
        let Some(view) = all.members.get(account).cloned() else {
            return false;
        };
        let source = view.read(cx);
        if source.retired || source.session.sync == SyncKind::StoreError {
            return false;
        }
        let changed = all.reader.as_ref() != Some(&view);
        if changed {
            all.reader = Some(view);
            self.store = source.store.clone();
            self.account_id = source.account_id.clone();
            self.attachment_fetcher = source.attachment_fetcher.clone();
        }
        self.draft_home = source.draft_home.clone();
        self.sync = source.sync.clone();
        self.session
            .account_name
            .clone_from(&source.session.account_name);
        self.session
            .account_email
            .clone_from(&source.session.account_email);
        self.session.sync = source.session.sync.clone();
        let trusted_delivery = source.web_mail.trusted_delivery;
        let preferences = changed.then(|| source.web_mail.preferences.clone());
        if let Some(preferences) = preferences {
            self.clear_open_message();
            self.bodies = Default::default();
            self.body_loads.clear_mailbox();
            self.pending_bodies = Default::default();
            self.failed_attachments.clear();
            self.attachment_retry = None;
            self.attachment_metadata = Default::default();
            self.note_view = Default::default();
            self.session.agent_notes = Default::default();
            self.web_mail.release(cx);
            self.web_mail = super::web_mail::Reader::new(self.web_mail.reading);
            self.web_mail.trusted_delivery = trusted_delivery;
            self.web_mail.preferences = preferences;
        }
        true
    }

    pub(super) fn all_reader_scope(&self, cx: &gpui::App) -> (FolderRoles, Vec<String>, String) {
        let source = self
            .all_inboxes
            .as_ref()
            .and_then(AllInboxes::reader)
            .map(|view| &view.read(cx).session)
            .unwrap_or(&self.session);
        let folder = if self.session.all_inboxes {
            source.roles.name(FolderRole::Inbox)
        } else {
            self.session
                .search_source(self.session.cursor_id.as_deref().unwrap_or_default())
                .unwrap_or(&self.session.folder)
        };
        (
            source.roles.clone(),
            source.folders.clone(),
            folder.to_owned(),
        )
    }
}

#[derive(Clone, PartialEq, Eq)]
enum Destination {
    Role(FolderRole),
    Folder(String),
}

impl Destination {
    fn matches(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Folder(first), Self::Folder(second)) => first.eq_ignore_ascii_case(second),
            _ => self == other,
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Role(role) => role.label(),
            Self::Folder(name) => name,
        }
    }
}

#[derive(Clone)]
enum Edit {
    Remove(Removal),
    Move(Destination),
    Triage(TriageKind),
    Flag(FlagChange),
    Label(fell_core::LabelChange),
}

impl Edit {
    fn removes_rows(&self) -> bool {
        matches!(
            self,
            Self::Remove(_)
                | Self::Move(_)
                | Self::Triage(TriageKind::Snooze(_) | TriageKind::Mute)
        )
    }

    fn apply(
        &self,
        store: &Store,
        account: &AccountId,
        threads: Vec<ThreadId>,
    ) -> fell_core::Result<(usize, Option<i64>)> {
        use Removal::{Archive, Delete, Purge, Spam};
        let roles = store.folder_roles(account)?;
        let folder = roles.name(FolderRole::Inbox);
        let scopes = || {
            threads
                .iter()
                .cloned()
                .map(|id| (id, folder.to_owned()))
                .collect::<Vec<_>>()
        };
        let (action, count) = match self {
            Self::Remove(Archive) => store.archive_scoped_threads(account, &scopes())?,
            Self::Remove(Delete) => store.delete_scoped_threads(account, &scopes())?,
            Self::Remove(Spam) => store.spam_scoped_threads(account, &scopes())?,
            Self::Remove(Purge { .. }) => unreachable!("the merged list contains Inbox rows"),
            Self::Move(destination) => {
                let name = match destination {
                    Destination::Role(role) => roles.name(*role).to_owned(),
                    Destination::Folder(name) => store
                        .folder_names(account)?
                        .into_iter()
                        .find(|found| found.eq_ignore_ascii_case(name))
                        .ok_or_else(|| {
                            fell_core::Error::Action("Destination folder is unavailable".into())
                        })?,
                };
                store.move_scoped_threads(account, &scopes(), &name)?
            }
            Self::Flag(change) => {
                let action = store.flag_threads(account, &threads, folder, *change)?;
                return Ok((threads.len(), action.map(|action| action.id)));
            }
            Self::Triage(TriageKind::Snooze(until)) => {
                return store.set_thread_snoozes_with_action(account, &threads, Some(*until));
            }
            Self::Triage(TriageKind::Unsnooze) => {
                return store.set_thread_snoozes_with_action(account, &threads, None);
            }
            Self::Triage(TriageKind::Mute) => {
                let (_, action) = store.toggle_thread_mutes_with_action(account, &threads)?;
                return Ok((threads.len(), action));
            }
            Self::Label(change) => {
                let ids = threads.iter().map(|id| id.0.clone()).collect::<Vec<_>>();
                let current = store.thread_labels(account, &ids)?;
                let changed = threads
                    .into_iter()
                    .filter(|id| {
                        let Some(row) = current.get(id.as_str()) else {
                            return false;
                        };
                        match change {
                            fell_core::LabelChange::Add(label) => !row.labels.contains(label),
                            fell_core::LabelChange::Remove(label) => row.labels.contains(label),
                        }
                    })
                    .collect::<Vec<_>>();
                if changed.is_empty() {
                    return Ok((0, None));
                }
                let action = store.label_threads(account, &changed, change.clone())?;
                (action, changed.len())
            }
        };
        Ok((count, Some(action.id)))
    }

    fn notice(&self, count: usize, accounts: usize, undo: &str) -> Option<Toast> {
        let text = match self {
            Self::Remove(Removal::Archive) if accounts > 1 => {
                format!("Archived {count} from {accounts} accounts")
            }
            Self::Remove(Removal::Archive) if count == 1 => "Archived".into(),
            Self::Remove(Removal::Archive) => format!("Archived {count}"),
            Self::Remove(Removal::Delete) if count == 1 => "Moved to Trash".into(),
            Self::Remove(Removal::Delete) => format!("Moved {count} to Trash"),
            Self::Remove(Removal::Spam) if count == 1 => "Moved to Spam".into(),
            Self::Remove(Removal::Spam) => format!("Moved {count} to Spam"),
            Self::Move(to) if count == 1 => format!("Moved to {}", to.name()),
            Self::Move(to) => format!("Moved {count} to {}", to.name()),
            Self::Triage(TriageKind::Snooze(until)) => {
                let time = crate::model::snooze::toast_time(super::now_unix(), *until);
                if count == 1 {
                    format!("Snoozed until {time}")
                } else {
                    format!("Snoozed {count} until {time}")
                }
            }
            Self::Triage(TriageKind::Unsnooze) if count == 0 => {
                return Some(Toast {
                    text: "No snoozes to clear".into(),
                    alert: false,
                    hint: None,
                    details: false,
                    label: None,
                });
            }
            Self::Triage(TriageKind::Unsnooze) => "Unsnoozed".into(),
            Self::Triage(TriageKind::Mute) if count == 1 => "Muted".into(),
            Self::Triage(TriageKind::Mute) => format!("Muted {count}"),
            Self::Label(fell_core::LabelChange::Add(name)) => format!("Applied {name}"),
            Self::Label(fell_core::LabelChange::Remove(name)) => format!("Removed {name}"),
            Self::Flag(_) | Self::Remove(Removal::Purge { .. }) => return None,
        };
        Some(Toast::with_hint(text, undo.to_owned(), ""))
    }
}

#[derive(Clone)]
pub(super) struct UndoEntry {
    pub account: AccountId,
    pub action: i64,
    pub owner: gpui::WeakEntity<Inbox>,
}

fn by_account(ids: &[String]) -> Vec<(AccountId, Vec<ThreadId>)> {
    let mut batches = BTreeMap::<_, Vec<_>>::new();
    for (account, thread) in ids.iter().filter_map(|id| owner_and_thread(id)) {
        batches.entry(account.0).or_default().push(thread);
    }
    batches
        .into_iter()
        .map(|(id, threads)| (AccountId(id), threads))
        .collect()
}

fn folder_options(inbox: &Inbox) -> Vec<Destination> {
    let roles = &inbox.session.roles;
    present::merge_folders(roles, &inbox.session.folders)
        .into_iter()
        .filter(|name| !roles.is(FolderRole::Inbox, name) && !roles.is_view(name))
        .map(|name| {
            roles
                .role_of(&name)
                .map_or(Destination::Folder(name), Destination::Role)
        })
        .collect()
}

fn refresh_owner(owner: &Entity<Inbox>, cx: &mut gpui::App) {
    owner.update(cx, |inbox, cx| {
        if let Some(queue) = &inbox.notifications {
            queue.refresh();
        }
        inbox.send_to_server();
        if inbox.session.search.is_some() {
            inbox.refresh_search(cx);
        }
        inbox.request_threads(
            ReadIntent {
                changed: true,
                refresh_open: true,
                ..ReadIntent::default()
            },
            cx,
        );
    });
}

impl Inbox {
    pub(super) fn prepare_all_move(&mut self, cx: &mut Context<Self>) {
        let Some(all) = &mut self.all_inboxes else {
            return;
        };
        let groups = by_account(&self.session.action_targets());
        let mut options: Vec<(Destination, Vec<String>)> = Vec::new();
        for (account, _) in &groups {
            let Some(owner) = all.owner(account) else {
                continue;
            };
            let inbox = owner.read(cx);
            let (name, _) =
                super::account_header(&inbox.session.account_name, &inbox.session.account_email);
            for destination in folder_options(inbox) {
                if let Some((_, owners)) = options
                    .iter_mut()
                    .find(|(option, _)| destination.matches(option))
                {
                    owners.push(name.to_owned());
                } else {
                    options.push((destination, vec![name.to_owned()]));
                }
            }
        }
        options.sort_by_key(|(_, owners)| owners.len() != groups.len());
        all.destinations.clear();
        let choices = &mut self.session.all_move_choices;
        choices.clear();
        for (destination, owners) in options {
            let name = destination.name().to_owned();
            if choices
                .iter()
                .any(|choice| choice.name.eq_ignore_ascii_case(&name))
            {
                continue;
            }
            let unavailable = owners.len() != groups.len();
            let detail = unavailable.then(|| format!("only in {}", owners.join(", ")));
            if !unavailable {
                all.destinations.insert(name.clone(), destination);
            }
            choices.push(PickerChoice {
                name: name.clone(),
                title: name,
                detail,
                date: None,
                mark: None,
                divider: false,
                unavailable,
            });
        }
        if choices.is_empty() {
            self.session.dismiss_overlay();
            self.session
                .show_toast("No shared destination folder", false);
        }
    }

    pub(super) fn all_removal(
        &mut self,
        ids: Vec<String>,
        removal: Removal,
        cx: &mut Context<Self>,
    ) {
        self.edit_all(ids, Edit::Remove(removal), cx);
    }

    pub(super) fn all_move(&mut self, edit: MoveEdit, cx: &mut Context<Self>) {
        let ids = edit.scopes.into_iter().map(|(id, _)| id.0).collect();
        let all = self.all_inboxes.as_ref().expect("merged inbox");
        if let Some(destination) = all.destinations.get(&edit.destination).cloned() {
            self.edit_all(ids, Edit::Move(destination), cx);
        }
    }

    pub(super) fn all_triage(&mut self, edit: TriageEdit, cx: &mut Context<Self>) {
        self.edit_all(edit.ids, Edit::Triage(edit.kind), cx);
    }

    pub(super) fn all_flags(&mut self, edit: FlagEdit, cx: &mut Context<Self>) {
        self.edit_all(edit.ids, Edit::Flag(edit.change), cx);
    }

    pub(super) fn all_labels(&mut self, edit: LabelEdit, cx: &mut Context<Self>) {
        let change = if edit.add {
            fell_core::LabelChange::Add(edit.name)
        } else {
            fell_core::LabelChange::Remove(edit.name)
        };
        self.edit_all(edit.ids, Edit::Label(change), cx);
    }

    fn edit_all(&mut self, ids: Vec<String>, edit: Edit, cx: &mut Context<Self>) {
        let groups = by_account(&ids);
        if groups.is_empty() {
            return;
        }
        let all = self.all_inboxes.as_mut().expect("merged inbox");
        let serial = all.begin_edit();
        let jobs: Vec<_> = groups
            .into_iter()
            .map(|(account, threads)| {
                let edit = edit.clone();
                let owner = all.owner(&account).map(Entity::downgrade);
                let reply = all.owner(&account).and_then(|view| {
                    let inbox = view.read(cx);
                    (!inbox.retired && inbox.session.sync != SyncKind::StoreError).then(|| {
                        let account = account.clone();
                        inbox
                            .store
                            .call(move |store| edit.apply(store, &account, threads))
                    })
                });
                async move {
                    let result = match reply {
                        Some(reply) => reply
                            .await
                            .map_err(|_| "A mailbox store stopped".to_string())
                            .and_then(|result| result.map_err(|err| err.to_string())),
                        None => Err("An account is unavailable".into()),
                    };
                    (account, owner, result)
                }
            })
            .collect();
        let key = self.selected_key();
        let removes = edit.removes_rows();
        if removes {
            self.session.picked.clear();
            self.session.visual = false;
            self.session.anchor_id = None;
            self.session.cancel_read_delay();
            self.take_out_of_list(&ids, cx);
        }
        let flag_write = if let Edit::Flag(change) = edit {
            self.flag_gen += 1;
            let generation = self.flag_gen;
            for id in &ids {
                self.protect_flag(id, change, generation, None);
            }
            Some((change, generation))
        } else {
            None
        };
        cx.spawn(async move |this, cx| {
            let results = join_all(jobs).await;
            let _ = this.update(cx, |inbox, cx| {
                let mut entries = Vec::new();
                let mut failure = None;
                let mut count = 0;
                for (account, owner, result) in results {
                    match result {
                        Ok((changed, action)) => {
                            count += changed;
                            if let (Some(action), Some(owner)) = (action, owner) {
                                if let Some(view) = owner.upgrade() {
                                    refresh_owner(&view, cx);
                                }
                                entries.push(UndoEntry {
                                    account,
                                    action,
                                    owner,
                                });
                            }
                        }
                        Err(error) => {
                            failure.get_or_insert(error);
                        }
                    }
                }
                for id in ids {
                    if removes {
                        inbox.pending_removals.remove(&id);
                    }
                    if let Some((change, generation)) = flag_write
                        && let Some(flags) = inbox.pending_flags.get_mut(&id)
                    {
                        let slot = flags.slot(change);
                        if let Some(flag) = slot.as_mut().filter(|flag| flag.write == generation) {
                            flag.through = Some(inbox.read_gen);
                        }
                    }
                }
                let failed = failure.is_some();
                if let Some(error) = failure {
                    let text = if entries.is_empty() {
                        error
                    } else {
                        format!("Some changes were saved · {error}")
                    };
                    inbox.session.show_toast(text, true);
                } else {
                    let undo = crate::keymap::binding_text(
                        &inbox.session.keymap,
                        crate::keymap::Action::Undo,
                    );
                    if let Some(toast) = edit.notice(count, entries.len(), &undo) {
                        inbox.session.set_toast(toast);
                        inbox.expire_action_notice(cx);
                    }
                }
                inbox.finish_all_edit(serial, entries, cx);
                inbox.request_threads(
                    ReadIntent {
                        changed: true,
                        refresh_open: true,
                        select: key.filter(|_| failed && removes),
                        ..ReadIntent::default()
                    },
                    cx,
                );
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn toggle_all_flags(
        &mut self,
        ids: Vec<String>,
        star: bool,
        cx: &mut Context<Self>,
    ) {
        let all = self.all_inboxes.as_ref().expect("merged inbox");
        let jobs: Vec<_> = by_account(&ids)
            .into_iter()
            .filter_map(|(account, threads)| {
                let store = &all.owner(&account)?.read(cx).store;
                Some(store.call(move |store| {
                    let roles = store.folder_roles(&account)?;
                    store.folder_selection_flags(
                        &account,
                        &threads,
                        roles.name(FolderRole::Inbox),
                        star,
                    )
                }))
            })
            .collect();
        cx.spawn(async move |this, cx| {
            let results = join_all(jobs).await;
            let _ = this.update(cx, |inbox, cx| {
                let mut count = 0;
                let mut all_marked = true;
                for result in results {
                    match result {
                        Ok(Ok((found, marked))) => {
                            count += found;
                            all_marked &= marked;
                        }
                        _ => {
                            inbox
                                .session
                                .show_toast("Could not read the selected flags", true);
                            cx.notify();
                            return;
                        }
                    }
                }
                if count == 0 {
                    inbox.session.show_toast("No conversation", false);
                    cx.notify();
                    return;
                }
                let change = match (star, all_marked) {
                    (true, true) => FlagChange::Unstar,
                    (true, false) => FlagChange::Star,
                    (false, true) => FlagChange::Read,
                    (false, false) => FlagChange::Unread,
                };
                inbox.session.mark_rows(ids.clone(), change);
                inbox.edit_all(ids, Edit::Flag(change), cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn read_all_label_targets(&mut self, cx: &mut Context<Self>) {
        self.label_targets_read += 1;
        let generation = self.label_targets_read;
        let all = self.all_inboxes.as_ref().expect("merged inbox");
        let jobs: Vec<_> = by_account(self.session.label_target_ids())
            .into_iter()
            .filter_map(|(account, threads)| {
                let store = &all.owner(&account)?.read(cx).store;
                Some(store.call(move |store| {
                    let ids = threads
                        .into_iter()
                        .map(|thread| thread.0)
                        .collect::<Vec<_>>();
                    store.thread_labels(&account, &ids).map(|labels| {
                        labels
                            .into_iter()
                            .map(|(id, row)| (row_id(&account, &ThreadId(id)), row))
                            .collect::<HashMap<_, _>>()
                    })
                }))
            })
            .collect();
        cx.spawn(async move |this, cx| {
            let results = join_all(jobs).await;
            let mut labels = HashMap::new();
            let result = results
                .into_iter()
                .try_for_each(|result| {
                    labels.extend(
                        result
                            .map_err(|_| "A mailbox store stopped".to_string())?
                            .map_err(|err| err.to_string())?,
                    );
                    Ok::<_, String>(())
                })
                .map(|()| labels);
            let _ = this.update(cx, |inbox, cx| {
                if inbox.label_targets_read == generation && inbox.session.label_picker_active() {
                    inbox.session.load_label_targets(result);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn finish_all_edit(
        &mut self,
        serial: u64,
        entries: Vec<UndoEntry>,
        cx: &mut Context<Self>,
    ) {
        let all = self.all_inboxes.as_mut().expect("merged inbox");
        all.pending.insert(serial, Some(entries));
        while let Some((&next, Some(_))) = all.pending.first_key_value() {
            if let Some(Some(entries)) = all.pending.remove(&next)
                && !entries.is_empty()
            {
                all.undo.push(entries);
            }
        }
        if all.pending.is_empty() && std::mem::take(&mut all.undo_requested) {
            self.undo_all(cx);
        }
    }

    pub(super) fn undo_all(&mut self, cx: &mut Context<Self>) {
        let all = self.all_inboxes.as_mut().expect("merged inbox");
        if !all.pending.is_empty() || self.undos_in_flight > 0 {
            all.undo_requested = true;
            return;
        }
        let Some(entries) = all.undo.pop() else {
            self.session
                .show_toast("Nothing to undo in All inboxes", false);
            return;
        };
        let undo_index = all.undo.len();
        self.undos_in_flight += 1;
        let jobs: Vec<_> = entries
            .into_iter()
            .map(|entry| {
                let owner = all
                    .owner(&entry.account)
                    .filter(|view| view.downgrade() == entry.owner);
                let reply = owner.map(|view| {
                    let account = entry.account.clone();
                    let action = entry.action;
                    view.read(cx).store.call(move |store| {
                        let (undone, restored) = store.undo_notification(&account, action)?;
                        let key = store.folder_roles(&account).ok().and_then(|roles| {
                            let folder = roles.name(FolderRole::Inbox);
                            restored
                                .iter()
                                .filter_map(|thread| {
                                    store.folder_key_of(&account, folder, thread).ok().flatten()
                                })
                                .min()
                                .map(|key| owned_key(&account, key))
                        });
                        Ok::<_, fell_core::Error>((undone.kind, key))
                    })
                });
                async move {
                    let result = match reply {
                        Some(reply) => reply.await.unwrap_or_else(|_| {
                            Err(fell_core::Error::Action("A mailbox store stopped".into()))
                        }),
                        None => Err(fell_core::Error::Action(
                            "An account was reopened or removed".into(),
                        )),
                    };
                    (entry, result)
                }
            })
            .collect();
        cx.spawn(async move |this, cx| {
            let results = join_all(jobs).await;
            let _ = this.update(cx, |inbox, cx| {
                inbox.undos_in_flight -= 1;
                let mut restored = None;
                let mut failure = None;
                let mut remaining = Vec::new();
                let mut notice = None;
                for (entry, result) in results {
                    match result {
                        Ok((kind, key)) => {
                            let text = super::undo_notice(kind, false);
                            notice = Some(if notice.is_none_or(|previous| previous == text) {
                                text
                            } else {
                                "Undone"
                            });
                            restored = restored.into_iter().chain(key).min();
                            if let Some(owner) = entry.owner.upgrade() {
                                refresh_owner(&owner, cx);
                            }
                        }
                        Err(fell_core::Error::Action(ref text)) if text == "nothing to undo" => {}
                        Err(error) => {
                            remaining.push(entry);
                            failure.get_or_insert(error.to_string());
                        }
                    }
                }
                let all = inbox.all_inboxes.as_mut().expect("merged inbox");
                if !remaining.is_empty() {
                    all.undo.insert(undo_index.min(all.undo.len()), remaining);
                }
                let again = std::mem::take(&mut all.undo_requested);
                let failed = failure.is_some();
                if let Some(error) = failure {
                    let text = if notice.is_some() {
                        format!("Undid part of the change · {error}")
                    } else {
                        format!("Couldn't undo · {error}")
                    };
                    inbox.session.show_toast(text, true);
                } else {
                    inbox
                        .session
                        .show_toast(notice.unwrap_or("Nothing to undo in All inboxes"), false);
                    inbox.expire_action_notice(cx);
                }
                inbox.request_threads(
                    ReadIntent {
                        changed: true,
                        refresh_open: true,
                        select: restored,
                        ..ReadIntent::default()
                    },
                    cx,
                );
                if again && !failed {
                    inbox.undo_all(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use fell_core::{Account, AccountConfig, Message, MessageId, PasswordSource, RemoteId};

    use super::*;
    use crate::keymap::{KeyId, Keymap};
    use crate::model::Session;

    fn account(id: &str) -> (Store, Account) {
        let mut config = AccountConfig::bridge(
            format!("{id}@example.test"),
            id,
            PasswordSource::Inline("fixture".into()),
        );
        config.id = id.into();
        config.write_back = true;
        let account = config.account();
        let store = Store::open_in_memory().unwrap();
        store.configure_connection(&config).unwrap();
        (store, account)
    }

    fn sample() -> Message {
        let fixture = crate::fixtures::fixture_account();
        let store = Store::open_in_memory().unwrap();
        crate::fixtures::seed_if_empty(&store, &fixture).unwrap();
        let thread = store.list_threads(&fixture.id).unwrap().remove(0).thread_id;
        store
            .thread_messages(&fixture.id, &thread)
            .unwrap()
            .remove(0)
    }

    fn insert(
        mailbox: &(Store, Account),
        pattern: &Message,
        id: &str,
        uid: u32,
        date: i64,
    ) -> ThreadId {
        let mut message = pattern.clone();
        message.account_id = mailbox.1.id.clone();
        message.headers.message_id = MessageId::new(format!("{id}@example.test"));
        message.headers.proton_conversation_id = None;
        message.headers.in_reply_to = None;
        message.headers.references.clear();
        message.headers.subject = format!("Conversation {id}");
        message.headers.date_unix = date;
        message.remote_id = RemoteId(format!("imap:1:{uid}:INBOX"));
        message.thread_id = ThreadId::from_message_id(&message.headers.message_id);
        message.folders = vec!["INBOX".into()];
        message.unread = true;
        message.starred = false;
        message.archived = false;
        message.labels.clear();
        message.snooze_until = None;
        mailbox.0.upsert_message(&message).unwrap()
    }

    fn merged(request: PageRequest, mailboxes: &[&(Store, Account)]) -> ListRead {
        let pages = mailboxes
            .iter()
            .map(|(store, account)| {
                request
                    .read(store, &account.id, &account.email, account.id.as_str())
                    .unwrap()
            })
            .collect();
        request.merge(pages)
    }

    #[test]
    fn equal_activity_pages_have_no_gaps_and_selection_stays_on_its_account() {
        let personal = account("personal");
        let work = account("work");
        let sample = sample();
        let shared = insert(&personal, &sample, "same", 1, 10);
        assert_eq!(insert(&work, &sample, "same", 1, 10), shared);
        let older = insert(&personal, &sample, "older", 2, 5);
        let newest = insert(&work, &sample, "newest", 2, 20);
        let stores = [&work, &personal];
        let top = PageRequest {
            span: FolderSpan::Top { rows: 2 },
            center: 3,
            follow: None,
            labels: false,
        };
        let page = merged(top.clone(), &stores);
        let personal_row = row_id(&personal.1.id, &shared);
        let work_row = row_id(&work.1.id, &shared);
        assert_eq!(
            page.rows
                .iter()
                .map(|row| row.id.clone())
                .collect::<Vec<_>>(),
            [row_id(&work.1.id, &newest), personal_row.clone()]
        );
        let boundary = super::super::row_key(page.rows.last().unwrap());
        let next = merged(
            PageRequest {
                span: FolderSpan::Window {
                    first: Some(boundary.clone()),
                    last: boundary,
                    before: 1,
                    after: 2,
                },
                ..top.clone()
            },
            &stores,
        );
        assert_eq!(
            next.rows
                .iter()
                .map(|row| row.id.clone())
                .collect::<Vec<_>>(),
            [
                row_id(&work.1.id, &newest),
                personal_row,
                work_row.clone(),
                row_id(&personal.1.id, &older)
            ]
        );
        assert!(!next.more_before && !next.more_after);
        assert_eq!(next.unread, 4);
        assert_eq!(next.rows[1].account_label.as_deref(), Some("personal"));
        assert_eq!(next.rows[2].account_label.as_deref(), Some("work"));

        let mut session = Session::new("", "", SyncKind::LocalOffline, Keymap::default());
        session.set_page(next.rows, next.more_before, next.more_after, next.unread);
        assert!(session.select_if_present(&work_row));
        for (mailbox, id, date) in [
            (&personal, "personal-arrival", 30),
            (&work, "work-arrival", 40),
        ] {
            insert(mailbox, &sample, id, 3, date);
            let request = PageRequest {
                span: session.span(Default::default(), None, None),
                center: session.threads.len() + 2,
                follow: Some((work.1.id.clone(), shared.clone())),
                ..top.clone()
            };
            let refreshed = merged(request, &stores);
            assert!(refreshed.followed.is_some());
            session.set_page(
                refreshed.rows,
                refreshed.more_before,
                refreshed.more_after,
                refreshed.unread,
            );
            assert_eq!(session.cursor_id.as_deref(), Some(work_row.as_str()));
        }
    }

    #[test]
    fn inbox_actions_and_their_undo_use_only_the_owning_store() {
        let personal = account("personal");
        let work = account("work");
        let sample = sample();
        let shared = insert(&personal, &sample, "same", 1, 10);
        assert_eq!(insert(&work, &sample, "same", 1, 10), shared);
        let mailboxes = HashMap::from([
            (personal.1.id.clone(), &personal.0),
            (work.1.id.clone(), &work.0),
        ]);
        let personal_row = row_id(&personal.1.id, &shared);
        let work_row = row_id(&work.1.id, &shared);
        let target = [work_row.clone()];
        let apply = |ids: &[String], edit: Edit| {
            by_account(ids)
                .into_iter()
                .map(|(account, threads)| {
                    let (count, action) =
                        edit.apply(mailboxes[&account], &account, threads).unwrap();
                    assert_eq!(count, 1);
                    (account, action.unwrap())
                })
                .collect::<Vec<_>>()
        };
        let undo = |actions: Vec<(AccountId, i64)>| {
            for (account, action) in actions {
                mailboxes[&account]
                    .undo_notification(&account, action)
                    .unwrap();
            }
        };
        let visible = |(store, account): &(Store, Account)| {
            store
                .folder_key_of(&account.id, "INBOX", &shared)
                .unwrap()
                .is_some()
        };
        let message = |(store, account): &(Store, Account)| {
            store
                .thread_messages(&account.id, &shared)
                .unwrap()
                .remove(0)
        };
        for edit in [
            Edit::Remove(Removal::Delete),
            Edit::Remove(Removal::Spam),
            Edit::Move(Destination::Role(FolderRole::Archive)),
            Edit::Triage(TriageKind::Snooze(super::super::now_unix() + 86_400)),
            Edit::Triage(TriageKind::Mute),
        ] {
            let undo_entries = apply(&target, edit);
            assert!(!visible(&work));
            assert!(visible(&personal));
            undo(undo_entries);
            assert!(visible(&work));
        }
        let undo_entries = apply(&target, Edit::Flag(FlagChange::Read));
        assert!(!message(&work).unread && message(&personal).unread);
        undo(undo_entries);
        let undo_entries = apply(&target, Edit::Flag(FlagChange::Star));
        assert!(message(&work).starred && !message(&personal).starred);
        undo(undo_entries);
        let undo_entries = apply(
            &target,
            Edit::Label(fell_core::LabelChange::Add("Follow up".into())),
        );
        assert_eq!(message(&work).labels, ["Follow up"]);
        assert!(message(&personal).labels.is_empty());
        undo(undo_entries);

        // Exercise both selection styles against colliding raw thread IDs.
        let page = merged(
            PageRequest {
                span: FolderSpan::Top { rows: 10 },
                center: 10,
                follow: None,
                labels: false,
            },
            &[&personal, &work],
        );
        for visual in [false, true] {
            let mut session = Session::new("", "", SyncKind::LocalOffline, Keymap::default());
            session.set_page(page.rows.clone(), false, false, page.unread);
            assert!(session.select_if_present(&personal_row));
            session.on_key(KeyId::Char(if visual { 'V' } else { 'x' }), 10);
            session.on_key(KeyId::Char('j'), 10);
            if !visual {
                session.on_key(KeyId::Char('x'), 10);
            }
            let selected = session.action_targets();
            assert_eq!(selected, [personal_row.clone(), work_row.clone()]);
            let actions = apply(&selected, Edit::Remove(Removal::Archive));
            assert!(!visible(&personal) && !visible(&work));
            for (account, action) in &actions {
                let outbox = mailboxes[account].outbox(account).unwrap();
                assert!(
                    outbox.iter().any(|write| write.action_id == Some(*action)),
                    "archive write-back belongs to {account:?}"
                );
            }
            undo(actions);
            assert!(visible(&personal) && visible(&work));
        }
    }
}
