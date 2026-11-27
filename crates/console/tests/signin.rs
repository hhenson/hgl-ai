//! Sign-in (OWN-6) end to end: mock GitHub and Apple, a software passkey, the console's
//! router on loopback. No hub and no real OAuth call.

mod common;

use std::sync::Arc;

use base64::Engine as _;
use common::*;
use hgl_ai_console::App;
use webauthn_authenticator_rs::softpasskey::SoftPasskey;

async fn setup() -> (Providers, Harness, String) {
    let (pem, public) = apple_key();
    let p = Providers::start(public).await;
    let h = Harness::new(&p, &pem);
    let app = Arc::new(App::new(h.cfg.clone()).unwrap());
    let base = serve(app).await;
    (p, h, base)
}

#[tokio::test]
async fn github_then_passkey_then_sign_out_and_in() {
    let (_p, h, base) = setup().await;
    let mut b = Browser::new(&base);

    // Not signed in: pages send you to sign-in.
    let r = b.get("/").await;
    assert_eq!(r.status(), 303);
    assert_eq!(r.headers()["location"], "/signin");
    let page = b.get("/signin").await.text().await.unwrap();
    assert!(page.contains("Continue with GitHub") && page.contains("Sign in with Apple"));

    // GitHub vouches; the flow cookie binds the browser; then a passkey.
    let r = b.get("/auth/github").await;
    let loc = r.headers()["location"].to_str().unwrap().to_string();
    assert!(
        loc.contains("client_id=Iv1.example") && loc.contains("allow_signup=false"),
        "{loc}"
    );
    let flow = r.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(
        flow.starts_with("hgl_flow=")
            && flow.contains("Path=/auth; HttpOnly; Secure; SameSite=None"),
        "{flow}"
    );
    let state = webauthn_rs::prelude::Url::parse(&loc)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .to_string();
    let r = b
        .get(&format!(
            "/auth/github/callback?code=good-hhenson&state={state}"
        ))
        .await;
    assert_eq!(r.status(), 200);
    assert!(
        r.text()
            .await
            .unwrap()
            .contains("Signed in as <strong>github:hhenson</strong>")
    );
    let mut key = SoftPasskey::new(true);
    let r = register(&mut b, &mut key).await;
    assert_eq!(r.status(), 200);
    let set: Vec<String> = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    assert!(
        set.iter()
            .any(|c| c.starts_with("hgl_session=")
                && c.contains("HttpOnly; Secure; SameSite=Strict")),
        "{set:?}"
    );
    let page = b.get("/telemetry").await;
    assert_eq!(page.status(), 200);
    let csp = page.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(csp.contains("script-src 'self'") && csp.contains("frame-ancestors 'none'"));
    let page = page.text().await.unwrap();
    assert!(page.contains("github:hhenson") && page.contains("The hub is not reachable"));

    // The passkeys file is private and holds no session.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let f = h.dir.path().join("data/console/passkeys.json");
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    // Sign-out needs the session's CSRF token and the console's origin.
    let token = b.csrf("/telemetry").await;
    let r = b
        .post_form("/auth/signout", &[("csrf", "wrong")], Some(ORIGIN))
        .await;
    assert_eq!(r.status(), 403);
    let r = b
        .post_form(
            "/auth/signout",
            &[("csrf", &token)],
            Some("https://evil.example.com"),
        )
        .await;
    assert_eq!(r.status(), 403);
    let r = b
        .post_form("/auth/signout", &[("csrf", &token)], Some(ORIGIN))
        .await;
    assert_eq!(r.status(), 303);
    assert_eq!(b.get("/").await.status(), 303, "signed out");

    // The passkey alone signs in again, in a fresh browser too.
    let mut fresh = Browser::new(&base);
    let r = sign_in(&mut fresh, &mut key).await;
    assert_eq!(r.status(), 200);
    assert_eq!(fresh.get("/telemetry").await.status(), 200);

    // A passkey challenge from another site's page is refused.
    let r = Browser::new(&base)
        .http
        .post(format!("{base}/auth/passkey/login/start"))
        .header("origin", "https://evil.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    // A passkey the console never registered is refused.
    let mut stranger = SoftPasskey::new(true);
    let mut c = Browser::new(&base);
    let r = c
        .post_json("/auth/passkey/login/start", &serde_json::json!({}))
        .await;
    let rcr: webauthn_rs::prelude::RequestChallengeResponse = r.json().await.unwrap();
    use webauthn_authenticator_rs::WebauthnAuthenticator as _;
    assert!(
        stranger.do_authentication(origin_url(), rcr).is_err(),
        "it holds none of the allowed credentials"
    );
}

#[tokio::test]
async fn refusals() {
    let (_p, _h, base) = setup().await;

    // Not on the allow-list.
    let mut b = Browser::new(&base);
    let r = github(&mut b, "good-mallory").await;
    assert_eq!(r.status(), 403);
    assert!(
        r.text()
            .await
            .unwrap()
            .contains("github:mallory may not sign in")
    );
    let r = b
        .post_json("/auth/passkey/register/start", &serde_json::json!({}))
        .await;
    assert_eq!(r.status(), 400, "no enrolment without the provider's word");

    // The provider refuses the code.
    let mut b = Browser::new(&base);
    assert_eq!(github(&mut b, "bad").await.status(), 502);

    // A callback without the flow cookie (another browser), or replayed.
    let mut b = Browser::new(&base);
    let r = b.get("/auth/github").await;
    let loc = r.headers()["location"].to_str().unwrap().to_string();
    let state = webauthn_rs::prelude::Url::parse(&loc)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .to_string();
    let mut other = Browser::new(&base);
    let r = other
        .get(&format!(
            "/auth/github/callback?code=good-hhenson&state={state}"
        ))
        .await;
    assert_eq!(r.status(), 400);
    let r = b
        .get(&format!(
            "/auth/github/callback?code=good-hhenson&state={state}"
        ))
        .await;
    assert_eq!(r.status(), 400, "the flow was spent by the first attempt");
    let r = b
        .get("/auth/github/callback?code=good-hhenson&state=forged")
        .await;
    assert_eq!(r.status(), 400);

    // No passkey yet: sign-in says how to start.
    let mut b = Browser::new(&base);
    let r = b
        .post_json("/auth/passkey/login/start", &serde_json::json!({}))
        .await;
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("GitHub or Apple first"));

    // Owner pages refuse a POST without a session.
    let r = b
        .post_form(
            "/conversations",
            &[("csrf", "x"), ("title", "t"), ("body", "b")],
            Some(ORIGIN),
        )
        .await;
    assert_eq!(r.status(), 403);
    assert_eq!(b.get("/telemetry.json").await.status(), 401);
    assert_eq!(b.get("/telemetry/events").await.status(), 401);
}

#[tokio::test]
async fn sign_in_with_apple() {
    let (p, _h, base) = setup().await;
    let mut b = Browser::new(&base);
    let r = b.get("/auth/apple").await;
    assert_eq!(r.status(), 303);
    let loc = webauthn_rs::prelude::Url::parse(r.headers()["location"].to_str().unwrap()).unwrap();
    let q: std::collections::BTreeMap<String, String> = loc.query_pairs().into_owned().collect();
    assert_eq!(q["response_mode"], "form_post");
    assert_eq!(q["scope"], "email");
    assert_eq!(q["client_id"], APPLE_CLIENT);
    // Apple posts back across sites: only the SameSite=None flow cookie comes with it.
    let code = format!("good:owner@example.com:{}", q["nonce"]);
    let r = b
        .post_form(
            "/auth/apple/callback",
            &[("code", &code), ("state", &q["state"])],
            Some("https://appleid.apple.com"),
        )
        .await;
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("apple:owner@example.com"));
    let mut key = SoftPasskey::new(true);
    assert_eq!(register(&mut b, &mut key).await.status(), 200);
    assert_eq!(b.get("/").await.status(), 200);

    // The client secret was an ES256 JWT signed with the Services ID's key.
    let secret = p.apple_secrets.lock().unwrap()[0].clone();
    let parts: Vec<&str> = secret.split('.').collect();
    let dec = |s: &str| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s)
            .unwrap()
    };
    let header: serde_json::Value = serde_json::from_slice(&dec(parts[0])).unwrap();
    let claims: serde_json::Value = serde_json::from_slice(&dec(parts[1])).unwrap();
    assert_eq!(
        (header["alg"].as_str(), header["kid"].as_str()),
        (Some("ES256"), Some("KEY1234567"))
    );
    assert_eq!(claims["iss"], "TEAM123456");
    assert_eq!(claims["sub"], APPLE_CLIENT);
    assert_eq!(claims["aud"], "https://appleid.apple.com");
    use p256::ecdsa::signature::Verifier as _;
    let sig = p256::ecdsa::Signature::from_slice(&dec(parts[2])).unwrap();
    p.apple_key
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
        .unwrap();

    // A token signed with a key Apple never published is refused.
    let mut b = Browser::new(&base);
    let r = b.get("/auth/apple").await;
    let loc = webauthn_rs::prelude::Url::parse(r.headers()["location"].to_str().unwrap()).unwrap();
    let qs: std::collections::BTreeMap<String, String> = loc.query_pairs().into_owned().collect();
    let code = format!("good:owner@example.com:{}:badsig", qs["nonce"]);
    let r = b
        .post_form(
            "/auth/apple/callback",
            &[("code", &code), ("state", &qs["state"])],
            None,
        )
        .await;
    assert_eq!(r.status(), 502);
    assert!(r.text().await.unwrap().contains("signature"));

    // A wrong nonce (a replayed id_token) is refused.
    let mut b = Browser::new(&base);
    let r = b.get("/auth/apple").await;
    let loc = webauthn_rs::prelude::Url::parse(r.headers()["location"].to_str().unwrap()).unwrap();
    let state = loc
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .to_string();
    let r = b
        .post_form(
            "/auth/apple/callback",
            &[("code", "good:owner@example.com:other"), ("state", &state)],
            None,
        )
        .await;
    assert_eq!(r.status(), 502);
}

fn query(loc: &str) -> std::collections::BTreeMap<String, String> {
    webauthn_rs::prelude::Url::parse(loc)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

/// Start Google's flow; the provider will answer `code` built from what it was sent.
async fn google(b: &mut Browser, email: &str, flag: &str) -> reqwest::Response {
    let r = b.get("/auth/google").await;
    assert_eq!(r.status(), 303);
    let q = query(r.headers()["location"].to_str().unwrap());
    assert_eq!(q["scope"], "openid email");
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["client_id"], GOOGLE_CLIENT);
    let code = format!("good:{email}:{}:{}:{flag}", q["nonce"], q["code_challenge"]);
    let code: String = webauthn_rs::prelude::Url::parse("http://x/")
        .unwrap()
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &q["state"])
        .finish()
        .query()
        .unwrap()
        .to_string();
    b.get(&format!("/auth/google/callback?{code}")).await
}

#[tokio::test]
async fn sign_in_with_google() {
    let (p, _h, base) = setup().await;
    let mut b = Browser::new(&base);
    let r = google(&mut b, "owner@example.com", "").await;
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("google:owner@example.com"));
    let mut key = SoftPasskey::new(true);
    assert_eq!(register(&mut b, &mut key).await.status(), 200);
    let page = b.get("/").await.text().await.unwrap();
    assert!(
        page.contains("OW (google:owner@example.com)"),
        "the handle is shown"
    );

    // Refused: a token signed by a key Google never published, one for another client,
    // an unverified email, and an email not on the list.
    for (email, flag, status, why) in [
        ("owner@example.com", "badsig", 502, "signature"),
        ("owner@example.com", "aud", 502, "another client"),
        ("owner@example.com", "unverified", 502, "verified email"),
        ("mallory@example.com", "", 403, "may not sign in"),
    ] {
        let mut b = Browser::new(&base);
        let r = google(&mut b, email, flag).await;
        assert_eq!(r.status(), status, "{flag} {email}");
        assert!(r.text().await.unwrap().contains(why), "{why}");
        let r = b
            .post_json("/auth/passkey/register/start", &serde_json::json!({}))
            .await;
        assert_eq!(r.status(), 400, "no enrolment after a refusal");
    }
    // The same email through Apple is another identity: allowed only because the
    // list names `apple:` too; Google's keys were fetched once and cached.
    assert_eq!(p.jwks_fetches.lock().unwrap().get("google"), Some(&1));

    // A PKCE verifier that does not match the challenge (a stolen code) is refused.
    let mut b = Browser::new(&base);
    let r = b.get("/auth/google").await;
    let q = query(r.headers()["location"].to_str().unwrap());
    let r = b
        .get(&format!(
            "/auth/google/callback?code=good:owner@example.com:{}:wrongchallenge&state={}",
            q["nonce"], q["state"]
        ))
        .await;
    assert_eq!(r.status(), 502);
}
