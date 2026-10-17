//! Opt-in native probe. Never opens the login/default keychain.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "creates a disposable macOS Keychain; run explicitly"]
fn isolated_keychain_round_trip() {
    use dbeast::credentials::native;
    use security_framework::os::macos::keychain::{CreateOptions, SecKeychain};
    let directory =
        std::env::temp_dir().join(format!("dbeast-keychain-probe-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("disposable.keychain");
    struct Cleanup(std::path::PathBuf, std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::process::Command::new("/usr/bin/security")
                .arg("delete-keychain")
                .arg(&self.0)
                .output();
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }
    let _cleanup = Cleanup(path.clone(), directory);
    let mut keychain = CreateOptions::new()
        .password(&uuid::Uuid::new_v4().to_string())
        .prompt_user(false)
        .create(&path)
        .unwrap();
    let service = "ai.inprojects.dbeast.disposable-probe";
    let account = "vault-v1";
    native::write(&mut keychain, service, account, b"synthetic-secret").unwrap();
    let (_, original_item) = security_framework::os::macos::passwords::find_generic_password(
        Some(std::slice::from_ref(&keychain)),
        service,
        account,
    )
    .unwrap();
    native::write(&mut keychain, service, account, b"synthetic-updated-vault").unwrap();
    let (_, updated_item) = security_framework::os::macos::passwords::find_generic_password(
        Some(std::slice::from_ref(&keychain)),
        service,
        account,
    )
    .unwrap();
    assert_eq!(
        original_item, updated_item,
        "vault updates must retain the same Keychain item"
    );
    assert_eq!(
        native::read(&keychain, service, account)
            .unwrap()
            .as_deref(),
        Some(b"synthetic-updated-vault".as_slice())
    );
    drop(keychain);
    let reopened = SecKeychain::open(&path).unwrap();
    assert_eq!(
        native::read(&reopened, service, account)
            .unwrap()
            .as_deref(),
        Some(b"synthetic-updated-vault".as_slice())
    );
    native::delete(&reopened, service, account).unwrap();
    assert!(native::read(&reopened, service, account).unwrap().is_none());
    native::delete(&reopened, service, account).unwrap();
}
