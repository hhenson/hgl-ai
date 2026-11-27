//! The console terminates TLS itself from configured certificate and key files: a
//! certificate made for the test, a client that trusts only it. No hub.

mod common;

use std::time::Duration;

use common::*;

#[tokio::test]
async fn serves_https_from_its_own_certificate() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (pem, public) = apple_key();
    let p = Providers::start(public).await;
    let mut h = Harness::new(&p, &pem);
    let ck = rcgen::generate_simple_self_signed(vec!["console.example.com".to_string()]).unwrap();
    let cert = h.dir.path().join("cert.pem");
    let key = h.dir.path().join("key.pem");
    std::fs::write(&cert, ck.cert.pem()).unwrap();
    write_secret(&key, &ck.signing_key.serialize_pem());
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let c = h.cfg.console.as_mut().unwrap();
    c.listen = addr.to_string();
    c.url = format!("https://console.example.com:{port}");
    c.tls_cert = Some(cert.display().to_string());
    c.tls_key = Some(key.display().to_string());
    // No hub: the console keeps serving while it retries.
    h.cfg.hub.url = "nats://127.0.0.1:1".into();
    h.cfg.hub.token_file = h.dir.path().join("none").display().to_string();
    let server = tokio::spawn(hgl_ai_console::service::run(h.cfg.clone()));

    let client = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(ck.cert.pem().as_bytes()).unwrap())
        .resolve("console.example.com", addr)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let url = format!("https://console.example.com:{port}/signin");
    let mut got = None;
    for _ in 0..100 {
        if let Ok(r) = client.get(&url).send().await {
            got = Some(r);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let r = got.expect("the console answered over TLS");
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["strict-transport-security"], "max-age=31536000");
    assert!(r.text().await.unwrap().contains("Sign in with a passkey"));
    // Plain HTTP on that port gets no page.
    let plain = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/signin"))
        .send()
        .await;
    assert!(plain.map(|r| r.status() != 200).unwrap_or(true));
    server.abort();
}
