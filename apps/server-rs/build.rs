//! Derive the server version from the repository's `package.json`, which is
//! the single version source the release workflow bumps and the tmux plugin
//! uses to pick a release bundle. Keeping the hello frame in step with it
//! means a mismatch between plugin and binary is visible in the handshake.

use std::fs;
use std::path::Path;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let package_json = Path::new(&manifest_dir).join("../../package.json");
    println!("cargo:rerun-if-changed={}", package_json.display());

    let version = fs::read_to_string(&package_json)
        .ok()
        .and_then(|raw| package_version(&raw))
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION"));
    println!("cargo:rustc-env=OPENSESSIONS_VERSION={version}");
}

/// Extract `"version": "<x>"` without pulling a JSON parser into the build.
fn package_version(raw: &str) -> Option<String> {
    let start = raw.find("\"version\"")?;
    let rest = &raw[start + "\"version\"".len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let version = &rest[..end];
    (!version.is_empty()).then(|| version.to_string())
}
