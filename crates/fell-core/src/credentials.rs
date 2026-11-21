//! Account passwords stored by the desktop Secret Service.

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum KeyringError {
    #[error("no password saved in the desktop keyring for this account")]
    Missing,
    #[error("the desktop keyring is locked")]
    Locked,
    #[error("the desktop Secret Service {}", if *.timed_out { "did not respond in time" } else { "is unavailable" })]
    Unavailable { timed_out: bool },
    #[error("the keyring password is empty or is not valid UTF-8")]
    Invalid,
    #[error("the desktop keyring has multiple passwords for this account")]
    Ambiguous,
}

/// Blocking storage for a Fell account's credential key. Use on a worker
/// thread. Implementations should replace the password for an existing key.
pub trait PasswordStore {
    fn read(&self, key: &str) -> Result<String, KeyringError>;
    fn store(&self, key: &str, password: &str) -> Result<(), KeyringError>;
}

/// The default collection of the desktop Secret Service on the session bus.
pub struct SecretService;

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod desktop {
    use std::future::Future;
    use std::time::Duration;

    use oo7::dbus::{Error, Service, ServiceError};

    use super::{KeyringError, PasswordStore, SecretService};

    const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
    const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);

    impl PasswordStore for SecretService {
        fn read(&self, key: &str) -> Result<String, KeyringError> {
            async_io::block_on(read_password(key, REQUEST_TIMEOUT, Service::new()))
        }

        fn store(&self, key: &str, password: &str) -> Result<(), KeyringError> {
            if password.is_empty() {
                return Err(KeyringError::Invalid);
            }
            async_io::block_on(async {
                let service = with_deadline(REQUEST_TIMEOUT, Service::new()).await?;
                let alias = Service::DEFAULT_COLLECTION;
                let collection =
                    match with_deadline(REQUEST_TIMEOUT, service.with_alias(alias)).await? {
                        Some(collection) => collection,
                        // Initial keyring creation can prompt just like unlocking.
                        None => {
                            with_deadline(
                                PROMPT_TIMEOUT,
                                service.create_collection("Default", alias, None),
                            )
                            .await?
                        }
                    };
                if with_deadline(REQUEST_TIMEOUT, collection.is_locked()).await? {
                    with_deadline(PROMPT_TIMEOUT, collection.unlock(None)).await?;
                }
                with_deadline(
                    REQUEST_TIMEOUT,
                    collection.create_item(
                        "Fell mail password",
                        &attributes(key),
                        password,
                        true,
                        None,
                    ),
                )
                .await?;
                Ok(())
            })
        }
    }

    async fn read_password(
        key: &str,
        timeout: Duration,
        connect: impl Future<Output = Result<Service<'static>, Error>>,
    ) -> Result<String, KeyringError> {
        with_deadline(timeout, async {
            let service = connect.await?;
            let collection = service
                .with_alias(Service::DEFAULT_COLLECTION)
                .await?
                .ok_or(KeyringError::Missing)?;
            let items = collection.search_items(&attributes(key)).await?;
            let item = match items.as_slice() {
                [] => return Err(KeyringError::Missing),
                [item] => item,
                _ => return Err(KeyringError::Ambiguous),
            };
            // Sync can retry in the background. Leave unlocking to the user.
            let secret = item.secret().await?;
            let password =
                std::str::from_utf8(secret.as_bytes()).map_err(|_| KeyringError::Invalid)?;
            if password.is_empty() {
                return Err(KeyringError::Invalid);
            }
            Ok(password.to_owned())
        })
        .await
    }

    async fn with_deadline<T, E>(
        timeout: Duration,
        request: impl Future<Output = Result<T, E>>,
    ) -> Result<T, KeyringError>
    where
        KeyringError: From<E>,
    {
        let deadline = async_io::Timer::after(timeout);
        futures_lite::future::or(
            async {
                deadline.await;
                Err(KeyringError::Unavailable { timed_out: true })
            },
            async { request.await.map_err(KeyringError::from) },
        )
        .await
    }

    fn attributes(key: &str) -> std::collections::HashMap<&str, &str> {
        std::collections::HashMap::from([("application", "fell"), ("account", key)])
    }

    impl From<Error> for KeyringError {
        fn from(error: Error) -> Self {
            match error {
                Error::Service(ServiceError::IsLocked(_)) | Error::Dismissed => Self::Locked,
                Error::Service(ServiceError::NoSuchObject(_)) | Error::Deleted => Self::Missing,
                _ => Self::Unavailable { timed_out: false },
            }
        }
    }

    #[cfg(test)]
    #[test]
    fn hung_keyring_read_returns_unavailable() {
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = async_io::block_on(read_password(
                "test-account",
                Duration::from_millis(20),
                std::future::pending(),
            ));
            send.send(result).unwrap();
        });
        let error = receive
            .recv_timeout(Duration::from_secs(2))
            .expect("a stalled Secret Service request must stop at the deadline")
            .unwrap_err();
        assert_eq!(error, KeyringError::Unavailable { timed_out: true });
        assert_eq!(
            error.to_string(),
            "the desktop Secret Service did not respond in time"
        );
        worker.join().unwrap();
    }
}

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
impl PasswordStore for SecretService {
    fn read(&self, _: &str) -> Result<String, KeyringError> {
        Err(KeyringError::Unavailable { timed_out: false })
    }

    fn store(&self, _: &str, _: &str) -> Result<(), KeyringError> {
        Err(KeyringError::Unavailable { timed_out: false })
    }
}
