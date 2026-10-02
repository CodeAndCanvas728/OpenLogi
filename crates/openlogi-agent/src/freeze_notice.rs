//! The user notification for a freeze-exit restart.
//!
//! The watchdog's `exit(78)` otherwise looks like nothing happened: the
//! menu-bar item and remapping simply reappear seconds later. This posts a
//! notification the next time the agent starts with a freeze-exit marker on
//! disk, so the restart is visible instead of silent. Authorization is
//! requested lazily — only when there is something to report — and a denial or
//! delivery failure degrades to the log and the GUI's status banner.

use objc2::runtime::Bool;
use objc2_foundation::{NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
    UNUserNotificationCenter,
};
use openlogi_ipc::FreezeExitReport;
use tracing::{info, warn};

/// Identifier of the freeze-exit notification: a repeated exit replaces the
/// previous one instead of stacking copies of the same news.
const NOTICE_ID: &str = "openlogi-freeze-exit";

/// Post the freeze-exit notification for `report`. Fire-and-forget: the
/// authorization prompt (first time only) and delivery both complete on the
/// system's queues, so this returns without waiting for either.
pub(crate) fn post_freeze_notice(report: &FreezeExitReport) {
    info!(
        reason = %report.reason,
        segment = %report.segment,
        "notifying that the previous agent run freeze-exited"
    );

    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(&rust_i18n::t!(
        "freeze_exit.notification.title"
    )));
    content.setBody(&NSString::from_str(&rust_i18n::t!(
        "freeze_exit.notification.body"
    )));
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str(NOTICE_ID),
        &content,
        None,
    );

    let center = UNUserNotificationCenter::currentNotificationCenter();
    let delivering = center.clone();
    center.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert,
        &block2::RcBlock::new(move |granted: Bool, _error: *mut NSError| {
            if !granted.as_bool() {
                warn!("notification permission denied — the freeze exit is reported in the app instead");
                return;
            }
            delivering.addNotificationRequest_withCompletionHandler(
                &request,
                Some(&block2::RcBlock::new(|error: *mut NSError| {
                    if !error.is_null() {
                        warn!("the freeze-exit notification could not be delivered");
                    }
                })),
            );
        }),
    );
}
