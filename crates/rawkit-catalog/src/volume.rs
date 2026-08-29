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
//! # The shape of this file
//!
//! Every platform answers the same two questions — *where is this mounted* and
//! *what does the kernel call that volume* — and then makes the same decision
//! about which of those to trust. Only the asking is per-platform; the deciding
//! is [`VolumeId::from_linux_mount`], [`VolumeId::from_macos_volume`] and
//! [`VolumeId::from_windows_root`], which are ordinary functions over ordinary
//! data.
//!
//! That split is the same one `scan_on` makes by taking a `VolumeId` instead of
//! resolving one: push the host-dependent part to the edge, and what remains can
//! be tested on any machine — including the two nobody here has.
//!
//! # When there is no identity to be had
//!
//! tmpfs, overlayfs, container mounts and some network filesystems have no UUID
//! and no serial, and never will. Refusing them would lock out a photographer
//! whose library sits on one, so they resolve to [`VolumeId::MountPath`], whose
//! `is_stable()` is false — relink then falls back to content hashes rather than
//! trusting a mount point that moves.
//!
//! The tempting shortcut — calling such a volume a `NetworkShare` so the
//! schema's `CHECK` is satisfied — would be worse than not running. Relink
//! trusts these values, and a fabricated identity is one that matches the wrong
//! drive later, silently.

use crate::{CatalogError, VolumeId};
use std::path::Path;

impl VolumeId {
    /// The volume `path` lives on.
    ///
    /// A filesystem UUID when there is one, and the mount point when there is
    /// not. See [`VolumeId::from_linux_mount`] for why the fallback is not a
    /// shrug.
    #[cfg(target_os = "linux")]
    pub fn resolve(path: &Path) -> Result<Self, CatalogError> {
        let (device, mount) = mount_for(path)?;
        Ok(VolumeId::from_linux_mount(
            &mount,
            uuid_for_device(&device).ok(),
        ))
    }

    /// The volume `path` lives on.
    ///
    /// `statfs` for the mount point, then `getattrlist` for the volume UUID.
    /// Not DiskArbitration: that means linking a framework and holding a
    /// `DASessionRef` to learn one 16-byte value the kernel will hand over
    /// through a syscall.
    #[cfg(target_os = "macos")]
    pub fn resolve(path: &Path) -> Result<Self, CatalogError> {
        let mount = mount_point(path)?;
        Ok(VolumeId::from_macos_volume(&mount, volume_uuid(&mount)))
    }

    /// The volume `path` lives on.
    ///
    /// `GetVolumePathNameW` for the mount root — which is `C:\` for most
    /// drives, the containing folder for a volume mounted into a directory, and
    /// `\\server\share\` for a UNC path — then `GetVolumeInformationW` for
    /// the serial number.
    #[cfg(windows)]
    pub fn resolve(path: &Path) -> Result<Self, CatalogError> {
        let root = volume_root(path)?;
        Ok(VolumeId::from_windows_root(&root, volume_serial(&root)))
    }

    /// The volume `path` lives on.
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    pub fn resolve(_path: &Path) -> Result<Self, CatalogError> {
        Err(CatalogError::Unsupported(
            "resolving a volume identity is implemented for Linux, macOS and \
             Windows; this platform would need its own mount lookup",
        ))
    }

    /// What a Linux mount amounts to, given the UUID of the device behind it.
    ///
    /// Separate from [`resolve`](Self::resolve) because reading `/proc/mounts`
    /// and deciding what the answer means are different jobs, and only the first
    /// one needs Linux.
    pub fn from_linux_mount(mount: &str, uuid: Option<String>) -> Self {
        match uuid {
            Some(uuid) if !uuid.is_empty() => VolumeId::Uuid(uuid),
            _ => VolumeId::MountPath(mount.to_string()),
        }
    }

    /// What a macOS mount amounts to, given the UUID `getattrlist` reported.
    ///
    /// An all-zero UUID is not an identity. The kernel returns exactly that for
    /// a volume that has none, and storing it would make every such volume look
    /// like every other one — the failure this whole type exists to avoid.
    pub fn from_macos_volume(mount: &str, uuid: Option<[u8; 16]>) -> Self {
        match uuid {
            Some(bytes) if bytes != [0u8; 16] => VolumeId::Uuid(format_uuid(&bytes)),
            _ => VolumeId::MountPath(mount.to_string()),
        }
    }

    /// What a Windows volume root amounts to, given the serial number
    /// `GetVolumeInformationW` reported.
    ///
    /// A UNC root is a share and says so, which is the one place the
    /// `NetworkShare` variant arrives as fact rather than as a guess: on Linux
    /// and macOS an SMB mount looks like any other directory.
    ///
    /// A **zero serial falls back to the mount path**, for the same reason an
    /// all-zero UUID does. Virtual and network filesystems report 0, and 0 as an
    /// identity would match all of them to each other.
    pub fn from_windows_root(root: &str, serial: Option<u32>) -> Self {
        let plain = crate::path::without_verbatim_prefix(root);
        if let Some(share) = unc_identity(&plain) {
            return share;
        }
        match serial {
            Some(serial) if serial != 0 => VolumeId::WindowsSerial(serial),
            _ => VolumeId::MountPath(plain),
        }
    }
}

/// The 8-4-4-4-12 spelling, uppercase, as `diskutil info` prints it.
///
/// Matching that matters: someone comparing a catalog row against what their Mac
/// reports should not have to wonder whether the case is significant.
fn format_uuid(bytes: &[u8; 16]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(36);
    for (i, byte) in bytes.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        let _ = write!(out, "{byte:02X}");
    }
    out
}

/// The share a UNC root names, if it names one.
///
/// `\\?\` and `\\.\` open with the same two backslashes but name local
/// devices, not hosts, so they are not shares. A host with no share after it —
/// `\\nas` — is not one either: there is no volume there to identify.
fn unc_identity(root: &str) -> Option<VolumeId> {
    let rest = root.strip_prefix(r"\\")?;
    if rest.starts_with("?\\") || rest.starts_with(".\\") {
        return None;
    }
    let mut parts = rest.split('\\').filter(|part| !part.is_empty());
    Some(VolumeId::NetworkShare {
        host: parts.next()?.to_string(),
        share: parts.next()?.to_string(),
    })
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

/// Where the filesystem holding `path` is mounted.
///
/// `statfs` rather than a `/proc/mounts` equivalent, because macOS has no such
/// file and the kernel will simply say which mount a path resolved to — no
/// prefix matching, and no chance of picking the shorter of two nested mounts.
#[cfg(target_os = "macos")]
fn mount_point(path: &Path) -> Result<String, CatalogError> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| CatalogError::Io(format!("{}: path contains a NUL byte", path.display())))?;
    let mut info: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is NUL-terminated and outlives the call, and `info` is a
    // zeroed `statfs` the kernel fills in.
    if unsafe { libc::statfs(c_path.as_ptr(), &mut info) } != 0 {
        return Err(CatalogError::Io(format!(
            "{}: {}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: `f_mntonname` is a NUL-terminated path the kernel wrote.
    let mount = unsafe { std::ffi::CStr::from_ptr(info.f_mntonname.as_ptr()) };
    Ok(mount.to_string_lossy().into_owned())
}

/// The volume UUID of the filesystem mounted at `mount`, if it has one.
///
/// `getattrlist` returns a length-prefixed buffer, so the guard that matters is
/// **the returned length**: anything other than the four bytes of the length
/// plus the sixteen of the UUID means the kernel answered a different question
/// than the one this code thinks it asked, and the bytes after it are not a
/// UUID. `attrlist` and the attribute bits come from `libc` rather than being
/// spelled out here, so the one layout this file owns is the small one above.
///
/// `None` on any doubt, and the caller falls back to the mount path.
#[cfg(target_os = "macos")]
fn volume_uuid(mount: &str) -> Option<[u8; 16]> {
    let c_path = std::ffi::CString::new(mount).ok()?;
    let mut request = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        // ATTR_VOL_INFO is not optional: without it the volume bits are ignored.
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_UUID,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut buffer = [0u8; 4 + 16];
    // SAFETY: `c_path` is NUL-terminated, `request` is a fully-initialised
    // `attrlist`, and `buffer` is exactly the size passed alongside it.
    let rc = unsafe {
        libc::getattrlist(
            c_path.as_ptr(),
            &mut request as *mut libc::attrlist as *mut libc::c_void,
            buffer.as_mut_ptr() as *mut libc::c_void,
            buffer.len(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    let returned = u32::from_ne_bytes(buffer[..4].try_into().ok()?);
    if returned as usize != buffer.len() {
        return None;
    }
    buffer[4..].try_into().ok()
}

/// The mount root of the volume holding `path`, without its extended-length
/// prefix.
///
/// Canonicalised first for the same reason Linux does it: a path through a
/// junction or with `..` in it would otherwise be asked about as written.
#[cfg(windows)]
fn volume_root(path: &Path) -> Result<String, CatalogError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;

    let path = path
        .canonicalize()
        .map_err(|e| CatalogError::Io(format!("{}: {e}", path.display())))?;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // The documented maximum for a path this call can return, which is the
    // extended-length limit rather than MAX_PATH — the input may well be one.
    let mut buffer = vec![0u16; 32768];
    // SAFETY: `wide` is NUL-terminated, and `buffer` is writable for the length
    // passed alongside it.
    let ok = unsafe { GetVolumePathNameW(wide.as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32) };
    if ok == 0 {
        return Err(CatalogError::Io(format!(
            "{}: {}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    Ok(crate::path::without_verbatim_prefix(
        &String::from_utf16_lossy(&buffer[..end]),
    ))
}

/// The serial number of the volume mounted at `root`, if it reports one.
///
/// Every out-parameter but the serial is null: the volume label and filesystem
/// name are things this code would only have to ignore, and asking for them
/// means two more buffers that can be sized wrongly.
#[cfg(windows)]
fn volume_serial(root: &str) -> Option<u32> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;

    let wide: Vec<u16> = std::ffi::OsStr::new(root)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut serial: u32 = 0;
    // SAFETY: `wide` is NUL-terminated, `serial` is a valid out-parameter, and
    // every buffer argument is null with a matching zero length.
    let ok = unsafe {
        GetVolumeInformationW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    (ok != 0).then_some(serial)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uuid_is_spelled_the_way_the_platform_spells_it() {
        // The bytes are what `getattrlist` hands back; the string is what
        // `diskutil info` shows for them. Someone checking a catalog row against
        // their own machine compares these two by eye, so the grouping and the
        // case are part of the contract, not cosmetics.
        let bytes = [
            0x1E, 0xC2, 0x2A, 0x4B, 0x33, 0x77, 0x4B, 0x0E, 0xA3, 0x91, 0x5F, 0x1D, 0x2C, 0x88,
            0x90, 0x44,
        ];
        assert_eq!(format_uuid(&bytes), "1EC22A4B-3377-4B0E-A391-5F1D2C889044");
        assert_eq!(format_uuid(&[0u8; 16]).len(), 36);
    }

    #[test]
    fn an_all_zero_uuid_is_not_an_identity() {
        // The kernel returns exactly this for a volume that has none. Storing it
        // would make every such volume match every other one, which is the
        // failure this type exists to prevent.
        let zero = VolumeId::from_macos_volume("/Volumes/Untitled", Some([0u8; 16]));
        assert_eq!(zero, VolumeId::MountPath("/Volumes/Untitled".into()));
        assert!(!zero.is_stable());

        let real = VolumeId::from_macos_volume("/", Some([1u8; 16]));
        assert!(matches!(real, VolumeId::Uuid(_)));
        assert!(real.is_stable());
    }

    #[test]
    fn a_missing_linux_uuid_falls_back_to_the_mount() {
        assert_eq!(
            VolumeId::from_linux_mount("/mnt/scratch", None),
            VolumeId::MountPath("/mnt/scratch".into())
        );
        assert_eq!(
            VolumeId::from_linux_mount("/", Some("abcd-1234".into())),
            VolumeId::Uuid("abcd-1234".into())
        );
    }

    #[test]
    fn a_share_is_told_apart_from_a_drive_and_from_a_device() {
        let share = VolumeId::NetworkShare {
            host: "nas".into(),
            share: "photos".into(),
        };
        assert_eq!(unc_identity(r"\\nas\photos\"), Some(share.clone()));
        assert_eq!(unc_identity(r"\\nas\photos"), Some(share));

        // A drive is not a share.
        assert_eq!(unc_identity(r"C:\"), None);
        // `\\?\` and `\\.\` name local devices, not hosts.
        assert_eq!(unc_identity(r"\\?\C:\"), None);
        assert_eq!(unc_identity(r"\\.\PhysicalDrive0"), None);
        // A host with no share behind it identifies no volume.
        assert_eq!(unc_identity(r"\\nas"), None);
        assert_eq!(unc_identity(r"\\"), None);
    }

    #[test]
    fn a_zero_serial_is_not_an_identity_either() {
        // Virtual and network filesystems report 0, so trusting it would match
        // all of them to each other — the same mistake as an all-zero UUID.
        assert_eq!(
            VolumeId::from_windows_root(r"C:\", Some(0)),
            VolumeId::MountPath(r"C:\".into())
        );
        assert_eq!(
            VolumeId::from_windows_root(r"C:\", None),
            VolumeId::MountPath(r"C:\".into())
        );
        assert_eq!(
            VolumeId::from_windows_root(r"\\?\C:\", Some(0xDEAD_BEEF)),
            VolumeId::WindowsSerial(0xDEAD_BEEF)
        );
        // A share is a share whatever serial came back with it: the serial of a
        // mapped drive belongs to the server, and two machines mapping the same
        // share must agree on what it is.
        assert_eq!(
            VolumeId::from_windows_root(r"\\?\UNC\nas\photos\", Some(0xDEAD_BEEF)),
            VolumeId::NetworkShare {
                host: "nas".into(),
                share: "photos".into(),
            }
        );
    }

    #[test]
    fn every_local_path_resolves_to_something() {
        // The property this needs to have, and the one it did not: a filesystem
        // with no stable identity is answered rather than refused. It used to be
        // an error, which locked out anyone whose library sits on tmpfs,
        // overlayfs or a container mount — including CI, which is how it was
        // found.
        //
        // Two paths, because on any given machine either may or may not carry an
        // identity and the point is that neither is a failure.
        for path in [std::env::temp_dir(), std::env::current_dir().unwrap()] {
            match VolumeId::resolve(&path) {
                Ok(id) => assert_eq!(
                    id.is_stable(),
                    !matches!(id, VolumeId::MountPath(_) | VolumeId::NetworkShare { .. }),
                    "{id:?} disagrees with itself about whether it is stable"
                ),
                Err(e) => panic!("{} was refused: {e}", path.display()),
            }
        }
    }

    /// The one assertion that has to know what machine it is on.
    ///
    /// AGENTS.md says tests must not assume the host filesystem, and this bends
    /// that deliberately. The rule is there so a test does not depend on a
    /// *quirk*; a boot volume having an identity is not a quirk, it is true of
    /// every real Mac and every real PC. And without it there is no proof at all
    /// that the FFI reads real bytes: a wrong buffer layout or a bad attribute
    /// bit does not fail loudly, it falls back to the mount path and looks
    /// exactly like success. The dev box is Linux, so CI is the only place this
    /// can ever run.
    #[test]
    #[cfg(any(target_os = "macos", windows))]
    fn a_boot_volume_has_a_real_identity() {
        #[cfg(target_os = "macos")]
        let (root, expected) = (Path::new("/"), "a volume UUID from getattrlist");
        #[cfg(windows)]
        let (root, expected) = (Path::new(r"C:\"), "a serial from GetVolumeInformationW");

        let id = VolumeId::resolve(root).expect("the boot volume resolves");
        assert!(
            id.is_stable(),
            "expected {expected} for {}, got {id:?} — the fallback fired, which \
             means the system call did not answer the question it was asked",
            root.display()
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
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
}
