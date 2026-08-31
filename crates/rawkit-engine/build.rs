//! The renderer's identity, derived from its own source.
//!
//! # Why a build script rather than a constant
//!
//! Cached previews are keyed on the edit that produced them, which answers "has
//! the user changed anything" and not "would this build still produce these
//! pixels". Every engine change makes every stored preview a lie, and a lie that
//! looks exactly like the truth: the thumbnail is simply wrong, and nothing
//! about it says so.
//!
//! A hand-maintained version number is the usual answer and it relies on
//! remembering. Two commits changed pixels on the day this was written — one in
//! WGSL and one in Rust — so anything narrower than *all* of the engine's source
//! would have missed half of them.
//!
//! The cost is honest and worth naming: a comment-only edit to this crate
//! rebuilds every preview in every library. Rebuilding is cheap and automatic;
//! showing somebody a stale thumbnail is neither.
//!
//! # Determinism
//!
//! The digest must be the same on Linux, macOS and Windows for the same source,
//! or a catalog carried between them would rebuild previews it already has. Two
//! things threaten that and both are handled: the order files are visited, which
//! is sorted rather than whatever the filesystem offers, and carriage returns,
//! which a Windows checkout may add and which are stripped before hashing.

use std::path::{Path, PathBuf};

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("a manifest directory"));
    let mut sources = Vec::new();
    for dir in ["src", "shaders"] {
        collect(&root.join(dir), &mut sources);
    }
    // Sorted, because a digest that depends on the order a directory happens to
    // be read in is a digest that differs between two machines with identical
    // source.
    sources.sort();

    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for path in &sources {
        println!("cargo:rerun-if-changed={}", path.display());
        // The path is hashed as well as the contents, so moving code between
        // files changes the digest even when the bytes are the same overall.
        let name = path.strip_prefix(&root).unwrap_or(path);
        fold(
            &mut hash,
            name.to_string_lossy().replace('\\', "/").as_bytes(),
        );
        let text = std::fs::read(path).unwrap_or_default();
        let normalised: Vec<u8> = text.into_iter().filter(|&b| b != b'\r').collect();
        fold(&mut hash, &normalised);
    }
    println!("cargo:rustc-env=RAWKIT_ENGINE_DIGEST={hash:016x}");
}

/// FNV-1a. Chosen because a build script should not need a dependency to
/// answer a question this small, and because nothing here is adversarial —
/// the digest exists to notice change, not to resist forgery.
fn fold(hash: &mut u64, bytes: &[u8]) {
    for &byte in bytes {
        *hash ^= byte as u64;
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

fn collect(dir: &Path, into: &mut Vec<PathBuf>) {
    println!("cargo:rerun-if-changed={}", dir.display());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, into);
        } else if path
            .extension()
            .is_some_and(|e| e == "rs" || e == "wgsl" || e == "md")
        {
            into.push(path);
        }
    }
}
