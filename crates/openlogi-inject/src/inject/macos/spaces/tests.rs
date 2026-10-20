use objc2_foundation::NSArray;

use super::*;
use crate::inject::space_switch::{self, Outcome};

/// One adjacent-Space switch's streamed drag distance, mirroring the
/// backend's own computation for a 1500 pt display.
const TEST_DISTANCE: f64 = 750.0;

fn test_timeline(sign: f64) -> Vec<SwipeStep> {
    space_switch::swipe_timeline(sign, TEST_DISTANCE)
}

/// Progress round-trips through a float32 field (the Dock reads origin
/// offsets at single precision — MMF's float32-encoded field 135 note), and
/// its conversion loses a few float32 ULPs on the way, so comparisons allow
/// a single-precision-sized error instead of exact bits.
#[expect(clippy::cast_possible_truncation, reason = "see the doc comment")]
fn assert_progress(actual: f64, expected: f64, context: &str) {
    let tolerance = 8.0_f32 * f32::EPSILON * (expected as f32).abs();
    assert!(
        (actual as f32 - expected as f32).abs() <= tolerance,
        "{context}: {actual} != {expected}"
    );
}

#[test]
fn field_swipe_encodes_eased_phases_for_both_directions() {
    for sign in [-1.0_f64, 1.0] {
        for (index, step) in test_timeline(sign).iter().enumerate() {
            let event = field_swipe_event(step).expect("CGEvent allocation");
            assert_eq!(CGEvent::r#type(Some(&event)), CGEventType(30));
            for (field, expected) in [(55, 30), (110, 23), (123, 1), (132, step.phase.code())] {
                assert_eq!(
                    CGEvent::integer_value_field(Some(&event), CGEventField(field)),
                    i64::from(expected),
                    "step {index}: field {field}"
                );
            }
            assert_progress(
                CGEvent::double_value_field(Some(&event), CGEventField(124)),
                step.progress,
                "step {index}: cumulative progress",
            );
            assert_eq!(
                CGEvent::double_value_field(Some(&event), CGEventField(129)).to_bits(),
                step.velocity.to_bits(),
                "step {index}: exit velocity"
            );
            assert_eq!(
                CGEvent::integer_value_field(Some(&event), CGEventField::EventSourceUserData),
                crate::inject::SYNTHETIC_EVENT_USER_DATA
            );
        }
    }
}

#[test]
fn hid_dock_swipe_attaches_streamed_phases_for_both_scroll_directions() {
    type CopyHidEvent = unsafe extern "C" fn(*const CGEvent) -> *mut AnyObject;

    autoreleasepool(|_| {
        let setter_available = app_services::sky_light_symbol(c"SLEventSetIOHIDEvent").is_some();
        let class_available = AnyClass::get(c"HIDEvent").is_some();
        let copy_hid = app_services_symbol(c"CGEventCopyIOHIDEvent");
        eprintln!(
            "HID swipe test availability: class={class_available}, setter={setter_available}, copy={}",
            copy_hid.is_some()
        );
        if !class_available || !setter_available || copy_hid.is_none() {
            assert!(
                NSProcessInfo::processInfo()
                    .operatingSystemVersion()
                    .majorVersion
                    < 27,
                "macOS 27 requires the HID swipe APIs"
            );
            eprintln!("HID swipe assertions skipped: private APIs unavailable on this older macOS");
            return;
        }
        // SAFETY: CGEventCopyIOHIDEvent returns a +1 HIDEvent, as documented
        // by Mac Mouse Fix's CGEventHIDEventBridge.m at f92d2d53a.
        let copy_hid = unsafe {
            std::mem::transmute::<*mut c_void, CopyHidEvent>(copy_hid.expect("copy API available"))
        };
        // SAFETY: the resolved SkyLight symbol has the `void(CGEventRef, IOHIDEventRef)` ABI.
        let set_hid = unsafe {
            std::mem::transmute::<*mut c_void, SetHidEvent>(
                app_services::sky_light_symbol(c"SLEventSetIOHIDEvent").expect("setter API available"),
            )
        };
        let class = AnyClass::get(c"HIDEvent").expect("HIDEvent class");
        // macOS 27 reads the swipe's sign the opposite way from the legacy
        // field encoding, and only under natural scrolling (#1703).
        for (natural, direction, sign) in [
            (true, Direction::Previous, 1.0_f64),
            (true, Direction::Next, -1.0_f64),
            (false, Direction::Previous, -1.0_f64),
            (false, Direction::Next, 1.0_f64),
        ] {
            let steps = test_timeline(sign);
            for (index, step) in steps.iter().enumerate() {
                let event = hid_swipe_event(class, set_hid, step)
                    .expect("HIDEvent-backed CGEvent");
                assert_eq!(CGEvent::r#type(Some(&event)), CGEventType(30));
                assert_eq!(
                    CGEvent::integer_value_field(Some(&event), CGEventField::EventSourceUserData),
                    crate::inject::SYNTHETIC_EVENT_USER_DATA
                );
                // SAFETY: event is live; the Copy API returns a retained HIDEvent or null.
                let hid = unsafe { copy_hid(&raw const *event) };
                // SAFETY: adopt the Copy-rule reference once; Retained releases the HIDEvent.
                let hid = unsafe { Retained::from_raw(hid) }.expect("attached HIDEvent");
                // SAFETY: HIDEvent.options returns uint32_t.
                let options: u32 = unsafe { msg_send![&*hid, options] };
                assert_eq!(options, step.phase.code() << 24, "step {index}: encoded phase");
                // SAFETY: HIDEvent's integer getter returns NSInteger for this field.
                let motion: isize =
                    unsafe { msg_send![&*hid, integerValueForField: HID_FIELD_MOTION] };
                // SAFETY: HIDEvent's integer getter returns NSInteger for this field.
                let flavor: isize =
                    unsafe { msg_send![&*hid, integerValueForField: HID_FIELD_FLAVOR] };
                // SAFETY: HIDEvent's progress getter returns double.
                let progress: f64 =
                    unsafe { msg_send![&*hid, doubleValueForField: HID_FIELD_PROGRESS] };
                assert_eq!(motion, HID_MOTION_HORIZONTAL);
                assert_eq!(flavor, HID_FLAVOR_DOCK_PRIMARY);
                assert_progress(
                    progress,
                    step.progress,
                    &format!("step {index}: natural={natural}, direction={direction:?}"),
                );
                // SAFETY: HIDEvent.children returns a nullable NSArray of HIDEvent objects.
                let children: Option<Retained<NSArray<AnyObject>>> =
                    unsafe { msg_send![&*hid, children] };
                assert_eq!(
                    children.as_ref().map_or(0, |children| children.len()),
                    usize::from(step.phase == SwipePhase::Ended),
                    "step {index}: only the end phase carries one velocity child"
                );
                if let Some(velocity) = children.and_then(|children| children.firstObject()) {
                    // SAFETY: the child is a live HIDEvent; its X getter returns double.
                    let vx: f64 =
                        unsafe { msg_send![&*velocity, doubleValueForField: HID_FIELD_VELOCITY_X] };
                    // SAFETY: the child is a live HIDEvent; its Y getter returns double.
                    let vy: f64 =
                        unsafe { msg_send![&*velocity, doubleValueForField: HID_FIELD_VELOCITY_Y] };
                    assert_eq!(vx.to_bits(), step.velocity.to_bits());
                    assert_eq!(vy.to_bits(), step.velocity.to_bits());
                }
            }
        }
        eprintln!("HID swipe assertions executed: both directions, both preferences, all phases");
    });
}

/// Opt-in hardware test. Run in a logged-in macOS session with Accessibility
/// granted to the test host, a next Space on the pointer's display, and no
/// concurrent trackpad/keyboard Space switching. Switches right, then left back
/// to the original Space. A failure can leave the display on the next Space;
/// never changes keyboard settings, moves the cursor, or retries.
#[test]
#[ignore = "changes the current macOS Space; requires explicit interactive execution"]
fn interactive_space_round_trip() {
    autoreleasepool(|_| {
        let display_id = cursor_display().expect("one unambiguous pointer display");
        let mut backend = Native::new(display_id).expect("Space SPI available");
        let before = backend.state().expect("read initial display state");
        let result = space_switch::run(&mut backend, Direction::Next, PostGate::for_test())
            .expect("Space switch confirmed");
        let Outcome::Reached(target) = result else {
            panic!("move to a Space with a right-hand neighbor before running this test");
        };
        let after = backend.state().expect("read final display state");
        assert_eq!(after.display, before.display);
        assert_eq!(after.current, target);
        assert_ne!(after.current, before.current);
        assert_eq!(cursor_display(), Some(display_id));
        // Give the reverse transaction its own observer and deadline. The
        // expectation is the original native ID, not the direction encoder.
        let mut reverse = Native::new(display_id).expect("Space SPI available");
        assert_eq!(
            space_switch::run(&mut reverse, Direction::Previous, PostGate::for_test())
                .expect("reverse Space switch confirmed"),
            Outcome::Reached(before.current)
        );
        assert_eq!(
            reverse.state().expect("read restored display state"),
            before
        );
        assert_eq!(cursor_display(), Some(display_id));
    });
}
