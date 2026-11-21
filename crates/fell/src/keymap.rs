//! Leader chords, which-key, and the command palette index.
//!
//! Use `m` to move, `e` to archive, `#` to move to Trash, and `!` for
//! spam or not spam. `U` toggles unread and `s` toggles star. `u` undoes
//! the latest reversible action.

use std::fmt::Write as _;

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
    Compose,
    Reply,
    ReplyAll,
    Forward,
    FocusAttachments,
    PreviousAttachment,
    NextAttachment,
    OpenAttachment,
    SaveAttachment,
    SaveAllAttachments,
    RetryAttachment,
    FolderPicker,
    Label,
    LabelList,
    Search,
    NextHit,
    PreviousHit,
    Move,
    Archive,
    ToggleUnread,
    ToggleStar,
    Delete,
    Spam,
    Undo,
    SyncNow,
    ToggleSelect,
    Visual,
    FullWidth,
    ToggleQuotes,
    Palette,
    Help,
    About,
    ReopenReview,
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
            Self::Compose => "Compose",
            Self::Reply => "Reply",
            Self::ReplyAll => "Reply all",
            Self::Forward => "Forward",
            Self::FocusAttachments => "Focus attachments",
            Self::PreviousAttachment => "Previous attachment",
            Self::NextAttachment => "Next attachment",
            Self::OpenAttachment => "Open attachment",
            Self::SaveAttachment => "Save attachment",
            Self::SaveAllAttachments => "Save all attachments",
            Self::RetryAttachment => "Retry attachment action",
            Self::FolderPicker => "Choose folder",
            Self::Label => "Apply or remove label",
            Self::LabelList => "Go to label",
            Self::Search => "Search mail",
            Self::NextHit => "Next search hit",
            Self::PreviousHit => "Previous search hit",
            Self::Move => "Move",
            Self::Archive => "Archive",
            Self::ToggleUnread => "Mark unread or read",
            Self::ToggleStar => "Star or unstar",
            Self::Delete => "Delete",
            Self::Spam => "Spam / not spam",
            Self::Undo => "Undo",
            Self::SyncNow => "Sync now",
            Self::ToggleSelect => "Toggle select",
            Self::Visual => "Visual select",
            Self::FullWidth => "Full-width conversation",
            Self::ToggleQuotes => "Quoted text and recipients",
            Self::Palette => "Command palette",
            Self::Help => "Help",
            Self::About => "About Fell",
            Self::ReopenReview => "Reopen agent review",
        }
    }

    pub fn all() -> &'static [Action] {
        &ALL_ACTIONS
    }
}

const ALL_ACTIONS: [Action; 44] = [
    Action::Next,
    Action::Prev,
    Action::First,
    Action::Last,
    Action::Open,
    Action::Back,
    Action::PageDown,
    Action::PageUp,
    Action::Inbox,
    Action::Sent,
    Action::Drafts,
    Action::Compose,
    Action::Reply,
    Action::ReplyAll,
    Action::Forward,
    Action::FocusAttachments,
    Action::PreviousAttachment,
    Action::NextAttachment,
    Action::OpenAttachment,
    Action::SaveAttachment,
    Action::SaveAllAttachments,
    Action::RetryAttachment,
    Action::FolderPicker,
    Action::Label,
    Action::LabelList,
    Action::Search,
    Action::NextHit,
    Action::PreviousHit,
    Action::Move,
    Action::Archive,
    Action::ToggleUnread,
    Action::ToggleStar,
    Action::Delete,
    Action::Spam,
    Action::Undo,
    Action::SyncNow,
    Action::ToggleSelect,
    Action::Visual,
    Action::FullWidth,
    Action::ToggleQuotes,
    Action::Palette,
    Action::Help,
    Action::ReopenReview,
    Action::About,
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
    bind(&[KeyId::Char('a')], Action::FocusAttachments, None);
    bind(&[KeyId::Char('S')], Action::SaveAttachment, None);
    bind(&[KeyId::Char('m')], Action::Move, None);
    bind(&[KeyId::Char('e')], Action::Archive, None);
    bind(&[KeyId::Char('U')], Action::ToggleUnread, None);
    bind(&[KeyId::Char('s')], Action::ToggleStar, None);
    bind(&[KeyId::Char('#')], Action::Delete, None);
    bind(&[KeyId::Char('!')], Action::Spam, None);
    bind(&[KeyId::Char('u')], Action::Undo, None);
    bind(&[KeyId::Char('x')], Action::ToggleSelect, None);
    bind(&[KeyId::Char('V')], Action::Visual, None);
    bind(&[KeyId::Char('z')], Action::FullWidth, None);
    bind(&[KeyId::Char('q')], Action::ToggleQuotes, None);
    bind(&[KeyId::Char(':')], Action::Palette, None);
    bind(&[KeyId::CtrlShiftP], Action::Palette, None);
    bind(&[KeyId::Char('?')], Action::Help, None);
    bind(&[KeyId::Leader, KeyId::Char('?')], Action::Help, None);
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
        &[KeyId::Leader, KeyId::Char('a'), KeyId::Char('r')],
        Action::ReopenReview,
        Some("Agents"),
    );
    bind(
        &[KeyId::Leader, KeyId::Char('t'), KeyId::Char('z')],
        Action::FullWidth,
        Some("Toggles"),
    );
    bind(
        &[KeyId::Leader, KeyId::Char('t'), KeyId::Char('q')],
        Action::ToggleQuotes,
        Some("Toggles"),
    );
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

pub fn which_key(keymap: &Keymap, prefix: &[KeyId]) -> Vec<WhichItem> {
    let mut groups: Vec<(String, Vec<&Binding>)> = Vec::new();
    for binding in &keymap.bindings {
        if binding.keys.len() <= prefix.len() || !binding.keys.starts_with(prefix) {
            continue;
        }
        let key = key_label(binding.keys[prefix.len()], &keymap.leader_name);
        if let Some((_, bucket)) = groups.iter_mut().find(|(name, _)| name == &key) {
            bucket.push(binding);
        } else {
            groups.push((key, vec![binding]));
        }
    }
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
    for (index, action) in Action::all().iter().copied().enumerate() {
        let label = action.label();
        let score = fuzzy(query, label).or_else(|| match action {
            Action::About => fuzzy(query, "version"),
            Action::ReopenReview => fuzzy(query, "Reopen closed agent review"),
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
                .any(|item| item.action == Action::Help && item.binding == "? · space ?")
        );
        let leader = which_key(&keymap, &[KeyId::Leader]);
        assert!(
            leader
                .iter()
                .any(|item| item.key == "?" && item.label == "Help" && !item.is_group)
        );
        assert_eq!(
            match_seq(&keymap, &[KeyId::Leader, KeyId::Char('?')]),
            SeqMatch::Action(Action::Help)
        );
        assert!(
            leader
                .iter()
                .any(|item| item.key == "f" && item.label == "Folders" && item.is_group)
        );
        assert!(
            leader
                .iter()
                .any(|item| item.key == "t" && item.label == "Toggles" && item.is_group)
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
