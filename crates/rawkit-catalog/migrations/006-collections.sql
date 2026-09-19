-- A collection is a sequence somebody made by hand.
--
-- That is the whole justification for a second way of choosing photographs when
-- the library already has filters. A filter answers "which frames satisfy this
-- rule"; it is re-evaluated, it is in whatever order the library sorts by, and
-- nobody can put one frame before another inside it. A collection answers "these
-- ones, in this order, because I said so" — a print run, a submission, an edit.
-- The `position` column below is the difference, and it is the reason this table
-- exists rather than a saved filter.
CREATE TABLE collections (
    id         INTEGER PRIMARY KEY,
    -- Collections nest. A parent is a collection like any other and may hold
    -- photographs of its own, which is a deliberate simplification of
    -- Lightroom's split between collections and collection sets: two kinds of
    -- container is a rule to learn, and the only thing it buys is forbidding
    -- something nobody wanted to do.
    parent_id  INTEGER REFERENCES collections (id) ON DELETE CASCADE,
    name       TEXT    NOT NULL,
    -- The one collection a keypress adds to, in the sense Lightroom's quick
    -- collection has. A column rather than a reserved name, because a name is
    -- something a person can type, rename, or collide with by accident.
    is_quick   INTEGER NOT NULL DEFAULT 0 CHECK (is_quick IN (0, 1)),
    created_at INTEGER NOT NULL
);

-- Siblings cannot share a name. Two collections called "Portfolio" under one
-- parent are indistinguishable in a list, which is where they are chosen from.
-- `parent_id` is NULL at the top level and SQLite treats NULLs as distinct in a
-- UNIQUE index, so the top level needs its own partial index to get the rule.
CREATE UNIQUE INDEX collections_sibling_name
    ON collections (parent_id, name) WHERE parent_id IS NOT NULL;
CREATE UNIQUE INDEX collections_root_name
    ON collections (name) WHERE parent_id IS NULL;

-- At most one quick collection, enforced here rather than by the code that
-- creates it: "there is exactly one" is a property of the catalog, and a second
-- one arriving through an import or a future migration should fail loudly
-- instead of leaving the keypress with two places to put a photograph.
CREATE UNIQUE INDEX collections_one_quick ON collections (is_quick) WHERE is_quick = 1;

-- Membership, and the order it was put in.
CREATE TABLE collection_images (
    collection_id INTEGER NOT NULL REFERENCES collections (id) ON DELETE CASCADE,
    image_id      INTEGER NOT NULL REFERENCES images (id) ON DELETE CASCADE,
    -- Where this frame sits in the hand-made sequence. Sparse and not
    -- necessarily contiguous: appending takes the current maximum and adds one,
    -- and a reorder rewrites only the rows that moved. Nothing reads the
    -- absolute value, so gaps left by a removal cost nothing and closing them
    -- would be a write per remaining row for no visible difference.
    position      INTEGER NOT NULL,
    added_at      INTEGER NOT NULL,
    -- Adding a photograph twice is not an error and does not make two rows; it
    -- is somebody pressing the key again on a frame already in the collection.
    PRIMARY KEY (collection_id, image_id)
);

-- The order a collection is read in, which is every read of one.
CREATE INDEX collection_images_order ON collection_images (collection_id, position);
-- And the reverse: "which collections is this frame in", asked once per frame
-- while culling to light the indicator.
CREATE INDEX collection_images_by_image ON collection_images (image_id);

-- And it exists from the moment the table does.
--
-- Created here rather than on first use so that "there is exactly one quick
-- collection" needs no code to be true. Making it lazily would mean a name that
-- might already be taken by a collection the user made, and a first press of the
-- key that can fail for a reason nobody would connect to it.
INSERT INTO collections (parent_id, name, is_quick, created_at)
VALUES (NULL, 'Quick Collection', 1, CAST(strftime('%s', 'now') AS INTEGER));
