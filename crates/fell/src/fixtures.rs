//! Fixture mailbox used until read-only IMAP is on main.
//!
//! Messages are written through the store. The window only reads them back.

use fell_core::message::{Address, Message, MessageHeaders, MessageId, MessageParts, RemoteId};
use fell_core::{Account, AccountId, BackendKind, FixtureBackend, Store, ThreadId};
use fell_core::{SyncReport, sync_once};

use std::time::{SystemTime, UNIX_EPOCH};

pub fn fixture_account() -> Account {
    Account {
        id: AccountId("local".into()),
        display_name: "Tessa".into(),
        email: "tessa@harbor.test".into(),
        backend_kind: BackendKind::Bridge,
    }
}

pub fn seed_if_empty(store: &Store, account: &Account) -> fell_core::Result<SyncReport> {
    if !store.list_threads(&account.id)?.is_empty() {
        return Ok(SyncReport::default());
    }
    let mut messages = fixture_messages(account);
    messages.extend(extra_messages(account, extra_count()));
    let backend = FixtureBackend { messages };
    sync_once(account, &backend, store)
}

/// `FELL_DEMO_EXTRA=N` pads a fresh demo mailbox with N plain inbox threads
/// below the fixtures, for frame-time runs that need a long list.
fn extra_count() -> usize {
    std::env::var("FELL_DEMO_EXTRA")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
        .min(100_000)
}

const EXTRA_SENDERS: [(&str, &str); 6] = [
    ("Ada", "ada@harbor.test"),
    ("Build bot", "builds@harbor.test"),
    ("Mira", "mira@harbor.test"),
    ("Theo", "theo@harbor.test"),
    ("Calendar", "calendar@harbor.test"),
    ("Priya", "priya@harbor.test"),
];

const EXTRA_SUBJECTS: [&str; 6] = [
    "Weekly numbers",
    "Build passed on main",
    "Notes from the design review",
    "Can you look at this before Friday?",
    "Invitation: planning sync",
    "Draft of the release post",
];

fn extra_messages(account: &Account, count: usize) -> Vec<Message> {
    // Older than every fixture, so the fixtures stay on top.
    let base = now().saturating_sub(30 * 86_400);
    (0..count)
        .map(|index| {
            let (name, email) = EXTRA_SENDERS[index % EXTRA_SENDERS.len()];
            let subject = format!(
                "{} #{}",
                EXTRA_SUBJECTS[index % EXTRA_SUBJECTS.len()],
                index + 1
            );
            let id = MessageId::new(format!("extra-{index}@fell.test"));
            Message {
                account_id: account.id.clone(),
                headers: MessageHeaders {
                    message_id: id.clone(),
                    proton_conversation_id: None,
                    in_reply_to: None,
                    references: Vec::new(),
                    from: Address {
                        name: Some(name.into()),
                        email: email.into(),
                    },
                    subject,
                    date_unix: base.saturating_sub(index as i64 * 3_600),
                },
                remote_id: RemoteId(format!("fixture:extra-{index}")),
                thread_id: ThreadId::from_message_id(&id),
                snippet: "A filler thread for long-list runs.".into(),
                body_text: Some("A filler thread for long-list runs.".into()),
                unread: index % 3 == 0,
                folders: vec!["INBOX".into()],
                labels: Vec::new(),
                arrived_via: vec![account.email.clone()],
                to: vec![Address {
                    name: Some(account.display_name.clone()),
                    email: account.email.clone(),
                }],
                cc: Vec::new(),
                bcc: Vec::new(),
                starred: false,
                has_attachment: false,
                archived: false,
                snooze_until: None,
                list_id: None,
                reply_to: Vec::new(),
                attachment_names: Vec::new(),
                parts: None,
            }
        })
        .collect()
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

struct DraftMail {
    id: &'static str,
    name: &'static str,
    email: &'static str,
    subject: &'static str,
    body: &'static str,
    ago: i64,
    unread: bool,
    folders: &'static [&'static str],
    labels: &'static [&'static str],
    reply_to: Option<&'static str>,
    proton: Option<&'static str>,
    starred: bool,
    attachment: bool,
}

fn fixture_messages(account: &Account) -> Vec<Message> {
    let stamp = now();
    let rows = [
        DraftMail {
            id: "cabin-1@fell.test",
            name: "Tessa",
            email: "tessa@harbor.test",
            subject: "Cabin weekend",
            body: "Did the cabin come through? I can drive if you sort the food.",
            ago: 26 * 3600 + 20 * 60,
            unread: false,
            folders: &["Sent"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "cabin-2@fell.test",
            name: "Ada",
            email: "ada@harbor.test",
            subject: "Re: Cabin weekend",
            body: "Tessa,\n\nThe cabin is ours for the 14th to the 16th. A few things before we go:\n\n* Bring the good headlamp; the path from the car is dark.\n* Maya has the key. She gets there Friday at 6.\n* Firewood is on the porch, and there is more in the shed.\n\nThe map is below, and the booking is here if you want to check the dates.\n\nCan't wait.\n\n-- \nAda\nField notes, mostly\nnotes.harbor.test\n\nOn Mon, Nov 16, 2026 at 8:12 PM Tessa <tessa@harbor.test> wrote:\n> Did the cabin come through? I can drive if you sort the food.",
            ago: 25 * 60,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: Some("cabin-1@fell.test"),
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "dispatch-42@fell.test",
            name: "The Lakeside Dispatch",
            email: "news@dispatch.test",
            subject: "The week the river slowed",
            body: "The Lakeside Dispatch, issue 42: notes from the town by the water.\n\nThe week the river slowed. For three days the river barely moved.",
            ago: 90 * 60,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "venue-1@fell.test",
            name: "Priya Natarajan",
            email: "priya@harbor.test",
            subject: "Venue for the launch",
            body: "Two options: the hall on Mill Lane, or the back room at the library. Mill Lane is bigger.",
            ago: 30 * 3600,
            unread: false,
            folders: &["INBOX"],
            labels: &["work"],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "venue-2@fell.test",
            name: "Tessa",
            email: "tessa@harbor.test",
            subject: "Re: Venue for the launch",
            body: "Mill Lane works for me. Can you check they have a projector?\n\n> Two options: the hall on Mill Lane, or the back room at the library.",
            ago: 28 * 3600,
            unread: false,
            folders: &["Sent"],
            labels: &[],
            reply_to: Some("venue-1@fell.test"),
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "venue-3@fell.test",
            name: "Priya Natarajan",
            email: "priya@harbor.test",
            subject: "Re: Venue for the launch",
            body: "Booked. The hall on Mill Lane holds 80, and they'll leave the projector up for us.\nDeposit is paid; I'll send the receipt to Sam.\n\nGet Outlook for Android\n\nFrom: Tessa <tessa@harbor.test>\nSent: Tuesday, November 17, 2026 4:02 PM\nTo: Priya Natarajan <priya@harbor.test>\nSubject: Re: Venue for the launch\n\nMill Lane works for me. Can you check they have a projector?",
            ago: 3 * 3600 + 30 * 60,
            unread: false,
            folders: &["INBOX"],
            labels: &["work"],
            reply_to: Some("venue-2@fell.test"),
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "offsite@fell.test",
            name: "Tessa",
            email: "tessa@harbor.test",
            subject: "Offsite plan for March",
            body: "Booked the lodge for the 12th to the 14th. Agenda to follow; send me anything you want on it by Friday.",
            ago: 5 * 60,
            unread: false,
            // Sent to the team list, which delivers a copy back to the inbox.
            folders: &["Sent", "INBOX"],
            labels: &["work"],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "thread-a-1@fell.test",
            name: "Ruben",
            email: "ruben@harbor.test",
            subject: "Sent copies in the thread",
            body: "Can we keep Sent in the thread?\n\nI don't want a second sidebar for it.",
            ago: 2 * 86_400,
            unread: false,
            folders: &["INBOX"],
            labels: &["work"],
            reply_to: None,
            proton: Some("sent-copies"),
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "thread-a-2@fell.test",
            name: "Tessa",
            email: "tessa@harbor.test",
            subject: "Re: Sent copies in the thread",
            body: "The list stays one folder wide. Your reply in Sent still shows up here.\n\n-- \nTessa\n\n> Can we keep Sent in the thread?",
            ago: 86_400,
            unread: false,
            folders: &["Sent"],
            labels: &["work"],
            reply_to: Some("thread-a-1@fell.test"),
            proton: Some("sent-copies"),
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "thread-a-3@fell.test",
            name: "Ruben",
            email: "ruben@harbor.test",
            subject: "Re: Sent copies in the thread",
            body: "Yes. m moves the copies in this folder. The Sent copy stays in Sent.\n\n-- \nRuben\n\n> Can we keep Sent in the thread?\n> I don't want a second sidebar for it.\n>> The list stays one folder wide.\n> > Your reply in Sent still shows up here.",
            ago: 12 * 60,
            unread: true,
            folders: &["INBOX"],
            labels: &["work"],
            reply_to: Some("thread-a-2@fell.test"),
            proton: Some("sent-copies"),
            starred: true,
            attachment: false,
        },
        DraftMail {
            id: "rows@fell.test",
            name: "Ada",
            email: "ada@harbor.test",
            subject: "Row height for the inbox",
            body: "Two lines is enough. Here are the numbers from the test, so you can check me:\n\nDensity | Row height | Rows on screen | Notes\nCompact | 44 px | 19 | Snippet cut early on narrow tiles\nDefault | 52 px | 16 | Who, then subject and snippet\nRoomy | 64 px | 13 | Feels slow with j held down\n\nWhat I'd do, in order:\n\n1. Ship Default.\n2. Keep Compact behind a setting.\n3. Drop Roomy.\n\nThe paperclip is the diagram; the spec is on the wiki.\n\nOn Sun, Nov 15, 2026 at 9:02 AM Tessa <tessa@harbor.test> wrote:\n> How many lines should a row be?",
            ago: 40 * 60,
            unread: true,
            folders: &["INBOX"],
            labels: &["design"],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: true,
        },
        DraftMail {
            id: "narrow@fell.test",
            name: "Maya",
            email: "maya@harbor.test",
            subject: "Narrow window check",
            body: "When the tile gets narrow, show one pane. Enter opens, Esc comes back.",
            ago: 2 * 3600,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "bridge@fell.test",
            name: "Ruben",
            email: "ruben@harbor.test",
            subject: "Bridge on port 1143",
            body: "STARTTLS before LOGIN. The UI should not wait on it.",
            ago: 3 * 3600,
            unread: false,
            folders: &["INBOX"],
            labels: &["work"],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "dinner@fell.test",
            name: "Sam",
            email: "sam@harbor.test",
            subject: "Thursday dinner",
            body: "Same place as last time, 7pm. I'll grab the corner table.",
            ago: 26 * 3600,
            unread: false,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "tiles@fell.test",
            name: "Maya",
            email: "maya@harbor.test",
            subject: "Hyprland tiles",
            body: "A half-width tile should not crush the split. One pane, same keys.",
            ago: 3 * 86_400,
            unread: false,
            folders: &["INBOX"],
            labels: &["design"],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "tickets@fell.test",
            name: "Maya",
            email: "maya@harbor.test",
            subject: "Tickets for Friday",
            body: "Two seats, aisle. I'll forward the PDF when it lands.",
            ago: 4 * 86_400,
            unread: false,
            folders: &["INBOX"],
            labels: &["travel"],
            reply_to: None,
            proton: None,
            starred: true,
            attachment: true,
        },
        DraftMail {
            id: "qresync@fell.test",
            name: "Ruben",
            email: "ruben@harbor.test",
            subject: "QRESYNC notes",
            body: "CONDSTORE when the server has it. Otherwise just the new UIDs.",
            ago: 5 * 86_400,
            unread: false,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "font@fell.test",
            name: "Ada",
            email: "ada@harbor.test",
            subject: "Font on Omarchy",
            body: "Use the UI face and the size Omarchy already set. Don't invent a third.",
            ago: 6 * 86_400,
            unread: false,
            folders: &["INBOX"],
            labels: &["design"],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "weekly@fell.test",
            name: "Weekly",
            email: "notes@harbor.test",
            subject: "Weekly notes",
            body: "A short note on native text.\n\nQuoted history stays folded.\n\n> older weekly\n> still folded",
            ago: 9 * 86_400,
            unread: false,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "labels@fell.test",
            name: "Ada",
            email: "ada@harbor.test",
            subject: "Labels are not folders",
            body: "A chip on the row, not a sidebar. l will edit them later.",
            ago: 8 * 86_400,
            unread: false,
            folders: &["INBOX"],
            labels: &["work", "design"],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "intro@fell.test",
            name: "Tessa",
            email: "tessa@harbor.test",
            subject: "Introducing Fell",
            body: "Threads first, local store, no send from the agent.",
            ago: 5 * 86_400,
            unread: false,
            folders: &["Sent"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "invoice@fell.test",
            name: "Tessa",
            email: "tessa@harbor.test",
            subject: "Re: Invoice",
            body: "Draft only. Not sent.",
            ago: 6 * 3600,
            unread: true,
            folders: &["Drafts"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "hosting@fell.test",
            name: "Sam",
            email: "sam@harbor.test",
            subject: "Old hosting bill",
            body: "Already paid. Sitting in Archive.",
            ago: 20 * 86_400,
            unread: false,
            folders: &["Archive"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "spam@fell.test",
            name: "Prize",
            email: "prize@example.test",
            subject: "You have won",
            body: "This stays in Spam.",
            ago: 12 * 86_400,
            unread: true,
            folders: &["Spam"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        // One mail from each family of the fell-render corpus, as their
        // clients and senders write them.
        DraftMail {
            id: "corpus-sale@fell.test",
            name: "Northpine Outfitters",
            email: "hello@email.northpine.example",
            subject: "Up to 30% off insulated jackets",
            body: "Our warmest jackets are up to 30% off until Sunday at midnight.",
            ago: 240,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "corpus-rota@fell.test",
            name: "Teo Marsh",
            email: "teo@harbour.example.org",
            subject: "Re: Rota for the open day",
            body: "Hi Ines, thanks for pulling the numbers together.",
            ago: 720,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "corpus-review@fell.test",
            name: "Lena Hart",
            email: "lena@studio.example.com",
            subject: "Re: Moving the review",
            body: "That works for me. I'll book the small room for the 3rd.",
            ago: 2400,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "corpus-saturday@fell.test",
            name: "Noor Achebe",
            email: "noor@example.net",
            subject: "Re: Saturday",
            body: "Yes, Saturday is fine. I'll bring the long table from the garage.",
            ago: 7200,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "corpus-desks@fell.test",
            name: "Ruth Allard",
            email: "ruth@example.org",
            subject: "RE: Desks for the second floor",
            body: "Hi Sven, thanks, the revised quote works for us.",
            ago: 10800,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "corpus-keys@fell.test",
            name: "Mira Solberg",
            email: "mira@proton.example",
            subject: "Re: Keys",
            body: "Hi Jun, the keys are under the blue pot by the back door.",
            ago: 14400,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "corpus-backup@fell.test",
            name: "Hanne Berg",
            email: "hanne@library.example.org",
            subject: "Re: Backup",
            body: "Hi Oskar, I ran the backup script again last night.",
            ago: 18000,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "corpus-ferry@fell.test",
            name: "Bayline Ferries",
            email: "bookings@bayline.example",
            subject: "Booking confirmed: Harbour Town to Gull Island",
            body: "Your booking is confirmed. Your code is BX7-42QK.",
            ago: 21600,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "corpus-readings@fell.test",
            name: "Ola Brenne",
            email: "ola@weather.example.org",
            subject: "October readings",
            body: "Here are the station readings for October.",
            ago: 25200,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "corpus-relay@fell.test",
            name: "Kai Moreno",
            email: "kai@ops.example.com",
            subject: "Relay deploy failed again",
            body: "The relay deploy failed again at 22:14.",
            ago: 28800,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "corpus-lecture@fell.test",
            name: "Yael Dagan",
            email: "yael@kehila.example",
            subject: "Re: הרצאה ביום חמישי",
            body: "שלום אבי, ההרצאה ביום חמישי מתחילה בשעה 18:00.",
            ago: 32400,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "corpus-scanner@fell.test",
            name: "Hanne Berg",
            email: "hanne@library.example.org",
            subject: "Re: Moving the scanner",
            body: "Monday is fine. It only needs the afternoon once the cart is free.",
            ago: 36000,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "corpus-checklist@fell.test",
            name: "Dee Okoro",
            email: "dee@harbour.example.org",
            subject: "Re: Boat checklist",
            body: "Answers inline.",
            ago: 39600,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "corpus-desks-fw@fell.test",
            name: "Ruth Allard",
            email: "ruth@example.org",
            subject: "FW: Chairs for the meeting room",
            body: "The quote for the new chairs is below.",
            ago: 43200,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "corpus-proofs@fell.test",
            name: "Lena Hart",
            email: "lena@studio.example.com",
            subject: "Fwd: Print proofs ready",
            body: "Ana, these are for the catalogue.",
            ago: 46800,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "corpus-kale@fell.test",
            name: "Noor Achebe",
            email: "noor@example.net",
            subject: "The allotment this morning",
            body: "Look at the frost on the kale!",
            ago: 50400,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "corpus-visit@fell.test",
            name: "Pia Lund",
            email: "pia@works.example.com",
            subject: "Visit on Thursday",
            body: "Thursday at 14:00 works for the site visit.",
            ago: 54000,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "corpus-meeting@fell.test",
            name: "佐藤 美咲",
            email: "misaki@example.jp.example",
            subject: "来週の打ち合わせ",
            body: "武田さん お疲れさまです。",
            ago: 57600,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "corpus-briefing@fell.test",
            name: "Ines Varga",
            email: "ines@harbour.example.org",
            subject: "Invitation: Open day briefing @ Thu 26 Nov 2026 18:00 - 19:00 (GMT)",
            body: "Open day briefing",
            ago: 61200,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "corpus-relay-2@fell.test",
            name: "Kai Moreno",
            email: "kai@ops.example.com",
            subject: "Re: Relay deploy failed again",
            body: "Staging is on retries = 5 now.",
            ago: 64800,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "corpus-signin@fell.test",
            name: "Bayline Ferries",
            email: "bookings@bayline.example",
            subject: "Your sign-in code",
            body: "Use this code to sign in to your Bayline account.",
            ago: 68400,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
    ];
    rows.into_iter()
        .map(|row| message(account, stamp, row))
        .collect()
}

/// Who each fixture message went to. Mail to the person by default, and a
/// longer list on the first thread so the conversation shows cc, "+N more",
/// and the Bcc Fell keeps on the person's own sent copy.
type People = &'static [(&'static str, &'static str)];

fn recipients(row: &DraftMail, me: &str) -> (People, People, People) {
    const TEAM: People = &[("Ada", "ada@harbor.test"), ("Maya", "maya@harbor.test")];
    const EVERYONE: People = &[
        ("Ada", "ada@harbor.test"),
        ("Maya", "maya@harbor.test"),
        ("Sam", "sam@harbor.test"),
        ("Priya Natarajan", "priya@harbor.test"),
    ];
    match row.id {
        "thread-a-1@fell.test" => (&[("Tessa", "tessa@harbor.test")], TEAM, &[]),
        "thread-a-2@fell.test" => (
            &[("Ruben", "ruben@harbor.test")],
            TEAM,
            &[("Sam", "sam@harbor.test")],
        ),
        "thread-a-3@fell.test" => (&[("Tessa", "tessa@harbor.test")], EVERYONE, &[]),
        "offsite@fell.test" => (
            &[("Ada", "ada@harbor.test")],
            &[
                ("Maya", "maya@harbor.test"),
                ("Sam", "sam@harbor.test"),
                ("Priya Natarajan", "priya@harbor.test"),
                ("Ruben", "ruben@harbor.test"),
                ("Bob Stone", "bob@harbor.test"),
                ("Lena Ortiz", "lena@harbor.test"),
            ],
            &[("Riya", "riya@harbor.test")],
        ),
        _ if row.email == me => (&[("Ruben", "ruben@harbor.test")], &[], &[]),
        _ => (&[("Tessa", "tessa@harbor.test")], &[], &[]),
    }
}

fn people(list: People) -> Vec<Address> {
    list.iter()
        .map(|(name, email)| Address {
            name: Some((*name).into()),
            email: (*email).into(),
        })
        .collect()
}

fn message(account: &Account, now: i64, row: DraftMail) -> Message {
    let (to, cc, bcc) = recipients(&row, &account.email);
    let references = row.reply_to.map(MessageId::new).into_iter().collect();
    Message {
        account_id: account.id.clone(),
        headers: MessageHeaders {
            message_id: MessageId::new(row.id),
            proton_conversation_id: row.proton.map(str::to_string),
            in_reply_to: row.reply_to.map(MessageId::new),
            references,
            from: Address {
                name: Some(row.name.into()),
                email: row.email.into(),
            },
            subject: row.subject.into(),
            date_unix: now.saturating_sub(row.ago),
        },
        remote_id: RemoteId(format!("fixture:{}", row.id)),
        thread_id: ThreadId::from_message_id(&MessageId::new(row.id)),
        snippet: first_line(row.body),
        body_text: Some(row.body.into()),
        unread: row.unread,
        folders: row
            .folders
            .iter()
            .map(|folder| (*folder).to_string())
            .collect(),
        labels: row
            .labels
            .iter()
            .map(|label| (*label).to_string())
            .collect(),
        arrived_via: vec![account.email.clone()],
        to: people(to),
        cc: people(cc),
        bcc: people(bcc),
        starred: row.starred,
        has_attachment: row.attachment,
        archived: false,
        snooze_until: None,
        list_id: None,
        reply_to: reply_address(row.id),
        attachment_names: attachment_names(&row),
        parts: Some(parts(&row)),
    }
}

/// Demo messages whose HTML is a fell-render corpus mail.
const CORPUS_MAIL: &[(&str, &str)] = &[
    ("corpus-sale@fell.test", "esp-marketing"),
    ("corpus-rota@fell.test", "gmail-reply-nested"),
    ("corpus-review@fell.test", "apple-mac-reply"),
    ("corpus-saturday@fell.test", "apple-iphone-reply"),
    ("corpus-desks@fell.test", "outlook-reply"),
    ("corpus-keys@fell.test", "proton-reply"),
    ("corpus-backup@fell.test", "thunderbird-reply"),
    ("corpus-ferry@fell.test", "esp-receipt"),
    ("corpus-readings@fell.test", "data-table"),
    ("corpus-relay@fell.test", "pre-code"),
    ("corpus-lecture@fell.test", "rtl-reply"),
    ("corpus-scanner@fell.test", "thunderbird-inline-reply"),
    ("corpus-checklist@fell.test", "gmail-inline-reply"),
    ("corpus-desks-fw@fell.test", "outlook-forward"),
    ("corpus-proofs@fell.test", "apple-forward"),
    ("corpus-kale@fell.test", "personal-photo"),
    ("corpus-visit@fell.test", "outlook-signature-logo"),
    ("corpus-meeting@fell.test", "cjk-note"),
    ("corpus-briefing@fell.test", "calendar-invite"),
    ("corpus-relay-2@fell.test", "webmail-gt-reply"),
    ("corpus-signin@fell.test", "otp-code"),
];

/// The parts sync would store: the HTML of the mail that has it, the plain
/// text of every message, and a newsletter's bulk headers.
fn parts(row: &DraftMail) -> MessageParts {
    if let Some((_, name)) = CORPUS_MAIL.iter().find(|(id, _)| *id == row.id)
        && let Some(mail) = fell_render::corpus()
            .into_iter()
            .find(|mail| mail.name == *name)
    {
        return MessageParts {
            html: Some(mail.html),
            plain: Some(row.body.to_string()),
            // Sync keeps the headers that mark bulk mail.
            bulk_headers: mail
                .headers
                .fields()
                .filter(|(name, value)| {
                    let mut one = fell_render::MailHeaders::new();
                    one.add(*name, *value);
                    one.is_bulk()
                })
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            mime_parts: Vec::new(),
        };
    }
    let (html, bulk): (Option<&str>, &[(&str, &str)]) = match row.id {
        "cabin-2@fell.test" => (Some(CABIN_REPLY_HTML), &[]),
        "dispatch-42@fell.test" => (
            Some(DISPATCH_HTML),
            &[
                (
                    "List-Unsubscribe",
                    "<https://dispatch.test/unsubscribe?u=demo>",
                ),
                ("Precedence", "bulk"),
            ],
        ),
        "venue-1@fell.test" => (Some(VENUE_FIRST_HTML), &[]),
        "rows@fell.test" => (Some(ROWS_REPLY_HTML), &[]),
        "venue-3@fell.test" => (Some(VENUE_REPLY_HTML), &[]),
        _ => (None, &[]),
    };
    MessageParts {
        html: html.map(str::to_string),
        // The newsletter sends HTML only, as many do.
        plain: (row.id != "dispatch-42@fell.test").then(|| row.body.to_string()),
        bulk_headers: bulk
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect(),
        mime_parts: attachment_parts(row),
    }
}

/// The part list sync would store for a fixture's attachments, so the
/// row's paperclip and names come from it as they do for synced mail.
fn attachment_parts(row: &DraftMail) -> Vec<fell_core::MimePart> {
    attachment_names(row)
        .into_iter()
        .enumerate()
        .map(|(at, name)| fell_core::MimePart {
            section: (at + 2).to_string(),
            role: fell_core::PartRole::Attachment,
            mime_type: "application/octet-stream".into(),
            filename: Some(name),
            size: 0,
            encoded_size: 0,
            encoding: fell_core::TransferEncoding::default(),
            content_id: None,
            disposition: fell_core::Disposition::Attachment,
            nested: false,
        })
        .collect()
}

/// A reply as Gmail writes it: lists, a link, a remote image, a signature
/// after `-- `, and the earlier message in a `gmail_quote`.
const CABIN_REPLY_HTML: &str = r#"<div dir="ltr"><div>Tessa,</div><div><br></div><div>The cabin is ours for the <b>14th to the 16th</b>. A few things before we go:</div><ul><li>Bring the <i>good</i> headlamp; the path from the car is dark.</li><li>Maya has the key. She gets there Friday at 6.</li><li>Firewood is on the porch, and there is more in the shed.</li></ul><div>The map is below, and the booking is <a href="https://cabins.test/booking/4821">here</a> if you want to check the dates.</div><div><br></div><div><img src="https://cabins.test/map/lake-path.png" alt="Map of the lake path" width="480"></div><div><br></div><div>Can't wait.</div><div><br></div>-- <br><div dir="ltr" class="gmail_signature"><div>Ada</div><div>Field notes, mostly</div><div>notes.harbor.test</div></div></div><br><div class="gmail_quote gmail_quote_container"><div dir="ltr" class="gmail_attr">On Mon, Nov 16, 2026 at 8:12 PM Tessa &lt;tessa@harbor.test&gt; wrote:<br></div><blockquote class="gmail_quote" style="margin:0px 0px 0px 0.8ex;border-left:1px solid rgb(204,204,204);padding-left:1ex"><div dir="ltr">Did the cabin come through? I can drive if you sort the food.</div></blockquote></div>"#;

/// A newsletter as ESPs send them: a hidden preheader padded with
/// `&zwnj;&nbsp;`, nested presentation tables, remote images, a two-column
/// story row, a data table, social icons, fine print and an open-tracking
/// pixel, with bulk headers.
const DISPATCH_HTML: &str = r#"<!DOCTYPE html><html><head><meta charset="utf-8"><style>.btn a{color:#fff}</style></head>
<body style="margin:0;background:#eeeae2">
<span style="display:none;max-height:0;overflow:hidden">This week: low water, a new ferry timetable, and the reading hour moves to Wednesday.&#847;&zwnj;&nbsp;&#847;&zwnj;&nbsp;</span>
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="background:#eeeae2"><tr><td align="center" style="padding:24px 12px">
<table role="presentation" width="600" cellpadding="0" cellspacing="0" style="background:#ffffff">
<tr><td style="padding:12px 24px;font-size:11px;color:#888">Having trouble reading this? <a href="https://dispatch.test/view/42">View it in your browser</a></td></tr>
<tr><td align="center" style="padding:20px 24px"><a href="https://dispatch.test/"><img src="https://cdn.dispatch.test/logo.png" width="180" height="40" alt="The Lakeside Dispatch"></a></td></tr>
<tr><td><img src="https://cdn.dispatch.test/hero-42.jpg" width="600" height="280" alt="Low water under the old pier" style="display:block"></td></tr>
<tr><td style="padding:28px 32px 8px;font-family:Georgia,serif">
<h1 style="font-size:30px;margin:0">The week the river slowed</h1>
<p style="font-size:17px;line-height:1.6;color:#333">For three days the river barely moved. The ferry ran on time anyway, and the bakery on Pier Street sold out of rye by nine. Down at the boatyard, Ines measured the water against the painted marks on the slipway and found it lower than any October she can remember, which is saying something after thirty-one years.</p>
<p style="font-size:17px;line-height:1.6;color:#333">The council says the weir upstream is working as it should. The anglers are less sure.</p>
</td></tr>
<tr><td align="center" style="padding:8px 32px 28px">
<table role="presentation" cellpadding="0" cellspacing="0"><tr><td class="btn" style="background:#1f4b6e;border-radius:4px;padding:12px 28px"><a href="https://dispatch.test/s/river" style="color:#ffffff;text-decoration:none;font-weight:bold">Read the full story &rarr;</a></td></tr></table>
</td></tr>
<tr><td style="padding:0 32px"><table role="presentation" width="100%" cellpadding="0" cellspacing="0"><tr>
<td width="50%" valign="top" style="padding:0 12px 24px 0"><img src="https://cdn.dispatch.test/ferry.jpg" width="260" height="160" alt="The morning ferry"><h3 style="font-family:Georgia,serif;margin:12px 0 4px">A new timetable</h3><p style="margin:0;font-size:15px;line-height:1.5">From Monday the first crossing leaves at 6:40, ten minutes earlier.</p><p style="margin:8px 0 0"><a href="https://dispatch.test/s/ferry">Timetable</a></p></td>
<td width="50%" valign="top" style="padding:0 0 24px 12px"><img src="https://cdn.dispatch.test/library.jpg" width="260" height="160" alt="The library steps"><h3 style="font-family:Georgia,serif;margin:12px 0 4px">Reading hour moves</h3><p style="margin:0;font-size:15px;line-height:1.5">Children's reading hour is now on Wednesdays at four, in the upstairs room.</p><p style="margin:8px 0 0"><a href="https://dispatch.test/s/library">Details</a></p></td>
</tr></table></td></tr>
<tr><td style="padding:0 32px 24px"><h2 style="font-family:Georgia,serif;font-size:20px">On the calendar</h2>
<table width="100%" cellpadding="6" cellspacing="0" style="font-size:15px;border-top:1px solid #ddd">
<tr><th align="left">Day</th><th align="left">What</th><th align="left">Where</th></tr>
<tr><td>Saturday</td><td>Boat swap</td><td>Old pier, 10am</td></tr>
<tr><td>Sunday</td><td>Choir</td><td>The square, 4pm</td></tr>
<tr><td>Wednesday</td><td>Reading hour</td><td>Library, 4pm</td></tr>
</table></td></tr>
<tr><td style="padding:0 32px 24px;font-family:Georgia,serif"><h2 style="font-size:20px">Letters</h2>
<blockquote style="border-left:3px solid #ccc;margin:0;padding-left:12px;font-style:italic">The new benches on the boardwalk are the best thing the council has done in years.</blockquote>
<p style="font-size:14px;color:#666">A reader on Dock Road</p></td></tr>
<tr><td align="center" style="padding:16px 32px;background:#f6f4ef">
<a href="https://social.test/dispatch"><img src="https://cdn.dispatch.test/i/mastodon.png" width="24" height="24" alt="Mastodon"></a>&nbsp;<a href="https://photos.test/dispatch"><img src="https://cdn.dispatch.test/i/photos.png" width="24" height="24" alt="Photos"></a>&nbsp;<a href="https://dispatch.test/rss"><img src="https://cdn.dispatch.test/i/rss.png" width="24" height="24" alt="RSS"></a>
</td></tr>
<tr><td align="center" style="padding:16px 32px 28px;font-size:11px;line-height:1.5;color:#999;background:#f6f4ef">
The Lakeside Dispatch &middot; 4 Quay Row, Lakeside &middot; You get this because you subscribed at dispatch.test.<br>
<a href="https://dispatch.test/prefs" style="color:#999">Preferences</a> &middot; <a href="https://dispatch.test/u?x=1" style="color:#999">Unsubscribe</a>
</td></tr>
</table><img src="https://t.dispatch.test/open.gif?u=1" width="1" height="1" alt="">
</td></tr></table></body></html>"#;

/// A Gmail reply with a data table, inline `<code>` inside a nested list,
/// a link and a nested quote.
const ROWS_REPLY_HTML: &str = r#"<div dir="ltr"><div>Two lines is enough. Here are the numbers from the test, so you can check me:</div><div><br></div>
<table border="1" cellpadding="4" style="border-collapse:collapse"><tr><th>Density</th><th>Row height</th><th>Rows on screen</th><th>Notes</th></tr>
<tr><td>Compact</td><td>44 px</td><td>19</td><td>Snippet cut early on narrow tiles</td></tr>
<tr><td>Default</td><td>52 px</td><td>16</td><td>Who, then subject and snippet</td></tr>
<tr><td>Roomy</td><td>64 px</td><td>13</td><td>Feels slow with j held down</td></tr></table>
<div><br></div><div>What I'd do, in order:</div>
<ol><li>Ship <b>Default</b>.</li><li>Keep <code>Compact</code> behind a setting.<ol><li>Only if someone asks.</li></ol></li><li>Drop Roomy.</li></ol>
<div>The paperclip is the diagram; the spec is <a href="https://design.test/rows">on the wiki</a>.</div>
<div><br></div><div class="gmail_quote"><div dir="ltr" class="gmail_attr">On Sun, Nov 15, 2026 at 9:02 AM Tessa &lt;tessa@harbor.test&gt; wrote:<br></div><blockquote class="gmail_quote" style="margin:0 0 0 .8ex;border-left:1px solid #ccc;padding-left:1ex">How many lines should a row be?<br><blockquote class="gmail_quote">Earlier: three felt like a lot.</blockquote></blockquote></div></div>"#;

const VENUE_FIRST_HTML: &str = r#"<div style="font-family:Aptos,sans-serif;font-size:12pt">Two options: the hall on <b>Mill Lane</b>, or the back room at the library. Mill Lane is bigger.</div><div><br></div><div id="ms-outlook-mobile-signature">Get Outlook for Android</div>"#;

/// Outlook's reply: no `>` and no blockquote, only a `From:` / `Sent:`
/// header over each earlier message.
const VENUE_REPLY_HTML: &str = r#"<div style="font-family:Aptos,sans-serif;font-size:12pt">Booked. The hall on Mill Lane holds 80, and they'll leave the projector up for us.</div><div style="font-family:Aptos,sans-serif;font-size:12pt">Deposit is paid; I'll send the receipt to Sam.</div><div><br></div><div id="ms-outlook-mobile-signature">Get Outlook for Android</div><div id="appendonsend"></div><hr style="display:inline-block;width:98%"><div id="divRplyFwdMsg" dir="ltr"><font face="Calibri, sans-serif" style="font-size:11pt"><b>From:</b> Tessa &lt;tessa@harbor.test&gt;<br><b>Sent:</b> Tuesday, November 17, 2026 4:02 PM<br><b>To:</b> Priya Natarajan &lt;priya@harbor.test&gt;<br><b>Subject:</b> Re: Venue for the launch</font><div>&nbsp;</div></div><div>Mill Lane works for me. Can you check they have a projector?</div><div><br></div><hr style="display:inline-block;width:98%"><div><font face="Calibri, sans-serif" style="font-size:11pt"><b>From:</b> Priya Natarajan &lt;priya@harbor.test&gt;<br><b>Sent:</b> Tuesday, November 17, 2026 11:40 AM<br><b>To:</b> Tessa &lt;tessa@harbor.test&gt;<br><b>Subject:</b> Venue for the launch</font></div><div>Two options: the hall on Mill Lane, or the back room at the library. Mill Lane is bigger.</div>"#;

/// The newsletter answers at its own address, so `r` on it shows Reply-To.
fn reply_address(id: &str) -> Vec<Address> {
    match id {
        "weekly@fell.test" => people(&[("Weekly replies", "weekly-replies@harbor.test")]),
        _ => Vec::new(),
    }
}

/// Names for the fixture attachments, as a forward lists them.
fn attachment_names(row: &DraftMail) -> Vec<String> {
    if !row.attachment {
        return Vec::new();
    }
    let names: &[&str] = match row.id {
        "rows@fell.test" => &["inbox-rows.png"],
        "tickets@fell.test" => &["tickets-friday.pdf", "seat-map.png"],
        _ => &["attachment"],
    };
    names.iter().map(|name| (*name).to_string()).collect()
}

fn first_line(body: &str) -> String {
    body.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixtures_land_in_the_store_and_span_folders() {
        let store = Store::open_in_memory().unwrap();
        let account = fixture_account();
        let report = seed_if_empty(&store, &account).unwrap();
        assert!(report.upserted > 10);
        let inbox = store.folder_threads(&account.id, "INBOX").unwrap();
        assert!(inbox.len() >= 10);
        let sent_copy = inbox
            .iter()
            .find(|thread| thread.message_count > 1)
            .unwrap();
        let messages = store
            .thread_messages(&account.id, &sent_copy.thread_id)
            .unwrap();
        assert!(
            messages
                .iter()
                .any(|message| message.folders.iter().any(|folder| folder == "Sent"))
        );
        assert!(
            messages
                .iter()
                .any(|message| message.folders.iter().any(|folder| folder == "INBOX"))
        );
        assert!(inbox.iter().any(|thread| thread.starred));
        assert!(inbox.iter().any(|thread| thread.has_attachment));
        assert_eq!(
            store.folder_threads(&account.id, "Drafts").unwrap().len(),
            1
        );
        let again = seed_if_empty(&store, &account).unwrap();
        assert_eq!(again.upserted, 0);
    }

    #[test]
    fn the_html_fixtures_are_stored_and_fold_as_their_clients_write_them() {
        use fell_render::Fold;
        let store = Store::open_in_memory().unwrap();
        let account = fixture_account();
        seed_if_empty(&store, &account).unwrap();
        let body = |id: &str| {
            let job =
                crate::mail_body::read_job(&store, &account.id, id, &[]).expect("stored HTML");
            job.prepare().body
        };
        let folds = |id: &str| {
            body(id)
                .folded
                .sections
                .iter()
                .map(|section| section.fold)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            folds("cabin-2@fell.test"),
            [None, Some(Fold::SignatureAndQuote)]
        );
        // "Get Outlook for Android" is one line: it shows in place, dimmed.
        assert_eq!(folds("venue-3@fell.test"), [None, None, Some(Fold::Quote)]);
        assert!(body("venue-3@fell.test").folded.sections[1].quiet);
        assert_eq!(folds("rows@fell.test"), [None, Some(Fold::Quote)]);
        // The newsletter is bulk mail: nothing folds, not even its letter.
        assert_eq!(folds("dispatch-42@fell.test"), [None]);
        // Its hidden preheader and open pixel never reach the page.
        let dispatch = body("dispatch-42@fell.test");
        let text = dispatch.folded.document.plain_text();
        assert!(text.starts_with("Having trouble reading this?"), "{text}");
        assert!(!text.contains("This week: low water"), "{text}");
        assert!(!format!("{:?}", dispatch.folded.document).contains("open.gif"));
        let threads = store.folder_threads(&account.id, "INBOX").unwrap();
        let venue = threads
            .iter()
            .find(|thread| thread.subject.contains("Venue"))
            .unwrap();
        assert_eq!(venue.message_count, 3);
        // Every fixture address is on a reserved test or example domain.
        for message in fixture_messages(&account) {
            let email = &message.headers.from.email;
            assert!(
                email.ends_with(".test")
                    || email.ends_with(".example")
                    || [
                        ".example.com",
                        ".example.net",
                        ".example.org",
                        "@example.net",
                        "@example.org",
                        "@example.com"
                    ]
                    .iter()
                    .any(|end| email.ends_with(end)),
                "{email}"
            );
        }
        // Every corpus family is in the demo mailbox, and stored as HTML.
        for (id, _) in CORPUS_MAIL {
            assert!(
                crate::mail_body::read_job(&store, &account.id, id, &[]).is_some(),
                "{id}"
            );
        }
    }
}
