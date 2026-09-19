//! Does the catalog still work at the size of a real library?
//!
//! # Why this exists
//!
//! Every catalog test in this repo builds a handful of rows, and the largest
//! fixture anywhere was **200** — of volumes, not even images. The P1 exit gate
//! is twenty thousand photographs. That is a factor of a hundred between what
//! is tested and what is claimed, and it had never been closed.
//!
//! It matters more than it sounds. The queries here run on a keystroke: a cull
//! narrows the set on every rating, and the grid asks for the sequence again
//! each time. Something accidentally quadratic is invisible on ten photographs
//! and makes the application feel dead on twenty thousand — and by the time it
//! is noticed, it is noticed while trying to use the thing rather than while
//! building it.
//!
//! # What it measures, and what it deliberately does not
//!
//! It builds the library through `scan_on`, the real path, on real files in a
//! temporary directory — so the numbers include the scan and the schema rather
//! than a hand-written approximation of them. It sweeps three sizes rather than
//! testing one, because a single number cannot tell linear from quadratic and
//! the **shape** is the question.
//!
//! It does **not** decode anything or render a preview image. Preview *rows* are
//! recorded, because half the table being populated is what makes `outstanding`
//! do its real work — but twenty thousand real RAW files is a different test
//! with a different cost, and mixing the two would hide a slow query behind a
//! slow decoder.
//!
//! `cargo test -p rawkit-catalog --test scale -- --ignored --nocapture`

use rawkit_catalog::collections;
use rawkit_catalog::cull::{self, Filter, Flag, Flagged, Judgement};
use rawkit_catalog::db::Catalog;
use rawkit_catalog::previews::{self, Level, Preview};
use rawkit_catalog::scan::{scan_on, FileMetadata};
use rawkit_catalog::VolumeId;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The sizes swept. The largest is the P1 gate; the two below it are there so
/// that a cost growing faster than the row count shows up as a curve rather
/// than as one number nobody has anything to compare against.
const SIZES: [usize; 3] = [1_000, 5_000, 20_000];

/// How long an interaction may take before it stops feeling like direct
/// manipulation.
///
/// Not a round number chosen for looking strict: it is roughly the point at
/// which a keypress stops appearing to cause its own result, and it is the
/// budget the *whole* keystroke has to fit in — the query, the render request
/// and the redraw. A query that eats all of it has eaten the budget.
const INSTANT: Duration = Duration::from_millis(100);

fn tempdir() -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "rawkit-scale-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&base).expect("temp dir");
    base
}

/// A library of `n` raws, scanned in, with judgements spread through it.
///
/// Capture times ascend but the filenames do not follow them — every third file
/// is dated out of order — so the sequence's sort has real work to do rather
/// than being handed rows that happen to arrive sorted.
fn library_of(root: &Path, n: usize) -> Catalog {
    let photos = root.join("photos");
    let _ = std::fs::remove_dir_all(&photos);
    // And the catalog, which the three sizes used to share.
    //
    // Only the *files* were cleared between them, so every row the previous
    // size wrote stayed — surviving as `missing = 1`, which `sequence` filters,
    // which is why this was invisible for as long as the table held nothing but
    // images. It stopped being invisible the moment something in the fixture
    // had a name: the second size tried to make a second collection called
    // "Portfolio". A fixture that says "a library of n photographs" has to be
    // one, or the numbers underneath belong to a library nobody described.
    let _ = std::fs::remove_file(root.join("library.rawkit"));
    std::fs::create_dir_all(&photos).expect("photo dir");
    for i in 0..n {
        std::fs::write(photos.join(format!("DSC{i:06}.ARW")), b"raw").expect("write");
    }

    let mut catalog = Catalog::open(&root.join("library.rawkit")).expect("open");
    scan_on(
        &mut catalog,
        &photos,
        VolumeId::Uuid("scale-volume".into()),
        |path: &Path| {
            let stem = path.file_stem()?.to_string_lossy().into_owned();
            let i: i64 = stem.trim_start_matches("DSC").parse().ok()?;
            Some(FileMetadata {
                // Shuffled against the filename, so the ORDER BY is not a
                // no-op on already-ordered input.
                captured_at: if i % 3 == 0 {
                    Some(1_700_000_000 + (i * 7) % 100_000)
                } else {
                    Some(1_700_000_000 + i)
                },
                ..FileMetadata::default()
            })
        },
    )
    .expect("scan");

    // A realistic spread: most frames untouched, a fifth rated, a tenth picked,
    // a few rejected. A filter that matches everything or nothing measures the
    // wrong thing.
    let hash = fixture_hash();
    let mut in_collection: Vec<i64> = Vec::new();
    let everything = cull::sequence(&catalog, &Filter::default()).expect("sequence");
    for (i, image) in everything.iter().enumerate() {
        let judgement = Judgement {
            rating: if i % 5 == 0 {
                Some((i % 6) as u8)
            } else {
                None
            },
            flag: match i % 10 {
                0 => Some(Flag::Pick),
                7 => Some(Flag::Reject),
                _ => None,
            },
            colour: if i % 13 == 0 {
                Some("red".into())
            } else {
                None
            },
        };
        cull::set(&catalog, image.id, &judgement).expect("set");

        // A tenth of the library goes into a collection, in an order that is
        // not the library's. Membership is what makes `all`'s GROUP BY do real
        // work, and the scrambled order is what makes `members` prove it is
        // reading `position` rather than falling back to capture time.
        if i % 10 == 3 {
            in_collection.push(image.id);
        }

        // Half the library already has its grid thumbnail, which is the state a
        // part-built library is in for most of its life — and the state that
        // makes `outstanding` do the most work, since it has to look at every
        // image to find out which half it is.
        if i % 2 == 0 {
            previews::record(
                &catalog,
                image.id,
                &Preview {
                    level: Level::Thumb,
                    path: previews::relative_path(image.id, Level::Thumb, &hash),
                    edit_state_hash: hash.clone(),
                    renderer: RENDERER.into(),
                    width: 256,
                    height: 171,
                    bytes: 9000,
                },
            )
            .expect("record");
        }
    }

    // Reversed, so the stored order disagrees with the library's on every row.
    in_collection.reverse();
    let shelf = collections::create(&catalog, "Portfolio", None).expect("create");
    collections::add(&catalog, shelf, &in_collection).expect("add");
    // And the quick collection, which every keystroke asks about.
    let quick = collections::quick(&catalog).expect("quick");
    collections::add(&catalog, quick, &in_collection[..in_collection.len() / 4]).expect("add");

    // **A heavy user, because two collections measured nothing.** The first
    // version of this fixture had the quick collection and one more, and every
    // number it produced was flattering: the listing counts membership across
    // the *whole catalog*, so its cost is set by how many collections somebody
    // has made over the years, not by the one on screen. A hundred of them at a
    // twentieth of the library each, and one that holds everything — six
    // memberships per photograph, which is a library that has been used.
    let ids: Vec<i64> = everything.iter().map(|image| image.id).collect();
    for c in 0..HEAVY_COLLECTIONS {
        let id = collections::create(&catalog, &format!("Set {c:03}"), None).expect("create");
        let chosen: Vec<i64> = ids.iter().skip(c % 20).step_by(20).copied().collect();
        collections::add(&catalog, id, &chosen).expect("add");
    }
    let whole = collections::create(&catalog, "Everything", None).expect("create");
    collections::add(&catalog, whole, &ids).expect("add");
    catalog
}

/// The hash of the edit every preview in the fixture was rendered for.
///
/// It has to be the hash of the **default** edit and not an arbitrary string,
/// because that is what `outstanding` compares against for an image nobody has
/// edited. Getting that wrong is not a silent inaccuracy — it makes every
/// recorded preview look stale, so the fixture measures a library with no
/// previews at all while appearing to have half of them. That is precisely how
/// this was first written, and the assertion below is what caught it.
fn fixture_hash() -> String {
    rawkit_editstate::EditState::default().content_hash()
}
const RENDERER: &str = "scale-test/0";
/// How many collections the heavy-user fixture makes, beside the three named ones.
const HEAVY_COLLECTIONS: usize = 100;

fn timed<T>(mut f: impl FnMut() -> T) -> (Duration, T) {
    let start = Instant::now();
    let out = f();
    (start.elapsed(), out)
}

#[test]
#[ignore = "writes twenty-six thousand files; a few seconds and a lot of inodes"]
fn the_catalog_holds_up_at_the_size_of_a_real_library() {
    let root = tempdir();
    println!(
        "{:>8} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>10}",
        "images", "scan", "open", "seq all", "seq pick", "match", "tally", "prev 1", "outstandng"
    );
    println!(
        "{:>8} {:>9} {:>9} {:>9} {:>10} {:>9} {:>10} {:>10} {:>9} {:>9}",
        "",
        "coll all",
        "members",
        "holds 1",
        "mem whole",
        "move 1",
        "move again",
        "reorder *",
        "drop 1",
        "drop 200"
    );

    let mut worst = Duration::ZERO;
    let mut worst_what = String::new();
    for n in SIZES {
        let (build, catalog) = timed(|| library_of(&root, n));
        drop(catalog);

        // A cold open, because that is what starting the application does —
        // and it is where a migration or an integrity check would show up.
        let (open, catalog) = timed(|| Catalog::open(&root.join("library.rawkit")).expect("open"));

        let (all, rows) = timed(|| cull::sequence(&catalog, &Filter::default()).expect("seq"));
        assert_eq!(rows.len(), n, "the scan did not produce {n} images");

        let picks = Filter::flagged(Flagged::Pick);
        let (pick, _) = timed(|| cull::sequence(&catalog, &picks).expect("seq"));

        // The question a cull asks after every keypress, so it is timed over a
        // hundred of them and reported per call.
        let ids: Vec<i64> = rows.iter().step_by(n / 100).map(|i| i.id).collect();
        let (matched, _) = timed(|| {
            for id in &ids {
                cull::matches(&catalog, *id, &picks).expect("matches");
            }
        });
        let per_match = matched / ids.len() as u32;

        let (tally, _) = timed(|| cull::tally(&catalog).expect("tally"));

        // What the grid does per visible cell, so timed per call like `matches`.
        let (looked, _) = timed(|| {
            for id in &ids {
                previews::lookup(&catalog, *id, Level::Thumb).expect("lookup");
            }
        });
        let per_lookup = looked / ids.len() as u32;

        // And what the background builder does to find out what is left.
        //
        // **An N+1 shape, and it is in this table because of that**: a query
        // per image, plus one per level, plus an edit hashed each time. It
        // measures linear and costs 300 ms at twenty thousand — which is
        // nothing, because `rawkit-cli`'s builder calls it *once* and then
        // works through the list it returns, and the job that follows decodes
        // twenty thousand RAW files. Checked rather than assumed: that is the
        // only call site in the workspace.
        //
        // Recorded anyway, because the shape is the kind that stops being
        // harmless quietly. Anything that starts asking it per image, or moves
        // it onto a path a finger is waiting on, turns a linear cost into a
        // quadratic one without changing this function at all.
        let (left, wanted) = timed(|| {
            previews::outstanding(&catalog, &[Level::Thumb], RENDERER).expect("outstanding")
        });

        // **These three are on the keystroke path**, which is why they are
        // here. `CullView` is rebuilt after every key a cull presses, and it
        // now asks for the list of collections and whether this frame is in the
        // quick one. A GROUP BY over membership that grew with the library
        // would make every keypress slower on a library that had used the
        // feature — the shape `outstanding` is watched for, on a path a finger
        // is actually waiting on.
        let (coll_all, listed) = timed(|| collections::all(&catalog).expect("collections"));
        assert!(
            listed.iter().any(|c| c.is_quick) && listed.len() == HEAVY_COLLECTIONS + 3,
            "the fixture should have the quick collection, Portfolio, Everything \
             and the heavy set: {} listed",
            listed.len()
        );
        let shelf = listed
            .iter()
            .find(|c| c.name == "Portfolio")
            .expect("Portfolio")
            .id;
        let whole = listed
            .iter()
            .find(|c| c.name == "Everything")
            .expect("Everything")
            .id;
        let (members, held) =
            timed(|| collections::members(&catalog, shelf, &Filter::default()).expect("members"));
        assert_eq!(held.len(), n / 10, "a tenth of the library is in it");
        let (asked, _) = timed(|| {
            for id in &ids {
                collections::holds(&catalog, shelf, *id).expect("holds");
            }
        });
        let per_holds = asked / ids.len() as u32;

        // The largest collection there can be, read whole — what switching the
        // grid to it costs.
        let (members_whole, everything_in) =
            timed(|| collections::members(&catalog, whole, &Filter::default()).expect("members"));
        assert_eq!(everything_in.len(), n);

        // **Moving one photograph one place**, which is a keypress and has a
        // keypress's budget. Two rows change, so two rows are written.
        let order: Vec<i64> = everything_in.iter().map(|image| image.id).collect();
        let (moved, _) = timed(|| {
            collections::swap(&catalog, whole, order[n / 2], order[n / 2 + 1]).expect("swap")
        });

        // **And again**, because the first write after a catalog opens also pays
        // for creating the write-ahead log, which is not the move's cost. The
        // second is what holding Shift and pressing an arrow twice feels like.
        let (moved_again, _) = timed(|| {
            collections::swap(&catalog, whole, order[n / 2], order[n / 2 + 1]).expect("swap")
        });

        // And the same move done the way the shell *first* did it: the whole
        // sequence handed to `reorder`, a write per member. Reported and kept
        // off the interaction budget for the reason `outstanding` is — nothing
        // a finger waits on calls it — and printed so that stays a decision
        // somebody can see rather than one they would have to rediscover.
        let mut wholesale = order.clone();
        wholesale.swap(n / 2, n / 2 + 1);
        let (rewritten, _) =
            timed(|| collections::reorder(&catalog, whole, &wholesale).expect("reorder"));

        // **What the missing index costs.** There is no index on `image_id`
        // alone, so the cascade when an image is deleted scans every membership
        // there is. That is a virtual copy being thrown away, not a keypress,
        // but it is measured rather than assumed — and it goes last, because
        // it changes the library the rest of this measured.
        let doomed = order[n / 3];
        let (dropped, _) = timed(|| {
            catalog
                .connection()
                .execute("DELETE FROM images WHERE id = ?1", [doomed])
                .expect("delete")
        });

        // **Two hundred at once, which is what "delete the rejects" is.** The
        // cascade looks up each deleted image's memberships, so whatever one
        // costs, this costs two hundred times — and a scan per image is a
        // quadratic nobody sees while the only caller deletes one virtual copy.
        let many: Vec<i64> = order
            .iter()
            .skip(7)
            .step_by(n / 250)
            .take(200)
            .copied()
            .collect();
        let (dropped_many, _) = timed(|| {
            let connection = catalog.connection();
            let transaction = connection.unchecked_transaction().expect("begin");
            for id in &many {
                transaction
                    .execute("DELETE FROM images WHERE id = ?1", [id])
                    .expect("delete");
            }
            transaction.commit().expect("commit");
        });

        println!(
            "{n:>8} {:>8.0?} {:>8.1?} {:>8.1?} {:>8.1?} {:>8.1?} {:>8.1?} {:>8.1?} {:>9.1?}",
            build, open, all, pick, per_match, tally, per_lookup, left
        );
        println!(
            "{:>8} {:>8.1?} {:>8.1?} {:>8.1?} {:>9.1?} {:>9.1?} {:>9.1?} {:>9.1?} {:>9.1?} {:>9.1?}",
            "", coll_all, members, per_holds, members_whole, moved, moved_again, rewritten, dropped, dropped_many
        );
        assert!(
            !wanted.is_empty() && wanted.len() < n,
            "the fixture should leave some previews outstanding and not all: {} of {n}",
            wanted.len()
        );

        // The scan is excluded from the interaction budget on purpose: it runs
        // once, with a progress indicator, when a card is imported. Everything
        // else here happens under a finger.
        for (what, took) in [
            ("open", open),
            ("sequence", all),
            ("filtered sequence", pick),
            ("matches", per_match),
            ("tally", tally),
            ("preview lookup", per_lookup),
            ("collection list", coll_all),
            ("collection members", members),
            ("collection holds", per_holds),
            ("whole-library collection", members_whole),
            ("moving one photograph", moved),
            ("moving another", moved_again),
            ("deleting one image", dropped),
            ("deleting two hundred images", dropped_many),
            // `outstanding` is excluded from the interaction budget and
            // reported anyway: it runs once, off the interactive path, to
            // decide what to build. What would matter is it growing faster than
            // the library — which the three rows are there to show.
        ] {
            if took > worst {
                worst = took;
                worst_what = format!("{what} at {n}");
            }
        }
    }

    let _ = std::fs::remove_dir_all(&root);
    assert!(
        worst < INSTANT,
        "{worst_what} took {worst:.1?}, past the {INSTANT:?} an interaction has \
         to fit in. The table above says whether it is a constant cost or a \
         shape — three sizes are printed precisely so that can be told apart."
    );
}
