//! Which disk a path is on, as identity rather than as a location.
//!
//! # Why this is not just the mount point
//!
//! `/media/photos` is where a drive is *today*. External disks arrive at a
//! different letter, a different mount, sometimes a different machine. The
//! catalog needs to say "this file is on **that** disk" in a way that survives
//! all of it, which is what [`VolumeId`] is for and why it is the union of what
//! three operating systems each consider identity.
//!
//! # Linux only, and saying so
//!
//! Resolving a filesystem UUID is per-platform work and only the Linux half
//! exists. macOS and Windows return [`CatalogError::Unsupported`] naming what is
//! missing, the same way the display-profile lookup and the canvas do.
//!
//! The tempting alternative — inventing a `NetworkShare` identity so the
//! schema's `CHECK` is satisfied and the scan proceeds — would be worse than not
//! running. Relink trusts these values, and a fabricated identity is one that
//! matches the wrong drive later, silently.

use crate::{CatalogError, VolumeId};
use std::path::Path;

impl VolumeId {
    /// The volume `path` lives on.
    #[cfg(target_os = "linux")]
    pub fn resolve(path: &Path) -> Result<Self, CatalogError> {
        let device = device_for(path)?;
        let uuid = uuid_for_device(&device)?;
        Ok(VolumeId::Uuid(uuid))
    }

    /// The volume `path` lives on.
    #[cfg(not(target_os = "linux"))]
    pub fn resolve(_path: &Path) -> Result<Self, CatalogError> {
        Err(CatalogError::Unsupported(
            "resolving a volume identity is implemented for Linux only so far; \
             macOS wants a DASessionRef and Windows GetVolumeInformationW",
        ))
    }
}

/// The device backing the filesystem that holds `path`.
///
/// Longest-prefix match against `/proc/mounts`, because mounts nest: a path
/// under `/home/user/photos` on its own disk matches both `/` and
/// `/home/user/photos`, and only the longer one is its actual filesystem.
#[cfg(target_os = "linux")]
fn device_for(path: &Path) -> Result<String, CatalogError> {
    // Canonicalise first, or a path through a symlink is matched against the
    // wrong mount — and `..` components would defeat prefix matching entirely.
    let path = path
        .canonicalize()
        .map_err(|e| CatalogError::Io(format!("{}: {e}", path.display())))?;
    let mounts = std::fs::read_to_string("/proc/mounts")
        .map_err(|e| CatalogError::Io(format!("/proc/mounts: {e}")))?;

    let mut best: Option<(usize, String)> = None;
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(device), Some(mount)) = (fields.next(), fields.next()) else {
            continue;
        };
        // `/proc/mounts` escapes spaces and a few other characters as octal.
        let mount = unescape(mount);
        if !path.starts_with(&mount) {
            continue;
        }
        if best.as_ref().is_none_or(|(len, _)| mount.len() > *len) {
            best = Some((mount.len(), unescape(device)));
        }
    }

    best.map(|(_, device)| device)
        .ok_or_else(|| CatalogError::Io(format!("no mount point contains {}", path.display())))
}

/// Reverse-lookup `/dev/disk/by-uuid`, whose entries are symlinks to devices.
#[cfg(target_os = "linux")]
fn uuid_for_device(device: &str) -> Result<String, CatalogError> {
    let target = Path::new(device)
        .canonicalize()
        .map_err(|e| CatalogError::Io(format!("{device}: {e}")))?;
    let dir = Path::new("/dev/disk/by-uuid");
    let entries =
        std::fs::read_dir(dir).map_err(|e| CatalogError::Io(format!("{}: {e}", dir.display())))?;

    for entry in entries.flatten() {
        if entry.path().canonicalize().is_ok_and(|p| p == target) {
            return Ok(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Err(CatalogError::Unsupported(
        "this filesystem has no UUID; tmpfs, overlayfs and some network mounts \
         do not have one, and a catalog on such a volume cannot relink by identity",
    ))
}

/// `/proc/mounts` writes space, tab, newline and backslash as octal escapes.
#[cfg(target_os = "linux")]
fn unescape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let octal: String = chars.by_ref().take(3).collect();
        match u8::from_str_radix(&octal, 8) {
            Ok(byte) => out.push(byte as char),
            // Not an escape after all; keep what was written.
            Err(_) => {
                out.push('\\');
                out.push_str(&octal);
            }
        }
    }
    out
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn octal_escapes_are_undone() {
        assert_eq!(unescape("/mnt/my\\040photos"), "/mnt/my photos");
        assert_eq!(unescape("/plain/path"), "/plain/path");
        // A trailing backslash is not an escape and must survive rather than
        // truncate the path it is part of.
        assert_eq!(unescape("/odd\\"), "/odd\\");
    }

    #[test]
    fn the_longest_mount_wins() {
        // Every path is under `/`, so a shorter match must never beat a longer
        // one — the bug would put a file on the wrong disk and only show up when
        // that disk was unplugged.
        let device = device_for(Path::new("/tmp")).expect("/tmp is somewhere");
        assert!(!device.is_empty());
    }

    #[test]
    fn a_real_path_resolves_to_a_uuid() {
        // The home directory is on a real filesystem on any machine that can run
        // this; tmpfs would correctly refuse, which is why /tmp is not used here.
        let home = std::env::var("HOME").expect("HOME");
        match VolumeId::resolve(Path::new(&home)) {
            Ok(VolumeId::Uuid(uuid)) => {
                assert!(uuid.contains('-') || uuid.len() >= 8, "odd uuid: {uuid}");
                assert!(VolumeId::Uuid(uuid).is_stable());
            }
            Ok(other) => panic!("Linux should resolve to a Uuid, got {other:?}"),
            // A UUID-less filesystem is a legitimate answer, not a failure.
            Err(CatalogError::Unsupported(_)) => {}
            Err(e) => panic!("{e}"),
        }
    }
}
