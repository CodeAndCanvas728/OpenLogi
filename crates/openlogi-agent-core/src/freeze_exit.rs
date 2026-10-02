//! The freeze-hazard marker and the dev agent's self-respawn.
//!
//! The macOS tap watchdogs `std::process::exit` when the tap thread stops
//! making progress, because only process death lets macOS tear a wedged HID
//! tap down — and nothing in the process observes that exit. The hook hands it
//! to `handle` first: the report is written to `MARKER_FILE` for the
//! successor to read back through [`take_marker`], and a development agent —
//! where nothing else restarts a dead agent — spawns a replacement before the
//! watchdog's `exit` lands. Production agents are supervised by their service
//! manager and never respawn from here.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use openlogi_core::brand::helper_bundle_root;
use openlogi_core::paths::{is_dev_profile, state_dir};
use openlogi_hook::FreezeExit;
use openlogi_ipc::FreezeExitReport;
use tracing::{info, warn};

/// Marker file under `state_dir()`: the exit report a successor reads back.
const MARKER_FILE: &str = "last_freeze_exit.json";
/// Budget file under `state_dir()`: the stamps of recent respawns, so a
/// freeze that reproduces on every start cannot crash-loop forever.
const BUDGET_FILE: &str = "freeze_respawn_budget.json";
/// How many freeze exits may spawn a replacement… see [`RESPAWN_WINDOW_SECS`].
const RESPAWN_LIMIT: usize = 3;
/// …within this many seconds. The two together bound a bad build to three
/// quick self-heals instead of an unbounded launch storm.
const RESPAWN_WINDOW_SECS: u64 = 10 * 60;

/// Handle a freeze-hazard exit, on the watchdog thread, immediately before
/// the process exits: record the report for the successor, then — only for a
/// dev-profile agent that was not already stopping — charge the respawn
/// budget and launch a replacement. Everything is best-effort and quick;
/// nothing awaits this handler, and a failure here must never prevent the
/// exit that is about to release the wedged tap.
pub(crate) fn handle(exit: FreezeExit) {
    let unix_time = unix_time_now();
    write_marker(&report_of(exit, unix_time));

    if exit.stop_requested {
        info!("freeze exit had an explicit stop in flight — not respawning");
        return;
    }
    if !is_dev_profile() {
        return;
    }

    let mut budget = load_budget();
    if !admit_respawn(&mut budget, unix_time) {
        warn!(
            limit = RESPAWN_LIMIT,
            window_secs = RESPAWN_WINDOW_SECS,
            "freeze-exit respawn budget spent — leaving the agent down"
        );
        return;
    }
    // Charge before spawning, and fail closed: if the budget cannot be
    // persisted, this exit must not become one the next one cannot count.
    if let Err(error) = store_budget(&budget) {
        warn!(%error, "could not persist the respawn budget — not respawning");
        return;
    }
    respawn();
}

/// Read and consume the freeze-exit marker the previous run left behind, for
/// the successor's status report. Called once at startup: the marker is
/// removed whether or not it parses, so the news is reported exactly once and
/// a corrupt file is not rediscovered on every subsequent start.
#[must_use]
pub fn take_marker() -> Option<FreezeExitReport> {
    let path = state_dir().ok()?.join(MARKER_FILE);
    let bytes = fs::read(&path).ok()?;
    let _ = fs::remove_file(&path);
    match serde_json::from_slice(&bytes) {
        Ok(report) => Some(report),
        Err(error) => {
            warn!(%error, "ignoring a corrupt freeze-exit marker");
            None
        }
    }
}

/// The report this exit leaves for its successor — the hook's stable reason
/// and segment names, carried verbatim.
fn report_of(exit: FreezeExit, unix_time: u64) -> FreezeExitReport {
    FreezeExitReport {
        reason: exit.reason.as_str().to_owned(),
        segment: exit.segment.to_owned(),
        unix_time,
        elapsed_ms: exit.elapsed_ms,
    }
}

fn unix_time_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn marker_path() -> Result<PathBuf, String> {
    let dir = state_dir().map_err(|error| error.to_string())?;
    Ok(dir.join(MARKER_FILE))
}

fn write_marker(report: &FreezeExitReport) {
    let path = match marker_path() {
        Ok(path) => path,
        Err(error) => {
            warn!(%error, "no state directory — the freeze exit will not be reported");
            return;
        }
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let Ok(bytes) = serde_json::to_vec(report) else {
        warn!("could not encode the freeze-exit report");
        return;
    };
    if let Err(error) = fs::write(&path, bytes) {
        warn!(%error, path = %path.display(), "could not write the freeze-exit marker");
    }
}

fn budget_path() -> Result<PathBuf, String> {
    let dir = state_dir().map_err(|error| error.to_string())?;
    Ok(dir.join(BUDGET_FILE))
}

fn load_budget() -> Vec<u64> {
    budget_path()
        .ok()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn store_budget(budget: &[u64]) -> Result<(), String> {
    let path = budget_path()?;
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let text = serde_json::to_string(budget).map_err(|error| error.to_string())?;
    fs::write(&path, text).map_err(|error| error.to_string())
}

/// Charge `now` against the budget — pruned to [`RESPAWN_WINDOW_SECS`] — and
/// report whether one more respawn fits under [`RESPAWN_LIMIT`].
fn admit_respawn(budget: &mut Vec<u64>, now: u64) -> bool {
    budget.retain(|stamp| now.saturating_sub(*stamp) < RESPAWN_WINDOW_SECS);
    if budget.len() >= RESPAWN_LIMIT {
        return false;
    }
    budget.push(now);
    true
}

/// Launch a replacement agent. From inside a bundle this goes through
/// LaunchServices (`open -g -n`) so the replacement gets the same bundle
/// identity TCC attributes permissions to; a bare dev binary is spawned
/// directly with stdio detached, because there is no bundle to open.
fn respawn() {
    let Ok(exe) = std::env::current_exe() else {
        warn!("could not resolve this executable — not respawning");
        return;
    };
    if let Some(bundle) = helper_bundle_root(&exe) {
        match Command::new("/usr/bin/open")
            .args(["-g", "-n"])
            .arg(bundle)
            .status()
        {
            Ok(status) if status.success() => {
                info!(bundle = %bundle.display(), "respawned the agent after a freeze exit");
            }
            Ok(status) => {
                warn!(%status, bundle = %bundle.display(), "the respawn launch failed");
            }
            Err(error) => {
                warn!(%error, bundle = %bundle.display(), "could not launch the replacement agent");
            }
        }
    } else {
        let spawned = Command::new(&exe)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Ok(child) => info!(pid = child.id(), "respawned the agent after a freeze exit"),
            Err(error) => {
                warn!(%error, exe = %exe.display(), "could not spawn the replacement agent");
            }
        }
    }
}

/// Install [`handle`] as the hook's freeze-exit handler. Called on every hook
/// start; the first registration in a process wins (it is the same function
/// item either way), so a hook restart's re-registration is a harmless no-op.
pub(crate) fn register() {
    let _ = openlogi_hook::set_freeze_exit_handler(handle);
}

#[cfg(test)]
mod tests {
    use openlogi_hook::{FreezeExit, FreezeExitReason};

    use super::*;

    #[test]
    fn report_carries_the_hooks_stable_strings() {
        let report = report_of(
            FreezeExit {
                reason: FreezeExitReason::TapThreadStalled,
                segment: "run_loop_slice",
                elapsed_ms: 1617,
                stop_requested: false,
            },
            42,
        );
        assert_eq!(report.reason, "tap_thread_stalled");
        assert_eq!(report.segment, "run_loop_slice");
        assert_eq!(report.unix_time, 42);
        assert_eq!(report.elapsed_ms, 1617);
    }

    #[test]
    fn respawn_budget_caps_inside_its_window_and_recovers_after_it() {
        let mut budget = Vec::new();
        assert!(admit_respawn(&mut budget, 1_000));
        assert!(admit_respawn(&mut budget, 1_001));
        assert!(admit_respawn(&mut budget, 1_002));
        assert!(
            !admit_respawn(&mut budget, 1_003),
            "the fourth exit inside the window must not spawn"
        );

        let after_window = 1_000 + RESPAWN_WINDOW_SECS;
        assert!(
            admit_respawn(&mut budget, after_window),
            "stale stamps age out, so the window admits again"
        );
        assert!(admit_respawn(&mut budget, after_window + 1));
        assert!(admit_respawn(&mut budget, after_window + 2));
        assert!(
            !admit_respawn(&mut budget, after_window + 3),
            "three fresh stamps cap again"
        );
    }
}
