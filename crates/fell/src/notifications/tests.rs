use super::*;

fn arrival(n: i64) -> InboxArrival {
    InboxArrival {
        id: MessageId::new(format!("{n}@demo.test")),
        sender: match n {
            5 => "Avery Lane".into(),
            4 => "Sam Ortiz".into(),
            _ => format!("Sender {n}"),
        },
        subject: "Field <test> & review".into(),
        snippet: "Hi Robin,    The route map is ready for review.".into(),
        date_unix: n,
    }
}

#[test]
fn suppresses_arrivals_while_viewing_inbox_and_signals_the_focus_transition() {
    let (commands, mut received) = unbounded();
    let queue = Queue {
        commands,
        viewing_inbox: Arc::new(AtomicBool::new(false)),
    };
    queue.set_viewing(true);
    queue.set_viewing(true);
    assert!(matches!(received.try_recv().unwrap(), Command::Focused));
    queue.committed(Ok(vec![arrival(1)]));
    assert!(received.try_recv().is_err());

    queue.set_viewing(false);
    queue.committed(Ok(Vec::new()));
    assert!(received.try_recv().is_err());

    let second = arrival(2);
    queue.committed(Ok(vec![second.clone()]));
    let Command::Committed(_, arrivals) = received.try_recv().unwrap() else {
        panic!("expected committed arrivals");
    };
    assert_eq!(arrivals, [second]);
}

#[test]
fn queued_commits_are_collected_before_an_overdue_deadline() {
    futures::executor::block_on(async {
        let (commands, mut received) = unbounded();
        let old = Instant::now() - COLLECT_FOR * 2;
        for n in 1..=5 {
            commands
                .unbounded_send(Command::Committed(old, vec![arrival(n)]))
                .unwrap();
        }
        let mut bus = None;
        let mut batch = Vec::new();
        let deadline = Some(old + COLLECT_FOR);
        while let Event::Command(Some(Command::Committed(_, mut arrivals))) =
            next_event(&mut bus, &mut received, deadline).await
        {
            batch.append(&mut arrivals);
        }
        let group = notices(batch, NotificationMode::On, false);
        assert_eq!(group.len(), 1);
        assert_eq!(group[0].title, "5 new messages");
    });
}

#[test]
fn sends_three_individual_notices_and_groups_four_or_more() {
    let singles = notices((1..=3).map(arrival).collect(), NotificationMode::On, true);
    assert_eq!(singles.len(), 3);
    assert_eq!(singles[0].title, "Sender 1");
    assert_eq!(
        singles[0].body,
        "<b>Field &lt;test&gt; &amp; review</b>\nHi Robin, The route map is ready for review."
    );
    assert_eq!(
        singles[0].kind.buttons(),
        [
            "default",
            "Open",
            "archive",
            "Archive",
            "mark-read",
            "Mark as read"
        ]
    );

    let groups = notices((1..=5).map(arrival).collect(), NotificationMode::On, true);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].title, "5 new messages");
    assert_eq!(groups[0].body, "Avery Lane, Sam Ortiz and 3 others");
    assert_eq!(groups[0].kind.buttons(), ["default", "Open Inbox"]);
    assert!(matches!(
        groups[0].kind.action("default"),
        Some(Action::OpenInbox)
    ));
    assert!(groups[0].kind.action("archive").is_none());
    assert!(groups[0].kind.action("mark-read").is_none());
    assert_eq!(
        groups[0].messages,
        (1..=5).rev().map(arrival).collect::<Vec<_>>()
    );
}

#[test]
fn private_notifications_hide_content_and_show_group_count() {
    for (count, title) in [
        (1, "1 new message"),
        (3, "3 new messages"),
        (5, "5 new messages"),
    ] {
        let posted = notices(
            (1..=count).map(arrival).collect(),
            NotificationMode::Private,
            true,
        );
        assert_eq!(posted.len(), 1);
        assert_eq!(posted[0].title, title);
        assert!(posted[0].body.is_empty());
        assert_eq!(posted[0].messages.len(), count as usize);
        assert_eq!(posted[0].kind.buttons(), ["default", "Open Inbox"]);
        assert!(matches!(
            posted[0].kind.action("default"),
            Some(Action::OpenInbox)
        ));
        assert!(posted[0].kind.action("archive").is_none());
        assert!(posted[0].kind.action("mark-read").is_none());
    }
}

#[test]
fn action_feedback_only_includes_the_clean_subject_in_normal_mode() {
    let confirmation = || Confirmation {
        action_id: 42,
        subject: Some("Route <review>\u{202e}\nClick me".into()),
    };
    let normal = feedback_notice("Archived", Some(confirmation()), NotificationMode::On, true);
    assert_eq!(normal.body, "Route &lt;review&gt;\nClick to undo");
    assert_eq!(normal.kind.buttons(), ["default", "Undo", "undo", "Undo"]);
    for key in ["default", "undo"] {
        assert!(matches!(normal.kind.action(key), Some(Action::Undo(42))));
    }

    let private = feedback_notice(
        "Archived",
        Some(confirmation()),
        NotificationMode::Private,
        true,
    );
    assert_eq!(private.body, "Click to undo");

    let result = feedback_notice("Moved back to Inbox", None, NotificationMode::On, true);
    assert!(result.body.is_empty());
    assert!(result.kind.buttons().is_empty());
}

#[test]
fn untrusted_subject_and_preview_are_each_one_plain_line() {
    let controls =
        "\u{061c}\u{200e}\u{200f}\u{202a}\u{202e}\u{2066}\u{2069}\u{206a}\u{206f}\u{feff}\u{00ad}";
    for sep in [
        "\n", "\r", "\r\n", "\u{b}", "\u{c}", "\u{85}", "\u{2028}", "\u{2029}", "\u{1c}", "\u{1d}",
        "\u{1e}",
    ] {
        let mut mail = arrival(1);
        mail.sender = format!("Avery{controls} Lane{sep}Fake sender");
        mail.subject = format!("Route{controls} <map>{sep}Fake subject");
        mail.snippet = format!("Please{controls}\t review & reply{sep}Fake preview");
        let posted = notices(vec![mail], NotificationMode::On, false);
        assert_eq!(posted[0].title, "Avery Lane");
        assert_eq!(posted[0].body, "Route <map>\nPlease review & reply");
    }
    let mut mail = arrival(1);
    mail.snippet = "é".repeat(200);
    let posted = notices(vec![mail], NotificationMode::On, false);
    assert_eq!(posted[0].body.lines().nth(1).unwrap().chars().count(), 140);
}

mod daemon {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command as Process, Stdio};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
    use zbus::zvariant::OwnedValue;

    struct TestBus(Child);

    impl TestBus {
        fn start() -> (Self, String) {
            static NEXT_BUS: AtomicU32 = AtomicU32::new(0);
            let listen = format!(
                "unix:abstract=fell-notifications-{}-{}",
                std::process::id(),
                NEXT_BUS.fetch_add(1, Relaxed)
            );
            let mut child = Self(
                Process::new("dbus-daemon")
                    .args([
                        "--session",
                        "--nofork",
                        "--print-address=1",
                        "--address",
                        &listen,
                    ])
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let mut address = String::new();
            BufReader::new(child.0.stdout.take().unwrap())
                .read_line(&mut address)
                .unwrap();
            (child, address.trim().to_owned())
        }
    }

    impl Drop for TestBus {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[derive(Default)]
    struct Calls {
        capabilities: AtomicU32,
        posted: Mutex<Vec<(u32, u32, String, String)>>,
        closed: Mutex<Vec<u32>>,
        fail_close: AtomicU32,
    }

    #[derive(Clone)]
    struct Daemon(Arc<Calls>);

    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl Daemon {
        fn get_capabilities(&self) -> Vec<&str> {
            self.0.capabilities.fetch_add(1, Relaxed);
            vec!["body-markup"]
        }

        #[expect(
            clippy::too_many_arguments,
            reason = "the notification protocol fixes the signature"
        )]
        async fn notify(
            &self,
            _app: &str,
            replaces: u32,
            _icon: &str,
            title: String,
            body: String,
            _actions: Vec<String>,
            _hints: HashMap<String, OwnedValue>,
            _expire: i32,
        ) -> u32 {
            let id = {
                let mut posted = self.0.posted.lock().unwrap();
                let id = if replaces > 0 {
                    replaces
                } else {
                    posted.len() as u32 + 1
                };
                posted.push((replaces, id, title, body));
                id
            };
            if id == 1 && replaces == 0 {
                Timer::after(BUS_TIMEOUT + Duration::from_millis(150)).await;
            }
            id
        }

        fn close_notification(&self, id: u32) -> zbus::fdo::Result<()> {
            self.0.closed.lock().unwrap().push(id);
            if self
                .0
                .fail_close
                .try_update(Relaxed, Relaxed, |n| n.checked_sub(1))
                .is_ok()
            {
                return Err(zbus::fdo::Error::Failed("temporarily unavailable".into()));
            }
            Ok(())
        }
    }

    async fn setup(address: &str, calls: Arc<Calls>) -> (zbus::Connection, Bus) {
        let service = Builder::address(address)
            .unwrap()
            .serve_at(PATH, Daemon(calls))
            .unwrap()
            .name(SERVICE)
            .unwrap()
            .build()
            .await
            .unwrap();
        let bus = connect_to(Builder::address(address).unwrap())
            .await
            .unwrap();
        (service, bus)
    }

    async fn flush_posts(bus: &mut Bus) {
        while let Some(result) = bus.posts.next().await {
            bus.finish_post(result, 0, 0).await;
        }
    }

    #[test]
    fn a_pass_without_new_mail_closes_a_notice_for_mail_that_disappeared() {
        let (_daemon, address) = TestBus::start();
        futures::executor::block_on(async {
            let calls = Arc::new(Calls::default());
            let (_service, mut bus) = setup(&address, calls.clone()).await;
            let account = crate::fixtures::fixture_account();
            let account_id = account.id.clone();
            let (notices, _worker_events) = unbounded();
            let (store, ready) = StoreClient::spawn(
                move || {
                    let store = fell_core::Store::open_in_memory().unwrap();
                    crate::fixtures::seed_if_empty(&store, &account).unwrap();
                    (store, ())
                },
                notices,
            );
            ready.await.unwrap();
            let id = MessageId::new("dispatch-42@fell.test");
            let mail = InboxArrival {
                id: id.clone(),
                ..arrival(1)
            };
            bus.prepare_delivery().await.unwrap();
            bus.deliver(
                vec![mail],
                &HashSet::from([id.clone()]),
                NotificationMode::On,
                0,
                0,
            );
            flush_posts(&mut bus).await;
            let account_for_write = account_id.clone();
            assert!(
                store
                    .call(move |store| store.delete_message(&account_for_write, &id))
                    .await
                    .unwrap()
                    .unwrap()
            );

            let (commands, incoming) = unbounded();
            let (actions, _invoked) = unbounded();
            let viewing = Arc::new(AtomicBool::new(false));
            let queue = Queue {
                commands,
                viewing_inbox: viewing.clone(),
            };
            let verify = async move {
                queue.pass_ended();
                bus_timeout(async {
                    while calls.closed.lock().unwrap().is_empty() {
                        Timer::after(Duration::from_millis(10)).await;
                    }
                    Ok(())
                })
                .await
                .expect("the active notice should close while the worker is running");
                assert_eq!(*calls.closed.lock().unwrap(), [1]);
                drop(queue);
            };
            futures::join!(
                run(
                    Some(bus),
                    incoming,
                    actions,
                    viewing,
                    NotificationMode::On,
                    store,
                    account_id
                ),
                verify,
            );
        });
    }

    #[test]
    fn a_late_notify_reply_is_kept_and_a_group_folds_in_unread_singles_and_groups() {
        let (_daemon, address) = TestBus::start();
        futures::executor::block_on(async {
            let calls = Arc::new(Calls::default());
            let (_service, mut bus) = setup(&address, calls.clone()).await;
            let mut unread = (1..=11).map(|n| arrival(n).id).collect::<HashSet<_>>();
            bus.prepare_delivery().await.unwrap();
            bus.deliver(vec![arrival(1)], &unread, NotificationMode::On, 0, 0);
            assert!(
                bus_timeout(async { Ok(next_post(&mut bus.posts).await) })
                    .await
                    .is_err()
            );
            // The original Notify future is still queued after the deadline.
            flush_posts(&mut bus).await;
            assert_eq!(bus.active[0].receipt.id, 1);

            bus.deliver(
                (2..=5).map(arrival).collect(),
                &unread,
                NotificationMode::On,
                0,
                0,
            );
            flush_posts(&mut bus).await;
            let group_id = bus.active[0].receipt.id;
            assert_eq!(bus.active.len(), 1);
            assert_eq!(*calls.closed.lock().unwrap(), [1]);
            assert_eq!(bus.active[0].messages.len(), 5);

            bus.deliver(vec![arrival(6)], &unread, NotificationMode::On, 0, 0);
            flush_posts(&mut bus).await;
            unread.remove(&arrival(4).id);
            bus.prepare_delivery().await.unwrap();
            bus.deliver(
                (7..=11).map(arrival).collect(),
                &unread,
                NotificationMode::On,
                0,
                0,
            );
            flush_posts(&mut bus).await;
            assert_eq!(bus.active.len(), 1);
            assert_eq!(bus.active[0].receipt.id, group_id);
            assert_eq!(bus.active[0].messages.len(), 10);
            assert_eq!(calls.capabilities.load(Relaxed), 1);
            let posted = calls.posted.lock().unwrap();
            assert_eq!(posted[1].2, "5 new messages");
            assert_eq!(
                posted[3],
                (
                    group_id,
                    group_id,
                    "10 new messages".into(),
                    "Sender 11, Sender 10 and 8 others".into()
                )
            );
        });
    }

    #[test]
    fn private_bursts_reuse_an_id_and_close_retries_without_losing_it() {
        let (_daemon, address) = TestBus::start();
        futures::executor::block_on(async {
            let calls = Arc::new(Calls::default());
            let (service, mut bus) = setup(&address, calls.clone()).await;
            let unread = (1..=2).map(|n| arrival(n).id).collect::<HashSet<_>>();
            bus.prepare_delivery().await.unwrap();
            for n in 1..=2 {
                bus.deliver(vec![arrival(n)], &unread, NotificationMode::Private, 0, 0);
                flush_posts(&mut bus).await;
            }
            assert_eq!(bus.active.len(), 1);
            assert_eq!(bus.active[0].receipt.id, 1);
            assert_eq!(bus.active[0].messages.len(), 2);
            assert_eq!(
                calls.posted.lock().unwrap()[1],
                (1, 1, "2 new messages".into(), String::new())
            );

            calls.fail_close.store(2, Relaxed);
            bus.close(|_| true).await;
            assert_eq!(*calls.closed.lock().unwrap(), [1, 1]);
            assert_eq!(bus.active[0].receipt.id, 1);
            assert!(bus.active[0].close_pending);

            service
                .emit_signal(
                    None::<&str>,
                    PATH,
                    SERVICE,
                    "ActionInvoked",
                    &(1_u32, "default"),
                )
                .await
                .unwrap();
            let signal = bus.signals.next().await.unwrap();
            let (actions, mut invoked) = unbounded();
            bus.on_signal(signal.clone(), &actions, 1);
            assert!(
                invoked.try_recv().is_err(),
                "discard actions from an old privacy mode"
            );
            bus.on_signal(signal, &actions, 0);
            assert!(matches!(
                invoked.try_recv().unwrap().action,
                Action::OpenInbox
            ));

            calls.fail_close.store(1, Relaxed);
            bus.close(|_| false).await;
            assert_eq!(*calls.closed.lock().unwrap(), [1, 1, 1, 1]);
            assert!(bus.active.is_empty());
        });
    }
}
