//! Message body for the inbox to embed.
//!
//! This module paints one message. It does not own the thread list, the
//! keymap, or the command palette. The preview window below is only so the
//! renderer can be seen before that shell lands; its `w` / `t` / `i` keys
//! are local to the preview.

mod body;
mod native;
mod theme;
#[cfg(target_os = "linux")]
mod webview;

pub use body::{BodyModel, MessageBody, prepare_stored, round_trip_body};
pub use native::document_column;
pub use theme::{Palette, ThemeKind};

use fell_render::{RenderMode, WebFrame, fixture, prepare, wrap_for_webview};
use gpui::{Context, FocusHandle, KeyDownEvent, Render, SharedString, Window, div, prelude::*, px};

use body::BodyModel as Model;
use theme::{ThemeKind as Theme, paint};

pub struct Reader {
    focus: FocusHandle,
    did_focus: bool,
    title: SharedString,
    source: SharedString,
    model: Model,
    theme: Theme,
    web_open: bool,
    web_key: String,
}

pub struct Opened {
    pub title: SharedString,
    pub source: SharedString,
    pub model: Model,
    pub theme: Theme,
}

impl Reader {
    pub fn new(opened: Opened, cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            did_focus: false,
            title: opened.title,
            source: opened.source,
            model: opened.model,
            theme: opened.theme,
            web_open: false,
            web_key: String::new(),
        }
    }

    fn sync_web(&mut self) {
        #[cfg(not(target_os = "linux"))]
        {
            return;
        }
        #[cfg(target_os = "linux")]
        {
            if self.model.mode() != RenderMode::Web {
                if self.web_open {
                    webview::hide();
                    self.web_open = false;
                    self.web_key.clear();
                }
                return;
            }
            let palette = self.theme.palette();
            let notice = if self.model.remote_images_allowed() {
                "Web view · remote images allowed for this message"
            } else {
                "Web view · remote images blocked · JavaScript off"
            };
            let html = wrap_for_webview(
                self.model.source_html(),
                &WebFrame {
                    scheme: palette.scheme(),
                    background: hex_css(palette.background),
                    foreground: hex_css(palette.foreground),
                    accent: hex_css(palette.accent),
                    title: self.title.to_string(),
                    notice: notice.to_string(),
                    allow_remote_images: self.model.remote_images_allowed(),
                },
            );
            if self.web_open && self.web_key == html {
                return;
            }
            webview::ensure_started();
            webview::show(webview::WebRequest {
                html: html.clone(),
                title: self.title.to_string(),
                background: channels(palette.background),
                foreground: channels(palette.foreground),
                allow_images: self.model.remote_images_allowed(),
            });
            self.web_key = html;
            self.web_open = true;
        }
    }
}

pub fn open_fixture(name: &str, theme: Theme) -> Opened {
    let Some(fixture) = fixture(name) else {
        return Opened {
            title: format!("Unknown fixture {name}").into(),
            source: "fixture".into(),
            model: Model::from_prepared(prepare(
                &format!("<p>No fixture named {name}.</p>"),
                &fell_render::MailHeaders::new(),
            )),
            theme,
        };
    };
    let subject = fixture
        .headers
        .get("Subject")
        .unwrap_or(fixture.name.as_str())
        .to_string();
    if fixture.expected == RenderMode::Native {
        match round_trip_body(&fixture.html, &subject) {
            Ok(message) => {
                let title = message.headers.subject.clone().into();
                Opened {
                    title,
                    source: "local store".into(),
                    model: Model::from_stored(&message),
                    theme,
                }
            }
            Err(err) => Opened {
                title: subject.into(),
                source: format!("store error: {err}").into(),
                model: Model::from_prepared(prepare(&fixture.html, &fixture.headers)),
                theme,
            },
        }
    } else {
        let _ = round_trip_body(&fixture.html, &subject);
        Opened {
            title: subject.into(),
            source: "fixture headers".into(),
            model: Model::from_prepared(prepare(&fixture.html, &fixture.headers)),
            theme,
        }
    }
}

impl Render for Reader {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.did_focus {
            self.did_focus = true;
            self.focus.focus(window);
        }
        self.sync_web();
        let palette = self.theme.palette();
        let mode = match self.model.mode() {
            RenderMode::Native => "native",
            RenderMode::Web => "web view",
        };
        let images = if self.model.remote_images_allowed() {
            "remote images allowed"
        } else {
            "remote images blocked"
        };
        div()
            .id("fell-reader")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(paint(palette.background))
            .text_color(paint(palette.foreground))
            .font_family("Liberation Sans")
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                let mods = &event.keystroke.modifiers;
                if mods.control || mods.alt || mods.platform || mods.shift {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "w" => this.model.toggle_mode(),
                    "t" => this.theme = this.theme.toggle(),
                    "i" => this
                        .model
                        .set_remote_images_allowed(!this.model.remote_images_allowed()),
                    _ => return,
                }
                cx.notify();
            }))
            .child(
                div()
                    .px_4()
                    .py_3()
                    .bg(paint(palette.surface))
                    .border_b(px(1.))
                    .border_color(paint(palette.border))
                    .flex()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().font_weight(gpui::FontWeight::BOLD).child("Fell"))
                            .child(div().text_sm().child(self.title.clone())),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(paint(palette.dimmed))
                            .child(format!("{mode} · {} · {}", self.theme.label(), self.source)),
                    ),
            )
            .child(
                div()
                    .px_4()
                    .py_2()
                    .text_sm()
                    .text_color(paint(palette.accent))
                    .child(images),
            )
            .child(
                div()
                    .id("message-body")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .px_5()
                    .py_4()
                    .child(document_column(
                        self.model.document(),
                        &palette,
                        self.model.remote_images_allowed(),
                    )),
            )
            .child(
                div()
                    .px_4()
                    .py_2()
                    .text_xs()
                    .text_color(paint(palette.dimmed))
                    .bg(paint(palette.surface))
                    .border_t(px(1.))
                    .border_color(paint(palette.border))
                    .child("w  native / web view     t  dark / light     i  remote images"),
            )
    }
}

fn channels(hex: u32) -> (u8, u8, u8) {
    (
        ((hex >> 16) & 0xff) as u8,
        ((hex >> 8) & 0xff) as u8,
        (hex & 0xff) as u8,
    )
}

fn hex_css(hex: u32) -> &'static str {
    match hex {
        0x1c1c1c => "#1c1c1c",
        0xe8e8e8 => "#e8e8e8",
        0xa3a3a3 => "#a3a3a3",
        0x8eb6ff => "#8eb6ff",
        0x2a2a2a => "#2a2a2a",
        0x3a3a3a => "#3a3a3a",
        0x242424 => "#242424",
        0xf6f5f3 => "#f6f5f3",
        0x1a1a1a => "#1a1a1a",
        0x5e5e5e => "#5e5e5e",
        0x0b4f9c => "#0b4f9c",
        0xe7e5e1 => "#e7e5e1",
        0xd5d2cc => "#d5d2cc",
        0xfffefc => "#fffefc",
        _ => "#000000",
    }
}
