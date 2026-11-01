use super::super::crypto::AccountCrypto;
use super::super::erase::{self, dir_contains};
use super::super::storage::ChannelStore;

#[test]
fn erase_removes_plaintext_and_ciphertext() {
    let dir = tempfile::tempdir().expect("test: temp");
    super::tighten_dir(dir.path());
    let store = ChannelStore::open(dir.path()).expect("test: store");
    let crypto = AccountCrypto::new(dir.path(), store.root(), true);
    crypto.unlock("agent-a").expect("test: unlock");
    let plaintext = b"secret-message-body";
    let path = store
        .put_message(&crypto, "agent-a", "conv-1", "m1", plaintext)
        .expect("test: put");
    let opened = store
        .get_message(&crypto, "agent-a", "conv-1", "m1")
        .expect("test: get");
    assert_eq!(opened, plaintext);
    let ciphertext = std::fs::read(&path).expect("test: read cipher");
    assert!(!dir_contains(store.root(), plaintext), "plaintext was written to disk");
    assert!(dir_contains(store.root(), &ciphertext));
    erase::erase_message(&store, "agent-a", "conv-1", "m1").expect("test: erase");
    assert!(!dir_contains(dir.path(), plaintext));
    assert!(!dir_contains(dir.path(), &ciphertext));
}

#[test]
fn legacy_plaintext_directory_is_deleted_on_open() {
    let dir = tempfile::tempdir().expect("test: temp");
    let legacy = dir.path().join("ramflux").join("plaintext");
    std::fs::create_dir_all(&legacy).expect("test: mkdir");
    std::fs::write(legacy.join("note.txt"), b"old plaintext").expect("test: write");
    let _store = ChannelStore::open(dir.path()).expect("test: open");
    assert!(!legacy.exists());
}
