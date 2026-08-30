-- Two ways to reuse a decision, which are not the same shape.
--
-- A *preset* is a look meant for photographs it has never seen, so it must be
-- partial: it carries the groups it claims and nothing else. `groups` is that
-- claim, and it is stored rather than inferred, because "every field that
-- differs from the default" changes meaning the moment a default changes.
-- Without it a preset made from a warm frame would impose that frame's crop and
-- exposure on every photograph it touched.
CREATE TABLE presets (
    -- The identity a user types and picks from, so it is the key. Renaming is
    -- delete-and-save, which is also what it is to the person doing it.
    name       TEXT PRIMARY KEY,
    -- A serialised EditState. Whole, not trimmed to `groups`: a preset that
    -- later grows a group would otherwise have to invent the values it never
    -- stored, and a whole state costs a few hundred bytes.
    json       TEXT NOT NULL,
    -- JSON array of rawkit_editstate::Group names — field names, not indices,
    -- so reordering the enum cannot silently repoint a stored preset.
    groups     TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

-- A *snapshot* is a place to come back to in one photograph, and every state
-- that photograph has ever had is already in `edit_states`. So a snapshot is a
-- name on a version, not a second copy of the pixels' description: there is no
-- way for the two to disagree about what was saved, and no second table to keep
-- in step when an image is deleted.
CREATE TABLE snapshots (
    image_id   INTEGER NOT NULL,
    version    INTEGER NOT NULL,
    name       TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    -- One name per image, so taking a snapshot twice under one name replaces it
    -- rather than leaving two rows a user cannot choose between.
    PRIMARY KEY (image_id, name),
    -- Against the (image_id, version) pair, which `edit_states_version` already
    -- makes unique. A snapshot cannot name a version that was never written,
    -- and deleting an image takes its snapshots with it.
    FOREIGN KEY (image_id, version) REFERENCES edit_states (image_id, version)
        ON DELETE CASCADE
);
