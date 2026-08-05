//! Input, forwarded to whatever is on screen.
//!
//! A TV box is driven by a remote that presents itself as a keyboard, and
//! occasionally by a mouse while somebody is working on it. There is no window
//! management to bind keys for, so the only thing intercepted is the way out.

use anyhow::{anyhow, Result};
use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
    KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::input::keyboard::{FilterResult, Keysym, ModifiersState};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::reexports::input::Libinput;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};
use tracing::info;

use crate::state::Tvbox;

/// Open libinput on the session's seat.
pub fn init(session: &LibSeatSession, seat_name: &str) -> Result<LibinputInputBackend> {
    let mut context = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
    context
        .udev_assign_seat(seat_name)
        .map_err(|_| anyhow!("failed to assign the libinput seat {seat_name}"))?;
    Ok(LibinputInputBackend::new(context))
}

/// Forward an input event.
pub fn handle(state: &mut Tvbox, event: InputEvent<LibinputInputBackend>) {
    match event {
        InputEvent::Keyboard { event } => keyboard(state, event),
        InputEvent::PointerMotion { event } => {
            let delta: Point<f64, Logical> = (event.delta_x(), event.delta_y()).into();
            let position = state.pointer_location + delta;
            pointer_motion(state, position, event.time_msec());
        }
        InputEvent::PointerMotionAbsolute { event } => {
            let Some(output) = state.output.clone() else {
                return;
            };
            let Some(geometry) = state.space.output_geometry(&output) else {
                return;
            };
            let position = event.position_transformed(geometry.size) + geometry.loc.to_f64();
            pointer_motion(state, position, event.time_msec());
        }
        InputEvent::PointerButton { event } => {
            let Some(pointer) = state.seat.get_pointer() else {
                return;
            };
            let serial = SERIAL_COUNTER.next_serial();
            let button = event.button_code();
            let button_state = event.state();

            if button_state == ButtonState::Pressed {
                let focus = state.surface_under(state.pointer_location);
                pointer.motion(
                    state,
                    focus.clone(),
                    &MotionEvent {
                        location: state.pointer_location,
                        serial,
                        time: event.time_msec(),
                    },
                );
            }

            pointer.button(
                state,
                &ButtonEvent {
                    button,
                    state: button_state,
                    serial,
                    time: event.time_msec(),
                },
            );
            pointer.frame(state);
        }
        InputEvent::PointerAxis { event } => {
            let Some(pointer) = state.seat.get_pointer() else {
                return;
            };
            let mut frame = AxisFrame::new(event.time_msec()).source(AxisSource::Wheel);
            for axis in [Axis::Horizontal, Axis::Vertical] {
                if let Some(value) = event.amount(axis) {
                    frame = frame.value(axis, value);
                }
                if let Some(discrete) = event.amount_v120(axis) {
                    frame = frame.v120(axis, discrete as i32);
                }
            }
            pointer.axis(state, frame);
            pointer.frame(state);
        }
        _ => {}
    }
}

fn pointer_motion(state: &mut Tvbox, position: Point<f64, Logical>, time: u32) {
    let Some(output) = state.output.clone() else {
        return;
    };
    let Some(geometry) = state.space.output_geometry(&output) else {
        return;
    };

    let position = (
        position.x.clamp(0.0, geometry.size.w as f64 - 1.0),
        position.y.clamp(0.0, geometry.size.h as f64 - 1.0),
    )
        .into();
    state.pointer_location = position;

    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };
    let focus = state.surface_under(position);
    pointer.motion(
        state,
        focus,
        &MotionEvent {
            location: position,
            serial: SERIAL_COUNTER.next_serial(),
            time,
        },
    );
    pointer.frame(state);
}

fn keyboard(state: &mut Tvbox, event: <LibinputInputBackend as InputBackend>::KeyboardKeyEvent) {
    let Some(keyboard) = state.seat.get_keyboard() else {
        return;
    };
    let serial = SERIAL_COUNTER.next_serial();
    let time = event.time_msec();
    let code = event.key_code();
    let key_state = event.state();
    let running = state.running.clone();

    keyboard.input::<(), _>(
        state,
        code,
        key_state,
        serial,
        time,
        |_state, modifiers, handle| {
            if key_state == KeyState::Pressed && is_quit(modifiers, handle.modified_sym()) {
                info!("quit combination pressed");
                running.store(false, std::sync::atomic::Ordering::SeqCst);
                return FilterResult::Intercept(());
            }
            FilterResult::Forward
        },
    );
}

/// Ctrl+Alt+Backspace, the way out when there is no session manager to ask.
fn is_quit(modifiers: &ModifiersState, keysym: Keysym) -> bool {
    modifiers.ctrl && modifiers.alt && keysym == Keysym::BackSpace
}
