-- Which colour profile to render a camera's photographs with.
--
-- Keyed by the body, not by the photograph, because that is what a DCP
-- describes: a characterisation of one sensor under one or two illuminants. A
-- profile chosen per image would be a different thing — a look — and would put
-- a machine-local file path inside `edit_states`, where it would break the
-- property that the same RAW and the same edit give the same pixels anywhere.
--
-- The path is stored rather than the profile. Adobe's profiles are not
-- redistributable and can be hundreds of kilobytes each; copying one into every
-- catalog would be both a licence question and a size one. The cost is that a
-- catalog moved to another machine loses its profiles until they are pointed at
-- again, and `name` exists so that a missing one can say what it was rather
-- than only where it used to be.
CREATE TABLE camera_profiles (
    camera_make  TEXT NOT NULL,
    camera_model TEXT NOT NULL,
    -- Absolute, and deliberately not normalised through `CatalogPath`: this is
    -- not a photograph inside a watched folder, it is an application resource
    -- somewhere else entirely, and relink has no business following it.
    path         TEXT NOT NULL,
    -- The profile's own `ProfileName` tag, for an interface to show and for a
    -- missing file to be named by.
    name         TEXT,
    chosen_at    INTEGER NOT NULL,
    PRIMARY KEY (camera_make, camera_model)
);
