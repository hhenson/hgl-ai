//! Implementor branch names: `<branch_prefix><issue>-<slug>` (docs/design.md §5).

/// Longest slug, in characters.
pub const SLUG_MAX: usize = 40;

/// The slug of a title or of a note's first body line: lowercased, `[a-z0-9-]`, at most
/// [`SLUG_MAX`] characters. A leading `[runtime] Action:` (PROTO-11) and `repo#N`
/// references are dropped; an empty result is `task`.
pub fn slug(title: &str) -> String {
    let mut s = title.lines().next().unwrap_or("").trim();
    if let Some(rest) = s.strip_prefix('[')
        && let Some(i) = rest.find(']')
    {
        s = rest[i + 1..].trim_start();
        if let Some((head, tail)) = s.split_once(':')
            && !head.is_empty()
            && head.split_whitespace().count() <= 2
            && head.chars().all(|c| c.is_alphanumeric() || c == ' ' || c == '-')
        {
            s = tail;
        }
    }
    let words: Vec<&str> = s.split_whitespace().filter(|w| !is_reference(w)).collect();
    let mut out = String::new();
    for c in words.join(" ").chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    if out.len() > SLUG_MAX {
        out.truncate(SLUG_MAX);
        if let Some(i) = out.rfind('-').filter(|i| *i >= SLUG_MAX / 2) {
            out.truncate(i);
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "task".into() } else { out }
}

/// `hgl-compiler#980`, `#980`, `owner/repo#12,`
fn is_reference(w: &str) -> bool {
    match w.split_once('#') {
        Some((_, n)) => {
            let digits = n.trim_end_matches(|c: char| !c.is_ascii_digit());
            !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

/// `<prefix><issue>-<slug>`.
pub fn branch_name(prefix: &str, issue: u64, title: &str) -> String {
    format!("{prefix}{issue}-{}", slug(title))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slug("[claude] Assign: hgl-compiler#980 Fix the generic resolver crash"), "fix-the-generic-resolver-crash");
        assert_eq!(slug("Add `foo::bar` support!!!\nsecond line"), "add-foo-bar-support");
        assert_eq!(slug("[claude] Assign: hgl-compiler#980"), "task");
        assert_eq!(slug(""), "task");
        assert_eq!(slug("Fix: keep colons without a runtime tag"), "fix-keep-colons-without-a-runtime-tag");
        assert_eq!(slug("Ünïcode — dashes ok"), "n-code-dashes-ok");
        let long = slug("a very long title that keeps going well past the forty character limit of a slug");
        assert!(long.len() <= SLUG_MAX, "{long}");
        assert!(!long.ends_with('-'));
        assert_eq!(long, "a-very-long-title-that-keeps-going-well");
        let unbroken = slug(&"x".repeat(60));
        assert_eq!(unbroken.len(), SLUG_MAX);
        for s in [long, unbroken] {
            assert!(s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        }
    }

    #[test]
    fn branches() {
        assert_eq!(branch_name("codex/", 980, "[codex] Assign: Fix it"), "codex/980-fix-it");
    }
}
