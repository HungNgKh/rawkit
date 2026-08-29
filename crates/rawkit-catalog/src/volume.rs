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
    ///
    /// A filesystem UUID when there is one, and the mount point when there is
    /// not. The fallback is not a shrug: tmpfs, overlayfs and container mounts
    /// have no UUID and never will, and refusing them means a photographer whose
    /// library sits on one cannot use the application at all. `is_stable()`
    /// reports the difference, so relink falls back to content hashes for these
    /// rather than trusting a mount point that moves.
    #[cfg(target_os = "linux")]
    pub fn resolve(path: &Path) -> Result<Self, CatalogError> {
        let (device, mount) = mount_for(path)?;
        match uuid_for_device(&device) {
            Ok(uuid) => Ok(VolumeId::Uuid(uuid)),
            Err(_) => Ok(VolumeId::MountPath(mount)),
        }
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

/// The device backing the filesystem that holds `path`, and where it is mounted.
///
/// Longest-prefix match against `/proc/mounts`, because mounts nest: a path
/// under `/home/user/photos` on its own disk matches both `/` and
/// `/home/user/photos`, and only the longer one is its actual filesystem.
///
/// The mount point comes back too, because it is the fallback identity when the
/// device turns out to have no UUID.
#[cfg(target_os = "linux")]
fn mount_for(path: &Path) -> Result<(String, String), CatalogError> {
    // Canonicalise first, or a path through a symlink is matched against the
    // wrong mount — and `..` components would defeat prefix matching entirely.
    let path = path
        .canonicalize()
        .map_err(|e| CatalogError::Io(format!("{}: {e}", path.display())))?;
    let mounts = std::fs::read_to_string("/proc/mounts")
        .map_err(|e| CatalogError::Io(format!("/proc/mounts: {e}")))?;

    let mut best: Option<(String, String)> = None;
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
        if best
            .as_ref()
            .is_none_or(|(_, best_mount)| mount.len() > best_mount.len())
        {
            best = Some((unescape(device), mount));
        }
    }

    best.ok_or_else(|| CatalogError::Io(format!("no mount point contains {}", path.display())))
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
        let (device, mount) = mount_for(Path::new("/tmp")).expect("/tmp is somewhere");
        assert!(!device.is_empty());
        assert!(
            Path::new("/tmp").starts_with(&mount),
            "/tmp is not under its own mount point {mount}"
        );
    }

    #[test]
    fn every_local_path_resolves_to_something() {
        // The property this needs to have, and the one it did not: a filesystem
        // with no UUID is answered rather than refused. It used to be an error,
        // which locked out anyone whose library sits on tmpfs, overlayfs or a
        // container mount — including CI, which is how it was found.
        //
        // Both /tmp and $HOME, because on any given machine either one may or
        // may not carry a UUID and the point is that neither is a failure.
        for path in [std::env::temp_dir(), std::env::var("HOME").unwrap().into()] {
            match VolumeId::resolve(&path) {
                Ok(id @ VolumeId::Uuid(_)) => assert!(id.is_stable()),
                Ok(id @ VolumeId::MountPath(_)) => assert!(
                    !id.is_stable(),
                    "a mount point is not a stable identity and must not claim to be"
                ),
                Ok(other) => panic!("Linux should not produce {other:?}"),
                Err(e) => panic!("{} was refused: {e}", path.display()),
            }
        }
    }
}
