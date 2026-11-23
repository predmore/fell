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
        let Self::Load { filter, direction } = self else {
            unreachable!("activity load request")
        };
        let (account, agent) = filter.as_ref().map_or((current_account, None), |agent| {
            (&agent.account_id, Some(agent.agent_id.as_str()))
        });
        store.agent_activity(account, agent, direction, PAGE_SIZE)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Activity {
    pub filter: Option<AgentSummary>,
    pub page: Arc<AgentActivityPage>,
    pub selected: usize,
    pub generation: u64,
    pub page_revision: u64,
    pub pending: bool,
    pub loading_slow: bool,
    pub load_failed: bool,
    pub confirmation: Option<Confirmation>,
    pub blocker_notice: Option<(String, ActivityBlocker)>,
}

#[derive(Clone, Debug)]
pub struct Confirmation {
    pub account: AccountId,
    pub id: i64,
    pub conversations: u32,
}

impl Activity {
    pub fn open(&mut self, filter: Option<AgentSummary>) -> Request {
        self.filter = filter;
        self.page = Arc::default();
        self.selected = 0;
        self.confirmation = None;
        self.blocker_notice = None;
        self.load(ActivityDirection::Latest)
    }

    fn start(&mut self, request: Request) -> Request {
        self.pending = true;
        self.generation = self.generation.wrapping_add(1);
        request
    }

    fn load(&mut self, direction: ActivityDirection) -> Request {
        self.load_failed = false;
        self.loading_slow = false;
        self.start(Request::Load {
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
        if let Some(blocker) = entry.blocker.clone() {
            let actor = entry.action.actor.clone();
            self.blocker_notice = Some((actor, blocker.clone()));
            if let Some(index) = self
                .page
                .entries
                .iter()
                .position(|entry| entry.action.id == blocker.id)
            {
                self.selected = index;
                return None;
            }
            return Some(self.load(ActivityDirection::At(blocker.id)));
        }
        let target = Confirmation {
            account: entry.action.account_id.clone(),
            id: entry.action.id,
            conversations: entry.conversations,
        };
        if entry.conversations > BULK_UNDO_THRESHOLD {
            self.confirmation = Some(target);
            return None;
        }
        Some(self.undo_request(target, None))
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
        let entries = &mut Arc::make_mut(&mut self.page).entries;
        let Some(index) = entries.iter().position(|entry| entry.action.id == id) else {
            return;
        };
        let (undone, older) = entries[index..].split_first_mut().expect("found entry");
        undone.action.undone = true;
        undone.snooze_retired = snooze_retired;
        let mut next = None;
        for entry in older {
            if entry
                .blocker
                .as_ref()
                .is_some_and(|blocker| blocker.id == id)
            {
                entry.blocker = next.clone();
                next.get_or_insert_with(|| ActivityBlocker {
                    id: entry.action.id,
                    kind: entry.action.kind.clone(),
                    created_unix: entry.action.created_unix,
                });
            }
        }
    }

    pub fn record_blocker(&mut self, id: i64, blocker: ActivityBlocker) {
        if let Some(actor) = self
            .page
            .entries
            .iter()
            .find(|entry| entry.action.id == id)
            .map(|entry| entry.action.actor.clone())
        {
            for entry in &mut Arc::make_mut(&mut self.page).entries {
                if entry.action.actor == actor
                    && entry.action.id < blocker.id
                    && !entry.action.undone
                    && !entry.draft_discarded
                {
                    entry.blocker = Some(blocker.clone());
                }
            }
            self.blocker_notice = Some((actor, blocker));
        }
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
            session.activity.blocker_notice.as_ref().unwrap().1.id,
            star.id
        );
        session.activity.mark_undone(star.id, false);
        session.activity.select(1);
        let step = session.on_key(KeyId::Char('u'), 10);
        assert!(matches!(step.activity, Some(Request::Undo { id, .. }) if id == read.id));
    }
}
