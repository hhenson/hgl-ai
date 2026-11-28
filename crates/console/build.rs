// Rebuild when the telemetry page's build output changes: `include_dir!` does not tell
// Cargo which files it read.
fn main() {
    println!("cargo:rerun-if-changed=web/dist");
}
