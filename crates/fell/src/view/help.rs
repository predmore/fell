//! The key reference scrolls to whole rows, even after wheel input.
use std::time::Instant;

use gpui::prelude::*;
use gpui::{Context, Div, FontWeight, MouseButton, Pixels, Size, Window, div, px, rgb};

use super::{CARD_TOP_PX, FontOpt, Inbox, conversation_line_style};
use crate::keymap::{ACTION_SECTIONS, Action, KeyId, Keymap, binding_text};
use crate::model::{Overlay, Session};
use crate::motion::{self, Glide, Motion};
use crate::theme::Theme;

const MAX_WIDTH: f32 = 520.;
const MAX_HEIGHT: f32 = 460.;
const INSET: f32 = 8.;
const LEFT: f32 = 14.;
const RIGHT: f32 = 24.;
const DETAIL_GAP: f32 = 8.;
const STACKED_KEY_INSET: f32 = 12.;

#[derive(Default)]
pub(super) struct HelpScroll {
    page: usize,
    last: usize,
    row: f32,
    detail_rows: usize,
    detail_gap: f32,
    target: usize,
    drawn: f32,
    glide: Option<Glide>,
    wheel_remainder: f32,
}

impl HelpScroll {
    /// An open and a scroll key can arrive before the first paint.
    pub(super) fn prepare(&mut self, now: Instant, measure: impl FnOnce() -> Layout) {
        if self.page == 0 {
            self.advance(&measure(), now);
        }
    }

    pub(super) fn on_key(&mut self, key: KeyId, motion: &Motion, now: Instant) -> bool {
        let half_page = (self.page / 2).max(1) as isize;
        let rows = match key {
            KeyId::Char('j') | KeyId::Down => 1,
            KeyId::Char('k') | KeyId::Up => -1,
            KeyId::Ctrl('d') => half_page,
            KeyId::Ctrl('u') => -half_page,
            _ => return false,
        };
        self.wheel_remainder = 0.;
        self.scroll(rows, motion, now);
        true
    }

    fn wheel(&mut self, rows: f32, motion: &Motion, now: Instant) {
        self.wheel_remainder += rows;
        let whole = self.wheel_remainder.trunc();
        self.wheel_remainder -= whole;
        let whole = whole as isize;
        let before = self.target;
        self.scroll(whole, motion, now);
        let applied = self.target as isize - before as isize;
        if applied != whole {
            self.wheel_remainder = 0.;
        }
    }

    fn scroll(&mut self, rows: isize, motion: &Motion, now: Instant) {
        let target = self.target.saturating_add_signed(rows).min(self.last);
        if target == self.target {
            return;
        }
        self.target = target;
        // Repeated input uses the latest destination and starts from what was
        // actually drawn, including when several keys land between frames.
        self.glide = Glide::start(
            motion,
            self.drawn,
            scroll_px(target, self.row, self.detail_rows, self.detail_gap),
            0.,
            motion::SLIDE,
            now,
        )
        .map(|glide| glide.gentle_beyond(3. * self.row));
    }

    pub(super) fn running(&self, now: Instant) -> bool {
        self.glide.is_some_and(|glide| glide.running(now))
    }

    fn advance(&mut self, layout: &Layout, now: Instant) {
        self.page = layout.page;
        self.last = layout.total - layout.page;
        if self.target > self.last
            || self.row != layout.row
            || self.detail_rows != layout.detail_rows
            || self.detail_gap != layout.detail_gap
        {
            self.target = self.target.min(self.last);
            self.glide = None;
        }
        self.row = layout.row;
        self.detail_rows = layout.detail_rows;
        self.detail_gap = layout.detail_gap;
        if !self.running(now) {
            self.glide = None;
        }
        let settled = scroll_px(self.target, self.row, self.detail_rows, self.detail_gap);
        self.drawn = self
            .glide
            .map_or(settled, |glide| glide.value(now))
            .min(layout.last_offset());
    }

    pub(super) fn card(
        &mut self,
        theme: &Theme,
        session: &Session,
        window: &Window,
        now: Instant,
        live: bool,
        cx: &mut Context<Inbox>,
    ) -> Option<(f32, Div)> {
        if session.overlay != Overlay::Help {
            return None;
        }
        let layout = Layout::for_window(theme, session, window);
        if live {
            self.advance(&layout, now);
        }
        let moving = self.glide.is_some();
        let drawn = if moving {
            motion::snap(self.drawn, window.scale_factor())
        } else {
            self.drawn
        };
        let offset = drawn.min(layout.last_offset());
        Some((layout.top, card(theme, layout, offset, moving, live, cx)))
    }
}

fn scroll_px(target: usize, row: f32, detail_rows: usize, detail_gap: f32) -> f32 {
    let y = target as f32 * row;
    if target >= detail_rows {
        y + detail_gap
    } else {
        y
    }
}

const SUBJECT_JOIN_HELP: &str = "Joined by subject · Fell matched the subject and participants because reply headers were missing. Space c s splits it off.";

pub(super) struct Layout {
    top: f32,
    width: f32,
    text_width: f32,
    header: f32,
    row: f32,
    line: f32,
    stacked: bool,
    details: Vec<(String, bool, usize)>,
    detail_rows: usize,
    detail_gap: f32,
    rows: Vec<HelpRow>,
    page: usize,
    total: usize,
}

impl Layout {
    /// The clip grows by the banner's gap only as far as the gap is in view,
    /// entering at the bottom or leaving at the top, so every resting offset
    /// fills the clip with whole lines. The card keeps its height, and the
    /// spare space becomes bottom padding.
    fn clip_height(&self, offset: f32) -> f32 {
        let page = self.page as f32 * self.row;
        let gap_top = self.detail_rows as f32 * self.row;
        let entered = (offset + page - gap_top).clamp(0., self.detail_gap);
        let remaining = (gap_top + self.detail_gap - offset).clamp(0., self.detail_gap);
        page + entered.min(remaining)
    }

    fn last_offset(&self) -> f32 {
        scroll_px(
            self.total - self.page,
            self.row,
            self.detail_rows,
            self.detail_gap,
        )
    }

    pub(super) fn for_window(theme: &Theme, session: &Session, window: &Window) -> Self {
        let error = session.detail_text().map(|text| (text, true));
        let subject_join = session
            .messages
            .iter()
            .any(|message| message.joined_by_subject && !message.merged_by_you)
            && super::present::visible_panes(
                f32::from(window.viewport_size().width),
                session.pane,
                session.full_width,
            )
            .1;
        Self::measure(
            theme.ui_px,
            window.viewport_size(),
            &session.keymap,
            error
                .into_iter()
                .chain(subject_join.then(|| (SUBJECT_JOIN_HELP.into(), false))),
            |error, width| {
                let style = conversation_line_style(window, theme, px(theme.ui_px));
                window
                    .text_system()
                    .shape_text(
                        error.to_owned().into(),
                        px(theme.ui_px),
                        &[style.to_run(error.len())],
                        Some(px(width)),
                        None,
                    )
                    .map_or(1, |lines| {
                        lines
                            .iter()
                            .map(|line| line.wrap_boundaries.len() + 1)
                            .sum()
                    })
            },
        )
    }

    fn measure(
        ui_px: f32,
        viewport: Size<Pixels>,
        keymap: &Keymap,
        details: impl IntoIterator<Item = (String, bool)>,
        count_detail_rows: impl Fn(&str, f32) -> usize,
    ) -> Self {
        let viewport_height = f32::from(viewport.height);
        let top = CARD_TOP_PX.min((viewport_height - 150.).max(0.));
        let width = (f32::from(viewport.width) - 24.).clamp(1., MAX_WIDTH);
        let text_width = (width - 2. - LEFT - RIGHT).max(1.);
        let line = (ui_px + 6.).ceil();
        let stacked = text_width < 21. * ui_px;
        let row = if stacked { 2. * line } else { line + 4. };
        let header = (ui_px + 26.).ceil();
        let height = (viewport_height - top - 16.).min(MAX_HEIGHT);
        let details: Vec<_> = details
            .into_iter()
            .map(|(text, error)| {
                let lines = count_detail_rows(&text, text_width);
                (text, error, lines)
            })
            .collect();
        let detail_rows: usize = details.iter().map(|(_, _, lines)| *lines).sum();
        let detail_gap = if detail_rows > 0 { DETAIL_GAP } else { 0. };
        let rows = help_rows(keymap).collect::<Vec<_>>();
        let total = rows.len() + detail_rows;
        let page = (((height - header - 2. - 2. * INSET - detail_gap) / row) as usize)
            .max(1)
            .min(total);
        Self {
            top,
            width,
            text_width,
            header,
            row,
            line,
            stacked,
            details,
            detail_rows,
            detail_gap,
            rows,
            page,
            total,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum HelpRow {
    Section(&'static str),
    Action(Action, String),
}

fn help_rows(keymap: &Keymap) -> impl Iterator<Item = HelpRow> {
    ACTION_SECTIONS.iter().flat_map(move |&(section, actions)| {
        let mut rows = actions
            .iter()
            .filter_map(move |&action| {
                let keys = binding_text(keymap, action);
                (!keys.is_empty()).then_some(HelpRow::Action(action, keys))
            })
            .peekable();
        let header = rows.peek().map(|_| HelpRow::Section(section));
        header.into_iter().chain(rows)
    })
}

fn thumb_height(body: f32, page: usize, total: usize, ui_px: f32) -> f32 {
    let minimum = ui_px * (16. / 15.);
    (body * page as f32 / total as f32).max(minimum).min(body)
}

fn card(
    theme: &Theme,
    layout: Layout,
    offset: f32,
    moving: bool,
    live: bool,
    cx: &mut Context<Inbox>,
) -> Div {
    let page_height = layout.page as f32 * layout.row;
    let body = page_height + layout.detail_gap;
    let last = layout.last_offset();
    let clip = layout.clip_height(offset);
    let action_offset = offset - layout.detail_gap;
    let action_start = ((action_offset / layout.row).floor() as usize).max(layout.detail_rows);
    let end = ((action_offset + clip) / layout.row).ceil() as usize;
    div()
        .w(px(layout.width))
        .h(px(layout.header + body + 2. * INSET + 2.))
        .flex_none()
        .bg(rgb(theme.surface))
        .border_1()
        .border_color(rgb(theme.border))
        .rounded_md()
        .shadow_lg()
        .overflow_hidden()
        .when(live, |card| {
            let row = layout.row;
            card.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(cx.listener(
                    move |inbox, event: &gpui::ScrollWheelEvent, _, cx| {
                        if inbox.session.overlay == Overlay::Help {
                            let rows = -f32::from(event.delta.pixel_delta(px(row)).y) / row;
                            inbox.help_scroll.wheel(
                                rows,
                                &inbox.motion.motion,
                                inbox.motion.clock.now(),
                            );
                            cx.stop_propagation();
                            cx.notify();
                        }
                    },
                ))
        })
        .child(
            div()
                .h(px(layout.header))
                .px(px(LEFT))
                .flex()
                .items_center()
                .border_b(px(1.))
                .border_color(rgb(theme.border))
                .font_weight(FontWeight::SEMIBOLD)
                .child("Keys"),
        )
        .child(
            div()
                .relative()
                .h(px(body + 2. * INSET))
                .child(
                    div()
                        .absolute()
                        .top(px(INSET))
                        .left(px(LEFT))
                        .right(px(RIGHT))
                        .h(px(clip))
                        .overflow_hidden()
                        .children((offset < layout.detail_rows as f32 * layout.row).then(|| {
                            div()
                                .absolute()
                                .left_0()
                                .right_0()
                                .top(px(-offset))
                                .line_height(px(layout.row))
                                .children(layout.details.iter().map(|(text, error, lines)| {
                                    div()
                                        .h(px(layout.row * *lines as f32))
                                        .text_color(rgb(if *error {
                                            theme.error
                                        } else {
                                            theme.dimmed
                                        }))
                                        .child(text.clone())
                                }))
                        }))
                        .child(
                            div()
                                .absolute()
                                .left_0()
                                .right_0()
                                .top(px(
                                    action_start as f32 * layout.row + layout.detail_gap - offset
                                ))
                                .when(
                                    action_start == layout.detail_rows && layout.detail_rows > 0,
                                    |keys| {
                                        keys.child(
                                            div()
                                                .absolute()
                                                .left_0()
                                                .right_0()
                                                .top(px(-layout.detail_gap / 2.))
                                                .h(px(1.))
                                                .bg(rgb(theme.border)),
                                        )
                                    },
                                )
                                .children(
                                    layout
                                        .rows
                                        .iter()
                                        .skip(action_start - layout.detail_rows)
                                        .take(end.saturating_sub(action_start))
                                        .map(|row| help_row(theme, row, &layout)),
                                ),
                        )
                        .children((moving && offset > 0.).then(|| edge(theme, true, 14.)))
                        .children((moving && offset < last).then(|| edge(theme, false, 14.))),
                )
                .when(last > 0., |viewport| {
                    // This gutter is reserved at every scroll position.
                    let thumb = thumb_height(body, layout.page, layout.total, theme.ui_px);
                    viewport.child(
                        div()
                            .absolute()
                            .right(px(9.))
                            .top(px(INSET))
                            .w(px(4.))
                            .h(px(body))
                            .rounded_full()
                            .bg(rgb(theme.border))
                            .child(
                                div()
                                    .absolute()
                                    .top(px((body - thumb) * (offset / last)))
                                    .w_full()
                                    .h(px(thumb))
                                    .rounded_full()
                                    .bg(rgb(theme.dimmed)),
                            ),
                    )
                }),
        )
}

fn help_row(theme: &Theme, row: &HelpRow, layout: &Layout) -> Div {
    let (action, keys) = match row {
        HelpRow::Section(label) => {
            return div()
                .h(px(layout.row))
                .flex()
                .items_end()
                .pb(px(3.))
                .line_height(px(theme.ui_px))
                .text_color(rgb(theme.dimmed))
                .font_weight(FontWeight::SEMIBOLD)
                .child(*label);
        }
        HelpRow::Action(action, keys) => (action, keys),
    };
    div()
        .h(px(layout.row))
        .line_height(px(layout.line))
        .when(!layout.stacked, |row| {
            row.flex().items_center().gap(px(12.))
        })
        .child(
            div()
                .min_w_0()
                // Give GPUI a definite width on its first ellipsis measurement.
                .when(layout.stacked, |label| {
                    label.w(px(layout.text_width)).h(px(layout.line))
                })
                .when(!layout.stacked, |label| label.flex_1())
                .truncate()
                .child(action.label()),
        )
        .child(
            div()
                .when(layout.stacked, |keys| {
                    keys.ml(px(STACKED_KEY_INSET))
                        .w(px((layout.text_width - STACKED_KEY_INSET).max(1.)))
                        .h(px(layout.line))
                })
                .font_family_opt(theme.mono_family.clone())
                .text_color(rgb(theme.dimmed))
                .truncate()
                .child(keys.clone()),
        )
}

/// While a row crosses the viewport boundary, its text fades into the card.
pub(super) fn edge(theme: &Theme, top: bool, height: f32) -> Div {
    div()
        .absolute()
        .left_0()
        .right_0()
        .h(px(height))
        .when(top, |fade| fade.top_0())
        .when(!top, |fade| fade.bottom_0())
        .bg(crate::theme::scroll_fade(theme.surface, top))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn wide_layout() -> Layout {
        Layout::measure(
            15.,
            gpui::size(px(520.), px(720.)),
            &Keymap::default(),
            None,
            |_, _| 0,
        )
    }

    #[test]
    fn first_scroll_key_can_page_before_paint() {
        let now = Instant::now();
        let motion = Motion::new(false);
        let mut scroll = HelpScroll::default();
        assert_eq!(scroll.page, 0);
        let keymap = Keymap::default();
        let measure =
            || Layout::measure(13., gpui::size(px(520.), px(720.)), &keymap, None, |_, _| 0);
        scroll.prepare(now, measure);
        let half_page = scroll.page / 2;
        assert!(half_page > 1);
        assert!(scroll.on_key(KeyId::Ctrl('d'), &motion, now));

        let layout = measure();
        scroll.advance(&layout, now);
        assert_eq!(scroll.target, half_page);
        assert_eq!(scroll.drawn, half_page as f32 * layout.row);
    }

    #[test]
    fn section_headers_and_bound_actions_follow_the_palette_order() {
        let keymap = Keymap::default();
        let rows: Vec<_> = help_rows(&keymap).collect();
        let headings: Vec<_> = rows
            .iter()
            .filter_map(|row| match row {
                HelpRow::Section(label) => Some(*label),
                HelpRow::Action(..) => None,
            })
            .collect();
        assert_eq!(
            headings,
            [
                "Navigation",
                "Search",
                "Reading",
                "Attachments",
                "Compose",
                "Organize",
                "Agents",
                "General",
            ]
        );
        let actions: Vec<_> = rows
            .into_iter()
            .filter_map(|row| match row {
                HelpRow::Section(_) => None,
                HelpRow::Action(action, keys) => Some((action, keys)),
            })
            .collect();
        let palette: Vec<_> = crate::keymap::palette_items(&keymap, "", &[])
            .into_iter()
            .filter(|item| !item.binding.is_empty())
            .map(|item| (item.action, item.binding))
            .collect();
        assert_eq!(actions, palette);
    }

    #[test]
    fn scroll_can_settle_with_a_whole_header_on_either_edge() {
        let now = Instant::now();
        let motion = Motion::new(false);
        let keymap = Keymap::default();
        for ui_px in [12., 18.] {
            for detail in [None, Some(("A full detail line".into(), true))] {
                let layout = Layout::measure(
                    ui_px,
                    gpui::size(px(420.), px(380.)),
                    &keymap,
                    detail,
                    |_, _| 1,
                );
                let header = layout.detail_rows
                    + layout
                        .rows
                        .iter()
                        .position(|row| *row == HelpRow::Section("Reading"))
                        .unwrap();
                let mut scroll = HelpScroll::default();
                scroll.advance(&layout, now);
                for rows_above_header in [layout.page - 1, 0] {
                    let first = header - rows_above_header;
                    assert!(first >= layout.detail_rows);
                    scroll.scroll((first - scroll.target) as isize, &motion, now);
                    scroll.advance(&layout, now);
                    let header_top = header as f32 * layout.row + layout.detail_gap - scroll.drawn;
                    assert_eq!(header_top, rows_above_header as f32 * layout.row);
                }
            }
        }
    }

    #[test]
    fn a_single_row_scroll_glides_across_the_detail_gap() {
        let start = Instant::now();
        let motion = Motion::new(true);
        let layout = Layout::measure(
            15.,
            gpui::size(px(520.), px(720.)),
            &Keymap::default(),
            Some(("A full detail line".into(), true)),
            |_, _| 1,
        );
        let mut scroll = HelpScroll::default();
        scroll.advance(&layout, start);
        assert_eq!(scroll.drawn, 0.);
        assert!(scroll.on_key(KeyId::Down, &motion, start));

        let end = start + motion::SLIDE;
        let destination = layout.row + DETAIL_GAP;
        let glide = scroll.glide.unwrap();
        assert_eq!(glide.value(start), 0.);
        assert_eq!(glide.value(end), destination);
        scroll.advance(&layout, end);
        assert_eq!(scroll.drawn, destination);
        assert!(!scroll.running(end));

        scroll.scroll(scroll.last as isize, &Motion::new(false), end);
        scroll.advance(&layout, end);
        assert_eq!(scroll.drawn, scroll.last as f32 * layout.row + DETAIL_GAP);
    }

    #[test]
    fn every_resting_offset_fills_the_clip_with_whole_lines() {
        for banner_lines in [1, 20] {
            let layout = Layout::measure(
                15.,
                gpui::size(px(520.), px(720.)),
                &Keymap::default(),
                Some(("A detail banner".into(), true)),
                |_, _| banner_lines,
            );
            let (row, banner) = (layout.row, layout.detail_rows as f32 * layout.row);
            let content = layout.total as f32 * row + DETAIL_GAP;
            // Line edges in content coordinates: banner lines, then shortcut
            // rows below the gap.
            let on_edge = |y: f32| {
                let lines = if y <= banner {
                    y
                } else {
                    y - banner - DETAIL_GAP
                };
                lines >= 0. && (lines / row - (lines / row).round()).abs() < 1e-4
            };
            let last = layout.total - layout.page;
            for target in 0..=last {
                let offset = scroll_px(target, row, layout.detail_rows, DETAIL_GAP);
                let bottom = offset + layout.clip_height(offset);
                assert!(on_edge(offset), "{banner_lines} lines, target {target} top");
                assert!(
                    on_edge(bottom) || (bottom - banner - DETAIL_GAP).abs() < 1e-4,
                    "{banner_lines} lines, target {target} bottom {bottom}"
                );
                assert!(
                    bottom <= content + 1e-4,
                    "{banner_lines} lines, target {target}"
                );
            }
            let bottom = layout.last_offset();
            assert_eq!(bottom + layout.clip_height(bottom), content);
        }
    }

    #[test]
    fn scrollbar_thumb_has_a_scaled_floor_bounded_by_the_track() {
        for ui_px in [12., 15., 18.] {
            let floor = thumb_height(50., 2, 100, ui_px);
            assert!((floor - 16. * ui_px / 15.).abs() < 0.001);
        }
        assert_eq!(thumb_height(300., 10, 50, 15.), 60.);
        assert_eq!(thumb_height(10., 1, 100, 15.), 10.);
    }

    #[test]
    fn keyboard_glides_retarget_from_the_drawn_offset() {
        let start = Instant::now();
        let motion = Motion::new(true);
        let mut scroll = HelpScroll::default();
        let layout = wide_layout();
        scroll.advance(&layout, start);
        assert!(scroll.on_key(KeyId::Char('j'), &motion, start));

        let next = start + Duration::from_millis(25);
        scroll.advance(&layout, next);
        let drawn = scroll.drawn;
        assert!(drawn > 0. && drawn < layout.row);
        assert!(scroll.on_key(KeyId::Ctrl('d'), &motion, next));
        scroll.advance(&layout, next);
        assert_eq!(scroll.drawn, drawn);
        assert_eq!(scroll.target, 1 + layout.page / 2);

        let end = next + motion::SLIDE;
        scroll.advance(&layout, end);
        assert_eq!(scroll.drawn, (1 + layout.page / 2) as f32 * layout.row);
        assert!(!scroll.running(end));
    }

    #[test]
    fn reduced_motion_and_wheel_stop_on_whole_rows() {
        let now = Instant::now();
        let motion = Motion::new(false);
        let mut scroll = HelpScroll::default();
        let layout = wide_layout();
        scroll.advance(&layout, now);
        scroll.wheel(0.4, &motion, now);
        assert_eq!(scroll.target, 0);
        scroll.wheel(0.7, &motion, now);
        assert_eq!(scroll.target, 1);
        scroll.wheel(layout.total as f32, &motion, now);
        scroll.advance(&layout, now);
        assert_eq!(scroll.target + scroll.page, layout.total);
        let bottom = (layout.total - layout.page) as f32 * layout.row;
        assert_eq!(scroll.drawn, bottom);
        assert!(!scroll.running(now));

        scroll.on_key(KeyId::Ctrl('u'), &motion, now);
        scroll.on_key(KeyId::Up, &motion, now);
        scroll.advance(&layout, now);
        assert_eq!(
            scroll.drawn,
            bottom - (layout.page / 2 + 1) as f32 * layout.row
        );
        assert!(!scroll.on_key(KeyId::Escape, &motion, now));
    }

    #[test]
    fn wheel_reverses_immediately_at_both_ends() {
        let now = Instant::now();
        let motion = Motion::new(false);
        let mut scroll = HelpScroll::default();
        scroll.advance(&wide_layout(), now);

        scroll.wheel(-2.9, &motion, now);
        assert_eq!(scroll.target, 0);
        scroll.wheel(1., &motion, now);
        assert_eq!(scroll.target, 1);

        scroll.scroll(scroll.last as isize, &motion, now);
        scroll.wheel(2.9, &motion, now);
        assert_eq!(scroll.target, scroll.last);
        scroll.wheel(-1., &motion, now);
        assert_eq!(scroll.target, scroll.last - 1);

        scroll.wheel(2.9, &motion, now);
        assert_eq!(scroll.target, scroll.last);
        scroll.wheel(-1., &motion, now);
        assert_eq!(scroll.target, scroll.last - 1);
    }
}
