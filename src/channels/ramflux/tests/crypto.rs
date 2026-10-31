use super::super::crypto::AccountCrypto;

#[test]
fn round_trip_aad_lock_and_debug() {
    let dir = tempfile::tempdir().expect("test: temp");
    super::tighten_dir(dir.path());
    let crypto = AccountCrypto::new(dir.path(), dir.path(), true);
    crypto.unlock("agent-a").expect("test: unlock");
    let blob = crypto.seal("agent-a", "conv-1", b"hello ramflux").expect("test: seal");
    let opened = crypto.open("agent-a", "conv-1", &blob).expect("test: open");
    assert_eq!(opened, b"hello ramflux");
    assert!(crypto.open("agent-a", "conv-2", &blob).is_err());
    crypto.lock("agent-a");
    assert!(crypto.open("agent-a", "conv-1", &blob).is_err());
    crypto.unlock("agent-a").expect("test: reunlock");
    let again = crypto.open("agent-a", "conv-1", &blob).expect("test: reopen");
    assert_eq!(again, b"hello ramflux");
    let rendered = format!("{crypto:?}");
    assert!(rendered.contains("redacted"));
    assert!(!rendered.contains("hello"));
    let wrapped = std::fs::read_dir(dir.path().join("keys")).expect("test: keys");
    for entry in wrapped.flatten() {
        let bytes = std::fs::read(entry.path()).unwrap_or_default();
        assert!(
            !bytes
                .windows(b"hello ramflux".len())
                .any(|window| window == b"hello ramflux")
        );
    }
}

#[test]
fn disabled_encryption_is_passthrough() {
    let dir = tempfile::tempdir().expect("test: temp");
    let crypto = AccountCrypto::new(dir.path(), dir.path(), false);
    let blob = crypto.seal("agent-a", "conv-1", b"plain").expect("test: seal");
    assert_eq!(blob, b"plain");
}
