//! GPUI shell for Fell. Linux / Wayland is the first target (Omarchy, Hyprland).
//!
//! This crate renders. It does not speak IMAP or Proton. It reads `fell-core`.

use fell_core::{Account, AccountId, BackendKind, Store};
use gpui::{
    App, Application, Bounds, Context, SharedString, TitlebarOptions, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, rgb, size,
};

struct FellApp {
    subtitle: SharedString,
}

impl Render for FellApp {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .bg(rgb(0x1c1c1c))
            .size_full()
            .justify_center()
            .items_center()
            .text_color(rgb(0xe8e8e8))
            .child(div().text_xl().child("Fell"))
            .child(div().text_sm().child(self.subtitle.clone()))
    }
}

fn load_subtitle() -> SharedString {
    let store = match Store::open_in_memory() {
        Ok(store) => store,
        Err(_) => return "local store unavailable".into(),
    };
    let account = Account {
        id: AccountId("local".into()),
        display_name: "Fell".into(),
        email: "unset@local".into(),
        backend_kind: BackendKind::Bridge,
    };
    if store.upsert_account(&account).is_err() {
        return "could not open mailbox".into();
    }
    match store.list_threads(&account.id) {
        Ok(threads) => format!("{} threads · waiting on Bridge sync", threads.len()).into(),
        Err(_) => "could not list threads".into(),
    }
}

fn main() {
    let subtitle = load_subtitle();
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(720.), px(480.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Fell".into()),
                    ..Default::default()
                }),
                app_id: Some("dev.predmore.fell".into()),
                focus: true,
                show: true,
                ..Default::default()
            },
            |_window, cx| cx.new(|_| FellApp { subtitle }),
        )
        .expect("open Fell window");
        cx.activate(true);
    });
}
