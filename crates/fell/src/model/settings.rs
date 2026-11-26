use crate::keymap::{self, Action, KeyId, Keymap};
use crate::settings::{Edit, Preference};

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
            Self::Reading => "When conversations are marked read",
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
            Self::Accounts => &[AccountSetup, EditAccount, ImportIdentities],
            Self::Appearance => &[Value(Theme), Value(FontSize), Value(ReduceMotion)],
            Self::Reading => &[Value(ReadDwell)],
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
            Self::AccountSetup => "Settings: Open account setup",
            Self::EditAccount => "Settings: Edit account.toml",
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
    EditAccount,
    ReadIdentityCandidates,
    ToggleIdentityCandidate(usize),
    ImportIdentities,
    EditKeymap,
    CheckModel,
    Scroll(i32),
    ChangeModel { install: bool },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Page {
    #[default]
    Index,
    Section(Section),
    Bindings,
    IdentityImport,
    ConfirmModel {
        install: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Availability {
    pub account_setup: bool,
    pub edit_account: bool,
    pub identity_count: usize,
    pub install_model: bool,
    pub remove_model: bool,
}

impl Default for Availability {
    fn default() -> Self {
        Self {
            account_setup: true,
            edit_account: false,
            identity_count: 0,
            install_model: true,
            remove_model: false,
        }
    }
}

impl Availability {
    pub fn shows(self, row: Row) -> bool {
        match row {
            Row::AccountSetup => self.account_setup,
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
}

impl Nav {
    pub fn reset(&mut self) {
        self.page = Page::Index;
        self.selected = 0;
    }

    pub fn rows(&self, section: Section) -> impl Iterator<Item = Row> {
        section
            .rows()
            .iter()
            .copied()
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
        for section in Section::ALL {
            let index = self.rows(section).position(|item| item == row);
            if let Some(index) = index {
                self.page = Page::Section(section);
                self.selected = index;
                break;
            }
        }
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

    pub fn move_to(&mut self, key: KeyId) {
        let len = match self.page {
            Page::Index => Section::ALL.len(),
            Page::Section(section) => self.rows(section).count(),
            Page::IdentityImport => self.identity_candidates + 1,
            Page::Bindings | Page::ConfirmModel { .. } => return,
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
