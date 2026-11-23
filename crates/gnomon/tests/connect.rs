//! Seam 1 for connecting: the Store's TLS settings.
//!
//! TLS with certificate verification is the default. Turning verification
//! off, or TLS off, takes an explicit setting. The test servers run without
//! TLS, so a proxy in `support::tls` terminates TLS in front of them with a
//! certificate each test makes.

mod support;

use gnomon::{ConnectOptions, Store, Tls};
use support::tls::{self, Answer, TestCa};
use support::TestDb;

/// The text of an error and of every error under it, which is where the
/// TLS failure's reason is.
fn chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        out.push_str(": ");
        out.push_str(&s.to_string());
        source = s.source();
    }
    out
}

#[tokio::test]
async fn by_default_the_store_refuses_a_server_whose_certificate_it_does_not_trust() {
    let db = TestDb::migrated().await;
    let ca = TestCa::new();
    let proxy = tls::proxy(
        db.server_addr().await,
        Answer::Tls(ca.server_cert(&["127.0.0.1"])),
    )
    .await;

    match Store::connect(&db.login_url_through(proxy), &ConnectOptions::default()).await {
        Err(e) => assert!(
            chain(&e).contains("UnknownIssuer"),
            "expected an untrusted certificate, got {}",
            chain(&e)
        ),
        Ok(_) => panic!("the Store trusted a certificate from a CA it was never given"),
    }
}

#[tokio::test]
async fn the_store_connects_over_tls_to_a_server_whose_certificate_chains_to_a_trusted_ca() {
    let db = TestDb::migrated().await;
    let ca = TestCa::new();
    let proxy = tls::proxy(
        db.server_addr().await,
        Answer::Tls(ca.server_cert(&["127.0.0.1"])),
    )
    .await;
    let options = ConnectOptions::default().tls(Tls::trust_pem(ca.pem.as_bytes()).unwrap());

    let store = Store::connect(&db.login_url_through(proxy), &options)
        .await
        .expect("the Store trusts the CA it was given");
    let t = support::tenant();
    store
        .as_user(support::user(t, &["support"]))
        .read_episode(t, gnomon::EpisodeId(uuid::Uuid::new_v4()))
        .await
        .expect("a call runs over TLS");
}

#[tokio::test]
async fn the_store_refuses_a_trusted_certificate_that_names_another_host() {
    let db = TestDb::migrated().await;
    let ca = TestCa::new();
    let proxy = tls::proxy(
        db.server_addr().await,
        Answer::Tls(ca.server_cert(&["db.example.test"])),
    )
    .await;
    let options = ConnectOptions::default().tls(Tls::trust_pem(ca.pem.as_bytes()).unwrap());

    match Store::connect(&db.login_url_through(proxy), &options).await {
        Err(e) => assert!(
            chain(&e).contains("not valid for name"),
            "expected a certificate for another name, got {}",
            chain(&e)
        ),
        Ok(_) => panic!("the Store accepted a certificate issued for another host"),
    }
}

#[tokio::test]
async fn by_default_the_store_never_falls_back_to_plain_text_when_the_server_refuses_tls() {
    let db = TestDb::migrated().await;
    let proxy = tls::proxy(db.server_addr().await, Answer::Refuse).await;

    match Store::connect(&db.login_url_through(proxy), &ConnectOptions::default()).await {
        Err(e) => assert!(
            chain(&e).contains("TLS"),
            "expected a refused TLS request, got {}",
            chain(&e)
        ),
        Ok(_) => panic!("the Store connected in plain text to a server that refused TLS"),
    }
}

#[tokio::test]
async fn sslmode_disable_in_the_connection_string_does_not_turn_tls_off() {
    let db = TestDb::migrated().await;
    let url = format!("{} sslmode=disable", db.login_url());

    match Store::connect(&url, &ConnectOptions::default()).await {
        Err(gnomon::Error::InvalidConnection(m)) => assert!(m.contains("sslmode=disable"), "{m}"),
        Err(other) => panic!("expected the settings to be refused, got {other:?}"),
        Ok(_) => panic!("sslmode=disable turned TLS off without an explicit setting"),
    }
}

#[tokio::test]
async fn turning_certificate_verification_off_takes_an_explicit_setting() {
    let db = TestDb::migrated().await;
    let ca = TestCa::new();
    let proxy = tls::proxy(
        db.server_addr().await,
        Answer::Tls(ca.server_cert(&["db.example.test"])),
    )
    .await;

    let options = ConnectOptions::default().tls(Tls::unverified());
    Store::connect(&db.login_url_through(proxy), &options)
        .await
        .expect("an unverified certificate is accepted when verification is turned off");
}

#[tokio::test]
async fn plain_text_takes_an_explicit_setting() {
    let db = TestDb::migrated().await;
    let options = ConnectOptions::default().tls(Tls::plaintext());
    Store::connect(&db.login_url(), &options)
        .await
        .expect("the Store connects in plain text when TLS is turned off");
}
