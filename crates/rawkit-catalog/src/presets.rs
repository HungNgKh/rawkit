//! Looks saved to be used again, on photographs they have never seen.
//!
//! # A preset is a state and a claim
//!
//! The state is a whole [`EditState`]; the claim is which [`Group`]s of it apply.
//! Applying reads only the claimed groups, so "warm and contrasty" stays warm
//! and contrasty rather than also imposing the crop of the frame it was made
//! from. [`rawkit_editstate::groups`] explains why that split is the whole point.
//!
//! # Why the state is stored whole
//!
//! Trimming it to the claimed groups would save a few hundred bytes and cost the
//! ability to widen a preset later — a preset that grew a group would have to
//! invent values it had never recorded. The groups are a view of the state, not
//! a cut of it.

use crate::{db::Catalog, CatalogError};
use rawkit_editstate::{EditState, Group};

/// A saved look.
#[derive(Debug, Clone, PartialEq)]
pub struct Preset {
    pub name: String,
    /// The parts of `state` this preset applies. Everything else in `state` is
    /// carried but never read — see the module note.
    pub groups: Vec<Group>,
    pub state: EditState,
}

impl Preset {
    /// Put this look onto an edit, leaving the parts it does not claim alone.
    pub fn apply_to(&self, target: &mut EditState) {
        target.adopt(&self.state, &self.groups);
    }
}

/// Save a look under `name`, replacing any earlier one with that name.
///
/// Refuses an empty claim: a preset that carries nothing would appear in the
/// list, be applied, and do nothing, which is indistinguishable from a bug.
pub fn save(
    catalog: &Catalog,
    name: &str,
    state: &EditState,
    groups: &[Group],
) -> Result<(), CatalogError> {
    if name.trim().is_empty() {
        return Err(CatalogError::Unsupported("a preset needs a name"));
    }
    if groups.is_empty() {
        return Err(CatalogError::Unsupported(
            "a preset that carries no groups would do nothing",
        ));
    }
    state.validate()?;

    let json = serde_json::to_string(state)
        .map_err(|e| CatalogError::Sqlite(format!("serialising a preset: {e}")))?;
    catalog.connection().execute(
        "INSERT INTO presets (name, json, groups, created_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (name) DO UPDATE
            SET json = excluded.json, groups = excluded.groups,
                created_at = excluded.created_at",
        rusqlite::params![name.trim(), json, encode_groups(groups), now()],
    )?;
    Ok(())
}

/// Every saved look, by name.
pub fn all(catalog: &Catalog) -> Result<Vec<Preset>, CatalogError> {
    let mut statement = catalog
        .connection()
        .prepare("SELECT name, json, groups FROM presets ORDER BY name")?;
    let rows = statement
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter().map(decode).collect()
}

/// One saved look, if it is there.
pub fn get(catalog: &Catalog, name: &str) -> Result<Option<Preset>, CatalogError> {
    let row: Option<(String, String, String)> = catalog
        .connection()
        .query_row(
            "SELECT name, json, groups FROM presets WHERE name = ?1",
            rusqlite::params![name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    row.map(decode).transpose()
}

/// Remove a saved look. Photographs it was applied to keep their edits: a
/// preset is a source of a decision, not the decision itself.
pub fn forget(catalog: &Catalog, name: &str) -> Result<(), CatalogError> {
    catalog.connection().execute(
        "DELETE FROM presets WHERE name = ?1",
        rusqlite::params![name],
    )?;
    Ok(())
}

fn decode((name, json, groups): (String, String, String)) -> Result<Preset, CatalogError> {
    let state: EditState = serde_json::from_str(&json)
        .map_err(|e| CatalogError::Sqlite(format!("preset {name:?}: {e}")))?;
    state.validate()?;
    Ok(Preset {
        name,
        groups: decode_groups(&groups)?,
        state,
    })
}

fn encode_groups(groups: &[Group]) -> String {
    let names: Vec<&str> = groups.iter().map(|g| g.as_str()).collect();
    serde_json::to_string(&names).expect("a list of static strings is serialisable")
}

/// A group name this build does not know came from a newer one. Refused rather
/// than skipped: applying most of somebody's preset and saying nothing is the
/// failure they cannot see.
fn decode_groups(encoded: &str) -> Result<Vec<Group>, CatalogError> {
    let names: Vec<String> = serde_json::from_str(encoded)
        .map_err(|e| CatalogError::Sqlite(format!("preset groups {encoded:?}: {e}")))?;
    names
        .iter()
        .map(|name| {
            Group::parse(name).ok_or_else(|| {
                CatalogError::Sqlite(format!(
                    "preset names a group {name:?} this build does not have — \
                     it was saved by a newer rawkit"
                ))
            })
        })
        .collect()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        Catalog::in_memory().expect("catalog")
    }

    fn warm_and_contrasty() -> EditState {
        let mut s = EditState::default();
        s.white_balance.temperature_k = Some(7000.0);
        s.tone.contrast = 0.4;
        s.crop.left = 0.3;
        s
    }

    #[test]
    fn a_library_starts_with_no_presets() {
        assert!(all(&catalog()).unwrap().is_empty());
        assert_eq!(get(&catalog(), "Warm").unwrap(), None);
    }

    #[test]
    fn a_preset_survives_being_read_back() {
        let c = catalog();
        let state = warm_and_contrasty();
        save(&c, "Warm", &state, &[Group::Tone, Group::WhiteBalance]).unwrap();

        let back = get(&c, "Warm").unwrap().unwrap();
        assert_eq!(back.state, state);
        assert_eq!(back.groups, vec![Group::Tone, Group::WhiteBalance]);
    }

    #[test]
    fn applying_a_preset_leaves_the_photograph_its_own_crop() {
        // The property presets exist for: a look is not a frame.
        let c = catalog();
        save(
            &c,
            "Warm",
            &warm_and_contrasty(),
            &[Group::Tone, Group::WhiteBalance],
        )
        .unwrap();

        let mut target = EditState::default();
        target.crop.left = 0.1;
        target.colour.saturation = 0.6;
        get(&c, "Warm").unwrap().unwrap().apply_to(&mut target);

        assert_eq!(target.tone.contrast, 0.4, "the claimed group arrives");
        assert_eq!(target.white_balance.temperature_k, Some(7000.0));
        assert_eq!(target.crop.left, 0.1, "the frame is the photograph's own");
        assert_eq!(
            target.colour.saturation, 0.6,
            "and so is anything the preset did not claim"
        );
    }

    #[test]
    fn saving_again_under_one_name_replaces_it() {
        let c = catalog();
        let mut first = EditState::default();
        first.tone.contrast = 0.1;
        let mut second = EditState::default();
        second.tone.contrast = 0.9;

        save(&c, "Punchy", &first, &[Group::Tone]).unwrap();
        save(&c, "Punchy", &second, &[Group::Tone]).unwrap();

        let saved = all(&c).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].state.tone.contrast, 0.9);
    }

    #[test]
    fn presets_come_back_in_a_stable_order() {
        let c = catalog();
        for name in ["Zinc", "Amber", "Moss"] {
            save(&c, name, &EditState::default(), &[Group::Tone]).unwrap();
        }
        let names: Vec<String> = all(&c).unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["Amber", "Moss", "Zinc"]);
    }

    #[test]
    fn a_preset_that_claims_nothing_is_refused() {
        // It would list, apply, and do nothing — the same as being broken.
        let c = catalog();
        assert!(save(&c, "Empty", &EditState::default(), &[]).is_err());
        assert!(save(&c, "  ", &EditState::default(), &[Group::Tone]).is_err());
    }

    #[test]
    fn forgetting_removes_only_that_one() {
        let c = catalog();
        save(&c, "A", &EditState::default(), &[Group::Tone]).unwrap();
        save(&c, "B", &EditState::default(), &[Group::Tone]).unwrap();
        forget(&c, "A").unwrap();
        assert_eq!(all(&c).unwrap().len(), 1);
    }

    #[test]
    fn a_group_from_a_newer_build_is_refused_rather_than_skipped() {
        // Applying most of somebody's preset in silence is the failure they
        // cannot see.
        let c = catalog();
        save(&c, "Future", &EditState::default(), &[Group::Tone]).unwrap();
        c.connection()
            .execute(
                r#"UPDATE presets SET groups = '["tone","dehaze"]' WHERE name = 'Future'"#,
                [],
            )
            .unwrap();
        assert!(get(&c, "Future").is_err());
        assert!(all(&c).is_err());
    }
}
