-- Two changes that travel together because one of them needs a table rebuild
-- and SQLite cannot rebuild half a table.
--
-- 1. `volumes` learns a fourth identity shape, for a filesystem that has none.
-- 2. `previews` arrives, now that there is a renderer to fill it.
--
-- This is the first migration to run on a catalog that already had a schema, so
-- it is also the first time the pre-migration backup does its job outside a test.

-- ---------------------------------------------------------------------------
-- volumes: admit a filesystem with no stable identity
-- ---------------------------------------------------------------------------
--
-- The original CHECK allowed three identity shapes, and a filesystem with none
-- of them — tmpfs, overlayfs, a container mount, some network mounts — could not
-- be catalogued at all. CI found this rather than a user: its runners are on
-- exactly such a filesystem, and every scan test failed on a refusal that was
-- entirely correct.
--
-- SQLite cannot alter a CHECK in place, so the table is rebuilt. That is the
-- expensive way to learn that a constraint set should be designed with room in
-- it, which is why the `previews.level` CHECK below deliberately allows a value
-- nothing writes yet.
--
-- `mount_path` is its own column rather than a reuse of `last_mount_path`: one
-- is identity and the other is a convenience, and collapsing them would make a
-- volume's identity change every time it was mounted somewhere else.

CREATE TABLE volumes_rebuilt (
    id                INTEGER PRIMARY KEY,
    kind              TEXT    NOT NULL CHECK (kind IN ('uuid', 'windows_serial', 'network_share', 'mount_path')),
    uuid              TEXT,
    windows_serial    INTEGER,
    host              TEXT,
    share             TEXT,
    -- Identity for a volume that has nothing better. Weak by construction, and
    -- `VolumeId::is_stable` says so, which is what makes content_hash the relink
    -- route for these rather than an optimisation.
    mount_path        TEXT,
    label             TEXT,
    -- Where it was last seen. A convenience, never an identity — even for a
    -- `mount_path` volume, where the two happen to start out equal.
    last_mount_path   TEXT,
    path_convention   TEXT    NOT NULL CHECK (path_convention IN ('exact', 'case_insensitive', 'case_insensitive_normalised')),
    -- Exactly one identity shape must be populated, and it must match `kind`.
    CHECK (
        (kind = 'uuid'           AND uuid IS NOT NULL AND windows_serial IS NULL AND host IS NULL AND share IS NULL AND mount_path IS NULL) OR
        (kind = 'windows_serial' AND uuid IS NULL AND windows_serial IS NOT NULL AND host IS NULL AND share IS NULL AND mount_path IS NULL) OR
        (kind = 'network_share'  AND uuid IS NULL AND windows_serial IS NULL AND host IS NOT NULL AND share IS NOT NULL AND mount_path IS NULL) OR
        (kind = 'mount_path'     AND uuid IS NULL AND windows_serial IS NULL AND host IS NULL AND share IS NULL AND mount_path IS NOT NULL)
    )
);

INSERT INTO volumes_rebuilt
    (id, kind, uuid, windows_serial, host, share, mount_path, label, last_mount_path, path_convention)
SELECT id, kind, uuid, windows_serial, host, share, NULL, label, last_mount_path, path_convention
  FROM volumes;

-- Safe only with foreign keys disabled, which the runner does for the duration
-- and checks with `PRAGMA foreign_key_check` before committing. With them on,
-- dropping a referenced table fires its ON DELETE CASCADE and takes every folder
-- in the library with it.
DROP TABLE volumes;
ALTER TABLE volumes_rebuilt RENAME TO volumes;

CREATE UNIQUE INDEX volumes_identity ON volumes (
    kind, ifnull(uuid, ''), ifnull(windows_serial, -1),
    ifnull(host, ''), ifnull(share, ''), ifnull(mount_path, '')
);

-- ---------------------------------------------------------------------------
-- previews
-- ---------------------------------------------------------------------------
--
-- Rendered copies at a few sizes, so a grid and a filmstrip do not each mean
-- decoding a RAW. Files on disk rather than blobs in here: they are large,
-- regenerable, and a catalog that doubles in size every time a preview is built
-- is a catalog whose backups become unusable.

CREATE TABLE previews (
    id              INTEGER PRIMARY KEY,
    image_id        INTEGER NOT NULL REFERENCES images (id) ON DELETE CASCADE,
    -- 'one_to_one' is allowed and nothing writes it yet. Normally a value with
    -- no writer would be left out — but widening this CHECK later means another
    -- full table rebuild, which is the lesson the `volumes` half of this
    -- migration paid for. A 1:1 preview is a per-tile thing generated on demand,
    -- not something a bulk build produces for a whole library.
    level           TEXT    NOT NULL CHECK (level IN ('thumb', 'small', 'standard', 'one_to_one')),
    -- Relative to the previews directory beside the catalog, so a library that
    -- moves keeps them.
    path            TEXT    NOT NULL,
    -- Which edit this shows. An edit makes every preview of that image stale,
    -- and this is how that is known without opening a single file.
    edit_state_hash TEXT    NOT NULL,
    width           INTEGER NOT NULL,
    height          INTEGER NOT NULL,
    bytes           INTEGER NOT NULL,
    created_at      INTEGER NOT NULL
);

-- One preview per image per level. A second one for the same level is not a
-- variant, it is the old one that should have been replaced.
CREATE UNIQUE INDEX previews_by_level ON previews (image_id, level);
