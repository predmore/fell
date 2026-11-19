//! Received HTML for the conversation view.
//!
//! A message's stored HTML goes through [`fell_render::prepare`], which keeps
//! only text, structure, links and image references (no script, style or
//! remote fetch survives into the document), then [`fell_render::fold_history`]
//! marks quoted history and the signature. The view paints the result with
//! [`crate::render::blocks_column`]. Remote images stay placeholders here: the
//! native painter never fetches them.
//!
//! None of this runs on the UI thread. The store thread reads the parts and
//! prepares the bodies the reader could see above the message they land on,
//! so nothing above it moves once the thread is on screen; the body they land
//! on, and any message opened later, is prepared on the background executor
//! and fades in under a header that is already drawn. The store thread
//! prepares at most [`STORE_PREPARE_BYTES`] of HTML per read, so one huge
//! message cannot hold up the folder; the rest go to the background too.
//! Prepared bodies are cached by message id and the stored parts' stamp,
//! so reopening a thread prepares nothing and a resync that replaces a
//! body is prepared again.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fell_core::{AccountId, MessageId, Store};
use fell_render::{FoldedDocument, MailHeaders, fold_history, prepare};

use crate::present::{ConvMessage, Rich};
use crate::render::{ImageGallery, Palette};
use crate::theme::Theme;

/// One message's HTML, prepared for the native painter.
pub struct HtmlBody {
    pub folded: FoldedDocument,
    /// Lines of text in each section, for a folded section's label.
    pub fold_lines: Vec<usize>,
    /// Each section is written right to left: its rule and fold line are
    /// mirrored.
    pub rtl: Vec<bool>,
    /// Decoded `data:` images, keyed by the document's own bytes.
    pub gallery: ImageGallery,
}

/// One message's stored HTML, read on the store thread, to prepare elsewhere.
pub struct BodyJob {
    pub id: String,
    /// [`fell_core::PartKinds::stamp`] of the parts read.
    pub stamp: u64,
    html: String,
    headers: Vec<(String, String)>,
}

/// A prepared body and the stamp of the parts it was prepared from.
pub struct PreparedBody {
    pub id: String,
    pub stamp: u64,
    pub body: Arc<HtmlBody>,
}

impl BodyJob {
    pub fn prepare(self) -> PreparedBody {
        PreparedBody {
            body: Arc::new(prepare_html(&self.html, &self.headers)),
            id: self.id,
            stamp: self.stamp,
        }
    }
}

/// `headers`: the message's `Subject:` and `From:`, then the bulk headers
/// sync kept. Bulk mail (List-Unsubscribe, Precedence, ESP headers) is not
/// folded: a newsletter's pull quote is content, not history.
pub fn prepare_html(html: &str, headers_in: &[(String, String)]) -> HtmlBody {
    let mut headers = MailHeaders::new();
    for (name, value) in headers_in {
        headers.add(name.as_str(), value.as_str());
    }
    let prepared = prepare(html, &headers);
    let folded = if headers.is_bulk() {
        FoldedDocument::unfolded(prepared.document)
    } else {
        fold_history(
            prepared.document,
            headers.get("Subject").unwrap_or_default(),
        )
    };
    // Built after folding: the gallery keys images by their bytes' address.
    let gallery = ImageGallery::from_document(&folded.document);
    let texts: Vec<String> = folded
        .sections
        .iter()
        .map(|section| {
            fell_render::StyledDocument {
                blocks: folded.blocks(section).to_vec(),
            }
            .plain_text()
        })
        .collect();
    let fold_lines = folded
        .sections
        .iter()
        .zip(&texts)
        .map(|(section, text)| match section.fold {
            None => 0,
            Some(_) => text.lines().filter(|line| !line.trim().is_empty()).count(),
        })
        .collect();
    let rtl = texts
        .iter()
        .map(|text| crate::render::is_right_to_left(text))
        .collect();
    HtmlBody {
        folded,
        fold_lines,
        rtl,
        gallery,
    }
}

/// How many prepared bodies stay cached. Enough for a long thread and the
/// few threads around it; the oldest goes first.
const CACHED_BODIES: usize = 96;

/// Most compressed HTML the store thread prepares in one conversation read
/// (about 200 KB of HTML, a few milliseconds). Bodies past it are prepared
/// on the background executor instead.
pub const STORE_PREPARE_BYTES: u64 = 32 * 1024;

/// Cached bodies by message id, with the stamp of the parts each came from.
pub type CachedBodies = HashMap<String, (u64, Arc<HtmlBody>)>;

#[derive(Default)]
pub struct BodyCache {
    entries: CachedBodies,
    order: VecDeque<String>,
}

impl BodyCache {
    /// The cached bodies, for a store-thread read to use instead of
    /// preparing again. Clones handles, not documents.
    pub fn snapshot(&self) -> CachedBodies {
        self.entries.clone()
    }

    /// Cache a prepared body. Returns what fell out, whose images the
    /// caller removes from the sprite atlas.
    pub fn insert(&mut self, prepared: &PreparedBody) -> Vec<Arc<HtmlBody>> {
        let id = prepared.id.clone();
        let body = Arc::clone(&prepared.body);
        let mut evicted = Vec::new();
        if let Some((_, old)) = self
            .entries
            .insert(id.clone(), (prepared.stamp, Arc::clone(&body)))
        {
            if !Arc::ptr_eq(&old, &body) {
                evicted.push(old);
            }
            self.order.retain(|known| known != &id);
        }
        self.order.push_back(id);
        while self.order.len() > CACHED_BODIES {
            if let Some(oldest) = self.order.pop_front()
                && let Some((_, old)) = self.entries.remove(&oldest)
            {
                evicted.push(old);
            }
        }
        evicted
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// What the store thread found for a conversation's bodies.
#[derive(Default)]
pub struct BodyReads {
    /// Bodies prepared on the store thread, to cache.
    pub prepared: Vec<PreparedBody>,
    /// Bodies to prepare on the background executor.
    pub jobs: Vec<BodyJob>,
}

/// Fill in each message's [`Rich`] state on the store thread.
///
/// A cached body prepared from the stored parts is used as it is. An
/// expanded message's HTML is read, and prepared here when it sits above
/// the last message and fits what is left of [`STORE_PREPARE_BYTES`], so the
/// thread arrives with the bodies above the reader's message in place. The
/// last message's HTML, and any past the budget, come back as jobs.
/// Collapsed messages stay [`Rich::Pending`] until they open; `busy` ids are
/// being prepared already. An expanded message whose cached body is stale
/// (the parts changed since) keeps painting it until the new one is ready.
/// A store error leaves a message on its stored text.
pub fn read_bodies(
    store: &Store,
    account: &AccountId,
    messages: &mut [ConvMessage],
    cached: &CachedBodies,
    busy: &HashSet<String>,
) -> BodyReads {
    let mut reads = BodyReads::default();
    let ids: Vec<&str> = messages.iter().map(|message| message.id.as_str()).collect();
    let Ok(kinds) = store.stored_part_kinds(account, &ids) else {
        return reads;
    };
    let last = messages.len().saturating_sub(1);
    let mut budget = STORE_PREPARE_BYTES;
    for (index, message) in messages.iter_mut().enumerate() {
        let found = kinds.get(&message.id).copied();
        let mut stale = false;
        message.rich = match found {
            Some(kinds) if kinds.html => match cached.get(&message.id) {
                Some((stamp, body)) if *stamp == kinds.stamp => Rich::Html(Arc::clone(body)),
                Some((_, body)) if message.expanded => {
                    stale = true;
                    Rich::Html(Arc::clone(body))
                }
                _ => Rich::Pending,
            },
            Some(kinds) if kinds.plain => Rich::Plain,
            _ => Rich::Missing,
        };
        if !message.expanded || busy.contains(&message.id) {
            continue;
        }
        if !(stale || message.rich == Rich::Pending) {
            continue;
        }
        let Some(kinds) = found else {
            continue;
        };
        match read_job_with(
            store,
            account,
            &message.id,
            kinds.stamp,
            &message.mail_headers,
        ) {
            Some(job) if index < last && kinds.html_bytes <= budget => {
                budget -= kinds.html_bytes;
                let prepared = job.prepare();
                message.rich = Rich::Html(Arc::clone(&prepared.body));
                reads.prepared.push(prepared);
            }
            Some(job) => reads.jobs.push(job),
            None if stale => {}
            None => message.rich = Rich::Plain,
        }
    }
    reads
}

/// One message's stored HTML. `None` when it has none or the read failed.
/// `mail_headers`: its [`ConvMessage::mail_headers`].
pub fn read_job(
    store: &Store,
    account: &AccountId,
    id: &str,
    mail_headers: &[(String, String)],
) -> Option<BodyJob> {
    let stamp = store.stored_part_kinds(account, &[id]).ok()?.get(id)?.stamp;
    read_job_with(store, account, id, stamp, mail_headers)
}

fn read_job_with(
    store: &Store,
    account: &AccountId,
    id: &str,
    stamp: u64,
    mail_headers: &[(String, String)],
) -> Option<BodyJob> {
    let parts = store
        .message_body(account, &MessageId(id.to_string()))
        .ok()??;
    let mut headers = mail_headers.to_vec();
    headers.extend(parts.bulk_headers);
    Some(BodyJob {
        id: id.to_string(),
        stamp,
        html: parts.html?,
        headers,
    })
}

/// How long after a failed ask (sync not started, no connection) a body
/// is asked for again.
pub const ASK_AGAIN_AFTER: Duration = Duration::from_secs(30);

/// Which messages' parts were asked of sync. Only a real answer (parts, or
/// that the server has none) is final; a failed ask is retried, so a
/// startup or transport error does not leave a message on plain text for
/// the rest of the session.
#[derive(Default)]
pub struct BodyAsks {
    asks: HashMap<String, Ask>,
}

enum Ask {
    Waiting,
    Answered,
    Failed(Instant),
}

impl BodyAsks {
    pub fn should_ask(&self, id: &str, now: Instant) -> bool {
        match self.asks.get(id) {
            None => true,
            Some(Ask::Failed(at)) => now.duration_since(*at) >= ASK_AGAIN_AFTER,
            Some(Ask::Waiting | Ask::Answered) => false,
        }
    }

    pub fn asking(&mut self, id: &str) {
        self.asks.insert(id.to_string(), Ask::Waiting);
    }

    pub fn answered(&mut self, id: &str) {
        self.asks.insert(id.to_string(), Ask::Answered);
    }

    pub fn failed(&mut self, id: &str, at: Instant) {
        self.asks.insert(id.to_string(), Ask::Failed(at));
    }

    /// Ask the failed ones again now.
    pub fn retry_failed(&mut self) {
        self.asks.retain(|_, ask| !matches!(ask, Ask::Failed(_)));
    }
}

/// The painter's colours from the inbox theme. Quote rules and code
/// backgrounds stay as quiet as the rest of the conversation.
pub fn palette(theme: &Theme) -> Palette {
    Palette {
        background: theme.background,
        foreground: theme.foreground,
        dimmed: theme.dimmed,
        accent: theme.accent,
        // Quote rules are marks a reader follows down the page: 3:1.
        quote: crate::theme::strong_border(theme),
        code_bg: mix(theme.surface, theme.border, 0.5),
        border: theme.border,
        surface: theme.surface,
    }
}

/// The palette for opened history: its text is the dimmed colour, as the
/// plain-text quote is.
pub fn history_palette(theme: &Theme) -> Palette {
    Palette {
        foreground: theme.dimmed,
        ..palette(theme)
    }
}

fn mix(a: u32, b: u32, t: f32) -> u32 {
    let channel = |shift: u32| {
        let from = ((a >> shift) & 0xff) as f32;
        let to = ((b >> shift) & 0xff) as f32;
        ((from + (to - from) * t).round() as u32).min(255) << shift
    };
    channel(16) | channel(8) | channel(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fell_core::{
        Account, Address, BackendKind, FolderRoles, Message, MessageHeaders, MessageParts,
        RemoteId, ThreadId,
    };
    use fell_render::{Fold, fixtures};

    fn account() -> Account {
        Account {
            id: AccountId("local".into()),
            display_name: "Fell".into(),
            email: "me@fell.test".into(),
            backend_kind: BackendKind::Bridge,
        }
    }

    fn mail(id: &str, thread: &str, ago: i64, parts: Option<MessageParts>) -> Message {
        let message_id = MessageId::new(id);
        let root = MessageId::new(thread);
        Message {
            account_id: account().id,
            headers: MessageHeaders {
                message_id: message_id.clone(),
                proton_conversation_id: None,
                in_reply_to: (id != thread).then(|| root.clone()),
                references: if id != thread {
                    vec![root.clone()]
                } else {
                    Vec::new()
                },
                from: Address {
                    name: Some("Ada".into()),
                    email: "ada@fell.test".into(),
                },
                subject: "Plans".into(),
                date_unix: 1_700_000_000 - ago,
            },
            remote_id: RemoteId(format!("fixture:{id}")),
            thread_id: ThreadId::from_message_id(&root),
            snippet: "snippet".into(),
            body_text: Some("stored text".into()),
            unread: false,
            folders: vec!["INBOX".into()],
            labels: Vec::new(),
            arrived_via: Vec::new(),
            starred: false,
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            has_attachment: false,
            archived: false,
            snooze_until: None,
            list_id: None,
            reply_to: Vec::new(),
            attachment_names: Vec::new(),
            parts,
        }
    }

    fn html_parts(html: &str, headers: &[(&str, &str)]) -> MessageParts {
        MessageParts {
            html: Some(html.to_string()),
            plain: None,
            bulk_headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                .collect(),
            mime_parts: Vec::new(),
        }
    }

    fn open(store: &Store, thread: &str) -> Vec<ConvMessage> {
        let messages = store
            .thread_messages(
                &account().id,
                &ThreadId::from_message_id(&MessageId::new(thread)),
            )
            .unwrap();
        crate::present::conversation(
            &messages,
            "me@fell.test",
            &FolderRoles::default(),
            &[],
            "INBOX",
        )
    }

    fn store_with(messages: &[Message]) -> Store {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        for message in messages {
            store.upsert_message(message).unwrap();
        }
        store
    }

    #[test]
    fn the_render_corpus_is_identical_through_the_inbox_path() {
        for fixture in fixtures().into_iter().chain(fell_render::corpus()) {
            let mut parts = html_parts(&fixture.html, &[]);
            // Sync keeps only the headers that mark bulk mail; the message's
            // own From and Subject come from its row.
            parts.bulk_headers = fixture
                .headers
                .fields()
                .filter(|(name, value)| {
                    let mut one = MailHeaders::new();
                    one.add(*name, *value);
                    one.is_bulk()
                })
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect();
            let id = format!("{}@fell.test", fixture.name);
            let mut message = mail(&id, &id, 0, Some(parts));
            if let Some(subject) = fixture.headers.get("Subject") {
                message.headers.subject = subject.to_string();
            }
            if let Some(from) = fixture.headers.get("From") {
                let (name, email) = match from.rsplit_once('<') {
                    Some((name, email)) => {
                        (name.trim().trim_matches('"'), email.trim_end_matches('>'))
                    }
                    None => ("", from),
                };
                message.headers.from = Address {
                    name: (!name.is_empty()).then(|| name.to_string()),
                    email: email.to_string(),
                };
            }
            let store = store_with(&[message]);
            let mut view = open(&store, &id);
            let reads = read_bodies(
                &store,
                &account().id,
                &mut view,
                &HashMap::new(),
                &HashSet::new(),
            );
            assert_eq!(view[0].rich, Rich::Pending, "{}", fixture.name);
            assert_eq!(reads.jobs.len(), 1, "{}", fixture.name);
            let body = reads.jobs.into_iter().next().unwrap().prepare().body;
            // The same calls on the fixture as it sits on disk.
            let direct = prepare(&fixture.html, &fixture.headers);
            let expected = if fixture.headers.is_bulk() {
                FoldedDocument::unfolded(direct.document)
            } else {
                fold_history(
                    direct.document,
                    fixture.headers.get("Subject").unwrap_or_default(),
                )
            };
            assert_eq!(body.folded, expected, "{}", fixture.name);
        }
    }

    #[test]
    fn bodies_above_the_last_arrive_prepared_and_the_last_is_a_job() {
        let reply =
            "<p>Yes.</p><div class=\"gmail_quote\"><blockquote><p>Lunch?</p></blockquote></div>";
        let mut first = mail(
            "a@fell.test",
            "a@fell.test",
            300,
            Some(html_parts(reply, &[])),
        );
        first.unread = true;
        let second = mail(
            "b@fell.test",
            "a@fell.test",
            200,
            Some(html_parts("<p>old</p>", &[])),
        );
        let mut plain = mail("c@fell.test", "a@fell.test", 150, None);
        plain.parts = Some(MessageParts {
            html: None,
            plain: Some("plain".into()),
            bulk_headers: Vec::new(),
            mime_parts: Vec::new(),
        });
        let bare = mail("d@fell.test", "a@fell.test", 120, None);
        let last = mail(
            "e@fell.test",
            "a@fell.test",
            100,
            Some(html_parts("<p>new</p>", &[])),
        );
        let store = store_with(&[first, second, plain, bare, last]);
        let mut view = open(&store, "a@fell.test");
        let reads = read_bodies(
            &store,
            &account().id,
            &mut view,
            &HashMap::new(),
            &HashSet::new(),
        );
        let states: Vec<_> = view
            .iter()
            .map(|message| format!("{:?}", message.rich))
            .collect();
        // Unread and expanded: prepared on the store thread. Collapsed: not
        // read. Plain-only: stored text. No parts: stored text, and collapsed
        // so not asked for. Last: a job.
        assert_eq!(states, ["Html", "Pending", "Plain", "Missing", "Pending"]);
        assert_eq!(reads.prepared.len(), 1);
        assert_eq!(reads.jobs.len(), 1);
        assert_eq!(reads.jobs[0].id, "e@fell.test");
        let Rich::Html(body) = &view[0].rich else {
            unreachable!()
        };
        let folds: Vec<_> = body
            .folded
            .sections
            .iter()
            .map(|section| section.fold)
            .collect();
        assert_eq!(folds, [None, Some(Fold::Quote)]);
    }

    #[test]
    fn a_cached_body_is_used_and_a_busy_one_is_not_read_again() {
        let only = mail(
            "a@fell.test",
            "a@fell.test",
            0,
            Some(html_parts("<p>x</p>", &[])),
        );
        let store = store_with(&[only]);
        let prepared = read_job(&store, &account().id, "a@fell.test", &[])
            .unwrap()
            .prepare();
        let body = Arc::clone(&prepared.body);
        let mut cache = BodyCache::default();
        cache.insert(&prepared);

        let mut view = open(&store, "a@fell.test");
        let reads = read_bodies(
            &store,
            &account().id,
            &mut view,
            &cache.snapshot(),
            &HashSet::new(),
        );
        assert_eq!(view[0].rich, Rich::Html(body));
        assert!(reads.jobs.is_empty() && reads.prepared.is_empty());

        let mut view = open(&store, "a@fell.test");
        let busy: HashSet<String> = std::iter::once("a@fell.test".to_string()).collect();
        let reads = read_bodies(&store, &account().id, &mut view, &HashMap::new(), &busy);
        assert_eq!(view[0].rich, Rich::Pending);
        assert!(reads.jobs.is_empty());
    }

    #[test]
    fn a_message_with_no_parts_paints_its_stored_text() {
        let only = mail("a@fell.test", "a@fell.test", 0, None);
        let store = store_with(&[only]);
        let mut view = open(&store, "a@fell.test");
        let reads = read_bodies(
            &store,
            &account().id,
            &mut view,
            &HashMap::new(),
            &HashSet::new(),
        );
        assert_eq!(view[0].rich, Rich::Missing);
        assert!(reads.jobs.is_empty() && reads.prepared.is_empty());
    }

    #[test]
    fn bulk_mail_is_not_folded() {
        let html = "<p>This week.</p><blockquote><p>A pull quote.</p></blockquote>";
        let personal = prepare_html(html, &[]);
        assert!(personal.folded.has_folds());
        let bulk = prepare_html(
            html,
            &[("List-Unsubscribe".into(), "<https://news.test/u>".into())],
        );
        assert!(!bulk.folded.has_folds());
    }

    #[test]
    fn script_and_remote_content_never_reach_the_document() {
        let body = prepare_html(
            "<p onclick=\"steal()\">Hi <a href=\"javascript:steal()\">there</a></p><script>steal()</script><style>p{background:url(https://track.test/p)}</style><img src=\"https://track.test/pixel.gif\" alt=\"Chart\">",
            &[],
        );
        let text = body.folded.document.plain_text();
        assert!(!text.contains("steal"), "{text}");
        assert!(!text.contains("background"), "{text}");
        let debug = format!("{:?}", body.folded.document);
        assert!(!debug.contains("javascript"), "{debug}");
        // The remote image is a reference the painter shows as blocked; it
        // is never fetched here.
        assert!(debug.contains("Remote(\"https://track.test/pixel.gif\")"));
    }

    #[test]
    fn the_cache_drops_the_oldest_body_first() {
        let mut cache = BodyCache::default();
        let body = Arc::new(prepare_html("<p>x</p>", &[]));
        let entry = |id: String, body: &Arc<HtmlBody>| PreparedBody {
            id,
            stamp: 1,
            body: Arc::clone(body),
        };
        let mut evicted = 0;
        for index in 0..CACHED_BODIES + 3 {
            evicted += cache.insert(&entry(format!("m{index}"), &body)).len();
        }
        assert_eq!(evicted, 3);
        assert_eq!(cache.len(), CACHED_BODIES);
        assert!(!cache.snapshot().contains_key("m0"));
        assert!(
            cache
                .snapshot()
                .contains_key(&format!("m{}", CACHED_BODIES + 2))
        );
        // Replacing an entry hands back the old one, unless it is the same.
        let other = Arc::new(prepare_html("<p>y</p>", &[]));
        let last = format!("m{}", CACHED_BODIES + 2);
        assert_eq!(cache.insert(&entry(last.clone(), &body)).len(), 0);
        assert_eq!(cache.insert(&entry(last, &other)).len(), 1);
    }

    #[test]
    fn a_body_whose_parts_changed_is_prepared_again_and_shows_until_then() {
        let first = mail(
            "a@fell.test",
            "a@fell.test",
            0,
            Some(html_parts("<p>first</p>", &[])),
        );
        let store = store_with(std::slice::from_ref(&first));
        let mut cache = BodyCache::default();
        let old = read_job(&store, &account().id, "a@fell.test", &[])
            .unwrap()
            .prepare();
        cache.insert(&old);
        // A resync replaces the parts.
        let mut replaced = first.clone();
        replaced.parts = Some(html_parts("<p>the second, longer version</p>", &[]));
        store.upsert_message(&replaced).unwrap();
        let mut view = open(&store, "a@fell.test");
        let reads = read_bodies(
            &store,
            &account().id,
            &mut view,
            &cache.snapshot(),
            &HashSet::new(),
        );
        // The old body stays on screen while the new one is prepared.
        assert_eq!(view[0].rich, Rich::Html(Arc::clone(&old.body)));
        assert_eq!(reads.jobs.len(), 1);
        let new = reads.jobs.into_iter().next().unwrap().prepare();
        assert_ne!(new.stamp, old.stamp);
        assert!(new.body.folded.document.plain_text().contains("second"));
        // Once cached, it is used as it is.
        cache.insert(&new);
        let mut view = open(&store, "a@fell.test");
        let reads = read_bodies(
            &store,
            &account().id,
            &mut view,
            &cache.snapshot(),
            &HashSet::new(),
        );
        assert_eq!(view[0].rich, Rich::Html(new.body));
        assert!(reads.jobs.is_empty());
    }

    #[test]
    fn the_store_thread_prepares_only_so_much_and_the_rest_waits() {
        // Bodies that compress poorly, so a few pass the budget.
        let noisy = |seed: u64| {
            let mut state = seed;
            let mut html = String::from("<p>");
            while html.len() < (STORE_PREPARE_BYTES as usize) * 6 / 5 {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                html.push(char::from(b'a' + (state >> 59) as u8 % 26));
                if state.is_multiple_of(7) {
                    html.push(' ');
                }
            }
            html.push_str("</p>");
            html
        };
        let mut messages = Vec::new();
        for index in 0..3u64 {
            let id = format!("m{index}@fell.test");
            let mut message = mail(
                &id,
                "m0@fell.test",
                300 - index as i64,
                Some(html_parts(&noisy(index + 1), &[])),
            );
            message.unread = true;
            messages.push(message);
        }
        messages.push(mail(
            "last@fell.test",
            "m0@fell.test",
            10,
            Some(html_parts("<p>last</p>", &[])),
        ));
        let store = store_with(&messages);
        let mut view = open(&store, "m0@fell.test");
        let reads = read_bodies(
            &store,
            &account().id,
            &mut view,
            &HashMap::new(),
            &HashSet::new(),
        );
        let states: Vec<_> = view
            .iter()
            .map(|message| format!("{:?}", message.rich))
            .collect();
        // The first fits the budget; the next two do not and go to the
        // background with the last, holding their space meanwhile.
        assert_eq!(states, ["Html", "Pending", "Pending", "Pending"]);
        assert_eq!(reads.prepared.len(), 1);
        assert_eq!(reads.jobs.len(), 3);
    }

    #[test]
    fn a_failed_ask_is_asked_again_and_an_answer_is_final() {
        let now = Instant::now();
        let mut asks = BodyAsks::default();
        assert!(asks.should_ask("a", now));
        asks.asking("a");
        assert!(!asks.should_ask("a", now), "one ask at a time");
        asks.failed("a", now);
        assert!(!asks.should_ask("a", now + Duration::from_secs(1)));
        assert!(asks.should_ask("a", now + ASK_AGAIN_AFTER));
        asks.failed("a", now);
        asks.retry_failed();
        assert!(asks.should_ask("a", now), "sync came back");
        asks.answered("a");
        assert!(!asks.should_ask("a", now + ASK_AGAIN_AFTER * 10));
        asks.retry_failed();
        assert!(!asks.should_ask("a", now));
    }

    #[test]
    fn folds_count_their_lines_for_the_label() {
        let body = prepare_html(
            "<p>Yes.</p><div class=\"gmail_quote\"><div>On Mon, Ada wrote:</div><blockquote><p>One</p><p>Two</p></blockquote></div>",
            &[],
        );
        assert_eq!(body.fold_lines, vec![0, 3]);
    }

    fn heavy_newsletter() -> String {
        let mut html = String::from(
            "<!doctype html><html><head><style>@media (max-width:600px){.c{width:100%}} td{font-family:Arial}</style></head><body><table role=\"presentation\" width=\"100%\"><tr><td><table role=\"presentation\" width=\"600\">",
        );
        for row in 0..400 {
            html.push_str(&format!(
                "<tr><td style=\"padding:12px;background:#fff\"><h2 style=\"font-size:20px\">Story {row}</h2><p style=\"margin:0 0 8px\">The river slowed this week and the town <a href=\"https://news.test/s/{row}\">noticed</a>. <strong>Shops</strong> opened late; <em>nobody</em> minded.</p><img src=\"https://news.test/i/{row}.jpg\" alt=\"Photo {row}\" width=\"560\"></td></tr>"
            ));
        }
        html.push_str("</table></td></tr></table></body></html>");
        html
    }

    /// `cargo test --release -p fell --lib measure_conversation_bodies -- --ignored --nocapture`
    #[test]
    #[ignore = "measurement; run in release with --nocapture"]
    fn measure_conversation_bodies() {
        use std::time::Instant;
        let reply = "<div dir=\"ltr\"><p>Thursday works. I'll bring the <b>notes</b>.</p><ul><li>One</li><li>Two</li></ul></div><div class=\"gmail_quote\"><div class=\"gmail_attr\">On Mon, Ada wrote:</div><blockquote class=\"gmail_quote\"><p>Does Thursday work?</p><p>Earlier text with a <a href=\"https://fell.test\">link</a>.</p></blockquote></div>";
        let mut messages = Vec::new();
        for index in 0..50 {
            let id = format!("m{index}@fell.test");
            let mut message = mail(
                &id,
                "m0@fell.test",
                1_000 - index as i64,
                Some(html_parts(reply, &[])),
            );
            message.unread = index % 5 == 0;
            messages.push(message);
        }
        let store = store_with(&messages);
        let mut cache = BodyCache::default();
        // Cold: every expanded body is read and prepared.
        let mut view = open(&store, "m0@fell.test");
        let started = Instant::now();
        let reads = read_bodies(
            &store,
            &account().id,
            &mut view,
            &cache.snapshot(),
            &HashSet::new(),
        );
        let cold = started.elapsed();
        for prepared in &reads.prepared {
            cache.insert(prepared);
        }
        for job in reads.jobs {
            cache.insert(&job.prepare());
        }
        // Warm the rest, as opening each message would.
        for message in &view {
            if let Some(job) = read_job(&store, &account().id, &message.id, &message.mail_headers) {
                cache.insert(&job.prepare());
            }
        }
        let mut samples = Vec::new();
        for _ in 0..25 {
            let started = Instant::now();
            let mut view = open(&store, "m0@fell.test");
            let reads = read_bodies(
                &store,
                &account().id,
                &mut view,
                &cache.snapshot(),
                &HashSet::new(),
            );
            samples.push(started.elapsed());
            assert!(reads.jobs.is_empty() && reads.prepared.is_empty());
            assert!(
                view.iter()
                    .all(|message| matches!(message.rich, Rich::Html(_)))
            );
        }
        samples.sort();
        let html = heavy_newsletter();
        let mut prepare_samples = Vec::new();
        for _ in 0..15 {
            let started = Instant::now();
            let body = prepare_html(
                &html,
                &[("List-Unsubscribe".into(), "<https://news.test/u>".into())],
            );
            prepare_samples.push(started.elapsed());
            assert!(body.folded.document.blocks.len() > 400);
        }
        prepare_samples.sort();
        println!(
            "50-message thread, bodies cached: read + bodies median {:?}, max {:?}; cold first open {:?}",
            samples[samples.len() / 2],
            samples[samples.len() - 1],
            cold
        );
        println!(
            "heavy newsletter ({} KB HTML): prepare median {:?}, max {:?}",
            html.len() / 1024,
            prepare_samples[prepare_samples.len() / 2],
            prepare_samples[prepare_samples.len() - 1]
        );
    }
}
