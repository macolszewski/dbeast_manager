//! Serialized, transactional configuration and credential persistence, run off the UI thread.
use crate::{
    config::{Config, atomic_write},
    credentials::{CredentialStore, Secrets, VAULT_ACCOUNT, Vault},
    model::PasswordStorage,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

pub struct Loaded {
    pub config: Config,
    pub warning: Option<String>,
}

pub(crate) enum Command {
    Load,
    Save { config: Box<Config>, apply: bool },
    LoadMcp,
    SaveMcp(Box<crate::mcp::McpState>),
}

const MCP_ACCOUNT: &str = "mcp-v1";

#[derive(serde::Serialize, serde::Deserialize)]
struct McpRecord {
    version: u8,
    /// Same owner rule as the password vault: a copied configuration cannot use it.
    owner: String,
    state: crate::mcp::McpState,
}

#[derive(Clone)]
struct VaultSnapshot {
    value: Option<Vault>,
    revision: Option<[u8; 32]>,
}

pub(crate) struct Persistence {
    path: PathBuf,
    store: Box<dyn CredentialStore>,
    current: Option<Config>,
    /// Old per-source items from an interrupted migration in the previous format.
    pending: Vec<String>,
    writable: bool,
    migration_failed: bool,
    disk_revision: Option<[u8; 32]>,
    vault: Option<VaultSnapshot>,
    #[cfg(test)]
    fail_config_write: bool,
    #[cfg(test)]
    force_nondurable: bool,
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Advisory locks never delete their files: replacing a locked inode would defeat exclusion.
/// Lock attempts 10 ms apart before reporting another instance.
const LOCK_ATTEMPTS: u32 = 25;

fn lock_file(path: &Path) -> Result<std::fs::File, String> {
    // Without O_NOFOLLOW, refuse a lock path that is a link (Windows symlink or junction).
    #[cfg(not(unix))]
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("Invalid password persistence lock".into());
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|_| "Cannot open password persistence lock")?;
    let metadata = file
        .metadata()
        .map_err(|_| "Cannot inspect password persistence lock")?;
    if !metadata.is_file() {
        return Err("Invalid password persistence lock".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no arguments or side effects.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err("Password persistence lock has unsafe ownership or permissions".into());
        }
    }
    // A lock just released can stay held for a moment, e.g. by a child process
    // forked before it closed the descriptor; a short wait avoids a false conflict.
    for attempt in 0..LOCK_ATTEMPTS {
        if file.try_lock().is_ok() {
            return Ok(file);
        }
        if attempt + 1 < LOCK_ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    Err("Another DBeast instance is saving these passwords. Retry after it finishes.".into())
}

impl Persistence {
    pub(crate) fn new(path: PathBuf, store: Box<dyn CredentialStore>) -> Self {
        Self {
            path,
            store,
            current: None,
            pending: Vec::new(),
            writable: false,
            migration_failed: false,
            disk_revision: None,
            vault: None,
            #[cfg(test)]
            fail_config_write: false,
            #[cfg(test)]
            force_nondurable: false,
        }
    }
    fn sidecar_path(&self, suffix: &str) -> PathBuf {
        let mut path = self.path.as_os_str().to_os_string();
        path.push(suffix);
        PathBuf::from(path)
    }
    fn journal_path(&self) -> PathBuf {
        self.sidecar_path(".credentials-pending.json")
    }
    fn path_lock(&self) -> Result<std::fs::File, String> {
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent).map_err(|_| "Cannot create configuration directory")?;
        lock_file(&self.sidecar_path(".credentials.lock"))
    }
    fn namespace_lock(namespace: &str) -> Result<std::fs::File, String> {
        // The UUID is validated first. All config locations share this per-user namespace lock.
        // O_NOFOLLOW, owner and mode checks reject a pre-created unsafe lock in a shared /tmp.
        lock_file(&std::env::temp_dir().join(format!("dbeast-vault-{namespace}.lock")))
    }
    fn owner(&self) -> Result<String, String> {
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let absolute = parent
            .canonicalize()
            .map_err(|_| "Cannot resolve configuration location")?
            .join(
                self.path
                    .file_name()
                    .ok_or("Invalid configuration filename")?,
            );
        Ok(format!(
            "{:x}",
            Sha256::digest(absolute.as_os_str().as_encoded_bytes())
        ))
    }
    fn read_config(&self) -> Result<Option<Vec<u8>>, String> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err("Cannot read configuration. Original file was kept.".into()),
        }
    }
    fn check_config_revision(&self) -> Result<(), String> {
        if self.read_config()?.as_deref().map(digest) != self.disk_revision {
            return Err("Configuration changed in another instance. Reload settings before saving; passwords were not changed.".into());
        }
        Ok(())
    }
    fn write_config(&mut self, config: &Config) -> Result<bool, String> {
        #[cfg(test)]
        if self.fail_config_write {
            return Err("Synthetic config write failure".into());
        }
        let durable = atomic_write(&self.path, config)?;
        self.disk_revision = Some(digest(
            &serde_json::to_vec_pretty(config).map_err(|_| "Cannot encode configuration")?,
        ));
        #[cfg(test)]
        if self.force_nondurable {
            return Ok(false);
        }
        Ok(durable)
    }
    fn write_journal(&self, namespace: &str) -> Result<(), String> {
        let journal = serde_json::json!({ "namespace": namespace, "references": self.pending });
        if !atomic_write(&self.journal_path(), &journal)? {
            return Err("Cannot confirm password journal durability".into());
        }
        Ok(())
    }
    fn service(config: &Config) -> String {
        format!(
            "ai.inprojects.dbeast.credentials.{}",
            config.credential_namespace
        )
    }
    fn validate(config: &Config) -> Result<(), String> {
        uuid::Uuid::parse_str(&config.credential_namespace)
            .map_err(|_| "Invalid credential namespace")?;
        if config.credential_vault_version > 1 {
            return Err("Unsupported credential storage version".into());
        }
        let mut ids = HashSet::new();
        for source in &config.data_sources {
            if source.id.is_empty() || !ids.insert(&source.id) {
                return Err("Duplicate or empty source ID; configuration was not changed".into());
            }
            if let Some(reference) = &source.credential_ref {
                let prefix = format!("{}:", source.id);
                let revision = reference
                    .strip_prefix(&prefix)
                    .ok_or("Credential reference belongs to another source")?;
                uuid::Uuid::parse_str(revision).map_err(|_| "Invalid credential revision")?;
            }
        }
        let live: HashSet<_> = config
            .data_sources
            .iter()
            .filter_map(|s| s.credential_ref.as_ref())
            .collect();
        if config.credential_cleanup.iter().any(|r| live.contains(r)) {
            return Err("Live credential appears in cleanup journal".into());
        }
        for reference in &config.credential_cleanup {
            Self::validate_reference(reference)?;
        }
        Ok(())
    }
    fn validate_reference(reference: &str) -> Result<(), String> {
        let (_, revision) = reference
            .rsplit_once(':')
            .ok_or("Invalid cleanup reference")?;
        uuid::Uuid::parse_str(revision).map_err(|_| "Invalid cleanup revision".to_string())?;
        Ok(())
    }
    fn read_vault(&mut self, config: &Config) -> Result<VaultSnapshot, String> {
        let bytes = self.store.read(&Self::service(config), VAULT_ACCOUNT)?;
        Ok(VaultSnapshot {
            value: bytes
                .as_deref()
                .map(|bytes| Vault::decode(bytes, &self.owner()?))
                .transpose()?,
            revision: bytes.as_deref().map(digest),
        })
    }
    fn ensure_vault(&mut self, config: &Config) -> Result<(), String> {
        if self.vault.is_none() {
            self.vault = Some(self.read_vault(config)?);
        }
        Ok(())
    }
    fn write_vault(&mut self, config: &Config, vault: Vault) -> Result<(), String> {
        // The namespace lock is held. A cached value alone could overwrite another process.
        let latest = self.read_vault(config)?;
        if let Some(expected) = &self.vault
            && latest.revision != expected.revision
        {
            return Err(
                "Password vault changed in another instance. Reload settings before saving.".into(),
            );
        }
        let bytes = serde_json::to_vec(&vault).map_err(|_| "Cannot encode password vault")?;
        // A first save may not have a config file/namespace yet. Persist the owner
        // before touching Keychain so a restart can find an ambiguously saved vault.
        self.write_journal(&config.credential_namespace)?;
        self.store
            .write(&Self::service(config), VAULT_ACCOUNT, &bytes)?;
        // Both the old and new config remain resolvable even if this read or the config write fails.
        let verified = self.store.read(&Self::service(config), VAULT_ACCOUNT)?;
        if verified.as_deref() != Some(bytes.as_slice()) {
            self.vault = None;
            return Err(format!("{} verification failed; configuration was kept", crate::credentials::STORE_NAME));
        }
        self.vault = Some(VaultSnapshot {
            value: Some(vault),
            revision: Some(digest(&bytes)),
        });
        Ok(())
    }
    fn cleanup(&mut self, config: &mut Config) -> Option<String> {
        let mut warning = None;
        if config.credential_vault_cleanup
            || !config.credential_cleanup.is_empty()
            || !self.pending.is_empty()
        {
            // Ownership/read failures must also block deletion of legacy items.
            let verified = (|| {
                let latest = self.read_vault(config)?;
                if self
                    .vault
                    .as_ref()
                    .is_some_and(|old| old.revision != latest.revision)
                {
                    return Err("Password vault changed in another instance. Reload settings before saving.".to_owned());
                }
                self.vault = Some(latest);
                Ok(())
            })();
            if let Err(error) = verified {
                return Some(format!(
                    "Settings saved, but protected password cleanup could not be verified. Retry after reloading settings. {error}"
                ));
            }
        }
        if config.credential_vault_cleanup {
            let result = (|| {
                self.ensure_vault(config)?;
                if let Some(mut compact) = self.vault.as_ref().and_then(|v| v.value.clone()) {
                    let live: HashSet<_> = config
                        .data_sources
                        .iter()
                        .filter_map(|s| s.credential_ref.as_ref())
                        .collect();
                    compact
                        .entries
                        .retain(|reference, _| live.contains(reference));
                    if self.vault.as_ref().and_then(|v| v.value.as_ref()) != Some(&compact) {
                        self.write_vault(config, compact)?;
                    }
                }
                Ok::<_, String>(())
            })();
            match result {
                Ok(()) => config.credential_vault_cleanup = false,
                Err(error) => {
                    return Some(format!(
                        "Settings saved, but old password revisions remain protected in the {}. Retry Apply. {error}",
                        crate::credentials::STORE_NAME
                    ));
                }
            }
        }
        let service = Self::service(config);
        let live: HashSet<_> = config
            .data_sources
            .iter()
            .filter_map(|s| s.credential_ref.as_ref())
            .collect();
        self.pending.retain(|reference| {
            // A journal written by the old implementation can include its successfully committed item.
            if config.credential_vault_version == 0 && live.contains(reference) {
                return false;
            }
            match self.store.delete(&service, reference) {
                Ok(()) => false,
                Err(_) => {
                    warning = Some(format!(
                        "Settings saved, but old {} items could not be removed. Retry Apply.",
                        crate::credentials::STORE_NAME
                    ));
                    true
                }
            }
        });
        config.credential_cleanup.retain(|reference| {
            match self.store.delete(&service, reference) {
                Ok(()) => false,
                Err(_) => {
                    warning = Some(format!(
                        "Settings saved, but old {} items could not be removed. Retry Apply.",
                        crate::credentials::STORE_NAME
                    ));
                    true
                }
            }
        });
        if let Err(error) = self.write_journal(&config.credential_namespace) {
            warning = Some(error);
        }
        warning
    }
    fn mcp_locks(&self) -> Result<(String, std::fs::File, std::fs::File), String> {
        let config = self
            .current
            .as_ref()
            .ok_or("Configuration is not loaded; agent connections were not changed.")?;
        let path_lock = self.path_lock()?;
        let namespace_lock = Self::namespace_lock(&config.credential_namespace)?;
        Ok((Self::service(config), path_lock, namespace_lock))
    }

    pub(crate) fn load_mcp(&mut self) -> Result<Option<crate::mcp::McpState>, String> {
        let (service, _path_lock, _namespace_lock) = self.mcp_locks()?;
        let Some(bytes) = self.store.read(&service, MCP_ACCOUNT)? else {
            return Ok(None);
        };
        let record: McpRecord =
            serde_json::from_slice(&bytes).map_err(|_| "Invalid saved agent connections")?;
        if record.version != 1 {
            return Err("Unsupported saved agent connections version".into());
        }
        if record.owner != self.owner()? {
            return Err("Saved agent connections belong to another configuration location.".into());
        }
        Ok(Some(record.state))
    }

    pub(crate) fn save_mcp(&mut self, state: crate::mcp::McpState) -> Result<(), String> {
        let (service, _path_lock, _namespace_lock) = self.mcp_locks()?;
        let bytes = serde_json::to_vec(&McpRecord {
            version: 1,
            owner: self.owner()?,
            state,
        })
        .map_err(|_| "Cannot encode agent connections")?;
        self.store.write(&service, MCP_ACCOUNT, &bytes)?;
        if self.store.read(&service, MCP_ACCOUNT)?.as_deref() != Some(bytes.as_slice()) {
            return Err(format!("{} verification of agent connections failed", crate::credentials::STORE_NAME));
        }
        Ok(())
    }

    pub(crate) fn load(&mut self) -> Result<Loaded, String> {
        self.writable = false;
        let _path_lock = self.path_lock()?;
        let previous = self.current.clone();
        let bytes = self.read_config()?;
        let (mut config, namespace_present) = match &bytes {
            Some(bytes) => {
                let value: serde_json::Value = serde_json::from_slice(bytes).map_err(
                    |_| "Cannot read configuration: invalid JSON. Original file was kept.",
                )?;
                let present = value.get("credential_namespace").is_some();
                (
                    serde_json::from_value::<Config>(value).map_err(
                        |_| "Cannot read configuration: invalid fields. Original file was kept.",
                    )?,
                    present,
                )
            }
            None => (Config::default(), false),
        };
        let mut recovered_namespace = false;
        match std::fs::read(self.journal_path()) {
            Ok(bytes) => {
                let journal: serde_json::Value = serde_json::from_slice(&bytes)
                    .map_err(|_| "Invalid password cleanup journal")?;
                let namespace = journal["namespace"]
                    .as_str()
                    .ok_or("Missing password cleanup namespace")?;
                if !namespace_present {
                    config.credential_namespace = namespace.to_owned();
                    recovered_namespace = true;
                }
                self.pending = serde_json::from_value(journal["references"].clone())
                    .map_err(|_| "Invalid password cleanup references")?;
                if namespace != config.credential_namespace && !self.pending.is_empty() {
                    return Err("Password cleanup journal belongs to another configuration".into());
                }
                for reference in &self.pending {
                    Self::validate_reference(reference)?;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.pending.clear(),
            Err(_) => return Err("Cannot read password cleanup journal".into()),
        }
        Self::validate(&config)?;
        let _namespace_lock = Self::namespace_lock(&config.credential_namespace)?;
        self.disk_revision = bytes.as_deref().map(digest);
        self.vault = None;
        let service = Self::service(&config);
        let mut warning = None;
        let mut legacy_plaintext = false;
        if recovered_namespace
            || config.credential_vault_version == 1
                && (config
                    .data_sources
                    .iter()
                    .any(|s| s.credential_ref.is_some())
                    || config.credential_vault_cleanup)
        {
            match self.read_vault(&config) {
                Ok(snapshot) => self.vault = Some(snapshot),
                Err(error) => warning = Some(error),
            }
        }
        let mut legacy_read_failed = false;
        for source in &mut config.data_sources {
            legacy_plaintext |= !Secrets::from_source(source).is_empty();
            if source.password_storage == PasswordStorage::ForSession
                && let Some(old) = previous.as_ref().and_then(|c| {
                    c.data_sources
                        .iter()
                        .find(|old| old.id == source.id && old.identity() == source.identity())
                })
            {
                Secrets::from_source(old).apply(source);
            }
            if let Some(reference) = &source.credential_ref {
                let secrets = if config.credential_vault_version == 1 {
                    self.vault
                        .as_ref()
                        .and_then(|s| s.value.as_ref())
                        .and_then(|v| v.entries.get(reference))
                        .cloned()
                        .ok_or_else(|| {
                            "Saved passwords are unavailable in the password vault".to_owned()
                        })
                } else if legacy_read_failed {
                    Err(format!("Password migration stopped after {} access failed. Retry loading when access is available.", crate::credentials::STORE_NAME))
                } else {
                    self.store
                        .read(&service, reference)
                        .and_then(|d| d.ok_or(format!("Saved passwords are missing from the {}", crate::credentials::STORE_NAME)))
                        .and_then(|d| {
                            serde_json::from_slice::<Secrets>(&d)
                                .map_err(|_| "Invalid saved passwords".into())
                        })
                };
                match secrets {
                    Ok(secrets) => secrets.apply(source),
                    Err(error) => {
                        source.credentials_unavailable = true;
                        if warning.is_none() {
                            warning = Some(error);
                        }
                        legacy_read_failed = true;
                    }
                }
            }
        }
        let migration = legacy_plaintext
            || config.credential_vault_version == 0
                && config
                    .data_sources
                    .iter()
                    .any(|s| s.credential_ref.is_some());
        if let Some(vault) = self.vault.as_ref().and_then(|v| v.value.as_ref()) {
            let live: HashSet<_> = config
                .data_sources
                .iter()
                .filter_map(|s| s.credential_ref.as_ref())
                .collect();
            config.credential_vault_cleanup |= vault.entries.keys().any(|r| !live.contains(r));
        }
        if warning.is_none()
            && (config.credential_vault_cleanup
                || !config.credential_cleanup.is_empty()
                || !self.pending.is_empty())
        {
            warning = Some("Old password revisions await cleanup. Use Apply in Data Sources and Drivers to retry.".into());
        }
        self.current = Some(config.clone());
        self.writable = true;
        self.migration_failed = false;
        // Cleanup warnings alone do not block a required migration, but missing secrets do.
        if migration
            && !config
                .data_sources
                .iter()
                .any(|s| s.credentials_unavailable)
        {
            match self.save_locked(config.clone(), true) {
                Ok(saved) => return Ok(saved),
                Err(error) => {
                    self.migration_failed = true;
                    warning = Some(format!(
                        "Password migration failed. Original configuration was kept. {error}"
                    ));
                }
            }
        }
        Ok(Loaded { config, warning })
    }
    pub(crate) fn save(&mut self, config: Config, apply: bool) -> Result<Loaded, String> {
        let _path_lock = self.path_lock()?;
        let current = self
            .current
            .as_ref()
            .ok_or("Configuration has not loaded; save was blocked")?;
        let _namespace_lock = Self::namespace_lock(&current.credential_namespace)?;
        self.check_config_revision()?;
        self.save_locked(config, apply)
    }
    fn save_locked(&mut self, mut config: Config, apply: bool) -> Result<Loaded, String> {
        if !self.writable {
            return Err("Configuration loading failed or is pending; save was blocked".into());
        }
        if !apply && self.migration_failed {
            return Err("Password migration is pending. Open Data Sources and Drivers and use Apply to retry or choose Until restart.".into());
        }
        let current = self
            .current
            .as_ref()
            .ok_or("Configuration has not loaded; save was blocked")?
            .clone();
        config.credential_namespace = current.credential_namespace.clone();
        config.credential_vault_version = 1;
        config.credential_vault_cleanup = current.credential_vault_cleanup;
        config.credential_cleanup = current.credential_cleanup.clone();
        if !apply {
            let scopes: std::collections::HashMap<_, _> = config
                .data_sources
                .iter()
                .map(|s| (s.id.clone(), s.scope.clone()))
                .collect();
            // Autosave never edits committed sources; it may only carry their
            // explorer scope and order (drag and drop in Database Explorer).
            let order: std::collections::HashMap<_, _> = config
                .data_sources
                .iter()
                .enumerate()
                .map(|(index, s)| (s.id.clone(), index))
                .collect();
            config.data_sources = current.data_sources.clone();
            config
                .data_sources
                .sort_by_key(|source| order.get(&source.id).copied().unwrap_or(usize::MAX));
            for source in &mut config.data_sources {
                if let Some(scope) = scopes.get(&source.id) {
                    source.scope = scope.clone();
                }
            }
        }
        Self::validate(&config)?;
        if config
            .data_sources
            .iter()
            .any(|s| s.credentials_unavailable)
        {
            return Err("Passwords are unavailable. Retry loading, or explicitly replace the passwords before saving.".into());
        }
        let needs_secrets = config.data_sources.iter().any(|s| {
            s.password_storage == PasswordStorage::Forever && !Secrets::from_source(s).is_empty()
        });
        if needs_secrets {
            self.ensure_vault(&current)?;
        }
        let mut vault = self
            .vault
            .as_ref()
            .and_then(|s| s.value.clone())
            .unwrap_or(Vault::new(self.owner()?));
        let mut changed = false;
        for source in &mut config.data_sources {
            let secrets = Secrets::from_source(source);
            let old = current.data_sources.iter().find(|s| s.id == source.id);
            source.credential_ref = None;
            if source.password_storage == PasswordStorage::Forever && !secrets.is_empty() {
                if let Some(old) = old.filter(|old| {
                    current.credential_vault_version == 1
                        && old.password_storage == PasswordStorage::Forever
                        && old
                            .credential_ref
                            .as_ref()
                            .and_then(|r| vault.entries.get(r))
                            == Some(&secrets)
                }) {
                    source.credential_ref = old.credential_ref.clone();
                } else {
                    let reference = format!("{}:{}", source.id, uuid::Uuid::new_v4());
                    vault.entries.insert(reference.clone(), secrets);
                    source.credential_ref = Some(reference);
                    changed = true;
                }
            }
        }
        if changed {
            // Every revision referenced by the last committed config must survive preparation.
            if current.credential_vault_version == 1
                && current
                    .data_sources
                    .iter()
                    .filter_map(|s| s.credential_ref.as_ref())
                    .any(|r| !vault.entries.contains_key(r))
            {
                return Err("Previous password revisions are unavailable. Reload before saving new passwords.".into());
            }
            self.write_vault(&current, vault.clone())?;
        }
        let live: HashSet<_> = config
            .data_sources
            .iter()
            .filter_map(|s| s.credential_ref.as_ref())
            .collect();
        for old in current
            .data_sources
            .iter()
            .filter_map(|s| s.credential_ref.as_ref())
        {
            if current.credential_vault_version == 0 {
                if !config.credential_cleanup.contains(old) {
                    config.credential_cleanup.push(old.clone());
                }
            } else if !live.contains(old) {
                config.credential_vault_cleanup = true;
            }
        }
        config.credential_vault_cleanup |= vault.entries.keys().any(|r| !live.contains(r));
        let durable = self.write_config(&config)?;
        self.current = Some(config.clone());
        self.migration_failed = false;
        if !durable {
            return Ok(Loaded { config, warning: Some("Settings replaced, but directory durability could not be confirmed. Both password revisions were kept; retry Apply.".into()) });
        }
        let mut warning = None;
        if apply {
            let before = (
                config.credential_vault_cleanup,
                config.credential_cleanup.clone(),
                self.pending.clone(),
            );
            warning = self.cleanup(&mut config);
            if before
                != (
                    config.credential_vault_cleanup,
                    config.credential_cleanup.clone(),
                    self.pending.clone(),
                )
            {
                // Repeating cleanup after a failed optional compaction is safe.
                let _ = self.write_config(&config);
            }
        }
        self.current = Some(config.clone());
        Ok(Loaded { config, warning })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        credentials::MemoryStore,
        model::{DataSource, DriverKind},
    };
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Faults {
        reads: usize,
        writes: usize,
        deletes: usize,
        fail_write: Option<usize>,
        fail_after_write: bool,
        deny_read: bool,
        deny_read_after_write: bool,
        mismatch_after_write: bool,
        mismatch_next_read: bool,
        deny_delete: bool,
    }
    struct Store {
        inner: MemoryStore,
        faults: Arc<Mutex<Faults>>,
    }
    impl CredentialStore for Store {
        fn read(&mut self, s: &str, a: &str) -> Result<Option<Vec<u8>>, String> {
            let mut faults = self.faults.lock().unwrap();
            faults.reads += 1;
            if faults.deny_read {
                return Err("Synthetic access denied".into());
            }
            if std::mem::take(&mut faults.mismatch_next_read) {
                return Ok(Some(b"mismatched-data".to_vec()));
            }
            self.inner.read(s, a)
        }
        fn write(&mut self, s: &str, a: &str, v: &[u8]) -> Result<(), String> {
            let mut faults = self.faults.lock().unwrap();
            faults.writes += 1;
            if faults.fail_write == Some(faults.writes) {
                return Err("Synthetic write denied".into());
            }
            self.inner.write(s, a, v)?;
            if faults.deny_read_after_write {
                faults.deny_read = true;
            }
            if faults.mismatch_after_write {
                faults.mismatch_next_read = true;
            }
            if faults.fail_after_write {
                return Err("Synthetic ambiguous write failure".into());
            }
            Ok(())
        }
        fn delete(&mut self, s: &str, a: &str) -> Result<(), String> {
            let mut faults = self.faults.lock().unwrap();
            faults.deletes += 1;
            if faults.deny_delete {
                return Err("Synthetic delete denied".into());
            }
            self.inner.delete(s, a)
        }
    }
    struct Fixture {
        dir: PathBuf,
        path: PathBuf,
        store: MemoryStore,
        faults: Arc<Mutex<Faults>>,
        namespaces: std::cell::RefCell<Vec<String>>,
    }
    impl Fixture {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("dbeast-credentials-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self {
                path: dir.join("config.json"),
                dir,
                store: MemoryStore::default(),
                faults: Arc::default(),
                namespaces: Default::default(),
            }
        }
        fn manager(&self) -> Persistence {
            Persistence::new(
                self.path.clone(),
                Box::new(Store {
                    inner: self.store.clone(),
                    faults: self.faults.clone(),
                }),
            )
        }
        fn sources(&self, count: usize) -> Config {
            let mut config = Config {
                credential_vault_version: 0,
                ..Config::default()
            };
            self.namespaces
                .borrow_mut()
                .push(config.credential_namespace.clone());
            for i in 0..count {
                let mut source = DataSource::new(DriverKind::Postgres);
                source.id = format!("source-{i}");
                source.password_storage = PasswordStorage::Forever;
                source.password = "synthetic-database-secret".into();
                source.ssh.password = "synthetic-ssh-secret".into();
                source.ssh.passphrase = "synthetic-key-secret".into();
                source.tls_identity_password = "synthetic-tls-secret".into();
                config.data_sources.push(source);
            }
            config
        }
        fn legacy(&self, count: usize) -> Config {
            let config = self.sources(count);
            let mut json = serde_json::to_value(&config).unwrap();
            json.as_object_mut()
                .unwrap()
                .remove("credential_vault_version");
            for source in json["data_sources"].as_array_mut().unwrap() {
                source["password"] = "synthetic-database-secret".into();
                source["ssh"]["password"] = "synthetic-ssh-secret".into();
                source["ssh"]["passphrase"] = "synthetic-key-secret".into();
                source["tls_identity_password"] = "synthetic-tls-secret".into();
            }
            std::fs::write(&self.path, serde_json::to_vec(&json).unwrap()).unwrap();
            config
        }
        fn legacy_items(&self, count: usize) -> Config {
            let mut config = self.sources(count);
            let service = Persistence::service(&config);
            for source in &mut config.data_sources {
                let reference = format!("{}:{}", source.id, uuid::Uuid::new_v4());
                self.store
                    .clone()
                    .write(
                        &service,
                        &reference,
                        &serde_json::to_vec(&Secrets::from_source(source)).unwrap(),
                    )
                    .unwrap();
                source.credential_ref = Some(reference);
            }
            std::fs::write(&self.path, serde_json::to_vec(&config).unwrap()).unwrap();
            config
        }
        fn reset_faults(&self) {
            *self.faults.lock().unwrap() = Faults::default();
        }
        fn counts(&self) -> (usize, usize, usize) {
            let f = self.faults.lock().unwrap();
            (f.reads, f.writes, f.deletes)
        }
        fn vault(&self, config: &Config) -> Vault {
            let bytes = self
                .store
                .clone()
                .read(&Persistence::service(config), VAULT_ACCOUNT)
                .unwrap()
                .unwrap();
            Vault::decode(&bytes, &self.manager().owner().unwrap()).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
            for namespace in self.namespaces.borrow().iter() {
                let _ = std::fs::remove_file(
                    std::env::temp_dir().join(format!("dbeast-vault-{namespace}.lock")),
                );
            }
        }
    }

    #[test]
    fn autosave_reorders_committed_sources_without_changing_them() {
        let f = Fixture::new();
        let mut manager = f.manager();
        manager.load().unwrap();
        let committed = manager.save(f.sources(3), true).unwrap().config;
        let mut draft = committed.clone();
        draft.data_sources.reverse();
        // An autosave cannot smuggle in edited connection settings.
        draft.data_sources[0].host = "edited-by-autosave".into();
        draft.source_group.insert(draft.data_sources[0].id.clone(), "Clients".into());
        let saved = manager.save(draft.clone(), false).unwrap().config;
        let ids = |config: &Config| config.data_sources.iter().map(|s| s.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&saved), ids(&draft));
        assert!(saved.data_sources.iter().all(|s| s.host != "edited-by-autosave"));
        assert_eq!(saved.source_group, draft.source_group);
    }

    #[test]
    fn agent_connections_round_trip_in_keychain_and_reject_copied_configs() {
        let f = Fixture::new();
        let mut manager = f.manager();
        manager.load().unwrap();
        manager.save(f.sources(0), true).unwrap();
        assert_eq!(manager.load_mcp().unwrap(), None);
        let state = crate::mcp::McpState {
            clients: vec![crate::mcp::StoredClient {
                id: uuid::Uuid::new_v4().to_string(),
                label: "Claude Code".into(),
                token_hash: "ab".repeat(32),
            }],
            shares: Vec::new(),
        };
        manager.save_mcp(state.clone()).unwrap();
        let mut restarted = f.manager();
        restarted.load().unwrap();
        assert_eq!(restarted.load_mcp().unwrap(), Some(state));
        assert!(!std::fs::read_to_string(&f.path).unwrap().contains("Claude Code"));
        let copy = f.dir.join("copy.json");
        std::fs::copy(&f.path, &copy).unwrap();
        let mut copied = Persistence::new(
            copy,
            Box::new(Store {
                inner: f.store.clone(),
                faults: f.faults.clone(),
            }),
        );
        copied.load().unwrap();
        assert!(copied.load_mcp().is_err());
        let mut unloaded = f.manager();
        assert!(unloaded.save_mcp(Default::default()).is_err());
    }
    #[test]
    fn migrate_all_secrets_restart_and_redact_debug_and_json() {
        let f = Fixture::new();
        let original = f.legacy(2);
        let loaded = f.manager().load().unwrap();
        assert!(loaded.warning.is_none());
        assert_eq!(loaded.config.credential_vault_version, 1);
        let disk = std::fs::read_to_string(&f.path).unwrap();
        for secret in [
            "synthetic-database-secret",
            "synthetic-ssh-secret",
            "synthetic-key-secret",
            "synthetic-tls-secret",
        ] {
            assert!(!disk.contains(secret));
            assert!(!serde_json::to_string(&original).unwrap().contains(secret));
            assert!(!format!("{original:?}").contains(secret));
        }
        let restarted = f.manager().load().unwrap();
        assert!(
            Secrets::from_source(&restarted.config.data_sources[0])
                == Secrets::from_source(&original.data_sources[0])
        );
        assert_eq!(f.vault(&loaded.config).entries.len(), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&f.path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn one_protected_read_loads_one_hundred_sources_and_autosave_uses_only_memory() {
        let f = Fixture::new();
        f.legacy(100);
        f.manager().load().unwrap();
        f.reset_faults();
        let mut manager = f.manager();
        let mut config = manager.load().unwrap().config;
        assert_eq!(f.counts(), (1, 0, 0));
        assert_eq!(config.data_sources.len(), 100);
        for _ in 0..10 {
            config.max_rows += 1;
            config = manager.save(config, false).unwrap().config;
        }
        manager.save(config, true).unwrap();
        assert_eq!(f.counts(), (1, 0, 0));
    }

    #[test]
    fn existing_per_source_items_migrate_once_then_only_vault_is_read() {
        let f = Fixture::new();
        let old = f.legacy_items(4);
        let saved = f.manager().load().unwrap();
        assert!(saved.warning.is_none());
        assert_eq!(saved.config.credential_vault_version, 1);
        assert_eq!(f.vault(&saved.config).entries.len(), 4);
        for source in old.data_sources {
            assert!(
                f.store
                    .clone()
                    .read(
                        &Persistence::service(&saved.config),
                        source.credential_ref.as_ref().unwrap()
                    )
                    .unwrap()
                    .is_none()
            );
        }
        f.reset_faults();
        assert!(f.manager().load().unwrap().warning.is_none());
        assert_eq!(f.counts(), (1, 0, 0));
    }

    #[test]
    fn legacy_denial_stops_after_first_read_and_keeps_every_item_and_config() {
        let f = Fixture::new();
        f.legacy_items(4);
        let before = std::fs::read(&f.path).unwrap();
        f.faults.lock().unwrap().deny_read = true;
        let mut manager = f.manager();
        let loaded = manager.load().unwrap();
        assert!(loaded.warning.is_some());
        assert!(
            loaded
                .config
                .data_sources
                .iter()
                .all(|s| s.credentials_unavailable)
        );
        assert_eq!(f.counts(), (1, 0, 0));
        assert!(manager.save(loaded.config, false).is_err());
        assert_eq!(std::fs::read(&f.path).unwrap(), before);
    }

    #[test]
    fn legacy_cleanup_denial_never_retries_on_startup_or_autosave() {
        let f = Fixture::new();
        f.legacy_items(4);
        f.faults.lock().unwrap().deny_delete = true;
        let loaded = f.manager().load().unwrap();
        assert!(loaded.warning.is_some());
        assert_eq!(loaded.config.credential_cleanup.len(), 4);
        f.reset_faults();
        f.faults.lock().unwrap().deny_delete = true;
        let mut manager = f.manager();
        let loaded = manager.load().unwrap();
        let saved = manager.save(loaded.config, false).unwrap();
        assert_eq!(f.counts(), (1, 0, 0));
        f.faults.lock().unwrap().deny_delete = false;
        let saved = manager.save(saved.config, true).unwrap();
        assert!(saved.warning.is_none());
        assert!(saved.config.credential_cleanup.is_empty());
        assert_eq!(f.counts(), (2, 0, 4));
    }

    #[test]
    fn failed_migration_preserves_plaintext_original_and_blocks_autosave() {
        let f = Fixture::new();
        f.legacy(2);
        let before = std::fs::read(&f.path).unwrap();
        f.faults.lock().unwrap().fail_write = Some(1);
        let mut manager = f.manager();
        let loaded = manager.load().unwrap();
        assert!(loaded.warning.is_some());
        assert!(manager.save(loaded.config.clone(), false).is_err());
        assert_eq!(std::fs::read(&f.path).unwrap(), before);
        f.reset_faults();
        assert!(manager.save(loaded.config, true).unwrap().warning.is_none());
    }

    #[test]
    fn denied_and_mismatched_verification_keep_old_committed_passwords() {
        for mismatch in [false, true] {
            let f = Fixture::new();
            f.legacy(1);
            let mut manager = f.manager();
            let mut config = manager.load().unwrap().config;
            let old = config.data_sources[0].credential_ref.clone().unwrap();
            let before = std::fs::read(&f.path).unwrap();
            config.data_sources[0].password = "synthetic-replacement".into();
            if mismatch {
                f.faults.lock().unwrap().mismatch_after_write = true;
            } else {
                f.faults.lock().unwrap().deny_read_after_write = true;
            }
            assert!(manager.save(config.clone(), true).is_err());
            assert_eq!(std::fs::read(&f.path).unwrap(), before);
            assert!(f.vault(&config).entries.contains_key(&old));
            f.reset_faults();
            let restarted = f.manager().load().unwrap();
            assert_eq!(
                restarted.config.data_sources[0].password,
                "synthetic-database-secret"
            );
        }
    }

    #[test]
    fn ambiguous_native_write_preserves_old_config_and_rejects_stale_retry() {
        let f = Fixture::new();
        f.legacy(1);
        let mut manager = f.manager();
        let mut config = manager.load().unwrap().config;
        let before = std::fs::read(&f.path).unwrap();
        let old = config.data_sources[0].credential_ref.clone().unwrap();
        config.data_sources[0].password = "synthetic-replacement".into();
        f.faults.lock().unwrap().fail_after_write = true;
        assert!(manager.save(config.clone(), true).is_err());
        assert_eq!(std::fs::read(&f.path).unwrap(), before);
        assert!(f.vault(&config).entries.contains_key(&old));
        f.reset_faults();
        assert!(
            manager
                .save(config, true)
                .err()
                .unwrap()
                .contains("another instance")
        );
        let mut recovered = manager.load().unwrap().config;
        recovered.data_sources[0].password = "synthetic-replacement".into();
        manager.save(recovered, true).unwrap();
        assert_eq!(
            f.manager().load().unwrap().config.data_sources[0].password,
            "synthetic-replacement"
        );
    }

    #[test]
    fn config_failure_keeps_both_revisions_until_explicit_cleanup() {
        let f = Fixture::new();
        f.legacy(1);
        let mut manager = f.manager();
        let mut config = manager.load().unwrap().config;
        let before = std::fs::read(&f.path).unwrap();
        let old = config.data_sources[0].credential_ref.clone().unwrap();
        config.data_sources[0].password = "synthetic-replacement".into();
        manager.fail_config_write = true;
        assert!(manager.save(config.clone(), true).is_err());
        assert_eq!(std::fs::read(&f.path).unwrap(), before);
        let vault = f.vault(&config);
        assert_eq!(vault.entries.len(), 2);
        assert!(vault.entries.contains_key(&old));
        let mut restarted = f.manager();
        let loaded = restarted.load().unwrap();
        assert_eq!(
            loaded.config.data_sources[0].password,
            "synthetic-database-secret"
        );
        assert_eq!(f.vault(&loaded.config).entries.len(), 2);
        let saved = restarted.save(loaded.config, true).unwrap();
        assert_eq!(f.vault(&saved.config).entries.len(), 1);
    }

    #[test]
    fn unconfirmed_directory_sync_keeps_both_revisions_and_recovers_new_config() {
        let f = Fixture::new();
        f.legacy(1);
        let mut manager = f.manager();
        let mut config = manager.load().unwrap().config;
        let old = config.data_sources[0].credential_ref.clone().unwrap();
        config.data_sources[0].password = "synthetic-replacement".into();
        manager.force_nondurable = true;
        let saved = manager.save(config, true).unwrap();
        assert!(saved.warning.is_some());
        let vault = f.vault(&saved.config);
        assert_eq!(vault.entries.len(), 2);
        assert!(vault.entries.contains_key(&old));
        let mut restarted = f.manager();
        let loaded = restarted.load().unwrap();
        assert_eq!(
            loaded.config.data_sources[0].password,
            "synthetic-replacement"
        );
        let saved = restarted.save(loaded.config, true).unwrap();
        assert_eq!(f.vault(&saved.config).entries.len(), 1);
    }

    #[test]
    fn session_only_pruning_failure_is_retryable_without_restoring_saved_secrets() {
        let f = Fixture::new();
        f.legacy(1);
        let mut manager = f.manager();
        let mut config = manager.load().unwrap().config;
        config.data_sources[0].password_storage = PasswordStorage::ForSession;
        let next_write = f.counts().1 + 1;
        f.faults.lock().unwrap().fail_write = Some(next_write);
        let saved = manager.save(config, true).unwrap();
        assert!(saved.warning.is_some());
        assert!(saved.config.credential_vault_cleanup);
        assert!(saved.config.data_sources[0].credential_ref.is_none());
        assert_eq!(
            saved.config.data_sources[0].password,
            "synthetic-database-secret"
        );
        assert!(
            Secrets::from_source(&f.manager().load().unwrap().config.data_sources[0]).is_empty()
        );
        f.reset_faults();
        assert_eq!(
            manager.load().unwrap().config.data_sources[0].password,
            "synthetic-database-secret"
        );
        let saved = manager.save(saved.config, true).unwrap();
        assert!(!saved.config.credential_vault_cleanup);
        assert!(f.vault(&saved.config).entries.is_empty());
    }

    #[test]
    fn rename_duplicate_remove_and_stale_ui_autosave_do_not_alias_credentials() {
        let f = Fixture::new();
        f.legacy(1);
        let mut manager = f.manager();
        let original = manager.load().unwrap().config;
        let mut edited = original.clone();
        edited.data_sources[0].name = "renamed".into();
        let mut duplicate = edited.data_sources[0].clone();
        duplicate.id = "duplicate".into();
        duplicate.credential_ref = None;
        duplicate.password = "synthetic-duplicate".into();
        edited.data_sources.push(duplicate);
        let saved = manager.save(edited, true).unwrap().config;
        assert_eq!(
            saved.data_sources[0].credential_ref,
            original.data_sources[0].credential_ref
        );
        assert_ne!(
            saved.data_sources[0].credential_ref,
            saved.data_sources[1].credential_ref
        );
        let mut stale = original;
        stale.credential_vault_version = 0;
        stale.max_rows = 123;
        let mut saved = manager.save(stale, false).unwrap().config;
        assert_eq!(saved.data_sources.len(), 2);
        assert_eq!(saved.data_sources[0].name, "renamed");
        assert_eq!(saved.credential_vault_version, 1);
        assert_eq!(saved.max_rows, 123);
        let removed_ref = saved.data_sources[0].credential_ref.clone().unwrap();
        saved.data_sources.remove(0);
        let saved = manager.save(saved, true).unwrap().config;
        assert!(!f.vault(&saved).entries.contains_key(&removed_ref));
        assert_eq!(
            f.manager().load().unwrap().config.data_sources[0].password,
            "synthetic-duplicate"
        );
    }

    #[test]
    fn denied_vault_load_blocks_autosave_but_explicit_session_replacement_recovers() {
        let f = Fixture::new();
        f.legacy(4);
        f.manager().load().unwrap();
        let before = std::fs::read(&f.path).unwrap();
        f.reset_faults();
        f.faults.lock().unwrap().deny_read = true;
        let mut manager = f.manager();
        let mut config = manager.load().unwrap().config;
        assert_eq!(f.counts(), (1, 0, 0));
        assert!(
            config
                .data_sources
                .iter()
                .all(|s| s.credentials_unavailable)
        );
        assert!(manager.save(config.clone(), false).is_err());
        assert_eq!(std::fs::read(&f.path).unwrap(), before);
        for source in &mut config.data_sources {
            source.credentials_unavailable = false;
            source.password_storage = PasswordStorage::ForSession;
            source.password = "synthetic-session".into();
        }
        let saved = manager.save(config, true).unwrap();
        assert!(saved.warning.is_some());
        assert!(
            saved
                .config
                .data_sources
                .iter()
                .all(|s| s.credential_ref.is_none())
        );
        assert_eq!(f.counts().1, 0);
    }

    #[test]
    fn corrupt_vault_never_becomes_an_empty_replacement() {
        let f = Fixture::new();
        f.legacy(1);
        let config = f.manager().load().unwrap().config;
        let service = Persistence::service(&config);
        f.store
            .clone()
            .write(&service, VAULT_ACCOUNT, b"corrupt-synthetic-vault")
            .unwrap();
        f.reset_faults();
        let mut manager = f.manager();
        let mut loaded = manager.load().unwrap().config;
        loaded.data_sources[0].credentials_unavailable = false;
        loaded.data_sources[0].password = "synthetic-replacement".into();
        assert!(manager.save(loaded, true).is_err());
        assert_eq!(f.counts().1, 0);
        assert_eq!(
            f.store
                .clone()
                .read(&service, VAULT_ACCOUNT)
                .unwrap()
                .unwrap(),
            b"corrupt-synthetic-vault"
        );
    }

    #[test]
    fn stale_process_rejects_changed_config_before_touching_keychain() {
        let f = Fixture::new();
        f.legacy(1);
        let mut first = f.manager();
        let stale = first.load().unwrap().config;
        let mut second = f.manager();
        let mut latest = second.load().unwrap().config;
        latest.data_sources[0].password = "synthetic-newest".into();
        second.save(latest, true).unwrap();
        f.reset_faults();
        assert!(
            first
                .save(stale, true)
                .err()
                .unwrap()
                .contains("another instance")
        );
        assert_eq!(f.counts(), (0, 0, 0));
        assert_eq!(
            f.manager().load().unwrap().config.data_sources[0].password,
            "synthetic-newest"
        );
    }

    #[test]
    fn stale_vault_cache_rejects_external_update_before_overwrite() {
        let f = Fixture::new();
        f.legacy(1);
        let mut manager = f.manager();
        let mut config = manager.load().unwrap().config;
        let mut changed = f.vault(&config);
        changed.entries.insert(
            format!("other:{}", uuid::Uuid::new_v4()),
            Secrets::from_source(&config.data_sources[0]),
        );
        let bytes = serde_json::to_vec(&changed).unwrap();
        f.store
            .clone()
            .write(&Persistence::service(&config), VAULT_ACCOUNT, &bytes)
            .unwrap();
        config.data_sources[0].password = "synthetic-replacement".into();
        f.reset_faults();
        assert!(
            manager
                .save(config.clone(), true)
                .err()
                .unwrap()
                .contains("another instance")
        );
        assert_eq!(f.counts().1, 0);
        assert_eq!(
            f.store
                .clone()
                .read(&Persistence::service(&config), VAULT_ACCOUNT)
                .unwrap()
                .unwrap(),
            bytes
        );
    }

    #[test]
    fn copied_config_cannot_use_or_prune_another_locations_vault() {
        let f = Fixture::new();
        f.legacy(1);
        let config = f.manager().load().unwrap().config;
        let copied = f.dir.join("copied.json");
        std::fs::copy(&f.path, &copied).unwrap();
        let mut manager = Persistence::new(copied, Box::new(f.store.clone()));
        let loaded = manager.load().unwrap();
        assert!(
            loaded
                .warning
                .unwrap()
                .contains("another configuration location")
        );
        assert!(loaded.config.data_sources[0].credentials_unavailable);
        assert!(manager.save(loaded.config, false).is_err());
        assert_eq!(f.vault(&config).entries.len(), 1);
    }

    #[test]
    fn namespace_lock_excludes_other_config_paths() {
        let f = Fixture::new();
        let config = f.legacy(1);
        let _lock = Persistence::namespace_lock(&config.credential_namespace).unwrap();
        let mut manager = f.manager();
        assert!(
            manager
                .load()
                .err()
                .unwrap()
                .contains("Another DBeast instance")
        );
        assert_eq!(f.counts(), (0, 0, 0));
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_or_symlink_lock_files_are_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let f = Fixture::new();
        let unsafe_path = f.dir.join("unsafe.lock");
        std::fs::write(&unsafe_path, b"").unwrap();
        std::fs::set_permissions(&unsafe_path, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(lock_file(&unsafe_path).is_err());
        let link = f.dir.join("link.lock");
        symlink(&unsafe_path, &link).unwrap();
        assert!(lock_file(&link).is_err());
    }

    #[test]
    fn invalid_config_and_duplicate_ids_never_write_or_access_store() {
        let f = Fixture::new();
        std::fs::write(&f.path, b"invalid JSON").unwrap();
        let mut manager = f.manager();
        assert!(manager.load().is_err());
        assert!(manager.save(Config::default(), false).is_err());
        assert_eq!(std::fs::read(&f.path).unwrap(), b"invalid JSON");
        let mut config = f.legacy(2);
        config.data_sources[1].id = config.data_sources[0].id.clone();
        std::fs::write(&f.path, serde_json::to_vec(&config).unwrap()).unwrap();
        assert!(manager.load().is_err());
        assert_eq!(f.counts(), (0, 0, 0));
    }

    #[test]
    fn old_pending_journal_retries_only_after_explicit_apply() {
        let f = Fixture::new();
        f.legacy(1);
        let mut manager = f.manager();
        let config = manager.load().unwrap().config;
        let reference = format!("orphan:{}", uuid::Uuid::new_v4());
        let service = Persistence::service(&config);
        f.store
            .clone()
            .write(&service, &reference, b"synthetic-orphan")
            .unwrap();
        let journal = serde_json::json!({"namespace": config.credential_namespace, "references": [reference]});
        std::fs::write(
            manager.journal_path(),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        f.reset_faults();
        let loaded = manager.load().unwrap();
        assert!(loaded.warning.is_some());
        assert_eq!(f.counts(), (1, 0, 0));
        manager.save(loaded.config, true).unwrap();
        assert!(
            f.store
                .clone()
                .read(&service, &reference)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn session_password_survives_failed_reload_and_retry_while_saves_are_blocked() {
        let f = Fixture::new();
        f.legacy(1);
        let mut manager = f.manager();
        let mut config = manager.load().unwrap().config;
        config.data_sources[0].password_storage = PasswordStorage::ForSession;
        let config = manager.save(config, true).unwrap().config;
        let valid = std::fs::read(&f.path).unwrap();
        std::fs::write(&f.path, b"broken JSON").unwrap();
        assert!(manager.load().is_err());
        assert!(manager.save(config, false).is_err());
        std::fs::write(&f.path, valid).unwrap();
        assert_eq!(
            manager.load().unwrap().config.data_sources[0].password,
            "synthetic-database-secret"
        );
    }

    #[test]
    fn replacing_config_accepts_empty_journal_but_protects_pending_legacy_items() {
        let f = Fixture::new();
        f.legacy(1);
        let mut manager = f.manager();
        let old = manager.load().unwrap().config;
        let replacement = f.legacy(1);
        assert_ne!(old.credential_namespace, replacement.credential_namespace);
        assert!(f.manager().load().unwrap().warning.is_none());
        let journal = serde_json::json!({"namespace": old.credential_namespace, "references": [format!("old-source:{}", uuid::Uuid::new_v4())]});
        std::fs::write(
            manager.journal_path(),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        let before = std::fs::read(&f.path).unwrap();
        assert!(manager.load().is_err());
        assert!(manager.save(replacement, false).is_err());
        assert_eq!(std::fs::read(&f.path).unwrap(), before);
    }

    #[test]
    fn separate_config_files_have_distinct_namespaces_and_journals() {
        let first = Fixture::new();
        first.legacy(1);
        let a = first.manager().load().unwrap().config;
        let second = Fixture::new();
        second.legacy(1);
        let mut manager = Persistence::new(second.path.clone(), Box::new(first.store.clone()));
        let b = manager.load().unwrap().config;
        assert_ne!(Persistence::service(&a), Persistence::service(&b));
        assert_ne!(first.manager().journal_path(), manager.journal_path());
    }

    #[test]
    fn missing_namespace_survives_ambiguous_vault_write_and_config_failure() {
        for ambiguous_write in [false, true] {
            let f = Fixture::new();
            f.legacy(2);
            let mut json: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&f.path).unwrap()).unwrap();
            json.as_object_mut().unwrap().remove("credential_namespace");
            std::fs::write(&f.path, serde_json::to_vec(&json).unwrap()).unwrap();
            let before = std::fs::read(&f.path).unwrap();
            let mut manager = f.manager();
            manager.fail_config_write = !ambiguous_write;
            f.faults.lock().unwrap().fail_after_write = ambiguous_write;
            let loaded = manager.load().unwrap();
            assert!(loaded.warning.is_some());
            assert_eq!(std::fs::read(&f.path).unwrap(), before);
            let namespace = loaded.config.credential_namespace.clone();
            f.namespaces.borrow_mut().push(namespace.clone());
            let journal: serde_json::Value =
                serde_json::from_slice(&std::fs::read(manager.journal_path()).unwrap()).unwrap();
            assert_eq!(journal["namespace"], namespace);
            assert!(journal["references"].as_array().unwrap().is_empty());
            assert_eq!(f.vault(&loaded.config).entries.len(), 2);
            f.reset_faults();
            let restarted = f.manager().load().unwrap();
            assert_eq!(restarted.config.credential_namespace, namespace);
            assert_eq!(f.vault(&restarted.config).entries.len(), 2);
            assert!(restarted.warning.is_none());
        }
    }

    #[test]
    fn failed_first_config_save_recovers_namespace_and_can_remove_orphan_revisions() {
        let f = Fixture::new();
        let mut manager = f.manager();
        let mut config = manager.load().unwrap().config;
        let namespace = config.credential_namespace.clone();
        f.namespaces.borrow_mut().push(namespace.clone());
        config.data_sources = f.sources(1).data_sources;
        manager.fail_config_write = true;
        assert!(manager.save(config.clone(), true).is_err());
        assert!(!f.path.exists());
        assert_eq!(f.vault(&config).entries.len(), 1);
        let mut restarted = f.manager();
        let loaded = restarted.load().unwrap();
        assert_eq!(loaded.config.credential_namespace, namespace);
        assert!(loaded.config.data_sources.is_empty());
        assert!(loaded.config.credential_vault_cleanup);
        let saved = restarted.save(loaded.config, true).unwrap();
        assert!(saved.warning.is_none());
        assert!(f.vault(&saved.config).entries.is_empty());
    }

    #[test]
    fn copied_config_session_replacement_cannot_delete_original_legacy_items() {
        let f = Fixture::new();
        let original = f.legacy_items(2);
        f.faults.lock().unwrap().deny_delete = true;
        let migrated = f.manager().load().unwrap().config;
        let copied = f.dir.join("copied-with-cleanup.json");
        std::fs::copy(&f.path, &copied).unwrap();
        f.reset_faults();
        let mut manager = Persistence::new(
            copied,
            Box::new(Store {
                inner: f.store.clone(),
                faults: f.faults.clone(),
            }),
        );
        let mut loaded = manager.load().unwrap().config;
        for source in &mut loaded.data_sources {
            source.credentials_unavailable = false;
            source.password_storage = PasswordStorage::ForSession;
        }
        let saved = manager.save(loaded, true).unwrap();
        assert!(
            saved
                .warning
                .unwrap()
                .contains("another configuration location")
        );
        assert_eq!(f.counts().2, 0);
        for source in &original.data_sources {
            assert!(
                f.store
                    .clone()
                    .read(
                        &Persistence::service(&original),
                        source.credential_ref.as_ref().unwrap()
                    )
                    .unwrap()
                    .is_some()
            );
        }
        assert_eq!(f.vault(&migrated).entries.len(), 2);
    }

    #[test]
    fn denied_or_corrupt_vault_blocks_all_legacy_cleanup() {
        for denied in [false, true] {
            let f = Fixture::new();
            f.legacy_items(1);
            f.faults.lock().unwrap().deny_delete = true;
            let migrated = f.manager().load().unwrap().config;
            f.reset_faults();
            if denied {
                f.faults.lock().unwrap().deny_read = true;
            } else {
                f.store
                    .clone()
                    .write(&Persistence::service(&migrated), VAULT_ACCOUNT, b"corrupt")
                    .unwrap();
            }
            let mut manager = f.manager();
            let mut config = manager.load().unwrap().config;
            for source in &mut config.data_sources {
                source.credentials_unavailable = false;
                source.password_storage = PasswordStorage::ForSession;
            }
            assert!(manager.save(config, true).unwrap().warning.is_some());
            assert_eq!(f.counts().2, 0);
        }
    }

    #[test]
    #[ignore = "release persistence benchmark with synthetic credentials only"]
    fn benchmark_config_and_credential_persistence() {
        let f = Fixture::new();
        f.legacy(100);
        let start = std::time::Instant::now();
        f.manager().load().unwrap();
        eprintln!(
            "100-source migration (single memory vault + atomic disk write): {:?}",
            start.elapsed()
        );
        f.reset_faults();
        let mut manager = f.manager();
        let start = std::time::Instant::now();
        let config = manager.load().unwrap().config;
        eprintln!(
            "100-source restart: {:?}; protected store reads: {}",
            start.elapsed(),
            f.counts().0
        );
        let start = std::time::Instant::now();
        for _ in 0..100 {
            manager.save(config.clone(), false).unwrap();
        }
        eprintln!(
            "100-source autosave including clone, JSON, lock, fsync, rename: {:?}/save; store calls after restart: {:?}",
            start.elapsed() / 100,
            f.counts()
        );
    }
}
