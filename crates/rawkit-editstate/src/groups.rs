//! Which parts of an edit a preset carries.
//!
//! # Why a preset is partial
//!
//! A preset that stored a whole [`EditState`] would be nearly useless, because
//! applying it would also impose the *photograph-specific* parts of the frame it
//! was made from. Reaching for "warm, contrasty" and getting someone else's crop
//! is not a smaller version of the right behaviour; it is the wrong behaviour.
//!
//! So a preset is a state plus the set of groups it claims, and applying one
//! copies only those. [`Group`] is that set's vocabulary.
//!
//! # What is deliberately not a group
//!
//! `orientation` and `crop` describe where the frame is, which is a decision
//! about one photograph and cannot be right about a second one. They have no
//! [`Group`], so no preset can carry them — a stronger guarantee than leaving
//! them out by convention, since there is no way to ask for them.
//!
//! `schema_version` is not a decision at all.
//!
//! The test `every_field_is_either_a_group_or_deliberately_not_one` holds this
//! together: adding a field to `EditState` fails it until the field is either
//! given a `Group` or listed as per-photograph.

use crate::EditState;

/// One part of an edit that a preset can carry.
///
/// The string forms are the `EditState` field names, so a stored preset names
/// the fields it sets rather than an index into a list that could be reordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Group {
    WhiteBalance,
    Tone,
    Detail,
    Colour,
    Hsl,
    Curve,
    Grade,
    Masks,
}

impl Group {
    /// Every group, in the order an interface should offer them — which is the
    /// order the renderer applies them, so a list of ticked boxes reads down the
    /// pipeline rather than in an order chosen by the alphabet.
    pub const ALL: [Group; 8] = [
        Group::WhiteBalance,
        Group::Tone,
        Group::Detail,
        Group::Colour,
        Group::Hsl,
        Group::Curve,
        Group::Grade,
        Group::Masks,
    ];

    /// The `EditState` field this group covers.
    pub fn as_str(self) -> &'static str {
        match self {
            Group::WhiteBalance => "white_balance",
            Group::Tone => "tone",
            Group::Detail => "detail",
            Group::Colour => "colour",
            Group::Hsl => "hsl",
            Group::Curve => "curve",
            Group::Grade => "grade",
            Group::Masks => "masks",
        }
    }

    /// Parse a stored group name. `None` for anything else, which is how a
    /// preset written by a newer build is noticed rather than mistaken for one
    /// of ours.
    pub fn parse(name: &str) -> Option<Group> {
        Group::ALL.into_iter().find(|g| g.as_str() == name)
    }

    /// What to call this group in an interface.
    pub fn label(self) -> &'static str {
        match self {
            Group::WhiteBalance => "White balance",
            Group::Tone => "Tone",
            Group::Detail => "Detail",
            Group::Colour => "Colour",
            Group::Hsl => "Hue mixer",
            Group::Curve => "Tone curve",
            Group::Grade => "Colour grading",
            Group::Masks => "Local adjustments",
        }
    }
}

impl EditState {
    /// Take `groups` from `source`, and leave everything else as it is.
    ///
    /// The operation a preset is: the parts asked for are replaced wholesale,
    /// never blended. Blending two looks needs a rule per slider — a curve and a
    /// hue band do not average the same way — and a preset that half-applied
    /// would be a look nobody designed.
    pub fn adopt(&mut self, source: &EditState, groups: &[Group]) {
        for group in groups {
            match group {
                Group::WhiteBalance => self.white_balance = source.white_balance,
                Group::Tone => self.tone = source.tone,
                Group::Detail => self.detail = source.detail,
                Group::Colour => self.colour = source.colour,
                Group::Hsl => self.hsl = source.hsl,
                Group::Curve => self.curve = source.curve.clone(),
                Group::Grade => self.grade = source.grade,
                // Cloned rather than copied, and *replacing* rather than
                // appending: a preset that added its masks to whatever the
                // target already had would build up a stack nobody asked for,
                // one application at a time.
                Group::Masks => self.masks = source.masks.clone(),
            }
        }
    }

    /// The groups in which this edit differs from the camera's own rendering.
    ///
    /// What a "save preset" dialogue should tick by default: the parts the user
    /// actually touched. Offering every group pre-ticked would make every preset
    /// carry six neutral settings that quietly reset whatever the target frame
    /// had, which is the partial-preset problem coming back through the door
    /// marked *convenience*.
    pub fn touched_groups(&self) -> Vec<Group> {
        let plain = EditState::default();
        let mut touched = Vec::new();
        for group in Group::ALL {
            let mut probe = plain.clone();
            probe.adopt(self, &[group]);
            if probe != plain {
                touched.push(group);
            }
        }
        touched
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// An edit with every group moved off its default, so a copy that misses one
    /// is visible.
    fn everything_moved() -> EditState {
        let mut s = EditState::default();
        s.white_balance.temperature_k = Some(5200.0);
        s.tone.exposure_ev = 1.0;
        s.detail.sharpen_amount = 0.5;
        s.colour.saturation = 0.4;
        s.hsl.red.hue = 0.3;
        s.curve.points = vec![[0.0, 0.0], [0.4, 0.6], [1.0, 1.0]];
        s.grade.shadows.saturation = 0.5;
        s.masks = vec![crate::Mask {
            exposure_ev: -1.0,
            ..crate::Mask::default()
        }];
        s
    }

    #[test]
    fn every_field_is_either_a_group_or_deliberately_not_one() {
        // The guard that keeps this module honest as `EditState` grows: a new
        // field must be given a `Group`, or named here as per-photograph.
        let json = serde_json::to_value(EditState::default()).unwrap();
        let fields: BTreeSet<String> = json
            .as_object()
            .expect("an EditState serialises as an object")
            .keys()
            .cloned()
            .collect();

        let mut accounted: BTreeSet<String> =
            Group::ALL.iter().map(|g| g.as_str().to_string()).collect();
        for not_a_look in ["schema_version", "orientation", "crop"] {
            accounted.insert(not_a_look.to_string());
        }

        assert_eq!(
            fields, accounted,
            "a new EditState field must either get a Group or be listed here as \
             per-photograph — otherwise presets silently stop carrying it"
        );
    }

    #[test]
    fn adopting_a_group_leaves_the_others_alone() {
        // The property the whole module exists for.
        let source = everything_moved();
        let mut target = EditState::default();
        target.tone.exposure_ev = -2.0;
        target.crop.left = 0.25;

        target.adopt(&source, &[Group::Colour]);

        assert_eq!(target.colour.saturation, 0.4, "the asked-for group arrives");
        assert_eq!(target.tone.exposure_ev, -2.0, "and nothing else moves");
        assert_eq!(target.crop.left, 0.25);
    }

    #[test]
    fn no_preset_can_carry_the_crop() {
        // Not "we chose not to tick it" — there is no way to ask.
        let mut source = EditState::default();
        source.crop.left = 0.4;
        source.orientation = crate::Orientation::Rotate90Cw;

        let mut target = EditState::default();
        target.adopt(&source, &Group::ALL);

        assert_eq!(target.crop, EditState::default().crop);
        assert_eq!(target.orientation, crate::Orientation::AsShot);
    }

    #[test]
    fn adopting_every_group_reproduces_the_look_exactly() {
        let source = everything_moved();
        let mut target = EditState::default();
        target.adopt(&source, &Group::ALL);
        assert_eq!(target, source, "every group together is the whole look");
    }

    #[test]
    fn an_untouched_edit_offers_no_groups() {
        assert!(EditState::default().touched_groups().is_empty());
    }

    #[test]
    fn only_the_groups_that_were_moved_are_offered() {
        let mut s = EditState::default();
        s.tone.contrast = 0.3;
        s.grade.highlights.hue = 40.0;
        s.grade.highlights.saturation = 0.2;
        // Per-photograph, and so not a group at all.
        s.crop.left = 0.1;

        assert_eq!(s.touched_groups(), vec![Group::Tone, Group::Grade]);
    }

    #[test]
    fn group_names_survive_a_round_trip() {
        for group in Group::ALL {
            assert_eq!(Group::parse(group.as_str()), Some(group));
        }
        assert_eq!(Group::parse("exposure"), None);
    }
}
