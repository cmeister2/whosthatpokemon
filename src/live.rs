//! Schedule recognition behind a camera preview without accumulating old frames.
//!
//! Submission and polling only move values through a one-slot mailbox. Full scene
//! detection and matching run on a worker thread. A caller with a card tracker can
//! submit stable card observations and reuse recognition by track ID and revision.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use image::DynamicImage;

use crate::{Error, Matcher, Recognition, RecognitionPipeline, ScanReport, ScannedCard};

/// A card located and, when needed, rectified by a camera-side tracker.
///
/// Keep `track_id` for one physical card only. Increment `revision` whenever its
/// identity may have changed, including a card swap or an unreliable track. A
/// matching cached revision reuses the previous recognition without examining
/// `crop`. Supply a crop until the first result for a revision has completed; it
/// can be `None` on later frames. An uncached card without a crop returns an error
/// for that frame. This explicit contract avoids guessing stability from pixels.
#[derive(Debug)]
pub struct TrackedCard {
    /// Identifier stable for one physical card while it remains in view.
    pub track_id: u64,
    /// Caller-maintained content revision for cache invalidation.
    pub revision: u64,
    /// Clockwise corners in the source frame, starting at a short card edge.
    pub corners: [[f32; 2]; 4],
    /// Area enclosed by the outline, in source-frame square pixels.
    pub area: f32,
    /// Rectified crop required on the first frame of a new revision.
    pub crop: Option<DynamicImage>,
}

/// One camera observation to process on the recognition worker.
#[derive(Debug)]
pub enum CameraFrame {
    /// Detect, rectify, and recognise each card in this full image.
    Scene(DynamicImage),
    /// Use caller-supplied card tracks and skip scene detection.
    Tracked {
        /// Width of the source camera frame.
        width: u32,
        /// Height of the source camera frame.
        height: u32,
        /// Current cards; missing track IDs are evicted from the cache.
        cards: Vec<TrackedCard>,
    },
}

/// Receipt for a nonblocking camera-frame submission.
#[derive(Debug)]
pub struct FrameSubmission {
    /// Monotonically increasing sequence number for the submitted frame.
    pub sequence: u64,
    /// Earlier pending frame displaced by this submission, if any.
    ///
    /// The caller receives ownership so its image buffers can be recycled or
    /// released outside the mailbox lock.
    pub replaced: Option<CameraFrame>,
}

/// Completed recognition associated with its original camera-frame sequence.
#[derive(Debug)]
pub struct FrameOutcome {
    /// Sequence number returned by [`LiveRecognition::submit`].
    pub sequence: u64,
    /// Recognition result or the error from extraction or matching.
    pub result: Result<ScanReport, Error>,
    /// Number of tracked cards matched afresh; `None` for automatic scene scans.
    pub fresh_matches: Option<usize>,
    /// Number of tracked cards whose earlier recognition was reused.
    pub cache_hits: usize,
}

/// A snapshot of the camera worker's bounded queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveStatus {
    /// Most recently submitted frame sequence.
    pub submitted: u64,
    /// Frame currently being processed by the worker, if any.
    pub processing: Option<u64>,
    /// Pending frame awaiting the worker, if any.
    pub pending: Option<u64>,
    /// Most recently completed frame sequence, if any.
    pub completed: Option<u64>,
    /// Count of pending frames displaced by newer submissions.
    pub replaced_frames: u64,
}

/// A reusable recognition worker with at most one pending camera frame.
///
/// `submit` never performs detection or matching. A slow scan may still take
/// seconds, so a reported result always carries its source sequence. A frontend
/// can update its preview independently and decide whether the result still fits
/// its current tracked card. Dropping this object waits for an in-flight scan.
pub struct LiveRecognition {
    /// Shared one-slot request and one-slot result mailbox.
    shared: Arc<Shared>,
    /// Owned worker; joining prevents the matcher from outliving its owner.
    worker: Option<JoinHandle<()>>,
}

/// State guarded by the mailbox mutex.
struct State {
    /// Last issued sequence number.
    submitted: u64,
    /// Request currently waiting for the worker.
    pending: Option<(u64, CameraFrame)>,
    /// Most recently completed, unpolled result.
    outcome: Option<FrameOutcome>,
    /// Sequence being processed, when the worker is busy.
    processing: Option<u64>,
    /// Last completed sequence, including one already polled.
    completed: Option<u64>,
    /// Number of queued frames replaced before processing.
    replaced_frames: u64,
    /// Tells the worker to exit once its current scan ends.
    closed: bool,
}

/// Synchronization primitives for the worker mailbox.
struct Shared {
    /// Protects queue and result state.
    state: Mutex<State>,
    /// Wakes the worker when a new frame arrives.
    ready: Condvar,
}

/// Recognition retained for one caller-declared stable card revision.
struct CachedRecognition {
    /// Revision at which this result was computed.
    revision: u64,
    /// Ranked candidates and acceptance status.
    recognition: Recognition,
}

impl LiveRecognition {
    /// Start a worker around an existing, configured recognition pipeline.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the operating system cannot start a thread.
    pub fn new<M: Matcher + Send + 'static>(pipeline: RecognitionPipeline<M>) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                submitted: 0,
                pending: None,
                outcome: None,
                processing: None,
                completed: None,
                replaced_frames: 0,
                closed: false,
            }),
            ready: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("card-recognition".into())
            .spawn(move || run_worker(pipeline, &worker_shared))?;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }

    /// Replace the pending request with the newest frame and return immediately.
    ///
    /// No image is copied, and an overwritten request is returned to the caller
    /// for buffer reuse. Only the worker performs extraction or matching. The
    /// sequence wraps after `u64::MAX` submissions.
    pub fn submit(&self, frame: CameraFrame) -> FrameSubmission {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.submitted = state.submitted.wrapping_add(1);
        let sequence = state.submitted;
        let replaced = state
            .pending
            .replace((sequence, frame))
            .map(|(_, frame)| frame);
        if replaced.is_some() {
            state.replaced_frames = state.replaced_frames.saturating_add(1);
        }
        self.shared.ready.notify_one();
        FrameSubmission { sequence, replaced }
    }

    /// Take the most recent completed result without waiting for recognition.
    ///
    /// If several results complete before polling, only the newest is retained.
    pub fn poll(&self) -> Option<FrameOutcome> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .outcome
            .take()
    }

    /// Inspect worker progress without waiting for extraction or matching.
    pub fn status(&self) -> LiveStatus {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        LiveStatus {
            submitted: state.submitted,
            processing: state.processing,
            pending: state.pending.as_ref().map(|(sequence, _)| *sequence),
            completed: state.completed,
            replaced_frames: state.replaced_frames,
        }
    }
}

impl Drop for LiveRecognition {
    fn drop(&mut self) {
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.closed = true;
            self.shared.ready.notify_one();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Drain one request at a time, retaining only the newest unpolled result.
fn run_worker<M: Matcher>(pipeline: RecognitionPipeline<M>, shared: &Shared) {
    let mut cache = HashMap::<u64, CachedRecognition>::new();
    loop {
        let (sequence, frame) = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            while state.pending.is_none() && !state.closed {
                state = shared.ready.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            if state.closed {
                return;
            }
            let Some((sequence, frame)) = state.pending.take() else {
                continue;
            };
            state.processing = Some(sequence);
            (sequence, frame)
        };
        let (result, fresh_matches, cache_hits) = process_frame(&pipeline, frame, &mut cache);
        let previous = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.processing = None;
            state.completed = Some(sequence);
            state.outcome.replace(FrameOutcome {
                sequence,
                result,
                fresh_matches,
                cache_hits,
            })
        };
        drop(previous);
    }
}

/// Run one full scan or build a report from independently tracked card crops.
fn process_frame<M: Matcher>(
    pipeline: &RecognitionPipeline<M>,
    frame: CameraFrame,
    cache: &mut HashMap<u64, CachedRecognition>,
) -> (Result<ScanReport, Error>, Option<usize>, usize) {
    match frame {
        CameraFrame::Scene(image) => {
            cache.clear();
            (pipeline.scan(&image), None, 0)
        }
        CameraFrame::Tracked {
            width,
            height,
            cards,
        } => {
            let mut fresh_matches = 0;
            let mut cache_hits = 0;
            let result = process_tracked(
                pipeline,
                width,
                height,
                cards,
                cache,
                &mut fresh_matches,
                &mut cache_hits,
            );
            (result, Some(fresh_matches), cache_hits)
        }
    }
}

/// Recognise new tracked revisions and reuse only caller-asserted stable ones.
fn process_tracked<M: Matcher>(
    pipeline: &RecognitionPipeline<M>,
    width: u32,
    height: u32,
    cards: Vec<TrackedCard>,
    cache: &mut HashMap<u64, CachedRecognition>,
    fresh_matches: &mut usize,
    cache_hits: &mut usize,
) -> Result<ScanReport, Error> {
    if width == 0 || height == 0 {
        return Err(Error::InvalidInput("camera frame must not be empty".into()));
    }
    let mut seen = HashSet::with_capacity(cards.len());
    for card in &cards {
        if !seen.insert(card.track_id) {
            return Err(Error::InvalidInput("duplicate tracked card ID".into()));
        }
        if !card.area.is_finite()
            || card.area <= 0.0
            || !card.corners.iter().flatten().all(|value| value.is_finite())
        {
            return Err(Error::InvalidInput("invalid tracked card geometry".into()));
        }
    }
    cache.retain(|track_id, _| seen.contains(track_id));
    let mut scanned = Vec::with_capacity(cards.len());
    for card in cards {
        let recognition = if let Some(entry) = cache.get(&card.track_id) {
            if entry.revision == card.revision {
                *cache_hits += 1;
                entry.recognition.clone()
            } else {
                recognize_revision(pipeline, &card, cache, fresh_matches)?
            }
        } else {
            recognize_revision(pipeline, &card, cache, fresh_matches)?
        };
        scanned.push(ScannedCard {
            corners: card.corners,
            area: card.area,
            recognition,
        });
    }
    Ok(ScanReport {
        width,
        height,
        cards: scanned,
    })
}

/// Match an uncached track revision, storing only successful recognition.
fn recognize_revision<M: Matcher>(
    pipeline: &RecognitionPipeline<M>,
    card: &TrackedCard,
    cache: &mut HashMap<u64, CachedRecognition>,
    fresh_matches: &mut usize,
) -> Result<Recognition, Error> {
    let Some(crop) = card.crop.as_ref() else {
        return Err(Error::InvalidInput(format!(
            "uncached tracked card {} needs a rectified crop",
            card.track_id
        )));
    };
    *fresh_matches += 1;
    let recognition = pipeline.recognize_crop(crop)?;
    cache.insert(
        card.track_id,
        CachedRecognition {
            revision: card.revision,
            recognition: recognition.clone(),
        },
    );
    Ok(recognition)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::{MatchOptions, MatchStatus};

    /// Return a deliberately undecided recognition for mailbox tests.
    fn unknown_recognition() -> Recognition {
        Recognition {
            status: MatchStatus::Unknown,
            identity_status: MatchStatus::Unknown,
            confidence: 0.0,
            identity_confidence: 0.0,
            candidates: Vec::new(),
        }
    }

    /// Matcher whose calls reveal whether a track revision was reused.
    struct CountingMatcher(Arc<AtomicUsize>);

    impl Matcher for CountingMatcher {
        fn recognize(
            &self,
            _image: &DynamicImage,
            _options: &MatchOptions,
        ) -> Result<Recognition, Error> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(unknown_recognition())
        }
    }

    /// One card observation with optional rectified crop.
    fn tracked(track_id: u64, revision: u64, crop: bool) -> TrackedCard {
        TrackedCard {
            track_id,
            revision,
            corners: [[0.0, 0.0], [10.0, 0.0], [10.0, 14.0], [0.0, 14.0]],
            area: 140.0,
            crop: crop.then(|| DynamicImage::new_rgb8(10, 14)),
        }
    }

    /// Submit a tracked frame with no expensive automatic detection.
    fn frame(cards: Vec<TrackedCard>) -> CameraFrame {
        CameraFrame::Tracked {
            width: 640,
            height: 480,
            cards,
        }
    }

    /// Wait only in tests for one specific result to complete.
    fn wait_for(live: &LiveRecognition, sequence: u64) -> FrameOutcome {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(outcome) = live.poll()
                && outcome.sequence == sequence
            {
                return outcome;
            }
            assert!(Instant::now() < deadline, "camera worker did not finish");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn tracked_revision_reuses_result_and_missing_track_evicts_it() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pipeline = RecognitionPipeline::new(CountingMatcher(Arc::clone(&calls))).unwrap();
        let live = LiveRecognition::new(pipeline).unwrap();

        let first = live.submit(frame(vec![tracked(7, 1, true)]));
        let result = wait_for(&live, first.sequence);
        assert_eq!(result.fresh_matches, Some(1));
        assert_eq!(result.cache_hits, 0);
        assert_eq!(result.result.unwrap().cards.len(), 1);

        let repeated = live.submit(frame(vec![tracked(7, 1, false)]));
        let result = wait_for(&live, repeated.sequence);
        assert_eq!(result.fresh_matches, Some(0));
        assert_eq!(result.cache_hits, 1);
        assert_eq!(result.result.unwrap().cards.len(), 1);

        let changed = live.submit(frame(vec![tracked(7, 2, true)]));
        let result = wait_for(&live, changed.sequence);
        assert_eq!(result.fresh_matches, Some(1));
        assert_eq!(result.cache_hits, 0);
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let absent = live.submit(frame(Vec::new()));
        assert!(
            wait_for(&live, absent.sequence)
                .result
                .unwrap()
                .cards
                .is_empty()
        );
        let returned = live.submit(frame(vec![tracked(7, 2, true)]));
        assert_eq!(wait_for(&live, returned.sequence).fresh_matches, Some(1));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn uncached_track_needs_crop_and_duplicate_id_is_rejected() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pipeline = RecognitionPipeline::new(CountingMatcher(Arc::clone(&calls))).unwrap();
        let live = LiveRecognition::new(pipeline).unwrap();

        let missing = live.submit(frame(vec![tracked(7, 1, false)]));
        assert!(matches!(
            wait_for(&live, missing.sequence).result,
            Err(Error::InvalidInput(_))
        ));

        let duplicate = live.submit(frame(vec![tracked(7, 1, true), tracked(7, 2, true)]));
        assert!(matches!(
            wait_for(&live, duplicate.sequence).result,
            Err(Error::InvalidInput(_))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    /// Matcher held on its first call so submissions can race its worker safely.
    struct BlockingMatcher {
        /// Counts actual recognition calls.
        calls: Arc<AtomicUsize>,
        /// Tells the test when matching has started.
        started: mpsc::Sender<()>,
        /// Allows the test to finish the first recognition.
        gate: Arc<(Mutex<bool>, Condvar)>,
    }

    impl Matcher for BlockingMatcher {
        fn recognize(
            &self,
            _image: &DynamicImage,
            _options: &MatchOptions,
        ) -> Result<Recognition, Error> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let _ = self.started.send(());
                let (lock, ready) = &*self.gate;
                let mut released = lock.lock().unwrap_or_else(|e| e.into_inner());
                while !*released {
                    released = ready.wait(released).unwrap_or_else(|e| e.into_inner());
                }
            }
            Ok(unknown_recognition())
        }
    }

    #[test]
    fn pending_mailbox_replaces_old_frames_while_matcher_is_busy() {
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (started_sender, started_receiver) = mpsc::channel();
        let pipeline = RecognitionPipeline::new(BlockingMatcher {
            calls: Arc::clone(&calls),
            started: started_sender,
            gate: Arc::clone(&gate),
        })
        .unwrap();
        let live = LiveRecognition::new(pipeline).unwrap();
        let first = live.submit(frame(vec![tracked(1, 1, true)]));
        let started = started_receiver.recv_timeout(Duration::from_secs(5));
        if started.is_err() {
            let (lock, ready) = &*gate;
            *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
            ready.notify_all();
        }
        assert!(started.is_ok(), "matcher did not enter its first call");

        let second = live.submit(frame(vec![tracked(2, 1, true)]));
        assert!(second.replaced.is_none());
        let third = live.submit(frame(vec![tracked(3, 1, true)]));
        assert_eq!(live.status().processing, Some(first.sequence));
        assert_eq!(live.status().pending, Some(third.sequence));
        assert_eq!(live.status().replaced_frames, 1);
        assert!(third.replaced.is_some());

        let (lock, ready) = &*gate;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        ready.notify_all();
        let outcome = wait_for(&live, third.sequence);
        assert_eq!(outcome.result.unwrap().cards.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
