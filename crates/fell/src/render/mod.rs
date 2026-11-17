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
use native::{ImageGallery, decode_image};
use theme::{ThemeKind as Theme, paint};

pub struct Reader {
    focus: FocusHandle,
    did_focus: bool,
    title: SharedString,
    from_line: SharedString,
    date_line: SharedString,
    model: Model,
    theme: Theme,
    web_open: bool,
    /// Wrapped HTML is rebuilt only when one of these three inputs changes.
    web_source: String,
    web_scheme: &'static str,
    web_images: bool,
    web_token: u64,
    web_image: Option<Arc<RenderImage>>,
    web_generation: u64,
    web_settled: bool,
    web_failed: bool,
    gallery: ImageGallery,
    _release: gpui::Subscription,
}

pub struct Opened {
    pub title: SharedString,
    pub from_line: SharedString,
    pub date_line: SharedString,
    pub model: Model,
    pub theme: Theme,
}

impl Reader {
    pub fn new(opened: Opened, cx: &mut Context<Self>) -> Self {
        let gallery = ImageGallery::from_document(opened.model.document());
        let release = cx.on_release(|this, app| {
            this.gallery.release(app);
            if let Some(image) = this.web_image.take() {
                app.drop_image(image, None);
            }
        });
        Self {
            focus: cx.focus_handle(),
            did_focus: false,
            title: opened.title,
            from_line: opened.from_line,
            date_line: opened.date_line,
            model: opened.model,
            theme: opened.theme,
            web_open: false,
            web_source: String::new(),
            web_scheme: "",
            web_images: false,
            web_token: 0,
            web_image: None,
            web_generation: 0,
            web_settled: false,
            web_failed: false,
            gallery,
            _release: release,
        }
    }

    fn clear_web_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(image) = self.web_image.take() {
            cx.drop_image(image, Some(window));
        }
    }

    fn sync_web(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                    self.web_source.clear();
                    self.web_scheme = "";
                    self.web_images = false;
                    self.clear_web_image(window, cx);
                    self.web_generation = 0;
                    self.web_settled = false;
                    self.web_failed = false;
                }
                return;
            }
            let palette = self.theme.palette();
            let scheme = palette.scheme();
            let allow_images = self.model.remote_images_allowed();
            let unchanged = web_frame_is_current(
                self.web_open,
                &self.web_source,
                self.model.source_html(),
                self.web_scheme,
                scheme,
                self.web_images,
                allow_images,
            );
            if unchanged {
                self.poll_web_frame(window, cx);
                return;
            }
            let source = self.model.source_html().to_string();
            let html = wrap_for_webview(
                &source,
                &WebFrame {
                    scheme,
                    allow_remote_images: allow_images,
                },
            );
            self.web_token = self.web_token.wrapping_add(1);
            self.clear_web_image(window, cx);
            self.web_generation = 0;
            self.web_settled = false;
            self.web_failed = false;
            webview::ensure_started();
            webview::show(webview::WebRequest {
                html,
                token: self.web_token,
                background: channels(palette.background),
                allow_images,
            });
            self.web_source = source;
            self.web_scheme = scheme;
            self.web_images = allow_images;
            self.web_open = true;
        }
    }

    #[cfg(target_os = "linux")]
    fn poll_web_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.web_failed || self.web_settled {
            return;
        }
        let seen = self.web_image.is_some().then_some(self.web_generation);
        match webview::status_for(self.web_token, seen) {
            webview::WebStatus::Ready {
                png,
                generation,
                settled,
            } => match decode_image(&png) {
                Some(image) => {
                    self.clear_web_image(window, cx);
                    self.web_image = Some(image);
                    self.web_generation = generation;
                    self.web_settled = settled;
                }
                None => self.web_failed = true,
            },
            webview::WebStatus::Unchanged { settled } => self.web_settled = settled,
            webview::WebStatus::Failed => self.web_failed = true,
            webview::WebStatus::Pending => {}
        }
    }
}

fn web_frame_is_current(
    open: bool,
    cached_source: &str,
    source: &str,
    cached_scheme: &str,
    scheme: &str,
    cached_images: bool,
    allow_images: bool,
) -> bool {
    open && cached_source == source && cached_scheme == scheme && cached_images == allow_images
}

fn display_address(addr: &fell_core::Address) -> String {
    match addr
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        Some(name) => format!("{name} <{}>", addr.email),
        None => addr.email.clone(),
    }
}

fn format_unix_date(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let clock = unix.rem_euclid(86_400);
    let hour = clock / 3_600;
    let minute = (clock % 3_600) / 60;
    format!(
        "{d} {} {y}, {hour:02}:{minute:02}",
        MONTHS[usize::try_from(m).unwrap_or(1).saturating_sub(1)]
    )
}

fn from_header(headers: &fell_render::MailHeaders) -> String {
    headers.get("From").map(str::trim).unwrap_or("").to_string()
}

pub fn open_fixture(name: &str, theme: Theme) -> Opened {
    let Some(fixture) = fixture(name) else {
        return Opened {
            title: format!("Unknown fixture {name}").into(),
            from_line: SharedString::default(),
            date_line: SharedString::default(),
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
                let from_line = display_address(&message.headers.from);
                let date_line = format_unix_date(message.headers.date_unix);
                Opened {
                    title,
                    from_line: from_line.into(),
                    date_line: date_line.into(),
                    model: Model::from_stored(&message),
                    theme,
                }
            }
            Err(_) => Opened {
                title: subject.into(),
                from_line: from_header(&fixture.headers).into(),
                date_line: SharedString::default(),
                model: Model::from_prepared(prepare(&fixture.html, &fixture.headers)),
                theme,
            },
        }
    } else {
        let _ = round_trip_body(&fixture.html, &subject);
        Opened {
            title: subject.into(),
            from_line: from_header(&fixture.headers).into(),
            date_line: format_unix_date(1_700_000_000).into(),
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
        self.sync_web(window, cx);
        if self.model.mode() == RenderMode::Web && !self.web_failed && !self.web_settled {
            window.request_animation_frame();
        }
        let palette = self.theme.palette();
        let (status, action) = if self.model.remote_images_allowed() {
            ("Remote images allowed", "Block sender")
        } else {
            ("Remote images blocked", "Allow sender")
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
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(self.title.clone()),
                            )
                            .when(!self.from_line.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_sm()
                                        .text_color(paint(palette.dimmed))
                                        .child(self.from_line.clone()),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(paint(palette.dimmed))
                            .child(self.date_line.clone()),
                    ),
            )
            .child(
                div()
                    .id("remote-images")
                    .px_4()
                    .py_2()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b(px(1.))
                    .border_color(paint(palette.border))
                    .bg(paint(palette.surface))
                    .text_sm()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                        this.model
                            .set_remote_images_allowed(!this.model.remote_images_allowed());
                        cx.notify();
                    }))
                    .child(status)
                    .child(
                        div()
                            .px_3()
                            .py_1()
                            .rounded(px(4.))
                            .border(px(1.))
                            .border_color(paint(palette.accent))
                            .text_color(paint(palette.accent))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(action),
                    ),
            )
            .child(message_body(self, &palette))
    }
}

fn message_body(reader: &Reader, palette: &theme::Palette) -> gpui::Stateful<gpui::Div> {
    let body = div()
        .id("message-body")
        .flex_1()
        .min_h(px(0.))
        .overflow_y_scroll()
        .px_4()
        .py_4();
    if reader.model.mode() == RenderMode::Web {
        if let Some(image) = reader.web_image.clone() {
            body.child(native::embedded_web(image))
        } else if reader.web_failed {
            body.child(
                div()
                    .text_color(paint(palette.foreground))
                    .child("The web view did not finish loading."),
            )
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
            &reader.gallery,
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

#[cfg(test)]
mod cache_tests {
    use super::web_frame_is_current;

    #[test]
    fn web_html_is_rebuilt_only_when_inputs_change() {
        assert!(web_frame_is_current(
            true, "<p>a</p>", "<p>a</p>", "dark", "dark", false, false
        ));
        assert!(!web_frame_is_current(
            false, "<p>a</p>", "<p>a</p>", "dark", "dark", false, false
        ));
        assert!(!web_frame_is_current(
            true, "<p>a</p>", "<p>b</p>", "dark", "dark", false, false
        ));
        assert!(!web_frame_is_current(
            true, "<p>a</p>", "<p>a</p>", "dark", "light", false, false
        ));
        assert!(!web_frame_is_current(
            true, "<p>a</p>", "<p>a</p>", "dark", "dark", false, true
        ));
    }
}
