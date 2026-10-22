//! Injects a build identifier so a client and a daemon built from different
//! revisions can notice each other.
//!
//! Order: `XCOM_BUILD` from the environment (build scripts pass the host's
//! `git describe`, including inside the container builds without git), then
//! `git describe` directly, then the crate version.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=XCOM_BUILD");

    let id = std::env::var("XCOM_BUILD")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            let out = std::process::Command::new("git")
                .args(["describe", "--always", "--dirty"])
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (!s.is_empty()).then_some(s)
        })
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    println!("cargo:rustc-env=XCOM_BUILD={id}");
}
