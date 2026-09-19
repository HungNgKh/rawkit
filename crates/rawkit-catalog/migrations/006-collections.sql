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
    -- The name as it is compared: normalised and lower-cased, by
    -- `collections::name_key`. "Portfolio" and "portfolio" are one name to the
    -- person reading a list, and two rows that differ only by a capital are a
    -- coin toss every time one is picked. Stored rather than computed by a
    -- collation, the way `path_key` and `filename_key` are, so the rule is one
    -- function in Rust and not whatever this build of SQLite thinks case is —
    -- `NOCASE` folds ASCII and nothing else.
    --
    -- **Decided now because it can only be decided now.** A uniqueness rule can
    -- be loosened on any catalog there will ever be; it can only be tightened
    -- on one that does not already break it.
    name_key   TEXT    NOT NULL,
    -- The one collection a keypress adds to, in the sense Lightroom's quick
    -- collection has. A column rather than a reserved name, because a name is
    -- something a person can type, rename, or collide with by accident.
    is_quick   INTEGER NOT NULL DEFAULT 0 CHECK (is_quick IN (0, 1)),
    created_at INTEGER NOT NULL,
    -- The quick collection lives at the top level and cannot be moved under
    -- anything. `collections::remove` refuses to delete it, but it guards the id
    -- it is handed: nest the quick collection inside another and deleting *that*
    -- cascades it away, leaving a catalog the shell cannot open. Nothing
    -- re-parents a collection yet, which is exactly when a rule like this is
    -- free to add.
    CHECK (is_quick = 0 OR parent_id IS NULL)
);

-- Siblings cannot share a name, compared the way a person compares them.
-- `parent_id` is NULL at the top level and SQLite treats NULLs as distinct in a
-- UNIQUE index, so the top level needs its own partial index to get the rule.
CREATE UNIQUE INDEX collections_sibling_name
    ON collections (parent_id, name_key) WHERE parent_id IS NOT NULL;
-- The quick collection is outside the rule. What identifies it is `is_quick`,
-- and its name is an English label this file chose — so without the exemption a
-- library imported from elsewhere with a top-level collection of the same name
-- would fail on a row the user never made and cannot see a reason for.
CREATE UNIQUE INDEX collections_root_name
    ON collections (name_key) WHERE parent_id IS NULL AND is_quick = 0;

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
-- photograph went from 27 ms to open to 130 ms. Two b-trees now, and each one
-- answers a question something actually asks.
--
-- **The key leads with the image, and that is the half that was wrong.** It was
-- `(collection_id, image_id)`, which left nothing to find a photograph's
-- memberships by — so the cascade when an image is deleted scanned the whole
-- table. Six milliseconds for one, which was weighed and accepted; what was not
-- weighed is that it is paid *per image*. Deleting two hundred — "throw away the
-- rejects", the most ordinary thing a cull ends with — measured 49 ms, 247 ms
-- and 1.1 s at one, five and twenty thousand photographs. With the image first
-- the cascade is an index search, "is this frame in this collection" is still a
-- point lookup because it names both columns, and "which collections is this
-- frame in" has an answer the day something asks. No third b-tree: the question
-- was never whether to index `image_id`, it was which of the two the table
-- should be sorted by, and the order index below already serves everything that
-- starts from a collection.
CREATE TABLE collection_images (
    collection_id INTEGER NOT NULL REFERENCES collections (id) ON DELETE CASCADE,
    -- An *image*, not a file, and the difference is virtual copies: two
    -- interpretations of one frame are two rows in `images`, and either can be
    -- in a collection without the other. Pointing at the file would make a
    -- collection unable to hold the black-and-white version and not the colour.
    image_id      INTEGER NOT NULL REFERENCES images (id) ON DELETE CASCADE,
    -- Where this frame sits in the hand-made sequence.
    --
    -- **An opaque sort key, and the contract is that nothing else is promised.**
    -- Not unique, not contiguous, never shown and never exported; reads order by
    -- `(position, image_id)` so equal values still have one answer. That is what
    -- keeps the numbering scheme free to change: dense today, and spreading the
    -- values out to make room for moving a block is one `UPDATE` on any catalog
    -- there will ever be, because nothing depends on what the numbers *are*.
    --
    -- Sparse and not necessarily contiguous: appending takes the current maximum and adds one,
    -- and moving a frame exchanges two values. Nothing reads the absolute
    -- number, so gaps left by a removal cost nothing and closing them would be a
    -- write per remaining row for no visible difference.
    position      INTEGER NOT NULL,
    -- Adding a photograph twice is not an error and does not make two rows; it
    -- is somebody pressing the key again on a frame already in the collection.
    -- Also what answers "is this frame in this collection", asked on every
    -- keypress of a cull.
    PRIMARY KEY (image_id, collection_id)
) WITHOUT ROWID;

-- Everything that starts from a collection: reading it in order, the maximum
-- an append needs, its count, emptying it, and the cascade when it is deleted.
-- In a `WITHOUT ROWID` table an index carries the key, so this is
-- `(collection_id, position, image_id)` on disk and reading a collection never
-- touches the table at all.
CREATE INDEX collection_images_order ON collection_images (collection_id, position);

-- And it exists from the moment the table does.
--
-- Created here rather than on first use so that "there is exactly one quick
-- collection" needs no code to be true. Making it lazily would mean a name that
-- might already be taken by a collection the user made, and a first press of the
-- key that can fail for a reason nobody would connect to it.
INSERT INTO collections (parent_id, name, name_key, is_quick, created_at)
VALUES (NULL, 'Quick Collection', 'quick collection', 1, CAST(strftime('%s', 'now') AS INTEGER));
