//! Leader chords, which-key, and the command palette index.
//!
//! Use `m` to move, `e` to archive, `#` to move to Trash, and `!` for
//! spam or not spam. `U` toggles unread and `s` toggles star. `u` undoes
//! the latest reversible action.

use std::fmt::Write as _;

use fell_core::NotificationMode;

use crate::model::settings::Row as SettingsRow;
use crate::settings::Preference;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyId {
    Char(char),
    Space,
    /// The configured leader, after it has been recognized.
    Leader,
    Ctrl(char),
    CtrlShiftP,
    Escape,
    Backspace,
    Enter,
    Up,
    Down,
    Tab,
    Left,
    Right,
    Home,
    End,
    Delete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    Next,
    Prev,
    First,
    Last,
    Open,
    Back,
    PageDown,
    PageUp,
    Inbox,
    Sent,
    Drafts,
    Snoozed,
    Muted,
    Compose,
    Reply,
    ReplyAll,
    Forward,
    FocusLinks,
    FocusAttachments,
    PreviousAttachment,
    NextAttachment,
    OpenAttachment,
    SaveAttachment,
    SaveAllAttachments,
    RetryAttachment,
    FolderPicker,
    AccountPicker,
    Label,
    LabelList,
    Search,
    NextHit,
    PreviousHit,
    Move,
    Archive,
    Snooze,
    Unsnooze,
    Mute,
    ToggleUnread,
    ToggleStar,
    Delete,
    Spam,
    Undo,
    SplitThread,
    MergeThreads,
    SyncNow,
    Notifications(NotificationMode),
    ToggleSelect,
    Visual,
    FullWidth,
    ToggleQuotes,
    ToggleWebView,
    AllowMessageImages,
    AllowSenderImages,
    BlockSenderImages,
    Palette,
    Help,
    About,
    Settings,
    Setting(SettingsRow),
    ReopenReview,
    Agents,
    AgentActivity,
    UndoAgentActivity,
    AgentControl(crate::model::agents::Control),
    UndoAgentRevoke,
    OpenDiagnosticLog,
    OpenCrashReport,
    ToggleAgentNote,
    DismissAgentNote,
    NextAgentNote,
    ScrollAgentNoteDown,
    ScrollAgentNoteUp,
    AcceptAgentSuggestion,
    ThreadsWithSuggestions,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Self::Next => "Next",
            Self::Prev => "Previous",
            Self::First => "First",
            Self::Last => "Last",
            Self::Open => "Open conversation",
            Self::Back => "Back to list",
            Self::PageDown => "Page down",
            Self::PageUp => "Page up",
            Self::Inbox => "Go to Inbox",
            Self::Sent => "Go to Sent",
            Self::Drafts => "Go to Drafts",
            Self::Snoozed => "Go to Snoozed",
            Self::Muted => "Go to Muted",
            Self::Compose => "Compose",
            Self::Reply => "Reply",
            Self::ReplyAll => "Reply all",
            Self::Forward => "Forward",
            Self::FocusLinks => "Focus message links",
            Self::FocusAttachments => "Focus attachments",
            Self::PreviousAttachment => "Previous attachment",
            Self::NextAttachment => "Next attachment",
            Self::OpenAttachment => "Open attachment",
            Self::SaveAttachment => "Save attachment",
            Self::SaveAllAttachments => "Save all attachments",
            Self::RetryAttachment => "Retry attachment action",
            Self::FolderPicker => "Choose folder",
            Self::AccountPicker => "Switch account",
            Self::Label => "Apply or remove label",
            Self::LabelList => "Go to label",
            Self::Search => "Search mail",
            Self::NextHit => "Next search hit",
            Self::PreviousHit => "Previous search hit",
            Self::Move => "Move",
            Self::Archive => "Archive",
            Self::Snooze => "Snooze",
            Self::Unsnooze => "Unsnooze",
            Self::Mute => "Mute or unmute",
            Self::ToggleUnread => "Mark unread or read",
            Self::ToggleStar => "Star or unstar",
            Self::Delete => "Delete",
            Self::Spam => "Spam / not spam",
            Self::Undo => "Undo",
            Self::SplitThread => "Split at message",
            Self::MergeThreads => "Merge conversations",
            Self::SyncNow => "Sync now",
            Self::Notifications(NotificationMode::On) => "Notifications: Show sender and subject",
            Self::Notifications(NotificationMode::Private) => {
                "Notifications: Hide sender and subject"
            }
            Self::Notifications(NotificationMode::Off) => "Notifications: Off",
            Self::ToggleSelect => "Toggle select",
            Self::Visual => "Visual select",
            Self::FullWidth => "Full-width conversation",
            Self::ToggleWebView => "Toggle web view",
            Self::AllowMessageImages => "Allow images for this message",
            Self::AllowSenderImages => "Always allow images from this sender",
            Self::BlockSenderImages => "Revoke image permission for this sender",
            Self::ToggleQuotes => "Quoted text and recipients",
            Self::Palette => "Command palette",
            Self::Help => "Help",
            Self::About => "About Fell",
            Self::Settings => "Settings",
            Self::Setting(row) => row.command(),
            Self::ReopenReview => "Reopen agent review",
            Self::Agents => "Agents",
            Self::AgentActivity => "Agent activity",
            Self::UndoAgentActivity => "Undo selected agent action",
            Self::AgentControl(control) => control.command(),
            Self::UndoAgentRevoke => "Undo agent revoke",
            Self::OpenDiagnosticLog => "Open diagnostic log",
            Self::OpenCrashReport => "Open crash report",
            Self::ToggleAgentNote => "Expand or collapse agent note",
            Self::DismissAgentNote => "Dismiss agent note",
            Self::NextAgentNote => "Next agent note",
            Self::ScrollAgentNoteDown => "Scroll agent note down",
            Self::ScrollAgentNoteUp => "Scroll agent note up",
            Self::AcceptAgentSuggestion => "Accept first agent suggestion",
            Self::ThreadsWithSuggestions => "Threads with suggestions",
        }
    }

    pub fn all() -> impl Iterator<Item = Action> {
        ACTION_SECTIONS
            .iter()
            .flat_map(|(_, actions)| actions.iter().copied())
    }
}

/// The default discovery order. A chord group can span sections when its
/// existing key serves more than one purpose, as with attachments and agents.
pub const ACTION_SECTIONS: &[(&str, &[Action])] = &[
    (
        "Navigation",
        &[
            Action::Next,
            Action::Prev,
            Action::First,
            Action::Last,
            Action::Open,
            Action::Back,
            Action::Inbox,
            Action::Sent,
            Action::Drafts,
            Action::FolderPicker,
            Action::AccountPicker,
            Action::LabelList,
            Action::Snoozed,
            Action::Muted,
        ],
    ),
    (
        "Search",
        &[Action::Search, Action::NextHit, Action::PreviousHit],
    ),
    (
        "Reading",
        &[
            Action::PageDown,
            Action::PageUp,
            Action::FullWidth,
            Action::ToggleQuotes,
            Action::ToggleWebView,
            Action::AllowMessageImages,
            Action::AllowSenderImages,
            Action::BlockSenderImages,
            Action::FocusLinks,
        ],
    ),
    (
        "Attachments",
        &[
            Action::FocusAttachments,
            Action::PreviousAttachment,
            Action::NextAttachment,
            Action::OpenAttachment,
            Action::SaveAttachment,
            Action::SaveAllAttachments,
            Action::RetryAttachment,
        ],
    ),
    (
        "Compose",
        &[
            Action::Compose,
            Action::Reply,
            Action::ReplyAll,
            Action::Forward,
        ],
    ),
    (
        "Organize",
        &[
            Action::ToggleSelect,
            Action::Visual,
            Action::Move,
            Action::Archive,
            Action::Label,
            Action::Snooze,
            Action::Unsnooze,
            Action::Mute,
            Action::ToggleUnread,
            Action::ToggleStar,
            Action::Delete,
            Action::Spam,
            Action::SplitThread,
            Action::MergeThreads,
            Action::Undo,
        ],
    ),
    (
        "Agents",
        &[
            Action::Agents,
            Action::ReopenReview,
            Action::AgentActivity,
            Action::ToggleAgentNote,
            Action::DismissAgentNote,
            Action::NextAgentNote,
            Action::ScrollAgentNoteDown,
            Action::ScrollAgentNoteUp,
            Action::AcceptAgentSuggestion,
            Action::ThreadsWithSuggestions,
        ],
    ),
    (
        "General",
        &[
            Action::Palette,
            Action::Help,
            Action::Settings,
            Action::SyncNow,
            Action::Notifications(NotificationMode::On),
            Action::Notifications(NotificationMode::Private),
            Action::Notifications(NotificationMode::Off),
            Action::Setting(SettingsRow::Value(Preference::Theme)),
            Action::Setting(SettingsRow::Value(Preference::FontSize)),
            Action::Setting(SettingsRow::Value(Preference::ReduceMotion)),
            Action::Setting(SettingsRow::Value(Preference::ReadDwell)),
            Action::Setting(SettingsRow::Value(Preference::WhichKey)),
            Action::Setting(SettingsRow::Value(Preference::Notifications)),
            Action::Setting(SettingsRow::AccountSetup),
            Action::Setting(SettingsRow::RemoveAccount),
            Action::Setting(SettingsRow::EditConnection),
            Action::Setting(SettingsRow::ReconnectAccount),
            Action::Setting(SettingsRow::ResendChanges),
            Action::Setting(SettingsRow::DiscardChanges),
            Action::Setting(SettingsRow::EditAccount),
            Action::Setting(SettingsRow::ImportIdentities),
            Action::Setting(SettingsRow::EffectiveBindings),
            Action::Setting(SettingsRow::EditKeymap),
            Action::Setting(SettingsRow::InstallModel),
            Action::Setting(SettingsRow::RemoveModel),
            Action::OpenDiagnosticLog,
            Action::OpenCrashReport,
            Action::About,
        ],
    ),
];

#[derive(Clone, Debug)]
pub struct Binding {
    pub keys: Vec<KeyId>,
    pub action: Action,
    pub group: Option<&'static str>,
}

#[derive(Clone, Debug)]
pub struct Keymap {
    pub leader: KeyId,
    pub leader_name: String,
    pub which_key_ms: u64,
    pub bindings: Vec<Binding>,
    // These single keys take precedence while a chip is focused.
    attachment_bindings: [(KeyId, Action); 4],
}

impl Default for Keymap {
    fn default() -> Self {
        Self::new(KeyId::Space, "space", 300)
    }
}

impl Keymap {
    pub fn new(leader: KeyId, leader_name: impl Into<String>, which_key_ms: u64) -> Self {
        let bindings = default_bindings();
        Self {
            leader,
            leader_name: leader_name.into(),
            which_key_ms,
            bindings,
            attachment_bindings: [
                (KeyId::Char('h'), Action::PreviousAttachment),
                (KeyId::Char('l'), Action::NextAttachment),
                (KeyId::Enter, Action::OpenAttachment),
                (KeyId::Char('o'), Action::OpenAttachment),
            ],
        }
    }

    pub fn attachment_action(&self, key: KeyId) -> Option<Action> {
        self.attachment_bindings
            .iter()
            .find_map(|&(binding, action)| (binding == key).then_some(action))
    }

    pub fn bindings_for(&self, action: Action) -> Vec<&Binding> {
        self.bindings
            .iter()
            .filter(|binding| binding.action == action)
            .collect()
    }
}

fn default_bindings() -> Vec<Binding> {
    let mut bindings = Vec::new();
    let mut bind = |keys: &[KeyId], action: Action, group: Option<&'static str>| {
        bindings.push(Binding {
            keys: keys.to_vec(),
            action,
            group,
        });
    };
    bind(&[KeyId::Char('j')], Action::Next, None);
    bind(&[KeyId::Char('k')], Action::Prev, None);
    bind(&[KeyId::Char('g'), KeyId::Char('g')], Action::First, None);
    bind(&[KeyId::Char('G')], Action::Last, None);
    bind(&[KeyId::Enter], Action::Open, None);
    bind(&[KeyId::Char('o')], Action::Open, None);
    bind(&[KeyId::Escape], Action::Back, None);
    bind(&[KeyId::Char('h')], Action::Back, None);
    bind(&[KeyId::Ctrl('d')], Action::PageDown, None);
    bind(&[KeyId::Ctrl('u')], Action::PageUp, None);
    bind(&[KeyId::Char('/')], Action::Search, None);
    bind(&[KeyId::Char('n')], Action::NextHit, None);
    bind(&[KeyId::Char('N')], Action::PreviousHit, None);
    bind(
        &[KeyId::Char('g'), KeyId::Char('i')],
        Action::Inbox,
        Some("Go"),
    );
    bind(
        &[KeyId::Char('g'), KeyId::Char('s')],
        Action::Sent,
        Some("Go"),
    );
    bind(
        &[KeyId::Char('g'), KeyId::Char('d')],
        Action::Drafts,
        Some("Go"),
    );
    bind(
        &[KeyId::Char('g'), KeyId::Char('f')],
        Action::FolderPicker,
        Some("Go"),
    );
    bind(
        &[KeyId::Char('g'), KeyId::Char('A')],
        Action::AccountPicker,
        Some("Go"),
    );
    bind(
        &[KeyId::Leader, KeyId::Char('A')],
        Action::AccountPicker,
        None,
    );
    bind(&[KeyId::Char('l')], Action::Label, None);
    bind(
        &[KeyId::Char('g'), KeyId::Char('l')],
        Action::LabelList,
        Some("Go"),
    );
    bind(&[KeyId::Char('c')], Action::Compose, None);
    bind(&[KeyId::Char('r')], Action::Reply, None);
    bind(&[KeyId::Char('R')], Action::ReplyAll, None);
    bind(&[KeyId::Char('f')], Action::Forward, None);
    bind(&[KeyId::Tab], Action::FocusLinks, None);
    bind(&[KeyId::Char('a')], Action::FocusAttachments, None);
    bind(&[KeyId::Char('S')], Action::SaveAttachment, None);
    bind(&[KeyId::Char('m')], Action::Move, None);
    bind(&[KeyId::Char('e')], Action::Archive, None);
    bind(&[KeyId::Char('b')], Action::Snooze, None);
    bind(&[KeyId::Char('B')], Action::Unsnooze, None);
    bind(&[KeyId::Char('M')], Action::Mute, None);
    bind(
        &[KeyId::Char('g'), KeyId::Char('z')],
        Action::Snoozed,
        Some("Go"),
    );
    bind(
        &[KeyId::Char('g'), KeyId::Char('m')],
        Action::Muted,
        Some("Go"),
    );
    bind(&[KeyId::Char('U')], Action::ToggleUnread, None);
    bind(&[KeyId::Char('s')], Action::ToggleStar, None);
    bind(&[KeyId::Char('#')], Action::Delete, None);
    bind(&[KeyId::Char('!')], Action::Spam, None);
    bind(&[KeyId::Char('u')], Action::Undo, None);
    for (key, action) in [('s', Action::SplitThread), ('m', Action::MergeThreads)] {
        bind(
            &[KeyId::Leader, KeyId::Char('c'), KeyId::Char(key)],
            action,
            Some("Conversations"),
        );
    }
    bind(&[KeyId::Char('x')], Action::ToggleSelect, None);
    bind(&[KeyId::Char('V')], Action::Visual, None);
    bind(&[KeyId::Char('z')], Action::FullWidth, None);
    bind(&[KeyId::Char('q')], Action::ToggleQuotes, None);
    bind(&[KeyId::Char(':')], Action::Palette, None);
    bind(&[KeyId::CtrlShiftP], Action::Palette, None);
    bind(&[KeyId::Char('?')], Action::Help, None);
    bind(&[KeyId::Leader, KeyId::Char(',')], Action::Settings, None);
    for (key, action) in [
        ('?', Action::Help),
        ('l', Action::OpenDiagnosticLog),
        ('c', Action::OpenCrashReport),
    ] {
        bind(
            &[KeyId::Leader, KeyId::Char('?'), KeyId::Char(key)],
            action,
            Some("General"),
        );
    }
    bind(
        &[KeyId::Leader, KeyId::Char('f'), KeyId::Char('i')],
        Action::Inbox,
        Some("Folders"),
    );
    bind(
        &[KeyId::Leader, KeyId::Char('f'), KeyId::Char('s')],
        Action::Sent,
        Some("Folders"),
    );
    bind(
        &[KeyId::Leader, KeyId::Char('f'), KeyId::Char('d')],
        Action::Drafts,
        Some("Folders"),
    );
    bind(
        &[KeyId::Leader, KeyId::Char('f'), KeyId::Char('f')],
        Action::FolderPicker,
        Some("Folders"),
    );
    bind(
        &[KeyId::Leader, KeyId::Char('f'), KeyId::Char('l')],
        Action::LabelList,
        Some("Folders"),
    );
    bind(
        &[KeyId::Leader, KeyId::Char('f'), KeyId::Char('L')],
        Action::Label,
        Some("Folders"),
    );
    for (key, action) in [('z', Action::Snoozed), ('M', Action::Muted)] {
        bind(
            &[KeyId::Leader, KeyId::Char('f'), KeyId::Char(key)],
            action,
            Some("Folders"),
        );
    }
    for (key, action) in [
        ('s', Action::Snooze),
        ('u', Action::Unsnooze),
        ('m', Action::Mute),
    ] {
        bind(
            &[KeyId::Leader, KeyId::Char('b'), KeyId::Char(key)],
            action,
            Some("Organize"),
        );
    }
    bind(
        &[KeyId::Leader, KeyId::Char('f'), KeyId::Char('m')],
        Action::Move,
        Some("Folders"),
    );
    bind(
        &[KeyId::Leader, KeyId::Char('f'), KeyId::Char('#')],
        Action::Delete,
        Some("Folders"),
    );
    bind(
        &[KeyId::Leader, KeyId::Char('f'), KeyId::Char('!')],
        Action::Spam,
        Some("Folders"),
    );
    bind(&[KeyId::Leader, KeyId::Char('s')], Action::SyncNow, None);
    bind(
        &[KeyId::Leader, KeyId::Char('a'), KeyId::Char('s')],
        Action::SaveAllAttachments,
        Some("Attachments / agents"),
    );
    bind(
        &[KeyId::Leader, KeyId::Char('a'), KeyId::Char('R')],
        Action::RetryAttachment,
        Some("Attachments / agents"),
    );
    bind(
        &[KeyId::Leader, KeyId::Char('a'), KeyId::Char('a')],
        Action::Agents,
        Some("Agents"),
    );
    bind(
        &[KeyId::Leader, KeyId::Char('a'), KeyId::Char('r')],
        Action::ReopenReview,
        Some("Agents"),
    );
    for (key, action) in [
        ('l', Action::AgentActivity),
        ('n', Action::ToggleAgentNote),
        ('x', Action::DismissAgentNote),
        ('j', Action::NextAgentNote),
        ('d', Action::ScrollAgentNoteDown),
        ('u', Action::ScrollAgentNoteUp),
        ('y', Action::AcceptAgentSuggestion),
        ('t', Action::ThreadsWithSuggestions),
    ] {
        bind(
            &[KeyId::Leader, KeyId::Char('a'), KeyId::Char(key)],
            action,
            Some("Agents"),
        );
    }
    for (key, action) in [
        ('w', Action::ToggleWebView),
        ('i', Action::AllowMessageImages),
        ('I', Action::AllowSenderImages),
        ('B', Action::BlockSenderImages),
    ] {
        if action != Action::BlockSenderImages {
            bind(&[KeyId::Char(key)], action, None);
        }
        bind(
            &[KeyId::Leader, KeyId::Char('t'), KeyId::Char(key)],
            action,
            Some("Reading / notifications"),
        );
    }
    bind(
        &[KeyId::Leader, KeyId::Char('t'), KeyId::Char('z')],
        Action::FullWidth,
        Some("Reading / notifications"),
    );
    bind(
        &[KeyId::Leader, KeyId::Char('t'), KeyId::Char('q')],
        Action::ToggleQuotes,
        Some("Reading / notifications"),
    );
    for (key, mode) in [
        ('o', NotificationMode::On),
        ('p', NotificationMode::Private),
        ('f', NotificationMode::Off),
    ] {
        bind(
            &[
                KeyId::Leader,
                KeyId::Char('t'),
                KeyId::Char('n'),
                KeyId::Char(key),
            ],
            Action::Notifications(mode),
            Some("Notifications"),
        );
    }
    bindings
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeqMatch {
    Action(Action),
    Prefix,
    None,
}

pub fn match_seq(keymap: &Keymap, seq: &[KeyId]) -> SeqMatch {
    let mut prefix = false;
    for binding in &keymap.bindings {
        if binding.keys == seq {
            return SeqMatch::Action(binding.action);
        }
        if binding.keys.starts_with(seq) {
            prefix = true;
        }
    }
    if prefix {
        SeqMatch::Prefix
    } else {
        SeqMatch::None
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WhichItem {
    pub key: String,
    pub label: String,
    pub is_group: bool,
}

#[cfg(test)]
pub fn which_key(keymap: &Keymap, prefix: &[KeyId]) -> Vec<WhichItem> {
    which_key_filtered(keymap, prefix, |_| true)
}

pub fn which_key_filtered(
    keymap: &Keymap,
    prefix: &[KeyId],
    visible: impl Fn(Action) -> bool,
) -> Vec<WhichItem> {
    let mut groups: Vec<(String, Vec<&Binding>)> = Vec::new();
    for binding in &keymap.bindings {
        if binding.keys.len() <= prefix.len()
            || !binding.keys.starts_with(prefix)
            || !visible(binding.action)
        {
            continue;
        }
        let key = key_label(binding.keys[prefix.len()], &keymap.leader_name);
        if let Some((_, bucket)) = groups.iter_mut().find(|(name, _)| name == &key) {
            bucket.push(binding);
        } else {
            groups.push((key, vec![binding]));
        }
    }
    // A group takes the position of its first action in the discovery order.
    groups.sort_by_cached_key(|(_, bindings)| {
        Action::all()
            .position(|action| bindings.iter().any(|binding| binding.action == action))
            .unwrap_or(usize::MAX)
    });
    groups
        .into_iter()
        .map(|(key, bindings)| which_item(key, prefix, &bindings))
        .collect()
}

fn which_item(key: String, prefix: &[KeyId], bindings: &[&Binding]) -> WhichItem {
    let (label, is_group) = if let [binding] = bindings
        && binding.keys.len() == prefix.len() + 1
    {
        (binding.action.label(), false)
    } else {
        let group = bindings
            .iter()
            .find_map(|binding| binding.group)
            .unwrap_or("More");
        (group, true)
    };
    WhichItem {
        key,
        label: label.to_owned(),
        is_group,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaletteItem {
    pub action: Action,
    pub label: String,
    pub binding: String,
    pub search: Option<String>,
}

pub fn recent_search_items<'a>(
    query: &'a str,
    searches: &'a [String],
) -> impl Iterator<Item = PaletteItem> + 'a {
    searches
        .iter()
        .filter(|search| fuzzy(query, search).is_some())
        .map(|search| PaletteItem {
            action: Action::Search,
            label: search.clone(),
            binding: "Recent search".to_owned(),
            search: Some(search.clone()),
        })
}

pub fn palette_items(keymap: &Keymap, query: &str, recent: &[Action]) -> Vec<PaletteItem> {
    let mut items: Vec<(i32, usize, PaletteItem)> = Vec::new();
    for (index, action) in Action::all().enumerate() {
        let label = action.label();
        let score = fuzzy(query, label).or_else(|| match action {
            Action::About => fuzzy(query, "version"),
            Action::ReopenReview => fuzzy(query, "Reopen closed agent review"),
            Action::Agents => fuzzy(query, "Manage connected agents and mailbox access"),
            _ => None,
        });
        let Some(score) = score else {
            continue;
        };
        let recent_rank = recent
            .iter()
            .position(|item| *item == action)
            .map(|pos| pos as i32)
            .unwrap_or(100);
        let query_empty = query.trim().is_empty();
        let sort = if query_empty { recent_rank } else { -score };
        items.push((
            sort,
            index,
            PaletteItem {
                action,
                label: label.to_string(),
                binding: binding_text(keymap, action),
                search: None,
            },
        ));
    }
    items.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    items.into_iter().map(|(_, _, item)| item).collect()
}

pub fn binding_text(keymap: &Keymap, action: Action) -> String {
    if let Action::Setting(SettingsRow::ResendChanges | SettingsRow::DiscardChanges) = action {
        let key = if action == Action::Setting(SettingsRow::ResendChanges) {
            crate::model::settings::RESEND_CHANGES_KEY
        } else {
            crate::model::settings::DISCARD_CHANGES_KEY
        };
        return format!("{} in Accounts", key_label(key, &keymap.leader_name));
    }
    if action == Action::Setting(SettingsRow::RemoveAccount) {
        let key = key_label(
            crate::model::settings::REMOVE_ACCOUNT_KEY,
            &keymap.leader_name,
        );
        return format!("{key} in Accounts");
    }
    if action == Action::Setting(SettingsRow::EditConnection) {
        let key = key_label(
            crate::model::settings::EDIT_CONNECTION_KEY,
            &keymap.leader_name,
        );
        return format!("{key} in Accounts");
    }
    keymap
        .bindings_for(action)
        .iter()
        .map(|binding| {
            binding
                .keys
                .iter()
                .map(|key| key_label(*key, &keymap.leader_name))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .chain(
            keymap
                .attachment_bindings
                .iter()
                .filter(|(_, found)| *found == action)
                .map(|(key, _)| key_label(*key, &keymap.leader_name)),
        )
        .collect::<Vec<_>>()
        .join(" · ")
}

pub fn key_label(key: KeyId, leader_name: &str) -> String {
    match key {
        KeyId::Char('I') => "⇧I".into(),
        KeyId::Char(ch) => ch.to_string(),
        KeyId::Space => "space".into(),
        KeyId::Leader => leader_name.to_string(),
        KeyId::Ctrl(ch) => format!("ctrl-{ch}"),
        KeyId::CtrlShiftP => "ctrl-shift-p".into(),
        KeyId::Escape => "esc".into(),
        KeyId::Backspace => "backspace".into(),
        KeyId::Enter => "enter".into(),
        KeyId::Up => "up".into(),
        KeyId::Down => "down".into(),
        KeyId::Tab => "tab".into(),
        KeyId::Left => "left".into(),
        KeyId::Right => "right".into(),
        KeyId::Home => "home".into(),
        KeyId::End => "end".into(),
        KeyId::Delete => "delete".into(),
    }
}

pub fn prefix_text(keys: &[KeyId], leader_name: &str) -> String {
    let mut out = String::new();
    for (index, key) in keys.iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        let _ = write!(out, "{}", key_label(*key, leader_name));
    }
    out
}

/// `leader` and `which_key_ms` from the keymap file. Other lines are ignored.
pub fn keymap_from_text(text: &str) -> Result<Keymap, String> {
    let mut leader = KeyId::Space;
    let mut leader_name = "space".to_string();
    let mut which_key_ms = 300_u64;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"').trim_matches('\'');
        match key {
            "leader" => {
                let parsed = parse_leader(value)?;
                leader = parsed.0;
                leader_name = parsed.1;
            }
            "which_key_ms" => {
                which_key_ms = value
                    .parse()
                    .map_err(|_| format!("bad which_key_ms {value}"))?;
            }
            _ => {}
        }
    }
    Ok(Keymap::new(leader, leader_name, which_key_ms))
}

fn parse_leader(value: &str) -> Result<(KeyId, String), String> {
    match value {
        "space" | "Space" => Ok((KeyId::Space, "space".into())),
        other if other.chars().count() == 1 => {
            let ch = other.chars().next().unwrap();
            Ok((KeyId::Char(ch), other.to_string()))
        }
        other => Err(format!("unsupported leader {other}")),
    }
}

pub(crate) fn fuzzy(query: &str, label: &str) -> Option<i32> {
    fuzzy_score(&fuzzy_query(query), label)
}

pub(crate) fn fuzzy_query(query: &str) -> Vec<char> {
    query.chars().flat_map(char::to_lowercase).collect()
}

pub(crate) fn fuzzy_score(query: &[char], label: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let mut score = 0;
    let mut qi = 0;
    let mut previous = None;
    let mut consecutive = false;
    for ch in label.chars().flat_map(char::to_lowercase) {
        let matches = ch == query[qi];
        if matches {
            score += 10;
            if consecutive {
                score += 8;
            }
            if previous.is_none_or(|ch: char| !ch.is_ascii_alphanumeric()) {
                score += 6;
            }
            qi += 1;
            if qi == query.len() {
                return Some(score);
            }
        }
        previous = Some(ch);
        consecutive = matches;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leader_defaults_to_space_and_m_moves() {
        let keymap = Keymap::default();
        assert_eq!(keymap.leader, KeyId::Space);
        assert_eq!(keymap.which_key_ms, 300);
        assert_eq!(
            match_seq(&keymap, &[KeyId::Char('m')]),
            SeqMatch::Action(Action::Move)
        );
        assert_eq!(
            match_seq(&keymap, &[KeyId::Char('e')]),
            SeqMatch::Action(Action::Archive)
        );
        assert_eq!(
            match_seq(&keymap, &[KeyId::Char('u')]),
            SeqMatch::Action(Action::Undo)
        );
        let root = which_key(&keymap, &[]);
        assert!(
            root.iter()
                .any(|item| item.key == "m" && item.label == "Move")
        );
        assert!(
            root.iter()
                .any(|item| item.key == "?" && item.label == "Help" && !item.is_group)
        );
        assert!(
            palette_items(&keymap, "help", &[])
                .iter()
                .any(|item| item.action == Action::Help && item.binding == "? · space ? ?")
        );
        let leader = which_key(&keymap, &[KeyId::Leader]);
        assert!(
            leader
                .iter()
                .any(|item| item.key == "?" && item.label == "General" && item.is_group)
        );
        let help = [KeyId::Leader, KeyId::Char('?')];
        assert_eq!(match_seq(&keymap, &help), SeqMatch::Prefix);
        for (key, action) in [
            ('?', Action::Help),
            ('c', Action::OpenCrashReport),
            ('l', Action::OpenDiagnosticLog),
        ] {
            assert_eq!(
                match_seq(&keymap, &[help[0], help[1], KeyId::Char(key)]),
                SeqMatch::Action(action)
            );
            assert!(
                which_key(&keymap, &help)
                    .iter()
                    .any(|item| item.key == key.to_string() && item.label == action.label())
            );
        }
        assert!(
            leader
                .iter()
                .any(|item| item.key == "f" && item.label == "Folders" && item.is_group)
        );
        assert!(leader.iter().any(|item| item.key == "t"
            && item.label == "Reading / notifications"
            && item.is_group));
        assert!(
            leader
                .iter()
                .any(|item| item.key == "b" && item.label == "Organize" && item.is_group)
        );
        assert!(leader.iter().all(|item| item.key != "m"));
        let folders = which_key(&keymap, &[KeyId::Leader, KeyId::Char('f')]);
        assert!(folders.iter().any(|item| item.key == "i"));
        assert!(
            folders
                .iter()
                .any(|item| item.key == "m" && item.label == "Move")
        );
    }

    #[test]
    fn which_key_marks_groups_and_shares_the_review_label_with_the_palette() {
        let keymap = Keymap::default();
        let leader = which_key(&keymap, &[KeyId::Leader]);
        assert!(
            leader
                .iter()
                .any(|item| item.key == "s" && item.label == "Sync now" && !item.is_group)
        );
        assert!(
            leader.iter().any(|item| item.key == "a"
                && item.label == "Attachments / agents"
                && item.is_group)
        );

        let agents = which_key(&keymap, &[KeyId::Leader, KeyId::Char('a')]);
        let review = agents.iter().find(|item| item.key == "r").unwrap();
        assert_eq!(review.label, "Reopen agent review");
        assert!(!review.is_group);

        for query in ["agent", "closed"] {
            let matches = palette_items(&keymap, query, &[]);
            let command = matches
                .iter()
                .find(|item| item.action == Action::ReopenReview)
                .unwrap();
            assert_eq!(command.label, review.label);
        }
    }

    #[test]
    fn every_bound_action_has_one_section() {
        let keymap = Keymap::default();
        let mut catalog = std::collections::HashSet::new();
        for action in Action::all() {
            assert!(catalog.insert(action), "duplicate action: {action:?}");
        }
        for action in keymap
            .bindings
            .iter()
            .map(|binding| binding.action)
            .chain(keymap.attachment_bindings.iter().map(|&(_, action)| action))
        {
            assert!(catalog.contains(&action), "missing action: {action:?}");
        }
    }

    #[test]
    fn which_key_follows_the_palette_order_at_each_chord_prefix() {
        let keymap = Keymap::default();
        let palette = palette_items(&keymap, "", &[]);
        let prefixes: std::collections::HashSet<_> = keymap
            .bindings
            .iter()
            .flat_map(|binding| (0..binding.keys.len()).map(|len| &binding.keys[..len]))
            .collect();
        for prefix in prefixes {
            let mut expected = Vec::new();
            for item in &palette {
                for binding in keymap.bindings_for(item.action) {
                    if binding.keys.starts_with(prefix)
                        && let Some(&key) = binding.keys.get(prefix.len())
                    {
                        let label = key_label(key, &keymap.leader_name);
                        if !expected.contains(&label) {
                            expected.push(label);
                        }
                    }
                }
            }
            let shown: Vec<_> = which_key(&keymap, prefix)
                .into_iter()
                .map(|item| item.key)
                .collect();
            assert_eq!(shown, expected, "prefix: {prefix:?}");
        }
    }

    #[test]
    fn chords_and_direct_keys_resolve() {
        let keymap = Keymap::default();
        assert_eq!(
            match_seq(&keymap, &[KeyId::Char('g'), KeyId::Char('i')]),
            SeqMatch::Action(Action::Inbox)
        );
        assert_eq!(match_seq(&keymap, &[KeyId::Char('g')]), SeqMatch::Prefix);
        assert_eq!(
            match_seq(
                &keymap,
                &[KeyId::Leader, KeyId::Char('f'), KeyId::Char('s')]
            ),
            SeqMatch::Action(Action::Sent)
        );
        assert_eq!(
            match_seq(&keymap, &[KeyId::Ctrl('d')]),
            SeqMatch::Action(Action::PageDown)
        );
        assert_eq!(
            match_seq(&keymap, &[KeyId::Ctrl('u')]),
            SeqMatch::Action(Action::PageUp)
        );
        assert_eq!(
            match_seq(&keymap, &[KeyId::Char(':')]),
            SeqMatch::Action(Action::Palette)
        );
        assert_eq!(
            match_seq(&keymap, &[KeyId::CtrlShiftP]),
            SeqMatch::Action(Action::Palette)
        );
        assert_eq!(match_seq(&keymap, &[KeyId::Space]), SeqMatch::None);
    }

    #[test]
    fn palette_lists_bindings_and_recent_first() {
        let keymap = Keymap::default();
        let items = palette_items(&keymap, "", &[Action::Sent, Action::Inbox]);
        assert_eq!(items[0].action, Action::Sent);
        assert_eq!(items[1].action, Action::Inbox);
        let move_item = items
            .iter()
            .find(|item| item.action == Action::Move)
            .expect("move is in the palette");
        assert!(move_item.binding.contains('m'));
        assert!(
            items
                .iter()
                .any(|item| item.action == Action::Undo && item.binding.contains('u'))
        );
        let inbox = palette_items(&keymap, "inbox", &[]);
        assert_eq!(inbox[0].action, Action::Inbox);
        assert!(inbox[0].binding.contains("g i"));
        assert!(inbox[0].binding.contains("space f i"));
    }

    #[test]
    fn c_composes_and_the_palette_says_so() {
        let keymap = Keymap::default();
        assert_eq!(
            match_seq(&keymap, &[KeyId::Char('c')]),
            SeqMatch::Action(Action::Compose)
        );
        let items = palette_items(&keymap, "compose", &[]);
        assert_eq!(items[0].action, Action::Compose);
        assert_eq!(items[0].binding, "c");
        assert!(
            which_key(&keymap, &[])
                .iter()
                .any(|item| item.key == "c" && item.label == "Compose")
        );
    }

    #[test]
    fn r_shift_r_and_f_answer_and_the_palette_lists_them() {
        let keymap = Keymap::default();
        for (key, action, label) in [
            ('r', Action::Reply, "Reply"),
            ('R', Action::ReplyAll, "Reply all"),
            ('f', Action::Forward, "Forward"),
        ] {
            assert_eq!(
                match_seq(&keymap, &[KeyId::Char(key)]),
                SeqMatch::Action(action)
            );
            let item = palette_items(&keymap, "", &[])
                .into_iter()
                .find(|item| item.action == action)
                .expect("in the palette");
            assert_eq!(item.binding, key.to_string());
            assert_eq!(item.label, label);
        }
        assert_eq!(
            palette_items(&keymap, "reply all", &[])[0].action,
            Action::ReplyAll
        );
        assert_eq!(
            palette_items(&keymap, "forward", &[])[0].action,
            Action::Forward
        );
        // The folders chord under the leader still starts with f.
        assert_eq!(
            match_seq(&keymap, &[KeyId::Leader, KeyId::Char('f')]),
            SeqMatch::Prefix
        );
    }

    #[test]
    fn hash_deletes_and_bang_marks_spam_in_the_palette_and_which_key() {
        let keymap = Keymap::default();
        for (key, action, label) in [
            ('#', Action::Delete, "Delete"),
            ('!', Action::Spam, "Spam / not spam"),
        ] {
            assert_eq!(
                match_seq(&keymap, &[KeyId::Char(key)]),
                SeqMatch::Action(action)
            );
            assert_eq!(
                match_seq(
                    &keymap,
                    &[KeyId::Leader, KeyId::Char('f'), KeyId::Char(key)]
                ),
                SeqMatch::Action(action)
            );
            let folders = which_key(&keymap, &[KeyId::Leader, KeyId::Char('f')]);
            assert!(
                folders
                    .iter()
                    .any(|item| item.key == key.to_string() && item.label == label),
                "{folders:?}"
            );
            let item = palette_items(&keymap, "", &[])
                .into_iter()
                .find(|item| item.action == action)
                .expect("in the palette");
            assert_eq!(item.label, label);
            assert_eq!(item.binding, format!("{key} · space f {key}"));
        }
        assert_eq!(
            palette_items(&keymap, "delete", &[])[0].action,
            Action::Delete
        );
        assert_eq!(palette_items(&keymap, "spam", &[])[0].action, Action::Spam);
    }

    #[test]
    fn leader_is_configurable() {
        let keymap = keymap_from_text("leader = \",\"\nwhich_key_ms = 0\n").unwrap();
        assert_eq!(keymap.leader, KeyId::Char(','));
        assert_eq!(keymap.which_key_ms, 0);
        assert_eq!(keymap.leader_name, ",");
        assert!(
            which_key(&keymap, &[KeyId::Leader])
                .iter()
                .any(|item| item.key == "f")
        );
    }
}
