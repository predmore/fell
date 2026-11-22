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
        (1..=5).rev().map(|n| arrival(n).id).collect::<Vec<_>>()
    );
}

#[test]
fn private_notifications_hide_content_and_show_group_count() {
    for (count, title, action) in [
        (1, "New mail", "Open"),
        (3, "3 new messages", "Open Inbox"),
        (5, "5 new messages", "Open Inbox"),
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
        assert_eq!(posted[0].kind.buttons(), ["default", action]);
    }
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
