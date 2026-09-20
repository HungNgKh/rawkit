//! Turning what you decided into photographs.
//!
//! # Why this is its own crate
//!
//! It is the composition layer: the catalog says which photographs and what was
//! decided about them, `rawkit-decode` and `rawkit-engine` turn one into pixels,
//! `rawkit-export` turns those into a file. None of those four wants to know
//! about the other three, and the code that joins them is not big — it is just
//! the only code that has an opinion about *which* photographs and *whether to
//! overwrite*, which is precisely the part that must not be written twice.
//!
//! It used to live inside the command-line binary, where the window could not
//! reach it. Two implementations of "skip a file that already exists" is two
//! chances to get it wrong, and only one of them would have had the test.
//!
//! # The loop this closes
//!
//! Everything before this records decisions: a scan puts files in a catalog, a
//! cull marks them, the shell stores an `EditState` per image. And none of it
//! could leave the application — `rawkit render` takes a RAW path and renders it
//! *as shot*, because it has never heard of a catalog. So an edit was a thing you
//! could make and look at and never receive.
//!
//! This reads the stored edit and renders with it. Same renderer, same output
//! transform, same embedded profile as every other file this project writes.
//!
//! # A delivery is always level zero
//!
//! This crate composes two things from the same stages — a delivery and a
//! [preview](previews) — and the difference between them is this section. What
//! follows is about deliveries.
//!
//! A preview is rendered from a reduced mosaic and says so; an export is not.
//! The pyramid's averaging softens the edge of a blown highlight, which is
//! acceptable in something you look at and not in something you deliver. There
//! is no option for this, because there is no defensible reason to want one.
//!
//! # It will not overwrite
//!
//! A file that already exists is skipped and counted, unless the caller says
//! otherwise. Exports are the one thing here that writes outside the catalog's
//! own directory, and silently replacing somebody's file is the kind of thing an
//! application gets to do exactly once.

pub mod previews;

use anyhow::{bail, Context, Result};
use rawkit_catalog::cull::{self, Filter, Flagged};
use rawkit_catalog::db::Catalog;
use rawkit_editstate::EditState;
use rawkit_engine::{render::DEFAULT_TILE, BayerPhase, Frame, Gpu, Output, Renderer};

/// Re-exported so a caller configuring an export does not also have to name the
/// engine: the setting is part of this crate's vocabulary, not the renderer's.
pub use rawkit_engine::sharpen::OutputSharpening;
/// Likewise: which file an export makes is part of configuring one.
pub use rawkit_export::Format;
use std::path::Path;

/// Quality for a delivered file. Higher than a preview's 85: this one is looked
/// at closely, printed, or sent to somebody.
const EXPORT_QUALITY: u8 = 92;

/// Which photographs to write.
///
/// Two cases, not four. "Everything", "the picks" and "three stars or better"
/// are the same kind of question asked with different terms, and that question
/// is [`Filter`] — the one the window narrows a cull with. Keeping them as
/// separate variants meant this file re-deciding what a pick is, in Rust, while
/// the library decided it in SQL; the two agreed until one of them learnt
/// something, and then an interface could show a set it would not export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Everything the filter admits — the whole library, for a filter that
    /// narrows nothing.
    Matching(Filter),
    /// One image, by its catalog id. Not expressible as a filter: an id is not
    /// a judgement, and an export of *this frame* must not depend on how it was
    /// judged.
    Image(i64),
}

impl Selection {
    /// Every present photograph.
    pub fn all() -> Self {
        Selection::Matching(Filter::default())
    }

    /// What survived a cull.
    pub fn picks() -> Self {
        Selection::Matching(Filter::flagged(Flagged::Pick))
    }

    /// At least this many stars.
    pub fn rated(stars: u8) -> Self {
        Selection::Matching(Filter::rated(stars))
    }
}

#[derive(Debug, Default)]
pub struct ExportReport {
    pub written: usize,
    pub skipped: usize,
    pub bytes: u64,
    pub failed: Vec<(String, String)>,
}

/// One photograph and the edit it is to be rendered with.
pub struct Chosen {
    pub image: cull::LibraryImage,
    pub state: EditState,
    /// The camera profile this body has been given, if any. Resolved here
    /// rather than at render time so that a file written from the terminal and
    /// the same photograph on screen cannot disagree about colour.
    pub profile: Option<rawkit_engine::CameraProfile>,
}

/// Where the files go.
pub enum Destination {
    /// A folder. Each photograph is named after its own file, as a JPEG — the
    /// only sane naming for a batch, because the alternative is asking a
    /// question once per photograph.
    Folder(std::path::PathBuf),
    /// One exact path, with the format taken from its extension. Refused for
    /// more than one photograph: a single name cannot hold a set, and quietly
    /// numbering them would invent a convention nobody asked for.
    File(std::path::PathBuf),
}

impl Destination {
    fn parent(&self) -> &Path {
        match self {
            Destination::Folder(dir) => dir,
            // A file with no parent is a bare name in the working directory,
            // which is somewhere, and `check_destination` can say so.
            Destination::File(path) => path.parent().unwrap_or(Path::new(".")),
        }
    }
}

/// Which photographs the selection names, and what was decided about each.
///
/// Separate from [`write`] because it is the only half that touches the catalog,
/// and the window holds its library behind a lock that a whole export must not
/// keep. Reading first and writing from a list is also what makes the export
/// consistent: photographs cannot be renamed or re-edited out from under it
/// halfway through.
pub fn gather(catalog: &Catalog, selection: Selection) -> Result<Vec<Chosen>> {
    let mut chosen = Vec::new();
    let mut parsed: std::collections::HashMap<String, Option<rawkit_engine::CameraProfile>> =
        std::collections::HashMap::new();
    // The selection is resolved by the catalog, in one query, rather than by
    // walking the library and asking about each judgement here. That is what
    // makes this and the window agree about what they are looking at.
    let images = match &selection {
        Selection::Matching(filter) => cull::sequence(catalog, filter)?,
        Selection::Image(id) => cull::sequence(catalog, &Filter::default())?
            .into_iter()
            .filter(|image| image.id == *id)
            .collect(),
    };
    for image in images {
        // The edit as it stands, or as shot for a photograph nobody has touched.
        let state = rawkit_catalog::edits::latest(catalog, image.id)?
            .map(|(_, state)| state)
            .unwrap_or_default();
        // One parse per profile, not per photograph: a batch is usually one
        // body, and these files run to hundreds of kilobytes.
        let profile = match rawkit_catalog::profiles::for_image(catalog, image.id)? {
            Some(found) => match parsed.entry(found.path.clone()) {
                std::collections::hash_map::Entry::Occupied(e) => e.get().clone(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    let loaded = std::fs::read(&found.path)
                        .ok()
                        .and_then(|bytes| rawkit_engine::profile::dcp::parse(&bytes).ok());
                    if loaded.is_none() {
                        // Named rather than silently ignored: a profile that has
                        // moved should say so, because the alternative is an
                        // export that quietly changes colour.
                        eprintln!(
                            "profile    : {} is missing or unreadable; {} renders with the \
                             decoder's own matrix",
                            found.path, image.filename
                        );
                    }
                    e.insert(loaded).clone()
                }
            },
            None => None,
        };
        chosen.push(Chosen {
            image,
            state,
            profile,
        });
    }
    if chosen.is_empty() {
        bail!("nothing in the library matches that selection");
    }
    Ok(chosen)
}

/// The terms an export runs under: what the files look like, and what the run
/// is allowed to do.
///
/// A struct rather than five more positional arguments, two of which are bare
/// integers and two of which are bare booleans — an order a caller can get
/// wrong without the compiler noticing.
#[derive(Debug, Clone, Copy)]
pub struct Delivery {
    /// Longest edge in pixels, hit exactly. 0 writes full resolution, and
    /// nothing is ever enlarged.
    pub max_dim: u32,
    /// How much sharpening the *file* gets, over and above the capture
    /// sharpening stored with the edit. See [`OutputSharpening`] for why this
    /// lives here and not in the `EditState`.
    pub sharpening: OutputSharpening,
    /// What kind of file to write, which also decides the extension when the
    /// destination is a folder rather than a named file.
    pub format: Format,
    /// Replace files that are already there.
    pub overwrite: bool,
    /// How many photographs to render at once.
    pub jobs: usize,
}

impl Default for Delivery {
    fn default() -> Self {
        Self {
            // Full resolution, unsharpened, refusing to overwrite: the three
            // choices that lose nothing the caller did not ask to lose.
            max_dim: 0,
            sharpening: OutputSharpening::None,
            format: Format::Jpeg {
                quality: EXPORT_QUALITY,
            },
            overwrite: false,
            jobs: 1,
        }
    }
}

/// Render and write what [`gather`] chose. Touches no catalog.
pub fn write(
    chosen: &[Chosen],
    to: &Destination,
    delivery: Delivery,
    mut progress: impl FnMut(usize, usize, &str),
) -> Result<ExportReport> {
    if let (Destination::File(path), false) = (to, chosen.len() == 1) {
        bail!(
            "{} names one file, and {} photographs were chosen",
            path.display(),
            chosen.len()
        );
    }
    if let Destination::Folder(dir) = to {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }

    let mut report = ExportReport::default();
    let gpu = Gpu::new()?;
    let renderer = Renderer::with_tile_size(&gpu, DEFAULT_TILE);

    // Same arrangement as the preview build, and for the same reasons:
    // photographs are independent, the catalog is read before any of them start,
    // and results come back on a channel so one thread does the reporting.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let (sender, receiver) = std::sync::mpsc::channel();
    let workers = delivery.jobs.max(1).min(chosen.len());

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let sender = sender.clone();
            let (next, chosen, gpu, renderer) = (&next, &chosen, &gpu, &renderer);
            scope.spawn(move || loop {
                let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(Chosen {
                    image,
                    state,
                    profile,
                }) = chosen.get(index)
                else {
                    break;
                };
                let destination = match to {
                    // `stem` rather than the file's own name, because a virtual
                    // copy shares that name: two interpretations of one frame
                    // would write one file, and the second would be skipped as
                    // already there — an export that quietly lost half its work.
                    Destination::Folder(dir) => {
                        dir.join(format!("{}.{}", image.stem(), delivery.format.extension()))
                    }
                    Destination::File(path) => path.clone(),
                };
                let outcome = if destination.exists() && !delivery.overwrite {
                    Ok(None)
                } else {
                    one(
                        gpu,
                        renderer,
                        Path::new(&image.path),
                        state,
                        profile.clone(),
                        delivery,
                        &destination,
                    )
                    .map(Some)
                };
                if sender.send((index, outcome)).is_err() {
                    break;
                }
            });
        }
        drop(sender);

        for (done, (index, outcome)) in receiver.into_iter().enumerate() {
            let image = &chosen[index].image;
            progress(done, chosen.len(), &image.filename);
            match outcome {
                Ok(Some(bytes)) => {
                    report.written += 1;
                    report.bytes += bytes;
                }
                Ok(None) => report.skipped += 1,
                // The whole chain. `to_string` is the outermost context alone —
                // "writing /some/path" — and the cause underneath it, a folder
                // that cannot be written or a disk that is full, is the only
                // part anyone can act on.
                Err(e) => report
                    .failed
                    .push((image.filename.clone(), format!("{e:#}"))),
            }
        }
    });

    progress(chosen.len(), chosen.len(), "");
    Ok(report)
}

/// Gather and write in one call, into a folder. What the command line does.
pub fn export(
    catalog: &Catalog,
    selection: Selection,
    to: &Path,
    delivery: Delivery,
    progress: impl FnMut(usize, usize, &str),
) -> Result<ExportReport> {
    let chosen = gather(catalog, selection)?;
    write(
        &chosen,
        &Destination::Folder(to.to_path_buf()),
        delivery,
        progress,
    )
}

/// Decode, render with the stored edit, and write one file.
fn one(
    gpu: &Gpu,
    renderer: &Renderer,
    raw_path: &Path,
    state: &EditState,
    profile: Option<rawkit_engine::CameraProfile>,
    delivery: Delivery,
    destination: &Path,
) -> Result<u64> {
    let raw = rawkit_decode::decode_file(raw_path)
        .with_context(|| format!("decoding {}", raw_path.display()))?;
    let phase = BayerPhase::from_cfa(raw.cfa)
        .with_context(|| format!("{:?} is not a Bayer sensor", raw.cfa))?;
    let mosaic = rawkit_engine::normalise(&raw);
    let frame = Frame {
        data: &mosaic,
        width: raw.width,
        height: raw.height,
        phase,
        as_shot_wb: [
            raw.as_shot_neutral[0],
            raw.as_shot_neutral[1],
            raw.as_shot_neutral[2],
        ],
        clip_level: 1.0,
        profile: profile.unwrap_or_else(|| rawkit_engine::render::profile_for(&raw)),
        recorded_orientation: raw.orientation,
    };

    // Level zero, the whole frame. No pyramid, no averaging.
    let developed = renderer.run(gpu, &frame, state, Output::Display)?;
    // The *developed* size, not the sensor's: a cropped frame that asked for a
    // 2000-pixel edge would otherwise be scaled by the factor its uncropped
    // version needed, and come out smaller than requested.
    let (width, height) =
        rawkit_engine::resize::fit(developed.width, developed.height, delivery.max_dim);
    let mut scaled = rawkit_engine::resize::resample(
        &developed.pixels,
        developed.width,
        developed.height,
        width,
        height,
    );
    // After the resize and never before it: sharpening detail that is about to
    // be thrown away costs time and leaves the halo without the edge.
    rawkit_engine::sharpen::sharpen(&mut scaled, width, height, delivery.sharpening);
    let bytes = rawkit_export::encode(&scaled, width, height, delivery.format)?;
    std::fs::write(destination, &bytes)
        .with_context(|| format!("writing {}", destination.display()))?;
    Ok(bytes.len() as u64)
}

/// Where a caller's `--to` lands, refusing the one place it must not.
///
/// Never inside the previews or backups directories: those are rotated and swept
/// by code that deletes, and a photograph in there would eventually be removed by
/// something that had every right to.
pub fn check_destination(catalog: &Catalog, to: &Destination) -> Result<()> {
    let to = to.parent();
    for reserved in [
        rawkit_catalog::previews::directory(catalog),
        catalog.backup_dir(),
    ]
    .into_iter()
    .flatten()
    {
        let same =
            to.canonicalize().ok() == reserved.canonicalize().ok() && to.canonicalize().is_ok();
        if same || to.starts_with(&reserved) {
            bail!(
                "{} is where the catalog keeps its own files, and sweeps them; \
                 exports need somewhere else",
                reserved.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_name_cannot_hold_a_set() {
        // Refused rather than resolved. The tempting fix is to number them, but
        // that invents a naming convention out of a mistake — and it does it
        // silently, over files the caller believed they had named themselves.
        //
        // Checked before any GPU exists, which is also why this runs anywhere.
        let chosen: Vec<Chosen> = (0..2)
            .map(|i| Chosen {
                image: cull::LibraryImage {
                    id: i,
                    path: format!("/nowhere/{i}.arw"),
                    filename: format!("{i}.arw"),
                    copy_name: None,
                },
                state: EditState::default(),
                profile: None,
            })
            .collect();
        let refused = write(
            &chosen,
            &Destination::File(std::path::PathBuf::from("/nowhere/one.jpg")),
            Delivery::default(),
            |_, _, _| {},
        )
        .expect_err("two photographs asked to become one file");
        assert!(
            refused.to_string().contains("names one file"),
            "refused for the wrong reason: {refused}"
        );
    }

    #[test]
    fn a_destination_inside_the_catalogs_own_directories_is_refused() {
        // The rotation and the sweep both delete, and both are entitled to. A
        // photograph written in there would disappear later for a reason nobody
        // would ever connect to this command.
        let dir = std::env::temp_dir().join(format!("rawkit-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let catalog = Catalog::open(&dir.join("lib.rawkit")).unwrap();

        let previews = rawkit_catalog::previews::directory(&catalog).unwrap();
        let folder = Destination::Folder;
        assert!(check_destination(&catalog, &folder(previews.clone())).is_err());
        assert!(check_destination(&catalog, &folder(previews.join("00"))).is_err());
        assert!(check_destination(&catalog, &folder(catalog.backup_dir().unwrap())).is_err());
        assert!(check_destination(&catalog, &folder(dir.join("exports"))).is_ok());

        // A single file is judged by the folder it would land in, so saving one
        // photograph into the previews directory is refused the same way.
        assert!(check_destination(&catalog, &Destination::File(previews.join("one.jpg"))).is_err());
        assert!(check_destination(&catalog, &Destination::File(dir.join("one.jpg"))).is_ok());

        drop(catalog);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod copy_naming_tests {
    use rawkit_catalog::cull::LibraryImage;

    fn image(filename: &str, copy: Option<&str>) -> LibraryImage {
        LibraryImage {
            id: 1,
            path: format!("/nowhere/{filename}"),
            filename: filename.into(),
            copy_name: copy.map(str::to_string),
        }
    }

    #[test]
    fn a_copy_and_its_original_do_not_write_the_same_file() {
        // The whole reason the copy name has to reach this far. Without it the
        // second file is skipped as already there, and an export of six picks
        // quietly writes four.
        assert_eq!(image("DSC00775.ARW", None).stem(), "DSC00775");
        assert_eq!(image("DSC00775.ARW", Some("mono")).stem(), "DSC00775-mono");
        assert_ne!(
            image("DSC00775.ARW", None).stem(),
            image("DSC00775.ARW", Some("copy 1")).stem()
        );
    }

    #[test]
    fn a_name_a_person_typed_cannot_write_outside_the_folder_they_chose() {
        // A copy is named in a text field. A separator in it would put the file
        // somewhere nobody asked for, and a space merely makes a name awkward.
        assert_eq!(image("a.ARW", Some("copy 1")).stem(), "a-copy-1");
        assert_eq!(image("a.ARW", Some("../etc")).stem(), "a-..-etc");
        assert_eq!(image("a.ARW", Some("high\tkey")).stem(), "a-high-key");
    }
}
