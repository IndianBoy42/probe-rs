//! Locates the workspace-built `probe-rs` server binary and exposes it to
//! `include_bytes!` so the pyprobe-rs extension embeds a schema-locked
//! server (the RPC protocol is version-unstable by design).
//!
//! Only active under the `bundled-server` feature. The binary must be built
//! beforehand — `just build-serve-bin` — because a nested `cargo build` from
//! build.rs would deadlock on the workspace build lock.

use sha2::{Digest, Sha256};
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_BUNDLED_SERVER").is_none() {
        return;
    }
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let target = manifest
        .parent()
        .expect("crate dir has a workspace parent")
        .join("target");
    // Preferred: the stripped `server` profile; fallback: a plain release build.
    let candidates = [
        target.join("server").join("probe-rs"),
        target.join("release").join("probe-rs"),
    ];
    for candidate in &candidates {
        // A candidate appearing or changing later must retrigger this build.
        println!("cargo:rerun-if-changed={}", candidate.display());
    }
    let placeholder = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("empty-server-bin");
    match candidates.iter().find(|p| p.is_file()) {
        Some(bin) => {
            let bytes = std::fs::read(bin).expect("read server binary");
            let digest = Sha256::digest(&bytes);
            let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
            println!("cargo:rustc-env=PYPROBE_RS_EMBED={}", bin.display());
            println!("cargo:rustc-env=PYPROBE_RS_EMBED_SHA={hex}");
        }
        None => {
            // Compile-compatible placeholder: empty bytes make the runtime
            // treat the embed as absent and fall back to workspace resolution.
            std::fs::write(&placeholder, b"").expect("write placeholder");
            println!("cargo:rustc-env=PYPROBE_RS_EMBED={}", placeholder.display());
            println!("cargo:rustc-env=PYPROBE_RS_EMBED_SHA={}", "0".repeat(64));
        }
    }
}
