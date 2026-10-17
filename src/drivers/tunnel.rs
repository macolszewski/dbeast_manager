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

/// Open tunnels keyed by SSH settings and target, so sessions can share them.
static TUNNELS: LazyLock<tokio::sync::Mutex<HashMap<String, Weak<Tunnel>>>> =
    LazyLock::new(Default::default);

/// Returns a tunnel to the data source's host and port, reusing an open one.
pub async fn open(ds: &DataSource) -> Result<Arc<Tunnel>> {
    let key = tunnel_key(ds)?;
    let mut tunnels = TUNNELS.lock().await;
    if let Some(t) = tunnels.get(&key).and_then(Weak::upgrade)
        && t.is_alive()
    {
        return Ok(t);
    }
    let tunnel = Arc::new(connect(&ds.ssh, &ds.host, ds.port).await?);
    tunnels.retain(|_, t| t.strong_count() > 0);
    tunnels.insert(key, Arc::downgrade(&tunnel));
    Ok(tunnel)
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
        SshAuth::Agent => {
            let mut agent = keys::agent::client::AgentClient::connect_env()
                .await
                .context("cannot reach ssh-agent (is SSH_AUTH_SOCK set?)")?;
            let identities = agent.request_identities().await?;
            let hash = session.best_supported_rsa_hash().await?.flatten();
            let mut ok = false;
            for identity in identities {
                if let keys::agent::AgentIdentity::PublicKey { key, .. } = identity
                    && session
                        .authenticate_publickey_with(&ssh.user, key, hash, &mut agent)
                        .await?
                        .success()
                {
                    ok = true;
                    break;
                }
            }
            ok
        }
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
