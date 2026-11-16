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

use std::sync::Arc;

use fell_render::{RenderMode, WebFrame, fixture, prepare, wrap_for_webview};
use gpui::{
    Context, FocusHandle, KeyDownEvent, Render, RenderImage, SharedString, Window, div, prelude::*,
    px,
};

use body::BodyModel as Model;
use native::decode_image;
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
    web_token: u64,
    web_image: Option<Arc<RenderImage>>,
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
            web_token: 0,
            web_image: None,
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
                    self.web_image = None;
                }
                return;
            }
            let palette = self.theme.palette();
            let html = wrap_for_webview(
                self.model.source_html(),
                &WebFrame {
                    scheme: palette.scheme(),
                    allow_remote_images: self.model.remote_images_allowed(),
                },
            );
            if self.web_open && self.web_key == html {
                if self.web_image.is_none()
                    && let Some(png) = webview::frame_for(self.web_token)
                {
                    self.web_image = decode_image(&png);
                }
                return;
            }
            self.web_token = self.web_token.wrapping_add(1);
            self.web_image = None;
            webview::ensure_started();
            webview::show(webview::WebRequest {
                html: html.clone(),
                token: self.web_token,
                background: channels(palette.background),
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
        if self.model.mode() == RenderMode::Web && self.web_image.is_none() {
            window.request_animation_frame();
        }
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
            .child(message_body(self, &palette))
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

fn message_body(reader: &Reader, palette: &theme::Palette) -> gpui::Stateful<gpui::Div> {
    let body = div()
        .id("message-body")
        .flex_1()
        .min_h(px(0.))
        .overflow_y_scroll()
        .px_5()
        .py_4();
    if reader.model.mode() == RenderMode::Web {
        if let Some(image) = reader.web_image.clone() {
            body.child(native::embedded_web(image))
        } else {
            body.child(
                div()
                    .text_color(paint(palette.foreground))
                    .child("Opening the web view"),
            )
        }
    } else {
        body.child(document_column(
            reader.model.document(),
            palette,
            reader.model.remote_images_allowed(),
        ))
    }
}

fn channels(hex: u32) -> (u8, u8, u8) {
    (
        ((hex >> 16) & 0xff) as u8,
        ((hex >> 8) & 0xff) as u8,
        (hex & 0xff) as u8,
    )
}
