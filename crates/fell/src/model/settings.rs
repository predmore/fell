use crate::keymap::{self, Action, KeyId, Keymap};
use crate::settings::{Edit, Preference};

pub const REMOVE_ACCOUNT_KEY: KeyId = KeyId::Char('d');
pub const EDIT_CONNECTION_KEY: KeyId = KeyId::Char('e');
pub const RESEND_CHANGES_KEY: KeyId = KeyId::Char('r');
pub const DISCARD_CHANGES_KEY: KeyId = KeyId::Char('x');

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Accounts,
    Appearance,
    Reading,
    Keymap,
    Notifications,
    Search,
    Agents,
}

impl Section {
    pub const ALL: [Self; 7] = [
        Self::Accounts,
        Self::Appearance,
        Self::Reading,
        Self::Keymap,
        Self::Notifications,
        Self::Search,
        Self::Agents,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Accounts => "Accounts",
            Self::Appearance => "Appearance",
            Self::Reading => "Reading",
            Self::Keymap => "Keymap",
            Self::Notifications => "Notifications",
            Self::Search => "Search",
            Self::Agents => "Agents",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Accounts => "Mailbox and connection",
            Self::Appearance => "Theme, text size and motion",
            Self::Reading => "Mark-read delay, web view and remote images",
            Self::Keymap => "Shortcuts and which-key",
            Self::Notifications => "Desktop alerts for new mail",
            Self::Search => "Local semantic search",
            Self::Agents => "Connected agents and mailbox access",
        }
    }

    pub fn rows(self) -> &'static [Row] {
        use Preference::*;
        use Row::*;
        match self {
            Self::Accounts => &[
                AccountSetup,
                EditAccount,
                ImportIdentities,
                RemoveAccount,
                EditConnection,
                ReconnectAccount,
                ResendChanges,
                DiscardChanges,
            ],
            Self::Appearance => &[Value(Theme), Value(FontSize), Value(ReduceMotion)],
            Self::Reading => &[Value(ReadDwell), Value(WebViewDefault), Value(RemoteImages)],
            Self::Keymap => &[Value(WhichKey), EffectiveBindings, EditKeymap],
            Self::Notifications => &[Value(Notifications)],
            Self::Search => &[InstallModel, RemoveModel],
            Self::Agents => &[ManageAgents],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Row {
    Value(Preference),
    AccountSetup,
    Account(usize),
    RemoveAccount,
    EditConnection,
    ReconnectAccount,
    ResendChanges,
    DiscardChanges,
    EditAccount,
    ImportIdentities,
    EffectiveBindings,
    EditKeymap,
    InstallModel,
    RemoveModel,
    ManageAgents,
}

impl Row {
    pub fn command(self) -> &'static str {
        match self {
            Self::Value(preference) => preference.command(),
            Self::Account(_) => "Settings: Open account",
            Self::RemoveAccount => "Settings: Remove account",
            Self::EditConnection => "Settings: Edit connection",
            Self::ReconnectAccount => "Settings: Reconnect account",
            Self::ResendChanges => "Settings: Resend unsynced changes",
            Self::DiscardChanges => "Settings: Discard unsynced changes",
            Self::AccountSetup => "Settings: Add account",
            Self::EditAccount => "Settings: Edit account file",
            Self::ImportIdentities => "Settings: Import sender addresses",
            Self::EffectiveBindings => "Settings: Show effective bindings",
            Self::EditKeymap => "Settings: Edit keymap.toml",
            Self::InstallModel => "Settings: Install local search model",
            Self::RemoveModel => "Settings: Remove local search model",
            Self::ManageAgents => "Settings: Open Agents",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Save(Edit),
    AccountSetup,
    OpenAccount(usize),
    EditConnection(usize),
    ReconnectAccount,
    HeldChanges { index: usize, resend: bool },
    ConfirmAccountRemoval(usize),
    RemoveAccount { delete: bool },
    EditAccount,
    ReadIdentityCandidates,
    ToggleIdentityCandidate(usize),
    ImportIdentities,
    EditKeymap,
    CheckModel,
    Scroll(i32),
    ScrollToEdge { bottom: bool },
    ChangeModel { install: bool },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Page {
    #[default]
    Index,
    Section(Section),
    Bindings,
    IdentityImport,
    ConfirmAccountRemoval {
        delete: bool,
        kind: AccountRemovalKind,
    },
    ConfirmModel {
        install: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountRemovalKind {
    Account,
    Disconnected,
    BrokenFile,
    Interrupted { can_restore: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Availability {
    pub account_setup: bool,
    pub edit_account: bool,
    pub identity_count: usize,
    pub accounts: usize,
    pub active_account: Option<usize>,
    pub kept_accounts: bool,
    pub held_changes: bool,
    pub install_model: bool,
    pub remove_model: bool,
}

impl Default for Availability {
    fn default() -> Self {
        Self {
            account_setup: true,
            edit_account: false,
            identity_count: 0,
            accounts: 0,
            active_account: None,
            kept_accounts: false,
            held_changes: false,
            install_model: true,
            remove_model: false,
        }
    }
}

impl Availability {
    pub fn shows(self, row: Row) -> bool {
        match row {
            Row::AccountSetup => self.account_setup,
            Row::Account(index) => index < self.accounts,
            Row::RemoveAccount => self.accounts > 0,
            Row::EditConnection => self.active_account.is_some(),
            Row::ReconnectAccount => self.kept_accounts,
            Row::ResendChanges | Row::DiscardChanges => self.held_changes,
            Row::EditAccount => self.edit_account,
            Row::ImportIdentities => self.identity_count > 0,
            Row::InstallModel => self.install_model,
            Row::RemoveModel => self.remove_model,
            _ => true,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Nav {
    pub page: Page,
    pub selected: usize,
    pub identity_candidates: usize,
    pub availability: Availability,
    bindings_g_pending: bool,
}

impl Nav {
    pub fn reset(&mut self) {
        self.page = Page::Index;
        self.selected = 0;
        self.bindings_g_pending = false;
    }

    pub fn rows(&self, section: Section) -> impl Iterator<Item = Row> {
        let accounts = if section == Section::Accounts {
            self.availability.accounts
        } else {
            0
        };
        let rows = (0..accounts).flat_map(move |index| {
            let active = (self.availability.active_account == Some(index))
                .then_some([Row::EditAccount, Row::ImportIdentities]);
            std::iter::once(Row::Account(index)).chain(active.into_iter().flatten())
        });
        rows.chain(
            section
                .rows()
                .iter()
                .copied()
                .filter(move |row| section != Section::Accounts || *row == Row::AccountSetup),
        )
        .filter(|row| self.availability.shows(*row))
    }

    pub fn set_availability(&mut self, next: Availability) {
        let selected_row = self.row();
        self.availability = next;
        if let Page::Section(section) = self.page {
            self.selected = selected_row
                .and_then(|row| self.rows(section).position(|item| item == row))
                .unwrap_or(0);
        }
    }

    pub fn select_row(&mut self, row: Row) {
        self.bindings_g_pending = false;
        for section in Section::ALL {
            let index = self.rows(section).position(|item| item == row);
            if let Some(index) = index {
                self.page = Page::Section(section);
                self.selected = index;
                break;
            }
        }
    }

    pub fn confirm_account_removal(&mut self, kind: AccountRemovalKind) {
        self.page = Page::ConfirmAccountRemoval {
            delete: kind == AccountRemovalKind::Disconnected,
            kind,
        };
    }

    pub fn back(&mut self) -> bool {
        match self.page {
            Page::Index => return false,
            Page::Section(section) => {
                self.page = Page::Index;
                self.selected = Section::ALL
                    .iter()
                    .position(|item| *item == section)
                    .unwrap();
            }
            Page::Bindings => self.select_row(Row::EffectiveBindings),
            Page::IdentityImport => self.select_row(Row::ImportIdentities),
            Page::ConfirmAccountRemoval { delete: true, kind }
                if kind != AccountRemovalKind::Disconnected =>
            {
                self.page = Page::ConfirmAccountRemoval {
                    delete: false,
                    kind,
                };
            }
            Page::ConfirmAccountRemoval { .. } => {
                self.page = Page::Section(Section::Accounts);
            }
            Page::ConfirmModel { install } => {
                self.page = Page::Section(Section::Search);
                self.selected = 0;
                self.select_row(if install {
                    Row::InstallModel
                } else {
                    Row::RemoveModel
                });
            }
        }
        true
    }

    pub fn enter_section(&mut self, section: Section) -> Option<Request> {
        self.page = Page::Section(section);
        self.selected = 0;
        (section == Section::Search).then_some(Request::CheckModel)
    }

    pub fn bindings_scroll_edge(&mut self, key: KeyId) -> Option<Request> {
        let pending = std::mem::take(&mut self.bindings_g_pending);
        match key {
            KeyId::Char('G') => Some(Request::ScrollToEdge { bottom: true }),
            KeyId::Char('g') if pending => Some(Request::ScrollToEdge { bottom: false }),
            KeyId::Char('g') => {
                self.bindings_g_pending = true;
                None
            }
            _ => None,
        }
    }

    pub fn move_to(&mut self, key: KeyId) {
        let len = match self.page {
            Page::Index => Section::ALL.len(),
            Page::Section(section) => self.rows(section).count(),
            Page::IdentityImport => self.identity_candidates + 1,
            Page::Bindings | Page::ConfirmModel { .. } | Page::ConfirmAccountRemoval { .. } => {
                return;
            }
        };
        if len != 0 {
            self.selected = match key {
                KeyId::Char('j') | KeyId::Down => (self.selected + 1) % len,
                KeyId::Char('k') | KeyId::Up => self.selected.checked_sub(1).unwrap_or(len - 1),
                _ => self.selected,
            };
        }
    }

    pub fn row(&self) -> Option<Row> {
        let Page::Section(section) = self.page else {
            return None;
        };
        self.rows(section).nth(self.selected)
    }
}

pub fn bindings(keymap: &Keymap) -> impl Iterator<Item = (Action, String)> {
    Action::all().filter_map(|action| {
        let keys = keymap::binding_text(keymap, action);
        (!keys.is_empty()).then_some((action, keys))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Overlay, Session, SyncKind};

    #[test]
    fn settings_chord_navigation_and_palette_share_the_same_rows() {
        let mut session = Session::new("", "", SyncKind::LocalOffline, Keymap::default());
        session.on_key(KeyId::Space, 10);
        session.on_key(KeyId::Char(','), 10);
        assert_eq!(session.overlay, Overlay::Settings);
        session.on_key(KeyId::Char('j'), 10);
        session.on_key(KeyId::Enter, 10);
        assert_eq!(
            session.settings_nav.page,
            Page::Section(Section::Appearance)
        );
        session.on_key(KeyId::Char('j'), 10);
        assert_eq!(
            session.settings_nav.row(),
            Some(Row::Value(Preference::FontSize))
        );
        assert_eq!(
            session.on_key(KeyId::Char('h'), 10).settings,
            Some(Request::Save(Edit::Cycle(Preference::FontSize, true)))
        );
        session.on_key(KeyId::Escape, 10);
        assert_eq!(session.settings_nav.page, Page::Index);
        assert_eq!(session.settings_nav.selected, 1);

        session.on_key(KeyId::Char(':'), 10);
        session.palette_query = "mark-read delay".into();
        session.on_key(KeyId::Enter, 10);
        assert_eq!(session.settings_nav.page, Page::Section(Section::Reading));
        assert_eq!(
            session.settings_nav.row(),
            Some(Row::Value(Preference::ReadDwell))
        );
    }

    #[test]
    fn a_comma_leader_can_still_open_settings() {
        let keymap = keymap::keymap_from_text("leader = ','").unwrap();
        let mut session = Session::new("", "", SyncKind::LocalOffline, keymap);
        session.on_key(KeyId::Char(','), 10);
        session.on_key(KeyId::Char(','), 10);
        assert_eq!(session.overlay, Overlay::Settings);
    }

    #[test]
    fn reading_defaults_open_from_the_palette_and_cycle_as_settings() {
        for (query, preference) in [
            ("web view default", Preference::WebViewDefault),
            ("remote images", Preference::RemoteImages),
        ] {
            let mut session = Session::new("", "", SyncKind::LocalOffline, Keymap::default());
            session.on_key(KeyId::Char(':'), 10);
            session.palette_query = format!("settings {query}");
            session.on_key(KeyId::Enter, 10);
            assert_eq!(session.settings_nav.page, Page::Section(Section::Reading));
            assert_eq!(session.settings_nav.row(), Some(Row::Value(preference)));
            assert_eq!(
                session.on_key(KeyId::Char('l'), 10).settings,
                Some(Request::Save(Edit::Cycle(preference, false)))
            );
        }
    }

    #[test]
    fn effective_bindings_jump_with_gg_and_capital_g() {
        let mut session = Session::new("", "", SyncKind::LocalOffline, Keymap::default());
        let bindings = Action::Setting(Row::EffectiveBindings);
        session.perform(bindings, 10);
        let g = KeyId::Char('g');

        assert_eq!(
            session.on_key(KeyId::Char('G'), 10).settings,
            Some(Request::ScrollToEdge { bottom: true })
        );
        assert_eq!(session.on_key(g, 10).settings, None);
        assert_eq!(
            session.on_key(g, 10).settings,
            Some(Request::ScrollToEdge { bottom: false })
        );
        assert_eq!(session.on_key(g, 10).settings, None);
        assert_eq!(
            session.on_key(KeyId::Char('j'), 10).settings,
            Some(Request::Scroll(2))
        );
        assert_eq!(session.on_key(g, 10).settings, None);
        session.on_key(KeyId::Escape, 10);
        session.perform(bindings, 10);
        assert_eq!(session.on_key(g, 10).settings, None);
    }

    #[test]
    fn unavailable_actions_are_absent_in_both_settings_and_palette() {
        let mut session = Session::new("", "", SyncKind::LocalOffline, Keymap::default());
        session.settings_nav.set_availability(Availability {
            account_setup: false,
            ..Availability::default()
        });
        assert_eq!(session.settings_nav.rows(Section::Accounts).count(), 0);
        assert_eq!(
            session
                .settings_nav
                .rows(Section::Search)
                .collect::<Vec<_>>(),
            [Row::InstallModel]
        );
        session.palette_query = "settings".into();
        let items = session.palette();
        assert!(!items.iter().any(|item| matches!(
            item.action,
            Action::Setting(Row::AccountSetup | Row::RemoveModel)
        )));
        assert!(
            items
                .iter()
                .any(|item| item.action == Action::Setting(Row::EditKeymap))
        );
    }

    #[test]
    fn palette_held_change_actions_target_the_selected_account() {
        let mut session = Session::new("", "", SyncKind::LocalOffline, Keymap::default());
        session.settings_nav.set_availability(Availability {
            accounts: 3,
            active_account: Some(0),
            held_changes: true,
            ..Availability::default()
        });
        session.settings_nav.select_row(Row::Account(2));
        assert_eq!(
            session
                .perform(Action::Setting(Row::DiscardChanges), 10)
                .settings,
            Some(Request::HeldChanges {
                index: 2,
                resend: false
            })
        );

        session.settings_nav.select_row(Row::AccountSetup);
        assert_eq!(
            session
                .perform(Action::Setting(Row::ResendChanges), 10)
                .settings,
            Some(Request::HeldChanges {
                index: 0,
                resend: true
            })
        );
    }

    #[test]
    fn palette_edit_connection_leaves_the_current_view_in_place() {
        let mut session = Session::new("", "", SyncKind::LocalOffline, Keymap::default());
        session.settings_nav.set_availability(Availability {
            accounts: 2,
            active_account: Some(0),
            ..Availability::default()
        });
        session.settings_nav.select_row(Row::Account(1));
        let edit_from_palette = |session: &mut Session| {
            session.on_key(KeyId::Char(':'), 10);
            session.palette_query = "edit connection".into();
            session.on_key(KeyId::Enter, 10).settings
        };

        assert_eq!(
            edit_from_palette(&mut session),
            Some(Request::EditConnection(0))
        );
        assert_eq!(session.overlay, Overlay::None);
        session.overlay = Overlay::Settings;
        assert_eq!(
            edit_from_palette(&mut session),
            Some(Request::EditConnection(1))
        );
        assert_eq!(session.overlay, Overlay::Settings);
        assert_eq!(session.settings_nav.row(), Some(Row::Account(1)));
    }

    #[test]
    fn disconnected_removal_requires_explicit_local_mail_confirmation() {
        let mut session = Session::new("", "", SyncKind::LocalOffline, Keymap::default());
        session.overlay = Overlay::Settings;
        session.settings_nav.set_availability(Availability {
            accounts: 1,
            kept_accounts: true,
            ..Availability::default()
        });
        session.settings_nav.select_row(Row::Account(0));
        assert_eq!(
            session.on_key(REMOVE_ACCOUNT_KEY, 10).settings,
            Some(Request::ConfirmAccountRemoval(0))
        );
        let kind = AccountRemovalKind::Disconnected;
        session.settings_nav.confirm_account_removal(kind);
        assert_eq!(session.on_key(KeyId::Enter, 10).settings, None);
        assert_eq!(
            session.settings_nav.page,
            Page::ConfirmAccountRemoval { delete: true, kind }
        );
        assert_eq!(session.on_key(KeyId::Escape, 10).settings, None);
        assert_eq!(session.settings_nav.row(), Some(Row::Account(0)));
        session.settings_nav.confirm_account_removal(kind);
        assert_eq!(
            session.on_key(KeyId::Char('y'), 10).settings,
            Some(Request::RemoveAccount { delete: true })
        );
        assert_eq!(session.settings_nav.row(), Some(Row::Account(0)));
    }

    #[test]
    fn a_model_change_starts_only_after_consent() {
        let mut session = Session::new("", "", SyncKind::LocalOffline, Keymap::default());
        let install = Action::Setting(Row::InstallModel);
        assert!(session.perform(install, 10).settings.is_none());
        assert_eq!(
            session.settings_nav.page,
            Page::ConfirmModel { install: true }
        );
        assert!(session.on_key(KeyId::Enter, 10).settings.is_none());
        assert!(session.on_key(KeyId::Escape, 10).settings.is_none());
        assert_eq!(session.settings_nav.page, Page::Section(Section::Search));
        session.perform(install, 10);
        assert_eq!(
            session.on_key(KeyId::Char('y'), 10).settings,
            Some(Request::ChangeModel { install: true })
        );
    }
}
