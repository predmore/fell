//! Single-line fields for account setup, with native text input and masking.

use std::ops::Range;

use gpui::{
    Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler, IntoElement,
    Keystroke, Pixels, Point, SharedString, Styled, TextRun, UTF16Selection, Window, canvas, fill,
    point, px, rgb, size,
};

use super::{Control, Field, Onboarding};

#[derive(Default)]
pub(super) struct Editor {
    anchor: usize,
    caret: usize,
    marked: Option<Range<usize>>,
    layout: Option<FieldLayout>,
}

impl Editor {
    pub fn reset(&mut self, end: usize) {
        *self = Self {
            anchor: end,
            caret: end,
            ..Self::default()
        };
    }

    fn range(&self) -> Range<usize> {
        self.anchor.min(self.caret)..self.anchor.max(self.caret)
    }

    fn move_to(&mut self, at: usize, extend: bool) {
        self.caret = at;
        if !extend {
            self.anchor = at;
        }
    }
}

struct FieldLayout {
    field: Field,
    origin: Point<Pixels>,
    line: gpui::ShapedLine,
}

impl Onboarding {
    pub(super) fn text_field(&self) -> Option<Field> {
        match self.control {
            Control::Field(Field::Security(_)) => None,
            Control::Field(field) => Some(field),
            _ => None,
        }
    }

    pub(super) fn point_caret(&mut self, field: Field, position: Point<Pixels>, extend: bool) {
        let Some(layout) = &self.editor.layout else {
            return;
        };
        if layout.field == field {
            let index = layout
                .line
                .closest_index_for_x(position.x - layout.origin.x);
            let text = self.form.text(field);
            let at = if field == Field::Password {
                text.char_indices()
                    .nth(index / '•'.len_utf8())
                    .map_or(text.len(), |(i, _)| i)
            } else {
                index.min(text.len())
            };
            self.editor.move_to(at, extend);
        }
    }

    fn replace(&mut self, field: Field, range: Range<usize>, insert: &str) -> Range<usize> {
        let mut value = self.form.text(field).to_owned();
        let room =
            4096_usize.saturating_sub(value.chars().count() - value[range.clone()].chars().count());
        let insert: String = insert
            .chars()
            .filter(|ch| !ch.is_control())
            .take(room)
            .collect();
        let added = range.start..range.start + insert.len();
        value.replace_range(range, &insert);
        self.form.set_text(field, value);
        self.editor.marked = None;
        self.editor.move_to(added.end, false);
        added
    }

    pub(super) fn edit_key(&mut self, key: &Keystroke, cx: &mut Context<Self>) -> bool {
        let Some(field) = self.text_field() else {
            return false;
        };
        let text = self.form.text(field);
        let selected = self.editor.range();
        let caret = self.editor.caret;
        let previous = text[..caret]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
        let next = caret + text[caret..].chars().next().map_or(0, char::len_utf8);
        let shift = key.modifiers.shift;
        match (key.modifiers.control, key.key.as_str()) {
            (true, "a") => {
                self.editor.anchor = 0;
                self.editor.move_to(text.len(), true);
            }
            (true, "c" | "x") => {
                if field != Field::Password && !selected.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        text[selected.clone()].to_owned(),
                    ));
                    if key.key == "x" {
                        self.replace(field, selected, "");
                    }
                }
            }
            (true, "v") => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.replace(field, selected, &text);
                }
            }
            (false, "left") => self.editor.move_to(
                if !shift && !selected.is_empty() {
                    selected.start
                } else {
                    previous
                },
                shift,
            ),
            (false, "right") => self.editor.move_to(
                if !shift && !selected.is_empty() {
                    selected.end
                } else {
                    next
                },
                shift,
            ),
            (false, "home") => self.editor.move_to(0, shift),
            (false, "end") => self.editor.move_to(text.len(), shift),
            (false, "backspace" | "delete") => {
                let range = if !selected.is_empty() {
                    selected
                } else if key.key == "backspace" {
                    previous..caret
                } else {
                    caret..next
                };
                self.replace(field, range, "");
            }
            _ => return false,
        }
        cx.notify();
        true
    }

    pub(super) fn field_line(
        &self,
        field: Field,
        placeholder: SharedString,
        active: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let text = self.form.text(field);
        let empty = text.is_empty();
        let masked = field == Field::Password;
        let draw_index = |i: usize| {
            if masked {
                text[..i].chars().count() * '•'.len_utf8()
            } else {
                i
            }
        };
        let display = if empty {
            placeholder
        } else if masked {
            "•".repeat(text.chars().count()).into()
        } else {
            text.to_owned().into()
        };
        let focused = active && self.text_field() == Some(field);
        let selection = self.editor.range();
        let (caret, selected) = if focused {
            (
                draw_index(self.editor.caret),
                draw_index(selection.start)..draw_index(selection.end),
            )
        } else {
            (0, 0..0)
        };
        let theme = &self.theme;
        let dimmed = rgb(theme.dimmed);
        let foreground = rgb(theme.foreground);
        let highlight = rgb(theme.selection);
        let selected_text = rgb(theme.on_selection);
        let accent = rgb(theme.accent);
        let font_size = px(theme.ui_px);
        let focus = self.focus.clone();
        let entity = cx.entity();
        canvas(
            move |bounds, window, _cx| {
                let run = TextRun {
                    len: display.len(),
                    font: window.text_style().font(),
                    color: (if empty { dimmed } else { foreground }).into(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let runs = if selected.is_empty() {
                    vec![run]
                } else {
                    vec![
                        TextRun {
                            len: selected.start,
                            ..run.clone()
                        },
                        TextRun {
                            len: selected.len(),
                            color: selected_text.into(),
                            ..run.clone()
                        },
                        TextRun {
                            len: display.len() - selected.end,
                            ..run
                        },
                    ]
                    .into_iter()
                    .filter(|run| run.len > 0)
                    .collect()
                };
                let line = window
                    .text_system()
                    .shape_line(display, font_size, &runs, None);
                let caret = line.x_for_index(caret);
                let scroll = if focused {
                    (caret + px(2.) - bounds.size.width).max(px(0.))
                } else {
                    px(0.)
                };
                let origin = point(bounds.left() - scroll, bounds.top());
                (line, origin, caret, selected)
            },
            move |bounds, (line, origin, caret, selected), window, cx| {
                if focused {
                    window.handle_input(
                        &focus,
                        ElementInputHandler::new(bounds, entity.clone()),
                        cx,
                    );
                    if !selected.is_empty() {
                        let left = line.x_for_index(selected.start);
                        let right = line.x_for_index(selected.end);
                        window.paint_quad(fill(
                            Bounds::new(
                                point(origin.x + left.min(right), origin.y),
                                size((right - left).abs(), bounds.size.height),
                            ),
                            highlight,
                        ));
                    }
                }
                let _ = line.paint(origin, bounds.size.height, window, cx);
                if focused {
                    if selected.is_empty() && focus.is_focused(window) {
                        window.paint_quad(fill(
                            Bounds::new(
                                point(origin.x + caret, origin.y + px(2.)),
                                size(px(1.5), bounds.size.height - px(4.)),
                            ),
                            accent,
                        ));
                    }
                    entity.update(cx, |view, _| {
                        view.editor.layout = Some(FieldLayout {
                            field,
                            origin,
                            line,
                        });
                    });
                }
            },
        )
        .w_full()
        .h(font_size + px(6.))
    }

    fn replacement_range(&self, field: Field, range: Option<Range<usize>>) -> Range<usize> {
        range
            .map(|range| from_utf16(self.form.text(field), range))
            .or_else(|| self.editor.marked.clone())
            .unwrap_or_else(|| self.editor.range())
    }
}

impl EntityInputHandler for Onboarding {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let field = self
            .text_field()
            .filter(|field| *field != Field::Password)?;
        let text = self.form.text(field);
        let range = from_utf16(text, range);
        *adjusted = Some(to_utf16(text, range.clone()));
        Some(text[range].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let text = self.form.text(self.text_field()?);
        Some(UTF16Selection {
            range: to_utf16(text, self.editor.range()),
            reversed: self.editor.caret < self.editor.anchor,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        let text = self.form.text(self.text_field()?);
        Some(to_utf16(text, self.editor.marked.clone()?))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.editor.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(field) = self.text_field() {
            self.replace(field, self.replacement_range(field, range), text);
            cx.notify();
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(field) = self.text_field() {
            let added = self.replace(field, self.replacement_range(field, range), text);
            if !added.is_empty() {
                if let Some(selected) = selected {
                    let range = from_utf16(&self.form.text(field)[added.clone()], selected);
                    self.editor.anchor = added.start + range.start;
                    self.editor.caret = added.start + range.end;
                }
                self.editor.marked = Some(added);
            }
            cx.notify();
        }
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(bounds)
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

fn from_utf16(text: &str, range: Range<usize>) -> Range<usize> {
    let offset = |target: usize| {
        let mut units = 0;
        for (index, ch) in text.char_indices() {
            if units >= target {
                return index;
            }
            units += ch.len_utf16();
        }
        text.len()
    };
    let start = offset(range.start);
    start..offset(range.end).max(start)
}

fn to_utf16(text: &str, range: Range<usize>) -> Range<usize> {
    text[..range.start].encode_utf16().count()..text[..range.end].encode_utf16().count()
}
