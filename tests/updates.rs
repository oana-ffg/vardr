use vardr::updates::verify;

// Public, non-secret signature fixtures from Ratatoskr Skill Sync (MIT,
// copyright 2026 Oana Goge). No private test/release signing key is included.
#[test]
fn signatures_authenticate_bytes_and_reject_tampering() {
    let key = include_str!("fixtures/update.pub");
    let bytes = include_bytes!("fixtures/update.txt");
    let signature = include_str!("fixtures/update.txt.sig");
    verify(key, bytes, signature).unwrap();
    let mut tampered = bytes.to_vec();
    tampered[0] ^= 1;
    assert!(verify(key, &tampered, signature).is_err());
    assert!(verify(vardr::updates::PUBLIC_KEY, bytes, signature).is_err());
    assert!(verify(key, bytes, "invalid-signature").is_err());
}
