//! Omarchy palette mapping, live file load, and the built-in dark and light themes.
//!
//! Text and markers are adjusted until they meet WCAG AA against the mapped
//! surfaces. Stock Omarchy `colors.toml` files are checked in CI.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) const TEXT_CONTRAST: f64 = 4.5;
pub(crate) const MARK_CONTRAST: f64 = 3.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeKind {
    Omarchy,
    FellDark,
    FellLight,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub name: String,
    pub kind: ThemeKind,
    pub background: u32,
    pub surface: u32,
    pub foreground: u32,
    pub dimmed: u32,
    pub accent: u32,
    pub selection: u32,
    pub on_selection: u32,
    /// Snippet and meta on a selected row. Dimmer than `on_selection`, still AA on `selection`.
    pub on_selection_dim: u32,
    pub on_selection_accent: u32,
    pub search_match: (u32, u32),
    pub selected_search_match: (u32, u32),
    label_outline: u32,
    selected_label_outline: u32,
    pub error: u32,
    pub border: u32,
    pub input_border: u32,
    pub star: u32,
    pub ui_family: Option<String>,
    pub ui_px: f32,
    pub mono_family: Option<String>,
    pub mono_px: f32,
}

#[derive(Clone, Debug)]
struct RawPalette {
    mode: Option<String>,
    colors: HashMap<String, u32>,
}

#[derive(Clone, Debug)]
struct Fonts {
    ui_family: Option<String>,
    ui_px: f32,
    mono_family: Option<String>,
    mono_px: f32,
}

impl Theme {
    pub(crate) fn note_rule(&self) -> u32 {
        self.accent
    }

    pub fn is_light(&self) -> bool {
        luminance(self.background) > 0.4
    }

    pub(crate) fn label_outline(&self, background: u32) -> u32 {
        if background == self.background {
            self.label_outline
        } else if background == self.selection {
            self.selected_label_outline
        } else {
            label_outline(background, self.dimmed)
        }
    }
}

fn label_outline(background: u32, text: u32) -> u32 {
    push_toward(background, background, MARK_CONTRAST, text).map_or_else(
        || ensure(background, background, MARK_CONTRAST),
        |(color, _)| color,
    )
}

/// Read the active Omarchy theme, or the built-in theme that follows the desktop.
pub fn load() -> Theme {
    let fonts = load_fonts();
    let theme = theme_candidates().into_iter().find_map(|path| {
        let text = fs::read_to_string(&path).ok()?;
        theme_from_palette(&text, &fonts, palette_name(&path))
    });
    let theme = theme.unwrap_or_else(|| builtin(prefers_dark(), fonts));
    let _ = (contrast_ok(&theme), theme.is_light());
    theme
}

fn theme_from_palette(text: &str, fonts: &Fonts, name: String) -> Option<Theme> {
    let raw = parse_palette(text);
    let background = color(&raw, &["background"])?;
    let foreground = color(&raw, &["foreground", "color7"])?;
    Some(map_palette(
        name,
        ThemeKind::Omarchy,
        &raw,
        background,
        foreground,
        fonts,
    ))
}

fn builtin(dark: bool, fonts: Fonts) -> Theme {
    if dark {
        let raw = RawPalette {
            mode: Some("dark".into()),
            colors: pairs(&[
                ("background", 0x1c1b19),
                ("foreground", 0xe8e4dc),
                ("accent", 0x3d9a86),
                ("selection", 0x2c333b),
                ("dark_foreground", 0xa39e94),
                ("red", 0xe06c75),
                ("yellow", 0xe0b15a),
            ]),
        };
        map_palette(
            "Fell dark".into(),
            ThemeKind::FellDark,
            &raw,
            0x1c1b19,
            0xe8e4dc,
            &fonts,
        )
    } else {
        let raw = RawPalette {
            mode: Some("light".into()),
            colors: pairs(&[
                ("background", 0xf6f4ef),
                ("foreground", 0x1f1c18),
                ("accent", 0x0f6e62),
                ("selection", 0xe4dfd4),
                ("dark_foreground", 0x5c564e),
                ("red", 0xb42318),
                ("yellow", 0x8a5a00),
            ]),
        };
        map_palette(
            "Fell light".into(),
            ThemeKind::FellLight,
            &raw,
            0xf6f4ef,
            0x1f1c18,
            &fonts,
        )
    }
}

fn map_palette(
    name: String,
    kind: ThemeKind,
    raw: &RawPalette,
    background: u32,
    foreground_raw: u32,
    fonts: &Fonts,
) -> Theme {
    let _mode = raw.mode.as_deref();
    let dim_raw = color(raw, &["dark_foreground", "muted", "color8"])
        .unwrap_or_else(|| mix(foreground_raw, background, 0.55));
    let (foreground, dimmed) = text_pair(foreground_raw, dim_raw, background);
    let surface = mix(background, foreground, 0.045);
    let (foreground, dimmed) = text_pair(foreground, dimmed, surface);
    let accent_raw = color(raw, &["accent", "blue", "color4", "color12"]).unwrap_or(foreground);
    let error_raw = color(raw, &["red", "bright_red", "color1"]).unwrap_or(0xcc3333);
    let star_raw = color(raw, &["yellow", "bright_yellow", "color3"]).unwrap_or(accent_raw);
    // The row highlight is the theme selection, or the theme surface when a
    // palette has no selection key. A light wash stays light. Text on it is
    // adjusted to AA instead of turning the wash into a mid-gray band.
    let selection_raw = color(raw, &["selection", "selection_background"])
        .or_else(|| color(raw, &["lighter_background"]))
        .unwrap_or(surface);
    let selection = usable_fill(selection_raw, background);
    let on_selection_raw = color(raw, &["selection_foreground"]).unwrap_or(foreground);
    let (on_selection, on_selection_dim) = text_pair(on_selection_raw, on_selection_raw, selection);
    let accent = ensure_all(accent_raw, &[background, surface], TEXT_CONTRAST);
    let on_selection_accent = if contrast(accent, selection) >= TEXT_CONTRAST {
        accent
    } else {
        push_toward(accent, selection, TEXT_CONTRAST, on_selection)
            .map_or(on_selection, |(color, _)| color)
    };
    let search_match = |fg, bg| {
        let fill = mix(bg, fg, 0.14);
        (ensure(fg, fill, TEXT_CONTRAST), fill)
    };
    let border = mix(background, foreground, 0.16);
    Theme {
        name,
        kind,
        background,
        surface,
        foreground,
        dimmed,
        accent,
        selection,
        on_selection,
        on_selection_dim,
        on_selection_accent,
        search_match: search_match(accent, background),
        selected_search_match: search_match(on_selection, selection),
        label_outline: label_outline(background, dimmed),
        selected_label_outline: label_outline(selection, on_selection),
        // A warning chip sits on a selected row too, so the error colour
        // holds AA on the selection as well as on the page.
        error: ensure_all(error_raw, &[background, surface, selection], TEXT_CONTRAST),
        border,
        input_border: ensure_all(
            mix(border, foreground, 0.35),
            &[background, surface],
            MARK_CONTRAST,
        ),
        // The star shows on the selected row too, so it meets AA there.
        star: ensure_all(star_raw, &[background, surface, selection], TEXT_CONTRAST),
        ui_family: fonts.ui_family.clone(),
        ui_px: fonts.ui_px,
        mono_family: fonts.mono_family.clone(),
        mono_px: fonts.mono_px,
    }
}

/// Every stock Omarchy theme in `stock-themes/`, plus the two built-in
/// themes, for contrast tests elsewhere in the crate.
#[cfg(test)]
pub(crate) fn every_test_theme() -> Vec<Theme> {
    let fonts = Fonts {
        ui_family: None,
        ui_px: 15.0,
        mono_family: None,
        mono_px: 13.0,
    };
    let mut themes = vec![builtin(true, fonts.clone()), builtin(false, fonts.clone())];
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stock-themes");
    for entry in fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap();
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        themes.push(theme_from_palette(&text, &fonts, name).unwrap());
    }
    themes
}

/// The border for marks a reader follows, like a quote's rule: the theme's
/// border pushed toward the text until it reaches 3:1 non-text contrast on
/// the background and the surface.
pub fn strong_border(theme: &Theme) -> u32 {
    let mut t = 0.16;
    loop {
        let color = mix(theme.background, theme.foreground, t);
        let enough = [theme.background, theme.surface]
            .iter()
            .all(|bg| contrast(color, *bg) >= MARK_CONTRAST);
        if enough || t >= 1.0 {
            return color;
        }
        t = (t + 0.02).min(1.0);
    }
}

const NOTE_TEXT_CONTRAST: f64 = 4.7;

pub(crate) fn note_text(color: u32, background: u32) -> u32 {
    ensure(color, background, NOTE_TEXT_CONTRAST)
}

pub fn contrast_ok(theme: &Theme) -> bool {
    let surfaces = [theme.background, theme.surface];
    for bg in surfaces {
        if contrast(theme.foreground, bg) < TEXT_CONTRAST
            || contrast(theme.dimmed, bg) < TEXT_CONTRAST
            || contrast(theme.accent, bg) < TEXT_CONTRAST
            || contrast(theme.error, bg) < TEXT_CONTRAST
            || contrast(theme.star, bg) < TEXT_CONTRAST
        {
            return false;
        }
    }
    [theme.search_match, theme.selected_search_match]
        .into_iter()
        .all(|(fg, fill)| contrast(fg, fill) >= TEXT_CONTRAST)
        && contrast(theme.on_selection, theme.selection) >= TEXT_CONTRAST
        && contrast(theme.star, theme.selection) >= TEXT_CONTRAST
        && contrast(theme.on_selection_dim, theme.selection) >= TEXT_CONTRAST
        && contrast(theme.on_selection_accent, theme.selection) >= TEXT_CONTRAST
        && contrast(theme.error, theme.selection) >= TEXT_CONTRAST
        && contrast(theme.foreground, theme.background)
            >= contrast(theme.dimmed, theme.background) + 0.8
        && contrast(theme.on_selection, theme.selection)
            >= contrast(theme.on_selection_dim, theme.selection) + 0.8
        && contrast(theme.note_rule(), theme.background) >= MARK_CONTRAST
        && [theme.dimmed, theme.accent].into_iter().all(|color| {
            contrast(note_text(color, theme.background), theme.background) >= NOTE_TEXT_CONTRAST
        })
        && contrast(strong_border(theme), theme.background) >= MARK_CONTRAST
}

/// A thread watching the Omarchy theme, GTK settings, and Alacritty config.
/// It sends `()` (at most every 100 ms) when one of them may have changed,
/// and ends when the receiver is gone. `false` when inotify is unavailable,
/// so the caller polls instead.
pub fn watch(changed: futures::channel::mpsc::UnboundedSender<()>) -> bool {
    watch_dirs(watched_dirs(), changed)
}

pub fn follow<V: 'static>(
    cx: &mut gpui::Context<V>,
    mut update: impl FnMut(&mut V, Theme, &mut gpui::Context<V>) + 'static,
) {
    use futures::StreamExt as _;

    let (changed, mut changes) = futures::channel::mpsc::unbounded();
    let watching = watch(changed);
    cx.spawn(async move |this, cx| {
        loop {
            if watching {
                if changes.next().await.is_none() {
                    break;
                }
            } else {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
            }
            let next = cx.background_executor().spawn(async { load() }).await;
            if this.update(cx, |view, cx| update(view, next, cx)).is_err() {
                break;
            }
        }
    })
    .detach();
}

/// Each folder holding a file `load` reads, and that folder's parent, so a
/// replaced folder (Omarchy rewrites `current/theme`) is noticed and
/// watched again.
fn watched_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for file in theme_candidates()
        .into_iter()
        .chain(gtk_settings_paths())
        .chain(alacritty_paths())
    {
        let Some(dir) = file.parent() else {
            continue;
        };
        dirs.push(dir.to_path_buf());
        if let Some(parent) = dir.parent() {
            dirs.push(parent.to_path_buf());
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

fn watch_dirs(dirs: Vec<PathBuf>, changed: futures::channel::mpsc::UnboundedSender<()>) -> bool {
    use notify::{RecursiveMode, Watcher};
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let Ok(mut watcher) =
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if event.is_ok_and(|event| changes_content(&event.kind)) {
                let _ = tx.send(());
            }
        })
    else {
        return false;
    };
    let rewatch = move |watcher: &mut notify::RecommendedWatcher| {
        let mut watched = 0;
        for dir in &dirs {
            if dir.is_dir() && watcher.watch(dir, RecursiveMode::NonRecursive).is_ok() {
                watched += 1;
            }
        }
        watched
    };
    // Out of inotify watches (or nothing to watch): let the caller poll.
    if rewatch(&mut watcher) == 0 {
        return false;
    }
    std::thread::Builder::new()
        .name("fell-theme-watch".into())
        .spawn(move || {
            loop {
                match rx.recv_timeout(std::time::Duration::from_secs(1)) {
                    Ok(()) => {}
                    // Nothing changed. Stop once the window is gone.
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if changed.is_closed() {
                            break;
                        }
                        continue;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
                // A theme switch writes several files. Settle, then reload once.
                std::thread::sleep(std::time::Duration::from_millis(100));
                while rx.try_recv().is_ok() {}
                rewatch(&mut watcher);
                if changed.unbounded_send(()).is_err() {
                    break;
                }
            }
        })
        .is_ok()
}

/// Events that can change what `load` reads. Opens, reads, and metadata
/// changes are left out: `load` itself opens these files, so counting them
/// would wake the watcher again after every reload.
fn changes_content(kind: &notify::EventKind) -> bool {
    use notify::EventKind;
    use notify::event::ModifyKind;
    !matches!(
        kind,
        EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(_))
    )
}

fn theme_candidates() -> Vec<PathBuf> {
    if let Ok(raw) = env::var("FELL_OMARCHY_THEME") {
        let path = PathBuf::from(raw);
        if path.is_dir() {
            return vec![path.join("colors.toml")];
        }
        return vec![path];
    }
    let home = home_dir();
    vec![
        home.join(".local/state/omarchy/current/theme/colors.toml"),
        home.join(".config/omarchy/current/theme/colors.toml"),
    ]
}

fn palette_name(path: &Path) -> String {
    path.parent()
        .and_then(|dir| dir.file_name())
        .and_then(|name| name.to_str())
        .filter(|name| *name != "theme")
        .unwrap_or("Omarchy")
        .to_string()
}

fn load_fonts() -> Fonts {
    let mut fonts = Fonts {
        // A face with a real bold cut, so unread rows are heavier than read ones
        // even when Omarchy has not named a UI font.
        ui_family: Some("DejaVu Sans".into()),
        ui_px: 15.0,
        mono_family: Some("DejaVu Sans Mono".into()),
        mono_px: 13.0,
    };
    for path in gtk_settings_paths() {
        if let Ok(text) = fs::read_to_string(&path)
            && let Some((family, px)) = gtk_font(&text)
        {
            fonts.ui_family = Some(family);
            fonts.ui_px = px;
            break;
        }
    }
    for path in alacritty_paths() {
        if let Ok(text) = fs::read_to_string(&path) {
            let (family, size) = alacritty_font(&text);
            if fonts.mono_family.is_none() {
                fonts.mono_family = family;
            }
            if let Some(size) = size {
                fonts.mono_px = size;
            }
            break;
        }
    }
    if let Ok(family) = env::var("FELL_UI_FONT")
        && !family.is_empty()
    {
        fonts.ui_family = Some(family);
    }
    if let Ok(px) = env::var("FELL_UI_FONT_PX")
        && let Ok(px) = px.parse::<f32>()
    {
        fonts.ui_px = px;
    }
    if let Ok(family) = env::var("FELL_MONO_FONT")
        && !family.is_empty()
    {
        fonts.mono_family = Some(family);
    }
    if let Ok(px) = env::var("FELL_MONO_FONT_PX")
        && let Ok(px) = px.parse::<f32>()
    {
        fonts.mono_px = px;
    }
    fonts
}

fn gtk_settings_paths() -> Vec<PathBuf> {
    let home = home_dir();
    let config = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    vec![
        config.join("gtk-3.0/settings.ini"),
        config.join("gtk-4.0/settings.ini"),
    ]
}

fn alacritty_paths() -> Vec<PathBuf> {
    let home = home_dir();
    let config = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    vec![
        config.join("alacritty/alacritty.toml"),
        home.join(".config/alacritty/alacritty.toml"),
    ]
}

fn gtk_font(text: &str) -> Option<(String, f32)> {
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("gtk-font-name") else {
            continue;
        };
        let value = rest.trim().trim_start_matches('=').trim();
        if value.is_empty() {
            continue;
        }
        let mut parts: Vec<&str> = value.split_whitespace().collect();
        let size = parts
            .last()
            .and_then(|token| token.parse::<f32>().ok())
            .unwrap_or(11.0);
        if parts
            .last()
            .and_then(|token| token.parse::<f32>().ok())
            .is_some()
        {
            parts.pop();
        }
        let family = parts.join(" ");
        if family.is_empty() {
            continue;
        }
        let px = (size * 96.0 / 72.0).clamp(11.0, 22.0);
        return Some((family, px));
    }
    None
}

fn alacritty_font(text: &str) -> (Option<String>, Option<f32>) {
    let mut in_font = false;
    let mut family = None;
    let mut size = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_font = trimmed.starts_with("[font");
            continue;
        }
        if !in_font {
            continue;
        }
        if family.is_none()
            && let Some(value) = value_of(trimmed, "family")
        {
            family = Some(value.trim_matches('"').trim_matches('\'').to_string());
        }
        if size.is_none()
            && let Some(value) = value_of(trimmed, "size")
        {
            size = value.parse().ok();
        }
    }
    (family, size)
}

fn prefers_dark() -> bool {
    if let Ok(value) = env::var("FELL_COLOR_SCHEME") {
        return !value.eq_ignore_ascii_case("light");
    }
    for path in gtk_settings_paths() {
        if let Ok(text) = fs::read_to_string(&path) {
            for line in text.lines() {
                let line = line.trim().replace(' ', "");
                if let Some(value) = line.strip_prefix("gtk-application-prefer-dark-theme=") {
                    return value != "0" && !value.eq_ignore_ascii_case("false");
                }
            }
        }
    }
    true
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn parse_palette(text: &str) -> RawPalette {
    let mut colors = HashMap::new();
    let mut mode = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = strip_inline_comment(value.trim())
            .trim()
            .trim_matches('"')
            .trim_matches('\'');
        if key == "mode" {
            mode = Some(value.to_string());
            continue;
        }
        if let Some(hex) = parse_hex(value) {
            colors.insert(key, hex);
        }
    }
    RawPalette { mode, colors }
}

fn strip_inline_comment(value: &str) -> &str {
    value.split(" #").next().unwrap_or(value)
}

fn parse_hex(value: &str) -> Option<u32> {
    let hex = value.strip_prefix('#')?;
    match hex.len() {
        6 => u32::from_str_radix(hex, 16).ok(),
        3 => {
            let chars: Vec<char> = hex.chars().collect();
            let expanded: String = chars.iter().flat_map(|ch| [*ch, *ch]).collect();
            u32::from_str_radix(&expanded, 16).ok()
        }
        _ => None,
    }
}

fn color(raw: &RawPalette, keys: &[&str]) -> Option<u32> {
    keys.iter().find_map(|key| raw.colors.get(*key).copied())
}

fn pairs(items: &[(&str, u32)]) -> HashMap<String, u32> {
    items
        .iter()
        .map(|(key, value)| ((*key).to_string(), *value))
        .collect()
}

fn value_of<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let (name, value) = line.split_once('=')?;
    if name.trim() == key {
        Some(value.trim())
    } else {
        None
    }
}

fn channel(c: f64) -> f64 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn luminance(rgb: u32) -> f64 {
    let r = ((rgb >> 16) & 255) as f64 / 255.0;
    let g = ((rgb >> 8) & 255) as f64 / 255.0;
    let b = (rgb & 255) as f64 / 255.0;
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

pub fn contrast(a: u32, b: u32) -> f64 {
    let l1 = luminance(a);
    let l2 = luminance(b);
    let (hi, lo) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
    (hi + 0.05) / (lo + 0.05)
}

pub(crate) fn mix(a: u32, b: u32, t: f64) -> u32 {
    fn mix_channel(a: u32, b: u32, shift: u32, t: f64) -> u32 {
        let av = (a >> shift) & 255;
        let bv = (b >> shift) & 255;
        let mixed = (av as f64 * (1.0 - t) + bv as f64 * t)
            .round()
            .clamp(0.0, 255.0) as u32;
        mixed << shift
    }
    mix_channel(a, b, 16, t) | mix_channel(a, b, 8, t) | mix_channel(a, b, 0, t)
}

fn ensure(fg: u32, bg: u32, min: f64) -> u32 {
    if contrast(fg, bg) >= min {
        return fg;
    }
    let dark = push_toward(fg, bg, min, 0x000000);
    let light = push_toward(fg, bg, min, 0x00ffffff);
    match (dark, light) {
        (Some((color, t)), Some((other, other_t))) => {
            if t <= other_t {
                color
            } else {
                other
            }
        }
        (Some((color, _)), None) => color,
        (None, Some((color, _))) => color,
        (None, None) => {
            if contrast(0x000000, bg) >= contrast(0x00ffffff, bg) {
                0x000000
            } else {
                0x00ffffff
            }
        }
    }
}

fn push_toward(fg: u32, bg: u32, min: f64, toward: u32) -> Option<(u32, f64)> {
    if contrast(toward, bg) < min {
        return None;
    }
    let mut lo = 0.0;
    let mut hi = 1.0;
    let mut best = toward;
    for _ in 0..24 {
        let mid = (lo + hi) / 2.0;
        let mixed = mix(fg, toward, mid);
        if contrast(mixed, bg) >= min {
            best = mixed;
            hi = mid;
        } else {
            lo = mid;
        }
    }
    if contrast(best, bg) >= min {
        Some((best, hi))
    } else {
        Some((toward, 1.0))
    }
}

pub(crate) fn ensure_all(fg: u32, backgrounds: &[u32], min: f64) -> u32 {
    let mut color = fg;
    for _ in 0..3 {
        let mut next = color;
        for bg in backgrounds {
            next = ensure(next, *bg, min);
        }
        if next == color {
            break;
        }
        color = next;
    }
    color
}

/// Keep `raw` when black or white text can meet AA on it. Otherwise walk it
/// toward `page` until one of those can, so a light selection stays light.
fn usable_fill(raw: u32, page: u32) -> u32 {
    if can_hold_text(raw) {
        return raw;
    }
    let mut lo = 0.0;
    let mut hi = 1.0;
    let mut best = page;
    for _ in 0..24 {
        let mid = (lo + hi) / 2.0;
        let mixed = mix(raw, page, mid);
        if can_hold_text(mixed) {
            best = mixed;
            hi = mid;
        } else {
            lo = mid;
        }
    }
    if can_hold_text(best) { best } else { page }
}

fn can_hold_text(fill: u32) -> bool {
    contrast(0x000000, fill) >= TEXT_CONTRAST || contrast(0xffffff, fill) >= TEXT_CONTRAST
}

/// Primary text and a dimmer secondary, both at least AA on `surface`.
/// The secondary starts from the theme's muted color and fades toward the
/// surface until it is clearly quieter than the primary.
fn text_pair(primary_raw: u32, dim_raw: u32, surface: u32) -> (u32, u32) {
    let mut primary = ensure(primary_raw, surface, TEXT_CONTRAST);
    let mut dim = fade_toward(dim_raw, surface);
    for _ in 0..10 {
        if contrast(primary, surface) >= contrast(dim, surface) + 0.8
            && contrast(primary, dim) >= 1.2
        {
            break;
        }
        let away = if luminance(surface) > 0.45 {
            0x000000
        } else {
            0xffffff
        };
        primary = ensure(mix(primary, away, 0.22), surface, TEXT_CONTRAST);
        dim = fade_toward(mix(dim, surface, 0.35), surface);
    }
    (primary, dim)
}

/// The quietest color on `surface` that still meets AA, starting from `color`.
fn fade_toward(color: u32, surface: u32) -> u32 {
    let mut lo = 0.0;
    let mut hi = 1.0;
    let mut best = ensure(color, surface, TEXT_CONTRAST);
    for _ in 0..24 {
        let mid = (lo + hi) / 2.0;
        let mixed = mix(color, surface, mid);
        if contrast(mixed, surface) >= TEXT_CONTRAST {
            best = mixed;
            lo = mid;
        } else {
            hi = mid;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_themes_meet_aa() {
        let fonts = Fonts {
            ui_family: None,
            ui_px: 15.0,
            mono_family: None,
            mono_px: 13.0,
        };
        assert!(contrast_ok(&builtin(true, fonts.clone())));
        assert!(contrast_ok(&builtin(false, fonts)));
    }

    #[test]
    fn stock_omarchy_themes_meet_aa() {
        let fonts = Fonts {
            ui_family: None,
            ui_px: 15.0,
            mono_family: None,
            mono_px: 13.0,
        };
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stock-themes");
        let mut count = 0;
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
                continue;
            }
            let text = fs::read_to_string(&path).unwrap();
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            let theme = theme_from_palette(&text, &fonts, name.clone())
                .unwrap_or_else(|| panic!("palette {name}"));
            assert!(
                contrast_ok(&theme),
                "{name} fg {:06x}/{:.2} dim {:06x}/{:.2}/{:.2} accent {:06x}/{:.2} sel {:06x}/{:.2} on {:06x}/{:.2} err {:06x}/{:.2}/sel {:.2} star {:06x}/{:.2}/sel {:.2} bg {:06x} surface {:06x}",
                theme.foreground,
                contrast(theme.foreground, theme.background),
                theme.dimmed,
                contrast(theme.dimmed, theme.background),
                contrast(theme.dimmed, theme.surface),
                theme.accent,
                contrast(theme.accent, theme.background),
                theme.selection,
                contrast(theme.selection, theme.background),
                theme.on_selection,
                contrast(theme.on_selection, theme.selection),
                theme.error,
                contrast(theme.error, theme.background),
                contrast(theme.error, theme.selection),
                theme.star,
                contrast(theme.star, theme.background),
                contrast(theme.star, theme.selection),
                theme.background,
                theme.surface,
            );
            count += 1;
        }
        assert!(count >= 20, "expected the stock Omarchy set, got {count}");
    }

    #[test]
    fn latte_selection_stays_light_and_text_meets_aa() {
        let fonts = Fonts {
            ui_family: None,
            ui_px: 15.0,
            mono_family: None,
            mono_px: 13.0,
        };
        let text = fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stock-themes/catppuccin-latte.toml"),
        )
        .unwrap();
        let theme = theme_from_palette(&text, &fonts, "catppuccin-latte".into()).unwrap();
        assert!(contrast_ok(&theme));
        assert!(
            luminance(theme.selection) > 0.45,
            "selection {:06x} collapsed into a mid-gray",
            theme.selection
        );
        assert!(contrast(theme.on_selection, theme.selection) >= TEXT_CONTRAST);
        assert!(contrast(theme.on_selection_dim, theme.selection) >= TEXT_CONTRAST);
        assert!(contrast(theme.accent, theme.selection) < TEXT_CONTRAST);
        assert!(contrast(theme.on_selection_accent, theme.selection) >= TEXT_CONTRAST);
        assert_ne!(theme.on_selection_accent, theme.on_selection);
        // The star on the selected row (was #945e13 on #ccd0da, 3.5:1).
        assert!(
            contrast(theme.star, theme.selection) >= TEXT_CONTRAST,
            "star {:06x} on selection {:06x}: {:.2}",
            theme.star,
            theme.selection,
            contrast(theme.star, theme.selection)
        );
        assert!(luminance(theme.dimmed) > luminance(theme.foreground));
        assert!(luminance(theme.on_selection_dim) > luminance(theme.on_selection));
    }

    #[test]
    fn dim_text_on_white_is_lifted() {
        let fonts = Fonts {
            ui_family: None,
            ui_px: 15.0,
            mono_family: None,
            mono_px: 13.0,
        };
        let text = "mode = \"light\"\nbackground = \"#ffffff\"\nforeground = \"#000000\"\naccent = \"#666666\"\ndark_foreground = \"#c0c0c0\"\nselection = \"#c0c0c0\"\nred = \"#dddddd\"\nyellow = \"#eeeeee\"\n";
        let theme = theme_from_palette(text, &fonts, "white".into()).unwrap();
        assert!(contrast(theme.dimmed, theme.background) >= TEXT_CONTRAST);
        assert!(contrast(theme.on_selection, theme.selection) >= TEXT_CONTRAST);
        assert!(contrast_ok(&theme));
        assert!(theme.is_light());
    }

    #[test]
    fn gtk_font_size_is_respected() {
        let (family, px) = gtk_font("gtk-font-name = Adwaita Sans 11\n").unwrap();
        assert_eq!(family, "Adwaita Sans");
        assert!((px - 14.666).abs() < 0.1);
    }

    #[test]
    fn live_mapping_changes_when_the_palette_changes() {
        let fonts = Fonts {
            ui_family: None,
            ui_px: 15.0,
            mono_family: None,
            mono_px: 13.0,
        };
        let dark = theme_from_palette(
            "background = \"#1a1b26\"\nforeground = \"#a9b1d6\"\naccent = \"#7aa2f7\"\n",
            &fonts,
            "tokyo-night".into(),
        )
        .unwrap();
        let light = theme_from_palette(
            "mode = \"light\"\nbackground = \"#eff1f5\"\nforeground = \"#4c4f69\"\naccent = \"#1e66f5\"\n",
            &fonts,
            "catppuccin-latte".into(),
        )
        .unwrap();
        assert_ne!(dark.background, light.background);
        assert!(!dark.is_light());
        assert!(light.is_light());
    }

    #[test]
    fn the_watch_sees_a_rewritten_and_a_replaced_theme_folder() {
        let root = std::env::temp_dir().join(format!(
            "fell-theme-watch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let theme = root.join("current/theme");
        fs::create_dir_all(&theme).unwrap();
        fs::write(theme.join("colors.toml"), "background = \"#000000\"\n").unwrap();
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        assert!(watch_dirs(vec![theme.clone(), root.join("current")], tx));
        let next = |rx: &mut futures::channel::mpsc::UnboundedReceiver<()>| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if rx.try_recv().is_ok() {
                    return true;
                }
                if std::time::Instant::now() > deadline {
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        fs::write(theme.join("colors.toml"), "background = \"#111111\"\n").unwrap();
        assert!(next(&mut rx), "a rewritten colors.toml");
        // Replace the folder the way a theme switch can.
        let staged = root.join("current/theme.new");
        fs::create_dir_all(&staged).unwrap();
        fs::write(staged.join("colors.toml"), "background = \"#222222\"\n").unwrap();
        fs::remove_dir_all(&theme).unwrap();
        fs::rename(&staged, &theme).unwrap();
        assert!(next(&mut rx), "a replaced theme folder");
        std::thread::sleep(std::time::Duration::from_millis(300));
        while rx.try_recv().is_ok() {}
        fs::write(theme.join("colors.toml"), "background = \"#333333\"\n").unwrap();
        assert!(next(&mut rx), "the new folder is watched too");
        // Reading the files, as a reload does, must not signal again.
        std::thread::sleep(std::time::Duration::from_millis(300));
        while rx.try_recv().is_ok() {}
        for _ in 0..5 {
            let _ = fs::read_to_string(theme.join("colors.toml"));
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert!(
            rx.try_recv().is_err(),
            "a reload's own reads woke the watcher"
        );
        drop(rx);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn onboarding_controls_meet_contrast_requirements() {
        for theme in every_test_theme() {
            let button = contrast(theme.accent, theme.background);
            let border_on_page = contrast(theme.input_border, theme.background);
            let border_on_input = contrast(theme.input_border, theme.surface);
            assert!(button >= TEXT_CONTRAST, "{}", theme.name);
            assert!(border_on_page >= MARK_CONTRAST, "{}", theme.name);
            assert!(border_on_input >= MARK_CONTRAST, "{}", theme.name);
            println!(
                "{}: primary {button:.2}, input against page {border_on_page:.2}, input fill {border_on_input:.2}",
                theme.name
            );
        }
    }

    #[test]
    fn strong_border_reaches_mark_contrast_on_every_stock_theme() {
        for theme in every_test_theme() {
            let border = strong_border(&theme);
            for bg in [theme.background, theme.surface] {
                assert!(
                    contrast(border, bg) >= MARK_CONTRAST,
                    "{}: {:.2}",
                    theme.name,
                    contrast(border, bg)
                );
            }
            // Still a rule, not text: quieter than the dimmed text.
            assert!(contrast(border, theme.background) <= contrast(theme.dimmed, theme.background));
        }
    }

    #[test]
    fn label_outlines_reach_mark_contrast_on_every_stock_theme() {
        for theme in every_test_theme() {
            let plain = contrast(theme.label_outline(theme.background), theme.background);
            let selected = contrast(theme.label_outline(theme.selection), theme.selection);
            assert!(
                plain >= MARK_CONTRAST && selected >= MARK_CONTRAST,
                "{}: {plain:.3} and {selected:.3}",
                theme.name
            );
            if matches!(theme.name.as_str(), "tokyo-night" | "catppuccin-latte") {
                eprintln!(
                    "{}: {plain:.3} unselected, {selected:.3} selected",
                    theme.name
                );
            }
        }
    }
}
