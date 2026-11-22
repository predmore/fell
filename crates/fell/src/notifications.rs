//! Desktop notifications run independently of IMAP and of the window.

use std::collections::{HashMap, VecDeque};
use std::future::pending;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_io::Timer;
use fell_core::{InboxArrival, MessageId};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::{FutureExt, StreamExt, future::join_all};
use zbus::connection::Builder;
use zbus::proxy::SignalStream;
use zbus::zvariant::Value;
use zbus::{Message, Proxy};

mod activation;
pub(crate) use activation::Activation;

const SERVICE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const COLLECT_FOR: Duration = Duration::from_millis(250);
const BUS_TIMEOUT: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy)]
pub(crate) enum Action {
    Open,
    Archive,
    Read,
}

pub(crate) struct Invoked {
    pub action: Action,
    pub messages: Vec<MessageId>,
    pub token: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct Notice {
    title: String,
    body: String,
    messages: Vec<MessageId>,
}

enum Command {
    Committed(Instant, Vec<InboxArrival>),
    PassEnded,
}

pub(crate) struct Queue {
    commands: UnboundedSender<Command>,
    viewing_inbox: Arc<AtomicBool>,
}

impl Queue {
    pub fn committed(&self, arrivals: fell_core::Result<Vec<InboxArrival>>) {
        match arrivals {
            Ok(arrivals) if !arrivals.is_empty() && !self.viewing_inbox.load(Ordering::Relaxed) => {
                let _ = self
                    .commands
                    .unbounded_send(Command::Committed(Instant::now(), arrivals));
            }
            Ok(_) => {}
            Err(err) => report_failure(&err),
        }
    }

    pub fn pass_ended(&self) {
        let _ = self.commands.unbounded_send(Command::PassEnded);
    }
}

pub(crate) fn start(
    viewing_inbox: Arc<AtomicBool>,
    show_content: bool,
) -> (Queue, UnboundedReceiver<Invoked>) {
    let (commands, incoming) = unbounded();
    let (actions, invoked) = unbounded();
    if let Err(err) = std::thread::Builder::new()
        .name("fell-notifications".into())
        .spawn(move || futures::executor::block_on(run(incoming, actions, show_content)))
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

async fn run(
    mut incoming: UnboundedReceiver<Command>,
    actions: UnboundedSender<Invoked>,
    show_content: bool,
) {
    let mut bus = None;
    let mut batch = Vec::new();
    let mut deadline = None;
    loop {
        let event = futures::select_biased! {
            signal = next_signal(&mut bus).fuse() => Event::Signal(signal),
            () = wait_until(deadline).fuse() => Event::Deadline,
            command = incoming.next().fuse() => Event::Command(command),
        };
        match event {
            Event::Command(Some(Command::Committed(at, mut arrivals))) => {
                deadline.get_or_insert(at + COLLECT_FOR);
                batch.append(&mut arrivals);
                continue;
            }
            Event::Command(Some(Command::PassEnded)) | Event::Deadline => {}
            Event::Command(None) => break,
            Event::Signal(Some(signal)) => {
                if let Some(invoked) = bus.as_mut().and_then(|bus| bus.on_signal(&signal)) {
                    let _ = actions.unbounded_send(invoked);
                }
                continue;
            }
            Event::Signal(None) => {
                bus = None;
                continue;
            }
        }
        deadline = None;
        if batch.is_empty() {
            continue;
        }
        let arrivals = std::mem::take(&mut batch);
        let result = async {
            if bus.is_none() {
                bus = Some(connect().await?);
            }
            if let Some(bus) = &mut bus {
                deliver(bus, arrivals, show_content).await?;
            }
            Ok::<_, zbus::Error>(())
        }
        .await;
        if let Err(err) = result {
            report_failure(&err);
        }
    }
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

trait Notifier {
    async fn notify(&mut self, notices: Vec<Notice>) -> zbus::Result<()>;
}

async fn deliver(
    notifier: &mut impl Notifier,
    mut arrivals: Vec<InboxArrival>,
    show_content: bool,
) -> zbus::Result<()> {
    if arrivals.len() > 3 {
        arrivals.sort_by_key(|arrival| std::cmp::Reverse(arrival.date_unix));
        return notifier
            .notify(vec![Notice {
                title: if show_content {
                    format!("{} new messages", arrivals.len())
                } else {
                    "New mail".into()
                },
                body: String::new(),
                messages: arrivals.into_iter().map(|arrival| arrival.id).collect(),
            }])
            .await;
    }
    let notices = arrivals
        .into_iter()
        .map(|arrival| {
            let (title, body) = if show_content {
                let sender = clean_line(&arrival.sender);
                let subject = clean_line(&arrival.subject);
                let title = match (sender.is_empty(), subject.is_empty()) {
                    (true, true) => "New mail".into(),
                    (true, false) => subject,
                    (false, true) => sender,
                    (false, false) => format!("{sender} — {subject}"),
                };
                (title, escape_body(&clean_line(&arrival.snippet)))
            } else {
                ("New mail".into(), String::new())
            };
            Notice {
                title,
                body,
                messages: vec![arrival.id],
            }
        })
        .collect();
    notifier.notify(notices).await
}

fn clean_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|ch| {
            !ch.is_control() && !matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(160)
        .collect::<String>()
        .trim()
        .to_owned()
}

fn escape_body(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

struct Bus {
    proxy: Proxy<'static>,
    signals: SignalStream<'static>,
    // Notification IDs belong to the daemon that returned them. A restarted
    // daemon can reuse an ID even if a previous popup never sent Closed.
    active: VecDeque<Active>,
}

struct Active {
    id: u32,
    owner: String,
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
        })
    };
    futures::select_biased! {
        bus = open.fuse() => bus,
        _ = FutureExt::fuse(Timer::after(BUS_TIMEOUT)) => Err(zbus::Error::Failure("session bus timed out".into())),
    }
}

impl Notifier for Bus {
    async fn notify(&mut self, notices: Vec<Notice>) -> zbus::Result<()> {
        let results = join_all(notices.into_iter().map(|notice| post(&self.proxy, notice))).await;
        let mut failure = Ok(());
        for result in results {
            match result {
                Ok(active) => {
                    const MAX_ACTIVE: usize = 128;
                    self.active
                        .retain(|old| old.owner == active.owner && old.id != active.id);
                    if self.active.len() == MAX_ACTIVE {
                        self.active.pop_front();
                    }
                    self.active.push_back(active);
                }
                Err(err) => failure = Err(err),
            }
        }
        failure
    }
}

async fn post(proxy: &Proxy<'_>, notice: Notice) -> zbus::Result<Active> {
    let actions = [
        "default",
        "Open",
        "archive",
        "Archive",
        "mark-read",
        "Mark read",
    ];
    let hints = HashMap::from([("desktop-entry", Value::from("dev.predmore.fell"))]);
    let reply = proxy
        .call_method(
            "Notify",
            &(
                "Fell",
                0u32,
                "mail-unread",
                notice.title,
                notice.body,
                actions.as_slice(),
                hints,
                -1i32,
            ),
        )
        .await?;
    let owner = reply
        .header()
        .sender()
        .map(ToString::to_string)
        .ok_or_else(|| zbus::Error::Failure("notification reply had no sender".into()))?;
    Ok(Active {
        id: reply.body().deserialize()?,
        owner,
        messages: notice.messages,
        token: None,
    })
}

impl Bus {
    fn on_signal(&mut self, signal: &Message) -> Option<Invoked> {
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
                let action = match key.as_str() {
                    "default" => Action::Open,
                    "archive" => Action::Archive,
                    "mark-read" => Action::Read,
                    _ => return None,
                };
                let active = self.active.remove(position(id)?)?;
                return Some(Invoked {
                    action,
                    messages: active.messages,
                    token: active.token,
                });
            }
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeNotifier(Vec<Notice>);

    impl Notifier for FakeNotifier {
        async fn notify(&mut self, notices: Vec<Notice>) -> zbus::Result<()> {
            self.0.extend(notices);
            Ok(())
        }
    }

    fn arrival(n: i64) -> InboxArrival {
        InboxArrival {
            id: MessageId::new(format!("{n}@demo.test")),
            sender: "Avery Lane".into(),
            subject: "Field test".into(),
            snippet: "Line 1 <note>\nLine 2".into(),
            date_unix: n,
        }
    }

    #[test]
    fn suppresses_only_arrivals_committed_while_viewing_inbox() {
        let (commands, mut received) = unbounded();
        let viewing_inbox = Arc::new(AtomicBool::new(true));
        let queue = Queue {
            commands,
            viewing_inbox: viewing_inbox.clone(),
        };
        queue.committed(Ok(vec![arrival(1)]));
        assert!(received.try_recv().is_err());

        viewing_inbox.store(false, Ordering::Relaxed);
        queue.committed(Ok(Vec::new()));
        assert!(received.try_recv().is_err());

        let second = arrival(2);
        queue.committed(Ok(vec![second.clone()]));
        let Command::Committed(_, arrivals) = received.try_recv().unwrap() else {
            panic!("expected committed arrivals");
        };
        let mut fake = FakeNotifier::default();
        futures::executor::block_on(deliver(&mut fake, arrivals, true)).unwrap();
        assert_eq!(fake.0[0].messages, [second.id]);
    }

    #[test]
    fn sends_three_individual_notices_and_groups_four_or_more() {
        let mut fake = FakeNotifier::default();
        futures::executor::block_on(deliver(&mut fake, (1..=3).map(arrival).collect(), true))
            .unwrap();
        assert_eq!(fake.0.len(), 3);
        assert_eq!(fake.0[0].title, "Avery Lane — Field test");
        assert_eq!(fake.0[0].body, "Line 1 &lt;note&gt;");

        fake.0.clear();
        futures::executor::block_on(deliver(&mut fake, (1..=5).map(arrival).collect(), true))
            .unwrap();
        assert_eq!(fake.0.len(), 1);
        assert_eq!(fake.0[0].title, "5 new messages");
        assert!(fake.0[0].body.is_empty());
        assert_eq!(
            fake.0[0].messages,
            (1..=5).rev().map(|n| arrival(n).id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn private_notifications_hide_content_and_group_count() {
        for count in [1, 5] {
            let mut fake = FakeNotifier::default();
            futures::executor::block_on(deliver(
                &mut fake,
                (1..=count).map(arrival).collect(),
                false,
            ))
            .unwrap();
            assert_eq!(fake.0.len(), 1);
            assert_eq!(fake.0[0].title, "New mail");
            assert!(fake.0[0].body.is_empty());
            assert_eq!(fake.0[0].messages.len(), count as usize);
        }
    }
}
