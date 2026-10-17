//! Linux password storage through the Secret Service. Runs only with
//! DBEAST_TEST_SECRET_SERVICE=1 and a keyring daemon on the session bus, and
//! touches only items of a unique, synthetic service name.

#[cfg(target_os = "linux")]
#[test]
fn linux_keyring_writes_reads_replaces_and_deletes() {
    use dbeast::credentials::{CredentialStore, SystemStore};
    if std::env::var_os("DBEAST_TEST_SECRET_SERVICE").is_none() {
        eprintln!("skipping: set DBEAST_TEST_SECRET_SERVICE=1 with a keyring daemon running");
        return;
    }
    let service = format!("dbeast-test-{}", uuid::Uuid::new_v4());
    let mut store = SystemStore::default();
    assert_eq!(store.read(&service, "vault-v1").unwrap(), None, "nothing stored yet");
    store.write(&service, "vault-v1", b"synthetic secret 1").unwrap();
    assert_eq!(store.read(&service, "vault-v1").unwrap().as_deref(), Some(&b"synthetic secret 1"[..]));
    // Writing again replaces the item instead of adding a second one.
    store.write(&service, "vault-v1", b"synthetic secret 2").unwrap();
    let mut fresh = SystemStore::default();
    assert_eq!(fresh.read(&service, "vault-v1").unwrap().as_deref(), Some(&b"synthetic secret 2"[..]));
    // Accounts are separate items.
    assert_eq!(fresh.read(&service, "mcp-v1").unwrap(), None);
    fresh.delete(&service, "vault-v1").unwrap();
    assert_eq!(store.read(&service, "vault-v1").unwrap(), None);
    fresh.delete(&service, "vault-v1").unwrap();
}
