//! Previews that build themselves.
//!
//! A catalog nobody had run `rawkit catalog --previews` on used to open as a
//! grid of nothing, and the only cure was a command the window never mentioned.
//! This is the cure moved inside: open a catalog and its previews arrive while
//! you cull.
//!
//! # Who touches what
//!
//! Two threads, and the line between them is the whole design.
//!
//! **The builder** owns a GPU device of its own — the same arrangement
//! [`rawkit_deliver::write`] already ships for export — and does everything
//! that is slow: decode, pyramid, render, resample, encode, write the files. It
//! is handed a [`Wanted`] and hands back [`Preview`]s. **It never opens the
//! catalog.** SQLite here has no `busy_timeout`, so a second connection writing
//! while the window saves an edit would not wait, it would fail; and a builder
//! that can fail to record is a builder that renders a photograph twice.
//!
//! **The render loop** owns the catalog, as it always has, and is the only
//! thing that records. [`Builder::pump`] runs at the top of every frame: it
//! takes what the builder finished, records it under one acquisition of the
//! library's lock, and reads one more page of what is outstanding. Both halves
//! are measured in the scale gate — a page of 64 is 1.4 ms and flat in the size
//! of the library, a photograph's three records well under a millisecond — so a
//! keypress that arrives mid-pump waits a couple of milliseconds of its hundred.
//!
//! # One photograph at a time, from a queue
//!
//! The terminal's build hands its workers a list fixed when it started. Here
//! that would be wrong in a way somebody would see: whatever was listed before
//! a person scrolled gets finished before anything they scrolled *to*. So a
//! worker takes one photograph, and asks again. What is asked for next can
//! change between any two photographs, which is as fine-grained as a build
//! that takes two thirds of a second per photograph can usefully be.

use crate::library::Library;
use crate::sequence::Source;
use rawkit_catalog::previews::{Preview, Wanted};
use rawkit_engine::{render::DEFAULT_TILE, Gpu, Renderer};
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

/// How many photographs the window builds at once.
///
/// One, and the reason is the machine this is developed on: an integrated GPU
/// sharing its memory with everything else, already drawing the canvas. Export
/// uses two, but an export is something a person started and is waiting for;
/// this is something nobody asked for, running under whatever they are doing,
/// and the right amount of their GPU for it to take is as little as still gets
/// there. Raise it only with frame times in hand — see `docs/decisions/previews.md`.
pub const PREVIEW_JOBS: usize = 1;

/// How many photographs one frame asks about. The scale gate's `page 64`.
const PAGE: usize = 64;

/// The most finished photographs one frame records.
///
/// A backstop and not the governor: the channel the builder sends on holds
/// [`PREVIEW_JOBS`], so a pump that stalls makes the builder wait rather than
/// letting results pile up for the frame that finally drains them.
const RECORDS_PER_FRAME: usize = 8;

/// What is waiting to be built, and what is being.
///
/// It owns its own rule — **a photograph is dispatched once** — rather than
/// trusting its callers to check. There is more than one source of work (the
/// walk through the library now; what is on screen and what was just edited,
/// next) and they do not know about each other, so "is this already wanted" has
/// to be answered here or it is answered nowhere, and the price of nowhere is
/// one RAW being decoded twice at once on a GPU this is trying to stay off.
#[derive(Default)]
pub struct Queue {
    far: VecDeque<Wanted>,
    /// Every id in `far`.
    queued: HashSet<i64>,
    /// Every id handed to a worker and not yet accounted for.
    ///
    /// **An id leaves this exactly when its result arrives, or when the result
    /// could not be sent — and for no other reason.** Not "when it is recorded":
    /// a result that fails, or is thrown away, is still a result, and an id
    /// left here by a path that forgot would never be built again.
    in_flight: HashSet<i64>,
}

impl Queue {
    /// Ask for a photograph. `false` if it was already wanted or is being built.
    pub fn push_far(&mut self, wanted: Wanted) -> bool {
        if self.in_flight.contains(&wanted.image_id) || !self.queued.insert(wanted.image_id) {
            return false;
        }
        self.far.push_back(wanted);
        true
    }

    /// The next photograph to build, which from this moment is in flight.
    /// Both halves under the caller's one lock, so there is no instant at which
    /// it is neither and could be asked for again.
    pub fn take_next(&mut self) -> Option<Wanted> {
        let wanted = self.far.pop_front()?;
        self.queued.remove(&wanted.image_id);
        self.in_flight.insert(wanted.image_id);
        Some(wanted)
    }

    /// A result for this photograph has come back, whatever it was.
    pub fn arrived(&mut self, image_id: i64) {
        self.in_flight.remove(&image_id);
    }

    /// Stop wanting everything that has not been started, and say how many
    /// that was. What is in flight finishes: it is most of a second of work
    /// already spent, and its result is as good as any other.
    pub fn forget(&mut self) -> usize {
        let forgotten = self.far.len();
        self.far.clear();
        self.queued.clear();
        forgotten
    }

    pub fn is_idle(&self) -> bool {
        self.far.is_empty() && self.in_flight.is_empty()
    }
}

/// What the builder sends back for one photograph.
pub struct Built {
    pub image_id: i64,
    pub filename: String,
    pub outcome: anyhow::Result<Vec<Preview>>,
}

/// How far along a build is, for the page to draw.
#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    pub done: usize,
    /// How many have been found wanting *so far*.
    pub total: usize,
    /// Whether the library is still being walked, which is whether `total` can
    /// still grow. The page says "of at least" while it can: a total that
    /// climbs under a label that called it the total is a small lie told sixty
    /// times a second.
    pub counting: bool,
    pub filename: String,
}

/// Everything the two threads and the page's commands share.
#[derive(Default)]
pub struct Shared {
    queue: Mutex<Queue>,
    /// Rung when the queue gains something, so an idle builder sleeps on this
    /// rather than waking twenty times a second to find nothing.
    woken: Condvar,
    /// The person said stop. Cleared only by them saying build.
    stopped: AtomicBool,
    /// The person said build.
    restart: AtomicBool,
    /// Somebody is dragging something, and the GPU is theirs. See
    /// [`Builder::hold`].
    held: AtomicBool,
    /// There is no second device to be had, and asking again will not find one.
    broken: AtomicBool,
    progress: Mutex<Option<Progress>>,
}

/// The build the window is running, where a command can reach it.
///
/// A static for the reason `EXPORTING` is one: the page asks over IPC, on a
/// thread that owns nothing. `None` when no catalog is open.
static RUNNING: OnceLock<Arc<Shared>> = OnceLock::new();

/// How far along the build is. `None` when nothing is being built.
pub fn progress() -> Option<Progress> {
    RUNNING.get()?.progress.lock().expect("progress").clone()
}

/// Stop building. `false` when nothing was being built, so the command can say
/// so rather than appear to have done something.
pub fn stop() -> bool {
    let Some(shared) = RUNNING.get() else {
        return false;
    };
    let building = shared.progress.lock().expect("progress").is_some();
    if building {
        shared.stopped.store(true, Ordering::Relaxed);
    }
    building
}

/// Build whatever is outstanding, from the top. `false` with no catalog open.
pub fn restart() -> bool {
    let Some(shared) = RUNNING.get() else {
        return false;
    };
    shared.restart.store(true, Ordering::Relaxed);
    true
}

/// Something the pump has to say. Returned rather than said, so that what the
/// pump decides can be tested without reading a status line other tests write.
#[derive(Debug, PartialEq)]
pub enum Said {
    Info(String),
    Failed(String),
}

/// What one frame of [`Builder::pump`] came to.
#[derive(Debug, Default, PartialEq)]
pub struct Pumped {
    pub said: Vec<Said>,
    /// The photographs whose previews reached the catalog this frame — what the
    /// grid needs in order to stop believing they have none.
    pub recorded: Vec<i64>,
}

/// The render loop's half: what it holds between frames.
pub struct Builder {
    shared: Arc<Shared>,
    results: Receiver<Built>,
    /// Which build is writing these previews; half of what makes one stale.
    stamp: String,
    /// Where the walk through the library has reached. `None` once it has been
    /// through all of it.
    cursor: Option<usize>,
    /// Whose positions `cursor` counts. A collection and the library number
    /// their photographs differently, so a change of source starts the walk
    /// again — cheaply, since everything already built is skipped by the same
    /// test that found it wanting.
    source: Option<Source>,
    found: usize,
    done: usize,
    written: usize,
    failed: Vec<(String, String)>,
    /// Photographs that failed this session, so a second walk does not try them
    /// again and report them again. Forgotten when somebody asks for a build by
    /// name: that is them saying the world has changed.
    gave_up: HashSet<i64>,
    filename: String,
    started: Option<std::time::Instant>,
    was_stopped: bool,
}

impl Builder {
    /// Start the builder's thread for the catalog whose previews live in `dir`.
    pub fn spawn(dir: PathBuf) -> Self {
        let stamp = rawkit_engine::renderer_version(&rawkit_decode::decoder_version());
        let shared = RUNNING.get_or_init(Arc::default).clone();
        let (sender, results) = std::sync::mpsc::sync_channel(PREVIEW_JOBS);
        let theirs = (shared.clone(), stamp.clone());
        std::thread::spawn(move || run(&theirs.0, &sender, &dir, &theirs.1));
        Self::with(shared, results, stamp)
    }

    fn with(shared: Arc<Shared>, results: Receiver<Built>, stamp: String) -> Self {
        Self {
            shared,
            results,
            stamp,
            cursor: None,
            source: None,
            found: 0,
            done: 0,
            written: 0,
            failed: Vec::new(),
            gave_up: HashSet::new(),
            filename: String::new(),
            started: None,
            was_stopped: false,
        }
    }

    /// Keep out of the way while an edit is moving, and come back when it stops.
    ///
    /// Measured on the integrated GPU this is developed on, dragging Contrast
    /// on a 24 MP frame at 4K: 29–43 ms a frame with the builder idle, 46–50 ms
    /// with it rendering, and the worst frame doubled to nearly a tenth of a
    /// second. One job was already the least it could take, so the rest of the
    /// answer is to take nothing while somebody is watching a slider.
    ///
    /// Between photographs only: the one in flight finishes, so the first
    /// second of a drag can still be shared. Stopping inside a photograph needs
    /// the renderer to be interruptible, and is its own change.
    pub fn hold(&self, moving: bool) {
        if self.shared.held.swap(moving, Ordering::Relaxed) && !moving {
            // Under the queue's lock, or the wakeup can be lost: a worker that
            // has read `held` and not yet begun to wait would sleep through
            // this, and once the walk is over nothing else ever rings.
            let _waiting = self.shared.queue.lock().expect("preview queue");
            self.shared.woken.notify_all();
        }
    }

    /// One frame's worth: record what arrived, ask about one more page.
    ///
    /// Never an error. This runs inside the frame, and an error out of the
    /// frame ends the render loop — a preview that could not be recorded is a
    /// thing to say, not a reason for the window to stop drawing.
    pub fn pump(&mut self, library: &Mutex<Library>) -> Pumped {
        let mut said = Vec::new();
        let mut recorded_now = Vec::new();
        if self.shared.broken.load(Ordering::Relaxed) {
            // The builder has already said why. Nothing will ever be taken from
            // the queue, so nothing should be put in it.
            self.cursor = None;
            self.shared.queue.lock().expect("preview queue").forget();
            *self.shared.progress.lock().expect("progress") = None;
            return Pumped::default();
        }
        if self.shared.restart.swap(false, Ordering::Relaxed) {
            self.shared.stopped.store(false, Ordering::Relaxed);
            self.gave_up.clear();
            self.source = None;
        }
        let stopped = self.shared.stopped.load(Ordering::Relaxed);
        let just_stopped = stopped && !self.was_stopped;
        if just_stopped {
            self.cursor = None;
            let forgotten = self.shared.queue.lock().expect("preview queue").forget();
            self.found -= forgotten.min(self.found);
        }
        self.was_stopped = stopped;

        let mut arrived = Vec::new();
        while arrived.len() < RECORDS_PER_FRAME {
            let Ok(built) = self.results.try_recv() else {
                break;
            };
            arrived.push(built);
        }
        if !arrived.is_empty() {
            // First, and whatever becomes of each result below. See `in_flight`.
            let mut queue = self.shared.queue.lock().expect("preview queue");
            for built in &arrived {
                queue.arrived(built.image_id);
            }
        }

        let mut page = None;
        {
            let library = library.lock().expect("library lock");
            for built in arrived {
                self.done += 1;
                self.filename.clone_from(&built.filename);
                let recorded = built.outcome.and_then(|previews| {
                    library.record_previews(built.image_id, &previews)?;
                    Ok(previews.len())
                });
                match recorded {
                    Ok(written) => {
                        self.written += written;
                        recorded_now.push(built.image_id);
                    }
                    Err(why) => {
                        self.gave_up.insert(built.image_id);
                        self.failed.push((built.filename, format!("{why:#}")));
                    }
                }
            }
            if !stopped {
                let source = library.source();
                if self.source != Some(source) {
                    self.source = Some(source);
                    self.cursor = Some(0);
                }
                if let Some(from) = self.cursor {
                    page = Some(library.outstanding_page(from, PAGE, &self.stamp));
                }
            }
        }

        match page {
            Some(Ok(page)) => {
                self.cursor = (!page.finished).then_some(page.next);
                let mut queue = self.shared.queue.lock().expect("preview queue");
                for wanted in page.wanted {
                    if !self.gave_up.contains(&wanted.image_id) && queue.push_far(wanted) {
                        self.found += 1;
                    }
                }
                drop(queue);
                self.shared.woken.notify_all();
            }
            Some(Err(why)) => {
                self.cursor = None;
                said.push(Said::Failed(format!(
                    "Previews stopped building, because the catalog could not be read: {why:#}"
                )));
            }
            None => {}
        }

        let idle = self.shared.queue.lock().expect("preview queue").is_idle();
        let running = self.cursor.is_some() || !idle;
        if running && self.found > 0 {
            self.started.get_or_insert_with(std::time::Instant::now);
        }
        // Stopping with nothing yet done is still something somebody did, and
        // gets its sentence: a command that answers with silence reads as a
        // command that did not work.
        if !running && (self.found > 0 || just_stopped) {
            said.extend(self.finish(stopped));
        }
        let progress = (running && self.found > 0).then(|| Progress {
            done: self.done,
            total: self.found,
            counting: self.cursor.is_some(),
            filename: self.filename.clone(),
        });
        *self.shared.progress.lock().expect("progress") = progress;
        Pumped {
            said,
            recorded: recorded_now,
        }
    }

    /// The run is over: say what it came to, once, and forget the counts.
    fn finish(&mut self, stopped: bool) -> Vec<Said> {
        let built = self.done - self.failed.len();
        let seconds = self
            .started
            .take()
            .map_or(0.0, |s| s.elapsed().as_secs_f64());
        eprintln!(
            "previews   : {built} photograph(s), {} file(s) in {seconds:.1}s{}",
            self.written,
            if stopped { ", stopped" } else { "" }
        );
        let photographs = |n: usize| if n == 1 { "photograph" } else { "photographs" };
        let mut said = vec![Said::Info(if stopped {
            format!(
                "Stopped building previews. {built} {} done",
                photographs(built)
            )
        } else {
            format!("Previews built for {built} {}", photographs(built))
        })];
        // The first named and the rest counted, as an export does it and for
        // the same reason: the cause is nearly always shared.
        if let Some((name, why)) = self.failed.first() {
            said.push(Said::Failed(match self.failed.len() {
                1 => format!("{name} has no preview: {why}"),
                n => format!("{n} photographs have no preview. The first, {name}: {why}"),
            }));
            for (name, why) in self.failed.iter().skip(1) {
                eprintln!("previews   : {name}: {why}");
            }
        }
        (self.found, self.done, self.written) = (0, 0, 0);
        self.failed.clear();
        self.filename.clear();
        said
    }
}

/// The builder's thread, for as long as the window is open.
fn run(shared: &Shared, results: &SyncSender<Built>, dir: &std::path::Path, stamp: &str) {
    // Not until there is something to build. Most catalogs are opened with
    // their previews already made, and a second device is a few hundred
    // megabytes of driver state to hold for a job that is not going to happen.
    {
        let mut queue = shared.queue.lock().expect("preview queue");
        while queue.far.is_empty() {
            queue = shared.woken.wait(queue).expect("preview queue");
        }
    }
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(why) => {
            shared.broken.store(true, Ordering::Relaxed);
            crate::failure(format!(
                "Previews cannot be built in the window on this machine: {why:#}"
            ));
            return;
        }
    };
    let renderer = Renderer::with_tile_size(&gpu, DEFAULT_TILE);
    std::thread::scope(|scope| {
        for _ in 0..PREVIEW_JOBS {
            scope.spawn(|| work(shared, results, &gpu, &renderer, dir, stamp));
        }
    });
}

/// Wait for a photograph to build, for as long as that takes.
fn next(shared: &Shared) -> Wanted {
    let mut queue = shared.queue.lock().expect("preview queue");
    loop {
        if !shared.held.load(Ordering::Relaxed) {
            if let Some(wanted) = queue.take_next() {
                return wanted;
            }
        }
        queue = shared.woken.wait(queue).expect("preview queue");
    }
}

fn work(
    shared: &Shared,
    results: &SyncSender<Built>,
    gpu: &Gpu,
    renderer: &Renderer,
    dir: &std::path::Path,
    stamp: &str,
) {
    loop {
        let wanted = next(shared);
        // A decoder meeting a file it was not written for is the likeliest
        // panic in the project, and here it would take the only worker with it:
        // the photograph would stay in flight for good and the count on screen
        // would stop one short of finishing, with nothing to say why.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rawkit_deliver::previews::one(gpu, renderer, dir, &wanted, stamp)
        }))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("rendering it crashed; see the terminal")));
        let built = Built {
            image_id: wanted.image_id,
            filename: wanted.filename,
            outcome,
        };
        if results.send(built).is_err() {
            // Nobody is listening, which is the window closing. The id is
            // accounted for here because no result will ever be.
            shared
                .queue
                .lock()
                .expect("preview queue")
                .arrived(wanted.image_id);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::tests::{library_at, Scratch};
    use rawkit_catalog::previews::Level;
    use rawkit_editstate::EditState;

    fn wanted(image_id: i64) -> Wanted {
        let state = EditState::default();
        Wanted {
            image_id,
            path: format!("/nowhere/{image_id}.ARW"),
            filename: format!("{image_id}.ARW"),
            edit_state_hash: state.content_hash(),
            state,
            missing: Level::BULK.to_vec(),
        }
    }

    /// A builder with no thread behind it: the test plays the worker, which is
    /// what lets everything the pump decides be checked with no GPU.
    fn builder(stamp: &str) -> (Builder, SyncSender<Built>) {
        let (sender, results) = std::sync::mpsc::sync_channel(8);
        (
            Builder::with(Arc::default(), results, stamp.to_string()),
            sender,
        )
    }

    /// What a worker would send back for a photograph that rendered.
    fn rendered(wanted: &Wanted, stamp: &str) -> Built {
        Built {
            image_id: wanted.image_id,
            filename: wanted.filename.clone(),
            outcome: Ok(wanted
                .missing
                .iter()
                .map(|&level| Preview {
                    level,
                    path: rawkit_catalog::previews::relative_path(
                        wanted.image_id,
                        level,
                        &wanted.edit_state_hash,
                    ),
                    edit_state_hash: wanted.edit_state_hash.clone(),
                    renderer: stamp.to_string(),
                    width: 3,
                    height: 2,
                    bytes: 6,
                })
                .collect()),
        }
    }

    fn take(builder: &Builder) -> Option<Wanted> {
        builder.shared.queue.lock().unwrap().take_next()
    }

    #[test]
    fn a_photograph_asked_for_twice_is_built_once() {
        // The walk through the library and whatever else wants a photograph do
        // not know about each other. The queue is where that stops mattering.
        let mut queue = Queue::default();
        assert!(queue.push_far(wanted(1)));
        assert!(queue.push_far(wanted(2)));
        assert!(!queue.push_far(wanted(1)), "already waiting");
        assert_eq!(queue.take_next().map(|w| w.image_id), Some(1));
        assert!(!queue.push_far(wanted(1)), "being built this moment");
        assert_eq!(queue.take_next().map(|w| w.image_id), Some(2));
        assert!(queue.take_next().is_none());
    }

    #[test]
    fn a_result_of_any_kind_lets_the_photograph_be_asked_for_again() {
        // `arrived` is called before anyone looks at what arrived. An id that
        // only left when its previews were *recorded* would, after one failure,
        // be in flight for the rest of the session and never built.
        let mut queue = Queue::default();
        queue.push_far(wanted(7));
        queue.take_next();
        assert!(!queue.is_idle());
        queue.arrived(7);
        assert!(queue.is_idle());
        assert!(queue.push_far(wanted(7)));
    }

    #[test]
    fn forgetting_the_queue_lets_what_was_started_finish() {
        let mut queue = Queue::default();
        for id in 1..=3 {
            queue.push_far(wanted(id));
        }
        queue.take_next();
        assert_eq!(queue.forget(), 2);
        assert!(queue.take_next().is_none());
        assert!(!queue.is_idle(), "the first is still being built");
        assert!(queue.push_far(wanted(2)), "forgotten is not forbidden");
    }

    #[test]
    fn what_the_builder_finishes_reaches_the_catalog_and_is_not_asked_for_again() {
        let scratch = Scratch::new("building-records");
        let library = Mutex::new(library_at(&scratch.0, 3));
        let (mut builder, worker) = builder("test-build");

        // One frame finds all three, and says nothing yet.
        assert_eq!(builder.pump(&library), Pumped::default());
        let progress = builder.shared.progress.lock().unwrap().clone().unwrap();
        assert_eq!((progress.done, progress.total), (0, 3));
        assert!(!progress.counting, "three photographs are one page");

        let mut recorded = Vec::new();
        while let Some(next) = take(&builder) {
            worker.send(rendered(&next, "test-build")).unwrap();
            recorded.extend(builder.pump(&library).recorded);
        }
        assert_eq!(recorded.len(), 3);
        assert!(builder.shared.progress.lock().unwrap().is_none());

        // Asked from the top, the way "build the previews" asks: nothing is
        // wanting, so nothing is queued and there is nothing to report.
        builder.shared.restart.store(true, Ordering::Relaxed);
        assert_eq!(builder.pump(&library), Pumped::default());
        assert!(take(&builder).is_none());
    }

    #[test]
    fn the_run_says_what_it_came_to_once_and_names_a_failure() {
        let scratch = Scratch::new("building-says");
        let library = Mutex::new(library_at(&scratch.0, 2));
        let (mut builder, worker) = builder("test-build");
        builder.pump(&library);

        let first = take(&builder).unwrap();
        worker
            .send(Built {
                image_id: first.image_id,
                filename: first.filename.clone(),
                outcome: Err(anyhow::anyhow!("not a RAW file")),
            })
            .unwrap();
        assert_eq!(builder.pump(&library).said, vec![], "one still to go");

        let second = take(&builder).unwrap();
        worker.send(rendered(&second, "test-build")).unwrap();
        assert_eq!(
            builder.pump(&library).said,
            vec![
                Said::Info("Previews built for 1 photograph".into()),
                Said::Failed(format!("{} has no preview: not a RAW file", first.filename)),
            ]
        );
        assert_eq!(builder.pump(&library), Pumped::default(), "said once");

        // A second walk — coming back from a collection starts one — finds the
        // failed photograph still wanting, and leaves it alone: it has been
        // reported, and trying it every walk would report it every walk.
        builder.source = None;
        builder.pump(&library);
        assert!(take(&builder).is_none());
        // Until somebody asks by name, which is them saying something changed.
        builder.shared.restart.store(true, Ordering::Relaxed);
        builder.pump(&library);
        assert_eq!(take(&builder).map(|w| w.image_id), Some(first.image_id));
    }

    #[test]
    fn a_held_builder_starts_nothing_and_wakes_when_let_go() {
        // The worker's own wait, run for real on a thread with nothing to
        // render: what is under test is that letting go *wakes* it. A lost
        // wakeup here is a build that stops for good the first time somebody
        // touches a slider after the walk through the library has finished.
        let shared: Arc<Shared> = Arc::default();
        let (_sender, results) = std::sync::mpsc::sync_channel(8);
        let builder = Builder::with(shared.clone(), results, "test-build".into());
        builder.hold(true);
        shared.queue.lock().unwrap().push_far(wanted(1));
        shared.woken.notify_all();

        let theirs = shared.clone();
        let worker = std::thread::spawn(move || next(&theirs).image_id);
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!worker.is_finished(), "held, so nothing starts");
        builder.hold(false);
        assert_eq!(worker.join().unwrap(), 1);
    }

    #[test]
    fn stopping_forgets_what_is_waiting_and_counts_what_was_done() {
        let scratch = Scratch::new("building-stops");
        let library = Mutex::new(library_at(&scratch.0, 3));
        let (mut builder, worker) = builder("test-build");
        builder.pump(&library);
        let first = take(&builder).unwrap();

        builder.shared.stopped.store(true, Ordering::Relaxed);
        assert_eq!(
            builder.pump(&library).said,
            vec![],
            "one is still in flight"
        );
        assert!(take(&builder).is_none(), "nothing else starts");

        worker.send(rendered(&first, "test-build")).unwrap();
        let pumped = builder.pump(&library);
        assert_eq!(pumped.recorded, vec![first.image_id]);
        assert_eq!(
            pumped.said,
            vec![Said::Info(
                "Stopped building previews. 1 photograph done".into()
            )]
        );
        // And it stays stopped: a frame later nothing has been queued again.
        builder.pump(&library);
        assert!(take(&builder).is_none());
    }
}
