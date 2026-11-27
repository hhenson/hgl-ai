//! Secret files: read at send time, never kept.

use tokio::io::AsyncReadExt;

/// Bound on a secret file.
const MAX: u64 = 64 << 10;

/// The trimmed content of the secret file `path` (`~/` expanded). Refused when it is
/// empty or, on unix, when group or others may access it. The reason names the file,
/// never its content.
pub(crate) async fn read(path: &str) -> Result<String, String> {
    let p = hgl_ai_config::expand(path).map_err(|e| format!("{path}: {e}"))?;
    let shown = p.display();
    let file = tokio::fs::File::open(&p)
        .await
        .map_err(|e| format!("{shown}: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = file
            .metadata()
            .await
            .map_err(|e| format!("{shown}: {e}"))?
            .permissions()
            .mode()
            & 0o777;
        if mode & 0o077 != 0 {
            return Err(format!(
                "{shown}: mode {mode:04o} is open to group or others; a secret needs 0600"
            ));
        }
    }
    let mut text = String::new();
    file.take(MAX)
        .read_to_string(&mut text)
        .await
        .map_err(|e| format!("{shown}: {e}"))?;
    let text = text.trim();
    if text.is_empty() {
        return Err(format!("{shown}: empty"));
    }
    Ok(text.to_string())
}
