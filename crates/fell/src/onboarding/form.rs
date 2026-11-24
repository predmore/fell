use std::net::IpAddr;

use fell_core::onboarding::{ConnectionProgress, ConnectionReport, connection_recovery};
use fell_core::{AccountConfig, BackendKind, ImapSecurity, PasswordSource, SyncFailureKind};

use crate::setup::SaveAccountError;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum Step {
    #[default]
    Provider,
    Identity,
    Servers,
    Connection,
    Done,
}

impl Step {
    pub fn number(self) -> usize {
        match self {
            Self::Provider => 1,
            Self::Identity | Self::Servers => 2,
            Self::Connection => 3,
            Self::Done => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum Provider {
    #[default]
    Bridge,
    Custom,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Self::Bridge => "Proton Mail Bridge",
            Self::Custom => "Generic IMAP",
        }
    }

    pub fn password_hint(self) -> &'static str {
        match self {
            Self::Bridge => "Copy the IMAP username and password from Mailbox details in Bridge.",
            Self::Custom => "Use the mail password or app password your provider gives you.",
        }
    }

    fn servers(self) -> [Endpoint; 2] {
        use ImapSecurity::{StartTls, Tls};
        let (imap, smtp) = match self {
            Self::Bridge => (("127.0.0.1", 1143, StartTls), ("127.0.0.1", 1025, StartTls)),
            Self::Custom => (("", 993, Tls), ("", 587, StartTls)),
        };
        [Endpoint::new(imap), Endpoint::new(smtp)]
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum Service {
    Imap = 0,
    Smtp = 1,
}

impl Service {
    pub const ALL: [Self; 2] = [Self::Imap, Self::Smtp];

    pub fn name(self) -> &'static str {
        match self {
            Self::Imap => "Incoming mail",
            Self::Smtp => "Outgoing mail",
        }
    }
}

struct Endpoint {
    host: String,
    port: String,
    security: ImapSecurity,
}

impl Endpoint {
    fn new((host, port, security): (&str, u16, ImapSecurity)) -> Self {
        Self {
            host: host.into(),
            port: port.to_string(),
            security,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum Field {
    Email,
    Name,
    Username,
    Password,
    Host(Service),
    Port(Service),
    Security(Service),
}

impl Field {
    pub fn step(self) -> Step {
        match self {
            Self::Host(_) | Self::Port(_) | Self::Security(_) => Step::Servers,
            _ => Step::Identity,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Issue {
    pub field: Field,
    pub title: &'static str,
    pub recovery: Option<&'static str>,
    pub suggest_starttls: bool,
}

impl Issue {
    fn input(field: Field, title: &'static str) -> Self {
        Self {
            field,
            title,
            recovery: None,
            suggest_starttls: false,
        }
    }

    fn connection(config: &AccountConfig, service: Service, error: &fell_core::Error) -> Self {
        let kind = fell_core::classify_sync_error(error);
        let bridge = config.backend == BackendKind::Bridge;
        let (field, title) = match kind {
            SyncFailureKind::LoginRejected | SyncFailureKind::Password => {
                (Field::Password, "Sign-in was rejected")
            }
            SyncFailureKind::Tls => (Field::Security(service), "Secure connection failed"),
            SyncFailureKind::Unreachable => (Field::Host(service), "Server didn’t answer"),
            SyncFailureKind::Other => (Field::Host(service), "Connection test failed"),
        };
        Self {
            field,
            title,
            suggest_starttls: kind == SyncFailureKind::Tls,
            recovery: match kind {
                SyncFailureKind::LoginRejected if bridge => {
                    Some("Use the password Bridge shows, not your Proton password.")
                }
                SyncFailureKind::Tls if bridge => Some(match service {
                    Service::Imap => "Bridge uses STARTTLS on port 1143.",
                    Service::Smtp => "Bridge uses STARTTLS on port 1025 for outgoing mail.",
                }),
                SyncFailureKind::Tls => Some(match service {
                    Service::Imap => "Try TLS on 993 or STARTTLS on 143.",
                    Service::Smtp => "Try TLS on 465 or STARTTLS on 587.",
                }),
                _ => connection_recovery(config, error),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum Check {
    #[default]
    Waiting,
    Testing,
    Connected,
    Failed,
}

impl Check {
    pub fn update(event: ConnectionProgress<'_>) -> (Service, Self) {
        match event {
            ConnectionProgress::ImapStarted => (Service::Imap, Self::Testing),
            ConnectionProgress::SmtpStarted => (Service::Smtp, Self::Testing),
            ConnectionProgress::ImapFinished(result) => (Service::Imap, Self::finished(result)),
            ConnectionProgress::SmtpFinished(result) => (Service::Smtp, Self::finished(result)),
        }
    }

    fn finished<T>(result: &fell_core::Result<T>) -> Self {
        if result.is_ok() {
            Self::Connected
        } else {
            Self::Failed
        }
    }
}

#[derive(Debug, Default)]
pub(super) enum Storage {
    #[default]
    Keyring,
    Saving {
        inline: bool,
    },
    Failed(SaveAccountError),
    ConfirmInline(SaveAccountError),
}

impl Storage {
    pub fn must_reopen(&self) -> bool {
        matches!(
            self,
            Self::Failed(SaveAccountError::File(
                crate::mailbox::AccountFileError::AlreadyConfigured
            ))
        )
    }

    pub fn inline_available(&self) -> bool {
        matches!(self, Self::Failed(SaveAccountError::Keyring { .. }))
    }

    pub fn confirm_inline(&mut self) {
        if self.inline_available()
            && let Self::Failed(error) = std::mem::take(self)
        {
            *self = Self::ConfirmInline(error);
        }
    }
}

pub(super) struct Form {
    pub step: Step,
    pub provider: Provider,
    pub checks: [Check; 2],
    pub bridge_unreachable: [bool; 2],
    pub issues: Vec<Issue>,
    pub storage: Storage,
    pub attempt: u64,
    email: String,
    name: String,
    username: String,
    password: String,
    servers: [Endpoint; 2],
    checked_from_servers: bool,
}

impl Default for Form {
    fn default() -> Self {
        Self {
            step: Step::Provider,
            provider: Provider::Bridge,
            checks: [Check::Waiting; 2],
            bridge_unreachable: [false; 2],
            issues: Vec::new(),
            storage: Storage::Keyring,
            attempt: 0,
            email: String::new(),
            name: String::new(),
            username: String::new(),
            password: String::new(),
            servers: Provider::Bridge.servers(),
            checked_from_servers: false,
        }
    }
}

impl Form {
    pub fn text(&self, field: Field) -> &str {
        match field {
            Field::Email => &self.email,
            Field::Name => &self.name,
            Field::Username => &self.username,
            Field::Password => &self.password,
            Field::Host(service) => &self.servers[service as usize].host,
            Field::Port(service) => &self.servers[service as usize].port,
            Field::Security(_) => unreachable!("security is a choice"),
        }
    }

    pub fn set_text(&mut self, field: Field, value: String) {
        let text = match field {
            Field::Email => &mut self.email,
            Field::Name => &mut self.name,
            Field::Username => &mut self.username,
            Field::Password => &mut self.password,
            Field::Host(service) => &mut self.servers[service as usize].host,
            Field::Port(service) => &mut self.servers[service as usize].port,
            Field::Security(_) => unreachable!("security is a choice"),
        };
        *text = value;
        self.issues.retain(|issue| match (field, issue.field) {
            (Field::Username, Field::Password) => false,
            _ => field != issue.field,
        });
    }

    pub fn security(&self, service: Service) -> ImapSecurity {
        self.servers[service as usize].security
    }

    pub fn offers_starttls(&self, service: Service) -> bool {
        self.security(service) != ImapSecurity::StartTls
            && self
                .issues
                .iter()
                .any(|issue| issue.field == Field::Security(service) && issue.suggest_starttls)
    }

    pub fn use_starttls(&mut self, service: Service) {
        let endpoint = &mut self.servers[service as usize];
        let (implicit, explicit) = match service {
            Service::Imap => ("993", "143"),
            Service::Smtp => ("465", "587"),
        };
        if self.provider != Provider::Bridge && endpoint.port.trim() == implicit {
            endpoint.port = explicit.into();
        }
        endpoint.security = ImapSecurity::StartTls;
        self.issues
            .retain(|issue| issue.field != Field::Security(service));
    }

    pub fn cycle_security(&mut self, service: Service, backwards: bool) {
        let endpoint = &mut self.servers[service as usize];
        let modes = if endpoint
            .host
            .trim()
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
        {
            &[
                ImapSecurity::Tls,
                ImapSecurity::StartTls,
                ImapSecurity::Plain,
            ][..]
        } else {
            &[ImapSecurity::Tls, ImapSecurity::StartTls][..]
        };
        let at = modes
            .iter()
            .position(|mode| *mode == endpoint.security)
            .unwrap_or(0);
        let offset = if backwards { modes.len() - 1 } else { 1 };
        endpoint.security = modes[(at + offset) % modes.len()];
        self.issues
            .retain(|issue| issue.field != Field::Security(service));
    }

    pub fn choose(&mut self, provider: Provider) {
        if self.provider != provider {
            self.provider = provider;
            self.servers = provider.servers();
            self.username.clear();
            self.password.clear();
            self.issues.clear();
        }
        self.step = Step::Identity;
    }

    pub fn edit_servers(&mut self) {
        self.step = Step::Servers;
    }

    pub fn needs_servers(&self) -> bool {
        self.provider == Provider::Custom
            || self
                .issues
                .iter()
                .any(|issue| issue.field.step() == Step::Servers)
    }

    pub fn back(&mut self) {
        self.step = match self.step {
            Step::Provider => Step::Provider,
            Step::Identity => Step::Provider,
            Step::Servers => Step::Identity,
            Step::Connection if self.checked_from_servers => Step::Servers,
            Step::Connection => Step::Identity,
            Step::Done if self.storage.must_reopen() => Step::Done,
            Step::Done if matches!(self.storage, Storage::Saving { .. }) => Step::Done,
            Step::Done if matches!(self.storage, Storage::ConfirmInline(_)) => {
                if let Storage::ConfirmInline(error) = std::mem::take(&mut self.storage) {
                    self.storage = Storage::Failed(error);
                }
                Step::Done
            }
            Step::Done => Step::Connection,
        };
    }

    pub fn validate_identity(&mut self) -> bool {
        self.issues
            .retain(|issue| issue.field.step() != Step::Identity);
        let email = self.email.trim();
        if !email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && !domain.is_empty() && !domain.contains('@')
        }) || email.contains(['<', '>'])
            || fell_core::compose::mailbox_parts(email).is_none()
        {
            self.issues
                .push(Issue::input(Field::Email, "Enter an email address."));
        }
        if self.password.is_empty() {
            self.issues
                .push(Issue::input(Field::Password, "Enter your mail password."));
        }
        !self
            .issues
            .iter()
            .any(|issue| issue.field.step() == Step::Identity)
    }

    pub fn begin_check(&mut self) -> Option<AccountConfig> {
        if !self.validate_identity() {
            self.step = Step::Identity;
            return None;
        }
        self.issues.clear();
        for service in Service::ALL {
            let endpoint = &self.servers[service as usize];
            let host = endpoint.host.trim();
            let loopback = host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
            if host.is_empty() || host.chars().any(char::is_whitespace) || host.contains(['/', '@'])
            {
                self.issues.push(Issue::input(
                    Field::Host(service),
                    "Enter a server hostname or IP address.",
                ));
            } else if self.provider == Provider::Bridge && !loopback {
                self.issues.push(Issue::input(
                    Field::Host(service),
                    "Use a loopback address for Bridge.",
                ));
            } else if endpoint.security == ImapSecurity::Plain && !loopback {
                self.issues.push(Issue::input(
                    Field::Security(service),
                    "Use TLS or STARTTLS for a remote server.",
                ));
            }
            if port(&endpoint.port).is_none() {
                self.issues
                    .push(Issue::input(Field::Port(service), "Port must be 1–65535."));
            }
        }
        if !self.issues.is_empty() {
            self.edit_servers();
            return None;
        }
        let config = self.config();
        if self.step != Step::Connection {
            self.checked_from_servers = self.step == Step::Servers;
        }
        self.step = Step::Connection;
        self.attempt = self.attempt.wrapping_add(1);
        self.checks = [Check::Testing, Check::Waiting];
        self.bridge_unreachable = [false; 2];
        self.storage = Storage::Keyring;
        Some(config)
    }

    fn config(&self) -> AccountConfig {
        let email = self.email.trim();
        let username = self.username.trim();
        let username = if username.is_empty() { email } else { username };
        let [imap, smtp] = &self.servers;
        let mut config = AccountConfig::bridge(
            email,
            username,
            PasswordSource::Inline(self.password.clone()),
        );
        if self.provider != Provider::Bridge {
            config.backend = BackendKind::Imap;
        }
        config.name = self.name.trim().into();
        config.host = imap.host.trim().into();
        config.port = port(&imap.port).expect("validated IMAP port");
        config.security = imap.security;
        config.smtp_host = Some(smtp.host.trim().into());
        config.smtp_port = port(&smtp.port);
        config.smtp_security = Some(smtp.security);
        config
    }

    pub fn accepts_check(&self, attempt: u64) -> bool {
        self.step == Step::Connection && self.attempt == attempt
    }

    pub fn finish_check(
        &mut self,
        attempt: u64,
        config: &AccountConfig,
        result: fell_core::Result<ConnectionReport>,
    ) -> Option<Field> {
        if !self.accepts_check(attempt) {
            return None;
        }
        match result {
            Ok(report) => {
                self.checks = [Check::finished(&report.imap), Check::finished(&report.smtp)];
                for (service, error) in [
                    (Service::Imap, report.imap.err()),
                    (Service::Smtp, report.smtp.err()),
                ] {
                    if let Some(error) = error {
                        self.record_check_issue(config, service, &error);
                    }
                }
            }
            Err(error) => {
                self.checks = [Check::Failed, Check::Waiting];
                self.record_check_issue(config, Service::Imap, &error);
            }
        }
        // Leave both failures on their fields. Visit credentials first if one
        // login failed, otherwise open the relevant server settings.
        let first = self
            .issues
            .iter()
            .min_by_key(|issue| issue.field.step() != Step::Identity)?
            .field;
        if first.step() == Step::Servers {
            self.edit_servers();
        } else {
            self.step = Step::Identity;
        }
        Some(first)
    }

    fn record_check_issue(
        &mut self,
        config: &AccountConfig,
        service: Service,
        error: &fell_core::Error,
    ) {
        if config.backend == BackendKind::Bridge
            && fell_core::classify_sync_error(error) == SyncFailureKind::Unreachable
        {
            self.bridge_unreachable[service as usize] = true;
        } else {
            let issue = Issue::connection(config, service, error);
            if !self.issues.contains(&issue) {
                self.issues.push(issue);
            }
        }
    }

    pub fn can_retry(&self) -> bool {
        self.bridge_unreachable.contains(&true) && !self.checks.contains(&Check::Testing)
    }

    pub fn connected(&self) -> bool {
        self.checks == [Check::Connected; 2]
    }

    pub fn done(&mut self) {
        if self.step == Step::Connection && self.connected() {
            self.step = Step::Done;
        }
    }

    pub fn begin_save(&mut self) -> Option<(AccountConfig, bool)> {
        if self.step != Step::Done
            || matches!(self.storage, Storage::Saving { .. })
            || self.storage.must_reopen()
        {
            return None;
        }
        let inline = matches!(self.storage, Storage::ConfirmInline(_));
        self.storage = Storage::Saving { inline };
        Some((self.config(), inline))
    }
}

fn port(text: &str) -> Option<u16> {
    text.trim().parse().ok().filter(|port| *port != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fell_core::Error;

    #[test]
    fn starttls_recovery_updates_standard_ports_and_keeps_custom_ports() {
        let mut form = Form::default();
        form.choose(Provider::Custom);
        form.set_text(Field::Port(Service::Smtp), "2525".into());
        form.use_starttls(Service::Imap);
        form.use_starttls(Service::Smtp);
        assert_eq!(form.security(Service::Imap), ImapSecurity::StartTls);
        assert_eq!(form.security(Service::Smtp), ImapSecurity::StartTls);
        assert_eq!(form.text(Field::Port(Service::Imap)), "143");
        assert_eq!(form.text(Field::Port(Service::Smtp)), "2525");
    }

    fn identity(form: &mut Form) {
        form.set_text(Field::Email, "robin@example.test".into());
        form.set_text(Field::Name, "Robin Ellis".into());
        form.set_text(Field::Password, "local-demo-only".into());
    }

    fn connected(form: &mut Form) {
        let config = form.begin_check().unwrap();
        assert_eq!(form.step, Step::Connection);
        assert_eq!(form.checks, [Check::Testing, Check::Waiting]);
        let report = ConnectionReport {
            imap: Ok(1),
            smtp: Ok(()),
        };
        assert_eq!(form.finish_check(form.attempt, &config, Ok(report)), None);
        assert!(form.connected());
    }

    #[test]
    fn bridge_walkthrough_and_back_preserve_the_form() {
        let mut form = Form::default();
        assert_eq!(form.step, Step::Provider);
        assert_eq!(form.provider, Provider::Bridge);
        form.choose(Provider::Bridge);
        assert_eq!(form.step, Step::Identity);
        identity(&mut form);
        form.back();
        assert_eq!(form.step, Step::Provider);
        form.choose(Provider::Bridge);
        assert_eq!(form.text(Field::Name), "Robin Ellis");
        form.edit_servers();
        form.back();
        assert!(!form.needs_servers());
        connected(&mut form);
        form.done();
        assert_eq!(form.step, Step::Done);
        form.back();
        assert_eq!(form.step, Step::Connection);
        form.done();
        let (config, inline) = form.begin_save().unwrap();
        assert!(!inline);
        assert_eq!(config.backend, BackendKind::Bridge);
        assert_eq!(config.username, config.email);
        assert_eq!(config.port, 1143);
        assert_eq!(config.smtp_config().port, 1025);
        assert!(form.begin_save().is_none());
        form.back();
        assert_eq!(form.step, Step::Done);
    }

    #[test]
    fn invalid_inputs_stay_by_the_field_and_plain_is_loopback_only() {
        let mut form = Form::default();
        form.choose(Provider::Custom);
        assert!(form.needs_servers());
        form.set_text(Field::Email, "name@".into());
        assert!(form.begin_check().is_none());
        assert_eq!(form.step, Step::Identity);
        assert_eq!(
            form.issues
                .iter()
                .map(|issue| issue.field)
                .collect::<Vec<_>>(),
            [Field::Email, Field::Password]
        );
        identity(&mut form);
        form.edit_servers();
        form.set_text(Field::Port(Service::Imap), "65536".into());
        assert!(form.begin_check().is_none());
        assert_eq!(form.step, Step::Servers);
        assert!(
            form.issues
                .iter()
                .any(|issue| issue.field == Field::Port(Service::Imap))
        );
        for service in Service::ALL {
            form.set_text(Field::Host(service), "127.0.0.1".into());
            form.set_text(Field::Port(service), "1234".into());
            while form.security(service) != ImapSecurity::Plain {
                form.cycle_security(service, false);
            }
        }
        form.set_text(Field::Host(Service::Imap), "imap.example.test".into());
        assert!(form.begin_check().is_none());
        assert_eq!(form.issues[0].field, Field::Security(Service::Imap));
        form.set_text(Field::Host(Service::Imap), "127.0.0.1".into());
        assert!(form.begin_check().unwrap().validate().is_ok());
        form.back();
        assert_eq!(form.step, Step::Servers);
        form.back();
        assert_eq!(form.step, Step::Identity);
    }

    #[test]
    fn a_stale_login_is_ignored_and_auth_and_tls_failures_are_actionable() {
        let mut form = Form::default();
        identity(&mut form);
        let config = form.begin_check().unwrap();
        let old = form.attempt;
        form.back();
        assert!(!form.accepts_check(old));
        let _ = form.begin_check().unwrap();
        let report = || ConnectionReport {
            imap: Err(Error::Backend("imap: LOGIN failed: NO".into())),
            smtp: Err(Error::Tls("SMTP STARTTLS".into())),
        };
        assert_eq!(form.finish_check(old, &config, Ok(report())), None);
        assert!(form.issues.is_empty());
        assert_eq!(
            form.finish_check(form.attempt, &config, Ok(report())),
            Some(Field::Password)
        );
        assert_eq!(form.step, Step::Identity);
        assert!(form.needs_servers());
        assert!(
            form.issues[0]
                .recovery
                .unwrap()
                .contains("Use the password Bridge shows, not your Proton password.")
        );
        assert_eq!(form.issues[1].field, Field::Security(Service::Smtp));
        assert_eq!(
            form.issues[1].recovery,
            Some("Bridge uses STARTTLS on port 1025 for outgoing mail.")
        );
        form.set_text(Field::Password, "corrected".into());
        assert_eq!(form.issues.len(), 1);
    }

    #[test]
    fn bridge_reachability_failure_can_retry_on_the_connection_step() {
        let mut form = Form::default();
        identity(&mut form);
        let config = form.begin_check().unwrap();
        let unreachable = Error::Backend("imap: connect 127.0.0.1:1143 failed: refused".into());
        let report = ConnectionReport {
            imap: Err(unreachable),
            smtp: Ok(()),
        };
        assert_eq!(form.finish_check(form.attempt, &config, Ok(report)), None);
        assert_eq!(form.step, Step::Connection);
        assert_eq!(form.bridge_unreachable, [true, false]);
        assert!(form.can_retry());
        assert_eq!(form.begin_check(), Some(config));
        assert!(!form.can_retry());
        form.back();
        assert_eq!(form.step, Step::Identity);
    }

    #[test]
    fn inline_fallback_requires_a_keyring_failure_and_confirmation() {
        let mut form = Form::default();
        identity(&mut form);
        connected(&mut form);
        form.done();
        form.storage = Storage::Failed(SaveAccountError::File("write failed".into()));
        assert!(!form.storage.inline_available());
        form.storage = Storage::Failed(SaveAccountError::Keyring {
            detail: "keyring unavailable".into(),
            kind: None,
            recovery: None,
        });
        assert!(form.storage.inline_available());
        form.storage.confirm_inline();
        assert!(matches!(form.storage, Storage::ConfirmInline(_)));
        form.back();
        assert_eq!(form.step, Step::Done);
        assert!(form.storage.inline_available());
        form.storage.confirm_inline();
        assert!(form.begin_save().unwrap().1);
    }
}
