//! What an export from the window is asked to do, and what is remembered of it.
//!
//! Export was two chords and no choices: Ctrl+E and a fixed full-size JPEG. The
//! writer underneath ([`rawkit_deliver::write`]) has always taken more than
//! that — a format, a longest edge, sharpening for the output, whether to
//! replace what is there — and the terminal has always offered it. This is
//! those options as something a page can send, and nothing the writer does not
//! do: no colour space, no naming pattern, no "add a number". A control for a
//! thing that cannot happen is worse than its absence.
//!
//! # Where the settings are kept
//!
//! Beside the window's geometry and the list of recent catalogs, and for the
//! same reason: a destination folder is a fact about this machine. A catalog
//! carried to another computer should not arrive with a preset pointing at a
//! folder that is not there.

use rawkit_deliver::Delivery;
use rawkit_deliver::OutputSharpening;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Which photographs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// The ones set aside with the select key, wherever the filter has them.
    Selected,
    /// What the window is showing: the library or a collection, through the
    /// filter, in the order it is shown in.
    Shown,
    /// The one on screen.
    Current,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileFormat {
    Jpeg,
    Png8,
    Png16,
    Tiff16,
}

/// Everything the export panel sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub scope: Scope,
    pub format: FileFormat,
    /// For JPEG only; carried for the others so that switching format and back
    /// does not forget it.
    pub quality: u8,
    /// Longest edge in pixels. 0 is full size, and nothing is ever enlarged.
    pub long_edge: u32,
    /// `none`, `low`, `standard` or `high` — the engine's own names, so that a
    /// setting it grows is a setting this accepts.
    pub sharpening: String,
    /// Replace files that are already there. Otherwise they are left and
    /// counted.
    pub replace: bool,
    pub folder: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        let delivery = Delivery::default();
        Self {
            scope: Scope::Shown,
            format: FileFormat::Jpeg,
            quality: match delivery.format {
                rawkit_export::Format::Jpeg { quality } => quality,
                _ => 92,
            },
            long_edge: 0,
            sharpening: OutputSharpening::None.as_str().into(),
            replace: false,
            folder: None,
        }
    }
}

impl Settings {
    /// What the writer is given, or why these settings cannot be carried out.
    /// The sentences are for a person: they arrive on the status line.
    pub fn delivery(&self, jobs: usize) -> Result<(Delivery, PathBuf), String> {
        let folder = self
            .folder
            .clone()
            .filter(|folder| !folder.as_os_str().is_empty())
            .ok_or("choose a folder to export into first")?;
        let sharpening = OutputSharpening::parse(&self.sharpening)
            .ok_or_else(|| format!("{} is not a kind of output sharpening", self.sharpening))?;
        if !(1..=100).contains(&self.quality) {
            return Err(format!(
                "{} is not a JPEG quality; it runs from 1 to 100",
                self.quality
            ));
        }
        // A floor rather than a refusal of small sizes in general: below this
        // it is a typing mistake — "20" for "2000" — and the result would be a
        // folder of postage stamps found an hour later.
        if self.long_edge != 0 && self.long_edge < 64 {
            return Err(format!(
                "{} pixels is too small to be meant; the smallest long edge is 64",
                self.long_edge
            ));
        }
        let format = match self.format {
            FileFormat::Jpeg => rawkit_export::Format::Jpeg {
                quality: self.quality,
            },
            FileFormat::Png8 => rawkit_export::Format::Png8,
            FileFormat::Png16 => rawkit_export::Format::Png16,
            FileFormat::Tiff16 => rawkit_export::Format::Tiff16,
        };
        Ok((
            Delivery {
                max_dim: self.long_edge,
                sharpening,
                format,
                overwrite: self.replace,
                jobs,
            },
            folder,
        ))
    }
}

/// A named set of settings. The scope is not part of one: "web, 2048" is how,
/// and which photographs is a different question asked every time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub settings: Settings,
}

/// What is kept between launches.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Kept {
    /// What was last exported with, which is what "export again" means and what
    /// the panel opens showing.
    #[serde(default)]
    pub last: Option<Settings>,
    #[serde(default)]
    pub presets: Vec<Preset>,
}

fn file(dir: &Path) -> PathBuf {
    dir.join("export.json")
}

/// Nothing here refuses, like the rest of this folder: a file that cannot be
/// read is a first run.
pub fn kept(dir: &Path) -> Kept {
    std::fs::read_to_string(file(dir))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn keep(dir: &Path, kept: &Kept) {
    if let Ok(text) = serde_json::to_string_pretty(kept) {
        let _ = std::fs::write(file(dir), text);
    }
}

/// Remember what was just exported with.
pub fn remember(dir: &Path, settings: &Settings) {
    let mut all = kept(dir);
    all.last = Some(settings.clone());
    keep(dir, &all);
}

/// Save these settings under a name, replacing a preset that has it already —
/// compared the way a person reads it, so "Web" and "web " are one preset.
pub fn save_preset(dir: &Path, name: &str, settings: &Settings) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("a preset needs a name".into());
    }
    let mut all = kept(dir);
    all.presets
        .retain(|preset| !preset.name.eq_ignore_ascii_case(name));
    all.presets.push(Preset {
        name: name.to_string(),
        settings: settings.clone(),
    });
    all.presets
        .sort_by_key(|preset| preset.name.to_ascii_lowercase());
    keep(dir, &all);
    Ok(())
}

pub fn forget_preset(dir: &Path, name: &str) {
    let mut all = kept(dir);
    all.presets
        .retain(|preset| !preset.name.eq_ignore_ascii_case(name.trim()));
    keep(dir, &all);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::tests::Scratch;

    fn into(folder: &str) -> Settings {
        Settings {
            folder: Some(folder.into()),
            ..Settings::default()
        }
    }

    #[test]
    fn the_defaults_lose_nothing_nobody_asked_to_lose() {
        let (delivery, folder) = into("/out").delivery(2).unwrap();
        assert_eq!(folder, PathBuf::from("/out"));
        assert_eq!(delivery.max_dim, 0, "full size");
        assert!(!delivery.overwrite, "and nothing replaced");
        assert_eq!(delivery.sharpening, OutputSharpening::None);
        assert!(matches!(
            delivery.format,
            rawkit_export::Format::Jpeg { quality: 92 }
        ));
        assert_eq!(delivery.jobs, 2);
    }

    #[test]
    fn what_cannot_be_carried_out_is_refused_in_words() {
        let nowhere = Settings::default().delivery(1).unwrap_err();
        assert!(nowhere.contains("choose a folder"), "{nowhere}");

        let tiny = Settings {
            long_edge: 20,
            ..into("/out")
        };
        assert!(tiny.delivery(1).unwrap_err().contains("too small"));

        let quality = Settings {
            quality: 0,
            ..into("/out")
        };
        assert!(quality.delivery(1).unwrap_err().contains("1 to 100"));

        let sharpening = Settings {
            sharpening: "extreme".into(),
            ..into("/out")
        };
        assert!(sharpening.delivery(1).unwrap_err().contains("extreme"));
    }

    #[test]
    fn every_format_the_panel_offers_is_one_the_writer_has() {
        for (format, extension) in [
            (FileFormat::Jpeg, "jpg"),
            (FileFormat::Png8, "png"),
            (FileFormat::Png16, "png"),
            (FileFormat::Tiff16, "tif"),
        ] {
            let settings = Settings {
                format,
                ..into("/out")
            };
            let (delivery, _) = settings.delivery(1).unwrap();
            assert_eq!(delivery.format.extension(), extension);
        }
    }

    #[test]
    fn the_last_settings_and_the_presets_come_back() {
        let scratch = Scratch::new("export-kept");
        assert_eq!(kept(&scratch.0), Kept::default(), "a first run");

        let web = Settings {
            long_edge: 2048,
            sharpening: "standard".into(),
            ..into("/out/web")
        };
        remember(&scratch.0, &web);
        save_preset(&scratch.0, "Web 2048", &web).unwrap();
        save_preset(&scratch.0, "archive", &into("/out/tiff")).unwrap();
        // The same name, as a person reads it: replaced, not listed twice.
        save_preset(&scratch.0, " web 2048 ", &into("/elsewhere")).unwrap();

        let back = kept(&scratch.0);
        assert_eq!(back.last, Some(web));
        let names: Vec<&str> = back.presets.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["archive", "web 2048"]);
        assert_eq!(back.presets[1].settings.folder, Some("/elsewhere".into()));

        forget_preset(&scratch.0, "ARCHIVE");
        assert_eq!(kept(&scratch.0).presets.len(), 1);
        assert!(save_preset(&scratch.0, "  ", &into("/out")).is_err());
    }

    #[test]
    fn a_file_somebody_edited_badly_is_a_first_run() {
        let scratch = Scratch::new("export-unreadable");
        std::fs::write(file(&scratch.0), b"{ not json").unwrap();
        assert_eq!(kept(&scratch.0), Kept::default());
    }
}
