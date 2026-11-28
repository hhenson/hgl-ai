//! Hub credentials (docs/install.md, OWN-5): a file of mode 0600 that holds either the
//! legacy shared token (one word) or one hub user's NKey seed:
//!
//! ```text
//! # hgl-ai hub credential; mode 0600, never commit or print it.
//! user = "build-1"
//! role = "seat"
//! seed = "SU..."
//! ```

use std::path::Path;

use crate::{BusError, Result};

/// What a client presents to the hub.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    /// The shared token of a single-token hub (every client alike).
    Token(String),
    /// A hub user of its own: the NKey seed signs the server's challenge.
    Nkey {
        seed: String,
        user: Option<String>,
        role: Option<String>,
    },
}

impl std::fmt::Debug for Credential {
    // Never the secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credential::Token(_) => f.write_str("Credential::Token(..)"),
            Credential::Nkey { user, role, .. } => f
                .debug_struct("Credential::Nkey")
                .field("user", user)
                .field("role", role)
                .finish_non_exhaustive(),
        }
    }
}

impl Credential {
    /// `user (role)` for a hub user, `shared token` for the legacy token.
    pub fn describe(&self) -> String {
        match self {
            Credential::Token(_) => "shared token".into(),
            Credential::Nkey { user, role, .. } => format!(
                "hub user {} ({})",
                user.as_deref().unwrap_or("unnamed"),
                role.as_deref().unwrap_or("unknown role")
            ),
        }
    }

    pub fn role(&self) -> Option<&str> {
        match self {
            Credential::Token(_) => None,
            Credential::Nkey { role, .. } => role.as_deref(),
        }
    }

    /// The inbox prefix this client uses (hgl-ai#111): a hub user's own,
    /// [`hgl_ai_proto::user_inbox`] of the public key of its seed, which a bare seed (no
    /// `role` line) has too; the `viewer`'s is [`hgl_ai_proto::VIEWER_INBOX`], as the
    /// console's page uses. `None` for the shared token, whose hub has no permissions.
    pub fn inbox_prefix(&self) -> Option<String> {
        match self {
            Credential::Token(_) => None,
            Credential::Nkey { role, .. } if role.as_deref() == Some("viewer") => {
                Some(hgl_ai_proto::VIEWER_INBOX.to_string())
            }
            Credential::Nkey { seed, .. } => nkeys::KeyPair::from_seed(seed)
                .ok()
                .map(|k| hgl_ai_proto::user_inbox(&k.public_key())),
        }
    }

    pub fn is_token(&self) -> bool {
        matches!(self, Credential::Token(_))
    }

    /// The file text for a hub user's credential.
    pub fn render_nkey(user: &str, role: &str, seed: &str) -> String {
        format!(
            "# hgl-ai hub credential; mode 0600, never commit or print it.\n\
             user = \"{user}\"\nrole = \"{role}\"\nseed = \"{seed}\"\n"
        )
    }
}

fn unquote(v: &str) -> &str {
    let v = v.trim();
    v.strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .unwrap_or(v)
}

/// Parse a credential file's text; `None` when it holds nothing.
pub fn parse_credential(text: &str) -> std::result::Result<Option<Credential>, String> {
    let mut seed = None;
    let mut user = None;
    let mut role = None;
    let mut keyed = false;
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = unquote(v).to_string();
        match k.trim() {
            "seed" => {
                keyed = true;
                seed = Some(v)
            }
            "user" => {
                keyed = true;
                user = Some(v)
            }
            "role" => {
                keyed = true;
                role = Some(v)
            }
            _ => {}
        }
    }
    if keyed {
        let seed = seed.ok_or("a hub user credential without a `seed` line")?;
        if !seed.starts_with("SU") || seed.len() < 20 {
            return Err("the seed is not an NKey user seed (SU...)".into());
        }
        return Ok(Some(Credential::Nkey { seed, user, role }));
    }
    let token: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    match token[..] {
        [] => Ok(None),
        // A bare NKey user seed (a host's own key pair, docs/fleet.md FLEET-10).
        [t] if t.starts_with("SU") && t.len() == 58 => Ok(Some(Credential::Nkey {
            seed: t.to_string(),
            user: None,
            role: None,
        })),
        [t] => Ok(Some(Credential::Token(t.to_string()))),
        _ => Err("a token file holds one line".into()),
    }
}

/// Read a credential file.
pub fn read_credential(path: &Path) -> Result<Option<Credential>> {
    let text = std::fs::read_to_string(path).map_err(|source| BusError::Token {
        path: path.display().to_string(),
        source,
    })?;
    parse_credential(&text).map_err(|e| BusError::Nats(format!("{}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hub user's inbox is its public key's, with or without the `role` line (hgl-ai#111).
    #[test]
    fn inbox_of_the_public_key() {
        let kp = nkeys::KeyPair::new_user();
        let seed = kp.seed().unwrap();
        let want = format!("_INBOX.{}", kp.public_key());
        let full = parse_credential(&Credential::render_nkey("w1", "seat", &seed)).unwrap();
        let bare = parse_credential(&seed).unwrap();
        for c in [full, bare] {
            assert_eq!(c.unwrap().inbox_prefix().as_deref(), Some(want.as_str()));
        }
        let viewer = parse_credential(&Credential::render_nkey("viewer", "viewer", &seed));
        assert_eq!(
            viewer.unwrap().unwrap().inbox_prefix().as_deref(),
            Some(hgl_ai_proto::VIEWER_INBOX)
        );
        assert_eq!(Credential::Token("t".into()).inbox_prefix(), None);
    }

    #[test]
    fn tokens_and_users() {
        assert_eq!(
            parse_credential("  s3cret\n").unwrap(),
            Some(Credential::Token("s3cret".into()))
        );
        assert_eq!(parse_credential("\n").unwrap(), None);
        let seed = format!("SU{}", "A".repeat(56));
        let text = Credential::render_nkey("build-1", "seat", &seed);
        let c = parse_credential(&text).unwrap().unwrap();
        assert_eq!(
            c,
            Credential::Nkey {
                seed: seed.clone(),
                user: Some("build-1".into()),
                role: Some("seat".into())
            }
        );
        assert_eq!(c.describe(), "hub user build-1 (seat)");
        assert!(!format!("{c:?}").contains(&seed), "Debug hides the seed");
        assert!(matches!(
            parse_credential(&format!("{seed}\n")).unwrap(),
            Some(Credential::Nkey { user: None, .. })
        ));
        assert!(parse_credential("user = \"x\"\n").is_err());
        assert!(parse_credential("seed = \"nope\"\n").is_err());
        assert!(parse_credential("a\nb\n").is_err());
    }
}
