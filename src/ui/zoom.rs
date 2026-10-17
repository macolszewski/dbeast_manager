//! Zoom IDE: the whole interface scales in 10 % steps with
//! ⌃⌥⇧= / ⌃⌥⇧- / ⌃⌥⇧0 (Ctrl+Alt+Shift on other systems); the scale is saved.

use super::{App, ServicesNotice};
use egui::{Context, Event, Key, KeyboardShortcut, Modifiers};

const MODIFIERS: Modifiers = Modifiers {
    alt: true,
    ctrl: true,
    shift: true,
    mac_cmd: false,
    command: false,
};

pub(super) const ZOOM_IN: Option<KeyboardShortcut> = Some(KeyboardShortcut::new(MODIFIERS, Key::Equals));
pub(super) const ZOOM_OUT: Option<KeyboardShortcut> = Some(KeyboardShortcut::new(MODIFIERS, Key::Minus));
pub(super) const RESET: Option<KeyboardShortcut> = Some(KeyboardShortcut::new(MODIFIERS, Key::Num0));

const MIN: f32 = 0.5;
const MAX: f32 = 2.5;
const STEP: f32 = 0.1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    In,
    Out,
    Reset,
}

pub(super) fn clamp(zoom: f32) -> f32 {
    if zoom.is_finite() { zoom.clamp(MIN, MAX) } else { 1.0 }
}

/// The next scale, on whole 10 % steps so repeated zooming does not drift.
pub(super) fn next(current: f32, step: Step) -> f32 {
    let tenths = (clamp(current) / STEP).round();
    clamp(match step {
        Step::In => (tenths + 1.0) * STEP,
        Step::Out => (tenths - 1.0) * STEP,
        Step::Reset => 1.0,
    })
}

/// Consumes a Zoom IDE key press, matched on the physical key.
pub(super) fn pressed(ctx: &Context) -> Option<Step> {
    ctx.input_mut(|input| {
        let mut found = None;
        input.events.retain(|event| {
            let Event::Key { key, physical_key, pressed: true, modifiers, .. } = event else {
                return true;
            };
            if !(modifiers.ctrl && modifiers.alt && modifiers.shift) || modifiers.mac_cmd {
                return true;
            }
            let physical = physical_key.unwrap_or(*key);
            let step = match physical {
                Key::Equals | Key::Plus => Step::In,
                Key::Minus => Step::Out,
                Key::Num0 => Step::Reset,
                _ => return true,
            };
            found = Some(step);
            false
        });
        found
    })
}

impl App {
    pub(super) fn zoom_ide(&mut self, ctx: &Context, step: Step) {
        let zoom = next(self.config.ui_zoom, step);
        // Key repeat at a limit would otherwise re-save the whole config.
        if zoom == self.config.ui_zoom {
            return;
        }
        ctx.set_zoom_factor(zoom);
        self.config.ui_zoom = zoom;
        self.save_config();
        self.notice = Some(ServicesNotice::info(format!("Zoom: {:.0} %", zoom * 100.0)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_moves_in_tenths_within_limits() {
        assert_eq!(next(1.0, Step::In), 1.1);
        assert!((next(1.1, Step::Out) - 1.0).abs() < 1e-6);
        assert_eq!(next(2.5, Step::In), 2.5);
        assert_eq!(next(0.5, Step::Out), 0.5);
        assert_eq!(next(1.7, Step::Reset), 1.0);
        assert_eq!(clamp(f32::NAN), 1.0);
        // 1.04 from an older value lands on the nearest step.
        assert!((next(1.04, Step::In) - 1.1).abs() < 1e-6);
    }
}
