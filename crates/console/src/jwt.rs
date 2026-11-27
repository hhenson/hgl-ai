//! OpenID Connect `id_token`s: the signature checked against the provider's published
//! keys (JWKS, fetched over TLS and cached for their `max-age`), then the claims. RS256
//! and ES256 only, each only with a key of its own type (no `none`, no HMAC).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::Deserialize;

/// Keys are kept at most this long whatever the provider says.
const MAX_CACHE: Duration = Duration::from_secs(24 * 3600);
/// Without `Cache-Control: max-age`, keys are kept this long.
const DEFAULT_CACHE: Duration = Duration::from_secs(3600);
/// An unknown `kid` refetches the keys, at most this often.
const REFETCH_GAP: Duration = Duration::from_secs(60);
/// Clock leeway for `exp` and `iat`.
const LEEWAY: i64 = 60;

#[derive(Debug, Clone, Deserialize)]
pub struct Jwk {
    #[serde(default)]
    pub kid: Option<String>,
    pub kty: String,
    #[serde(default)]
    pub alg: Option<String>,
    #[serde(default)]
    pub n: Option<String>,
    #[serde(default)]
    pub e: Option<String>,
    #[serde(default)]
    pub crv: Option<String>,
    #[serde(default)]
    pub x: Option<String>,
    #[serde(default)]
    pub y: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Jwks {
    pub keys: Vec<Jwk>,
}

fn b64(s: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .map_err(|_| "not base64url".to_string())
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    #[serde(default)]
    kid: Option<String>,
}

/// The `kid` a token names, unverified (to pick the keys).
pub fn kid(token: &str) -> Option<String> {
    let h = token.split('.').next()?;
    serde_json::from_slice::<Header>(&b64(h).ok()?).ok()?.kid
}

/// Check the signature of a compact JWS against `jwks`; its claims when it holds.
pub fn verify(token: &str, jwks: &Jwks) -> Result<serde_json::Value, String> {
    let parts: Vec<&str> = token.split('.').collect();
    let [h, p, sig] = parts[..] else {
        return Err("not a compact JWT".into());
    };
    let header: Header =
        serde_json::from_slice(&b64(h)?).map_err(|_| "an unreadable JWT header".to_string())?;
    let key = jwks
        .keys
        .iter()
        .find(|k| header.kid.is_some() && k.kid == header.kid)
        .ok_or("no published key has the token's kid")?;
    if let Some(a) = &key.alg
        && a != &header.alg
    {
        return Err(format!("the key is for {a}, the token says {}", header.alg));
    }
    let msg = format!("{h}.{p}");
    let sig = b64(sig)?;
    use ring::signature;
    let ok = match (header.alg.as_str(), key.kty.as_str()) {
        ("RS256", "RSA") => {
            let (n, e) = (
                b64(key.n.as_deref().ok_or("an RSA key without n")?)?,
                b64(key.e.as_deref().ok_or("an RSA key without e")?)?,
            );
            signature::RsaPublicKeyComponents { n: &n, e: &e }
                .verify(&signature::RSA_PKCS1_2048_8192_SHA256, msg.as_bytes(), &sig)
                .is_ok()
        }
        ("ES256", "EC") if key.crv.as_deref() == Some("P-256") => {
            let mut point = vec![4u8];
            point.extend(b64(key.x.as_deref().ok_or("an EC key without x")?)?);
            point.extend(b64(key.y.as_deref().ok_or("an EC key without y")?)?);
            signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, &point)
                .verify(msg.as_bytes(), &sig)
                .is_ok()
        }
        (a, k) => return Err(format!("a {a} token with a {k} key is not accepted")),
    };
    if !ok {
        return Err("the signature does not hold".into());
    }
    serde_json::from_slice(&b64(p)?).map_err(|_| "unreadable claims".to_string())
}

/// What an `id_token` must say.
pub struct Expect<'a> {
    pub issuers: &'a [&'a str],
    pub audience: &'a str,
    pub nonce: &'a str,
    pub now: i64,
}

/// Check the claims; the verified email when they hold.
pub fn check(claims: &serde_json::Value, x: &Expect) -> Result<String, String> {
    let s = |k: &str| claims.get(k).and_then(|v| v.as_str()).unwrap_or("");
    if !x.issuers.contains(&s("iss")) {
        return Err("an id_token from another issuer".into());
    }
    let aud_ok = match claims.get("aud") {
        Some(serde_json::Value::String(a)) => a == x.audience,
        Some(serde_json::Value::Array(v)) => {
            v.iter().any(|a| a.as_str() == Some(x.audience))
                && (v.len() == 1 || s("azp") == x.audience)
        }
        _ => false,
    };
    if !aud_ok {
        return Err("an id_token for another client".into());
    }
    let num = |k: &str| claims.get(k).and_then(|v| v.as_i64());
    if num("exp").is_none_or(|e| e + LEEWAY <= x.now) {
        return Err("the id_token expired".into());
    }
    if num("iat").is_some_and(|i| i > x.now + LEEWAY) {
        return Err("an id_token from the future".into());
    }
    if s("nonce") != x.nonce || x.nonce.is_empty() {
        return Err("the id_token's nonce does not match this sign-in".into());
    }
    let verified = match claims.get("email_verified") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(v)) => v == "true",
        _ => false,
    };
    let email = s("email");
    if email.is_empty() || !verified {
        return Err("no verified email".into());
    }
    Ok(email.to_string())
}

fn max_age(cache_control: Option<&str>) -> Duration {
    cache_control
        .and_then(|c| {
            c.split(',')
                .filter_map(|d| d.trim().strip_prefix("max-age="))
                .find_map(|v| v.trim().parse::<u64>().ok())
        })
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_CACHE)
        .min(MAX_CACHE)
}

struct Cached {
    jwks: std::sync::Arc<Jwks>,
    expires: Instant,
    fetched: Instant,
}

/// Each provider's keys, fetched on first use and again when they expire or a token
/// names a key they lack (a rotation).
#[derive(Default)]
pub struct JwksCache {
    entries: Mutex<HashMap<String, Cached>>,
}

impl JwksCache {
    pub async fn keys(
        &self,
        http: &reqwest::Client,
        url: &str,
        kid: Option<&str>,
    ) -> Result<std::sync::Arc<Jwks>, String> {
        let now = Instant::now();
        {
            let e = self.entries.lock().expect("jwks");
            if let Some(c) = e.get(url) {
                let has =
                    kid.is_none_or(|k| c.jwks.keys.iter().any(|j| j.kid.as_deref() == Some(k)));
                if c.expires > now && (has || now.duration_since(c.fetched) < REFETCH_GAP) {
                    return Ok(c.jwks.clone());
                }
            }
        }
        let r = http
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("fetching the signing keys: {}", e.without_url()))?;
        let ttl = max_age(
            r.headers()
                .get(reqwest::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
        );
        let jwks: Jwks = r
            .json()
            .await
            .map_err(|e| format!("reading the signing keys: {}", e.without_url()))?;
        let jwks = std::sync::Arc::new(jwks);
        self.entries.lock().expect("jwks").insert(
            url.to_string(),
            Cached {
                jwks: jwks.clone(),
                expires: now + ttl,
                fetched: now,
            },
        );
        Ok(jwks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = include_str!("../tests/fixtures/rs256.jwt");
    const JWKS: &str = include_str!("../tests/fixtures/rs256.jwks.json");

    fn expect() -> Expect<'static> {
        Expect {
            issuers: &["https://accounts.google.com"],
            audience: "test-client.apps.googleusercontent.com",
            nonce: "fixture-nonce",
            now: 1_764_201_700,
        }
    }

    #[test]
    fn rs256_against_published_keys() {
        let jwks: Jwks = serde_json::from_str(JWKS).unwrap();
        let claims = verify(TOKEN.trim(), &jwks).unwrap();
        assert_eq!(check(&claims, &expect()).unwrap(), "owner@example.com");
        assert_eq!(kid(TOKEN.trim()).as_deref(), Some("test-rsa"));

        // Any change to the signed part, or another key, breaks it.
        let t = TOKEN.trim();
        let (head, sig) = t.rsplit_once('.').unwrap();
        let mut claims_b64: Vec<&str> = head.split('.').collect();
        let forged = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            br#"{"iss":"https://accounts.google.com","aud":"test-client.apps.googleusercontent.com","email":"mallory@example.com","email_verified":true,"nonce":"fixture-nonce","exp":4102444800}"#,
        );
        claims_b64[1] = &forged;
        let tampered = format!("{}.{sig}", claims_b64.join("."));
        assert!(
            verify(&tampered, &jwks)
                .unwrap_err()
                .contains("does not hold")
        );
        let mut other = jwks.clone();
        other.keys[0].kid = Some("other".into());
        assert!(verify(t, &other).is_err(), "no key with that kid");
        let mut wrong_alg = jwks.clone();
        wrong_alg.keys[0].alg = Some("ES256".into());
        assert!(verify(t, &wrong_alg).is_err());
        // `alg: none` and HMAC are never accepted.
        let none = format!(
            "{}.{}.",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(br#"{"alg":"none","kid":"test-rsa"}"#),
            claims_b64[1]
        );
        assert!(verify(&none, &jwks).is_err());
    }

    #[test]
    fn claims() {
        let ok = serde_json::json!({
            "iss": "https://accounts.google.com", "aud": "c", "exp": 2000, "iat": 1000,
            "nonce": "n", "email": "o@example.com", "email_verified": true
        });
        let x = Expect {
            issuers: &["https://accounts.google.com"],
            audience: "c",
            nonce: "n",
            now: 1500,
        };
        assert_eq!(check(&ok, &x).unwrap(), "o@example.com");
        let with = |k: &str, v: serde_json::Value| {
            let mut c = ok.clone();
            c[k] = v;
            check(&c, &x)
        };
        assert!(with("iss", "https://evil.example.com".into()).is_err());
        assert!(
            with("aud", "other".into())
                .unwrap_err()
                .contains("another client")
        );
        assert!(
            with("aud", serde_json::json!(["c", "other"])).is_err(),
            "several audiences need azp"
        );
        assert!(with("exp", 1000.into()).unwrap_err().contains("expired"));
        assert!(with("iat", 9999.into()).is_err());
        assert!(with("nonce", "m".into()).unwrap_err().contains("nonce"));
        assert!(
            with("email_verified", false.into())
                .unwrap_err()
                .contains("verified")
        );
        assert!(with("email_verified", "false".into()).is_err());
        assert_eq!(
            max_age(Some("public, max-age=19800, must-revalidate")),
            Duration::from_secs(19800)
        );
        assert_eq!(max_age(None), DEFAULT_CACHE);
        assert_eq!(max_age(Some("max-age=999999999")), MAX_CACHE);
    }
}
