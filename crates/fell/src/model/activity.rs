//! Account and agent activity, with confirmations tied to a single log entry.

use std::sync::Arc;

use fell_core::agent::{
    ActivityBlocker, ActivityDirection, AgentActivity, AgentActivityPage, BULK_UNDO_THRESHOLD,
};
use fell_core::{AccountId, AgentSummary, Store};

use crate::keymap::{Action, KeyId, PaletteItem, fuzzy};

const PAGE_SIZE: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Load {
        account: Option<AccountId>,
        filter: Option<AgentSummary>,
        direction: ActivityDirection,
    },
    Undo {
        account: AccountId,
        id: i64,
        confirmed_bulk: Option<u32>,
    },
}

impl Request {
    pub fn load(
        self,
        store: &Store,
        current_account: &AccountId,
    ) -> fell_core::Result<AgentActivityPage> {
        let Self::Load {
            account,
            filter,
            direction,
        } = self
        else {
            unreachable!("activity load request")
        };
        let agent = filter.as_ref().map(|agent| agent.agent_id.as_str());
        store.agent_activity(
            account.as_ref().unwrap_or(current_account),
            agent,
            direction,
            PAGE_SIZE,
        )
    }
}

#[derive(Clone, Debug, Default)]
pub struct Activity {
    account: Option<AccountId>,
    pub filter: Option<AgentSummary>,
    pub page: Arc<AgentActivityPage>,
    pub selected: usize,
    pub generation: u64,
    pub page_revision: u64,
    pub pending: bool,
    pub pending_bulk_undo: Option<i64>,
    pub loading_slow: bool,
    /// The pending request fetches a page (not an undo), so a slow one may
    /// say the list is loading.
    pub loading_page: bool,
    pub load_failed: bool,
    pub confirmation: Option<Confirmation>,
    pub blocker_notice: Option<ActivityBlocker>,
    pub opened_from_agents: bool,
    jump_from: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct Confirmation {
    pub account: AccountId,
    pub id: i64,
    pub conversations: u32,
}

impl Activity {
    pub fn open(&mut self, filter: Option<AgentSummary>) -> Request {
        self.opened_from_agents = filter.is_some();
        self.account = filter.as_ref().map(|agent| agent.account_id.clone());
        self.filter = filter;
        self.jump_from = None;
        self.page = Arc::default();
        self.selected = 0;
        self.confirmation = None;
        self.blocker_notice = None;
        self.load(ActivityDirection::Latest)
    }

    fn start(&mut self, request: Request) -> Request {
        self.pending = true;
        self.pending_bulk_undo = match &request {
            Request::Undo {
                id,
                confirmed_bulk: Some(_),
                ..
            } => Some(*id),
            _ => None,
        };
        self.loading_page = matches!(request, Request::Load { .. });
        self.loading_slow = false;
        self.generation = self.generation.wrapping_add(1);
        request
    }

    fn load(&mut self, direction: ActivityDirection) -> Request {
        self.load_failed = false;
        self.start(Request::Load {
            account: self.account.clone(),
            filter: self.filter.clone(),
            direction,
        })
    }

    pub fn loaded(&mut self, page: AgentActivityPage, direction: ActivityDirection) {
        // Notes can be pruned between reading two pages. Keep the reader's
        // current selection if the page they asked for disappeared.
        if page.entries.is_empty() {
            let current = Arc::make_mut(&mut self.page);
            match direction {
                ActivityDirection::Older(_) => current.has_older = false,
                ActivityDirection::Newer(_) => current.has_newer = false,
                ActivityDirection::Latest | ActivityDirection::At(_) => {}
            }
            if direction != ActivityDirection::Latest {
                return;
            }
        }
        self.selected = if matches!(direction, ActivityDirection::Newer(_)) {
            page.entries.len().saturating_sub(1)
        } else {
            0
        };
        self.page = Arc::new(page);
        self.page_revision = self.page_revision.wrapping_add(1);
    }

    pub fn page(&mut self, older: bool) -> Option<Request> {
        if self.pending || self.confirmation.is_some() {
            return None;
        }
        let direction = if older && self.page.has_older {
            ActivityDirection::Older(self.page.entries.last()?.action.id)
        } else if !older && self.page.has_newer {
            ActivityDirection::Newer(self.page.entries.first()?.action.id)
        } else {
            return None;
        };
        self.blocker_notice = None;
        Some(self.load(direction))
    }

    pub fn select(&mut self, index: usize) {
        if self.confirmation.is_none() && index < self.page.entries.len() {
            self.selected = index;
            self.blocker_notice = None;
        }
    }

    pub fn navigate(&mut self, key: KeyId) -> Option<Request> {
        let len = self.page.entries.len();
        match key {
            KeyId::Down | KeyId::Char('j') if self.selected + 1 >= len => return self.page(true),
            KeyId::Down | KeyId::Char('j') => self.select(self.selected + 1),
            KeyId::Up | KeyId::Char('k') if self.selected == 0 => return self.page(false),
            KeyId::Up | KeyId::Char('k') => self.select(self.selected - 1),
            KeyId::Home => self.select(0),
            KeyId::End => self.select(len.saturating_sub(1)),
            _ => {}
        }
        None
    }

    pub fn undo(&mut self) -> Option<Request> {
        if self.pending || self.confirmation.is_some() {
            return None;
        }
        self.blocker_notice = None;
        let entry = self.selected_entry()?;
        if entry.action.undone || entry.draft_discarded {
            return None;
        }
        if let Some(blocker) = entry.blocker.clone()
            && blocker.actor.starts_with("agent:")
        {
            let id = entry.action.id;
            if self.page.entries.iter().any(|entry| {
                entry.action.id == blocker.id && (entry.action.undone || entry.draft_discarded)
            }) {
                self.clear_blocker(blocker.id);
            } else {
                return self.follow_blocker(id, blocker);
            }
        }
        let entry = self.selected_entry()?;
        let target = Confirmation {
            account: entry.action.account_id.clone(),
            id: entry.action.id,
            conversations: entry.conversations,
        };
        // Recheck local, outbox, and mailbox conflicts at the store. A bulk
        // reversal still requires confirmation once the conflict clears.
        if entry.conversations > BULK_UNDO_THRESHOLD && entry.blocker.is_none() {
            self.confirmation = Some(target);
            return None;
        }
        Some(self.undo_request(target, None))
    }

    pub fn request_confirmation(&mut self, id: i64, conversations: u32) {
        let Some(entry) = Arc::make_mut(&mut self.page)
            .entries
            .get_mut(self.selected)
            .filter(|entry| entry.action.id == id)
        else {
            return;
        };
        entry.blocker = None;
        entry.conversations = conversations;
        self.confirmation = Some(Confirmation {
            account: entry.action.account_id.clone(),
            id,
            conversations,
        });
    }

    pub fn confirm(&mut self, yes: bool) -> Option<Request> {
        let target = self.confirmation.take()?;
        yes.then(|| {
            let count = target.conversations;
            self.undo_request(target, Some(count))
        })
    }

    fn undo_request(&mut self, target: Confirmation, confirmed_bulk: Option<u32>) -> Request {
        self.start(Request::Undo {
            account: target.account,
            id: target.id,
            confirmed_bulk,
        })
    }

    pub fn mark_undone(&mut self, id: i64, snooze_retired: bool) {
        if let Some(entry) = Arc::make_mut(&mut self.page)
            .entries
            .iter_mut()
            .find(|entry| entry.action.id == id)
        {
            entry.action.undone = true;
            entry.snooze_retired = snooze_retired;
            entry.blocker = None;
        }
        self.clear_blocker(id);
    }

    fn clear_blocker(&mut self, id: i64) {
        for entry in &mut Arc::make_mut(&mut self.page).entries {
            if entry
                .blocker
                .as_ref()
                .is_some_and(|blocker| blocker.actor.starts_with("agent:") && blocker.id == id)
            {
                entry.blocker = None;
            }
        }
    }

    pub fn record_blocker(&mut self, id: i64, blocker: ActivityBlocker) -> Option<Request> {
        let entry = Arc::make_mut(&mut self.page)
            .entries
            .iter_mut()
            .find(|entry| entry.action.id == id)?;
        entry.blocker = Some(blocker.clone());
        if self
            .selected_entry()
            .is_some_and(|entry| entry.action.id == id)
        {
            self.follow_blocker(id, blocker)
        } else {
            None
        }
    }

    fn follow_blocker(&mut self, id: i64, blocker: ActivityBlocker) -> Option<Request> {
        self.blocker_notice = Some(blocker.clone());
        if !blocker.actor.starts_with("agent:") {
            return None;
        }
        if let Some(index) = self.page.entries.iter().position(|entry| {
            entry.action.id == blocker.id && !entry.action.undone && !entry.draft_discarded
        }) {
            self.selected = index;
            return None;
        }
        self.jump_from = Some(id);
        let direction = ActivityDirection::At(blocker.id);
        let mut request = self.load(direction);
        if let Request::Load { filter, .. } = &mut request
            && filter.as_ref().is_some_and(|agent| {
                blocker.actor.strip_prefix("agent:") != Some(agent.agent_id.as_str())
            })
        {
            *filter = None;
        }
        Some(request)
    }

    /// An off-page blocker can have been undone or discarded since the log
    /// was read. Keep the original page in that case and retry its selection.
    pub fn finish_jump(
        &mut self,
        page: &AgentActivityPage,
        id: i64,
        account_wide: bool,
    ) -> (bool, Option<Request>) {
        let Some(from) = self.jump_from.take() else {
            return (true, None);
        };
        if page.entries.first().is_some_and(|entry| {
            entry.action.id == id && !entry.action.undone && !entry.draft_discarded
        }) {
            if account_wide {
                self.filter = None;
            }
            return (true, None);
        }
        self.clear_blocker(id);
        let retry = if self
            .selected_entry()
            .is_some_and(|entry| entry.action.id == from)
        {
            self.undo()
        } else {
            None
        };
        (false, retry)
    }

    pub fn selected_entry(&self) -> Option<&AgentActivity> {
        self.page.entries.get(self.selected)
    }

    pub fn palette(&self, query: &str) -> Vec<PaletteItem> {
        let mut items = [
            (
                Action::UndoAgentActivity,
                "u",
                !self.pending
                    && self
                        .selected_entry()
                        .is_some_and(|entry| !entry.action.undone && !entry.draft_discarded),
            ),
            (Action::PageDown, "Ctrl-d", self.page.has_older),
            (Action::PageUp, "Ctrl-u", self.page.has_newer),
        ]
        .into_iter()
        .filter(|(_, _, available)| *available)
        .filter_map(|(action, binding, _)| {
            let label = match action {
                Action::PageDown => "Older activity",
                Action::PageUp => "Newer activity",
                _ => action.label(),
            };
            fuzzy(query, label).map(|score| {
                (
                    score,
                    PaletteItem {
                        action,
                        label: label.into(),
                        binding: binding.into(),
                        search: None,
                    },
                )
            })
        })
        .collect::<Vec<_>>();
        items.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        items.into_iter().map(|(_, item)| item).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Keymap;
    use crate::model::{Overlay, Session, Step, SyncKind};
    use fell_core::AgentGrant;
    use fell_core::agent::AgentActivity;

    fn session() -> (Store, AccountId, Session) {
        let account = crate::fixtures::fixture_account();
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account).unwrap();
        crate::fixtures::seed_if_empty(&store, &account).unwrap();
        let session = Session::new(
            account.display_name,
            account.email,
            SyncKind::LocalOffline,
            Keymap::default(),
        );
        (store, account.id, session)
    }

    #[test]
    fn activity_opens_from_selected_agent_and_account_palette() {
        let (store, account, mut session) = session();
        store
            .set_agent_grant(&account, "fern", AgentGrant::enabled(true, true, false))
            .unwrap();
        let Step {
            agents: Some(load), ..
        } = session.perform(Action::Agents, 10)
        else {
            panic!("expected agents to load");
        };
        session.agents.answer(load.run(&store).unwrap());
        let toggle = session.on_key(KeyId::Enter, 10).agents.unwrap();
        assert_eq!(session.overlay, Overlay::Agents);
        session.agents.answer(toggle.run(&store).unwrap());
        for _ in 0..4 {
            session.on_key(KeyId::Tab, 10);
        }
        let step = session.on_key(KeyId::Enter, 10);
        assert_eq!(session.overlay, Overlay::AgentActivity);
        assert!(
            matches!(step.activity, Some(Request::Load { filter: Some(agent), .. }) if agent.agent_id == "fern")
        );
        session.on_key(KeyId::Escape, 10);
        assert_eq!(session.overlay, Overlay::Agents);
        session.on_key(KeyId::Tab, 10);
        assert!(session.on_key(KeyId::Enter, 10).activity.is_none());
        assert!(session.agents.confirm.is_some());
        session.on_key(KeyId::Escape, 10);
        session.on_key(KeyId::Escape, 10);
        session.on_key(KeyId::Space, 10);
        session.on_key(KeyId::Char('a'), 10);
        assert!(
            session
                .which_items()
                .iter()
                .any(|item| item.label == "Agent activity")
        );
        session.on_key(KeyId::Escape, 10);
        session.on_key(KeyId::Char(':'), 10);
        let item = session
            .palette()
            .into_iter()
            .find(|item| item.action == Action::AgentActivity)
            .unwrap();
        let step = session.confirm_palette(item, 10);
        assert_eq!(session.overlay, Overlay::AgentActivity);
        assert!(matches!(
            step.activity,
            Some(Request::Load { filter: None, .. })
        ));
    }

    #[test]
    fn activity_bulk_prompt_can_be_dismissed_and_confirms_only_its_entry() {
        let (store, account, mut session) = session();
        let thread = store.list_threads(&account).unwrap().remove(0).thread_id;
        store
            .set_agent_grant(&account, "fern", AgentGrant::enabled(false, true, false))
            .unwrap();
        let action = store
            .agent_archive_thread(&account, &thread, "agent:fern")
            .unwrap()
            .unwrap();
        let step = session.perform(Action::AgentActivity, 10);
        let mut page = step.activity.unwrap().load(&store, &account).unwrap();
        page.entries[0].conversations = BULK_UNDO_THRESHOLD + 1;
        let mut older = AgentActivity {
            conversations: 1,
            ..page.entries[0].clone()
        };
        older.action.id -= 1;
        page.entries.push(older);
        session.activity.loaded(page, ActivityDirection::Latest);
        session.activity.pending = false;

        assert!(session.on_key(KeyId::Char('u'), 10).activity.is_none());
        assert!(session.activity.confirmation.is_some());
        assert!(session.on_key(KeyId::Escape, 10).activity.is_none());
        assert!(session.activity.confirmation.is_none());
        assert!(session.on_key(KeyId::Char('y'), 10).activity.is_none());

        assert!(session.on_key(KeyId::Char('u'), 10).activity.is_none());
        session.activity.select(1);
        assert_eq!(session.activity.selected, 0);
        let step = session.on_key(KeyId::Char('y'), 10);
        assert_eq!(
            step.activity,
            Some(Request::Undo {
                account,
                id: action.id,
                confirmed_bulk: Some(BULK_UNDO_THRESHOLD + 1),
            })
        );
        assert!(session.on_key(KeyId::Char('y'), 10).activity.is_none());
    }

    #[test]
    fn activity_keeps_selection_when_a_page_disappears() {
        let mut activity = Activity::default();
        let entry = AgentActivity {
            action: fell_core::RecordedAction {
                id: 20,
                account_id: AccountId("demo".into()),
                actor: "agent:fern".into(),
                kind: "annotate".into(),
                summary: String::new(),
                undone: false,
                created_unix: 20,
            },
            subject: None,
            conversations: 1,
            blocker: None,
            draft_discarded: false,
            snooze_retired: false,
            agent_off: false,
        };
        activity.loaded(
            AgentActivityPage {
                entries: vec![entry],
                has_newer: false,
                has_older: true,
            },
            ActivityDirection::Latest,
        );
        assert!(activity.page(true).is_some());
        activity.loaded(AgentActivityPage::default(), ActivityDirection::Older(20));
        assert_eq!(activity.page.entries[activity.selected].action.id, 20);
        assert!(!activity.page.has_older);
    }

    #[test]
    fn blocked_undo_jumps_to_the_latest_open_action_by_the_same_agent() {
        let (store, account, mut session) = session();
        let thread = store.list_threads(&account).unwrap().remove(0).thread_id;
        store
            .set_agent_grant(&account, "reed", AgentGrant::enabled(false, true, false))
            .unwrap();
        let read = store
            .mark_thread_read(&account, &thread, "agent:reed")
            .unwrap()
            .unwrap();
        let star = store
            .star_thread(&account, &thread, true, "agent:reed")
            .unwrap()
            .unwrap();
        let page = store
            .agent_activity(&account, None, ActivityDirection::Latest, 10)
            .unwrap();
        session.overlay = Overlay::AgentActivity;
        session.activity.loaded(page, ActivityDirection::Latest);
        session.activity.select(1);

        assert!(session.on_key(KeyId::Char('u'), 10).activity.is_none());
        assert_eq!(
            session.activity.selected_entry().unwrap().action.id,
            star.id
        );
        assert_eq!(
            session.activity.blocker_notice.as_ref().unwrap().id,
            star.id
        );
        session.activity.mark_undone(star.id, false);
        session.activity.select(1);
        let step = session.on_key(KeyId::Char('u'), 10);
        assert!(matches!(step.activity, Some(Request::Undo { id, .. }) if id == read.id));
    }

    #[test]
    fn an_off_page_blocker_undone_elsewhere_retries_the_original_row() {
        let (store, account, mut session) = session();
        let thread = store.list_threads(&account).unwrap().remove(0).thread_id;
        store
            .set_agent_grant(&account, "reed", AgentGrant::enabled(false, true, false))
            .unwrap();
        let read = store
            .mark_thread_read(&account, &thread, "agent:reed")
            .unwrap()
            .unwrap();
        let star = store
            .star_thread(&account, &thread, true, "agent:reed")
            .unwrap()
            .unwrap();
        session.activity.loaded(
            store
                .agent_activity(&account, None, ActivityDirection::Older(star.id), 1)
                .unwrap(),
            ActivityDirection::Older(star.id),
        );
        let request = session.activity.undo().unwrap();
        assert!(
            matches!(request, Request::Load { direction: ActivityDirection::At(id), .. } if id == star.id)
        );

        store.undo_agent_activity(&account, star.id, None).unwrap();
        let page = request.load(&store, &account).unwrap();
        session.activity.pending = false;
        let (show, retry) = session.activity.finish_jump(&page, star.id, true);
        assert!(!show);
        assert!(matches!(retry, Some(Request::Undo { id, .. }) if id == read.id));
        assert!(session.activity.selected_entry().unwrap().blocker.is_none());
    }

    #[test]
    fn activity_rechecks_a_cached_local_blocker_before_showing_bulk_confirmation() {
        let (store, account, mut session) = session();
        let thread = store.list_threads(&account).unwrap().remove(0).thread_id;
        store
            .set_agent_grant(&account, "fern", AgentGrant::enabled(false, true, false))
            .unwrap();
        let action = store
            .agent_move_thread(&account, &thread, "INBOX", "Work", "agent:fern")
            .unwrap()
            .unwrap();
        store
            .move_thread(&account, &thread, "Work", "Later")
            .unwrap();
        let mut page = store
            .agent_activity(&account, None, ActivityDirection::Latest, 10)
            .unwrap();
        assert_eq!(page.entries[0].blocker.as_ref().unwrap().actor, "local");
        page.entries[0].conversations = BULK_UNDO_THRESHOLD + 1;
        session.activity.loaded(page, ActivityDirection::Latest);
        assert!(
            matches!(session.activity.undo(), Some(Request::Undo { id, confirmed_bulk: None, .. }) if id == action.id)
        );
        assert!(session.activity.confirmation.is_none());
    }
}
