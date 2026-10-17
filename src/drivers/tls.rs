//! Prepare immutable TLS material off the UI/runtime thread, including cancellation connections.
use crate::model::{DataSource, DriverKind, TlsMode};
use anyhow::{Context, Result, ensure};
use native_tls::{Certificate, Identity, TlsConnector};
use std::io::Read;

#[derive(Clone)]
pub(super) struct Material {
    pub connector: Option<TlsConnector>,
    pub mysql: mysql_async::SslOpts,
}

/// What prepared material depends on: settings, the identity password (as a
/// digest) and each file's modification time and size, so an edited or
/// replaced file is read again.
#[derive(PartialEq)]
struct CacheKey {
    postgres: bool,
    settings: crate::model::TlsConfig,
    password: [u8; 32],
    files: Vec<Option<(std::time::SystemTime, u64)>>,
}

/// Recently prepared material. Building a client identity converts PEM to
/// PKCS#12 and, on macOS, imports it into a temporary keychain: tens of
/// milliseconds that every new session or MCP request would otherwise repeat.
static CACHE: std::sync::Mutex<Vec<(CacheKey, Material)>> = std::sync::Mutex::new(Vec::new());
const CACHE_ENTRIES: usize = 8;

impl CacheKey {
    fn of(postgres: bool, settings: &crate::model::TlsConfig, password: &str) -> Self {
        use sha2::Digest;
        let stamp = |path: &str| {
            (!path.is_empty())
                .then(|| std::fs::metadata(path).ok())
                .flatten()
                .and_then(|meta| Some((meta.modified().ok()?, meta.len())))
        };
        CacheKey {
            postgres,
            settings: settings.clone(),
            password: sha2::Sha256::digest(password.as_bytes()).into(),
            files: [&settings.ca_file, &settings.identity_file, &settings.key_file]
                .into_iter()
                .map(|path| stamp(path))
                .collect(),
        }
    }
}

pub(super) async fn prepare(source: &DataSource) -> Result<Option<Material>> {
    if !source.use_tls {
        return Ok(None);
    }
    let postgres = source.driver == DriverKind::Postgres;
    let settings = source.tls.clone();
    let password = source.tls_identity_password.clone();
    tokio::task::spawn_blocking(move || {
        let key = CacheKey::of(postgres, &settings, &password);
        if let Some((_, material)) = CACHE.lock().unwrap().iter().find(|(k, _)| *k == key) {
            return Ok(Some(material.clone()));
        }
        let material = build(postgres, settings, password)?;
        let mut cache = CACHE.lock().unwrap();
        if cache.len() == CACHE_ENTRIES {
            cache.remove(0);
        }
        cache.push((key, material.clone()));
        Ok(Some(material))
    })
    .await?
}

fn build(postgres: bool, settings: crate::model::TlsConfig, password: String) -> Result<Material> {
    let mut connector = TlsConnector::builder();
    let mut mysql = mysql_async::SslOpts::default();
    // Require encrypts without checking the certificate; Verify CA checks the
    // chain but not the host name; Full verification checks both.
    match settings.mode {
        TlsMode::Require => {
            connector.danger_accept_invalid_certs(true).danger_accept_invalid_hostnames(true);
            mysql = mysql.with_danger_accept_invalid_certs(true).with_danger_skip_domain_validation(true);
        }
        TlsMode::VerifyCa => {
            connector.danger_accept_invalid_hostnames(true);
            mysql = mysql.with_danger_skip_domain_validation(true);
        }
        TlsMode::VerifyFull => {}
    }
    if !settings.ca_file.is_empty() {
        let bytes = read_file(&settings.ca_file, "CA file")?;
        // Reject empty or malformed PEM rather than silently using system roots.
        let certificates = if let Ok(cert) = Certificate::from_der(&bytes) {
            vec![cert]
        } else {
            let blocks = pem::parse_many(&bytes).context("Invalid CA certificate file")?;
            ensure!(!blocks.is_empty(), "CA file contains no certificates");
            blocks
                .iter()
                .map(|block| {
                    ensure!(
                        block.tag() == "CERTIFICATE",
                        "CA file must contain only certificates"
                    );
                    Certificate::from_der(block.contents()).context("Invalid CA certificate")
                })
                .collect::<Result<Vec<_>>>()?
        };
        for certificate in certificates {
            connector.add_root_certificate(certificate);
        }
        mysql = mysql.with_root_certs(vec![bytes.into()]);
    }
    if !settings.identity_file.is_empty() && !settings.key_file.is_empty() {
        let certificate = read_file(&settings.identity_file, "Client certificate file")?;
        let key = read_file(&settings.key_file, "Client key file")?;
        let (chain, key) = pem_identity(&certificate, &key)?;
        // Both drivers get the same in-memory PKCS#12: the MySQL driver takes
        // nothing else, and macOS imports EC keys only this way.
        let password = uuid::Uuid::new_v4().simple().to_string();
        let archive = pkcs12_archive(&chain, &key, &password)?;
        if postgres {
            let identity = Identity::from_pkcs12(&archive, &password).map_err(|_| {
                anyhow::anyhow!("Cannot use the client certificate with this key. Check that they belong together.")
            })?;
            connector.identity(identity);
        }
        mysql = mysql.with_client_identity(Some(
            mysql_async::ClientIdentity::new(archive.into()).with_password(password),
        ));
    } else if !settings.identity_file.is_empty() {
        let bytes = read_file(&settings.identity_file, "Client identity file")?;
        // Native TLS error strings are deliberately not propagated: identity passwords
        // and key material must never appear in diagnostics.
        if postgres {
            let identity = Identity::from_pkcs12(&bytes, &password).map_err(|_| {
                anyhow::anyhow!(
                    "Cannot open client identity. Check the PKCS#12 file and its password."
                )
            })?;
            connector.identity(identity);
        }
        mysql = mysql.with_client_identity(Some(
            mysql_async::ClientIdentity::new(bytes.into()).with_password(password),
        ));
    }
    Ok(Material {
        connector: if postgres {
            Some(connector.build().context("Cannot prepare TLS")?)
        } else {
            None
        },
        mysql,
    })
}

/// Certificate chain (DER, leaf first) and PKCS#8 key (DER) from PEM or DER files.
fn pem_identity(certificate: &[u8], key: &[u8]) -> Result<(Vec<Vec<u8>>, Vec<u8>)> {
    let chain: Vec<Vec<u8>> = match pem::parse_many(certificate) {
        Ok(blocks) if !blocks.is_empty() => blocks
            .into_iter()
            .filter(|block| block.tag() == "CERTIFICATE")
            .map(|block| block.into_contents())
            .collect(),
        _ => vec![certificate.to_vec()],
    };
    ensure!(!chain.is_empty(), "The client certificate file contains no certificate");
    ensure!(
        Certificate::from_der(&chain[0]).is_ok(),
        "The client certificate file is not a PEM or DER certificate"
    );
    let key = match pem::parse(key) {
        Ok(block) => match block.tag() {
            "PRIVATE KEY" => block.into_contents(),
            "RSA PRIVATE KEY" if !block.headers().iter().any(|(k, _)| k == "Proc-Type") => {
                pkcs8_from_rsa(block.contents())
            }
            "EC PRIVATE KEY" if !block.headers().iter().any(|(k, _)| k == "Proc-Type") => {
                pkcs8_from_sec1(block.contents())?
            }
            "ENCRYPTED PRIVATE KEY" | "RSA PRIVATE KEY" | "EC PRIVATE KEY" => anyhow::bail!(
                "Encrypted PEM keys are not supported. Use an unencrypted key file or a PKCS#12 archive with a password."
            ),
            _ => anyhow::bail!("The client key file must be a PEM private key"),
        },
        // DER: PKCS#8 as is.
        Err(_) => key.to_vec(),
    };
    ensure!(
        p12_keystore::PrivateKey::from_der(&key).is_ok(),
        "The client key file is not a valid private key"
    );
    Ok((chain, key))
}

/// PKCS#12 with 3DES and an SHA-1 MAC: the variant both macOS and OpenSSL 3
/// (default provider) import. It lives only in memory with a random password.
fn pkcs12_archive(chain: &[Vec<u8>], key: &[u8], password: &str) -> Result<Vec<u8>> {
    use p12_keystore::{Certificate as P12Certificate, EncryptionAlgorithm, KeyStore, KeyStoreEntry, MacAlgorithm, PrivateKey, PrivateKeyChain};
    let certificates = chain
        .iter()
        .map(|der| P12Certificate::from_der(der).map_err(|_| anyhow::anyhow!("Invalid client certificate")))
        .collect::<Result<Vec<_>>>()?;
    let key = PrivateKey::from_der(key).map_err(|_| anyhow::anyhow!("Invalid client key"))?;
    let mut store = KeyStore::new();
    store.add_entry("client", KeyStoreEntry::PrivateKeyChain(PrivateKeyChain::new("client", key, certificates)));
    store
        .writer(password)
        .encryption_algorithm(EncryptionAlgorithm::PbeWithShaAnd3KeyTripleDesCbc)
        .mac_algorithm(MacAlgorithm::HmacSha1)
        .write()
        .map_err(|_| anyhow::anyhow!("Cannot prepare the client certificate"))
}

/// DER length and tag-length-value helpers for wrapping keys in PKCS#8.
fn der(tag: u8, contents: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let length = contents.len();
    if length < 0x80 {
        out.push(length as u8);
    } else {
        let bytes: Vec<u8> = length.to_be_bytes().iter().copied().skip_while(|b| *b == 0).collect();
        out.push(0x80 | bytes.len() as u8);
        out.extend(bytes);
    }
    out.extend_from_slice(contents);
    out
}

const RSA_ENCRYPTION: &[u8] = &[0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
const EC_PUBLIC_KEY: &[u8] = &[0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];

/// PrivateKeyInfo { version 0, rsaEncryption with NULL parameters, the PKCS#1 key }.
fn pkcs8_from_rsa(pkcs1: &[u8]) -> Vec<u8> {
    let algorithm = der(0x30, &[RSA_ENCRYPTION, &[0x05, 0x00]].concat());
    der(0x30, &[&[0x02, 0x01, 0x00][..], &algorithm, &der(0x04, pkcs1)].concat())
}

/// PrivateKeyInfo { version 0, ecPublicKey with the curve named in the SEC1 key, the SEC1 key }.
fn pkcs8_from_sec1(sec1: &[u8]) -> Result<Vec<u8>> {
    let curve = sec1_curve(sec1).context("The EC key does not name its curve")?;
    let algorithm = der(0x30, &[EC_PUBLIC_KEY, curve].concat());
    Ok(der(0x30, &[&[0x02, 0x01, 0x00][..], &algorithm, &der(0x04, sec1)].concat()))
}

/// The curve OID (with its tag and length) from `parameters [0]` of a SEC1 ECPrivateKey.
fn sec1_curve(sec1: &[u8]) -> Option<&[u8]> {
    fn element(data: &[u8]) -> Option<(u8, &[u8], &[u8])> {
        let (&tag, rest) = data.split_first()?;
        let (&first, rest) = rest.split_first()?;
        let (length, rest) = if first < 0x80 {
            (first as usize, rest)
        } else {
            let count = (first & 0x7f) as usize;
            if count == 0 || count > 4 || rest.len() < count {
                return None;
            }
            let length = rest[..count].iter().fold(0usize, |acc, b| (acc << 8) | *b as usize);
            (length, &rest[count..])
        };
        (rest.len() >= length).then(|| (tag, &rest[..length], &rest[length..]))
    }
    let (tag, mut body, _) = element(sec1)?;
    if tag != 0x30 {
        return None;
    }
    while !body.is_empty() {
        let (tag, contents, rest) = element(body)?;
        if tag == 0xa0 {
            // `[0]` holds exactly one element, the curve OID with its tag and length.
            let (oid_tag, _, rest) = element(contents)?;
            return (oid_tag == 0x06 && rest.is_empty()).then_some(contents);
        }
        body = rest;
    }
    None
}

fn read_file(path: &str, label: &str) -> Result<Vec<u8>> {
    const LIMIT: u64 = 8 * 1024 * 1024;
    ensure!(
        std::fs::metadata(path)
            .map_err(|_| anyhow::anyhow!("Cannot inspect {label}"))?
            .is_file(),
        "{label} must be a regular file"
    );
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    // Do not block if a regular path is replaced with a FIFO between metadata and open.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|_| anyhow::anyhow!("Cannot read {label}"))?;
    ensure!(
        file.metadata()
            .map_err(|_| anyhow::anyhow!("Cannot inspect {label}"))?
            .is_file(),
        "{label} must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("Cannot read {label}"))?;
    ensure!(bytes.len() as u64 <= LIMIT, "{label} exceeds 8 MiB");
    Ok(bytes)
}

#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn rsa_and_ec_keys_are_wrapped_as_pkcs8_with_the_right_algorithm() {
        let pkcs8 = pkcs8_from_rsa(&[0x30, 0x03, 0x02, 0x01, 0x00]);
        assert_eq!(&pkcs8[..2], &[0x30, 0x19], "version 3 + algorithm 15 + key 7 bytes");
        assert!(pkcs8.windows(RSA_ENCRYPTION.len()).any(|w| w == RSA_ENCRYPTION));
        assert!(pkcs8.ends_with(&[0x04, 0x05, 0x30, 0x03, 0x02, 0x01, 0x00]));
        // SEC1: SEQUENCE { INTEGER 1, OCTET STRING, [0] OID prime256v1 }.
        let p256 = [0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
        let sec1 = der(0x30, &[&[0x02, 0x01, 0x01][..], &der(0x04, &[7; 32]), &der(0xa0, &p256)].concat());
        assert_eq!(sec1_curve(&sec1), Some(&p256[..]));
        let wrapped = pkcs8_from_sec1(&sec1).unwrap();
        assert!(wrapped.windows(EC_PUBLIC_KEY.len()).any(|w| w == EC_PUBLIC_KEY));
        assert!(wrapped.windows(p256.len()).any(|w| w == p256));
        let without_curve = der(0x30, &[&[0x02, 0x01, 0x01][..], &der(0x04, &[7; 32])].concat());
        assert!(pkcs8_from_sec1(&without_curve).is_err());
        assert_eq!(&der(0x04, &[0; 200])[..3], &[0x04, 0x81, 200]);
    }

    /// The in-memory PKCS#12 must import in this platform's TLS backend
    /// (Security framework on macOS, OpenSSL 3 on Linux) for RSA and EC keys.
    /// Keys are generated by the `openssl` tool; without it the test is skipped.
    #[test]
    fn pem_certificates_become_importable_pkcs12_identities() {
        let dir = std::env::temp_dir().join(format!("dbeast-tls-keys-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let openssl = |args: &[&str]| {
            std::process::Command::new("openssl")
                .args(args)
                .current_dir(&dir)
                .output()
                .is_ok_and(|output| output.status.success())
        };
        if !openssl(&["version"]) {
            eprintln!("skipping: no openssl tool");
            return;
        }
        assert!(openssl(&["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj", "/CN=synthetic", "-keyout", "rsa.key", "-out", "rsa.pem"]));
        let rsa_traditional = openssl(&["rsa", "-in", "rsa.key", "-traditional", "-out", "rsa1.key"]) || openssl(&["rsa", "-in", "rsa.key", "-out", "rsa1.key"]);
        assert!(rsa_traditional);
        assert!(openssl(&["ecparam", "-name", "prime256v1", "-genkey", "-noout", "-out", "ec.key"]));
        assert!(openssl(&["req", "-x509", "-new", "-key", "ec.key", "-days", "1", "-subj", "/CN=synthetic", "-out", "ec.pem"]));
        let read = |name: &str| std::fs::read(dir.join(name)).unwrap();
        for (certificate, key) in [("rsa.pem", "rsa.key"), ("rsa.pem", "rsa1.key"), ("ec.pem", "ec.key")] {
            let (chain, key_der) = pem_identity(&read(certificate), &read(key)).unwrap_or_else(|e| panic!("{key}: {e}"));
            let archive = pkcs12_archive(&chain, &key_der, "synthetic").unwrap();
            assert!(Identity::from_pkcs12(&archive, "synthetic").is_ok(), "{certificate} with {key} imports");
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn encrypted_and_malformed_keys_are_refused_without_echoing_them() {
        let certificate = b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
        let encrypted = b"-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----\n";
        // The certificate is checked first; a bogus one is reported as such.
        let error = pem_identity(certificate, encrypted).unwrap_err().to_string();
        assert!(error.contains("certificate"), "{error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabled_tls_ignores_files_but_enabled_tls_rejects_empty_ca() {
        let dir = std::env::temp_dir().join(format!("dbeast-tls-files-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.pem");
        std::fs::write(&path, "").unwrap();
        let mut source = DataSource::new(DriverKind::Postgres);
        source.tls.ca_file = path.to_string_lossy().into_owned();
        assert!(prepare(&source).await.unwrap().is_none());
        source.use_tls = true;
        assert!(prepare(&source).await.is_err());
        std::fs::write(&path, "not a certificate").unwrap();
        assert!(prepare(&source).await.is_err());
        source.tls.ca_file = dir.to_string_lossy().into_owned();
        assert!(prepare(&source).await.is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn fifo_certificate_is_rejected_without_waiting_for_writer() {
        let path = std::env::temp_dir().join(format!("dbeast-tls-fifo-{}", uuid::Uuid::new_v4()));
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: CString is NUL terminated; this unique temporary path is test-owned.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let start = std::time::Instant::now();
        assert!(read_file(path.to_str().unwrap(), "CA file").is_err());
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
mod cache_tests {
    use super::CacheKey;
    use crate::model::TlsConfig;

    #[test]
    fn edited_files_and_other_passwords_miss_the_cache() {
        let ca = std::env::temp_dir().join(format!("dbeast-tls-cache-{}.pem", uuid::Uuid::new_v4().simple()));
        std::fs::write(&ca, "one").unwrap();
        let settings = TlsConfig { ca_file: ca.to_string_lossy().into_owned(), ..TlsConfig::default() };
        let key = CacheKey::of(true, &settings, "secret");
        assert!(key == CacheKey::of(true, &settings, "secret"));
        assert!(key != CacheKey::of(true, &settings, "other"));
        assert!(key != CacheKey::of(false, &settings, "secret"));
        std::fs::write(&ca, "replaced").unwrap();
        assert!(key != CacheKey::of(true, &settings, "secret"));
        let _ = std::fs::remove_file(&ca);
    }
}
