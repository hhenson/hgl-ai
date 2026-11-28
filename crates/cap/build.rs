use std::process::Command;

fn main() {
    let probe = |tool: &str| match Command::new(tool).arg("--version").output() {
        Ok(output) => format!(
            "{tool}: exit {} stdout {:?} stderr {:?}",
            output.status,
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => format!("{tool}: {error}"),
    };
    panic!("Node/npm probe: {}; {}", probe("node"), probe("npm"));
}
