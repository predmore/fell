//! Account passwords stored by the desktop Secret Service.

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum KeyringError {
    #[error("no password saved in the desktop keyring for this account")]
    Missing,
    #[error("the desktop keyring is locked")]
    Locked,
    #[error("the desktop Secret Service is unavailable")]
    Unavailable,
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
impl PasswordStore for SecretService {
    fn read(&self, key: &str) -> Result<String, KeyringError> {
        async_io::block_on(async {
            let service = oo7::dbus::Service::new().await.map_err(service_error)?;
            let collection = service
                .with_alias(oo7::dbus::Service::DEFAULT_COLLECTION)
                .await
                .map_err(service_error)?
                .ok_or(KeyringError::Missing)?;
            let items = collection
                .search_items(&attributes(key))
                .await
                .map_err(service_error)?;
            let item = match items.as_slice() {
                [] => return Err(KeyringError::Missing),
                [item] => item,
                _ => return Err(KeyringError::Ambiguous),
            };
            // Sync can retry in the background. Leave unlocking to the user.
            let secret = item.secret().await.map_err(service_error)?;
            let password =
                std::str::from_utf8(secret.as_bytes()).map_err(|_| KeyringError::Invalid)?;
            if password.is_empty() {
                return Err(KeyringError::Invalid);
            }
            Ok(password.to_owned())
        })
    }

    fn store(&self, key: &str, password: &str) -> Result<(), KeyringError> {
        if password.is_empty() {
            return Err(KeyringError::Invalid);
        }
        async_io::block_on(async {
            let service = oo7::dbus::Service::new().await.map_err(service_error)?;
            let collection = service.default_collection().await.map_err(service_error)?;
            if collection.is_locked().await.map_err(service_error)? {
                collection.unlock(None).await.map_err(service_error)?;
            }
            collection
                .create_item("Fell mail password", &attributes(key), password, true, None)
                .await
                .map_err(service_error)?;
            Ok(())
        })
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn attributes(key: &str) -> std::collections::HashMap<&str, &str> {
    std::collections::HashMap::from([("application", "fell"), ("account", key)])
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn service_error(error: oo7::dbus::Error) -> KeyringError {
    use oo7::dbus::{Error, ServiceError};
    match error {
        Error::Service(ServiceError::IsLocked(_)) | Error::Dismissed => KeyringError::Locked,
        Error::Service(ServiceError::NoSuchObject(_)) | Error::Deleted => KeyringError::Missing,
        _ => KeyringError::Unavailable,
    }
}

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
impl PasswordStore for SecretService {
    fn read(&self, _: &str) -> Result<String, KeyringError> {
        Err(KeyringError::Unavailable)
    }

    fn store(&self, _: &str, _: &str) -> Result<(), KeyringError> {
        Err(KeyringError::Unavailable)
    }
}
