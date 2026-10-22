use prime_radiant::redact;

/// Builds a fake secret at runtime, so no secret-shaped literal is committed.
fn fake(prefix: &[&str], body: &str, repeat: usize) -> String {
    let mut secret = prefix.concat();
    secret.push_str(&body.repeat(repeat));
    secret
}

#[test]
fn each_kind_of_secret_is_replaced() {
    let private_key = [
        &["-----BEGIN ", "RSA PRIVATE KEY-----"].concat(),
        "MIIEowIBAAKCAQEA0exampleexample",
        &["-----END ", "RSA PRIVATE KEY-----"].concat(),
    ]
    .join("\n");
    let cases = [
        (fake(&["gh", "p_"], "aB3d", 9), "[redacted]"),
        (fake(&["gh", "o_"], "aB3d", 9), "[redacted]"),
        (fake(&["github", "_pat_"], "aB3d_", 12), "[redacted]"),
        (fake(&["s", "k-"], "aB3d", 8), "[redacted]"),
        (fake(&["AK", "IA"], "ABCD2345", 2), "[redacted]"),
        (
            format!("Authorization: Bearer {}", fake(&[], "aB3d.", 6)),
            "Authorization: Bearer [redacted]",
        ),
        (private_key, "[redacted]"),
        (
            format!("OPENAI_API_KEY={}", fake(&[], "x1", 6)),
            "OPENAI_API_KEY=[redacted]",
        ),
        (
            "SLACK_TOKEN=\"abc def\"".to_owned(),
            "SLACK_TOKEN=[redacted]",
        ),
        ("DB_PASSWORD=hunter2".to_owned(), "DB_PASSWORD=[redacted]"),
        ("CLIENT_SECRET='s3'".to_owned(), "CLIENT_SECRET=[redacted]"),
    ];
    for (secret, expected) in cases {
        let text = format!("before {secret} after");
        assert_eq!(
            redact::secrets(&text).as_str(),
            format!("before {expected} after"),
            "{secret:?} is redacted"
        );
    }
}

#[test]
fn unterminated_private_key_is_redacted_to_the_end() {
    let text = [
        "key: -----BEGIN ",
        "PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0B",
    ]
    .concat();
    assert_eq!(redact::secrets(&text).as_str(), "key: [redacted]");
}

#[test]
fn ordinary_text_is_unchanged() {
    let text = "Bearer tokens expire. Run `cargo test` with RUST_LOG=debug. task-runner sk-learn";
    assert_eq!(redact::secrets(text).as_str(), text);
}
