//! Run attachment operations off the window thread and show their results.

use std::process::Child;
use std::time::Duration;

use fell_core::compose::{MAX_ATTACHMENT_BYTES, MAX_ATTACHMENT_FILES, ResolvedFile};
use fell_core::parts::{CachedPart, PartEvent, PartRequest};
use fell_core::{AccountId, MessageId, MimePart, PartRole};
use futures::StreamExt;
use gpui::{AsyncApp, Context, WeakEntity};

use super::Inbox;
use crate::attachments::{self, Download, Fetcher, Opened, Request, Target};
use crate::keymap::{self, Action};
use crate::model::Toast;
use crate::present::display_safe;
use crate::store_actor::StoreClient;

impl Inbox {
    pub(super) fn attachment_source(&self) -> AttachmentSource {
        AttachmentSource {
            store: self.store.clone(),
            fetcher: self.attachment_fetcher.clone(),
            account: self.account_id.clone(),
        }
    }

    pub(super) fn reveal_selected_attachment(&mut self) {
        self.reader_took_over(true);
        self.motion.conv_glide = None;
        self.reveal_attachment = true;
    }

    pub(super) fn start_attachment_action(
        &mut self,
        request: Request,
        confirmed: bool,
        cx: &mut Context<Self>,
    ) {
        if self.attachment_busy || self.forwarding_attachments {
            self.session
                .show_toast("An attachment action is already in progress", false);
            return;
        }
        self.attachment_busy = true;
        self.attachment_retry = None;
        self.session.attachment_error = None;
        let notice = match &request {
            Request::Open(target) => format!("Opening “{}”…", target.name()),
            Request::Save(targets) => match targets.as_slice() {
                [target] => format!("Saving “{}”…", target.name()),
                _ => format!("Saving {} attachments…", targets.len()),
            },
        };
        self.session.show_toast(notice, false);
        let source = self.attachment_source();
        cx.spawn(async move |this, cx| {
            let outcome = match request {
                Request::Open(target) => open(&source, &this, cx, target, confirmed).await,
                Request::Save(targets) => save(&source, &this, cx, targets).await,
            };
            let _ = this.update(cx, |inbox, cx| {
                inbox.attachment_busy = false;
                match outcome {
                    Outcome::Launch(target, child) => {
                        inbox.attachment_succeeded(format!("Opened “{}”", target.name()), cx);
                        inbox.watch_attachment_opener(target, child, cx);
                    }
                    Outcome::Confirm(target) => inbox.session.ask_attachment_confirmation(target),
                    Outcome::Saved(line) => inbox.attachment_succeeded(line, cx),
                    Outcome::Failed {
                        line,
                        detail,
                        retry,
                    } => inbox.attachment_failed(line, detail, retry),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn attachment_succeeded(&mut self, line: String, cx: &mut Context<Self>) {
        self.session.show_toast(line, false);
        self.dismiss_notice_after(self.session.toast_generation(), Duration::from_secs(3), cx);
    }

    fn attachment_failed(&mut self, line: String, detail: String, retry: Request) {
        let targets = match &retry {
            Request::Open(target) => std::slice::from_ref(target),
            Request::Save(targets) => targets.as_slice(),
        };
        self.failed_attachments.extend(targets.iter().map(part_key));
        self.attachment_retry = Some(retry);
        self.session.attachment_error = Some(format!("{line}\n{}", display_safe(&detail)));
        let key = keymap::binding_text(&self.session.keymap, Action::RetryAttachment);
        let mut toast = Toast::with_hint(line, key, " to retry");
        toast.alert = true;
        self.session.set_toast(toast);
    }

    // Some xdg-open implementations wait for the viewer to exit. Reap the
    // child without holding the download lane or blocking the window.
    fn watch_attachment_opener(&self, target: Target, mut child: Child, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { child.wait() })
                .await;
            let detail = match result {
                Ok(status) if status.success() => return,
                Ok(status) => format!("xdg-open exited with {status}"),
                Err(err) => format!("xdg-open: {err}"),
            };
            let _ = this.update(cx, |inbox, cx| {
                inbox.attachment_failed(
                    "Couldn't open attachment".into(),
                    detail,
                    Request::Open(target),
                );
                cx.notify();
            });
        })
        .detach();
    }
}

fn part_key(target: &Target) -> (String, String) {
    (target.message.clone(), target.part.section.clone())
}

enum Outcome {
    Launch(Target, Child),
    Confirm(Target),
    Saved(String),
    Failed {
        line: String,
        detail: String,
        retry: Request,
    },
}

async fn open(
    source: &AttachmentSource,
    this: &WeakEntity<Inbox>,
    cx: &mut AsyncApp,
    target: Target,
    confirmed: bool,
) -> Outcome {
    let opened = match source.fetch(this, cx, &target).await {
        Ok(cached) => {
            let part = target.part.clone();
            cx.background_executor()
                .spawn(async move {
                    attachments::open_cached(cached, &part, confirmed)
                        .map_err(|err| err.to_string())
                })
                .await
        }
        Err(err) => Err(err),
    };
    match opened {
        Ok(Opened::Confirm) => Outcome::Confirm(target),
        Ok(Opened::Launched(child)) => Outcome::Launch(target, child),
        Err(detail) => Outcome::Failed {
            line: "Couldn't open attachment".into(),
            detail,
            retry: Request::Open(target),
        },
    }
}

async fn save(
    source: &AttachmentSource,
    this: &WeakEntity<Inbox>,
    cx: &mut AsyncApp,
    targets: Vec<Target>,
) -> Outcome {
    let dir = match cx
        .background_executor()
        .spawn(async { attachments::downloads_dir() })
        .await
    {
        Ok(dir) => dir,
        Err(err) => {
            return Outcome::Failed {
                line: "Couldn't save to Downloads".into(),
                detail: err.to_string(),
                retry: Request::Save(targets),
            };
        }
    };
    let mut failed = Vec::new();
    let mut first_error = None;
    let mut saved = 0;
    let mut last_name = String::new();
    for target in targets {
        let result = match source.fetch(this, cx, &target).await {
            Ok(cached) => {
                let part = target.part.clone();
                let dir = dir.clone();
                cx.background_executor()
                    .spawn(async move {
                        attachments::save_to(cached, &part, &dir).map_err(|err| err.to_string())
                    })
                    .await
            }
            Err(err) => Err(err),
        };
        match result {
            Ok(path) => {
                saved += 1;
                last_name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
            }
            Err(err) => {
                first_error.get_or_insert_with(|| format!("{}: {err}", target.name()));
                failed.push(target);
            }
        }
    }
    if let Some(detail) = first_error {
        let line = if saved == 0 {
            "Couldn't save attachments".into()
        } else {
            format!("Saved {saved}, {} failed", failed.len())
        };
        return Outcome::Failed {
            line,
            detail,
            retry: Request::Save(failed),
        };
    }
    Outcome::Saved(if saved == 1 {
        format!("Saved “{last_name}” to Downloads")
    } else {
        format!("Saved {saved} attachments to Downloads")
    })
}

pub(super) struct AttachmentSource {
    store: StoreClient,
    fetcher: Fetcher,
    account: AccountId,
}

impl AttachmentSource {
    async fn fetch(
        &self,
        this: &WeakEntity<Inbox>,
        cx: &mut AsyncApp,
        target: &Target,
    ) -> Result<CachedPart, String> {
        let account = self.account.clone();
        let id = MessageId(target.message.clone());
        let expected = target.part.clone();
        let request = self
            .store
            .call(move |store| {
                let request = store.part_request(&account, &id, &expected.section)?;
                if request.is_some() {
                    return Ok(request.filter(|request| {
                        let mut actual = request.part.clone();
                        if expected.size == expected.estimated_decoded_size() {
                            actual.size = expected.size;
                        }
                        actual == expected
                    }));
                }
                // A local copy without server coordinates can still use bytes
                // previously cached for this canonical message.
                Ok::<_, fell_core::Error>(
                    store
                        .message_attachments(&account, &id)?
                        .filter(|parts| parts.contains(&expected))
                        .map(|_| PartRequest {
                            account_id: account,
                            message_id: id,
                            part: expected,
                            copies: Vec::new(),
                            decoded_size_known: true,
                        }),
                )
            })
            .await
            .map_err(|_| "The mailbox reader stopped".to_string())?
            .map_err(|err| err.to_string())?
            .filter(|request| request.part.role == PartRole::Attachment)
            .ok_or_else(|| "This attachment changed or is no longer in the mailbox".to_string())?;
        let fetcher = self.fetcher.clone();
        let (mut events, _cancel) = cx
            .background_executor()
            .spawn(async move { fetcher.queue(request) })
            .await?;
        let mut download: Option<Download> = None;
        while let Some(event) = events.next().await {
            match event {
                PartEvent::Downloading { total } => {
                    download = Some(Download { fetched: 0, total });
                }
                PartEvent::Progress { fetched, total } => {
                    if let Some(progress) = &mut download {
                        let old = progress.label();
                        progress.fetched = fetched;
                        progress.total = total;
                        if progress.label() == old {
                            continue;
                        }
                    }
                }
                PartEvent::Done(result) => {
                    Self::show_download(this, cx, target, None)?;
                    if result.is_ok() {
                        let _ = this.update(cx, |inbox, cx| {
                            inbox.failed_attachments.remove(&part_key(target));
                            cx.notify();
                        });
                    }
                    return result.map_err(|err| err.to_string());
                }
            }
            Self::show_download(this, cx, target, download)?;
        }
        if download.is_some() {
            Self::show_download(this, cx, target, None)?;
        }
        Err("The attachment download stopped".into())
    }

    fn show_download(
        this: &WeakEntity<Inbox>,
        cx: &mut AsyncApp,
        target: &Target,
        download: Option<Download>,
    ) -> Result<(), String> {
        this.update(cx, |inbox, cx| {
            inbox.attachment_download = download.map(|progress| {
                (
                    target.message.clone(),
                    target.part.section.clone(),
                    progress,
                )
            });
            cx.notify();
        })
        .map_err(|_| "The conversation closed".to_string())
    }

    pub(super) async fn forward_files(
        &self,
        this: &WeakEntity<Inbox>,
        cx: &mut AsyncApp,
        message: &str,
        parts: Vec<MimePart>,
    ) -> (Vec<ResolvedFile>, Vec<String>) {
        let mut files = Vec::new();
        let mut omitted = Vec::new();
        let mut remaining = MAX_ATTACHMENT_BYTES;
        for (index, part) in parts.into_iter().enumerate() {
            let target = Target {
                message: message.to_string(),
                part,
            };
            let name = target.name();
            if files.len() < MAX_ATTACHMENT_FILES
                && target.part.size <= remaining
                && let Ok(cached) = self.fetch(this, cx, &target).await
                && let Ok(bytes) = cx
                    .background_executor()
                    .spawn(async move { attachments::read_cached(cached) })
                    .await
            {
                remaining -= bytes.len() as u64;
                files.push(ResolvedFile {
                    // Keep separate paths even for duplicate display names.
                    path: format!("attachments/{index:04}/{name}"),
                    display_name: name,
                    bytes,
                    inline: false,
                });
            } else {
                omitted.push(name);
            }
        }
        (files, omitted)
    }
}
