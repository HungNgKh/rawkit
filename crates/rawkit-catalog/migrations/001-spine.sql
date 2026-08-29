-- The spine: a volume holds folders, a folder holds files, a file yields images,
-- an image accumulates edit states. Everything else in the design sketch hangs
-- off this and arrives in later migrations, when there is code that reads it.

-- Where files live, as identity rather than as a path. The three operating
-- systems disagree about what identity is, so all three shapes are columns from
-- the first schema: adding one later means migrating catalogs that already exist
-- on other people's disks.
CREATE TABLE volumes (
    id                INTEGER PRIMARY KEY,
    kind              TEXT    NOT NULL CHECK (kind IN ('uuid', 'windows_serial', 'network_share')),
    uuid              TEXT,
    windows_serial    INTEGER,
    host              TEXT,
    share             TEXT,
    label             TEXT,
    -- Where it was last seen. A convenience for reconnecting, never an identity.
    last_mount_path   TEXT,
    -- Which spelling rules apply to paths under this volume. Stored rather than
    -- assumed from the running OS, because a catalog can be opened on a
    -- different one and the keys would otherwise be silently incomparable.
    path_convention   TEXT    NOT NULL CHECK (path_convention IN ('exact', 'case_insensitive', 'case_insensitive_normalised')),
    -- Exactly one identity shape must be populated, and it must match `kind`.
    -- A network share has no stable identity at all, which is why content_hash
    -- is the relink fallback by design rather than by accident.
    CHECK (
        (kind = 'uuid'           AND uuid IS NOT NULL AND windows_serial IS NULL AND host IS NULL AND share IS NULL) OR
        (kind = 'windows_serial' AND uuid IS NULL AND windows_serial IS NOT NULL AND host IS NULL AND share IS NULL) OR
        (kind = 'network_share'  AND uuid IS NULL AND windows_serial IS NULL AND host IS NOT NULL AND share IS NOT NULL)
    )
);

CREATE UNIQUE INDEX volumes_identity ON volumes (kind, ifnull(uuid, ''), ifnull(windows_serial, -1), ifnull(host, ''), ifnull(share, ''));

CREATE TABLE folders (
    id              INTEGER PRIMARY KEY,
    volume_id       INTEGER NOT NULL REFERENCES volumes (id) ON DELETE CASCADE,
    parent_id       INTEGER REFERENCES folders (id) ON DELETE CASCADE,
    -- As the filesystem spells it, separators normalised to '/'. This is what
    -- reopens the folder.
    relative_path   TEXT    NOT NULL,
    -- What comparison uses, derived under the volume's convention. Two columns
    -- because storing only the normalised form eventually fails to open a
    -- directory whose real name is not the normalised one.
    path_key        TEXT    NOT NULL
);

CREATE UNIQUE INDEX folders_by_key ON folders (volume_id, path_key);

CREATE TABLE files (
    id              INTEGER PRIMARY KEY,
    folder_id       INTEGER NOT NULL REFERENCES folders (id) ON DELETE CASCADE,
    filename        TEXT    NOT NULL,
    filename_key    TEXT    NOT NULL,
    size            INTEGER NOT NULL,
    -- Seconds since the epoch. Cheap change detection before hashing.
    mtime           INTEGER NOT NULL,
    -- blake3 of the file. The relink key and the duplicate-detection key, and
    -- the reason a moved file is found rather than lost. Nullable because a scan
    -- records a file before it has been read.
    content_hash    TEXT,
    captured_at     INTEGER,
    camera_make     TEXT,
    camera_model    TEXT,
    -- The heuristic triple (captured_at, camera_serial, shutter_count) catches
    -- re-imports of files that were renamed, which a hash cannot.
    camera_serial   TEXT,
    shutter_count   INTEGER,
    lens            TEXT,
    -- Flagged by a scan, not discovered on access: a library tells you what is
    -- missing before you go looking for it.
    missing         INTEGER NOT NULL DEFAULT 0 CHECK (missing IN (0, 1)),
    imported_at     INTEGER NOT NULL
);

CREATE UNIQUE INDEX files_by_key ON files (folder_id, filename_key);
CREATE INDEX files_by_hash ON files (content_hash) WHERE content_hash IS NOT NULL;
CREATE INDEX files_by_capture ON files (captured_at);

-- One RAW can carry several edits. A virtual copy is a second image over the
-- same file, not a second file.
CREATE TABLE images (
    id              INTEGER PRIMARY KEY,
    file_id         INTEGER NOT NULL REFERENCES files (id) ON DELETE CASCADE,
    is_virtual_copy INTEGER NOT NULL DEFAULT 0 CHECK (is_virtual_copy IN (0, 1)),
    copy_name       TEXT,
    -- Culling metadata, on the image rather than the file so two copies of one
    -- frame can be rated apart.
    rating          INTEGER CHECK (rating BETWEEN 0 AND 5),
    flag            TEXT    CHECK (flag IN ('pick', 'reject')),
    colour_label    TEXT,
    created_at      INTEGER NOT NULL
);

CREATE INDEX images_by_file ON images (file_id);

-- Versioned, and that is the whole point. Overwriting would make the history
-- panel impossible and would throw away the record of a model proposal being
-- corrected by a person — which is exactly one supervised example, and cannot
-- be reconstructed afterwards.
CREATE TABLE edit_states (
    id               INTEGER PRIMARY KEY,
    image_id         INTEGER NOT NULL REFERENCES images (id) ON DELETE CASCADE,
    -- Monotonic per image, starting at 1.
    version          INTEGER NOT NULL,
    -- The serialised EditState. Kept as text rather than as columns because
    -- rawkit-editstate owns its shape: a schema that mirrors those fields would
    -- have to migrate every time the renderer learns a new one.
    json             TEXT    NOT NULL,
    -- EditState::content_hash of the above. What the preview cache is keyed on.
    edit_state_hash  TEXT    NOT NULL,
    -- 'user' | 'preset' | 'import' | 'model'. Written from
    -- rawkit_catalog::source_column so the strings cannot drift from the enum.
    source           TEXT    NOT NULL CHECK (source IN ('user', 'preset', 'import', 'model')),
    created_at       INTEGER NOT NULL
);

CREATE UNIQUE INDEX edit_states_version ON edit_states (image_id, version);
CREATE INDEX edit_states_by_hash ON edit_states (edit_state_hash);
