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

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use fell_core::{AccountId, PartStamp, Store};
use fell_render::{FoldedDocument, MailHeaders, fold_history, prepare};
use gpui::RenderImage;

use crate::present::{ConvMessage, Rich};
use crate::render::{ImageGallery, Palette};
use crate::text_render::{RenderedBody, render_stored_body};
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
    pub stamp: PartStamp,
    html: String,
    plain: Option<String>,
    headers: Vec<(String, String)>,
}

/// A prepared body and the stamp of the parts it was prepared from.
pub struct PreparedBody {
    pub id: String,
    pub stamp: PartStamp,
    pub body: Arc<HtmlBody>,
    fallback: Option<Arc<RenderedBody>>,
}

impl PreparedBody {
    pub fn rich(&self) -> Rich {
        rich_body(&self.body, self.fallback.as_ref())
    }
}

impl BodyJob {
    pub fn prepare(self) -> PreparedBody {
        let (body, fallback) = prepare_parts(&self.html, self.plain.as_deref(), &self.headers);
        PreparedBody {
            body,
            fallback,
            id: self.id,
            stamp: self.stamp,
        }
    }
}

/// The first sent card uses the same MIME parts and history folding as a reread.
pub(crate) fn prepare_sent(message: &fell_core::Message) -> Rich {
    let Some(parts) = &message.parts else {
        return Rich::Plain;
    };
    let Some(html) = &parts.html else {
        return Rich::Plain;
    };
    let mut headers = crate::present::mail_headers(message);
    headers.extend(parts.bulk_headers.iter().cloned());
    let (body, fallback) = prepare_parts(html, parts.plain.as_deref(), &headers);
    rich_body(&body, fallback.as_ref())
}

fn prepare_parts(
    html: &str,
    plain: Option<&str>,
    headers: &[(String, String)],
) -> (Arc<HtmlBody>, Option<Arc<RenderedBody>>) {
    let body = prepare_html(html, headers);
    let subject = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("Subject"))
        .map_or("", |(_, subject)| subject.as_str());
    let fallback = plain
        .filter(|plain| body.folded.document.is_empty() && !plain.trim().is_empty())
        .map(|plain| Arc::new(render_stored_body(Some(plain), "", subject)));
    (Arc::new(body), fallback)
}

fn rich_body(body: &Arc<HtmlBody>, fallback: Option<&Arc<RenderedBody>>) -> Rich {
    if !body.folded.document.is_empty() {
        Rich::Html(Arc::clone(body))
    } else if let Some(plain) = fallback {
        Rich::PlainPart(Arc::clone(plain))
    } else {
        Rich::Plain
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

#[derive(Default)]
pub struct BodyCache {
    entries: HashMap<String, (PartStamp, Rich)>,
    order: VecDeque<String>,
    retired: HashMap<String, Vec<RetiredBody>>,
}

struct RetiredBody {
    stamp: PartStamp,
    body: Weak<HtmlBody>,
    images: Vec<Arc<RenderImage>>,
}

impl BodyCache {
    /// Which bodies were already prepared when a store read started.
    pub fn stamps(&self) -> HashMap<String, PartStamp> {
        let mut stamps = HashMap::new();
        for (id, revisions) in &self.retired {
            if let Some(live) = revisions
                .iter()
                .rev()
                .find(|rev| rev.body.strong_count() > 0)
            {
                stamps.insert(id.clone(), live.stamp);
            }
        }
        stamps.extend(
            self.entries
                .iter()
                .map(|(id, (stamp, _))| (id.clone(), *stamp)),
        );
        stamps
    }

    pub fn insert(&mut self, prepared: &PreparedBody) {
        let id = prepared.id.clone();
        let rich = prepared.rich();
        if let Some((stamp, old)) = self
            .entries
            .insert(id.clone(), (prepared.stamp, rich.clone()))
        {
            if old != rich
                && let Rich::Html(old) = old
            {
                self.retire(id.clone(), stamp, &old);
            }
            self.order.retain(|known| known != &id);
        }
        self.order.push_back(id);
        while self.order.len() > CACHED_BODIES {
            if let Some(oldest) = self.order.pop_front()
                && let Some((stamp, Rich::Html(old))) = self.entries.remove(&oldest)
            {
                self.retire(oldest, stamp, &old);
            }
        }
    }

    fn retire(&mut self, id: String, stamp: PartStamp, body: &Arc<HtmlBody>) {
        self.retired.entry(id).or_default().push(RetiredBody {
            stamp,
            body: Arc::downgrade(body),
            images: body.gallery.images().cloned().collect(),
        });
    }

    /// A reread can still reuse an evicted body that the conversation owns.
    fn live_evicted(&self, id: &str, stamp: PartStamp) -> Option<Arc<HtmlBody>> {
        self.retired
            .get(id)?
            .iter()
            .rev()
            .filter(|rev| rev.stamp == stamp)
            .find_map(|rev| rev.body.upgrade())
    }

    /// The open conversation can outlive cache eviction. Hold only the
    /// images needed for atlas removal until that body has been released.
    pub fn release_unused_images(&mut self, mut release: impl FnMut(Arc<RenderImage>)) {
        self.retired.retain(|_, revisions| {
            for retired in revisions.extract_if(.., |rev| rev.body.strong_count() == 0) {
                retired.images.into_iter().for_each(&mut release);
            }
            !revisions.is_empty()
        });
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// What the store thread found for a conversation's bodies.
#[derive(Default)]
pub struct BodyReads {
    /// Bodies prepared on the store thread and their positions in the read.
    prepared: Vec<(usize, PreparedBody)>,
    /// The stored revision of each HTML body in the read.
    stamps: HashMap<String, PartStamp>,
}

/// Find which messages have HTML on the store thread.
///
/// Prepare small expanded bodies above the last message here so they
/// arrive in place. The window resolves the rest against its live cache
/// and starts any remaining loads for the revisions returned here.
pub fn read_bodies(
    store: &Store,
    account: &AccountId,
    messages: &mut [ConvMessage],
    cached: &HashMap<String, PartStamp>,
) -> BodyReads {
    let mut reads = BodyReads::default();
    let ids: Vec<&str> = messages.iter().map(|message| message.id.as_str()).collect();
    let Ok(kinds) = store.stored_part_kinds(account, &ids) else {
        return reads;
    };
    let last = messages.len().saturating_sub(1);
    let mut budget = STORE_PREPARE_BYTES;
    for (index, message) in messages.iter_mut().enumerate() {
        match kinds.get(&message.id) {
            Some(kinds) if kinds.html => {
                message.rich = Rich::Pending;
                reads.stamps.insert(message.id.clone(), kinds.stamp);
                if message.expanded
                    && index < last
                    && kinds.html_bytes <= budget
                    && cached.get(&message.id) != Some(&kinds.stamp)
                {
                    budget -= kinds.html_bytes;
                    if let Ok(Some(job)) = read_job_at_stamp(
                        store,
                        account,
                        &message.id,
                        kinds.stamp,
                        &message.mail_headers,
                    ) {
                        reads.prepared.push((index, job.prepare()));
                    }
                }
            }
            // A stored row with no parts is a settled result as well.
            Some(_) => message.rich = Rich::Plain,
            None => message.rich = Rich::Missing,
        }
    }
    reads
}

/// Read the text parts that belong to the expected revision. `None` means
/// the row changed or went away since the conversation read.
pub fn read_job_at_stamp(
    store: &Store,
    account: &AccountId,
    id: &str,
    stamp: PartStamp,
    mail_headers: &[(String, String)],
) -> fell_core::Result<Option<BodyJob>> {
    let Some(parts) = store.message_body_at_stamp(account, id, stamp)? else {
        return Ok(None);
    };
    let mut headers = mail_headers.to_vec();
    headers.extend(parts.bulk_headers);
    Ok(parts.html.map(|html| BodyJob {
        id: id.to_string(),
        stamp,
        html,
        plain: parts.plain,
        headers,
    }))
}

#[cfg(test)]
pub fn read_job(
    store: &Store,
    account: &AccountId,
    id: &str,
    headers: &[(String, String)],
) -> Option<BodyJob> {
    let stamp = store.stored_part_kinds(account, &[id]).ok()?.get(id)?.stamp;
    read_job_at_stamp(store, account, id, stamp, headers).ok()?
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyLoad {
    pub stamp: PartStamp,
    generation: u64,
}

/// Owns the unresolved revisions from the latest conversation read and the
/// loads already running for them. A reread can change what is wanted while
/// a load occupies the slot. The completion frees the slot for the new load.
#[derive(Default)]
pub struct BodyLoads {
    wanted: HashMap<String, PartStamp>,
    active: HashMap<String, BodyLoad>,
    generation: u64,
}

impl BodyLoads {
    /// Install the accepted conversation's bodies and remember what still
    /// needs loading.
    pub fn accept(
        &mut self,
        BodyReads {
            mut stamps,
            prepared,
        }: BodyReads,
        cache: &mut BodyCache,
        messages: &mut [ConvMessage],
    ) {
        for message in messages.iter_mut() {
            let Some(wanted) = stamps.get(&message.id) else {
                continue;
            };
            let cached = cache.entries.get(&message.id);
            let ready = cached
                .filter(|(stamp, _)| stamp == wanted)
                .map(|(_, rich)| rich.clone())
                .or_else(|| cache.live_evicted(&message.id, *wanted).map(Rich::Html));
            if let Some(rich) = ready {
                message.rich = rich;
                stamps.remove(&message.id);
            } else if message.expanded
                && let Some((_, rich)) = cached
            {
                message.rich = rich.clone();
            }
        }
        // Resolve the live cache before inserting anything. Cache eviction
        // must not take a ready body away from this conversation.
        for (index, body) in prepared {
            if stamps.get(&body.id) == Some(&body.stamp) {
                stamps.remove(&body.id);
                messages[index].rich = body.rich();
                cache.insert(&body);
            }
        }
        self.wanted = stamps;
    }

    pub fn clear(&mut self) {
        self.wanted.clear();
    }

    pub fn begin(&mut self, id: &str) -> Option<BodyLoad> {
        let stamp = *self.wanted.get(id)?;
        if self.active.contains_key(id) {
            return None;
        }
        self.generation += 1;
        let load = BodyLoad {
            stamp,
            generation: self.generation,
        };
        self.active.insert(id.to_string(), load);
        Some(load)
    }

    pub fn finish(&mut self, id: &str, load: BodyLoad) -> bool {
        if self.active.get(id) != Some(&load) {
            return false;
        }
        self.active.remove(id);
        if self.is_wanted(id, load) {
            self.wanted.remove(id);
            true
        } else {
            false
        }
    }

    pub fn is_wanted(&self, id: &str, load: BodyLoad) -> bool {
        self.wanted.get(id) == Some(&load.stamp)
    }
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
        Account, Address, BackendKind, FolderRoles, Message, MessageHeaders, MessageId,
        MessageParts, RemoteId, ThreadId,
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
            reply_to: Some(Vec::new()),
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

    fn job(store: &Store, message: &ConvMessage, load: BodyLoad) -> Option<BodyJob> {
        read_job_at_stamp(
            store,
            &account().id,
            &message.id,
            load.stamp,
            &message.mail_headers,
        )
        .unwrap()
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
            let reads = read_bodies(&store, &account().id, &mut view, &HashMap::new());
            assert!(reads.prepared.is_empty());
            let mut loads = BodyLoads::default();
            loads.accept(reads, &mut BodyCache::default(), &mut view);
            assert_eq!(view[0].rich, Rich::Pending, "{}", fixture.name);
            let load = loads.begin(&view[0].id).unwrap();
            let body = job(&store, &view[0], load).unwrap().prepare();
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
            match body.rich() {
                Rich::Html(body) => assert_eq!(body.folded, expected, "{}", fixture.name),
                Rich::Plain => assert!(expected.document.is_empty(), "{}", fixture.name),
                other => panic!("unexpected body for {}: {other:?}", fixture.name),
            }
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
        let reads = read_bodies(&store, &account().id, &mut view, &HashMap::new());
        assert_eq!(reads.prepared.len(), 1);
        let mut loads = BodyLoads::default();
        loads.accept(reads, &mut BodyCache::default(), &mut view);
        let states: Vec<_> = view
            .iter()
            .map(|message| format!("{:?}", message.rich))
            .collect();
        // Unread and expanded: prepared on the store thread. Collapsed: not
        // read. Plain-only: stored text. No parts: stored text, and collapsed
        // so not asked for. Last: a job.
        assert_eq!(states, ["Html", "Pending", "Plain", "Missing", "Pending"]);
        assert!(loads.begin("a@fell.test").is_none());
        assert!(loads.begin("e@fell.test").is_some());
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
    fn a_reread_uses_a_body_cached_since_it_started() {
        let id = "a@fell.test";
        let store = store_with(&[mail(id, id, 0, Some(html_parts("<p>x</p>", &[])))]);
        let mut cache = BodyCache::default();
        let mut loads = BodyLoads::default();
        let mut view = open(&store, id);
        let reads = read_bodies(&store, &account().id, &mut view, &cache.stamps());
        loads.accept(reads, &mut cache, &mut view);
        let load = loads.begin(id).unwrap();

        let mut next_view = open(&store, id);
        let reread = read_bodies(&store, &account().id, &mut next_view, &cache.stamps());
        let prepared = job(&store, &view[0], load).unwrap().prepare();
        assert!(loads.finish(id, load));
        cache.insert(&prepared);

        loads.accept(reread, &mut cache, &mut next_view);
        assert_eq!(next_view[0].rich, prepared.rich());
        assert!(loads.begin(id).is_none());
    }

    #[test]
    fn only_the_absent_parts_row_needs_an_on_demand_fetch() {
        let id = "a@fell.test";
        let legacy = mail(id, id, 0, None);
        let store = store_with(std::slice::from_ref(&legacy));
        for state in [Rich::Missing, Rich::Plain] {
            let mut view = open(&store, id);
            let reads = read_bodies(&store, &account().id, &mut view, &HashMap::new());
            assert!(view[0].expanded);
            assert_eq!(view[0].rich, state);
            assert!(reads.stamps.is_empty() && reads.prepared.is_empty());
            assert_eq!(
                view[0].body.blocks,
                [crate::text_render::BodyBlock::Text("stored text".into())]
            );
            if state == Rich::Missing {
                // A completed backfill with no server body records an empty row.
                store
                    .store_fetched_parts(&account().id, &[(MessageId::new(id), None)])
                    .unwrap();
                let kinds = store.stored_part_kinds(&account().id, &[id]).unwrap();
                assert!(!kinds[id].html && !kinds[id].plain);
            }
        }
        let mut filled = legacy;
        filled.parts = Some(html_parts("<p>Appeared on another copy.</p>", &[]));
        store.upsert_message(&filled).unwrap();
        let mut view = open(&store, id);
        read_bodies(&store, &account().id, &mut view, &HashMap::new());
        assert_eq!(view[0].rich, Rich::Pending);
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
        for index in 0..CACHED_BODIES + 3 {
            cache.insert(&PreparedBody {
                id: format!("m{index}"),
                stamp: PartStamp::default(),
                body: Arc::new(prepare_html("<p>x</p>", &[])),
                fallback: None,
            });
        }
        assert_eq!(cache.len(), CACHED_BODIES);
        assert!(!cache.stamps().contains_key("m0"));
        assert!(
            cache
                .stamps()
                .contains_key(&format!("m{}", CACHED_BODIES + 2))
        );
    }

    #[test]
    fn evicted_and_replaced_images_wait_for_the_open_body_to_be_released() {
        let id = "image@fell.test";
        let fixture = fell_render::fixture("native-personal-reply").unwrap();
        let make = |id: String, html: &str| PreparedBody {
            id,
            stamp: PartStamp::default(),
            body: Arc::new(prepare_html(html, &[])),
            fallback: None,
        };
        for replace in [false, true] {
            let mut cache = BodyCache::default();
            let store = store_with(&[mail(id, id, 0, Some(html_parts(&fixture.html, &[])))]);
            let mut view = open(&store, id);
            let image = read_job(&store, &account().id, id, &view[0].mail_headers)
                .unwrap()
                .prepare();
            let expected: Vec<_> = image.body.gallery.images().cloned().collect();
            assert!(!expected.is_empty());
            view[0].rich = image.rich();
            cache.insert(&image);
            drop(image);

            if replace {
                cache.insert(&make(id.into(), "<p>Updated</p>"));
            } else {
                for index in 0..CACHED_BODIES {
                    cache.insert(&make(format!("m{index}"), "<p>Other</p>"));
                }
                assert_eq!(cache.len(), CACHED_BODIES);
                assert!(!cache.entries.contains_key(id));
                let mut reread = open(&store, id);
                let reads = read_bodies(&store, &account().id, &mut reread, &cache.stamps());
                let mut loads = BodyLoads::default();
                loads.accept(reads, &mut cache, &mut reread);
                assert_eq!(reread[0].rich, view[0].rich);
                assert!(loads.begin(id).is_none());
                view = reread;
            }
            cache.release_unused_images(|_| panic!("the open body still needs its images"));
            view.clear();
            let mut released = Vec::new();
            cache.release_unused_images(|image| released.push(image));
            assert_eq!(released.len(), expected.len());
            assert!(
                released
                    .iter()
                    .zip(&expected)
                    .all(|(a, b)| Arc::ptr_eq(a, b))
            );
            cache.release_unused_images(|_| panic!("the images have already been released"));
        }
    }

    #[test]
    fn a_replacement_waits_for_older_work_then_loads_once() {
        let id = "a@fell.test";
        let mut message = mail(id, id, 0, Some(html_parts("<p>first</p>", &[])));
        let store = store_with(std::slice::from_ref(&message));
        let mut cache = BodyCache::default();
        let mut loads = BodyLoads::default();
        let mut first_view = open(&store, id);
        let reads = read_bodies(&store, &account().id, &mut first_view, &cache.stamps());
        loads.accept(reads, &mut cache, &mut first_view);
        let first = loads.begin(id).unwrap();
        let first_job = job(&store, &first_view[0], first).unwrap();

        // A newer revision arrives while the initial prepare is running.
        message.parts = Some(html_parts("<p>other</p>", &[]));
        store.upsert_message(&message).unwrap();
        let mut view = open(&store, id);
        let reads = read_bodies(&store, &account().id, &mut view, &cache.stamps());
        let second_stamp = reads.stamps[id];
        assert_ne!(first.stamp, second_stamp);
        loads.accept(reads, &mut cache, &mut view);
        assert!(loads.begin(id).is_none());
        assert!(job(&store, &first_view[0], first).is_none());

        let stale = first_job.prepare();
        assert!(!loads.finish(&stale.id, first));
        let second = loads.begin(id).unwrap();
        assert_eq!(second.stamp, second_stamp);
        assert!(!loads.finish(id, first));
        assert!(loads.begin(id).is_none());
        let prepared = job(&store, &view[0], second).unwrap().prepare();
        assert!(loads.finish(id, second));
        assert!(loads.begin(id).is_none());
        let Rich::Html(html) = prepared.rich() else {
            panic!("expected the replacement HTML");
        };
        assert_eq!(html.folded.document.plain_text(), "other");

        // The previously prepared body can stay on screen during a refresh.
        cache.insert(&prepared);
        message.parts = Some(html_parts("<p>third</p>", &[]));
        store.upsert_message(&message).unwrap();
        let mut view = open(&store, id);
        let reads = read_bodies(&store, &account().id, &mut view, &cache.stamps());
        loads.accept(reads, &mut cache, &mut view);
        assert_eq!(view[0].rich, prepared.rich());
        assert!(loads.begin(id).is_some());
    }

    #[test]
    fn empty_prepared_html_uses_the_plain_part_then_the_stored_body_text() {
        use crate::text_render::BodyBlock;

        let id = "a@fell.test";
        const PLAIN: &str = "Plain reply\n\n> Old text";
        for plain in [Some(PLAIN), Some("   "), None] {
            let mut parts = html_parts("<script>hidden()</script>", &[]);
            parts.plain = plain.map(str::to_string);
            let store = store_with(&[mail(id, id, 0, Some(parts))]);
            let mut view = open(&store, id);
            let mut cache = BodyCache::default();
            let mut loads = BodyLoads::default();
            let reads = read_bodies(&store, &account().id, &mut view, &cache.stamps());
            loads.accept(reads, &mut cache, &mut view);
            let load = loads.begin(id).unwrap();
            let prepared = job(&store, &view[0], load).unwrap().prepare();
            assert!(loads.finish(id, load));
            cache.insert(&prepared);
            let reread = read_bodies(&store, &account().id, &mut view, &cache.stamps());
            loads.accept(reread, &mut cache, &mut view);
            assert!(loads.begin(id).is_none());
            match (&view[0].rich, plain) {
                (Rich::PlainPart(body), Some(PLAIN)) => {
                    assert!(matches!(
                        body.blocks.as_slice(),
                        [BodyBlock::Text(text), BodyBlock::Quote(quote)]
                            if text.as_str() == "Plain reply" && quote.contains("Old text")
                    ));
                }
                (Rich::Plain, Some("   ") | None) => assert!(matches!(
                    view[0].body.blocks.as_slice(),
                    [BodyBlock::Text(text)] if text.as_str() == "stored text"
                )),
                (other, _) => panic!("unexpected fallback: {other:?}"),
            }
        }
    }

    #[test]
    fn plain_forwards_stay_open_in_storage_and_as_the_empty_html_fallback() {
        use crate::text_render::BodyBlock;

        let id = "a@fell.test";
        let plain = "FYI\n\nFrom: Ada <ada@example.test>\nSent: Mon\nSubject: Plans\n\nThe plan.";
        let mut mail = mail(id, id, 0, None);
        mail.headers.subject = "WG: Plans".into();
        mail.body_text = Some(plain.into());
        let store = store_with(std::slice::from_ref(&mail));
        let view = open(&store, id);
        assert_eq!(view[0].body.blocks, [BodyBlock::Text(plain.into())]);

        let mut parts = html_parts("<p hidden>Empty alternative</p>", &[]);
        parts.plain = Some(plain.into());
        mail.parts = Some(parts);
        store.upsert_message(&mail).unwrap();
        let prepared = read_job(&store, &account().id, &view[0].id, &view[0].mail_headers)
            .unwrap()
            .prepare();
        let Rich::PlainPart(body) = prepared.rich() else {
            panic!("expected the plain alternative");
        };
        assert_eq!(body.blocks, [BodyBlock::Text(plain.into())]);
    }

    #[test]
    fn nonempty_native_html_keeps_its_output_even_without_text() {
        let id = "a@fell.test";
        let mut parts = html_parts(r#"<img src="https://news.test/chart.png">"#, &[]);
        parts.plain = Some("plain alternative".to_string());
        let store = store_with(&[mail(id, id, 0, Some(parts))]);
        let mut view = open(&store, id);
        let reads = read_bodies(&store, &account().id, &mut view, &HashMap::new());
        let job = read_job_at_stamp(&store, &account().id, id, reads.stamps[id], &[])
            .unwrap()
            .unwrap();
        let Rich::Html(body) = job.prepare().rich() else {
            panic!("the native image placeholder was lost");
        };
        assert!(!body.folded.document.is_empty());
        assert!(body.folded.document.plain_text().is_empty());
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
        let reads = read_bodies(&store, &account().id, &mut view, &HashMap::new());
        assert_eq!(reads.prepared.len(), 1);
        let mut loads = BodyLoads::default();
        loads.accept(reads, &mut BodyCache::default(), &mut view);
        let states: Vec<_> = view
            .iter()
            .map(|message| format!("{:?}", message.rich))
            .collect();
        // The first fits the budget; the next two do not and go to the
        // background with the last, holding their space meanwhile.
        assert_eq!(states, ["Html", "Pending", "Pending", "Pending"]);
        assert!(loads.begin("m0@fell.test").is_none());
        for id in ["m1@fell.test", "m2@fell.test", "last@fell.test"] {
            assert!(loads.begin(id).is_some());
        }
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

    #[test]
    fn a_sent_reply_has_the_same_card_body_before_and_after_refresh() {
        let parent = mail("parent@fell.test", "parent@fell.test", 20, None);
        let mut reply = mail(
            "reply@fell.test",
            "parent@fell.test",
            10,
            Some(html_parts(
                "<p>Thursday works.</p><p>On Mon, Ada wrote:</p>\
                 <blockquote><p>Does Thursday work?</p></blockquote>",
                &[],
            )),
        );
        reply.headers.subject = "Re: Plans".into();
        let Rich::Html(first) = prepare_sent(&reply) else {
            panic!("the first card needs the prepared body");
        };
        let store = store_with(&[parent, reply]);
        let view = open(&store, "parent@fell.test");
        let last = view.last().unwrap();
        let refreshed = read_job(&store, &account().id, &last.id, &last.mail_headers)
            .unwrap()
            .prepare();
        assert_eq!(first.fold_lines, [0, 2]);
        assert_eq!(first.fold_lines, refreshed.body.fold_lines);
        assert_eq!(first.folded.sections, refreshed.body.folded.sections);
        assert_eq!(first.folded.document, refreshed.body.folded.document);
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
        let mut loads = BodyLoads::default();
        let started = Instant::now();
        let reads = read_bodies(&store, &account().id, &mut view, &cache.stamps());
        loads.accept(reads, &mut cache, &mut view);
        for message in view.iter().filter(|message| message.expanded) {
            if let Some(load) = loads.begin(&message.id) {
                cache.insert(&job(&store, message, load).unwrap().prepare());
                assert!(loads.finish(&message.id, load));
            }
        }
        let cold = started.elapsed();
        // Warm the rest, as opening each message would.
        for message in &view {
            if let Some(load) = loads.begin(&message.id) {
                cache.insert(&job(&store, message, load).unwrap().prepare());
                assert!(loads.finish(&message.id, load));
            }
        }
        let mut samples = Vec::new();
        for _ in 0..25 {
            let started = Instant::now();
            let mut view = open(&store, "m0@fell.test");
            let reads = read_bodies(&store, &account().id, &mut view, &cache.stamps());
            assert!(reads.prepared.is_empty());
            loads.accept(reads, &mut cache, &mut view);
            samples.push(started.elapsed());
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
