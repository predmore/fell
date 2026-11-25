//! Deterministic data for the store performance measurements.

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::path::Path;

use crate::embed::{BodyEmbedder, try_embed_lock};
use crate::store::{DROP_FTS_TRIGGERS, ensure_fts};
use crate::{
    Account, AccountId, Address, BackendKind, Disposition, Error, MailboxDelta, Message,
    MessageHeaders, MessageId, MessageParts, MimePart, PartRole, RemoteId, Result, Store, ThreadId,
    TransferEncoding,
};

pub const MESSAGES: usize = 100_000;
pub const LONG_MESSAGES: usize = 50;
pub const LONG_THREADS: usize = 20;
const BATCH: usize = 1_000;
const DATE: i64 = 1_700_000_000;
const BASES: usize = 128;
const FOLDERS: [&str; 4] = ["INBOX", "Sent", "Archive", "Work"];
const TOPICS: [&str; 8] = [
    "Project review and quarterly planning",
    "Local event and ticket information",
    "Room booking and travel itinerary",
    "School calendar and class registration",
    "Weekly gardening and weather update",
    "Delivery status and shipping details",
    "Community group notes and schedule",
    "Account details and service changes",
];
const WORDS: &[&str] = &[
    "order", "shipping", "account", "update", "weekly", "digest", "meeting", "project", "review",
    "summer", "launch", "team", "notes", "draft", "travel", "receipt", "offer", "member", "event",
    "report", "photos", "tickets", "garden",
];

pub struct Rng(pub u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    // Roughly normal, mean 0, deviation 1.
    fn gauss(&mut self) -> f32 {
        let sum: f32 = (0..6)
            .map(|_| (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32)
            .sum();
        (sum - 3.0) * std::f32::consts::SQRT_2
    }
}

/// Reuse a base model vector with deterministic jitter. These rows exercise
/// the real vector scan while keeping 100k model inferences out of setup.
pub fn noisy_vector(rng: &mut Rng, base: &[f32]) -> Vec<u8> {
    let values: Vec<f32> = base
        .iter()
        .map(|value| value + rng.gauss() * 0.01)
        .collect();
    let norm = values.iter().map(|v| v * v).sum::<f32>().sqrt();
    values
        .iter()
        .flat_map(|value| (value / norm).to_le_bytes())
        .collect()
}

pub fn account() -> Account {
    Account {
        id: AccountId("bench".into()),
        email: "me@bench.test".into(),
        display_name: "Bench Reader".into(),
        backend_kind: BackendKind::Bridge,
    }
}

fn message_id(index: usize) -> MessageId {
    MessageId::new(format!("m{index}@bench.test"))
}

pub fn long_thread(index: usize) -> ThreadId {
    ThreadId::from_message_id(&message_id(index * LONG_MESSAGES))
}

/// Hold the normal worker lock during setup and measurement. The store still
/// opens through its production path, without concurrent embedding/index writes.
pub fn hold_indexer(path: &Path) -> Result<rusqlite::Connection> {
    try_embed_lock(path)?.ok_or_else(|| Error::Invalid("benchmark embed lock already held".into()))
}

/// Build through the same batched write path as sync. Copies count towards
/// server deliveries, while the 100k count is for distinct stored messages.
pub fn seed(path: &Path) -> Result<()> {
    OpenOptions::new().write(true).create_new(true).open(path)?;
    let _index_lock = hold_indexer(path)?;
    let store = Store::open(path)?;
    // Index the finished fixture in bulk. While generating it, each Sent
    // copy would otherwise scan the growing FTS table to replace its row.
    let conn = store.connection();
    conn.execute_batch(DROP_FTS_TRIGGERS)?;
    let account = account();
    store.upsert_account(&account)?;
    let mut rng = Rng(17);
    let mut root = 0;
    let mut end = LONG_MESSAGES;
    let mut batch = Vec::with_capacity(BATCH + BATCH / 32);
    for index in 0..MESSAGES {
        if index == end {
            root = index;
            let len = if index < LONG_THREADS * LONG_MESSAGES {
                LONG_MESSAGES
            } else {
                match rng.below(100) {
                    0..50 => 1,
                    50..75 => 2 + rng.below(3),
                    75..90 => 5 + rng.below(6),
                    90..98 => 11 + rng.below(20),
                    _ => 31 + rng.below(50),
                }
            };
            end = index + len;
        }
        let message = message(&account, &mut rng, index, root);
        if index.is_multiple_of(32) {
            let mut copy = message.clone();
            copy.folders = vec!["Sent".into()];
            copy.remote_id = RemoteId(format!("imap:1:{}:Sent", index + 1));
            copy.unread = false;
            batch.push(copy);
        }
        batch.push(message);
        if (index + 1).is_multiple_of(BATCH) {
            store.apply_delta(
                &account.id,
                &MailboxDelta {
                    messages: std::mem::take(&mut batch),
                    ..MailboxDelta::default()
                },
            )?;
        }
    }
    ensure_fts(conn)?;
    let (rows, indexed): (i64, i64) = conn.query_row(
        "SELECT (SELECT count(*) FROM messages), (SELECT count(*) FROM messages_fts)",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(rows, MESSAGES as i64);
    assert_eq!(indexed, rows);
    Ok(())
}

/// Add newer, one-message Inbox threads that start out snoozed. Keeping this
/// phase separate preserves the baseline measurements on the 100k fixture.
pub fn add_snoozed_threads(store: &Store, count: usize) -> Result<()> {
    let account = account();
    let mut rng = Rng(17);
    for start in (MESSAGES..MESSAGES + count).step_by(BATCH) {
        let end = (start + BATCH).min(MESSAGES + count);
        let messages = (start..end)
            .map(|index| {
                let mut row = message(&account, &mut rng, index, index);
                row.headers.date_unix = 2_000_000_000 + index as i64;
                row.remote_id = RemoteId(format!("imap:1:{}:INBOX", index + 1));
                row.folders = vec!["INBOX".into()];
                row.unread = true;
                row.snooze_until = Some(i64::MAX);
                row
            })
            .collect();
        store.apply_delta(
            &account.id,
            &MailboxDelta {
                messages,
                ..MailboxDelta::default()
            },
        )?;
    }
    Ok(())
}

fn message(account: &Account, rng: &mut Rng, index: usize, root: usize) -> Message {
    let id = message_id(index);
    let parent = (index > root).then(|| message_id(index - 1));
    let list = index.is_multiple_of(32);
    let folder = if list || index == root && root < LONG_THREADS * LONG_MESSAGES {
        "INBOX"
    } else {
        FOLDERS[rng.below(FOLDERS.len())]
    };
    let sender = if folder == "Sent" || list {
        account.email.clone()
    } else {
        format!("p{}@bench.test", index % 12)
    };
    let intro = intro(index);
    let body_len = match index % 100 {
        0 => 20_000,
        1..=3 => 6_000 + rng.below(14_001),
        4..=15 => 2_000 + rng.below(4_001),
        16 => 512,
        _ => 512 + rng.below(1_489),
    };
    let mut body = format!("{intro}\n\nMessage reference {index}.\n\n");
    while body.len() < body_len {
        for _ in 0..12 {
            let _ = write!(body, "{} ", WORDS[rng.below(WORDS.len())]);
        }
        body.push_str("\n\n");
        if index > root {
            body.push_str("> Thanks for the earlier note.\n\n");
        }
    }
    body.truncate(body_len);
    let has_attachment = index.is_multiple_of(5);
    let parts = MessageParts {
        mime_parts: if has_attachment {
            vec![MimePart {
                section: "2".into(),
                role: PartRole::Attachment,
                mime_type: "application/pdf".into(),
                filename: Some("details.pdf".into()),
                size: 24_000,
                encoded_size: 32_000,
                encoding: TransferEncoding::Base64,
                content_id: None,
                disposition: Disposition::Attachment,
                nested: false,
            }]
        } else {
            Vec::new()
        },
        html: list.then(|| {
            format!(
                "<html><body><p>{}</p></body></html>",
                body.replace('\n', "<br>")
            )
        }),
        plain: Some(body.clone()),
        bulk_headers: if list {
            vec![(
                "List-Unsubscribe".into(),
                "<https://list.bench.test/u>".into(),
            )]
        } else {
            Vec::new()
        },
    };
    Message {
        account_id: account.id.clone(),
        headers: MessageHeaders {
            message_id: id,
            proton_conversation_id: None,
            in_reply_to: parent.clone(),
            references: parent.into_iter().collect(),
            from: Address {
                name: Some(format!("Reader {}", index % 12)),
                email: sender,
            },
            subject: format!("{} ({root})", TOPICS[root % TOPICS.len()]),
            date_unix: DATE + index as i64 * 60,
        },
        remote_id: RemoteId(format!("imap:1:{}:{folder}", index + 1)),
        thread_id: ThreadId::from_message_id(&message_id(root)),
        snippet: intro.into(),
        body_text: Some(body),
        unread: !list && index.is_multiple_of(7),
        folders: vec![folder.into()],
        labels: if index.is_multiple_of(9) {
            vec!["tag1".into()]
        } else {
            Vec::new()
        },
        arrived_via: Vec::new(),
        to: vec![Address {
            name: Some(account.display_name.clone()),
            email: account.email.clone(),
        }],
        cc: Vec::new(),
        bcc: Vec::new(),
        starred: index.is_multiple_of(11),
        has_attachment,
        archived: false,
        snooze_until: None,
        list_id: list.then(|| "weekly.list.bench.test".into()),
        reply_to: Some(Vec::new()),
        attachment_names: Vec::new(),
        parts: Some(parts),
    }
}

fn intro(index: usize) -> &'static str {
    match index % BASES {
        1..=4 => "The invoice is due next month.",
        5..=7 => "Please check the boiler for a leak.",
        8..=9 => "Download your boarding pass before you travel.",
        10..=12 => "The warranty details are in the letter.",
        _ => TOPICS[index % TOPICS.len()],
    }
}

pub fn seed_vectors(store: &Store, model: &dyn BodyEmbedder) -> Result<()> {
    let bases: Vec<_> = (0..BASES)
        .map(|index| model.embed(intro(index)))
        .collect::<std::result::Result<_, _>>()
        .map_err(|err| Error::Search(err.to_string()))?;
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let tx = store.connection().unchecked_transaction()?;
    {
        let mut insert = tx.prepare(
            "INSERT INTO message_embeddings
                (account_id, message_id, model, model_version, dims, body_hash, vector)
             VALUES (?1, ?2, ?3, ?4, ?5, '', ?6)",
        )?;
        let account = account();
        let model_id = model.id();
        for index in 0..MESSAGES {
            insert.execute(rusqlite::params![
                account.id.as_str(),
                message_id(index).as_str(),
                model_id.model,
                model_id.version,
                model.dims() as i64,
                noisy_vector(&mut rng, &bases[index % BASES]),
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}
