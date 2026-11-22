//! Desktop notifications run independently of IMAP and of the window.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::pending;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_io::Timer;
use fell_core::{AccountId, InboxArrival, MessageId, NotificationMode};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::{FutureExt, StreamExt, future::join_all};
use unicode_linebreak::{BreakClass, break_property};
use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};
use zbus::connection::Builder;
use zbus::proxy::SignalStream;
use zbus::zvariant::Value;
use zbus::{Message, Proxy};

use crate::store_actor::StoreClient;

mod activation;
pub(crate) use activation::Activation;

const SERVICE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const APP_ID: &str = "dev.predmore.fell";
const COLLECT_FOR: Duration = Duration::from_millis(250);
const BUS_TIMEOUT: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy)]
pub(crate) enum Action {
    Open,
    OpenInbox,
    Archive,
    Read,
}

pub(crate) struct Invoked {
    pub action: Action,
    pub messages: Vec<MessageId>,
    pub token: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Single,
    PrivateSingle,
    Group,
}

impl Kind {
    fn buttons(self) -> &'static [&'static str] {
        match self {
            Self::Single => &[
                "default",
                "Open",
                "archive",
                "Archive",
                "mark-read",
                "Mark as read",
            ],
            Self::PrivateSingle => &["default", "Open"],
            Self::Group => &["default", "Open Inbox"],
        }
    }

    fn action(self, key: &str) -> Option<Action> {
        match (self, key) {
            (Self::Group, "default") => Some(Action::OpenInbox),
            (_, "default") => Some(Action::Open),
            (Self::Single, "archive") => Some(Action::Archive),
            (Self::Single, "mark-read") => Some(Action::Read),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Notice {
    kind: Kind,
    title: String,
    body: String,
    messages: Vec<MessageId>,
}

enum Command {
    Committed(Instant, Vec<InboxArrival>),
    PassEnded,
    Focused,
    Refresh,
    SetMode(NotificationMode),
}

#[derive(Clone)]
pub(crate) struct Queue {
    commands: UnboundedSender<Command>,
    viewing_inbox: Arc<AtomicBool>,
}

impl Queue {
    pub fn committed(&self, arrivals: fell_core::Result<Vec<InboxArrival>>) {
        match arrivals {
            Ok(arrivals) if !arrivals.is_empty() && !self.viewing_inbox.load(Ordering::Relaxed) => {
                self.send(Command::Committed(Instant::now(), arrivals));
            }
            Ok(_) => {}
            Err(err) => report_failure(&err),
        }
    }

    pub fn pass_ended(&self) {
        self.send(Command::PassEnded);
    }

    pub fn set_viewing(&self, viewing: bool) {
        if self.viewing_inbox.load(Ordering::Relaxed) != viewing {
            self.viewing_inbox.store(viewing, Ordering::Relaxed);
            if viewing {
                self.send(Command::Focused);
            }
        }
    }

    pub fn refresh(&self) {
        self.send(Command::Refresh);
    }

    pub fn set_mode(&self, mode: NotificationMode) {
        self.send(Command::SetMode(mode));
    }

    fn send(&self, command: Command) {
        let _ = self.commands.unbounded_send(command);
    }
}

pub(crate) fn start(
    viewing: bool,
    mode: NotificationMode,
    store: StoreClient,
    account: AccountId,
) -> (Queue, UnboundedReceiver<Invoked>) {
    let (commands, incoming) = unbounded();
    let (actions, invoked) = unbounded();
    let viewing_inbox = Arc::new(AtomicBool::new(viewing));
    let focused = Arc::clone(&viewing_inbox);
    if let Err(err) = std::thread::Builder::new()
        .name("fell-notifications".into())
        .spawn(move || {
            futures::executor::block_on(run(incoming, actions, focused, mode, store, account))
        })
    {
        report_failure(&err);
    }
    (
        Queue {
            commands,
            viewing_inbox,
        },
        invoked,
    )
}

fn report_failure(err: &impl std::fmt::Display) {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        eprintln!("fell: desktop notification failed: {err}");
    }
}

enum Event {
    Command(Option<Command>),
    Signal(Option<Message>),
    Deadline,
}

async fn next_event(
    bus: &mut Option<Bus>,
    incoming: &mut UnboundedReceiver<Command>,
    deadline: Option<Instant>,
) -> Event {
    futures::select_biased! {
        signal = next_signal(bus).fuse() => Event::Signal(signal),
        // A daemon call can take longer than the window. Collect all
        // commits already queued before honouring an expired deadline.
        command = incoming.next().fuse() => Event::Command(command),
        () = wait_until(deadline).fuse() => Event::Deadline,
    }
}

async fn run(
    mut incoming: UnboundedReceiver<Command>,
    actions: UnboundedSender<Invoked>,
    viewing: Arc<AtomicBool>,
    mut mode: NotificationMode,
    store: StoreClient,
    account: AccountId,
) {
    let mut bus = None;
    let mut batch = Vec::new();
    let mut deadline = None;
    loop {
        let event = next_event(&mut bus, &mut incoming, deadline).await;
        match event {
            Event::Command(Some(Command::Committed(at, mut arrivals))) => {
                if mode != NotificationMode::Off && !viewing.load(Ordering::Relaxed) {
                    deadline.get_or_insert(at + COLLECT_FOR);
                    batch.append(&mut arrivals);
                }
                continue;
            }
            Event::Command(Some(Command::Refresh)) => {
                if let Some(bus) = &mut bus {
                    match unread_messages(&store, &account, Some(bus), &[]).await {
                        Ok(unread) => bus.close_stale(&unread).await,
                        Err(err) => report_failure(&err),
                    }
                }
                continue;
            }
            Event::Command(Some(Command::SetMode(next))) if mode == next => continue,
            Event::Command(Some(command @ (Command::Focused | Command::SetMode(_)))) => {
                if let Command::SetMode(next) = command {
                    mode = next;
                }
                deadline = None;
                batch.clear();
                if let Some(bus) = &mut bus {
                    bus.close(|_| true).await;
                }
                continue;
            }
            Event::Command(Some(Command::PassEnded)) | Event::Deadline => {}
            Event::Command(None) => break,
            Event::Signal(Some(signal)) => {
                if let Some(bus) = &mut bus
                    && let Some((active, action)) = bus.on_signal(&signal)
                {
                    let _ = actions.unbounded_send(Invoked {
                        action,
                        messages: active.messages,
                        token: active.token,
                    });
                    // Some daemons leave a popup visible after invoking an action.
                    bus.close_notification(&active.owner, active.id).await;
                }
                continue;
            }
            Event::Signal(None) => {
                bus = None;
                continue;
            }
        }
        deadline = None;
        let mut arrivals = std::mem::take(&mut batch);
        let unread = match unread_messages(&store, &account, bus.as_ref(), &arrivals).await {
            Ok(unread) => unread,
            Err(err) => {
                report_failure(&err);
                continue;
            }
        };
        if let Some(bus) = &mut bus {
            bus.close_stale(&unread).await;
        }
        arrivals.retain(|arrival| unread.contains(&arrival.id));
        if arrivals.is_empty() || viewing.load(Ordering::Relaxed) {
            continue;
        }
        let result = async {
            if bus.is_none() {
                bus = Some(connect().await?);
            }
            if let Some(bus) = &mut bus {
                bus.deliver(arrivals, mode, &viewing).await?;
            }
            Ok::<_, zbus::Error>(())
        }
        .await;
        if let Err(err) = result {
            report_failure(&err);
        }
    }
    if let Some(bus) = &mut bus {
        bus.close(|_| true).await;
    }
}

async fn unread_messages(
    store: &StoreClient,
    account: &AccountId,
    bus: Option<&Bus>,
    arrivals: &[InboxArrival],
) -> fell_core::Result<HashSet<MessageId>> {
    let ids = bus
        .into_iter()
        .flat_map(|bus| bus.active.iter().flat_map(|notice| &notice.messages))
        .chain(arrivals.iter().map(|arrival| &arrival.id))
        .cloned()
        .collect::<HashSet<_>>();
    if ids.is_empty() {
        return Ok(HashSet::new());
    }
    let ids = ids.into_iter().collect::<Vec<_>>();
    let account = account.clone();
    let unread = store
        .call(move |store| store.unread_inbox_messages(&account, &ids))
        .await
        .map_err(io::Error::other)??;
    Ok(unread.into_iter().collect())
}

async fn next_signal(bus: &mut Option<Bus>) -> Option<Message> {
    match bus {
        Some(bus) => bus.signals.next().await,
        None => pending().await,
    }
}

async fn wait_until(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        Timer::at(deadline).await;
    } else {
        pending::<()>().await;
    }
}

fn notices(mut arrivals: Vec<InboxArrival>, mode: NotificationMode, markup: bool) -> Vec<Notice> {
    let private = mode == NotificationMode::Private;
    let count = arrivals.len();
    if count > 3 || (private && count > 1) {
        arrivals.sort_by_key(|arrival| std::cmp::Reverse(arrival.date_unix));
        return vec![Notice {
            kind: Kind::Group,
            title: format!("{count} new messages"),
            body: if private {
                String::new()
            } else {
                body_text(&group_senders(&arrivals), markup)
            },
            messages: arrivals.into_iter().map(|arrival| arrival.id).collect(),
        }];
    }
    arrivals
        .into_iter()
        .map(|arrival| {
            let (title, body) = if private {
                ("New mail".into(), String::new())
            } else {
                let sender = clean_line(&arrival.sender, 140);
                let subject = clean_line(&arrival.subject, 160);
                let preview = clean_line(&arrival.snippet, 140);
                let mut body = body_text(&subject, markup);
                if markup && !body.is_empty() {
                    body = format!("<b>{body}</b>");
                }
                if !preview.is_empty() {
                    if !body.is_empty() {
                        body.push('\n');
                    }
                    body.push_str(&body_text(&preview, markup));
                }
                (
                    if sender.is_empty() {
                        "New mail".into()
                    } else {
                        sender
                    },
                    body,
                )
            };
            Notice {
                kind: if private {
                    Kind::PrivateSingle
                } else {
                    Kind::Single
                },
                title,
                body,
                messages: vec![arrival.id],
            }
        })
        .collect()
}

fn group_senders(arrivals: &[InboxArrival]) -> String {
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    for arrival in arrivals {
        let name = clean_line(&arrival.sender, 140);
        if !name.is_empty() && seen.insert(name.clone()) {
            names.push(name);
        }
    }
    match names.as_slice() {
        [] => String::new(),
        [first] => first.clone(),
        [first, second] => format!("{first} and {second}"),
        [first, second, rest @ ..] => {
            let others = if rest.len() == 1 {
                "1 other".into()
            } else {
                format!("{} others", rest.len())
            };
            format!("{first}, {second} and {others}")
        }
    }
}

fn clean_line(text: &str, limit: usize) -> String {
    let first = text
        .split(|ch| {
            matches!(
                break_property(ch as u32),
                BreakClass::Mandatory
                    | BreakClass::CarriageReturn
                    | BreakClass::LineFeed
                    | BreakClass::NextLine
            ) || matches!(ch, '\u{1c}'..='\u{1e}')
        })
        .next()
        .unwrap_or_default();
    let filtered: String = first
        .chars()
        .filter(|ch| {
            (!ch.is_control() || *ch == '\t') && ch.general_category() != GeneralCategory::Format
        })
        .collect();
    crate::present::one_line(&filtered)
        .chars()
        .take(limit)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

fn body_text(text: &str, markup: bool) -> String {
    if markup {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    } else {
        text.to_owned()
    }
}

struct Bus {
    proxy: Proxy<'static>,
    signals: SignalStream<'static>,
    active: VecDeque<Active>,
    icon: String,
}

struct Active {
    id: u32,
    // A daemon that restarts can immediately reuse an ID.
    owner: String,
    kind: Kind,
    messages: Vec<MessageId>,
    token: Option<String>,
}

async fn connect() -> zbus::Result<Bus> {
    let open = async {
        let conn = Builder::session()?
            .method_timeout(BUS_TIMEOUT)
            .build()
            .await?;
        let proxy = Proxy::new_owned(conn, SERVICE, PATH, SERVICE).await?;
        let signals = proxy.receive_all_signals().await?;
        Ok(Bus {
            proxy,
            signals,
            active: VecDeque::new(),
            icon: installed_icon(),
        })
    };
    futures::select_biased! {
        bus = open.fuse() => bus,
        _ = FutureExt::fuse(Timer::after(BUS_TIMEOUT)) => Err(zbus::Error::Failure("session bus timed out".into())),
    }
}

fn installed_icon() -> String {
    let user = std::env::var_os("XDG_DATA_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    let app = std::env::var_os("APPDIR").map(|dir| PathBuf::from(dir).join("usr/share"));
    let system = std::env::var_os("XDG_DATA_DIRS")
        .filter(|paths| !paths.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    for dir in app
        .into_iter()
        .chain(user)
        .chain(std::env::split_paths(&system))
    {
        let apps = std::fs::read_dir(dir.join("icons/hicolor"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|size| size.path().join("apps"));
        for icons in apps.chain(std::iter::once(dir.join("pixmaps"))) {
            for name in [
                "dev.predmore.fell.png",
                "dev.predmore.fell.svg",
                "dev.predmore.fell.xpm",
            ] {
                let icon = icons.join(name);
                if icon.is_absolute() && icon.is_file() {
                    return icon.to_string_lossy().into_owned();
                }
            }
        }
    }
    "mail-unread".into()
}

impl Bus {
    async fn close_stale(&mut self, unread: &HashSet<MessageId>) {
        self.close(|notice| notice.messages.iter().any(|id| !unread.contains(id)))
            .await;
    }

    async fn close(&mut self, should_close: impl Fn(&Active) -> bool) {
        let mut closed = Vec::new();
        self.active.retain(|notice| {
            if should_close(notice) {
                closed.push((notice.owner.clone(), notice.id));
                false
            } else {
                true
            }
        });
        join_all(
            closed
                .iter()
                .map(|(owner, id)| self.close_notification(owner, *id)),
        )
        .await;
    }

    async fn close_notification(&self, owner: &str, id: u32) {
        // Send to the daemon that issued this ID, even after a restart.
        let _ = self
            .proxy
            .connection()
            .call_method(
                Some(owner),
                PATH,
                Some(SERVICE),
                "CloseNotification",
                &(id,),
            )
            .await;
    }

    async fn deliver(
        &mut self,
        arrivals: Vec<InboxArrival>,
        mode: NotificationMode,
        viewing: &AtomicBool,
    ) -> zbus::Result<()> {
        let reply = self.proxy.call_method("GetCapabilities", &()).await?;
        let owner = reply
            .header()
            .sender()
            .map(ToString::to_string)
            .ok_or_else(|| zbus::Error::Failure("notification reply had no sender".into()))?;
        let capabilities: Vec<String> = reply.body().deserialize()?;
        let markup = capabilities
            .iter()
            .any(|capability| capability == "body-markup");
        if viewing.load(Ordering::Relaxed) {
            return Ok(());
        }

        self.active.retain(|old| old.owner == owner);
        let proxy = Proxy::new_owned(
            self.proxy.connection().clone(),
            owner.clone(),
            PATH,
            SERVICE,
        )
        .await?;
        let replaces = self
            .active
            .iter()
            .find(|notice| notice.kind == Kind::Group)
            .map_or(0, |notice| notice.id);
        let results = join_all(notices(arrivals, mode, markup).into_iter().map(|notice| {
            let replaces = if notice.kind == Kind::Group {
                replaces
            } else {
                0
            };
            post(&proxy, &self.icon, &owner, notice, replaces)
        }))
        .await;
        let mut failure = Ok(());
        for result in results {
            match result {
                Ok(active) => {
                    let replaced = if active.kind == Kind::Group {
                        replaces
                    } else {
                        0
                    };
                    self.active
                        .retain(|old| old.id != active.id && old.id != replaced);
                    const MAX_ACTIVE: usize = 128;
                    if self.active.len() == MAX_ACTIVE {
                        let oldest = self.active[0].id;
                        self.close(|notice| notice.id == oldest).await;
                    }
                    self.active.push_back(active);
                }
                Err(err) => failure = Err(err),
            }
        }
        failure
    }

    fn on_signal(&mut self, signal: &Message) -> Option<(Active, Action)> {
        let header = signal.header();
        let owner = header.sender()?.as_str();
        let position = |id| {
            self.active
                .iter()
                .position(|notice| notice.id == id && notice.owner == owner)
        };
        match header.member()?.as_str() {
            "ActivationToken" => {
                let (id, token): (u32, String) = signal.body().deserialize().ok()?;
                let index = position(id)?;
                self.active[index].token = Some(token);
            }
            "NotificationClosed" => {
                let (id, _): (u32, u32) = signal.body().deserialize().ok()?;
                self.active.remove(position(id)?);
            }
            "ActionInvoked" => {
                let (id, key): (u32, String) = signal.body().deserialize().ok()?;
                let index = position(id)?;
                let action = self.active[index].kind.action(&key)?;
                return Some((self.active.remove(index)?, action));
            }
            _ => {}
        }
        None
    }
}

async fn post(
    proxy: &Proxy<'_>,
    icon: &str,
    owner: &str,
    notice: Notice,
    replaces: u32,
) -> zbus::Result<Active> {
    let hints = HashMap::from([
        ("desktop-entry", Value::from(APP_ID)),
        ("category", Value::from("email.arrived")),
    ]);
    let id = proxy
        .call(
            "Notify",
            &(
                "Fell",
                replaces,
                icon,
                notice.title,
                notice.body,
                notice.kind.buttons(),
                hints,
                -1i32,
            ),
        )
        .await?;
    Ok(Active {
        id,
        owner: owner.to_owned(),
        kind: notice.kind,
        messages: notice.messages,
        token: None,
    })
}

#[cfg(test)]
mod tests;
