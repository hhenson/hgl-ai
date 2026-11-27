//! Implementor branch names: `<branch_prefix>q<issue>-<slug>` (docs/work.md, docs/design.md
//! §5), such as `codex/q980-fix-the-resolver`.

/// Longest slug, in characters.
pub const SLUG_MAX: usize = 40;

/// The slug of an issue title: lowercased, `[a-z0-9-]`, at most [`SLUG_MAX`] characters;
/// every word is kept (`[T; N] array literals` is `t-n-array-literals`). An empty result
/// is `task`.
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.lines().next().unwrap_or("").chars() {
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

/// The slug of a note's first body line, used when the issue's title is unknown: as
/// [`slug`], after dropping a leading `[runtime] Head:` (PROTO-11) and `repo#N` references.
pub fn note_slug(line: &str) -> String {
    let mut s = line.lines().next().unwrap_or("").trim();
    if let Some(rest) = s.strip_prefix('[')
        && let Some(i) = rest.find(']')
    {
        s = rest[i + 1..].trim_start();
        if let Some((head, tail)) = s.split_once(':')
            && !head.is_empty()
            && head.split_whitespace().count() <= 2
            && head
                .chars()
                .all(|c| c.is_alphanumeric() || c == ' ' || c == '-')
        {
            s = tail;
        }
    }
    let words: Vec<&str> = s.split_whitespace().filter(|w| !is_reference(w)).collect();
    slug(&words.join(" "))
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

/// `<prefix>q<issue>-<slug>`.
pub fn branch_name(prefix: &str, issue: u64, slug: &str) -> String {
    format!("{prefix}q{issue}-{slug}")
}

/// The starts of a task's branch names: `<prefix>q<issue>-`, and the older
/// `<prefix><issue>-`, so a branch pushed under either is resumed.
pub fn branch_starts(prefix: &str, issue: u64) -> [String; 2] {
    [format!("{prefix}q{issue}-"), format!("{prefix}{issue}-")]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_slugs() {
        assert_eq!(
            note_slug("[claude] Assign: hgl-compiler#980 Fix the generic resolver crash"),
            "fix-the-generic-resolver-crash"
        );
        assert_eq!(note_slug("[claude] Assign: hgl-compiler#980"), "task");
        assert_eq!(
            note_slug("Fix: keep colons without a runtime tag"),
            "fix-keep-colons-without-a-runtime-tag"
        );
        assert_eq!(
            note_slug("Class: standard (docs/roles.md §3)\nFollow hgl-ai#27."),
            "class-standard-docs-roles-md-3"
        );
    }

    #[test]
    fn title_slugs_keep_every_word() {
        assert_eq!(slug("[T; N] array literals"), "t-n-array-literals");
        assert_eq!(
            slug("[P-F1] Parser: reject a trailing comma"),
            "p-f1-parser-reject-a-trailing-comma"
        );
        assert_eq!(
            slug("[claude] Assign: a title that looks like a note"),
            "claude-assign-a-title-that-looks-like-a"
        );
        assert_eq!(slug("Fix hgl-compiler#980"), "fix-hgl-compiler-980");
        assert_eq!(
            slug("Add `foo::bar` support!!!\nsecond line"),
            "add-foo-bar-support"
        );
        for empty in ["", "   ", "!!! ??? ...", "🚀🔥", "—", "[]: #"] {
            assert_eq!(slug(empty), "task", "{empty:?}");
        }
        assert_eq!(slug("Ünïcode — dashes ok 🚀"), "n-code-dashes-ok");
        let long = slug(
            "a very long title that keeps going well past the forty character limit of a slug",
        );
        assert!(long.len() <= SLUG_MAX, "{long}");
        assert_eq!(long, "a-very-long-title-that-keeps-going-well");
        let unbroken = slug(&"x".repeat(60));
        assert_eq!(unbroken.len(), SLUG_MAX);
        for s in [long, unbroken] {
            assert!(
                s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            );
        }
    }

    #[test]
    fn branches() {
        assert_eq!(branch_name("codex/", 980, "fix-it"), "codex/q980-fix-it");
        assert_eq!(branch_starts("codex/", 98), ["codex/q98-", "codex/98-"]);
    }
}
