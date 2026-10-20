//! Space-switch transaction policy, independent of macOS FFI for regression tests.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

const PREPARATION_TIMEOUT: Duration = Duration::from_secs(2);
const CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Direction {
    Previous,
    Next,
}

impl Direction {
    pub(super) fn sign(self) -> f64 {
        match self {
            Self::Previous => -1.0,
            Self::Next => 1.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SpaceState {
    pub display: String,
    pub current: u64,
    /// Mission Control order, including fullscreen Spaces. Never sort by ID.
    pub ordered: Vec<u64>,
}

impl SpaceState {
    fn target(&self, direction: Direction) -> Result<Option<u64>, Failure> {
        if self.display.is_empty()
            || self.current == 0
            || self
                .ordered
                .iter()
                .enumerate()
                .any(|(i, id)| *id == 0 || self.ordered[..i].contains(id))
        {
            // #1086: this rejection needs no multi-display topology at all —
            // a zero/empty id or a duplicate in `ordered` fails it on a
            // single display too, unlike the display-match miss #1694
            // already logs in the macOS backend.
            tracing::debug!(
                display = %self.display,
                current = self.current,
                ordered = ?self.ordered,
                "Space target: the read Space list failed validation"
            );
            return Err(Failure::Unavailable);
        }
        let Some(index) = self.ordered.iter().position(|id| *id == self.current) else {
            // The current Space passed validation above (nonzero, no
            // duplicates) but isn't in `ordered` at all — e.g. a Space kind
            // the `type` filter upstream excludes (Stage Manager, a kind
            // other than the plain/fullscreen 0|4 it keeps).
            tracing::debug!(
                display = %self.display,
                current = self.current,
                ordered = ?self.ordered,
                "Space target: the current Space is not in the ordered list"
            );
            return Err(Failure::Unavailable);
        };
        let neighbor = match direction {
            Direction::Previous => index.checked_sub(1),
            Direction::Next => index.checked_add(1),
        };
        Ok(neighbor.and_then(|i| self.ordered.get(i)).copied())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Failure {
    Unavailable,
    ContextChanged,
    PostFailed,
    TimedOut,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Boundary,
    Reached(u64),
}

/// Fraction of the cursor display's width one streamed gesture drags the
/// Space across before its end edge commits the switch.
pub(super) const SWIPE_DRAG_FRACTION: f64 = 0.5;

/// Changed edges streamed between one gesture's began and end edges.
pub(super) const SWIPE_CHANGES: usize = 12;

/// Gap between consecutive streamed edges, approximating the ~8 ms reporting
/// cadence Mac Mouse Fix measured on real trackpad DockSwipes (commit
/// f92d2d53a). 12 ms over 14 edges keeps a switch under ~170 ms while
/// staying close enough to the real cadence to read as one gesture.
pub(super) const SWIPE_STEP_INTERVAL: Duration = Duration::from_millis(12);

/// Exit velocity the end edge carries so the Dock commits the in-flight
/// gesture. Proven by the teleport encoding this file replaces and in the
/// same range as Mac Mouse Fix's real-event exit speeds.
pub(super) const SWIPE_EXIT_SPEED: f64 = 9999.0;

/// Gesture phase of one streamed DockSwipe edge. The codes are the values
/// both DockSwipe encodings read for began/changed/ended (see the macOS
/// spaces backend).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SwipePhase {
    Began,
    Changed,
    Ended,
}

impl SwipePhase {
    pub(super) const fn code(self) -> u32 {
        match self {
            Self::Began => 1,
            Self::Changed => 2,
            Self::Ended => 4,
        }
    }
}

/// One streamed gesture edge: its phase, the cumulative signed drag distance
/// it reports, and its exit velocity (nonzero only on the end edge).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct SwipeStep {
    pub phase: SwipePhase,
    pub progress: f64,
    pub velocity: f64,
}

/// Cumulative eased fraction of the drag covered `i` edges into the gesture.
fn eased_ratio(i: usize, total: usize) -> f64 {
    // Bounded by construction to a two-digit step count; no truncation risk.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the step counts are the bounded constants above"
    )]
    let position = f64::from(i as u32) / f64::from(total as u32);
    position * position * (3.0 - 2.0 * position)
}

/// Build the streamed gesture for one adjacent-Space switch: a began edge,
/// eased changed edges sliding the Space toward the target at the real
/// reporting cadence, and an end edge that flicks it home — the shape a real
/// three-finger trackpad swipe produces, which the Dock plays its own slide
/// animation for. `sign` carries the swipe-direction resolution of the
/// encoding in use (the legacy field encoding and the macOS 27 HIDEvent
/// encoding disagree, see the macOS spaces backend); `distance` is the plain
/// positive drag distance.
pub(super) fn swipe_timeline(sign: f64, distance: f64) -> Vec<SwipeStep> {
    debug_assert!(distance.is_finite() && distance > 0.0);
    debug_assert!(matches!(sign, -1.0 | 1.0));
    // The began edge reports the first increment; each changed edge the next
    // cumulative position; the end edge the full distance plus the flick.
    let total = SWIPE_CHANGES + 1;
    let progress_at = |i: usize| sign * eased_ratio(i, total) * distance;
    let mut steps = Vec::with_capacity(total + 1);
    steps.push(SwipeStep {
        phase: SwipePhase::Began,
        progress: progress_at(1),
        velocity: 0.0,
    });
    for i in 2..=total {
        steps.push(SwipeStep {
            phase: SwipePhase::Changed,
            progress: progress_at(i),
            velocity: 0.0,
        });
    }
    steps.push(SwipeStep {
        phase: SwipePhase::Ended,
        progress: sign * distance,
        velocity: sign * SWIPE_EXIT_SPEED,
    });
    steps
}

/// One native transaction owns its observer and monotonic start time.
pub(super) trait Backend {
    fn state(&mut self) -> Result<SpaceState, Failure>;
    /// Finish allocation and cursor checks before committing the gate. Never
    /// put native preparation inside the committed, non-cancellable output.
    /// `emit` posts the began edge only; this method then streams the rest of
    /// the gesture before returning, so the confirmation loop starts after
    /// the full timeline.
    fn post(&mut self, direction: Direction, gate: PostGate) -> Result<(), Failure>;
    fn elapsed(&self) -> Duration;
    /// Wake on a notification or the deadline, without losing notifications
    /// between `state` and this call. A wakeup alone never proves success.
    fn wait_for_change(&mut self, remaining: Duration);
}

#[repr(u8)]
enum PostState {
    Preparing,
    Posting,
    Canceled,
}

struct PostControl {
    state: AtomicU8,
    deadline: Instant,
}

impl PostControl {
    /// False means posting already won; its native calls cannot be canceled.
    fn cancel(&self) -> bool {
        match self.state.compare_exchange(
            PostState::Preparing as u8,
            PostState::Canceled as u8,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => true,
            Err(state) => state == PostState::Canceled as u8,
        }
    }
}

/// Sole authority to emit both phases. Cancellation and commitment arbitrate
/// atomically, so a timed-out native query cannot later produce stale output.
pub(super) struct PostGate {
    control: Arc<PostControl>,
    posted: mpsc::Sender<()>,
}

struct PostWait {
    control: Arc<PostControl>,
    receive: mpsc::Receiver<()>,
}

impl PostGate {
    #[cfg(test)]
    pub(super) fn for_test() -> Self {
        Self::new(Instant::now() + PREPARATION_TIMEOUT).0
    }

    fn new(deadline: Instant) -> (Self, PostWait) {
        let control = Arc::new(PostControl {
            state: AtomicU8::new(PostState::Preparing as u8),
            deadline,
        });
        let (posted, receive) = mpsc::channel();
        (
            Self {
                control: Arc::clone(&control),
                posted,
            },
            PostWait { control, receive },
        )
    }

    /// Only the opening (began) post call belongs in `emit`: no allocation,
    /// state queries, retries, or cancellation checks between edges. The
    /// remaining streamed gesture runs after `emit` returns, on the
    /// dedicated worker thread, once the blocker has been released.
    pub(super) fn commit(self, emit: impl FnOnce()) -> Result<(), Failure> {
        if Instant::now() >= self.control.deadline {
            self.control.cancel();
            return Err(Failure::TimedOut);
        }
        self.control
            .state
            .compare_exchange(
                PostState::Preparing as u8,
                PostState::Posting as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .map_err(|_| Failure::TimedOut)?;
        emit();
        let _ = self.posted.send(());
        Ok(())
    }
}

impl PostWait {
    fn wait(self, now: Instant) -> Result<(), Failure> {
        let remaining = self.control.deadline.saturating_duration_since(now);
        if self.receive.recv_timeout(remaining) == Err(mpsc::RecvTimeoutError::Timeout) {
            if self.control.cancel() {
                return Err(Failure::TimedOut);
            }
            // Posting won the race. Do not release later actions before the
            // committed began post returns; a second timeout would allow a
            // late swipe.
            let _ = self.receive.recv();
        }
        // Disconnection is cancellation/worker exit, not evidence of success.
        // After unwinding it can also mean partial output; never retry it.
        Ok(())
    }
}

/// Bound preparation, not native event posting. Return after irrevocable
/// cancellation, posting acknowledgement, or worker exit. Confirmation stays
/// detached; a canceled but stuck worker retains its single-flight lease.
pub(super) fn spawn_ordered(work: impl FnOnce(PostGate) + Send + 'static) -> Result<(), Failure> {
    let (gate, wait) = PostGate::new(Instant::now() + PREPARATION_TIMEOUT);
    std::thread::Builder::new()
        .name("openlogi-spaces".into())
        .spawn(move || work(gate))
        .map_err(|error| {
            tracing::warn!(%error, "Space switch worker unavailable");
            Failure::Unavailable
        })?;
    wait.wait(Instant::now())
}

pub(super) fn run(
    backend: &mut impl Backend,
    direction: Direction,
    gate: PostGate,
) -> Result<Outcome, Failure> {
    let initial = backend.state()?;
    let Some(target) = initial.target(direction)? else {
        return Ok(Outcome::Boundary);
    };
    // Topology or active-Space changes during preparation invalidate the plan.
    if backend.state()? != initial {
        return Err(Failure::ContextChanged);
    }
    backend.post(direction, gate)?;
    loop {
        let state = backend.state()?;
        if state.display != initial.display || state.ordered != initial.ordered {
            return Err(Failure::ContextChanged);
        }
        if state.current == target {
            return Ok(Outcome::Reached(target));
        }
        if state.current != initial.current {
            return Err(Failure::ContextChanged);
        }
        // Read BEFORE checking the deadline: a missed/coalesced notification
        // still gets one authoritative final query. Never resend on timeout.
        let Some(remaining) = CONFIRMATION_TIMEOUT
            .checked_sub(backend.elapsed())
            .filter(|remaining| !remaining.is_zero())
        else {
            return Err(Failure::TimedOut);
        };
        backend.wait_for_change(remaining);
    }
}

/// Single in-flight transaction; no queue of relative actions that can run
/// later on a different display. The owned lease also releases on spawn failure.
pub(super) struct Lease<'a>(&'a AtomicBool);

impl<'a> Lease<'a> {
    pub(super) fn acquire(busy: &'a AtomicBool) -> Option<Self> {
        busy.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Self(busy))
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests;
