//! Orbit camera and perspective projection for Depth View and Database Graph.
//! Pure math, no egui state, so scenes can be tested without rendering.
//!
//! World axes: X to the right, Y down the screen, Z away from the viewer. In
//! Depth View X runs along a row's columns, Y down the table order and Z into
//! the depth (earlier times are farther away).

use egui::{Pos2, Rect};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::ui) struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
    pub fn sub(self, other: Self) -> Self {
        Self::new(self.x - other.x, self.y - other.y, self.z - other.z)
    }
    pub fn add(self, other: Self) -> Self {
        Self::new(self.x + other.x, self.y + other.y, self.z + other.z)
    }
    pub fn scale(self, factor: f32) -> Self {
        Self::new(self.x * factor, self.y * factor, self.z * factor)
    }
    pub fn dot(self, other: Self) -> f32 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }
    fn cross(self, other: Self) -> Self {
        Self::new(
            self.y * other.z - self.z * other.y,
            self.z * other.x - self.x * other.z,
            self.x * other.y - self.y * other.x,
        )
    }
    fn normalized(self) -> Self {
        let length = self.dot(self).sqrt().max(f32::EPSILON);
        self.scale(1.0 / length)
    }
}

/// Orbit camera around a target point. Yaw turns around the vertical axis,
/// pitch looks from above (positive) or below.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::ui) struct Camera {
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
    pub target: Vec3,
}

pub(in crate::ui) const FOV_Y: f32 = 0.9;
const NEAR: f32 = 1.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ui) enum Preset {
    Front,
    Oblique,
    Top,
}

impl Camera {
    /// Frames points from a preset direction so that their projected outline
    /// fills the viewport, measured rather than estimated from a sphere.
    pub fn framing(preset: Preset, points: &[Vec3], viewport: Rect) -> Self {
        Self::framing_into(preset, points, viewport, viewport)
    }

    /// Like [`Camera::framing`], projecting onto `viewport` but fitting the
    /// outline into `fit`, a part of it left free by overlays.
    pub fn framing_into(preset: Preset, points: &[Vec3], viewport: Rect, fit: Rect) -> Self {
        let (yaw, pitch) = match preset {
            Preset::Front => (0.0, 0.0),
            Preset::Oblique => (-0.38, 0.28),
            Preset::Top => (0.0, 1.45),
        };
        let (min, max) = points.iter().fold(
            (Vec3::new(f32::MAX, f32::MAX, f32::MAX), Vec3::new(f32::MIN, f32::MIN, f32::MIN)),
            |(lo, hi), p| {
                (
                    Vec3::new(lo.x.min(p.x), lo.y.min(p.y), lo.z.min(p.z)),
                    Vec3::new(hi.x.max(p.x), hi.y.max(p.y), hi.z.max(p.z)),
                )
            },
        );
        let (min, max) = if points.is_empty() { (Vec3::new(0.0, 0.0, 0.0), Vec3::new(1.0, 1.0, 1.0)) } else { (min, max) };
        let size = max.sub(min);
        let radius = size.dot(size).sqrt() * 0.5;
        let mut camera = Self {
            yaw,
            pitch,
            distance: (radius / (FOV_Y * 0.5).tan()).max(200.0),
            target: min.add(size.scale(0.5)),
        };
        for _ in 0..24 {
            let view = camera.view();
            let mut bounds = Rect::NOTHING;
            for point in points {
                match view.project(*point, viewport) {
                    Some(projected) => bounds.extend_with(projected.pos),
                    None => bounds = Rect::EVERYTHING,
                }
            }
            let fill = (bounds.width() / fit.width()).max(bounds.height() / fit.height());
            // Center the projected outline, which perspective moves off target.
            let offset = bounds.center() - fit.center();
            if !fill.is_finite() || !offset.is_finite() {
                break;
            }
            if (fill - 0.92).abs() < 0.02 && offset.length() < 1.0 {
                break;
            }
            // Damped: near points grow and move faster than the target depth
            // suggests, so full steps overshoot.
            camera.distance = (camera.distance * (fill / 0.92).powf(0.6)).clamp(60.0, 60_000.0);
            camera.pan(-offset.x * 0.4, -offset.y * 0.4, viewport);
        }
        camera
    }

    fn eye(&self) -> Vec3 {
        // Looking from negative Z (near, later times) towards positive Z.
        let direction = Vec3::new(
            self.yaw.sin() * self.pitch.cos(),
            -self.pitch.sin(),
            -self.yaw.cos() * self.pitch.cos(),
        );
        self.target.add(direction.scale(self.distance))
    }

    pub fn orbit(&mut self, dx: f32, dy: f32) {
        self.yaw += dx * 0.008;
        self.pitch = (self.pitch + dy * 0.008).clamp(-1.5, 1.5);
    }

    pub fn zoom(&mut self, wheel: f32) {
        self.distance = (self.distance * (-wheel * 0.0025).exp()).clamp(60.0, 60_000.0);
    }

    /// Moves the target in the view plane, by screen pixels at its depth.
    pub fn pan(&mut self, dx: f32, dy: f32, viewport: Rect) {
        let view = self.view();
        let per_pixel = 2.0 * self.distance * (FOV_Y * 0.5).tan() / viewport.height().max(1.0);
        self.target = self
            .target
            .add(view.right.scale(-dx * per_pixel))
            .add(view.up.scale(dy * per_pixel));
    }

    pub fn view(&self) -> View {
        let eye = self.eye();
        let forward = self.target.sub(eye).normalized();
        // Screen up is world -Y (rows grow downwards on screen).
        let world_up = Vec3::new(0.0, -1.0, 0.0);
        let right = forward.cross(world_up).normalized();
        let up = right.cross(forward);
        View {
            eye,
            forward,
            right,
            up,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(in crate::ui) struct View {
    eye: Vec3,
    forward: Vec3,
    right: Vec3,
    up: Vec3,
}

/// A projected point: screen position and depth along the view direction.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::ui) struct Projected {
    pub pos: Pos2,
    pub depth: f32,
}

impl View {
    pub fn project(&self, point: Vec3, viewport: Rect) -> Option<Projected> {
        let relative = point.sub(self.eye);
        let depth = relative.dot(self.forward);
        if depth < NEAR {
            return None;
        }
        let focal = viewport.height() * 0.5 / (FOV_Y * 0.5).tan();
        let x = relative.dot(self.right) / depth * focal;
        let y = relative.dot(self.up) / depth * focal;
        Some(Projected {
            pos: viewport.center() + egui::vec2(x, -y),
            depth,
        })
    }

    /// Whether a face through `point` with outward `normal` is seen from the front.
    pub fn faces_camera(&self, point: Vec3, normal: Vec3) -> bool {
        self.eye.sub(point).dot(normal) > 0.0
    }

    /// World movement that shifts a point at `depth` by a screen delta.
    pub fn screen_delta_to_world(&self, delta: egui::Vec2, depth: f32, viewport: Rect) -> Vec3 {
        let per_pixel = 1.0 / self.pixels_per_unit(depth, viewport);
        self.right
            .scale(delta.x * per_pixel)
            .add(self.up.scale(-delta.y * per_pixel))
    }

    /// Screen pixels per world unit at a given depth.
    pub fn pixels_per_unit(&self, depth: f32, viewport: Rect) -> f32 {
        viewport.height() * 0.5 / (FOV_Y * 0.5).tan() / depth.max(NEAR)
    }
}

/// Point-in-convex-quad test for picking projected cards.
pub(in crate::ui) fn quad_contains(quad: &[Pos2; 4], point: Pos2) -> bool {
    let mut sign = 0.0f32;
    for index in 0..4 {
        let a = quad[index];
        let b = quad[(index + 1) % 4];
        let cross = (b - a).x * (point - a).y - (b - a).y * (point - a).x;
        if cross.abs() < f32::EPSILON {
            continue;
        }
        if sign == 0.0 {
            sign = cross.signum();
        } else if cross.signum() != sign {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corners(size: Vec3) -> Vec<Vec3> {
        (0..8)
            .map(|c| {
                Vec3::new(
                    if c & 1 == 0 { 0.0 } else { size.x },
                    if c & 2 == 0 { 0.0 } else { size.y },
                    if c & 4 == 0 { 0.0 } else { size.z },
                )
            })
            .collect()
    }

    fn viewport() -> Rect {
        Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0))
    }

    #[test]
    fn front_view_keeps_columns_rows_and_shrinks_with_depth() {
        let size = Vec3::new(400.0, 300.0, 600.0);
        let camera = Camera::framing(Preset::Front, &corners(size), viewport());
        let view = camera.view();
        let near_left = view.project(Vec3::new(0.0, 0.0, 0.0), viewport()).unwrap();
        let near_right = view.project(Vec3::new(400.0, 0.0, 0.0), viewport()).unwrap();
        let near_down = view.project(Vec3::new(0.0, 300.0, 0.0), viewport()).unwrap();
        let far_right = view.project(Vec3::new(400.0, 0.0, 600.0), viewport()).unwrap();
        let far_left = view.project(Vec3::new(0.0, 0.0, 600.0), viewport()).unwrap();
        assert!(near_right.pos.x > near_left.pos.x, "columns run left to right");
        assert!(near_down.pos.y > near_left.pos.y, "rows run downwards");
        assert!(far_left.depth > near_left.depth, "earlier times are farther");
        assert!(
            (far_right.pos.x - far_left.pos.x) < (near_right.pos.x - near_left.pos.x),
            "farther cards look smaller"
        );
    }

    #[test]
    fn orbit_zoom_and_pan_move_the_camera_predictably() {
        let size = Vec3::new(400.0, 300.0, 600.0);
        let mut camera = Camera::framing(Preset::Oblique, &corners(size), viewport());
        let mut outline = Rect::NOTHING;
        for corner in corners(size) {
            outline.extend_with(camera.view().project(corner, viewport()).unwrap().pos);
        }
        assert!((outline.center() - viewport().center()).length() < 2.0, "the outline is centered");
        let distance = camera.distance;
        camera.zoom(200.0);
        assert!(camera.distance < distance);
        camera.orbit(100.0, 1_000.0);
        assert!(camera.pitch <= 1.5);
        let pivot = camera.target;
        let before = camera.view().project(pivot, viewport()).unwrap();
        camera.pan(50.0, 0.0, viewport());
        let after = camera.view().project(pivot, viewport()).unwrap();
        assert!((after.pos.x - before.pos.x - 50.0).abs() < 1.0, "pan follows the pointer");
        let view = camera.view();
        let point = view.project(pivot, viewport()).unwrap();
        let moved = pivot.add(view.screen_delta_to_world(egui::vec2(30.0, -20.0), point.depth, viewport()));
        let shifted = view.project(moved, viewport()).unwrap();
        assert!((shifted.pos - point.pos - egui::vec2(30.0, -20.0)).length() < 0.5, "dragging follows the pointer");
        let front = Camera::framing(Preset::Front, &corners(size), viewport()).view();
        assert!(front.faces_camera(Vec3::new(0.0, 0.0, 0.0), Vec3::new(0.0, 0.0, -1.0)), "the near face looks at the viewer");
        assert!(!front.faces_camera(Vec3::new(0.0, 0.0, 600.0), Vec3::new(0.0, 0.0, 1.0)), "the back face is hidden");
    }

    #[test]
    fn points_behind_the_camera_are_not_projected_and_quads_are_picked() {
        let camera = Camera::framing(Preset::Front, &corners(Vec3::new(100.0, 100.0, 100.0)), viewport());
        let view = camera.view();
        assert!(view.project(camera.eye().add(Vec3::new(0.0, 0.0, -10.0)), viewport()).is_none());
        let quad = [
            egui::pos2(0.0, 0.0),
            egui::pos2(10.0, 0.0),
            egui::pos2(10.0, 10.0),
            egui::pos2(0.0, 10.0),
        ];
        assert!(quad_contains(&quad, egui::pos2(5.0, 5.0)));
        assert!(!quad_contains(&quad, egui::pos2(15.0, 5.0)));
    }
}
