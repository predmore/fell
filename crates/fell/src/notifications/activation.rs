use std::time::{Duration, Instant};

use gpui::Window;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::protocol::{wl_callback, wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;

pub(crate) struct Activation {
    queue: EventQueue<Globals>,
    state: Globals,
    token: String,
    deadline: Instant,
}

impl Activation {
    pub fn connect(window: &Window, token: String) -> Option<Self> {
        let RawDisplayHandle::Wayland(display) = window.display_handle().ok()?.as_raw() else {
            return None;
        };
        // SAFETY: GPUI owns the display. This guest queue is kept in the
        // window's root view while the registry request is pending.
        let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
        let connection = Connection::from_backend(backend);
        let queue = connection.new_event_queue();
        connection.display().get_registry(&queue.handle(), ());
        connection.display().sync(&queue.handle(), ());
        connection.flush().ok()?;
        Some(Self {
            queue,
            state: Globals::default(),
            token,
            deadline: Instant::now() + Duration::from_millis(300),
        })
    }

    /// Returns whether the compositor's registry reply is still pending.
    pub fn pending(&mut self, window: &Window) -> bool {
        if self.queue.dispatch_pending(&mut self.state).is_ok() {
            if let Some(activation) = self.state.activation.take() {
                let target = surface(window, activation.backend());
                if let Some(surface) = &target {
                    activation.activate(std::mem::take(&mut self.token), surface);
                }
                activation.destroy();
                let _ = self.queue.flush();
                if target.is_some() {
                    return false;
                }
            }
            if !self.state.ready && Instant::now() < self.deadline {
                return true;
            }
        }
        window.activate_window();
        false
    }
}

fn surface(
    window: &Window,
    backend: &wayland_client::backend::WeakBackend,
) -> Option<wl_surface::WlSurface> {
    let RawWindowHandle::Wayland(handle) = HasWindowHandle::window_handle(window).ok()?.as_raw()
    else {
        return None;
    };
    // SAFETY: the handle borrows this window's live surface for the request.
    let id = unsafe {
        ObjectId::from_ptr(
            wl_surface::WlSurface::interface(),
            handle.surface.as_ptr().cast(),
        )
    }
    .ok()?;
    wl_surface::WlSurface::from_id(&Connection::from_backend(backend.upgrade()?), id).ok()
}

#[derive(Default)]
struct Globals {
    activation: Option<XdgActivationV1>,
    ready: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Globals {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
            && interface == XdgActivationV1::interface().name
        {
            state.activation = Some(registry.bind(name, 1, queue, ()));
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for Globals {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.ready = true;
    }
}

wayland_client::delegate_noop!(Globals: ignore XdgActivationV1);
