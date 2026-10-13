//! Scroll elasticity and rebound physics for GPUI scroll containers.
//!
//! The spring model follows Photon Engine's `Compositing::ElasticOverscroll`
//! so browser content and native GPUI surfaces respond consistently.

use std::{collections::VecDeque, time::Instant};

use crate::{Pixels, Point};

/// Scales the rendered stretch; lower values make edge overscroll gentler.
const ELASTIC_SCROLL_INTENSITY: f32 = 0.8;
const RUBBER_BAND_STIFFNESS: f32 = 12.0;
const RUBBER_BAND_AMPLITUDE: f32 = 0.31;
const RUBBER_BAND_PERIOD: f64 = 1.2;
const REBOUND_DECAY_PER_SECOND: f64 = RUBBER_BAND_STIFFNESS as f64 / RUBBER_BAND_PERIOD;
const LARGEST_BOUNCE_AS_SHARE_OF_SCROLLPORT: f32 = 0.2;
const STRETCH_THAT_COUNTS_AS_SPRUNG_BACK: f32 = 0.25;
const VELOCITY_SAMPLING_WINDOW_SECONDS: f64 = 0.1;
const MAXIMUM_PAUSE_BEFORE_LIFT_SECONDS: f64 = 0.1;
const MAX_TRACKED_STEPS: usize = 20;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Vector {
    x: f32,
    y: f32,
}

impl Vector {
    fn from_point(point: Point<Pixels>) -> Self {
        Self {
            x: f32::from(point.x),
            y: f32::from(point.y),
        }
    }

    fn to_point(self) -> Point<Pixels> {
        Point::new(Pixels::from(self.x), Pixels::from(self.y))
    }

    fn scaled(self, scale: f32) -> Self {
        Self {
            x: self.x * scale,
            y: self.y * scale,
        }
    }

    fn is_zero(self) -> bool {
        self.x == 0.0 && self.y == 0.0
    }
}

#[derive(Clone, Copy, Debug)]
struct ScrollStep {
    delta: Vector,
    time: Instant,
}

#[derive(Clone, Copy, Debug)]
struct Rebound {
    started_at: Instant,
    initial_stretch: Vector,
    initial_velocity: Vector,
    current_stretch: Vector,
}

/// The current stretch and analytic rebound for a scrollable element.
#[derive(Debug, Default)]
pub(crate) struct ElasticScroll {
    scrollport_size: Vector,
    accumulated_overscroll: Vector,
    rebound: Option<Rebound>,
    velocity_steps: VecDeque<ScrollStep>,
}

impl ElasticScroll {
    pub(crate) fn set_scrollport_size(&mut self, size: Point<Pixels>) {
        self.scrollport_size = Vector::from_point(size);
    }

    pub(crate) fn stretch(&self) -> Point<Pixels> {
        self.rebound
            .map_or_else(
                || {
                    self.accumulated_overscroll
                        .scaled(1.0 / RUBBER_BAND_STIFFNESS)
                },
                |rebound| rebound.current_stretch,
            )
            .scaled(ELASTIC_SCROLL_INTENSITY)
            .to_point()
    }

    pub(crate) fn is_at_rest(&self) -> bool {
        self.rebound.is_none() && self.accumulated_overscroll.is_zero()
    }

    pub(crate) fn add_step(&mut self, delta: Point<Pixels>, time: Instant) {
        if self.velocity_steps.len() == MAX_TRACKED_STEPS {
            self.velocity_steps.pop_front();
        }
        self.velocity_steps.push_back(ScrollStep {
            delta: Vector::from_point(delta),
            time,
        });
    }

    /// Returns the part of a finger delta left for ordinary scrolling after it
    /// first relaxes any existing stretch.
    pub(crate) fn relax_by(&mut self, delta: Point<Pixels>) -> Point<Pixels> {
        if let Some(rebound) = self.rebound.take() {
            self.accumulated_overscroll = rebound.current_stretch.scaled(RUBBER_BAND_STIFFNESS);
        }
        let delta = Vector::from_point(delta);
        let remaining = Vector {
            x: relax_axis(self.accumulated_overscroll.x, delta.x),
            y: relax_axis(self.accumulated_overscroll.y, delta.y),
        };
        self.accumulated_overscroll.x = relax_axis_value(self.accumulated_overscroll.x, delta.x);
        self.accumulated_overscroll.y = relax_axis_value(self.accumulated_overscroll.y, delta.y);
        remaining.to_point()
    }

    /// Add the unconsumed part of a precise scroll gesture at an edge.
    pub(crate) fn pull_by(&mut self, delta: Point<Pixels>) {
        debug_assert!(self.rebound.is_none());
        let delta = Vector::from_point(delta);
        let max_x = self.scrollport_size.x * RUBBER_BAND_STIFFNESS;
        let max_y = self.scrollport_size.y * RUBBER_BAND_STIFFNESS;
        self.accumulated_overscroll.x =
            (self.accumulated_overscroll.x + delta.x).clamp(-max_x, max_x);
        self.accumulated_overscroll.y =
            (self.accumulated_overscroll.y + delta.y).clamp(-max_y, max_y);
    }

    pub(crate) fn release(&mut self, velocity: Option<Point<Pixels>>, now: Instant) -> bool {
        if self.rebound.is_some() {
            return false;
        }
        let initial_stretch = self
            .accumulated_overscroll
            .scaled(1.0 / RUBBER_BAND_STIFFNESS);
        if initial_stretch.is_zero() {
            self.velocity_steps.clear();
            return false;
        }
        self.accumulated_overscroll = Vector::default();
        let velocity = velocity.map_or(Vector::default(), Vector::from_point);
        self.rebound = Some(Rebound {
            started_at: now,
            initial_stretch,
            initial_velocity: Vector {
                x: release_velocity_for_axis(initial_stretch.x, velocity.x, self.scrollport_size.x),
                y: release_velocity_for_axis(initial_stretch.y, velocity.y, self.scrollport_size.y),
            },
            current_stretch: initial_stretch,
        });
        self.velocity_steps.clear();
        true
    }

    /// Advances the rebound to an absolute frame time. Absolute sampling keeps
    /// the curve stable across missed frames and different refresh rates.
    pub(crate) fn advance_rebound_to(&mut self, frame_time: Instant) -> bool {
        let Some(rebound) = self.rebound.as_mut() else {
            return false;
        };
        let elapsed = frame_time
            .checked_duration_since(rebound.started_at)
            .unwrap_or_default()
            .as_secs_f64();
        let horizontal = sample_rebound_axis(
            rebound.initial_stretch.x,
            rebound.initial_velocity.x,
            self.scrollport_size.x,
            elapsed,
        );
        let vertical = sample_rebound_axis(
            rebound.initial_stretch.y,
            rebound.initial_velocity.y,
            self.scrollport_size.y,
            elapsed,
        );
        if horizontal.sprung_back && vertical.sprung_back {
            self.rebound = None;
            return false;
        }
        rebound.current_stretch = Vector {
            x: if horizontal.sprung_back {
                0.0
            } else {
                horizontal.stretch
            },
            y: if vertical.sprung_back {
                0.0
            } else {
                vertical.stretch
            },
        };
        true
    }

    pub(crate) fn velocity_at_lift(&self, lift_time: Instant) -> Option<Point<Pixels>> {
        let last_step = *self.velocity_steps.back()?;
        if lift_time
            .checked_duration_since(last_step.time)
            .unwrap_or_default()
            .as_secs_f64()
            > MAXIMUM_PAUSE_BEFORE_LIFT_SECONDS
        {
            return None;
        }
        let mut first = self.velocity_steps.len() - 1;
        while first > 0
            && last_step
                .time
                .checked_duration_since(self.velocity_steps[first - 1].time)
                .unwrap_or_default()
                .as_secs_f64()
                <= VELOCITY_SAMPLING_WINDOW_SECONDS
        {
            first -= 1;
        }
        let point_count = self.velocity_steps.len() - first;
        if point_count < 2 {
            return None;
        }

        let mut sum_t = 0.0;
        let mut sum_tt = 0.0;
        let mut sum_x = 0.0;
        let mut sum_y = 0.0;
        let mut sum_tx = 0.0;
        let mut sum_ty = 0.0;
        let mut x = 0.0;
        let mut y = 0.0;
        for step in self.velocity_steps.iter().skip(first) {
            x += f64::from(step.delta.x);
            y += f64::from(step.delta.y);
            // `Instant` only represents unsigned durations, while the Engine
            // fit uses signed offsets relative to the newest step.
            let t = -last_step
                .time
                .checked_duration_since(step.time)
                .unwrap_or_default()
                .as_secs_f64();
            sum_t += t;
            sum_tt += t * t;
            sum_x += x;
            sum_y += y;
            sum_tx += t * x;
            sum_ty += t * y;
        }
        let n = point_count as f64;
        let determinant = n * sum_tt - sum_t * sum_t;
        if determinant <= 0.0 {
            return None;
        }
        Some(Point::new(
            Pixels::from(((n * sum_tx - sum_t * sum_x) / determinant) as f32),
            Pixels::from(((n * sum_ty - sum_t * sum_y) / determinant) as f32),
        ))
    }

    pub(crate) fn clear(&mut self) {
        self.accumulated_overscroll = Vector::default();
        self.rebound = None;
        self.velocity_steps.clear();
    }
}

fn relax_axis(accumulated: f32, delta: f32) -> f32 {
    if !((accumulated > 0.0 && delta < 0.0) || (accumulated < 0.0 && delta > 0.0)) {
        return delta;
    }
    if delta.abs() <= accumulated.abs() {
        0.0
    } else {
        delta + accumulated
    }
}

fn relax_axis_value(accumulated: f32, delta: f32) -> f32 {
    if !((accumulated > 0.0 && delta < 0.0) || (accumulated < 0.0 && delta > 0.0)) {
        return accumulated;
    }
    if delta.abs() <= accumulated.abs() {
        accumulated + delta
    } else {
        0.0
    }
}

fn fastest_release_speed_for(scrollport_length: f32) -> f32 {
    (f64::from(LARGEST_BOUNCE_AS_SHARE_OF_SCROLLPORT)
        * f64::from(scrollport_length)
        * REBOUND_DECAY_PER_SECOND
        * std::f64::consts::E
        / f64::from(RUBBER_BAND_AMPLITUDE)) as f32
}

fn release_velocity_for_axis(stretch: f32, velocity: f32, scrollport_length: f32) -> f32 {
    if stretch == 0.0 {
        return 0.0;
    }
    velocity.clamp(
        -fastest_release_speed_for(scrollport_length),
        fastest_release_speed_for(scrollport_length),
    )
}

struct ReboundSample {
    stretch: f32,
    sprung_back: bool,
}

fn sample_rebound_axis(
    initial_stretch: f32,
    initial_velocity: f32,
    scrollport_length: f32,
    elapsed_seconds: f64,
) -> ReboundSample {
    if initial_stretch == 0.0 {
        return ReboundSample {
            stretch: 0.0,
            sprung_back: true,
        };
    }
    let elapsed = elapsed_seconds as f32;
    let before_decay = initial_stretch + RUBBER_BAND_AMPLITUDE * initial_velocity * elapsed;
    if (before_decay > 0.0) != (initial_stretch > 0.0) {
        return ReboundSample {
            stretch: 0.0,
            sprung_back: true,
        };
    }
    let stretch = (before_decay * (-elapsed_seconds * REBOUND_DECAY_PER_SECOND).exp() as f32)
        .clamp(-scrollport_length, scrollport_length);
    let growth_before_decay =
        RUBBER_BAND_AMPLITUDE * initial_velocity - REBOUND_DECAY_PER_SECOND as f32 * before_decay;
    let still_stretching = before_decay * growth_before_decay > 0.0;
    ReboundSample {
        stretch,
        sprung_back: !still_stretching && stretch.abs() < STRETCH_THAT_COUNTS_AS_SPRUNG_BACK,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::px;
    use std::time::Duration;

    fn after(start: Instant, milliseconds: u64) -> Instant {
        start + Duration::from_millis(milliseconds)
    }

    fn viewport() -> Point<Pixels> {
        Point::new(px(800.0), px(600.0))
    }

    #[test]
    fn stretch_uses_engine_curve_at_configured_intensity() {
        let mut scroll = ElasticScroll::default();
        scroll.set_scrollport_size(viewport());
        scroll.pull_by(Point::new(px(0.0), px(-19.0)));
        let expected_scale = ELASTIC_SCROLL_INTENSITY / RUBBER_BAND_STIFFNESS;
        assert!((f32::from(scroll.stretch().y) + 19.0 * expected_scale).abs() < 0.001);
        scroll.pull_by(Point::new(px(0.0), px(-381.0)));
        assert!((f32::from(scroll.stretch().y) + 400.0 * expected_scale).abs() < 0.001);
    }

    #[test]
    fn release_rebound_is_time_based_and_never_crosses_the_edge() {
        let mut scroll = ElasticScroll::default();
        scroll.set_scrollport_size(viewport());
        scroll.pull_by(Point::new(px(0.0), px(-1200.0)));
        let released = Instant::now();
        scroll.release(None, released);
        let mut previous = f32::from(scroll.stretch().y);
        let mut rebounding = true;
        for milliseconds in (8..=1000).step_by(8) {
            rebounding = scroll.advance_rebound_to(after(released, milliseconds));
            let stretch = f32::from(scroll.stretch().y);
            assert!(stretch <= 0.0);
            assert!(stretch >= previous);
            previous = stretch;
            if !rebounding {
                break;
            }
        }
        assert!(!rebounding);
        assert!(scroll.is_at_rest());
    }

    #[test]
    fn inward_release_velocity_returns_to_the_edge_without_crossing_it() {
        let mut scroll = ElasticScroll::default();
        scroll.set_scrollport_size(viewport());
        scroll.pull_by(Point::new(px(0.0), px(-120.0)));
        let released = Instant::now();
        scroll.release(Some(Point::new(px(0.0), px(1_000.0))), released);

        let mut previous = f32::from(scroll.stretch().y);
        for milliseconds in (8..=80).step_by(8) {
            let rebounding = scroll.advance_rebound_to(after(released, milliseconds));
            let stretch = f32::from(scroll.stretch().y);
            assert!(stretch <= 0.0);
            assert!(stretch >= previous);
            previous = stretch;
            if !rebounding {
                break;
            }
        }

        assert!(scroll.is_at_rest());
        assert_eq!(scroll.stretch(), Point::new(px(0.0), px(0.0)));
    }

    #[test]
    fn fingers_can_take_over_a_rebound_without_jumping() {
        let mut scroll = ElasticScroll::default();
        scroll.set_scrollport_size(viewport());
        scroll.pull_by(Point::new(px(0.0), px(-1200.0)));
        let released = Instant::now();
        scroll.release(None, released);
        assert!(scroll.advance_rebound_to(after(released, 50)));
        let before = f32::from(scroll.stretch().y);
        assert_eq!(
            scroll.relax_by(Point::new(px(0.0), px(-40.0))),
            Point::new(px(0.0), px(-40.0))
        );
        assert!((f32::from(scroll.stretch().y) - before).abs() < 1.0);
    }

    #[test]
    fn velocity_uses_recent_steps_and_rejects_a_long_pause_before_lift() {
        let mut scroll = ElasticScroll::default();
        let start = Instant::now();
        for milliseconds in [0, 10, 20] {
            scroll.add_step(Point::new(px(0.0), px(-10.0)), after(start, milliseconds));
        }

        let velocity = scroll.velocity_at_lift(after(start, 30)).unwrap();
        assert!(f32::from(velocity.y + px(1_000.0)).abs() < 0.1);
        assert!(scroll.velocity_at_lift(after(start, 121)).is_none());
    }

    #[test]
    fn release_velocity_is_limited_to_a_configured_share_of_the_viewport() {
        let mut scroll = ElasticScroll::default();
        scroll.set_scrollport_size(viewport());
        scroll.pull_by(Point::new(px(0.0), px(4.0)));
        let released = Instant::now();
        scroll.release(Some(Point::new(px(1_000_000.0), px(1_000_000.0))), released);
        let mut furthest = 0.0f32;
        for milliseconds in (8..=2000).step_by(8) {
            if !scroll.advance_rebound_to(after(released, milliseconds)) {
                break;
            }
            furthest = furthest.max(f32::from(scroll.stretch().y));
        }
        assert!(furthest > 60.0);
        assert!(
            furthest
                <= 600.0 * LARGEST_BOUNCE_AS_SHARE_OF_SCROLLPORT * ELASTIC_SCROLL_INTENSITY + 1.0
        );
    }
}
