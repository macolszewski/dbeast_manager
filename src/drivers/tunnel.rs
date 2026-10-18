//! SSH tunnels: a local port forwarded through an SSH server to the database,
//! ("Use SSH tunnel"). All sessions of a data source share one
//! tunnel, which closes when the last of them is dropped.

use crate::model::{DataSource, SshAuth, SshConfig};
use anyhow::{Context, Result, bail};
use russh::client;
use russh::keys::{self, PrivateKeyWithHashAlg};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Weak};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

pub struct Tunnel {
    pub local_port: u16,
    session: Arc<client::Handle<HostKeyCheck>>,
    forwarder: JoinHandle<()>,
}

impl Tunnel {
    fn is_alive(&self) -> bool {
        !self.session.is_closed() && !self.forwarder.is_finished()
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.forwarder.abort();
    }
}

/// Live shared values keyed by a string. Each key has its own slot lock, so a
/// slow initializer only delays callers of the same key; the map lock is held
/// only to find or create the slot, never across an await.
struct Registry<T> {
    slots: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<Weak<T>>>>>,
}

impl<T> Default for Registry<T> {
    fn default() -> Self {
        Self {
            slots: Default::default(),
        }
    }
}

impl<T> Registry<T> {
    /// Returns the live value for `key`, or runs `init` once for all concurrent
    /// callers of that key. A failed or cancelled `init` leaves the slot empty,
    /// so the next waiter tries again instead of hanging.
    async fn get_or_open<F>(&self, key: String, alive: fn(&T) -> bool, init: F) -> Result<Arc<T>>
    where
        F: Future<Output = Result<T>>,
    {
        let slot = {
            let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
            // Drop slots nobody waits on and whose value is gone.
            slots.retain(|_, s| {
                Arc::strong_count(s) > 1 || s.try_lock().map_or(true, |w| w.strong_count() > 0)
            });
            slots.entry(key).or_default().clone()
        };
        let mut current = slot.lock().await;
        if let Some(value) = current.upgrade()
            && alive(&value)
        {
            return Ok(value);
        }
        let value = Arc::new(init.await?);
        *current = Arc::downgrade(&value);
        Ok(value)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.slots.lock().unwrap().len()
    }
}

/// Open tunnels keyed by SSH settings and target, so sessions can share them.
static TUNNELS: LazyLock<Registry<Tunnel>> = LazyLock::new(Default::default);

/// Returns a tunnel to the data source's host and port, reusing an open one.
pub async fn open(ds: &DataSource) -> Result<Arc<Tunnel>> {
    TUNNELS
        .get_or_open(
            tunnel_key(ds)?,
            Tunnel::is_alive,
            connect(&ds.ssh, &ds.host, ds.port),
        )
        .await
}

async fn connect(ssh: &SshConfig, db_host: &str, db_port: u16) -> Result<Tunnel> {
    if ssh.host.trim().is_empty() || ssh.user.trim().is_empty() {
        bail!("SSH tunnel needs a host and a user name");
    }
    let config = Arc::new(client::Config {
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(30)),
        nodelay: true,
        ..Default::default()
    });
    let check = HostKeyCheck {
        host: ssh.host.clone(),
        port: ssh.port,
        known_hosts: known_hosts_path(),
    };
    let target = (ssh.host.as_str(), ssh.port);
    let mut session = tokio::time::timeout(
        Duration::from_secs(15),
        client::connect(config, target, check),
    )
    .await
    .with_context(|| format!("SSH connection to {}:{} timed out", ssh.host, ssh.port))?
    .map_err(|e| anyhow::anyhow!("SSH connection to {}:{}: {e}", ssh.host, ssh.port))?;
    authenticate(&mut session, ssh).await?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let local_port = listener.local_addr()?.port();
    let session = Arc::new(session);
    let forward = session.clone();
    let db_host = db_host.to_string();
    let forwarder = tokio::spawn(async move {
        while let Ok((mut socket, peer)) = listener.accept().await {
            let session = forward.clone();
            let host = db_host.clone();
            tokio::spawn(async move {
                let channel = session
                    .channel_open_direct_tcpip(
                        host,
                        db_port.into(),
                        peer.ip().to_string(),
                        peer.port().into(),
                    )
                    .await;
                // On failure the socket is dropped and the driver reports a closed connection.
                if let Ok(channel) = channel {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
                }
            });
        }
    });
    Ok(Tunnel {
        local_port,
        session,
        forwarder,
    })
}

#[cfg(unix)]
async fn agent_authenticate(session: &mut client::Handle<HostKeyCheck>, user: &str) -> Result<bool> {
    let agent = keys::agent::client::AgentClient::connect_env()
        .await
        .context("cannot reach ssh-agent (is SSH_AUTH_SOCK set?)")?;
    try_agent_keys(session, user, agent).await
}

/// The Windows OpenSSH agent listens on a named pipe (or `SSH_AUTH_SOCK` when
/// set to one); Pageant is the other common agent there.
#[cfg(windows)]
async fn agent_authenticate(session: &mut client::Handle<HostKeyCheck>, user: &str) -> Result<bool> {
    use keys::agent::client::AgentClient;
    let pipe = std::env::var_os("SSH_AUTH_SOCK")
        .unwrap_or_else(|| r"\\.\pipe\openssh-ssh-agent".into());
    match AgentClient::connect_named_pipe(&pipe).await {
        Ok(agent) => try_agent_keys(session, user, agent).await,
        Err(_) => {
            let agent = AgentClient::connect_pageant()
                .await
                .context("cannot reach ssh-agent (start the OpenSSH Authentication Agent service or Pageant)")?;
            try_agent_keys(session, user, agent).await
        }
    }
}

async fn try_agent_keys<S>(
    session: &mut client::Handle<HostKeyCheck>,
    user: &str,
    mut agent: keys::agent::client::AgentClient<S>,
) -> Result<bool>
where
    S: keys::agent::client::AgentStream + Unpin + Send + 'static,
{
    let identities = agent.request_identities().await?;
    let hash = session.best_supported_rsa_hash().await?.flatten();
    for identity in identities {
        if let keys::agent::AgentIdentity::PublicKey { key, .. } = identity
            && session
                .authenticate_publickey_with(user, key, hash, &mut agent)
                .await?
                .success()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn authenticate(session: &mut client::Handle<HostKeyCheck>, ssh: &SshConfig) -> Result<()> {
    let who = format!("{}@{}", ssh.user, ssh.host);
    let ok = match ssh.auth {
        SshAuth::Password => session
            .authenticate_password(&ssh.user, &ssh.password)
            .await?
            .success(),
        SshAuth::KeyPair => {
            let path = expand_home(&ssh.key_file);
            let passphrase = (!ssh.passphrase.is_empty()).then_some(ssh.passphrase.as_str());
            let key = keys::load_secret_key(&path, passphrase)
                .with_context(|| format!("cannot load private key {}", path.display()))?;
            let hash = session.best_supported_rsa_hash().await?.flatten();
            session
                .authenticate_publickey(&ssh.user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await?
                .success()
        }
        SshAuth::Agent => agent_authenticate(session, &ssh.user).await?,
    };
    if !ok {
        bail!("SSH authentication failed for {who} ({})", ssh.auth.label());
    }
    Ok(())
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => directories::BaseDirs::new()
            .map_or_else(|| PathBuf::from(path), |d| d.home_dir().join(rest)),
        None => PathBuf::from(path),
    }
}

/// Host keys are kept in the application's own known_hosts file: the first
/// key seen for a host is trusted and remembered, a different one later is
/// rejected, as a changed key may mean someone is intercepting the connection.
fn known_hosts_path() -> PathBuf {
    crate::config::Config::data_dir().join("known_hosts")
}

pub struct HostKeyCheck {
    host: String,
    port: u16,
    known_hosts: PathBuf,
}

impl client::Handler for HostKeyCheck {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        server: &keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let keys::PublicKeyOrCertificate::PublicKey { key, .. } = server else {
            bail!("SSH host certificates are not supported");
        };
        match keys::check_known_hosts_path(&self.host, self.port, key, &self.known_hosts) {
            Ok(true) => Ok(true),
            Ok(false) => {
                keys::known_hosts::learn_known_hosts_path(
                    &self.host,
                    self.port,
                    key,
                    &self.known_hosts,
                )?;
                Ok(true)
            }
            Err(keys::Error::KeyChanged { line }) => bail!(
                "SSH host key of {}:{} has changed (line {line} of {}); remove that line if the change is expected",
                self.host,
                self.port,
                self.known_hosts.display()
            ),
            Err(e) => Err(e.into()),
        }
    }
}

// In-memory only: changing authentication must never reuse a tunnel authenticated
// with different secrets. This key is never serialized to disk or included in Debug.
fn tunnel_key(ds: &DataSource) -> Result<String> {
    Ok(serde_json::to_string(&(
        &ds.ssh,
        &ds.ssh.password,
        &ds.ssh.passphrase,
        &ds.host,
        ds.port,
    ))?)
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Dummy(bool);

    fn alive(d: &Dummy) -> bool {
        d.0
    }

    #[tokio::test]
    async fn slow_key_does_not_block_other_keys() {
        let registry = Registry::<Dummy>::default();
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        let slow = registry.get_or_open("a".into(), alive, async {
            let _ = wait.await;
            bail!("unreachable")
        });
        let fast = async {
            let b = tokio::time::timeout(
                Duration::from_secs(1),
                registry.get_or_open("b".into(), alive, async { Ok(Dummy(true)) }),
            )
            .await
            .expect("B must not wait for A")
            .unwrap();
            // The live B is reused without running its initializer again.
            let again = tokio::time::timeout(
                Duration::from_secs(1),
                registry.get_or_open("b".into(), alive, async { bail!("reconnected") }),
            )
            .await
            .expect("reuse must not wait for A")
            .unwrap();
            assert!(Arc::ptr_eq(&b, &again));
            let _ = release.send(());
        };
        let (a, ()) = tokio::join!(slow, fast);
        assert!(a.is_err());
    }

    #[tokio::test]
    async fn concurrent_callers_of_one_key_share_a_single_open() {
        let registry = Registry::<Dummy>::default();
        let opens = AtomicUsize::new(0);
        let init = || async {
            opens.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(Dummy(true))
        };
        let (x, y) = tokio::join!(
            registry.get_or_open("a".into(), alive, init()),
            registry.get_or_open("a".into(), alive, init()),
        );
        assert!(Arc::ptr_eq(&x.unwrap(), &y.unwrap()));
        assert_eq!(opens.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_or_cancelled_open_lets_the_next_caller_retry() {
        let registry = Registry::<Dummy>::default();
        let failed = registry
            .get_or_open("a".into(), alive, async { bail!("down") })
            .await;
        assert!(failed.is_err());
        // Cancel an initializer while it holds the slot.
        let pending = registry.get_or_open("a".into(), alive, std::future::pending());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), pending)
                .await
                .is_err()
        );
        let ok = tokio::time::timeout(
            Duration::from_secs(1),
            registry.get_or_open("a".into(), alive, async { Ok(Dummy(true)) }),
        )
        .await
        .expect("cancelled open must release the slot");
        assert!(ok.is_ok());
    }

    #[tokio::test]
    async fn dead_values_are_reopened_and_unused_slots_dropped() {
        let registry = Registry::<Dummy>::default();
        let dead = registry
            .get_or_open("a".into(), alive, async { Ok(Dummy(false)) })
            .await
            .unwrap();
        let fresh = registry
            .get_or_open("a".into(), alive, async { Ok(Dummy(true)) })
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&dead, &fresh));
        drop((dead, fresh));
        registry
            .get_or_open("b".into(), alive, async { Ok(Dummy(true)) })
            .await
            .unwrap();
        assert_eq!(registry.len(), 1, "slot of the dropped tunnel is removed");
    }
}

#[cfg(test)]
mod credential_tests {
    use super::*;
    #[test]
    fn changed_password_or_passphrase_changes_tunnel_identity() {
        let mut source = DataSource::new(crate::model::DriverKind::Postgres);
        let original = tunnel_key(&source).unwrap();
        source.ssh.password = "synthetic-password".into();
        assert_ne!(tunnel_key(&source).unwrap(), original);
        source.ssh.password.clear();
        source.ssh.passphrase = "synthetic-passphrase".into();
        assert_ne!(tunnel_key(&source).unwrap(), original);
    }
}
