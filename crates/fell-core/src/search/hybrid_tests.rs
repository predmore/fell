//! Hybrid search: quality, filters, keyword parity, and following the index.
//!
//! CI cannot download all-MiniLM-L6-v2, so these tests use [`Concepts`], a
//! tiny deterministic test model: each dimension is one topic, and a word
//! counts towards its topic. Paraphrases that share no word with a message
//! still land near it, which is the property hybrid search relies on. The
//! mail is invented, on `.test` domains.

use std::time::{Duration, Instant};

use super::*;
use crate::account::{Account, BackendKind};
use crate::embed::{
    BodyEmbedder, EmbedderId, LocalHashV1, ModelError, SendEmbedder, index_embeddings,
    index_embeddings_with, testing_embedder,
};
use crate::message::{Address, Message, MessageHeaders, RemoteId};
use crate::store::Store;

/// Topic words. A word's topic is its dimension.
const TOPICS: &[&[&str]] = &[
    &[
        "invoice", "bill", "billing", "payment", "pay", "paid", "owed", "owe", "money", "due",
        "overdue", "balance", "refund", "charge", "cost",
    ],
    &[
        "plumber",
        "plumbing",
        "contractor",
        "builder",
        "builders",
        "electrician",
        "repair",
        "leak",
        "pipe",
        "boiler",
        "tradesman",
    ],
    &[
        "flight",
        "plane",
        "airline",
        "boarding",
        "airport",
        "departure",
        "fly",
        "trip",
    ],
    &[
        "hotel",
        "booking",
        "reservation",
        "room",
        "stay",
        "accommodation",
        "lodging",
    ],
    &[
        "dentist",
        "dental",
        "teeth",
        "tooth",
        "checkup",
        "hygienist",
        "filling",
    ],
    &[
        "birthday",
        "party",
        "celebration",
        "cake",
        "celebrate",
        "surprise",
        "candles",
    ],
    &[
        "password",
        "login",
        "credentials",
        "reset",
        "locked",
        "verification",
        "security",
    ],
    &[
        "meeting", "standup", "agenda", "minutes", "sync", "call", "catchup",
    ],
    &[
        "budget",
        "forecast",
        "spreadsheet",
        "quarterly",
        "finance",
        "numbers",
        "projections",
    ],
    &[
        "puppy",
        "dog",
        "vet",
        "veterinarian",
        "pet",
        "kennel",
        "vaccination",
    ],
    &[
        "lease",
        "rent",
        "landlord",
        "apartment",
        "tenant",
        "flat",
        "tenancy",
    ],
    &[
        "package",
        "parcel",
        "delivery",
        "shipped",
        "courier",
        "tracking",
        "dispatched",
    ],
    &[
        "recipe",
        "dinner",
        "cook",
        "cooking",
        "ingredients",
        "bake",
        "oven",
    ],
    &[
        "school",
        "teacher",
        "class",
        "homework",
        "pupils",
        "classroom",
        "term",
    ],
    &[
        "car", "mechanic", "garage", "tyre", "tyres", "engine", "brakes",
    ],
    &[
        "garden", "lawn", "hedge", "plants", "seeds", "gardener", "compost",
    ],
];

/// The test model: a count of topic words, L2-normalised.
pub(crate) struct Concepts;

impl BodyEmbedder for Concepts {
    fn id(&self) -> EmbedderId {
        EmbedderId {
            model: "test-concepts",
            version: "1",
        }
    }

    fn dims(&self) -> usize {
        TOPICS.len()
    }

    fn embed(&self, body: &str) -> std::result::Result<Vec<f32>, ModelError> {
        let mut values = vec![0.0f32; TOPICS.len()];
        for word in body
            .to_lowercase()
            .split(|ch: char| !ch.is_alphanumeric())
            .filter(|word| !word.is_empty())
        {
            for (dim, words) in TOPICS.iter().enumerate() {
                if words.contains(&word) {
                    values[dim] += 1.0;
                }
            }
        }
        let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
        if norm > 0.0 {
            for value in &mut values {
                *value /= norm;
            }
        }
        Ok(values)
    }
}

fn concepts() -> SendEmbedder {
    Box::new(Concepts)
}

fn placeholder() -> SendEmbedder {
    Box::new(LocalHashV1)
}

/// Resets the test embedder when a test ends, even on a panic.
struct EmbedderReset;

impl Drop for EmbedderReset {
    fn drop(&mut self) {
        testing_embedder(None);
    }
}

fn account_id() -> AccountId {
    AccountId("acct".into())
}

fn account() -> Account {
    Account {
        id: account_id(),
        display_name: "Robin".into(),
        email: "robin@home.test".into(),
        backend_kind: BackendKind::Bridge,
    }
}

/// One invented message: `(id, sender, subject, body)`.
type Mail = (&'static str, &'static str, &'static str, &'static str);

const CORPUS: &[Mail] = &[
    (
        "plumber-invoice",
        "accounts@brightline-plumbing.test",
        "Invoice 4471",
        "Invoice 4471 for the kitchen leak repair is due on Friday.",
    ),
    (
        "boiler-quote",
        "office@brightline-plumbing.test",
        "Boiler service",
        "We can repair the boiler pipe next Tuesday morning.",
    ),
    (
        "flight-confirm",
        "noreply@skyways.test",
        "Your itinerary",
        "Boarding opens at gate 12. Your flight departs the airport at 9:40.",
    ),
    (
        "hotel-confirm",
        "stay@harbour-hotel.test",
        "Reservation 88213",
        "Your room reservation for three nights is confirmed.",
    ),
    (
        "dentist-reminder",
        "frontdesk@smile-dental.test",
        "Reminder",
        "Your dental checkup with the hygienist is on 3 March.",
    ),
    (
        "party-invite",
        "sam@friends.test",
        "Saturday!",
        "Surprise birthday party for Jo, bring candles for the cake.",
    ),
    (
        "password-reset",
        "security@cloudvault.test",
        "Action needed",
        "Use this link to reset your login credentials within an hour.",
    ),
    (
        "standup-notes",
        "lee@work.test",
        "Notes from today",
        "Minutes from the standup: the agenda for Thursday is attached.",
    ),
    (
        "budget-q3",
        "finance@work.test",
        "Q3",
        "The quarterly forecast spreadsheet has the revised projections.",
    ),
    (
        "vet-visit",
        "clinic@pawsvet.test",
        "Booster due",
        "The puppy vaccination with the veterinarian is next week.",
    ),
    (
        "lease-renewal",
        "lettings@oakfield-homes.test",
        "Renewal",
        "Your landlord has sent the tenancy renewal for the flat.",
    ),
    (
        "parcel-out",
        "updates@quickpost.test",
        "On its way",
        "Your parcel was dispatched with the courier. Tracking: QP99812.",
    ),
    (
        "recipe-share",
        "nan@family.test",
        "That stew",
        "Here are the ingredients. Cook it slowly in the oven.",
    ),
    (
        "school-trip",
        "office@greenhill-school.test",
        "Year 4",
        "The class teacher asks pupils to finish the homework before term ends.",
    ),
    (
        "garage-mot",
        "bookings@kwikfix-garage.test",
        "Ready for collection",
        "The mechanic replaced the brakes and two tyres.",
    ),
    (
        "garden-tips",
        "hello@greenfingers.test",
        "Autumn jobs",
        "Trim the hedge and spread compost before the lawn sleeps.",
    ),
    (
        "newsletter-1",
        "news@daily-digest.test",
        "Morning digest",
        "Markets opened higher and the weather stays mild this week.",
    ),
    (
        "newsletter-2",
        "news@daily-digest.test",
        "Evening digest",
        "A new museum opens downtown and the bridge reopens on Monday.",
    ),
    (
        "chat-1",
        "alex@friends.test",
        "hey",
        "Are you around later? Want to grab a coffee.",
    ),
    (
        "chat-2",
        "alex@friends.test",
        "re: hey",
        "Sounds good, see you at the usual place.",
    ),
    (
        "car-insurance",
        "renewals@safedrive.test",
        "Policy",
        "Your car policy renews next month. No action is needed.",
    ),
    (
        "rent-receipt",
        "lettings@oakfield-homes.test",
        "Receipt",
        "We received this month's rent for the apartment. Thank you.",
    ),
    (
        "refund",
        "support@shoply.test",
        "Refund issued",
        "A refund for your order has been issued to your card.",
    ),
    (
        "team-call",
        "lee@work.test",
        "Moved",
        "The sync call moved to 3pm, same agenda as before.",
    ),
];

/// A paraphrased query and the messages it should find. None of them shares
/// a word with the query, so keyword search cannot.
const PARAPHRASES: &[(&str, &[&str])] = &[
    ("money owed plumber", &["plumber-invoice"]),
    ("when do i fly", &["flight-confirm"]),
    ("lodging booking", &["hotel-confirm"]),
    ("teeth appointment", &["dentist-reminder"]),
    ("celebration plans", &["party-invite"]),
    ("locked out security", &["password-reset"]),
    ("dog jab", &["vet-visit"]),
    ("tenant paperwork", &["lease-renewal", "rent-receipt"]),
    ("delivery status", &["parcel-out"]),
    ("dinner idea", &["recipe-share"]),
    ("engine work done", &["garage-mot"]),
    ("finance numbers", &["budget-q3"]),
];

/// Queries whose words are in the mail. Hybrid must keep these hits.
const EXACT: &[(&str, &[&str])] = &[
    ("invoice", &["plumber-invoice"]),
    ("courier", &["parcel-out"]),
    ("museum", &["newsletter-2"]),
];

fn mail(id: &str, sender: &str, subject: &str, body: &str, date: i64) -> Message {
    let message_id = MessageId::new(format!("{id}@corpus.test"));
    Message {
        account_id: account_id(),
        headers: MessageHeaders {
            message_id: message_id.clone(),
            proton_conversation_id: None,
            in_reply_to: None,
            references: Vec::new(),
            from: Address {
                name: None,
                email: sender.into(),
            },
            subject: subject.into(),
            date_unix: date,
        },
        remote_id: RemoteId(format!("acct:{id}")),
        thread_id: ThreadId::from_message_id(&message_id),
        snippet: body.chars().take(60).collect(),
        body_text: Some(body.into()),
        unread: false,
        folders: vec!["INBOX".into()],
        labels: Vec::new(),
        arrived_via: Vec::new(),
        to: Vec::new(),
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
}

fn corpus_mail() -> Vec<Message> {
    CORPUS
        .iter()
        .enumerate()
        .map(|(index, (id, sender, subject, body))| {
            mail(
                id,
                sender,
                subject,
                body,
                1_700_000_000 + index as i64 * 600,
            )
        })
        .collect()
}

fn fill(store: &Store, messages: &[Message]) {
    store.upsert_account(&account()).unwrap();
    for message in messages {
        store.upsert_message(message).unwrap();
    }
}

fn index_with(store: &Store, embedder: &dyn BodyEmbedder) {
    while index_embeddings_with(&store.conn, 256, embedder).unwrap() > 0 {}
}

/// An in-memory store of the corpus, indexed and searched with [`Concepts`].
fn concept_store() -> (Store, EmbedderReset) {
    let reset = EmbedderReset;
    testing_embedder(Some(concepts));
    let store = Store::open_in_memory().unwrap();
    fill(&store, &corpus_mail());
    index_with(&store, &Concepts);
    (store, reset)
}

fn ids(results: &SearchResults) -> Vec<String> {
    results
        .hits
        .iter()
        .map(|hit| {
            hit.message_id
                .as_str()
                .trim_end_matches("@corpus.test")
                .to_string()
        })
        .collect()
}

fn keyword(store: &Store, query: &str) -> SearchResults {
    store
        .search_with(&account_id(), query, SearchOptions { semantic: false })
        .unwrap()
}

fn hybrid(store: &Store, query: &str) -> SearchResults {
    store.search(&account_id(), query).unwrap()
}

fn recall_at_10(found: &[String], relevant: &[&str]) -> f64 {
    let top: Vec<&str> = found.iter().take(10).map(String::as_str).collect();
    let hit = relevant.iter().filter(|id| top.contains(id)).count();
    hit as f64 / relevant.len() as f64
}

fn mean_recall(store: &Store, queries: &[(&str, &[&str])], semantic: bool) -> f64 {
    let total: f64 = queries
        .iter()
        .map(|(query, relevant)| {
            let results = if semantic {
                hybrid(store, query)
            } else {
                keyword(store, query)
            };
            recall_at_10(&ids(&results), relevant)
        })
        .sum();
    total / queries.len() as f64
}

#[test]
fn hybrid_beats_keyword_on_paraphrased_queries() {
    let (store, _reset) = concept_store();
    assert!(store.semantic_search_active());
    let keyword_recall = mean_recall(&store, PARAPHRASES, false);
    let hybrid_recall = mean_recall(&store, PARAPHRASES, true);
    println!("recall@10 on paraphrases: keyword {keyword_recall:.2}, hybrid {hybrid_recall:.2}");
    assert!(
        hybrid_recall >= 0.9 && hybrid_recall > keyword_recall + 0.5,
        "keyword {keyword_recall:.2}, hybrid {hybrid_recall:.2}"
    );
    // Exact words still win: hybrid keeps every keyword hit, first.
    for (query, relevant) in EXACT {
        let words = ids(&keyword(&store, query));
        let fused = ids(&hybrid(&store, query));
        assert_eq!(recall_at_10(&words, relevant), 1.0, "{query}");
        assert_eq!(recall_at_10(&fused, relevant), 1.0, "{query}");
        assert_eq!(fused[0], relevant[0], "{query}: {fused:?}");
    }
}

#[test]
fn only_semantic_hits_are_marked() {
    let (store, _reset) = concept_store();
    // "invoice" is a word in one message; others are only close in meaning.
    let results = hybrid(&store, "invoice");
    let first = &results.hits[0];
    assert_eq!(first.message_id.as_str(), "plumber-invoice@corpus.test");
    assert!(!first.semantic_only);
    let related: Vec<String> = results
        .hits
        .iter()
        .filter(|hit| hit.semantic_only)
        .map(|hit| hit.message_id.as_str().to_string())
        .collect();
    assert!(
        related.contains(&"refund@corpus.test".to_string()),
        "{related:?}"
    );
    for hit in &keyword(&store, "invoice").hits {
        assert!(!hit.semantic_only);
    }
    // A paraphrase matches no word, so every hit is related only.
    let results = hybrid(&store, "money owed plumber");
    assert!(!results.hits.is_empty());
    assert!(results.hits.iter().all(|hit| hit.semantic_only));
}

#[test]
fn operators_filter_the_semantic_side() {
    let reset = EmbedderReset;
    testing_embedder(Some(concepts));
    let store = Store::open_in_memory().unwrap();
    let mut messages = corpus_mail();
    for message in &mut messages {
        if message.headers.message_id.as_str() == "plumber-invoice@corpus.test" {
            message.unread = true;
            message.folders = vec!["Archive".into()];
        }
    }
    fill(&store, &messages);
    index_with(&store, &Concepts);
    let found = |query: &str| ids(&hybrid(&store, query));
    assert!(found("money owed").contains(&"refund".to_string()));
    assert_eq!(found("money owed in:Archive"), ["plumber-invoice"]);
    assert_eq!(found("money owed is:unread"), ["plumber-invoice"]);
    assert_eq!(found("money owed from:brightline"), ["plumber-invoice"]);
    assert_eq!(found("money owed from:shoply"), ["refund"]);
    assert!(found("money owed before:2000-01-01").is_empty());
    assert!(found("money owed has:nothing").is_empty());
    // Operators alone have no words to embed: that is keyword search.
    assert_eq!(hybrid(&store, "in:Archive"), keyword(&store, "in:Archive"));
    drop(reset);
}

#[test]
fn quoted_replies_do_not_crowd_out_related_threads() {
    let _reset = EmbedderReset;
    testing_embedder(Some(concepts));
    let store = Store::open_in_memory().unwrap();

    // The old 300-message window filled with eight 40-message threads.
    // Each reply quotes the preceding note and scores above the hotel bill.
    let mut messages = Vec::new();
    for thread in 0..8 {
        let mut parent = None;
        let mut previous_note = String::new();
        for reply in 0..40 {
            let note = format!("Payment update {reply}. Please check the invoice.");
            let body = if previous_note.is_empty() {
                note.clone()
            } else {
                format!("{note}\n\n> {previous_note}")
            };
            let mut message = mail(
                &format!("billing-{thread}-{reply}"),
                "accounts@work.test",
                &format!("Paperwork {thread}"),
                &body,
                1_700_000_000 + thread * 40 + reply,
            );
            message.headers.in_reply_to = parent.replace(message.headers.message_id.clone());
            messages.push(message);
            previous_note = note;
        }
    }
    let hotel = mail(
        "hotel-bill",
        "desk@hotel.test",
        "Receipt",
        "Your bill covers the room, booking and lodging.",
        1_700_001_000,
    );
    let hotel_id = hotel.headers.message_id.clone();
    messages.push(hotel);
    fill(&store, &messages);
    index_with(&store, &Concepts);

    assert!(keyword(&store, "money owed").hits.is_empty());
    let results = hybrid(&store, "money owed");
    assert_eq!(results.hits.len(), 9);
    assert!(results.hits.iter().all(|hit| hit.semantic_only));
    assert_eq!(results.hits.last().unwrap().message_id, hotel_id);
}

#[test]
fn without_a_model_search_is_exactly_keyword_search() {
    let queries = [
        "invoice",
        "money owed plumber",
        "invioce",
        "\"kitchen leak\"",
        "from:brightline repair",
        "in:INBOX",
        "is:unread",
        "has:attachment",
        "subject:reminder",
        "before:2024-01-01 after:2020-01-01 rent",
        "   ",
        "has:nothing",
    ];
    // No model installed (the default in tests): nothing loads, and the
    // placeholder vectors the indexer stored are never read for ranking.
    let store = Store::open_in_memory().unwrap();
    fill(&store, &corpus_mail());
    while index_embeddings(&store.conn, 256).unwrap() > 0 {}
    assert!(!store.semantic_search_active());
    for query in queries {
        let default = hybrid(&store, query);
        assert_eq!(default, keyword(&store, query), "{query}");
        assert!(default.hits.iter().all(|hit| !hit.semantic_only));
    }
    assert_eq!(ids(&hybrid(&store, "invoice")), ["plumber-invoice"]);
    assert!(hybrid(&store, "money owed plumber").hits.is_empty());
    assert_eq!(
        hybrid(&store, "invioce").did_you_mean.as_deref(),
        Some("invoice")
    );

    // The placeholder as the active embedder is not semantic either.
    let _reset = EmbedderReset;
    testing_embedder(Some(placeholder));
    let store = Store::open_in_memory().unwrap();
    fill(&store, &corpus_mail());
    while index_embeddings(&store.conn, 256).unwrap() > 0 {}
    assert!(!store.semantic_search_active());
    for query in queries {
        assert_eq!(hybrid(&store, query), keyword(&store, query), "{query}");
    }
}

#[test]
fn semantic_off_is_keyword_search_with_a_model() {
    let (store, _reset) = concept_store();
    let results = keyword(&store, "money owed plumber");
    assert!(results.hits.is_empty());
    assert_eq!(
        keyword(&store, "invoice"),
        execute_keyword(&store.conn, &account_id(), "invoice").unwrap()
    );
}

#[test]
fn the_matrix_follows_new_and_deleted_vectors() {
    let (store, _reset) = concept_store();
    assert!(!ids(&hybrid(&store, "dog jab")).contains(&"kennel".to_string()));
    let kennel = mail(
        "kennel",
        "desk@happy-kennels.test",
        "Your stay",
        "The kennel has room for the puppy over the holiday.",
        1_800_000_000,
    );
    store.upsert_message(&kennel).unwrap();
    // Not indexed yet: no vector, so no semantic hit.
    assert!(!ids(&hybrid(&store, "dog jab")).contains(&"kennel".to_string()));
    index_with(&store, &Concepts);
    assert!(ids(&hybrid(&store, "dog jab")).contains(&"kennel".to_string()));
    // A deleted message leaves the matrix with its vector.
    store
        .delete_message(&account_id(), &kennel.headers.message_id)
        .unwrap();
    assert!(!ids(&hybrid(&store, "dog jab")).contains(&"kennel".to_string()));
    // A rewritten body moves the row: the vet visit becomes a recipe.
    let mut vet = corpus_mail()
        .into_iter()
        .find(|message| message.headers.message_id.as_str() == "vet-visit@corpus.test")
        .unwrap();
    vet.body_text = Some("Bake the cake in a hot oven.".into());
    store.upsert_message(&vet).unwrap();
    index_with(&store, &Concepts);
    let found = ids(&hybrid(&store, "dog jab"));
    assert!(!found.contains(&"vet-visit".to_string()), "{found:?}");
    assert!(ids(&hybrid(&store, "dinner idea")).contains(&"vet-visit".to_string()));
}

#[test]
fn a_search_that_fell_behind_the_log_loads_the_matrix_again() {
    let (store, _reset) = concept_store();
    assert!(!ids(&hybrid(&store, "dog jab")).contains(&"kennel".to_string()));
    store
        .upsert_message(&mail(
            "kennel",
            "desk@happy-kennels.test",
            "Your stay",
            "The kennel has room for the puppy.",
            1_800_000_000,
        ))
        .unwrap();
    index_with(&store, &Concepts);
    // As if the indexer had trimmed entries this store had not read yet.
    store
        .conn
        .execute("DELETE FROM embedding_changes", [])
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO embedding_changes (account_id, message_id) VALUES ('acct', 'none@x')",
            [],
        )
        .unwrap();
    assert!(ids(&hybrid(&store, "dog jab")).contains(&"kennel".to_string()));
}

#[test]
fn the_indexer_keeps_the_change_log_short() {
    let (store, _reset) = concept_store();
    store
        .conn
        .execute_batch(&format!(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {})
             INSERT INTO embedding_changes (account_id, message_id)
             SELECT 'acct', 'filler' || i FROM n;",
            crate::embed::CHANGES_KEPT * 2
        ))
        .unwrap();
    let mut first = corpus_mail().remove(0);
    first.body_text = Some("The rent is due.".into());
    store.upsert_message(&first).unwrap();
    index_with(&store, &Concepts);
    let kept: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM embedding_changes", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(kept <= crate::embed::CHANGES_KEPT + 1, "{kept}");
}

#[test]
fn fusion_ranks_by_both_lists_and_keeps_keyword_ties_first() {
    let hit = |thread: &str, semantic_only: bool| SearchHit {
        message_id: MessageId(format!("{thread}-m")),
        thread_id: ThreadId(thread.into()),
        subject: String::new(),
        snippet: String::new(),
        sender_email: String::new(),
        date_unix: 0,
        semantic_only,
    };
    let fused = fuse(
        vec![hit("a", false), hit("b", false), hit("c", false)],
        vec![hit("c", true), hit("d", true)],
    );
    let order: Vec<(&str, bool)> = fused
        .iter()
        .map(|hit| (hit.thread_id.as_str(), hit.semantic_only))
        .collect();
    assert_eq!(
        order,
        [("c", false), ("a", false), ("b", false), ("d", true)]
    );
}

fn temp_db(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fell-hybrid-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("mail.db")
}

/// The indexer backs off while it has nothing to do (up to 30 s), so a
/// message queued after a quiet spell can wait several seconds.
fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ready() {
        assert!(start.elapsed() < Duration::from_secs(40), "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn vectors(store: &Store) -> i64 {
    store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM message_embeddings WHERE model = 'test-concepts'",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn search_follows_the_background_indexer_and_never_waits_for_a_writer() {
    let _reset = EmbedderReset;
    testing_embedder(Some(concepts));
    let path = temp_db("indexer");
    let store = Store::open(&path).unwrap();
    let messages = corpus_mail();
    fill(&store, &messages);
    wait_for("the indexer did not embed the corpus", || {
        vectors(&store) == messages.len() as i64
    });
    // The warm-up reads with its own read-only connection.
    let reader = super::semantic::open_reader(&path).unwrap();
    let count: i64 = reader
        .query_row("SELECT COUNT(*) FROM message_embeddings", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, messages.len() as i64);
    drop(reader);
    store.warm_search();

    // Another connection holds the write lock, as a long sync batch would.
    let writer = rusqlite::Connection::open(&path).unwrap();
    writer
        .execute_batch(
            "BEGIN IMMEDIATE;
             INSERT INTO embedding_changes (account_id, message_id) VALUES ('acct', 'held@x');",
        )
        .unwrap();
    let started = Instant::now();
    let found = ids(&hybrid(&store, "money owed plumber"));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "search waited {:?} for the writer",
        started.elapsed()
    );
    assert_eq!(found.first().map(String::as_str), Some("plumber-invoice"));
    writer.execute_batch("ROLLBACK").unwrap();
    drop(writer);

    // New mail: the indexer embeds it on its thread, and search picks the
    // vector up from the change log.
    store
        .upsert_message(&mail(
            "kennel",
            "desk@happy-kennels.test",
            "Your stay",
            "The kennel has room for the puppy.",
            1_800_000_000,
        ))
        .unwrap();
    wait_for("the indexer did not embed the new message", || {
        vectors(&store) == messages.len() as i64 + 1
    });
    assert!(ids(&hybrid(&store, "dog jab")).contains(&"kennel".to_string()));
    drop(store);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn the_matrix_holds_one_byte_per_dimension_and_a_scale_per_row() {
    let _reset = EmbedderReset;
    testing_embedder(Some(concepts));
    let store = Store::open_in_memory().unwrap();
    let messages = corpus_mail();
    fill(&store, &messages);
    index_with(&store, &Concepts);
    let (rows, matrix, _) = store.testing_vector_sizes().unwrap();
    // A body with no topic word is the zero vector, which is no row.
    let nonzero = CORPUS
        .iter()
        .filter(|(_, _, _, body)| Concepts.embed(body).unwrap().iter().any(|v| *v != 0.0))
        .count();
    assert_eq!(rows, nonzero);
    assert!(nonzero < messages.len());
    // Room is reserved for every stored vector, zero ones included.
    let row_bytes = TOPICS.len() + std::mem::size_of::<f32>();
    assert!(matrix >= rows * row_bytes, "{matrix}");
    assert!(matrix <= messages.len() * row_bytes, "{matrix}");
}

const BENCH_WORDS: &[&str] = &[
    "invoice",
    "payment",
    "overdue",
    "refund",
    "receipt",
    "balance",
    "plumber",
    "boiler",
    "leak",
    "builder",
    "quote",
    "flight",
    "airport",
    "boarding",
    "hotel",
    "booking",
    "room",
    "dentist",
    "checkup",
    "birthday",
    "party",
    "cake",
    "password",
    "login",
    "account",
    "meeting",
    "agenda",
    "minutes",
    "budget",
    "forecast",
    "quarter",
    "puppy",
    "vet",
    "lease",
    "landlord",
    "rent",
    "parcel",
    "courier",
    "delivery",
    "recipe",
    "dinner",
    "school",
    "teacher",
    "homework",
    "garage",
    "mechanic",
    "brakes",
    "garden",
    "hedge",
    "project",
    "deadline",
    "review",
    "draft",
    "contract",
    "signature",
    "insurance",
    "policy",
    "renewal",
    "ticket",
    "concert",
    "museum",
    "weather",
    "weekend",
    "holiday",
    "train",
    "station",
    "delay",
    "doctor",
    "pharmacy",
    "prescription",
    "bank",
    "transfer",
    "salary",
    "pension",
    "tax",
    "return",
    "council",
    "library",
    "book",
    "newsletter",
    "subscription",
    "order",
    "shipping",
    "warranty",
    "laptop",
    "phone",
    "repair",
    "update",
    "release",
    "server",
    "outage",
    "report",
    "survey",
    "volunteer",
    "charity",
    "donation",
    "wedding",
    "invitation",
    "photos",
    "camping",
    "hiking",
    "running",
    "gym",
    "class",
    "yoga",
    "football",
    "match",
    "score",
    "election",
    "vote",
    "neighbour",
    "noise",
];

fn bench_body(rng: &mut crate::test_support::mailbox::Rng) -> String {
    let mut words = Vec::with_capacity(14);
    for _ in 0..14 {
        words.push(BENCH_WORDS[rng.below(BENCH_WORDS.len())]);
    }
    words.join(" ")
}

fn rss_bytes() -> Option<usize> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: usize = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

/// Hybrid search over 100k messages with the real all-MiniLM-L6-v2, query
/// embedding included. The messages draw their bodies from 400 bases of
/// random words; each stored vector is its base's MiniLM vector plus a
/// little noise, so the scan, the cutoff, and the hit lookup all do real
/// work. Needs the pinned model files (see `fell embeddings install`):
///
/// `FELL_BENCH_MODEL_DIR=<models root> cargo test --release -p fell-core --lib
/// measure_hybrid_search -- --ignored --nocapture`
#[test]
#[ignore = "needs the installed MiniLM files and builds a 100k-message mailbox"]
fn measure_hybrid_search_at_100k_messages() {
    use crate::embed::{
        MINILM_L6_V2_INT8, OnnxEmbedder, installed_model_dir, testing_model_source,
    };
    use crate::test_support::mailbox::{Rng, noisy_vector};
    struct SourceReset;
    impl Drop for SourceReset {
        fn drop(&mut self) {
            testing_model_source(None);
        }
    }
    let root = std::path::PathBuf::from(
        std::env::var_os("FELL_BENCH_MODEL_DIR").expect("set FELL_BENCH_MODEL_DIR"),
    );
    let dir = installed_model_dir(&MINILM_L6_V2_INT8, &root).expect("MiniLM is not installed");
    let model = OnnxEmbedder::load(&MINILM_L6_V2_INT8, &dir).unwrap();
    let _source = SourceReset;
    testing_model_source(Some((root, &MINILM_L6_V2_INT8)));

    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let bases: Vec<String> = (0..400).map(|_| bench_body(&mut rng)).collect();
    let started = Instant::now();
    let base_vectors: Vec<Vec<f32>> = bases
        .iter()
        .map(|body| model.embed(body).unwrap())
        .collect();
    println!("embedded 400 base bodies in {:.1?}", started.elapsed());

    let store = Store::open_in_memory().unwrap();
    store.upsert_account(&account()).unwrap();
    let folders = ["INBOX", "INBOX", "Archive", "Work", "INBOX"];
    let senders = [
        "ana@north.test",
        "ben@south.test",
        "cy@east.test",
        "di@west.test",
    ];
    let built = Instant::now();
    let mut picks = Vec::with_capacity(100_000);
    for start in (0..100_000).step_by(1_000) {
        let batch: Vec<Message> = (start..start + 1_000)
            .map(|index| {
                let base = rng.below(bases.len());
                picks.push(base);
                let body = format!("{} ref{index}", bases[base]);
                let mut message = mail(
                    &format!("m{index}"),
                    senders[index % senders.len()],
                    &format!("note {index}"),
                    &body,
                    1_600_000_000 + index as i64 * 60,
                );
                message.folders = vec![folders[index % folders.len()].into()];
                message.unread = index % 7 == 0;
                message
            })
            .collect();
        store
            .apply_delta(
                &account_id(),
                &crate::MailboxDelta {
                    messages: batch,
                    ..crate::MailboxDelta::default()
                },
            )
            .unwrap();
    }
    {
        let tx = store.conn.unchecked_transaction().unwrap();
        let mut insert = tx
            .prepare(
                "INSERT INTO message_embeddings
                    (account_id, message_id, model, model_version, dims, body_hash, vector)
                 VALUES ('acct', ?1, ?2, ?3, 384, '', ?4)",
            )
            .unwrap();
        for (index, base) in picks.iter().enumerate() {
            let blob = noisy_vector(&mut rng, &base_vectors[*base]);
            insert
                .execute(rusqlite::params![
                    format!("m{index}@corpus.test"),
                    MINILM_L6_V2_INT8.id.model,
                    MINILM_L6_V2_INT8.id.version,
                    blob
                ])
                .unwrap();
        }
        drop(insert);
        tx.execute("DELETE FROM embedding_queue", []).unwrap();
        tx.commit().unwrap();
    }
    println!("built 100k messages and vectors in {:.1?}", built.elapsed());

    let rss_before = rss_bytes();
    let loaded = Instant::now();
    assert!(store.semantic_search_active());
    println!(
        "loaded the model and the matrix in {:.1?}",
        loaded.elapsed()
    );
    let (rows, matrix, keys) = store.testing_vector_sizes().unwrap();
    assert_eq!(rows, 100_000);
    println!(
        "matrix: {rows} rows, {:.1} MB int8 codes and scales, {:.1} MB ids and lookup",
        matrix as f64 / 1e6,
        keys as f64 / 1e6
    );
    if let (Some(before), Some(after)) = (rss_before, rss_bytes()) {
        println!(
            "resident memory grew {:.1} MB over the load (model included)",
            after.saturating_sub(before) as f64 / 1e6
        );
    }
    assert!(matrix <= 40_000_000, "int8 matrix is {matrix} bytes");

    let queries = [
        "money owed to the plumber",
        "when does my flight leave",
        "somewhere to stay for the trip",
        "my tooth hurts",
        "invoice",
        "invoice payment overdue",
        "rent landlord",
        "a celebration for my sister",
        "from:ana@north.test delivery",
        "in:Archive budget meeting",
        "is:unread school",
        "after:2021-01-01 pension tax",
        "laptop broken under warranty",
        "train cancelled",
        "server down again",
        "football results",
        "thank you for your donation",
        "concert tickets",
        "yoga class schedule",
        "noisy neighbours complaint",
    ];
    let mut runs = Vec::new();
    let mut keyword_runs = Vec::new();
    let mut hits = 0;
    let mut related = 0;
    for round in 0..5 {
        for query in queries {
            let started = Instant::now();
            let results = store.search(&account_id(), query).unwrap();
            runs.push(started.elapsed());
            let started = Instant::now();
            keyword(&store, query);
            keyword_runs.push(started.elapsed());
            if round == 0 {
                hits += results.hits.len();
                related += results.hits.iter().filter(|hit| hit.semantic_only).count();
            }
        }
    }
    let pct = |runs: &mut Vec<Duration>, p: usize| {
        runs.sort();
        runs[(runs.len() * p / 100).min(runs.len() - 1)]
    };
    let p50 = pct(&mut runs, 50);
    let p95 = pct(&mut runs, 95);
    println!(
        "hybrid, {} searches: p50 {p50:.1?}, p95 {p95:.1?}, max {:.1?}; {hits} hits in the first round, {related} related only",
        runs.len(),
        runs[runs.len() - 1]
    );
    println!(
        "keyword only: p50 {:.1?}, p95 {:.1?}",
        pct(&mut keyword_runs, 50),
        pct(&mut keyword_runs, 95)
    );
    let started = Instant::now();
    for _ in 0..20 {
        model.embed("money owed to the plumber").unwrap();
    }
    println!("query embedding alone: {:.1?} each", started.elapsed() / 20);
    assert!(p95 <= Duration::from_millis(150), "p95 {p95:?}");
}

/// What a sync and the indexer write together when a bill is archived and
/// its body rewritten: the folder, the text, and the vector of the new text.
fn archive_and_rewrite(path: &std::path::Path, message: &str) {
    let writer = rusqlite::Connection::open(path).unwrap();
    writer.busy_timeout(Duration::from_secs(10)).unwrap();
    let body = "Trim the garden hedge and the lawn.";
    let vector: Vec<u8> = Concepts
        .embed(body)
        .unwrap()
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    writer
        .execute_batch("BEGIN IMMEDIATE")
        .and_then(|()| {
            writer.execute(
                "UPDATE messages SET folders = 'Archive', snippet = ?2, body_text = ?2
                 WHERE message_id = ?1",
                rusqlite::params![message, body],
            )
        })
        .and_then(|_| {
            writer.execute(
                "UPDATE message_embeddings SET vector = ?2 WHERE message_id = ?1",
                rusqlite::params![message, vector],
            )
        })
        .and_then(|_| writer.execute_batch("COMMIT"))
        .unwrap();
}

#[test]
fn a_search_reads_one_snapshot_while_sync_and_the_indexer_commit() {
    let _reset = EmbedderReset;
    testing_embedder(Some(concepts));
    let bill = "plumber-invoice@corpus.test";
    let query = "money owed in:inbox";
    for step in [Step::Refreshed, Step::Filtered, Step::Scanned] {
        let path = temp_db("snapshot");
        let store = Store::open(&path).unwrap();
        let messages = corpus_mail();
        fill(&store, &messages);
        wait_for("the indexer did not embed the corpus", || {
            vectors(&store) == messages.len() as i64
        });
        let before = hybrid(&store, query);
        let hit = before
            .hits
            .iter()
            .find(|hit| hit.message_id.as_str() == bill)
            .expect("the bill is in the inbox and close to the query");
        assert!(hit.snippet.contains("kitchen leak"), "{hit:?}");

        // Another connection commits in the middle of the search. The
        // search still reads the state it started in: the bill in Inbox,
        // with the text its matched vector came from.
        let writer_path = path.clone();
        let mut wrote = false;
        testing_on_step(Some(Box::new(move |at| {
            if at == step && !wrote {
                wrote = true;
                archive_and_rewrite(&writer_path, bill);
            }
        })));
        let during = hybrid(&store, query);
        testing_on_step(None);
        assert_eq!(during, before, "a commit at {step:?} split the search");

        // The next search sees the commit: the bill is in Archive now.
        let after = hybrid(&store, query);
        assert!(
            after.hits.iter().all(|hit| hit.message_id.as_str() != bill),
            "{step:?}: {after:?}"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}

#[test]
fn semantic_rows_apply_the_operators_again() {
    let reset = EmbedderReset;
    testing_embedder(Some(concepts));
    let store = Store::open_in_memory().unwrap();
    let mut messages = corpus_mail();
    for message in &mut messages {
        if message.headers.message_id.as_str() == "plumber-invoice@corpus.test" {
            message.folders = vec!["Archive".into()];
        }
    }
    fill(&store, &messages);
    let rows = |query: &str| {
        let parsed = parse_query(query).unwrap();
        let mut found: Vec<String> = semantic_rows(
            &store.conn,
            &account_id(),
            &parsed,
            &["plumber-invoice@corpus.test", "refund@corpus.test"],
        )
        .unwrap()
        .into_keys()
        .collect();
        found.sort();
        found
    };
    assert_eq!(
        rows("money"),
        ["plumber-invoice@corpus.test", "refund@corpus.test"]
    );
    // Both an operator column and a stored filter.
    assert_eq!(rows("money in:inbox"), ["refund@corpus.test"]);
    assert_eq!(
        rows("money from:brightline"),
        ["plumber-invoice@corpus.test"]
    );
    assert!(rows("money from:brightline in:inbox").is_empty());
    drop(reset);
}

#[test]
fn a_corrected_query_is_the_one_the_semantic_side_embeds() {
    let (store, _reset) = concept_store();
    let typo = hybrid(&store, "invioce leak");
    assert_eq!(typo.did_you_mean.as_deref(), Some("invoice leak"));
    let corrected = hybrid(&store, "invoice leak");
    assert_eq!(corrected.did_you_mean, None);
    assert_eq!(typo.hits, corrected.hits);
    let first = &typo.hits[0];
    assert_eq!(first.message_id.as_str(), "plumber-invoice@corpus.test");
    assert!(!first.semantic_only);
    // The correction's operators filter the semantic side too.
    let filtered = hybrid(&store, "invioce leak from:brightline");
    assert_eq!(
        filtered.did_you_mean.as_deref(),
        Some("invoice leak from:brightline")
    );
    assert!(
        filtered
            .hits
            .iter()
            .all(|hit| hit.sender_email.ends_with("@brightline-plumbing.test")),
        "{filtered:?}"
    );
}

/// An embedder that says it has no dimensions.
struct NoDims;

impl BodyEmbedder for NoDims {
    fn id(&self) -> EmbedderId {
        EmbedderId {
            model: "test-no-dims",
            version: "1",
        }
    }

    fn dims(&self) -> usize {
        0
    }

    fn embed(&self, _body: &str) -> std::result::Result<Vec<f32>, ModelError> {
        Ok(Vec::new())
    }
}

#[test]
fn an_embedder_with_no_dimensions_is_refused() {
    let (store, _reset) = concept_store();
    assert!(store.testing_search_embedder(Box::new(NoDims)).is_err());
    let index = super::semantic::VectorIndex::load(&store.conn, NoDims.id(), 0);
    assert!(index.is_err());

    // Installed as the model, it never loads: search stays keyword search.
    testing_embedder(Some(|| Box::new(NoDims)));
    let store = Store::open_in_memory().unwrap();
    fill(&store, &corpus_mail());
    assert!(!store.semantic_search_active());
    assert_eq!(hybrid(&store, "invoice"), keyword(&store, "invoice"));
}
