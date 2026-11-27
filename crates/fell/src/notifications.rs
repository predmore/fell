//! Desktop notifications run independently of IMAP and of the window.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::pending;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use async_io::Timer;
use fell_core::{AccountId, InboxArrival, MessageId, NotificationMode, diagnostic};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::future::{BoxFuture, join_all};
use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};
use unicode_linebreak::{BreakClass, break_property};
use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};
use zbus::connection::Builder;
use zbus::names::UniqueName;
use zbus::proxy::{OwnerChangedStream, SignalStream};
use zbus::zvariant::Value;
use zbus::{Message, Proxy};

use crate::store_actor::StoreClient;

mod activation;
pub(crate) use activation::Activation;

const SERVICE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const APP_ID: &str = "dev.predmore.fell";
const COLLECT_FOR: Duration = Duration::from_millis(250);
const BUS_TIMEOUT: Duration = Duration::from_secs(1);
const FEEDBACK_TIMEOUT_MS: i32 = 5000;

#[derive(Debug, Clone, Copy)]
pub(crate) enum Action {
    Open,
    OpenInbox,
    Archive,
    Read,
    Undo(i64),
}

pub(crate) struct Invoked {
    pub action: Action,
    pub messages: Vec<MessageId>,
    pub subject: Option<String>,
    pub token: Option<String>,
    pub receipt: Receipt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Single,
    Group,
    Private,
    Feedback(Option<i64>),
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
            Self::Group | Self::Private => &["default", "Open Inbox"],
            Self::Feedback(Some(_)) => &["default", "Undo", "undo", "Undo"],
            Self::Feedback(None) => &[],
        }
    }

    fn action(self, key: &str) -> Option<Action> {
        match (self, key) {
            (Self::Group | Self::Private, "default") => Some(Action::OpenInbox),
            (Self::Single, "default") => Some(Action::Open),
            (Self::Single, "archive") => Some(Action::Archive),
            (Self::Single, "mark-read") => Some(Action::Read),
            (Self::Feedback(Some(id)), "default" | "undo") => Some(Action::Undo(id)),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Notice {
    kind: Kind,
    title: String,
    body: String,
    messages: Vec<InboxArrival>,
}

pub(crate) struct Confirmation {
    pub action_id: i64,
    pub subject: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Receipt {
    owner: String,
    id: u32,
    mode_generation: u64,
}

#[cfg(test)]
impl Receipt {
    pub(crate) fn fixture() -> Self {
        Self {
            owner: ":1.1".into(),
            id: 1,
            mode_generation: 0,
        }
    }
}

enum Command {
    Committed(Instant, Vec<InboxArrival>),
    PassEnded,
    Focused,
    Refresh,
    SetMode(NotificationMode),
    Feedback(Receipt, String, Option<Confirmation>),
    Dismiss(Receipt),
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
            Err(err) => report_failure(Failure::Lookup, &err),
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

    pub fn feedback(&self, receipt: Receipt, text: String, confirmation: Option<Confirmation>) {
        self.send(Command::Feedback(receipt, text, confirmation));
    }

    pub fn dismiss(&self, receipt: Receipt) {
        self.send(Command::Dismiss(receipt));
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
            futures::executor::block_on(run(None, incoming, actions, focused, mode, store, account))
        })
    {
        report_failure(Failure::Start, &err);
    }
    (
        Queue {
            commands,
            viewing_inbox,
        },
        invoked,
    )
}

#[repr(u8)]
enum Failure {
    Start = 1 << 0,
    Lookup = 1 << 1,
    Connect = 1 << 2,
    Post = 1 << 3,
    Close = 1 << 4,
}

fn report_failure(kind: Failure, err: &(impl std::error::Error + 'static)) {
    static LOGGED: AtomicU8 = AtomicU8::new(0);
    let bit = kind as u8;
    if LOGGED.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
        diagnostic!(
            WARN,
            "Desktop notification failed",
            error = err as &dyn std::error::Error
        );
    }
}

enum Event {
    Command(Option<Command>),
    Bus(BusEvent),
    Deadline,
}

enum BusEvent {
    Signal(Option<Message>),
    Owner(Option<Option<UniqueName<'static>>>),
    Posted(PostResult),
}

async fn next_event(
    bus: &mut Option<Bus>,
    incoming: &mut UnboundedReceiver<Command>,
    deadline: Option<Instant>,
) -> Event {
    let deadline = if bus.as_ref().is_some_and(|bus| !bus.posts.is_empty()) {
        None
    } else {
        deadline
    };
    futures::select_biased! {
        event = next_bus_event(bus).fuse() => Event::Bus(event),
        // Collect queued commits before honouring an expired deadline.
        command = incoming.next().fuse() => Event::Command(command),
        () = wait_until(deadline).fuse() => Event::Deadline,
    }
}

async fn next_bus_event(bus: &mut Option<Bus>) -> BusEvent {
    let Some(bus) = bus else {
        return pending().await;
    };
    futures::select_biased! {
        owner = bus.owners.next().fuse() => BusEvent::Owner(owner),
        post = next_post(&mut bus.posts).fuse() => BusEvent::Posted(post),
        signal = bus.signals.next().fuse() => BusEvent::Signal(signal),
    }
}

async fn next_post(posts: &mut FuturesUnordered<BoxFuture<'static, PostResult>>) -> PostResult {
    if posts.is_empty() {
        pending().await
    } else {
        posts.next().await.expect("pending notification call")
    }
}

async fn run(
    mut bus: Option<Bus>,
    mut incoming: UnboundedReceiver<Command>,
    actions: UnboundedSender<Invoked>,
    viewing: Arc<AtomicBool>,
    mut mode: NotificationMode,
    store: StoreClient,
    account: AccountId,
) {
    let mut batch = Vec::new();
    let mut deadline = None;
    let mut mode_generation = 0;
    let mut mail_generation = 0;
    let mut refresh_after_post = false;
    loop {
        let event = next_event(&mut bus, &mut incoming, deadline).await;
        match event {
            Event::Command(Some(Command::Committed(at, mut arrivals))) => {
                if mode != NotificationMode::Off && !viewing.load(Ordering::Relaxed) {
                    deadline.get_or_insert(at + COLLECT_FOR);
                    batch.append(&mut arrivals);
                }
            }
            Event::Command(Some(command @ (Command::Refresh | Command::PassEnded))) => {
                if matches!(command, Command::PassEnded) && deadline.is_some() {
                    deadline = Some(Instant::now());
                }
                // A pass can remove or change mail without reporting any arrivals.
                if let Some(bus) = &mut bus {
                    refresh_after_post |= !bus.posts.is_empty();
                    refresh_notices(&store, &account, bus).await;
                }
            }
            Event::Command(Some(Command::SetMode(next))) if mode == next => {}
            Event::Command(Some(command @ (Command::Focused | Command::SetMode(_)))) => {
                let mode_changed = if let Command::SetMode(next) = command {
                    mode = next;
                    mode_generation += 1;
                    true
                } else {
                    false
                };
                mail_generation += 1;
                deadline = None;
                batch.clear();
                if let Some(bus) = &mut bus {
                    bus.close(|notice| {
                        mode_changed || (!notice.kind.is_feedback() && !notice.invoked)
                    })
                    .await;
                }
            }
            Event::Command(Some(Command::Feedback(receipt, title, confirmation))) => {
                if receipt.mode_generation == mode_generation
                    && let Some(bus) = &mut bus
                    && bus.is_owner(&receipt.owner)
                {
                    bus.enqueue(
                        feedback_notice(
                            &title,
                            confirmation,
                            mode,
                            bus.daemon.as_ref().is_some_and(|daemon| daemon.markup),
                        ),
                        receipt.id,
                        Vec::new(),
                        mode_generation,
                        mail_generation,
                    );
                }
            }
            Event::Command(Some(Command::Dismiss(receipt))) => {
                if let Some(bus) = &mut bus
                    && bus.is_owner(&receipt.owner)
                {
                    bus.close(|notice| notice.receipt == receipt).await;
                }
            }
            Event::Command(None) => break,
            Event::Bus(BusEvent::Owner(Some(owner))) => {
                if let Some(bus) = &mut bus {
                    bus.owner_changed(owner.as_deref());
                }
            }
            Event::Bus(BusEvent::Signal(None) | BusEvent::Owner(None)) => bus = None,
            Event::Bus(BusEvent::Signal(Some(signal))) => {
                if let Some(bus) = &mut bus {
                    bus.on_signal(signal, &actions, mode_generation);
                }
            }
            Event::Bus(BusEvent::Posted(post)) => {
                if let Some(bus) = &mut bus {
                    bus.finish_post(post, mode_generation, mail_generation)
                        .await;
                    if bus.posts.is_empty() && std::mem::take(&mut refresh_after_post) {
                        refresh_notices(&store, &account, bus).await;
                    }
                    for signal in std::mem::take(&mut bus.early_signals) {
                        bus.on_signal(signal, &actions, mode_generation);
                    }
                }
            }
            Event::Deadline => {
                deadline = None;
                let mut arrivals = std::mem::take(&mut batch);
                let unread = match unread_messages(&store, &account, bus.as_ref(), &arrivals).await
                {
                    Ok(unread) => unread,
                    Err(err) => {
                        report_failure(Failure::Lookup, &err);
                        continue;
                    }
                };
                arrivals.retain(|arrival| unread.contains(&arrival.id));
                if let Some(bus) = &mut bus {
                    bus.close_stale(&unread).await;
                }
                if arrivals.is_empty() || viewing.load(Ordering::Relaxed) {
                    continue;
                }
                let result = async {
                    if bus.is_none() {
                        bus = Some(connect().await?);
                    }
                    if let Some(bus) = &mut bus {
                        bus.prepare_delivery().await?;
                        if !viewing.load(Ordering::Relaxed) {
                            bus.deliver(arrivals, &unread, mode, mode_generation, mail_generation);
                        }
                    }
                    Ok::<_, zbus::Error>(())
                }
                .await;
                if let Err(err) = result {
                    report_failure(Failure::Connect, &err);
                }
            }
        }
    }
    if let Some(bus) = &mut bus {
        bus.close(|_| true).await;
    }
}

async fn refresh_notices(store: &StoreClient, account: &AccountId, bus: &mut Bus) {
    match unread_messages(store, account, Some(bus), &[]).await {
        Ok(unread) => bus.close_stale(&unread).await,
        Err(err) => report_failure(Failure::Lookup, &err),
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
        .chain(arrivals)
        .map(|arrival| &arrival.id)
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

async fn wait_until(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        Timer::at(deadline).await;
    } else {
        pending::<()>().await;
    }
}

fn aggregate(mode: NotificationMode, count: usize) -> bool {
    mode == NotificationMode::Private || count > 3
}

fn feedback_notice(
    title: &str,
    confirmation: Option<Confirmation>,
    mode: NotificationMode,
    markup: bool,
) -> Notice {
    let mut body = String::new();
    if let Some(confirmation) = &confirmation {
        if mode == NotificationMode::On
            && let Some(subject) = &confirmation.subject
        {
            body = body_text(&clean_line(subject, 160), markup);
            if !body.is_empty() {
                body.push('\n');
            }
        }
        body.push_str("Click to undo");
    }
    Notice {
        kind: Kind::Feedback(confirmation.map(|value| value.action_id)),
        title: clean_line(title, 160),
        body,
        messages: Vec::new(),
    }
}

fn notices(mut arrivals: Vec<InboxArrival>, mode: NotificationMode, markup: bool) -> Vec<Notice> {
    if aggregate(mode, arrivals.len()) {
        let private = mode == NotificationMode::Private;
        arrivals.sort_by_key(|arrival| std::cmp::Reverse(arrival.date_unix));
        let count = arrivals.len();
        return vec![Notice {
            kind: if private { Kind::Private } else { Kind::Group },
            title: if count == 1 {
                "1 new message".into()
            } else {
                format!("{count} new messages")
            },
            body: if private {
                String::new()
            } else {
                body_text(&group_senders(&arrivals), markup)
            },
            messages: arrivals,
        }];
    }
    arrivals
        .into_iter()
        .map(|arrival| {
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
            Notice {
                kind: Kind::Single,
                title: if sender.is_empty() {
                    "New mail".into()
                } else {
                    sender
                },
                body,
                messages: vec![arrival],
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
    owners: OwnerChangedStream<'static>,
    daemon: Option<Daemon>,
    active: VecDeque<Active>,
    posts: FuturesUnordered<BoxFuture<'static, PostResult>>,
    early_signals: Vec<Message>,
    icon: String,
}

struct Daemon {
    owner: String,
    markup: bool,
}

struct Active {
    receipt: Receipt,
    kind: Kind,
    messages: Vec<InboxArrival>,
    token: Option<String>,
    close_pending: bool,
    invoked: bool,
}

struct PostResult {
    result: zbus::Result<Active>,
    replaces: u32,
    fold: Vec<u32>,
    mail_generation: u64,
}

impl Kind {
    fn is_feedback(self) -> bool {
        matches!(self, Self::Feedback(_))
    }
}

async fn bus_timeout<T>(call: impl Future<Output = zbus::Result<T>>) -> zbus::Result<T> {
    futures::select_biased! {
        reply = call.fuse() => reply,
        _ = FutureExt::fuse(Timer::after(BUS_TIMEOUT)) => Err(zbus::Error::Failure("session bus timed out".into())),
    }
}

async fn connect() -> zbus::Result<Bus> {
    connect_to(Builder::session()?).await
}

async fn connect_to(builder: Builder<'_>) -> zbus::Result<Bus> {
    bus_timeout(async {
        // Notify has no client-side reply deadline. The daemon can display
        // the popup before it replies with the only ID we can use to close it.
        let conn = builder.build().await?;
        let proxy = Proxy::new_owned(conn, SERVICE, PATH, SERVICE).await?;
        let owners = proxy.receive_owner_changed().await?;
        let signals = proxy.receive_all_signals().await?;
        Ok(Bus {
            proxy,
            signals,
            owners,
            daemon: None,
            active: VecDeque::new(),
            posts: FuturesUnordered::new(),
            early_signals: Vec::new(),
            icon: installed_icon(),
        })
    })
    .await
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
    fn is_owner(&self, owner: &str) -> bool {
        self.daemon
            .as_ref()
            .is_some_and(|daemon| daemon.owner == owner)
    }

    fn owner_changed(&mut self, owner: Option<&str>) {
        if self.daemon.as_ref().map(|daemon| daemon.owner.as_str()) != owner {
            self.daemon = None;
            self.active.clear();
            self.posts.clear();
            self.early_signals.clear();
        }
    }

    async fn prepare_delivery(&mut self) -> zbus::Result<()> {
        if self.daemon.is_none() {
            let reply = bus_timeout(self.proxy.call_method("GetCapabilities", &())).await?;
            let owner = reply
                .header()
                .sender()
                .map(ToString::to_string)
                .ok_or_else(|| zbus::Error::Failure("notification reply had no sender".into()))?;
            let capabilities: Vec<String> = reply.body().deserialize()?;
            self.daemon = Some(Daemon {
                owner,
                markup: capabilities
                    .iter()
                    .any(|capability| capability == "body-markup"),
            });
        }
        Ok(())
    }

    async fn close_stale(&mut self, unread: &HashSet<MessageId>) {
        self.close(|notice| {
            !notice.invoked
                && !notice.kind.is_feedback()
                && notice
                    .messages
                    .iter()
                    .all(|mail| !unread.contains(&mail.id))
        })
        .await;
    }

    async fn close(&mut self, should_close: impl Fn(&Active) -> bool) {
        let mut closing = Vec::new();
        for notice in &mut self.active {
            if notice.close_pending || should_close(notice) {
                notice.close_pending = true;
                closing.push(notice.receipt.clone());
            }
        }
        let results = join_all(
            closing
                .iter()
                .map(|receipt| self.close_notification(receipt)),
        )
        .await;
        for (receipt, result) in closing.into_iter().zip(results) {
            match result {
                Ok(()) => self.active.retain(|old| old.receipt != receipt),
                Err(err) => report_failure(Failure::Close, &err),
            }
        }
    }

    async fn close_notification(&self, receipt: &Receipt) -> zbus::Result<()> {
        let close = async || {
            // Send to the daemon that issued this ID, even after a restart.
            bus_timeout(self.proxy.connection().call_method(
                Some(receipt.owner.as_str()),
                PATH,
                Some(SERVICE),
                "CloseNotification",
                &(receipt.id,),
            ))
            .await
            .map(|_| ())
        };
        if close().await.is_ok() {
            return Ok(());
        }
        close().await
    }

    fn deliver(
        &mut self,
        mut arrivals: Vec<InboxArrival>,
        unread: &HashSet<MessageId>,
        mode: NotificationMode,
        mode_generation: u64,
        mail_generation: u64,
    ) {
        let mut replaces = 0;
        let mut fold = Vec::new();
        if aggregate(mode, arrivals.len()) {
            let mut ids = arrivals
                .iter()
                .map(|mail| mail.id.clone())
                .collect::<HashSet<_>>();
            for old in &self.active {
                if old.invoked || old.kind.is_feedback() {
                    continue;
                }
                if matches!(old.kind, Kind::Group | Kind::Private)
                    && old.receipt.mode_generation == mode_generation
                {
                    replaces = old.receipt.id;
                } else {
                    fold.push(old.receipt.id);
                }
                if old.close_pending {
                    continue;
                }
                arrivals.extend(
                    old.messages
                        .iter()
                        .filter(|mail| unread.contains(&mail.id) && ids.insert(mail.id.clone()))
                        .cloned(),
                );
            }
        }
        let markup = self.daemon.as_ref().is_some_and(|daemon| daemon.markup);
        for notice in notices(arrivals, mode, markup) {
            self.enqueue(
                notice,
                replaces,
                std::mem::take(&mut fold),
                mode_generation,
                mail_generation,
            );
        }
    }

    fn enqueue(
        &mut self,
        notice: Notice,
        replaces: u32,
        fold: Vec<u32>,
        mode_generation: u64,
        mail_generation: u64,
    ) {
        let Some(daemon) = &self.daemon else { return };
        let owner = daemon.owner.clone();
        let conn = self.proxy.connection().clone();
        let icon = self.icon.clone();
        self.posts.push(
            async move {
                let result = post(&conn, &icon, owner, notice, replaces, mode_generation).await;
                PostResult {
                    result,
                    replaces,
                    fold,
                    mail_generation,
                }
            }
            .boxed(),
        );
    }

    async fn finish_post(&mut self, post: PostResult, mode_generation: u64, mail_generation: u64) {
        let active = match post.result {
            Ok(active) => active,
            Err(err) => {
                report_failure(Failure::Post, &err);
                // A failed feedback replacement leaves the acted-on popup to close.
                self.close(|old| old.receipt.id == post.replaces && old.invoked)
                    .await;
                return;
            }
        };
        let id = active.receipt.id;
        let expired = active.receipt.mode_generation != mode_generation
            || (!active.kind.is_feedback() && post.mail_generation != mail_generation);
        self.active.retain(|old| old.receipt.id != id);
        const MAX_ACTIVE: usize = 128;
        let oldest = (self.active.len() >= MAX_ACTIVE).then(|| self.active[0].receipt.id);
        self.active.push_back(active);
        self.close(|notice| {
            let id_to_close = notice.receipt.id;
            (expired && id_to_close == id)
                || post.fold.contains(&id_to_close)
                || (post.replaces != id && id_to_close == post.replaces)
                || oldest == Some(id_to_close)
        })
        .await;
    }

    fn on_signal(
        &mut self,
        signal: Message,
        actions: &UnboundedSender<Invoked>,
        mode_generation: u64,
    ) {
        let header = signal.header();
        let Some(owner) = header.sender().map(|owner| owner.as_str()) else {
            return;
        };
        let Some(member) = header.member().map(|member| member.as_str()) else {
            return;
        };
        if !self.is_owner(owner) {
            return;
        }
        let (id, value) = match member {
            "ActivationToken" | "ActionInvoked" => {
                let Ok((id, value)) = signal.body().deserialize::<(u32, String)>() else {
                    return;
                };
                (id, value)
            }
            "NotificationClosed" => {
                let Ok((id, _)) = signal.body().deserialize::<(u32, u32)>() else {
                    return;
                };
                (id, String::new())
            }
            _ => return,
        };
        let Some(index) = self.active.iter().position(|old| old.receipt.id == id) else {
            // The daemon can emit signals as soon as it displays a popup.
            if !self.posts.is_empty() && self.early_signals.len() < 128 {
                self.early_signals.push(signal);
            }
            return;
        };
        let active = &mut self.active[index];
        match member {
            "NotificationClosed" => {
                self.active.remove(index);
            }
            "ActivationToken" => active.token = Some(value),
            // Signals are handled after close() returns. If it failed, the
            // visible notice still works while its close is queued for retry.
            "ActionInvoked"
                if !active.invoked && active.receipt.mode_generation == mode_generation =>
            {
                if let Some(action) = active.kind.action(&value) {
                    active.invoked = true;
                    let _ = actions.unbounded_send(Invoked {
                        action,
                        messages: active.messages.iter().map(|mail| mail.id.clone()).collect(),
                        subject: active.messages.first().map(|mail| mail.subject.clone()),
                        token: active.token.take(),
                        receipt: active.receipt.clone(),
                    });
                }
            }
            _ => {}
        }
    }
}

async fn post(
    conn: &zbus::Connection,
    icon: &str,
    owner: String,
    notice: Notice,
    replaces: u32,
    mode_generation: u64,
) -> zbus::Result<Active> {
    let mut hints = HashMap::from([("desktop-entry", Value::from(APP_ID))]);
    let timeout = if notice.kind.is_feedback() {
        hints.insert("transient", Value::from(true));
        FEEDBACK_TIMEOUT_MS
    } else {
        hints.insert("category", Value::from("email.arrived"));
        -1
    };
    let reply = conn
        .call_method(
            Some(owner.as_str()),
            PATH,
            Some(SERVICE),
            "Notify",
            &(
                "Fell",
                replaces,
                icon,
                notice.title,
                notice.body,
                notice.kind.buttons(),
                hints,
                timeout,
            ),
        )
        .await?;
    Ok(Active {
        receipt: Receipt {
            id: reply.body().deserialize()?,
            owner,
            mode_generation,
        },
        kind: notice.kind,
        messages: notice.messages,
        token: None,
        close_pending: false,
        invoked: false,
    })
}

#[cfg(test)]
mod tests;
