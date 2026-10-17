//! Versioned credential bundles. This module never logs provider errors or secret values.
use crate::model::DataSource;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

pub(crate) const VAULT_ACCOUNT: &str = "vault-v1";

/// What the system password storage is called in messages.
pub const STORE_NAME: &str = if cfg!(target_os = "macos") { "Keychain" } else { "system keyring" };

/// Immutable credential revisions share one stable, protected Keychain item.
/// The owner prevents a copied configuration from pruning another file's revisions.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Vault {
    pub version: u8,
    pub owner: String,
    pub entries: BTreeMap<String, Secrets>,
}

impl Vault {
    pub(crate) fn new(owner: String) -> Self {
        Self {
            version: 1,
            owner,
            entries: BTreeMap::new(),
        }
    }
    pub(crate) fn decode(bytes: &[u8], owner: &str) -> Result<Self, String> {
        let vault: Self = serde_json::from_slice(bytes).map_err(|_| "Invalid password vault")?;
        if vault.version != 1 {
            return Err("Unsupported password vault version".into());
        }
        if vault.owner != owner {
            return Err("The password vault belongs to another configuration location. Restore the original configuration path; copied configurations cannot share a password vault.".into());
        }
        for reference in vault.entries.keys() {
            let (_, revision) = reference
                .rsplit_once(':')
                .ok_or("Invalid vault reference")?;
            uuid::Uuid::parse_str(revision).map_err(|_| "Invalid vault revision")?;
        }
        Ok(vault)
    }
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Secrets {
    password: String,
    ssh_password: String,
    passphrase: String,
    #[serde(default)]
    tls_identity_password: String,
}

impl Secrets {
    pub(crate) fn from_source(source: &DataSource) -> Self {
        Self {
            password: source.password.clone(),
            ssh_password: source.ssh.password.clone(),
            passphrase: source.ssh.passphrase.clone(),
            tls_identity_password: source.tls_identity_password.clone(),
        }
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.password.is_empty()
            && self.ssh_password.is_empty()
            && self.passphrase.is_empty()
            && self.tls_identity_password.is_empty()
    }
    pub(crate) fn apply(self, source: &mut DataSource) {
        source.password = self.password;
        source.ssh.password = self.ssh_password;
        source.ssh.passphrase = self.passphrase;
        source.tls_identity_password = self.tls_identity_password;
    }
}

/// Store errors contain a fixed description and, on macOS, an OSStatus only.
pub trait CredentialStore: Send {
    fn read(&mut self, service: &str, account: &str) -> Result<Option<Vec<u8>>, String>;
    fn write(&mut self, service: &str, account: &str, value: &[u8]) -> Result<(), String>;
    fn delete(&mut self, service: &str, account: &str) -> Result<(), String>;
}

type MemoryEntries = HashMap<(String, String), Vec<u8>>;

/// Explicit dependency injection for tests; never selected by a config path or environment variable.
#[derive(Clone, Default)]
pub struct MemoryStore {
    entries: std::sync::Arc<std::sync::Mutex<MemoryEntries>>,
}
impl CredentialStore for MemoryStore {
    fn read(&mut self, service: &str, account: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(self
            .entries
            .lock()
            .unwrap()
            .get(&(service.into(), account.into()))
            .cloned())
    }
    fn write(&mut self, service: &str, account: &str, value: &[u8]) -> Result<(), String> {
        self.entries
            .lock()
            .unwrap()
            .insert((service.into(), account.into()), value.into());
        Ok(())
    }
    fn delete(&mut self, service: &str, account: &str) -> Result<(), String> {
        self.entries
            .lock()
            .unwrap()
            .remove(&(service.into(), account.into()));
        Ok(())
    }
}

#[derive(Default)]
pub struct SystemStore {
    #[cfg(target_os = "macos")]
    keychain: Option<security_framework::os::macos::keychain::SecKeychain>,
    #[cfg(target_os = "linux")]
    secrets: Option<secret_service::blocking::SecretService<'static>>,
}
#[cfg(target_os = "macos")]
impl SystemStore {
    fn keychain(
        &mut self,
    ) -> Result<&mut security_framework::os::macos::keychain::SecKeychain, String> {
        if self.keychain.is_none() {
            self.keychain = Some(native::default_keychain()?);
        }
        Ok(self.keychain.as_mut().unwrap())
    }
}
#[cfg(target_os = "macos")]
impl CredentialStore for SystemStore {
    fn read(&mut self, service: &str, account: &str) -> Result<Option<Vec<u8>>, String> {
        native::read(self.keychain()?, service, account)
    }
    fn write(&mut self, service: &str, account: &str, value: &[u8]) -> Result<(), String> {
        native::write(self.keychain()?, service, account, value)
    }
    fn delete(&mut self, service: &str, account: &str) -> Result<(), String> {
        native::delete(self.keychain()?, service, account)
    }
}
#[cfg(target_os = "linux")]
impl SystemStore {
    /// The default collection of the Secret Service (GNOME Keyring, KWallet,
    /// KeePassXC), unlocked; the desktop may ask the user to unlock it.
    fn collection(&mut self) -> Result<secret_service::blocking::Collection<'_>, String> {
        use secret_service::{EncryptionType, blocking::SecretService};
        if self.secrets.is_none() {
            self.secrets = Some(SecretService::connect(EncryptionType::Dh).map_err(|_| {
                "No system keyring is available (Secret Service over D-Bus, such as GNOME Keyring or KWallet). Choose Until restart.".to_string()
            })?);
        }
        let collection = self
            .secrets
            .as_ref()
            .expect("connected above")
            .get_default_collection()
            .map_err(linux::error)?;
        if collection.is_locked().map_err(linux::error)? {
            collection.unlock().map_err(linux::error)?;
        }
        Ok(collection)
    }
}
#[cfg(target_os = "linux")]
impl CredentialStore for SystemStore {
    fn read(&mut self, service: &str, account: &str) -> Result<Option<Vec<u8>>, String> {
        let collection = self.collection()?;
        let items = collection
            .search_items(linux::attributes(service, account))
            .map_err(linux::error)?;
        let Some(item) = items.first() else {
            return Ok(None);
        };
        if item.is_locked().map_err(linux::error)? {
            item.unlock().map_err(linux::error)?;
        }
        item.get_secret().map(Some).map_err(linux::error)
    }
    fn write(&mut self, service: &str, account: &str, value: &[u8]) -> Result<(), String> {
        let collection = self.collection()?;
        // `replace` updates the item with the same attributes in place.
        collection
            .create_item(
                &format!("DBeast {service} {account}"),
                linux::attributes(service, account),
                value,
                true,
                "application/octet-stream",
            )
            .map(|_| ())
            .map_err(linux::error)
    }
    fn delete(&mut self, service: &str, account: &str) -> Result<(), String> {
        let collection = self.collection()?;
        for item in collection
            .search_items(linux::attributes(service, account))
            .map_err(linux::error)?
        {
            item.delete().map_err(linux::error)?;
        }
        Ok(())
    }
}
#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    // Never include provider details: they may echo attributes or paths.
    pub(super) fn error(_: secret_service::Error) -> String {
        "System keyring access failed. Unlock the keyring or allow access, then retry.".into()
    }
    pub(super) fn attributes<'a>(service: &'a str, account: &'a str) -> HashMap<&'a str, &'a str> {
        HashMap::from([("application", "dbeast"), ("service", service), ("account", account)])
    }
}
#[cfg(windows)]
impl CredentialStore for SystemStore {
    fn read(&mut self, service: &str, account: &str) -> Result<Option<Vec<u8>>, String> {
        windows::read(service, account)
    }
    fn write(&mut self, service: &str, account: &str, value: &[u8]) -> Result<(), String> {
        windows::write(service, account, value)
    }
    fn delete(&mut self, service: &str, account: &str) -> Result<(), String> {
        windows::delete(service, account)
    }
}
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
impl CredentialStore for SystemStore {
    fn read(&mut self, _: &str, _: &str) -> Result<Option<Vec<u8>>, String> {
        Err("System password storage is supported on macOS, Linux and Windows. Choose Until restart.".into())
    }
    fn write(&mut self, _: &str, _: &str, _: &[u8]) -> Result<(), String> {
        Err("System password storage is supported on macOS, Linux and Windows. Choose Until restart.".into())
    }
    fn delete(&mut self, _: &str, _: &str) -> Result<(), String> {
        Err("System password storage is supported on macOS, Linux and Windows.".into())
    }
}

/// Windows keeps each secret in a file under the configuration folder,
/// encrypted with DPAPI for the current user account. Credential Manager is
/// not used because it limits a secret to 2.5 KB, and the vault holding every
/// source's passwords can be larger.
#[cfg(windows)]
mod windows {
    use sha2::{Digest, Sha256};
    use std::path::PathBuf;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    fn path(service: &str, account: &str) -> PathBuf {
        let name: String = Sha256::digest(format!("{service}\0{account}"))
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        crate::config::Config::data_dir().join("credentials").join(format!("{name}.dpapi"))
    }

    /// Binds a secret to its service and account, so a file copied over
    /// another entry does not decrypt.
    fn entropy(service: &str, account: &str) -> Vec<u8> {
        format!("dbeast\0{service}\0{account}").into_bytes()
    }

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
    }

    /// Copies a blob allocated by DPAPI and frees it.
    fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        // SAFETY: DPAPI returned a LocalAlloc buffer of cbData bytes, freed once here.
        unsafe {
            let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            LocalFree(out.pbData.cast());
            bytes
        }
    }

    pub(super) fn read(service: &str, account: &str) -> Result<Option<Vec<u8>>, String> {
        let sealed = match std::fs::read(path(service, account)) {
            Ok(sealed) => sealed,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("Cannot read the saved password file.".into()),
        };
        let entropy = entropy(service, account);
        let (input, extra) = (blob(&sealed), blob(&entropy));
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: the input blobs point to live buffers; DPAPI fills `out`.
        let ok = unsafe {
            CryptUnprotectData(&input, std::ptr::null_mut(), &extra, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        };
        if ok == 0 {
            return Err("Saved passwords cannot be decrypted by this Windows account. Enter them again.".into());
        }
        Ok(Some(take(out)))
    }

    pub(super) fn write(service: &str, account: &str, value: &[u8]) -> Result<(), String> {
        let entropy = entropy(service, account);
        let (input, extra) = (blob(value), blob(&entropy));
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: the input blobs point to live buffers; DPAPI fills `out`.
        let ok = unsafe {
            CryptProtectData(&input, std::ptr::null(), &extra, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        };
        if ok == 0 {
            return Err("Windows could not encrypt the password (DPAPI).".into());
        }
        let sealed = take(out);
        let path = path(service, account);
        let dir = path.parent().expect("credentials folder");
        std::fs::create_dir_all(dir).map_err(|_| "Cannot create the password folder.".to_string())?;
        // Write and rename, so an interrupted save keeps the previous secret.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &sealed).map_err(|_| "Cannot save the password file.".to_string())?;
        std::fs::rename(&tmp, &path).map_err(|_| "Cannot save the password file.".to_string())
    }

    pub(super) fn delete(service: &str, account: &str) -> Result<(), String> {
        match std::fs::remove_file(path(service, account)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err("Cannot delete the saved password file.".into()),
        }
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn secrets_round_trip_and_stay_bound_to_their_entry() {
            let service = format!("dbeast-test-{}", uuid::Uuid::new_v4());
            assert_eq!(super::read(&service, "a").unwrap(), None);
            super::write(&service, "a", b"secret").unwrap();
            assert_eq!(super::read(&service, "a").unwrap().as_deref(), Some(&b"secret"[..]));
            std::fs::copy(super::path(&service, "a"), super::path(&service, "b")).unwrap();
            assert!(super::read(&service, "b").is_err(), "entropy binds the file to its entry");
            for account in ["a", "b"] {
                super::delete(&service, account).unwrap();
            }
            assert_eq!(super::read(&service, "a").unwrap(), None);
        }
    }
}

#[cfg(target_os = "macos")]
pub mod native {
    // security-framework wraps Apple's Keychain Services; explicit keychain handles
    // keep transaction targets stable and let tests use disposable keychains only.
    // https://docs.rs/security-framework/3.7.0/security_framework/os/macos/passwords/index.html
    // https://docs.rs/security-framework/3.7.0/security_framework/item/struct.ItemSearchOptions.html
    use security_framework::{
        base::Error,
        item::{ItemClass, ItemSearchOptions},
        os::macos::{keychain::SecKeychain, passwords::find_generic_password},
    };
    fn error(error: Error) -> String {
        format!(
            "Keychain access failed (OSStatus {}). Unlock the keychain or allow access, then retry.",
            error.code()
        )
    }
    pub fn default_keychain() -> Result<SecKeychain, String> {
        SecKeychain::default().map_err(error)
    }
    pub fn read(
        keychain: &SecKeychain,
        service: &str,
        account: &str,
    ) -> Result<Option<Vec<u8>>, String> {
        match find_generic_password(Some(std::slice::from_ref(keychain)), service, account) {
            Ok((password, _)) => Ok(Some(password.to_vec())),
            Err(e) if e.code() == -25300 => Ok(None),
            Err(e) => Err(error(e)),
        }
    }
    pub fn write(
        keychain: &mut SecKeychain,
        service: &str,
        account: &str,
        value: &[u8],
    ) -> Result<(), String> {
        // Update the same item in place, preserving its identity and access controls.
        // Unlike set_generic_password, never fall back to add after an access denial.
        match find_generic_password(Some(std::slice::from_ref(keychain)), service, account) {
            Ok((_, mut item)) => item.set_password(value).map_err(error),
            Err(e) if e.code() == -25300 => keychain
                .add_generic_password(service, account, value)
                .map(|_| ())
                .map_err(error),
            Err(e) => Err(error(e)),
        }
    }
    pub fn delete(keychain: &SecKeychain, service: &str, account: &str) -> Result<(), String> {
        match ItemSearchOptions::new()
            .keychains(std::slice::from_ref(keychain))
            .class(ItemClass::generic_password())
            .service(service)
            .account(account)
            .delete()
        {
            Ok(()) => Ok(()),
            Err(e) if e.code() == -25300 => Ok(()),
            Err(e) => Err(error(e)),
        }
    }
}

/// Reopening the same test config in one process simulates a persistent vault.
pub(crate) fn fixture_store() -> MemoryStore {
    use std::sync::{LazyLock, Mutex};
    static STORES: LazyLock<Mutex<HashMap<std::path::PathBuf, MemoryStore>>> =
        LazyLock::new(Default::default);
    let path = crate::config::Config::path().expect("test config path");
    STORES.lock().unwrap().entry(path).or_default().clone()
}

#[cfg(test)]
mod tls_secret_tests {
    use super::*;
    use crate::model::DriverKind;

    #[test]
    fn old_bundles_load_and_tls_secret_stays_out_of_config_and_debug() {
        let old = br#"{"password":"old-db","ssh_password":"old-ssh","passphrase":"old-key"}"#;
        let secrets: Secrets = serde_json::from_slice(old).unwrap();
        let mut source = DataSource::new(DriverKind::Postgres);
        secrets.apply(&mut source);
        assert!(source.tls_identity_password.is_empty());
        source.tls_identity_password = "synthetic-tls-archive-secret".into();
        let bundle = serde_json::to_vec(&Secrets::from_source(&source)).unwrap();
        let restored: Secrets = serde_json::from_slice(&bundle).unwrap();
        let mut destination = DataSource::new(DriverKind::Postgres);
        restored.apply(&mut destination);
        assert_eq!(
            source.tls_identity_password,
            destination.tls_identity_password
        );
        assert!(
            !serde_json::to_string(&source)
                .unwrap()
                .contains("synthetic-tls")
        );
        assert!(!format!("{source:?}").contains("synthetic-tls"));
        let mut tls_only = DataSource::new(DriverKind::MySql);
        tls_only.tls_identity_password = "synthetic-tls-only".into();
        assert!(!Secrets::from_source(&tls_only).is_empty());
    }
}
