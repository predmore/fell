//! Verification state shared by the inbox and compose. Completed checks
//! stay visible for the messages the person tried to answer this session.

use std::collections::{HashMap, HashSet};

use fell_core::{AccountId, MessageId, ReplyToProgress};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplyCheck {
    Waiting,
    Checking,
    Unavailable,
    Verified,
}

#[derive(Default)]
pub(crate) struct ReplyChecks {
    account: Option<AccountId>,
    offline: bool,
    active: HashSet<String>,
    watched: HashMap<String, ReplyCheck>,
}

impl gpui::Global for ReplyChecks {}

impl ReplyChecks {
    pub fn start(&mut self, account: AccountId, offline: bool) {
        *self = Self {
            account: Some(account),
            offline,
            ..Self::default()
        };
    }

    pub fn set_offline(&mut self, offline: bool) {
        self.offline = offline;
    }

    pub fn watch(&mut self, id: MessageId) -> bool {
        use std::collections::hash_map::Entry;

        if let Entry::Vacant(entry) = self.watched.entry(id.0) {
            entry.insert(ReplyCheck::Waiting);
            true
        } else {
            false
        }
    }

    pub fn pending(&self) -> Vec<MessageId> {
        self.watched
            .iter()
            .filter(|(_, check)| **check != ReplyCheck::Verified)
            .map(|(id, _)| MessageId(id.clone()))
            .collect()
    }

    pub fn apply(&mut self, progress: ReplyToProgress) {
        match progress {
            ReplyToProgress::Checking(ids) => {
                self.active = ids.into_iter().map(|id| id.0).collect()
            }
            ReplyToProgress::Verified(ids) => {
                for id in ids {
                    self.active.remove(&id.0);
                    if let Some(check) = self.watched.get_mut(&id.0) {
                        *check = ReplyCheck::Verified;
                    }
                }
            }
            ReplyToProgress::Finished => {
                for id in self.active.drain() {
                    if let Some(check) = self.watched.get_mut(&id) {
                        *check = ReplyCheck::Unavailable;
                    }
                }
            }
        }
    }

    pub fn for_message(&self, id: &str) -> Option<ReplyCheck> {
        self.watched
            .get(id)
            .map(|&check| self.status(id, check))
            .or_else(|| self.active.contains(id).then_some(ReplyCheck::Checking))
    }

    pub fn for_draft(&self, account: &AccountId, id: &str) -> ReplyCheck {
        if self.account.as_ref() != Some(account) {
            return ReplyCheck::Waiting;
        }
        self.status(
            id,
            self.watched.get(id).copied().unwrap_or(ReplyCheck::Waiting),
        )
    }

    fn status(&self, id: &str, check: ReplyCheck) -> ReplyCheck {
        match check {
            ReplyCheck::Verified => ReplyCheck::Verified,
            _ if self.active.contains(id) => ReplyCheck::Checking,
            _ if self.offline => ReplyCheck::Unavailable,
            _ => check,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_checks_report_partial_progress_and_keep_ready_cues_scoped() {
        let mut checks = ReplyChecks::default();
        let account = AccountId("local".into());
        let one = MessageId::new("a@x");
        let two = MessageId::new("b@x");
        let other = MessageId::new("other@x");
        let unrelated = MessageId::new("unrelated@x");
        checks.start(account.clone(), true);
        checks.watch(one.clone());
        checks.watch(two.clone());
        assert_eq!(
            checks.for_message(one.as_str()),
            Some(ReplyCheck::Unavailable)
        );
        checks.set_offline(false);
        checks.apply(ReplyToProgress::Checking(vec![
            one.clone(),
            two.clone(),
            other.clone(),
        ]));
        assert_eq!(
            checks.for_draft(&account, one.as_str()),
            ReplyCheck::Checking
        );
        assert_eq!(
            checks.for_message(other.as_str()),
            Some(ReplyCheck::Checking)
        );
        assert_eq!(checks.for_message(unrelated.as_str()), None);
        checks.apply(ReplyToProgress::Verified(vec![one.clone(), other.clone()]));
        assert_eq!(checks.for_message(one.as_str()), Some(ReplyCheck::Verified));
        assert_eq!(checks.for_message(other.as_str()), None);
        assert_eq!(checks.for_message(two.as_str()), Some(ReplyCheck::Checking));
        checks.apply(ReplyToProgress::Finished);
        assert_eq!(
            checks.for_message(two.as_str()),
            Some(ReplyCheck::Unavailable)
        );
        assert_eq!(checks.for_message(other.as_str()), None);
        checks.apply(ReplyToProgress::Checking(vec![two.clone()]));
        checks.apply(ReplyToProgress::Verified(vec![two.clone()]));
        checks.apply(ReplyToProgress::Finished);
        assert_eq!(checks.for_message(one.as_str()), Some(ReplyCheck::Verified));
        assert_eq!(checks.for_message(two.as_str()), Some(ReplyCheck::Verified));
        assert_eq!(
            checks.for_draft(&AccountId("other".into()), two.as_str()),
            ReplyCheck::Waiting
        );
    }
}
