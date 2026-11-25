//! The live preferences in settings.toml. Account, keymap and window state
//! still have their own files.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use fell_core::NotificationMode;
use toml_edit::{DocumentMut, Item, Value};

use crate::mailbox;
use crate::motion::Motion;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorScheme {
    #[default]
    System,
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Preference {
    Theme,
    FontSize,
    ReduceMotion,
    ReadDwell,
    WhichKey,
    Notifications,
}

impl Preference {
    const ALL: [Self; 6] = [
        Self::Theme,
        Self::FontSize,
        Self::ReduceMotion,
        Self::ReadDwell,
        Self::WhichKey,
        Self::Notifications,
    ];

    fn key(self) -> &'static str {
        match self {
            Self::Theme => "theme",
            Self::FontSize => "font_size",
            Self::ReduceMotion => "reduce_motion",
            Self::ReadDwell => "read_dwell_ms",
            Self::WhichKey => "which_key_ms",
            Self::Notifications => "notifications",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Theme => "Theme",
            Self::FontSize => "Font size",
            Self::ReduceMotion => "Reduce motion",
            Self::ReadDwell => "Mark-read delay",
            Self::WhichKey => "Which-key delay",
            Self::Notifications => "Desktop notifications",
        }
    }

    pub fn command(self) -> &'static str {
        match self {
            Self::Theme => "Settings: Theme",
            Self::FontSize => "Settings: Font size",
            Self::ReduceMotion => "Settings: Reduce motion",
            Self::ReadDwell => "Settings: Mark-read delay",
            Self::WhichKey => "Settings: Which-key delay",
            Self::Notifications => "Settings: Desktop notifications",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Theme => "Desktop colors or a Fell theme.",
            Self::FontSize => "Inbox and composer text.",
            Self::ReduceMotion => "Limit interface animations.",
            Self::ReadDwell => "Wait before marking an open thread read.",
            Self::WhichKey => "Wait for shortcut suggestions.",
            Self::Notifications => "What new-mail alerts show.",
        }
    }

    pub fn display(self, values: Values) -> String {
        match self {
            Self::Theme => match values.theme {
                ColorScheme::System => "Follow desktop",
                ColorScheme::Dark => "Dark",
                ColorScheme::Light => "Light",
            }
            .into(),
            Self::FontSize => values
                .font_size
                .map_or_else(|| "System".into(), |px| format!("{px} px")),
            Self::ReduceMotion => match values.reduce_motion {
                None => "Follow desktop",
                Some(true) => "On",
                Some(false) => "Off",
            }
            .into(),
            Self::ReadDwell => delay_label(values.read_dwell_ms),
            Self::WhichKey => delay_label(values.which_key_ms),
            Self::Notifications => match values.notifications {
                NotificationMode::On => "Sender + subject",
                NotificationMode::Private => "Private",
                NotificationMode::Off => "Off",
            }
            .into(),
        }
    }

    fn advance(self, values: Values, backwards: bool) -> Value {
        match self {
            Self::Theme => cycle(
                &["system", "dark", "light"],
                match values.theme {
                    ColorScheme::System => "system",
                    ColorScheme::Dark => "dark",
                    ColorScheme::Light => "light",
                },
                backwards,
            )
            .into(),
            Self::FontSize => match step(
                &[0, 12, 13, 14, 15, 16, 17, 18],
                values.font_size.unwrap_or(0),
                backwards,
            ) {
                0 => Value::from("system"),
                px => Value::from(px as i64),
            },
            Self::ReduceMotion => match cycle(
                &[None, Some(true), Some(false)],
                values.reduce_motion,
                backwards,
            ) {
                Some(reduce) => reduce.into(),
                None => "system".into(),
            },
            Self::ReadDwell => Value::from(step(
                &[0, 250, 500, 750, 1000, 1500, 2000, 3000, 5000, 10000],
                values.read_dwell_ms,
                backwards,
            ) as i64),
            Self::WhichKey => Value::from(step(
                &[0, 150, 300, 500, 750, 1000, 1500, 3000, 5000],
                values.which_key_ms,
                backwards,
            ) as i64),
            Self::Notifications => cycle(
                &["on", "private", "off"],
                values.notifications.as_str(),
                backwards,
            )
            .into(),
        }
    }

    fn apply(self, item: &Item, values: &mut Values) -> Option<()> {
        let integer = |range: std::ops::RangeInclusive<u64>| {
            let value = u64::try_from(item.as_integer()?).ok()?;
            range.contains(&value).then_some(value)
        };
        match self {
            Self::Theme => {
                values.theme = match item.as_str()? {
                    "system" => ColorScheme::System,
                    "dark" => ColorScheme::Dark,
                    "light" => ColorScheme::Light,
                    _ => return None,
                };
            }
            Self::FontSize => {
                values.font_size = if item.as_str() == Some("system") {
                    None
                } else {
                    Some(integer(12..=18)?)
                };
            }
            Self::ReduceMotion => {
                values.reduce_motion = if item.as_str() == Some("system") {
                    None
                } else {
                    Some(item.as_bool()?)
                };
            }
            Self::ReadDwell => values.read_dwell_ms = integer(0..=10_000)?,
            Self::WhichKey => values.which_key_ms = integer(0..=5_000)?,
            Self::Notifications => {
                values.notifications = match item.as_str()? {
                    "on" => NotificationMode::On,
                    "private" => NotificationMode::Private,
                    "off" => NotificationMode::Off,
                    _ => return None,
                };
            }
        }
        Some(())
    }
}

fn delay_label(ms: u64) -> String {
    if ms == 0 {
        "Immediately".into()
    } else {
        format!("{ms} ms")
    }
}

fn cycle<T: Copy + PartialEq>(options: &[T], current: T, backwards: bool) -> T {
    let index = options
        .iter()
        .position(|value| *value == current)
        .unwrap_or(0);
    let offset = if backwards { options.len() - 1 } else { 1 };
    options[(index + offset) % options.len()]
}

fn step(options: &[u64], current: u64, backwards: bool) -> u64 {
    if backwards {
        options
            .iter()
            .rev()
            .copied()
            .find(|value| *value < current)
            .unwrap_or(options[options.len() - 1])
    } else {
        options
            .iter()
            .copied()
            .find(|value| *value > current)
            .unwrap_or(options[0])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Values {
    pub theme: ColorScheme,
    pub font_size: Option<u64>,
    /// None follows GTK, even if a legacy environment override is set.
    pub reduce_motion: Option<bool>,
    pub read_dwell_ms: u64,
    pub which_key_ms: u64,
    pub notifications: NotificationMode,
}

impl Default for Values {
    fn default() -> Self {
        Self {
            theme: ColorScheme::System,
            font_size: None,
            reduce_motion: Motion::environment_preference(),
            read_dwell_ms: 0,
            which_key_ms: 300,
            notifications: NotificationMode::On,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub values: Values,
    pub read_error: Option<String>,
    pub unknown: Vec<String>,
    pub invalid: Vec<(Preference, String)>,
}

impl Snapshot {
    fn parse(document: &DocumentMut, mut values: Values) -> Self {
        let mut invalid = Vec::new();
        for preference in Preference::ALL {
            if let Some(item) = document.get(preference.key())
                && preference.apply(item, &mut values).is_none()
            {
                invalid.push((preference, invalid_label(item)));
            }
        }
        let unknown = document
            .iter()
            .filter(|(key, _)| {
                !Preference::ALL
                    .iter()
                    .any(|preference| preference.key() == *key)
            })
            .map(|(key, _)| key.to_owned())
            .collect();
        Self {
            values,
            read_error: None,
            unknown,
            invalid,
        }
    }

    pub fn notice(&self) -> Option<String> {
        self.read_error.clone().or_else(|| {
            (!self.invalid.is_empty()).then(|| {
                format!(
                    "settings.toml contains invalid values · {}",
                    self.invalid
                        .iter()
                        .map(|(key, _)| key.key())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
        })
    }

    pub fn read(path: Option<&Path>, defaults: Values) -> Self {
        let fallback = Self::parse(&DocumentMut::new(), defaults);
        fallback.reload(path, defaults)
    }

    pub fn reload(&self, path: Option<&Path>, defaults: Values) -> Self {
        let Some(path) = path else {
            return Self::parse(&DocumentMut::new(), defaults);
        };
        match read_complete(path) {
            Ok((document, _)) => Self::parse(&document, defaults),
            Err(error) => Self {
                read_error: Some(format!("Could not read settings.toml · {error}")),
                ..self.clone()
            },
        }
    }
}

fn invalid_label(item: &Item) -> String {
    let bounded = |text: &str| {
        let mut chars = text.chars();
        let mut label: String = chars.by_ref().take(18).collect();
        if chars.next().is_some() {
            label.push('…');
        }
        crate::present::display_safe(&label)
    };
    if let Some(text) = item.as_str() {
        format!("{:?}", bounded(text))
    } else if let Some(value) = item.as_value() {
        bounded(&value.to_string())
    } else {
        "Table".into()
    }
}

pub fn path() -> Option<PathBuf> {
    mailbox::config_dir().map(|dir| dir.join("settings.toml"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Source {
    device: u64,
    inode: u64,
    modified: (i64, i64),
    text: String,
}

fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn resolve(path: &Path) -> io::Result<PathBuf> {
    if !path.is_symlink() {
        return Ok(path.to_path_buf());
    }
    let target = path.canonicalize()?;
    if target
        .file_name()
        .is_some_and(|name| name == "account.toml")
    {
        return Err(io::Error::other(
            "settings.toml must not link to account.toml",
        ));
    }
    Ok(target)
}

fn read_source(path: &Path, account: &Path) -> io::Result<Option<Source>> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let before = file.metadata()?;
    // SAFETY: geteuid has no arguments and returns the current effective user.
    let user = unsafe { libc::geteuid() };
    if !before.is_file()
        || before.uid() != user
        || fs::metadata(account).is_ok_and(|meta| same_file(&before, &meta))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "settings.toml must be a regular file owned by you and separate from account.toml",
        ));
    }
    let mut bytes = Vec::new();
    let mut second = Vec::new();
    file.read_to_end(&mut bytes)?;
    let between = file.metadata()?;
    file.rewind()?;
    file.read_to_end(&mut second)?;
    let after = file.metadata()?;
    let observed = fs::symlink_metadata(path).map_err(|_| changed_while_saving())?;
    let stamp = |meta: &fs::Metadata| (meta.mtime(), meta.mtime_nsec());
    if bytes != second
        || [&between, &after, &observed]
            .into_iter()
            .any(|meta| meta.len() != bytes.len() as u64 || stamp(meta) != stamp(&before))
        || before.len() != bytes.len() as u64
        || !same_file(&before, &observed)
    {
        return Err(changed_while_saving());
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Expected UTF-8 text"))?;
    Ok(Some(Source {
        device: before.dev(),
        inode: before.ino(),
        modified: stamp(&before),
        text,
    }))
}

fn changed_while_saving() -> io::Error {
    io::Error::other("settings.toml changed while saving. Try again")
}

fn read_complete(path: &Path) -> io::Result<(DocumentMut, Option<Source>)> {
    let target = resolve(path)?;
    let source = read_source(&target, &path.with_file_name("account.toml"))?;
    let document = if let Some(source) = &source {
        source.text.parse().map_err(|error: toml_edit::TomlError| {
            let offset = error.span().map_or(0, |span| span.start);
            let line = 1 + source
                .text
                .bytes()
                .take(offset)
                .filter(|b| *b == b'\n')
                .count();
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("TOML syntax error on line {line} · {}", error.message()),
            )
        })?
    } else {
        DocumentMut::new()
    };
    Ok((document, source))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edit {
    Cycle(Preference, bool),
    Notifications(NotificationMode),
}

pub fn save(path: &Path, defaults: Values, edit: Edit) -> io::Result<Snapshot> {
    let config = path.parent().expect("settings file has a parent");
    let config_dir = mailbox::private_config_dir(config)?;
    let target = resolve(path)?;
    let dir = target.parent().expect("target file has a parent");
    let lock = if dir == config {
        config_dir
    } else {
        File::open(dir)?
    };
    lock.try_lock()?;
    let (mut document, before) = read_complete(path)?;
    let (preference, mut value) = match edit {
        Edit::Cycle(preference, backwards) => (
            preference,
            preference.advance(Snapshot::parse(&document, defaults).values, backwards),
        ),
        Edit::Notifications(mode) => (Preference::Notifications, Value::from(mode.as_str())),
    };
    if let Some(existing) = document.get_mut(preference.key()) {
        let Some(old) = existing.as_value().filter(|old| !old.is_inline_table()) else {
            return Err(io::Error::other(
                "Correct the setting in settings.toml first",
            ));
        };
        *value.decor_mut() = old.decor().clone();
        *existing = Item::Value(value);
    } else {
        document.insert(preference.key(), Item::Value(value));
    }
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    let text = document.to_string();
    file.write_all(text.as_bytes())?;
    file.as_file().sync_all()?;
    if resolve(path)? != target
        || read_source(&target, &path.with_file_name("account.toml"))
            .map_err(|_| changed_while_saving())?
            != before
    {
        return Err(changed_while_saving());
    }
    file.persist(&target)?;
    Ok(Snapshot::parse(&document, defaults))
}

/// Watch the directory as well as the file's ancestors, since the first
/// settings save may also create the config directory.
pub fn watched_dirs(path: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![];
    for file in std::iter::once(path.to_path_buf()).chain(path.canonicalize().ok()) {
        for dir in file.ancestors().skip(1) {
            dirs.push(dir.to_path_buf());
            if dir.is_dir() {
                break;
            }
        }
    }
    if let Some(parent) = path.parent().and_then(Path::parent) {
        dirs.push(parent.to_path_buf());
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

pub fn appearance() -> Values {
    Snapshot::read(path().as_deref(), Values::default()).values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_config_and_settings_have_private_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("fell");
        let path = dir.join("settings.toml");
        let defaults = Values::default();
        save(&path, defaults, Edit::Cycle(Preference::FontSize, false)).unwrap();
        assert_eq!(fs::metadata(dir).unwrap().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn settings_symlinks_preserve_owned_dotfiles_and_refuse_account_files() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let owned = temp.path().join("preferences.toml");
        let config = temp.path().join("settings.toml");
        let account = temp.path().join("account.toml");
        fs::write(&account, "# Existing account\n").unwrap();
        let defaults = Values::default();
        symlink(&account, &config).unwrap();
        assert!(save(&config, defaults, Edit::Cycle(Preference::FontSize, false)).is_err());
        assert_eq!(
            fs::read_to_string(&account).unwrap(),
            "# Existing account\n"
        );

        let path = temp.path().join("fell/settings.toml");
        mailbox::create_mailbox_dirs(path.parent().unwrap()).unwrap();
        fs::write(&owned, "# Existing preferences\n").unwrap();
        symlink(&owned, &path).unwrap();
        save(&path, defaults, Edit::Cycle(Preference::FontSize, false)).unwrap();
        assert!(path.is_symlink());
        let updated = fs::read_to_string(owned).unwrap();
        assert!(updated.contains("# Existing preferences"));
        assert!(updated.contains("font_size = 12"));
    }

    #[test]
    fn completed_in_place_edits_can_shrink_or_empty_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.toml");
        let defaults = Values::default();
        let original = "# Reading preferences\nread_dwell_ms = 500\nnotifications = 'private'\n";
        for (edited, dwell, notifications) in [
            (
                "read_dwell_ms = 500\nnotifications = 'private'\n",
                500,
                NotificationMode::Private,
            ),
            (original.trim_end(), 500, NotificationMode::Private),
            (
                "# Reading preferences\nread_dwell_ms = 500\n",
                500,
                defaults.notifications,
            ),
            ("", defaults.read_dwell_ms, defaults.notifications),
        ] {
            fs::write(&path, original).unwrap();
            let previous = Snapshot::read(Some(&path), defaults);
            fs::write(&path, edited).unwrap();
            let next = previous.reload(Some(&path), defaults);
            assert_eq!(next.values.read_dwell_ms, dwell);
            assert_eq!(next.values.notifications, notifications);
            assert!(next.notice().is_none());

            let saved = save(&path, defaults, Edit::Cycle(Preference::FontSize, false)).unwrap();
            assert_eq!(saved.values.read_dwell_ms, dwell);
            assert_eq!(saved.values.notifications, notifications);
            assert_eq!(saved.values.font_size, Some(12));
        }
    }

    #[test]
    fn a_syntax_error_keeps_previous_values_and_a_valid_edit_recovers() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.toml");
        let defaults = Values::default();
        fs::write(&path, "read_dwell_ms = 500\n").unwrap();
        let previous = Snapshot::read(Some(&path), defaults);
        let broken = "# Reading preferences\nread_dwell_ms = [";
        fs::write(&path, broken).unwrap();

        let next = previous.reload(Some(&path), defaults);
        assert_eq!(next.values, previous.values);
        assert!(
            next.notice()
                .unwrap()
                .contains("TOML syntax error on line 2")
        );
        let error = save(&path, defaults, Edit::Cycle(Preference::FontSize, false)).unwrap_err();
        assert!(error.to_string().contains("TOML syntax error on line 2"));
        assert_eq!(fs::read_to_string(&path).unwrap(), broken);

        fs::write(&path, "read_dwell_ms=250").unwrap();
        let recovered = next.reload(Some(&path), defaults);
        assert_eq!(recovered.values.read_dwell_ms, 250);
        assert!(recovered.notice().is_none());
        assert!(save(&path, defaults, Edit::Cycle(Preference::FontSize, false)).is_ok());
    }

    #[test]
    fn edit_keeps_comments_order_and_unknown_settings_from_latest_disk_contents() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.toml");
        let original = "# quiet\nwhich_key_ms   =   300 # delay\nfuture = 'spelling'\n\n[other]\nchoice = [1, 2] # saved\n";
        fs::write(&path, original).unwrap();

        let snapshot = save(
            &path,
            Values::default(),
            Edit::Cycle(Preference::WhichKey, false),
        )
        .unwrap();

        assert_eq!(snapshot.values.which_key_ms, 500);
        assert_eq!(snapshot.unknown, ["future", "other"]);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            original.replace("300", "500")
        );
        assert_eq!(Snapshot::read(Some(&path), Values::default()), snapshot);
    }

    #[test]
    fn reload_applies_first_save_and_atomic_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("fell");
        let path = dir.join("settings.toml");
        let defaults = Values::default();
        let previous = Snapshot::read(Some(&path), defaults);

        save(&path, defaults, Edit::Cycle(Preference::ReadDwell, false)).unwrap();
        let previous = previous.reload(Some(&path), defaults);
        assert_eq!(previous.values.read_dwell_ms, 250);

        let mut edit = tempfile::NamedTempFile::new_in(&dir).unwrap();
        write!(edit, "read_dwell_ms = 1500\nnotifications = 'off'\n").unwrap();
        edit.persist(&path).unwrap();
        let reloaded = previous.reload(Some(&path), defaults);
        assert_eq!(reloaded.values.read_dwell_ms, 1500);
        assert_eq!(reloaded.values.notifications, NotificationMode::Off);
    }

    #[test]
    fn missing_or_bad_keys_keep_legacy_preferences_and_valid_keys_apply() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.toml");
        let legacy = Values {
            which_key_ms: 730,
            read_dwell_ms: 1200,
            reduce_motion: Some(true),
            notifications: NotificationMode::Private,
            ..Values::default()
        };
        assert_eq!(Snapshot::read(Some(&path), legacy).values, legacy);
        fs::write(&path, "which_key_ms = -1\nread_dwell_ms = 'now'\nnotifications = 0\nreduce_motion = 'system'\n").unwrap();
        let bad = Snapshot::read(Some(&path), legacy);
        assert_eq!(
            bad.values,
            Values {
                reduce_motion: None,
                ..legacy
            }
        );
        assert_eq!(
            bad.notice().unwrap(),
            "settings.toml contains invalid values · read_dwell_ms, which_key_ms, notifications"
        );

        fs::write(&path, "theme = 'dark'\nwhich_key_ms = [").unwrap();
        let broken = Snapshot::read(Some(&path), legacy);
        assert_eq!(broken.values, legacy);
        assert!(broken.notice().is_some());
        assert!(save(&path, legacy, Edit::Cycle(Preference::Theme, false)).is_err());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "theme = 'dark'\nwhich_key_ms = ["
        );
    }
}
