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
--
-- **An index into the library, and built like one.** A collection copies
-- nothing: not the photograph, not its edit, not a preview. A membership is
-- three integers saying "this image, in this collection, at this place", and the
-- table is shaped so that is all it costs on disk as well.
--
-- `WITHOUT ROWID` makes the primary key *be* the table rather than an index
-- beside one. The first shape of this had four b-trees per membership — the
-- rowid table, the key's index, the order index and a by-image index — and it
-- mattered somewhere nobody was looking: opening a catalog runs a full integrity
-- check, which reads every page, so a library with six memberships per
-- photograph went from 27 ms to open to 130 ms. Two b-trees now, and each one is
-- a question something actually asks.
CREATE TABLE collection_images (
    collection_id INTEGER NOT NULL REFERENCES collections (id) ON DELETE CASCADE,
    -- An *image*, not a file, and the difference is virtual copies: two
    -- interpretations of one frame are two rows in `images`, and either can be
    -- in a collection without the other. Pointing at the file would make a
    -- collection unable to hold the black-and-white version and not the colour.
    image_id      INTEGER NOT NULL REFERENCES images (id) ON DELETE CASCADE,
    -- Where this frame sits in the hand-made sequence. Sparse and not
    -- necessarily contiguous: appending takes the current maximum and adds one,
    -- and moving a frame exchanges two values. Nothing reads the absolute
    -- number, so gaps left by a removal cost nothing and closing them would be a
    -- write per remaining row for no visible difference.
    position      INTEGER NOT NULL,
    -- Adding a photograph twice is not an error and does not make two rows; it
    -- is somebody pressing the key again on a frame already in the collection.
    -- Also what answers "is this frame in this collection", asked on every
    -- keypress of a cull.
    PRIMARY KEY (collection_id, image_id)
) WITHOUT ROWID;

-- The order a collection is read in, which is every read of one, and the
-- maximum an append needs.
--
-- There is deliberately **no index on `image_id` alone**. The first shape had
-- one for "which collections is this frame in", which nothing asks — and an
-- index nobody queries is a b-tree every write maintains and every open checks.
-- What it would also have served is the cascade when an image is deleted, which
-- without it scans this table; that is measured in the scale gate rather than
-- assumed, and it is a virtual copy being thrown away, not a keypress.
CREATE INDEX collection_images_order ON collection_images (collection_id, position);

-- And it exists from the moment the table does.
--
-- Created here rather than on first use so that "there is exactly one quick
-- collection" needs no code to be true. Making it lazily would mean a name that
-- might already be taken by a collection the user made, and a first press of the
-- key that can fail for a reason nobody would connect to it.
INSERT INTO collections (parent_id, name, is_quick, created_at)
VALUES (NULL, 'Quick Collection', 1, CAST(strftime('%s', 'now') AS INTEGER));
