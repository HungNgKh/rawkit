//! Where a file lives, in a form two machines can agree on.
//!
//! # The problem this exists for
//!
//! A catalog remembers files by path, and "the same path" means different things
//! on each operating system:
//!
//! - **Linux** filenames are bytes. `Photo.ARW` and `photo.arw` are two files,
//!   and two spellings of an accented character are two files.
//! - **Windows** is case-insensitive and writes `\`, so `C:\Photos\A.ARW` and
//!   `c:/photos/a.arw` are one file with four spellings.
//! - **macOS** is case-insensitive by default *and* normalisation-sensitive in a
//!   way that bites: `é` can be one code point or two, HFS+ stored the
//!   decomposed form, and a filename that came from a Finder copy can differ
//!   byte-for-byte from the same name typed in a terminal.
//!
//! Get this wrong and the symptom is not an error. It is the same photo
//! appearing twice in the library, or a relink that cannot find a file sitting
//! right there.
//!
//! # Two strings, not one
//!
//! [`CatalogPath`] keeps what the operating system gave it *and* a key derived
//! for comparison. Opening a file uses the first; finding it in the catalog uses
//! the second. Storing only the normalised form would be the tempting
//! simplification and would eventually fail to open a file whose real name is
//! not the normalised one.
//!
//! # Why the convention is a parameter and not a `cfg`
//!
//! [`PathConvention::host`] is the only thing here that knows which OS this is.
//! The rules themselves are data, so all three are exercised by the tests on
//! whichever machine runs them — a rule this subtle should not be testable only
//! on the platform it applies to.

use std::path::Path;
use unicode_normalization::UnicodeNormalization;

/// How a filesystem decides whether two names are the same file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathConvention {
    /// Bytes are identity: Linux, and macOS volumes formatted case-sensitively.
    Exact,
    /// Case-insensitive, no normalisation: Windows.
    CaseInsensitive,
    /// Case-insensitive and Unicode-normalising: macOS by default.
    ///
    /// Normalising to NFC rather than the NFD that HFS+ stored, because NFC is
    /// what the rest of the world produces and both open the same file.
    CaseInsensitiveNormalised,
}

impl PathConvention {
    /// What this machine's filesystem does, by default.
    ///
    /// A *default*, and deliberately not a probe: a case-sensitive APFS volume
    /// or a case-insensitive ext4 mount both exist and neither is detectable
    /// without touching the filesystem. Being wrong here costs a duplicate or a
    /// missed match, not data — and per-volume detection belongs with the volume
    /// record, once there is one.
    pub const fn host() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::CaseInsensitiveNormalised
        }
        #[cfg(target_os = "windows")]
        {
            Self::CaseInsensitive
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Self::Exact
        }
    }

    fn key_for(self, path: &str) -> String {
        match self {
            Self::Exact => path.to_string(),
            // ASCII-only folding, which is what NTFS and APFS actually do for
            // the characters that matter here. Full Unicode case folding would
            // claim more than the filesystems deliver and would merge names they
            // keep apart.
            Self::CaseInsensitive => path.to_ascii_lowercase(),
            Self::CaseInsensitiveNormalised => path.nfc().collect::<String>().to_ascii_lowercase(),
        }
    }
}

/// Undo Windows' extended-length prefix.
///
/// `canonicalize` returns `\\?\C:\...`, and both `scan` and the volume
/// resolver canonicalise before they store anything — so without this a Windows
/// catalog would hold paths beginning `//?/`, and `\\?\` **disables** Windows'
/// path parsing, forward slashes included. The stored path would look plausible
/// and open nothing. Rust's `std` puts the prefix back on its own when a path is
/// long enough to need it, so the plain spelling is the one worth keeping.
///
/// Idempotent, and a no-op on every path that does not carry the prefix, which
/// is every path on Linux and macOS.
pub(crate) fn without_verbatim_prefix(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        path.to_string()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// Linux filenames are bytes and need not be UTF-8. Refusing beats mangling:
    /// a lossy conversion produces a path that looks fine in the catalog and
    /// cannot open the file, which is the failure this module exists to prevent.
    #[error("{0} is not valid UTF-8; rawkit cannot catalog it")]
    NotUtf8(String),
}

/// A file's location, as stored and as compared.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CatalogPath {
    stored: String,
    key: String,
}

impl CatalogPath {
    /// Take a path from the operating system, under a given convention.
    ///
    /// Separators become `/` regardless of platform, so a catalog carried
    /// between machines reads the same and comparison never has to consider
    /// which slash a path was written with. Windows accepts `/` everywhere it
    /// accepts `\`, so this costs nothing to reverse — everywhere except behind
    /// an extended-length prefix, which is why that comes off first.
    pub fn new(path: &Path, convention: PathConvention) -> Result<Self, PathError> {
        let stored = without_verbatim_prefix(
            path.to_str()
                .ok_or_else(|| PathError::NotUtf8(path.to_string_lossy().into_owned()))?,
        )
        .replace('\\', "/");
        Ok(Self {
            key: convention.key_for(&stored),
            stored,
        })
    }

    /// Under this machine's convention.
    pub fn host(path: &Path) -> Result<Self, PathError> {
        Self::new(path, PathConvention::host())
    }

    /// What to open. As the operating system spelled it, bar separators.
    pub fn stored(&self) -> &str {
        &self.stored
    }

    /// What to compare and index on.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Whether two paths name the same file under `convention`.
    ///
    /// Takes the convention rather than comparing keys directly, because two
    /// `CatalogPath`s built under different conventions have incomparable keys —
    /// which is exactly what happens to a catalog carried from a Mac to a Linux
    /// box, and is why the convention belongs in the catalog beside the paths.
    pub fn same_file(a: &Path, b: &Path, convention: PathConvention) -> Result<bool, PathError> {
        Ok(Self::new(a, convention)?.key == Self::new(b, convention)?.key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn key(path: &str, convention: PathConvention) -> String {
        CatalogPath::new(&PathBuf::from(path), convention)
            .expect("valid utf-8")
            .key()
            .to_string()
    }

    #[test]
    fn the_extended_length_prefix_comes_off() {
        // `canonicalize` produces these, and `scan` canonicalises before it
        // resolves. Left on, every root would start with two backslashes and be
        // mistaken for a share.
        assert_eq!(without_verbatim_prefix(r"\\?\C:\"), r"C:\");
        assert_eq!(
            without_verbatim_prefix(r"\\?\UNC\nas\photos\"),
            r"\\nas\photos\"
        );
        assert_eq!(without_verbatim_prefix(r"C:\"), r"C:\");
        assert_eq!(without_verbatim_prefix(r"\\nas\photos\"), r"\\nas\photos\");
        // Applied twice on the way through `CatalogPath::new` and by `from_windows_root`, so it has to be
        // idempotent rather than merely correct once.
        assert_eq!(
            without_verbatim_prefix(&without_verbatim_prefix(r"\\?\C:\")),
            r"C:\"
        );
    }

    #[test]
    fn separators_are_one_shape_everywhere() {
        let windows = CatalogPath::new(
            &PathBuf::from(r"C:\Photos\2026\DSC00881.ARW"),
            PathConvention::Exact,
        )
        .unwrap();
        assert_eq!(windows.stored(), "C:/Photos/2026/DSC00881.ARW");
    }

    #[test]
    fn a_canonicalised_windows_path_is_stored_as_one_that_opens() {
        // The bug this prevents: `scan` canonicalises its root, Windows returns
        // the extended-length form, and the catalog ends up holding `//?/C:/...`
        // — which looks like a path and is not one, because `\\?\` turns off the
        // parsing that would have accepted those forward slashes.
        let stored = CatalogPath::new(
            &PathBuf::from(r"\\?\C:\Photos\2026\DSC00881.ARW"),
            PathConvention::Exact,
        )
        .unwrap();
        assert_eq!(stored.stored(), "C:/Photos/2026/DSC00881.ARW");

        // A share keeps its two leading slashes; it is the `?\UNC\` in the
        // middle that has to go.
        let share = CatalogPath::new(
            &PathBuf::from(r"\\?\UNC\nas\photos\a.ARW"),
            PathConvention::Exact,
        )
        .unwrap();
        assert_eq!(share.stored(), "//nas/photos/a.ARW");
    }

    #[test]
    fn case_matters_on_linux_and_not_on_windows() {
        // The same two names, and the right answer differs by platform. This is
        // the whole reason the convention is carried rather than assumed.
        assert_ne!(
            key("/photos/A.ARW", PathConvention::Exact),
            key("/photos/a.arw", PathConvention::Exact),
            "Linux keeps these apart, and merging them would lose a file"
        );
        assert_eq!(
            key(r"C:\Photos\A.ARW", PathConvention::CaseInsensitive),
            key("c:/photos/a.arw", PathConvention::CaseInsensitive),
            "Windows says these are one file, and treating them as two duplicates it"
        );
    }

    #[test]
    fn macos_sees_through_unicode_normalisation() {
        // "café.arw", composed and decomposed. Finder and a terminal can produce
        // either for the same file, and on Linux they really are two names.
        let composed = "/photos/caf\u{e9}.arw";
        let decomposed = "/photos/cafe\u{301}.arw";
        assert_ne!(composed, decomposed, "the inputs differ byte for byte");

        assert_eq!(
            key(composed, PathConvention::CaseInsensitiveNormalised),
            key(decomposed, PathConvention::CaseInsensitiveNormalised),
            "macOS opens one file from both spellings; the catalog must agree"
        );
        assert_ne!(
            key(composed, PathConvention::Exact),
            key(decomposed, PathConvention::Exact),
            "on Linux these are two files and normalising would merge them"
        );
        assert_ne!(
            key(composed, PathConvention::CaseInsensitive),
            key(decomposed, PathConvention::CaseInsensitive),
            "Windows folds case but not normalisation"
        );
    }

    #[test]
    fn the_original_spelling_survives_normalisation() {
        // The key is for finding; the stored path is for opening. A file whose
        // real name is decomposed must still be openable after cataloguing.
        let decomposed = "/photos/cafe\u{301}.arw";
        let path = CatalogPath::new(
            &PathBuf::from(decomposed),
            PathConvention::CaseInsensitiveNormalised,
        )
        .unwrap();
        assert_eq!(
            path.stored(),
            decomposed,
            "storing the normalised form would eventually fail to open the file"
        );
        assert_ne!(path.stored(), path.key());
    }

    #[test]
    fn same_file_answers_per_convention() {
        let a = PathBuf::from("/photos/Sunset.ARW");
        let b = PathBuf::from("/photos/sunset.arw");
        assert!(!CatalogPath::same_file(&a, &b, PathConvention::Exact).unwrap());
        assert!(CatalogPath::same_file(&a, &b, PathConvention::CaseInsensitive).unwrap());
    }

    #[test]
    #[cfg(unix)]
    fn a_filename_that_is_not_utf8_is_refused_rather_than_mangled() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        // Legal on Linux, and lossy conversion would turn it into a path that
        // looks fine in the catalog and cannot open the file.
        let raw = OsStr::from_bytes(b"/photos/\xff\xfe.arw");
        let result = CatalogPath::new(Path::new(raw), PathConvention::Exact);
        assert!(matches!(result, Err(PathError::NotUtf8(_))));
    }

    #[test]
    fn the_host_convention_is_one_of_the_three() {
        // Not an assertion about which — that differs per runner. It is an
        // assertion that `host` returns something the rules understand, which is
        // what a missing `cfg` arm would break.
        let host = PathConvention::host();
        assert_eq!(key("/photos/a.arw", host), key("/photos/a.arw", host));
    }
}
