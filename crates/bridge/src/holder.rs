//! Who holds a seat lease (PROTO-6): `<host> pid <pid> (<what>) id <host-id>`, where `what`
//! is `bridge`, `note wait` or `note peek`, and `host-id` is a random id this host keeps in
//! `<data_dir>/host-id`. A lease whose holder is of this host (same label and id) and whose
//! process is gone is taken over at once, by a bridge or an interactive receive; a holder of
//! another machine that shares the label (a configuration error: labels are unique) has
//! another id and is never taken over, so two such hosts do not steal a seat from each
//! other.

use std::io::Read as _;
use std::path::Path;

/// The file under `data_dir` that keeps this host's id.
pub const HOST_ID_FILE: &str = "host-id";

/// This host's id: read from `<data_dir>/host-id`, made on first use. On an error (logged)
/// an id for this process only.
pub fn host_id(data_dir: &Path) -> String {
    let p = data_dir.join(HOST_ID_FILE);
    let read = |p: &Path| -> Option<String> {
        let mut s = String::new();
        std::fs::File::open(p)
            .ok()?
            .take(256)
            .read_to_string(&mut s)
            .ok()?;
        let s = s.trim().to_string();
        valid_id(&s).then_some(s)
    };
    if let Some(id) = read(&p) {
        return id;
    }
    let id = random_id();
    let made = std::fs::create_dir_all(data_dir).and_then(|()| {
        // Two processes making it at once: the first rename wins and both read it back.
        let tmp = data_dir.join(format!("{HOST_ID_FILE}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, format!("{id}\n"))?;
        if p.exists() {
            let _ = std::fs::remove_file(&tmp);
            return Ok(());
        }
        std::fs::rename(&tmp, &p)
    });
    if let Err(e) = made {
        tracing::warn!(path = %p.display(), "keeping the host id: {e}; using one for this process");
        return id;
    }
    read(&p).unwrap_or(id)
}

fn valid_id(s: &str) -> bool {
    (8..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn random_id() -> String {
    let mut b = [0u8; 8];
    let got = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    if got.is_err() {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        b = ((t as u64) ^ (u64::from(std::process::id()) << 32)).to_be_bytes();
    }
    hex::encode(b)
}

/// The holder string of this process: `<host> pid <pid> (<what>) id <id>`.
pub fn holder(host: &str, id: &str, what: &str) -> String {
    format!("{host} pid {} ({what}) id {id}", std::process::id())
}

/// Whether `holder` names a seat server of this host (label `host`, id `id`) that is gone:
/// its process is dead, or, for a bridge, it is this process (a refresh whose answer was
/// lost). A holder without an id (an older version) counts as this host's by its label.
pub fn dead_own(holder: &str, host: &str, id: &str) -> bool {
    let Some(rest) = holder.strip_prefix(&format!("{host} pid ")) else {
        return false;
    };
    let Some((pid, rest)) = rest.split_once(' ') else {
        return false;
    };
    let Ok(pid) = pid.parse::<u32>() else {
        return false;
    };
    let Some(what_end) = rest.find(')') else {
        return false;
    };
    let (what, tail) = rest.split_at(what_end + 1);
    match tail.trim().strip_prefix("id ") {
        Some(theirs) if theirs.trim() != id => return false,
        Some(_) => {}
        None if tail.trim().is_empty() => {}
        None => return false,
    }
    match what {
        "(bridge)" if pid == std::process::id() => true,
        "(bridge)" | "(note wait)" | "(note peek)" => {
            pid != std::process::id() && !process_alive(pid)
        }
        _ => false,
    }
}

/// Whether process `pid` exists on this host (`EPERM`: it exists, someone else's).
pub fn process_alive(pid: u32) -> bool {
    let Ok(p) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 checks existence only.
    let r = unsafe { libc::kill(p, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_id_is_kept() {
        let d = tempfile::tempdir().unwrap();
        let a = host_id(&d.path().join("data"));
        assert!(valid_id(&a), "{a}");
        assert_eq!(host_id(&d.path().join("data")), a);
        assert_ne!(host_id(&d.path().join("other")), a);
    }

    #[test]
    fn dead_holders_of_this_host_only() {
        let me = std::process::id();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let h = |pid: u32, what: &str, id: &str| format!("lab1 pid {pid} ({what}) id {id}");
        assert!(dead_own(
            &h(dead, "note wait", "aa11bb22"),
            "lab1",
            "aa11bb22"
        ));
        assert!(dead_own(
            &h(dead, "note peek", "aa11bb22"),
            "lab1",
            "aa11bb22"
        ));
        assert!(dead_own(&h(dead, "bridge", "aa11bb22"), "lab1", "aa11bb22"));
        assert!(dead_own(&h(me, "bridge", "aa11bb22"), "lab1", "aa11bb22"));
        assert!(!dead_own(
            &h(me, "note wait", "aa11bb22"),
            "lab1",
            "aa11bb22"
        ));
        // Another machine with the same label: never ours.
        assert!(!dead_own(
            &h(dead, "bridge", "cc33dd44"),
            "lab1",
            "aa11bb22"
        ));
        assert!(!dead_own(
            &h(dead, "bridge", "aa11bb22"),
            "lab2",
            "aa11bb22"
        ));
        assert!(!dead_own(&h(dead, "land", "aa11bb22"), "lab1", "aa11bb22"));
        // An older holder without an id: by its label.
        assert!(dead_own(
            &format!("lab1 pid {dead} (note wait)"),
            "lab1",
            "aa11bb22"
        ));
        assert!(!dead_own("lab1 pid x (bridge)", "lab1", "aa11bb22"));
        assert_eq!(
            holder("lab1", "aa11bb22", "note wait"),
            format!("lab1 pid {me} (note wait) id aa11bb22")
        );
    }
}
