//! Mailbox access controls, independent of the overlay that shows them.

use std::sync::Arc;

use fell_core::{Account, AgentGrant, AgentSummary, Store};

use crate::keymap::{Action, KeyId, PaletteItem, fuzzy};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Control {
    Read,
    Triage,
    Draft,
    Revoke,
}

impl Control {
    // Add Annotate here and to the scope accessors when #129 adds the grant.
    pub const ALL: [Self; 4] = [Self::Read, Self::Triage, Self::Draft, Self::Revoke];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Read => "Read",
            Self::Triage => "Triage",
            Self::Draft => "Draft",
            Self::Revoke => "Revoke",
        }
    }

    pub const fn command(self) -> &'static str {
        match self {
            Self::Read => "Toggle agent read access",
            Self::Triage => "Toggle agent triage access",
            Self::Draft => "Toggle agent draft access",
            Self::Revoke => "Revoke agent access",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Read => "Read mail and threads.",
            Self::Triage => "Archive, label, star, snooze, mark read. No deletes.",
            Self::Draft => "Write drafts for review. Only you can send.",
            Self::Revoke => "Remove all access to this account.",
        }
    }

    const fn key(self) -> char {
        match self {
            Self::Read => 'r',
            Self::Triage => 't',
            Self::Draft => 'd',
            Self::Revoke => 'x',
        }
    }

    pub fn scope(self, grant: AgentGrant) -> Option<bool> {
        let granted = match self {
            Self::Read => grant.read,
            Self::Triage => grant.triage,
            Self::Draft => grant.draft,
            Self::Revoke => return None,
        };
        Some(grant.enabled && granted)
    }

    fn toggle(self, grant: AgentGrant) -> AgentGrant {
        let mut grant = if grant.enabled {
            grant
        } else {
            AgentGrant::DISABLED
        };
        let scope = match self {
            Self::Read => &mut grant.read,
            Self::Triage => &mut grant.triage,
            Self::Draft => &mut grant.draft,
            Self::Revoke => return grant,
        };
        *scope = !*scope;
        grant.enabled = grant.read || grant.triage || grant.draft;
        grant
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Scope,
    Revoke,
    Undo,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Load,
    Change {
        row: AgentSummary,
        grant: AgentGrant,
        kind: ChangeKind,
    },
}

pub enum Reply {
    Loaded(Roster),
    Changed {
        before: AgentSummary,
        after: AgentSummary,
        kind: ChangeKind,
    },
    Stale(Roster),
}

pub struct Roster {
    accounts: Vec<Account>,
    rows: Vec<AgentSummary>,
}

impl Roster {
    fn load(store: &Store) -> fell_core::Result<Self> {
        Ok(Self {
            accounts: store.list_accounts()?,
            rows: store.list_agent_grants()?,
        })
    }
}

impl Request {
    pub fn run(self, store: &Store) -> fell_core::Result<Reply> {
        match self {
            Self::Load => Ok(Reply::Loaded(Roster::load(store)?)),
            Self::Change { row, grant, kind } => match store.update_agent_grant(&row, grant)? {
                Some(after) => Ok(Reply::Changed {
                    before: row,
                    after,
                    kind,
                }),
                None => Ok(Reply::Stale(Roster::load(store)?)),
            },
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Agents {
    pub rows: Arc<Vec<AgentSummary>>,
    accounts: Arc<Vec<Account>>,
    pub selected: usize,
    column: usize,
    pub pending: bool,
    pub confirm: Option<Confirmation>,
    undo: Option<(AgentSummary, AgentGrant)>,
    pub notice: Option<(&'static str, bool)>,
    pub notice_generation: u64,
}

#[derive(Clone, Debug)]
pub struct Confirmation {
    pub row: AgentSummary,
    pub control: Control,
}

impl Agents {
    pub fn open(&mut self) -> Option<Request> {
        self.confirm = None;
        if self.pending {
            return None;
        }
        self.set_notice(None);
        self.pending = true;
        Some(Request::Load)
    }

    pub fn control(&self) -> Control {
        Control::ALL[self.column]
    }

    pub fn show_accounts(&self) -> bool {
        self.accounts.len() > 1
    }

    pub fn email(&self, row: &AgentSummary) -> Option<&str> {
        self.accounts
            .iter()
            .find(|account| account.id == row.account_id)
            .map(|account| account.email.as_str())
    }

    pub fn select(&mut self, index: usize) {
        self.selected = index;
        if self.column >= self.controls() {
            self.column = 0;
        }
    }

    fn controls(&self) -> usize {
        Control::ALL.len()
            - usize::from(
                !self
                    .rows
                    .get(self.selected)
                    .is_some_and(|row| row.grant.enabled),
            )
    }

    pub fn can_undo(&self) -> bool {
        self.undo.is_some()
    }

    pub fn palette(&self, query: &str) -> Vec<PaletteItem> {
        let commands = Control::ALL.map(|control| (Action::AgentControl(control), control.key()));
        let mut items = commands
            .into_iter()
            .chain([(Action::UndoAgentRevoke, 'u')])
            .filter_map(|(action, key)| {
                let label = action.label();
                fuzzy(query, label).map(|score| {
                    (
                        score,
                        PaletteItem {
                            action,
                            label: label.into(),
                            binding: key.to_string(),
                            search: None,
                        },
                    )
                })
            })
            .collect::<Vec<_>>();
        items.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        items.into_iter().map(|(_, item)| item).collect()
    }

    pub fn key(&mut self, key: KeyId) -> Option<Request> {
        if self.confirm.is_some() {
            return self.confirm_revoke(key == KeyId::Char('y'));
        }
        match key {
            KeyId::Down | KeyId::Char('j') => {
                self.select((self.selected + 1).min(self.rows.len().saturating_sub(1)));
            }
            KeyId::Up | KeyId::Char('k') => self.select(self.selected.saturating_sub(1)),
            KeyId::Home => self.select(0),
            KeyId::End => self.select(self.rows.len().saturating_sub(1)),
            KeyId::Tab => self.column = (self.column + 1) % self.controls(),
            KeyId::Right | KeyId::Char('l') => {
                self.column = (self.column + 1).min(self.controls() - 1);
            }
            KeyId::Left | KeyId::Char('h') => self.column = self.column.saturating_sub(1),
            KeyId::Enter | KeyId::Space => return self.activate(self.selected, self.control()),
            KeyId::Char('u') => return self.undo(),
            KeyId::Char(key) => {
                if let Some(control) = Control::ALL
                    .into_iter()
                    .find(|control| control.key() == key)
                {
                    return self.activate(self.selected, control);
                }
            }
            _ => {}
        }
        None
    }

    pub fn activate(&mut self, index: usize, control: Control) -> Option<Request> {
        if self.pending || self.confirm.is_some() {
            return None;
        }
        let row = self.rows.get(index)?.clone();
        if control == Control::Revoke && !row.grant.enabled {
            return None;
        }
        self.selected = index;
        self.column = Control::ALL.iter().position(|item| *item == control)?;
        let grant = control.toggle(row.grant);
        if control == Control::Revoke || !grant.enabled {
            self.confirm = Some(Confirmation { row, control });
            self.set_notice(None);
            return None;
        }
        self.change(row, grant, ChangeKind::Scope)
    }

    pub fn confirm_revoke(&mut self, yes: bool) -> Option<Request> {
        let Confirmation { row, .. } = self.confirm.take()?;
        if yes {
            self.change(row, AgentGrant::DISABLED, ChangeKind::Revoke)
        } else {
            None
        }
    }

    pub fn undo(&mut self) -> Option<Request> {
        let (row, grant) = self.undo.clone()?;
        self.change(row, grant, ChangeKind::Undo)
    }

    fn change(
        &mut self,
        row: AgentSummary,
        grant: AgentGrant,
        kind: ChangeKind,
    ) -> Option<Request> {
        if self.pending || self.confirm.is_some() {
            return None;
        }
        self.pending = true;
        self.set_notice(None);
        Some(Request::Change { row, grant, kind })
    }

    pub fn answer(&mut self, reply: Reply) {
        self.pending = false;
        match reply {
            Reply::Loaded(rows) => self.replace_rows(rows),
            Reply::Stale(rows) => {
                self.replace_rows(rows);
                self.set_notice(Some(("Access changed elsewhere", true)));
            }
            Reply::Changed {
                before,
                after,
                kind,
            } => {
                if let Some(row) = Arc::make_mut(&mut self.rows)
                    .iter_mut()
                    .find(|row| same_agent(row, &after))
                {
                    *row = after.clone();
                }
                let notice = match kind {
                    ChangeKind::Scope => {
                        self.validate_undo();
                        None
                    }
                    ChangeKind::Revoke => {
                        if self
                            .rows
                            .get(self.selected)
                            .is_some_and(|row| same_agent(row, &after))
                        {
                            self.column = 0;
                        }
                        self.undo = Some((after, before.grant));
                        Some(("Access revoked", false))
                    }
                    ChangeKind::Undo => {
                        self.undo = None;
                        Some(("Access restored", false))
                    }
                };
                self.set_notice(notice);
            }
        }
    }

    pub fn fail(&mut self, message: &'static str) {
        self.pending = false;
        self.set_notice(Some((message, true)));
    }

    pub fn clear_notice(&mut self, generation: u64) {
        if self.notice_generation == generation {
            self.set_notice(None);
        }
    }

    fn set_notice(&mut self, notice: Option<(&'static str, bool)>) {
        self.notice_generation = self.notice_generation.wrapping_add(1);
        self.notice = notice;
    }

    fn replace_rows(&mut self, Roster { accounts, rows }: Roster) {
        let selected = (self.rows.get(self.selected))
            .and_then(|prior| rows.iter().position(|row| same_agent(row, prior)))
            .unwrap_or_else(|| self.selected.min(rows.len().saturating_sub(1)));
        self.accounts = Arc::new(accounts);
        self.rows = Arc::new(rows);
        self.select(selected);
        self.validate_undo();
    }

    fn validate_undo(&mut self) {
        if let Some((revoked, _)) = &self.undo
            && !self
                .rows
                .iter()
                .any(|row| same_agent(row, revoked) && row.revision == revoked.revision)
        {
            self.undo = None;
        }
    }
}

fn same_agent(a: &AgentSummary, b: &AgentSummary) -> bool {
    a.agent_id == b.agent_id && a.account_id == b.account_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Keymap;
    use crate::model::{Overlay, Session, Step, SyncKind};

    fn setup() -> (Store, AgentSummary, Session) {
        let store = Store::open_in_memory().unwrap();
        let account = crate::fixtures::fixture_account();
        store.upsert_account(&account).unwrap();
        let grant = AgentGrant::enabled(true, false, true);
        store.set_agent_grant(&account.id, "codex", grant).unwrap();
        let row = store.list_agent_grants().unwrap().pop().unwrap();
        let session = Session::new(
            account.display_name,
            account.email,
            SyncKind::LocalOffline,
            Keymap::default(),
        );
        (store, row, session)
    }

    fn apply(session: &mut Session, store: &Store, step: Step) {
        session
            .agents
            .answer(step.agents.unwrap().run(store).unwrap());
    }

    #[test]
    fn agent_access_round_trips_through_the_inbox_keys_and_palette() {
        let (store, row, mut session) = setup();
        session.on_key(KeyId::Space, 10);
        session.on_key(KeyId::Char('a'), 10);
        assert!(
            session
                .which_items()
                .iter()
                .any(|item| item.key == "a" && item.label == "Agents")
        );
        let step = session.on_key(KeyId::Char('a'), 10);
        apply(&mut session, &store, step);
        assert_eq!(session.overlay, Overlay::Agents);
        assert_eq!(session.agents.rows.as_slice(), std::slice::from_ref(&row));

        session.on_key(KeyId::Char(':'), 10);
        let triage = session
            .palette()
            .into_iter()
            .find(|item| item.action == Action::AgentControl(Control::Triage))
            .unwrap();
        let step = session.confirm_palette(triage, 10);
        apply(&mut session, &store, step);
        let changed = AgentGrant::enabled(true, true, true);
        let read_grant = || store.agent_grant(&row.account_id, &row.agent_id).unwrap();
        assert_eq!(read_grant(), changed);

        assert!(session.on_key(KeyId::Char('x'), 10).agents.is_none());
        assert!(session.agents.confirm.is_some());
        assert_eq!(read_grant(), changed);
        assert!(session.on_key(KeyId::Escape, 10).agents.is_none());
        assert!(session.on_key(KeyId::Char('y'), 10).agents.is_none());
        assert_eq!(read_grant(), changed);

        session.on_key(KeyId::Char('x'), 10);
        let step = session.on_key(KeyId::Char('y'), 10);
        apply(&mut session, &store, step);
        assert_eq!(read_grant(), AgentGrant::DISABLED);
        assert_eq!(session.agents.selected, 0);
        assert_eq!(session.agents.control(), Control::Read);
        assert!(session.agents.can_undo());
        let step = session.on_key(KeyId::Char('u'), 10);
        apply(&mut session, &store, step);
        assert_eq!(read_grant(), changed);
        assert!(!session.agents.can_undo());
    }

    #[test]
    fn last_scope_needs_confirmation_and_uses_the_same_undo() {
        let (store, row, mut session) = setup();
        let grant = AgentGrant::enabled(true, false, false);
        store
            .set_agent_grant(&row.account_id, &row.agent_id, grant)
            .unwrap();
        let read_grant = || store.agent_grant(&row.account_id, &row.agent_id).unwrap();
        let step = session.perform(Action::Agents, 10);
        apply(&mut session, &store, step);

        assert!(session.on_key(KeyId::Space, 10).agents.is_none());
        assert_eq!(
            session.agents.confirm.as_ref().unwrap().control,
            Control::Read
        );
        assert_eq!(read_grant(), grant);
        session.on_key(KeyId::Escape, 10);
        assert!(session.agents.confirm.is_none());
        assert_eq!(read_grant(), grant);

        session.on_key(KeyId::Space, 10);
        let step = session.on_key(KeyId::Char('y'), 10);
        apply(&mut session, &store, step);
        assert_eq!(read_grant(), AgentGrant::DISABLED);
        assert_eq!(session.agents.control(), Control::Read);
        let step = session.on_key(KeyId::Char('u'), 10);
        apply(&mut session, &store, step);
        assert_eq!(read_grant(), grant);

        session.on_key(KeyId::Char('x'), 10);
        let step = session.on_key(KeyId::Char('y'), 10);
        apply(&mut session, &store, step);
        let step = session.on_key(KeyId::Space, 10);
        apply(&mut session, &store, step);
        assert_eq!(read_grant(), grant);
    }

    #[test]
    fn undo_does_not_restore_access_after_a_newer_grant_was_revoked() {
        let (store, row, mut session) = setup();
        let step = session.perform(Action::Agents, 10);
        apply(&mut session, &store, step);
        session.on_key(KeyId::Char('x'), 10);
        let step = session.on_key(KeyId::Char('y'), 10);
        apply(&mut session, &store, step);

        store
            .set_agent_grant(&row.account_id, &row.agent_id, row.grant)
            .unwrap();
        store
            .set_agent_grant(&row.account_id, &row.agent_id, AgentGrant::DISABLED)
            .unwrap();
        let step = session.on_key(KeyId::Char('u'), 10);
        apply(&mut session, &store, step);
        assert_eq!(
            store.agent_grant(&row.account_id, &row.agent_id).unwrap(),
            AgentGrant::DISABLED
        );
        assert!(!session.agents.can_undo());
    }
}
