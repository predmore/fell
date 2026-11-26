//! Native message painting, the offscreen WebKit worker, and the fixture
//! preview. The inbox owns received-mail preferences and the keymap.

mod body;
mod native;
#[cfg(target_os = "linux")]
mod proxy;
#[cfg(target_os = "linux")]
pub(crate) mod remote_images;
mod theme;
#[cfg(target_os = "linux")]
pub(crate) mod webview;

pub use body::{BodyModel, MessageBody, prepare_stored, round_trip_body};
pub use native::{
    BLOCK_GAP, ImageGallery, ListMarkerFont, blocks_column, document_column, is_right_to_left,
};
pub(crate) use native::{
    BlockColumn, BlockColumns, READING_MEASURE_PX, blocks_column_spaced, plain_text,
};
pub use theme::{Palette, ThemeKind};

use std::sync::Arc;

use fell_render::{RenderMode, WebMail, fixture, prepare};
use gpui::{Context, FocusHandle, KeyDownEvent, Render, SharedString, Window, div, prelude::*, px};

use body::BodyModel as Model;
use theme::{ThemeKind as Theme, paint};

pub struct Reader {
    focus: FocusHandle,
    did_focus: bool,
    title: SharedString,
    from_line: SharedString,
    date_line: SharedString,
    model: Model,
    theme: Theme,
    web_mail: Arc<WebMail>,
    web_key: Option<(i32, &'static str, bool)>,
    web_image: Option<webview::Snapshot>,
    web_job: Option<gpui::Task<()>>,
    web_allowed: Arc<std::sync::atomic::AtomicBool>,
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
        webview::prewarm();
        let gallery = ImageGallery::from_document(opened.model.document());
        let web_mail = Arc::new(WebMail::prepare(opened.model.source_html()));
        let release = cx.on_release(|this, app| {
            this.web_allowed
                .store(false, std::sync::atomic::Ordering::Release);
            this.gallery.release(app);
            if let Some(image) = this.web_image.take() {
                app.drop_image(image.image, None);
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
            web_mail,
            web_key: None,
            web_image: None,
            web_job: None,
            web_allowed: Default::default(),
            web_failed: false,
            gallery,
            _release: release,
        }
    }

    fn sync_web(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use std::sync::atomic::Ordering;
        let web = self.model.mode() == RenderMode::Web;
        let width = f32::from(window.viewport_size().width)
            .round()
            .clamp(1., 1600.) as i32;
        let palette = self.theme.palette();
        let allowed = self.model.remote_images_allowed();
        let key = web.then_some((width, palette.scheme(), allowed));
        if self.web_key == key {
            return;
        }
        self.web_key = key;
        self.web_job = None;
        self.web_allowed.store(false, Ordering::Release);
        if let Some(old) = self.web_image.take() {
            cx.drop_image(old.image, Some(window));
        }
        self.web_failed = false;
        let Some((width, scheme, allowed)) = key else {
            return;
        };
        self.web_allowed = Arc::new(std::sync::atomic::AtomicBool::new(allowed));
        let permission = Arc::clone(&self.web_allowed);
        let mail = Arc::clone(&self.web_mail);
        self.web_job = Some(cx.spawn(async move |this, cx| {
            let html = cx
                .background_spawn(async move {
                    if allowed {
                        remote_images::inline_page(&mail, scheme, || {
                            permission.load(Ordering::Acquire)
                        })
                        .0
                    } else {
                        mail.page(scheme, |_| None)
                    }
                })
                .await;
            let image = webview::render(webview::WebRequest {
                html,
                width,
                background: palette.background,
                settle_images: allowed,
            })
            .await;
            let _ = this.update(cx, |reader, cx| {
                reader.web_failed = image.is_none();
                reader.web_image = image;
                cx.notify();
            });
        }));
    }
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
            cx.on_focus_lost(window, |reader, window, _| reader.focus.focus(window))
                .detach();
        }
        self.sync_web(window, cx);
        let palette = self.theme.palette();
        let web = self.model.mode() == RenderMode::Web;
        let edge = banner_edge(self.theme, web, palette.border);
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
            .font_family("Inter")
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let mods = &event.keystroke.modifiers;
                if !mods.control && !mods.alt && !mods.platform && event.keystroke.key == "tab" {
                    if mods.shift {
                        window.focus_prev();
                    } else {
                        window.focus_next();
                    }
                    cx.stop_propagation();
                    return;
                }
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
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_baseline()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_lg()
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .child(self.title.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(paint(palette.dimmed))
                                            .mt(px(-2.))
                                            .child(self.date_line.clone()),
                                    ),
                            )
                            .when(!self.from_line.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_sm()
                                        .text_color(paint(palette.dimmed))
                                        .child(self.from_line.clone()),
                                )
                            }),
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
                    .gap_2()
                    .border_b(px(1.))
                    .border_color(paint(edge))
                    .bg(paint(palette.code_bg))
                    .text_sm()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                        this.model
                            .set_remote_images_allowed(!this.model.remote_images_allowed());
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_w(px(0.))
                            .child(remote_image_mark(palette.accent))
                            .child(
                                div()
                                    .text_color(paint(palette.foreground))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child(status),
                            ),
                    )
                    .child(
                        div()
                            .px_3()
                            .py_1()
                            .rounded(px(4.))
                            .border(px(1.))
                            .border_color(paint(palette.accent))
                            .text_color(paint(palette.accent))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(action),
                    ),
            )
            .child(message_body(self, &palette, window))
    }
}

/// Accent tick so the remote-image status is not another dimmed header line.
fn remote_image_mark(tone: u32) -> gpui::Div {
    div()
        .w(px(3.))
        .h(px(14.))
        .flex_none()
        .rounded(px(1.))
        .bg(paint(tone))
}

fn message_body(
    reader: &Reader,
    palette: &theme::Palette,
    window: &Window,
) -> gpui::Stateful<gpui::Div> {
    const NATIVE_BODY_PAD_PX: f32 = 16.;
    let web = reader.model.mode() == RenderMode::Web;
    let mut body = div()
        .id("message-body")
        .flex_1()
        .min_h(px(0.))
        .overflow_y_scroll();
    if !web {
        body = body.p(px(NATIVE_BODY_PAD_PX));
    }
    if web {
        if let Some(image) = reader.web_image.clone() {
            body.child(web_snapshot(
                &image,
                f32::from(window.viewport_size().width),
            ))
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
        let mut style = window.text_style();
        style.font_family = "Inter".into();
        let markers = ListMarkerFont::new(window, style);
        body.child(document_column(
            reader.model.document(),
            palette,
            reader.model.remote_images_allowed(),
            &reader.gallery,
            (f32::from(window.viewport_size().width) - 2. * NATIVE_BODY_PAD_PX).max(0.),
            &markers,
        ))
    }
}

/// Divider under the remote-image banner.
///
/// In the dark theme the shared border is lighter than the banner, so it
/// disappears where a beige newsletter starts. That edge is one flat
/// hairline, darker than the banner, with no shade on the page. Light
/// theme and native mail keep the shared border. The banner fill and the
/// accent mark stay as they are.
fn banner_edge(theme: Theme, web: bool, border: u32) -> u32 {
    if web && theme == Theme::Dark {
        0x111111
    } else {
        border
    }
}

pub(crate) fn web_snapshot(snapshot: &webview::Snapshot, width: f32) -> gpui::Div {
    let height = snapshot.height * width / snapshot.width;
    div().w(px(width)).h(px(height)).child(
        gpui::img(snapshot.image.clone())
            .w(px(width))
            .h(px(height))
            .object_fit(gpui::ObjectFit::Fill),
    )
}

#[cfg(test)]
mod tests {
    use super::{Theme, banner_edge};

    #[test]
    fn dark_newsletter_edge_is_darker_than_the_banner() {
        let dark = Theme::Dark.palette();
        let edge = banner_edge(Theme::Dark, true, dark.border);
        assert!(edge < dark.surface);
        assert!(edge < dark.border);
        assert_eq!(banner_edge(Theme::Dark, false, dark.border), dark.border);
        let light = Theme::Light.palette();
        assert_eq!(banner_edge(Theme::Light, true, light.border), light.border);
    }
}
