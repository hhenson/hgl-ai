//! The hub token: 32 random bytes as hex, in a file of mode 0600. It is never printed.
//! A client's credential file holds that token (a single-token hub) or its own hub
//! user's NKey seed ([`crate::users`]); [`read_credential`] and [`copy`] take either.

use std::io::Read as _;
use std::path::Path;

use crate::sys::System;
use crate::{Result, io_at, other};

pub const MODE: u32 = 0o600;

/// What happened to the token file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenAction {
    Kept,
    Created,
    Rotated,
}

/// 32 bytes from the system's random device, hex encoded.
pub fn generate() -> Result<String> {
    let mut buf = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(|e| other(format!("reading /dev/urandom: {e}")))?;
    Ok(hex::encode(buf))
}

/// A token must fit in `nats.conf` and on one line: printable, no quotes or spaces.
pub fn validate(t: &str) -> Result<()> {
    let ok = !t.is_empty()
        && t.chars()
            .all(|c| c.is_ascii_graphic() && !"\"'\\#".contains(c));
    if ok {
        Ok(())
    } else {
        Err(other(
            "the hub token must be one word of printable characters without quotes",
        ))
    }
}

/// The token in `path`, trimmed; `None` when the file is absent.
pub fn read(sys: &dyn System, path: &Path) -> Result<Option<String>> {
    if !sys.exists(path) {
        return Ok(None);
    }
    let t = sys
        .read_to_string(path)
        .map_err(io_at(path))?
        .trim()
        .to_string();
    validate(&t).map_err(|e| other(format!("{}: {e}", path.display())))?;
    Ok(Some(t))
}

/// The credential in `path`: a token or a hub user; `None` when the file is absent.
pub fn read_credential(sys: &dyn System, path: &Path) -> Result<Option<hgl_ai_bus::Credential>> {
    if !sys.exists(path) {
        return Ok(None);
    }
    let text = sys.read_to_string(path).map_err(io_at(path))?;
    match hgl_ai_bus::parse_credential(&text) {
        Ok(Some(hgl_ai_bus::Credential::Token(t))) => {
            validate(&t).map_err(|e| other(format!("{}: {e}", path.display())))?;
            Ok(Some(hgl_ai_bus::Credential::Token(t)))
        }
        Ok(c) => Ok(c),
        Err(e) => Err(other(format!("{}: {e}", path.display()))),
    }
}

/// True when only the owner can read or write the file.
pub fn mode_ok(mode: u32) -> bool {
    mode & 0o077 == 0
}

/// Keep the token at `path` (tightening its mode), or write a new one when it
/// is absent or `rotate` is set. Returns the token and what was done.
pub fn ensure(sys: &dyn System, path: &Path, rotate: bool) -> Result<(String, TokenAction)> {
    if !rotate && let Some(t) = read(sys, path)? {
        if !mode_ok(sys.mode(path).map_err(io_at(path))?) {
            sys.chmod(path, MODE).map_err(io_at(path))?;
        }
        return Ok((t, TokenAction::Kept));
    }
    let action = if sys.exists(path) {
        TokenAction::Rotated
    } else {
        TokenAction::Created
    };
    let t = generate()?;
    sys.write(path, format!("{t}\n").as_bytes(), MODE)
        .map_err(io_at(path))?;
    Ok((t, action))
}

/// Copy a credential file (a token or a hub user) to `to` with mode 0600; a no-op copy
/// when both are the same file. Returns what it holds.
pub fn copy(sys: &dyn System, from: &Path, to: &Path) -> Result<hgl_ai_bus::Credential> {
    let c = read_credential(sys, from)?
        .ok_or_else(|| other(format!("no credential at {}", from.display())))?;
    let same = match (std::fs::canonicalize(from), std::fs::canonicalize(to)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if same {
        if !mode_ok(sys.mode(to).map_err(io_at(to))?) {
            sys.chmod(to, MODE).map_err(io_at(to))?;
        }
        return Ok(c);
    }
    let text = match &c {
        hgl_ai_bus::Credential::Token(t) => format!("{t}\n"),
        hgl_ai_bus::Credential::Nkey { .. } => sys.read_to_string(from).map_err(io_at(from))?,
    };
    sys.write(to, text.as_bytes(), MODE).map_err(io_at(to))?;
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::fake::Fake;
    use crate::{DryRun, Os};

    #[test]
    fn generated_tokens_are_hex_and_distinct() {
        let a = generate().unwrap();
        let b = generate().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        validate(&a).unwrap();
        assert!(validate("").is_err() && validate("a b").is_err() && validate("a\"b").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn ensure_creates_keeps_and_rotates() {
        let d = tempfile::tempdir().unwrap();
        let f = Fake::new(d.path(), Os::Linux);
        let p = f.env.home.join(".config/hgl-ai/hub.token");
        let (t1, a) = ensure(&f, &p, false).unwrap();
        assert_eq!(a, TokenAction::Created);
        assert_eq!(f.mode(&p).unwrap(), 0o600);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), format!("{t1}\n"));

        f.chmod(&p, 0o644).unwrap();
        let (t2, a) = ensure(&f, &p, false).unwrap();
        assert_eq!((t2.as_str(), a), (t1.as_str(), TokenAction::Kept));
        assert_eq!(f.mode(&p).unwrap(), 0o600, "mode tightened");

        let (t3, a) = ensure(&f, &p, true).unwrap();
        assert_eq!(a, TokenAction::Rotated);
        assert_ne!(t3, t1);
        assert_eq!(read(&f, &p).unwrap().unwrap(), t3);
    }

    #[cfg(unix)]
    #[test]
    fn copy_sets_mode_and_dry_run_writes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let f = Fake::new(d.path(), Os::Linux);
        let src = f.env.home.join("hub.token");
        std::fs::write(&src, "abc123\n").unwrap();
        let dst = f.env.home.join(".config/hgl-ai/hub.token");
        copy(&DryRun::new(&f), &src, &dst).unwrap();
        assert!(!dst.exists());
        copy(&f, &src, &dst).unwrap();
        assert_eq!(read(&f, &dst).unwrap().as_deref(), Some("abc123"));
        assert_eq!(f.mode(&dst).unwrap(), 0o600);
        copy(&f, &dst, &dst).unwrap();
        assert!(copy(&f, &f.env.home.join("missing"), &dst).is_err());

        let cred = f.env.home.join("w1.cred");
        let text =
            hgl_ai_bus::Credential::render_nkey("w1", "seat", &format!("SU{}", "B".repeat(56)));
        std::fs::write(&cred, &text).unwrap();
        let c = copy(&f, &cred, &dst).unwrap();
        assert_eq!(c.role(), Some("seat"));
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), text);
        assert_eq!(f.mode(&dst).unwrap(), 0o600);
    }
}
