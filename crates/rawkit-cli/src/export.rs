//! Turning what you decided into photographs.
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
//! # Always level zero
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

use anyhow::{bail, Context, Result};
use rawkit_catalog::cull::{self, Flag};
use rawkit_catalog::db::Catalog;
use rawkit_editstate::EditState;
use rawkit_engine::{render::DEFAULT_TILE, BayerPhase, Frame, Gpu, Output, Renderer};
use std::path::Path;

/// Quality for a delivered file. Higher than a preview's 85: this one is looked
/// at closely, printed, or sent to somebody.
const EXPORT_QUALITY: u8 = 92;

/// Which photographs to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    /// Everything present in the library.
    All,
    /// What survived a cull.
    Picks,
    /// At least this many stars.
    Rated(u8),
    /// One image, by its catalog id.
    Image(i64),
}

#[derive(Debug, Default)]
pub struct ExportReport {
    pub written: usize,
    pub skipped: usize,
    pub bytes: u64,
    pub failed: Vec<(String, String)>,
}

/// Render and write every photograph the selection names.
pub fn export(
    catalog: &Catalog,
    selection: Selection,
    to: &Path,
    max_dim: u32,
    overwrite: bool,
    jobs: usize,
    mut progress: impl FnMut(usize, usize, &str),
) -> Result<ExportReport> {
    let mut chosen = Vec::new();
    for image in cull::sequence(catalog)? {
        let judgement = cull::judgement(catalog, image.id)?;
        let wanted = match selection {
            Selection::All => true,
            Selection::Picks => judgement.flag == Some(Flag::Pick),
            Selection::Rated(stars) => judgement.rating.unwrap_or(0) >= stars,
            Selection::Image(id) => image.id == id,
        };
        if !wanted {
            continue;
        }
        // The edit as it stands, or as shot for a photograph nobody has touched.
        let state = rawkit_catalog::edits::latest(catalog, image.id)?
            .map(|(_, state)| state)
            .unwrap_or_default();
        chosen.push((image, state));
    }
    if chosen.is_empty() {
        bail!("nothing in the library matches that selection");
    }

    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;

    let mut report = ExportReport::default();
    let gpu = Gpu::new()?;
    let renderer = Renderer::with_tile_size(&gpu, DEFAULT_TILE);

    // Same arrangement as the preview build, and for the same reasons:
    // photographs are independent, the catalog is read before any of them start,
    // and results come back on a channel so one thread does the reporting.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let (sender, receiver) = std::sync::mpsc::channel();
    let workers = jobs.max(1).min(chosen.len());

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let sender = sender.clone();
            let (next, chosen, gpu, renderer) = (&next, &chosen, &gpu, &renderer);
            scope.spawn(move || loop {
                let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some((image, state)) = chosen.get(index) else {
                    break;
                };
                let destination = to.join(format!(
                    "{}.jpg",
                    Path::new(&image.filename)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| image.filename.clone())
                ));
                let outcome = if destination.exists() && !overwrite {
                    Ok(None)
                } else {
                    one(
                        gpu,
                        renderer,
                        Path::new(&image.path),
                        state,
                        max_dim,
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
            let (image, _) = &chosen[index];
            progress(done, chosen.len(), &image.filename);
            match outcome {
                Ok(Some(bytes)) => {
                    report.written += 1;
                    report.bytes += bytes;
                }
                Ok(None) => report.skipped += 1,
                Err(e) => report.failed.push((image.filename.clone(), e.to_string())),
            }
        }
    });

    progress(chosen.len(), chosen.len(), "");
    Ok(report)
}

/// Decode, render with the stored edit, and write one file.
fn one(
    gpu: &Gpu,
    renderer: &Renderer,
    raw_path: &Path,
    state: &EditState,
    max_dim: u32,
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
        profile: crate::render::profile_for(&raw),
    };

    // Level zero, the whole frame. No pyramid, no averaging.
    let rgba = renderer.run(gpu, &frame, state, Output::Display)?;
    let step = crate::render::downsample_step(raw.width, raw.height, max_dim);
    let (scaled, width, height) = crate::render::downsample(&rgba, raw.width, raw.height, step);
    let bytes = rawkit_export::encode(
        &scaled,
        width,
        height,
        rawkit_export::Format::Jpeg {
            quality: EXPORT_QUALITY,
        },
    )?;
    std::fs::write(destination, &bytes)
        .with_context(|| format!("writing {}", destination.display()))?;
    Ok(bytes.len() as u64)
}

/// Where a caller's `--to` lands, refusing the one place it must not.
///
/// Never inside the previews or backups directories: those are rotated and swept
/// by code that deletes, and a photograph in there would eventually be removed by
/// something that had every right to.
pub fn check_destination(catalog: &Catalog, to: &Path) -> Result<()> {
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
    fn a_destination_inside_the_catalogs_own_directories_is_refused() {
        // The rotation and the sweep both delete, and both are entitled to. A
        // photograph written in there would disappear later for a reason nobody
        // would ever connect to this command.
        let dir = std::env::temp_dir().join(format!("rawkit-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let catalog = Catalog::open(&dir.join("lib.rawkit")).unwrap();

        let previews = rawkit_catalog::previews::directory(&catalog).unwrap();
        assert!(check_destination(&catalog, &previews).is_err());
        assert!(check_destination(&catalog, &previews.join("00")).is_err());
        assert!(check_destination(&catalog, &catalog.backup_dir().unwrap()).is_err());
        assert!(check_destination(&catalog, &dir.join("exports")).is_ok());

        drop(catalog);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
