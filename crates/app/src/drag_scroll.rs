//! Drag-to-scroll with flick momentum for lists: press and drag to move the content, release
//! while moving and it keeps gliding, slowing with friction. A press that moves less than
//! [`THRESHOLD`] stays a click; after a real drag, [`DragScroll::dragged`] lets row click
//! handlers ignore the release.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use gpui_kit::{Context, Pixels, Point, ScrollHandle, Task, point, px};

/// Movement (px) before a press becomes a drag.
const THRESHOLD: f32 = 4.;
/// Pointer samples older than this do not count towards the release velocity.
const SAMPLE_WINDOW: Duration = Duration::from_millis(80);
const FRAME: Duration = Duration::from_millis(16);
/// Velocity kept per second of gliding (exponential friction).
const FRICTION_PER_SEC: f32 = 0.04;
/// Below this speed (px/s) a release does not fling, and a fling stops.
const MIN_SPEED: f32 = 40.;
const MAX_SPEED: f32 = 12_000.;

#[derive(Default)]
pub struct DragScroll {
    press: Option<Press>,
    dragged: bool,
    fling: Option<Task<()>>,
}

struct Press {
    start_y: f32,
    last_y: f32,
    samples: VecDeque<(Instant, f32)>,
}

impl DragScroll {
    /// True once the current (or last) press moved far enough to be a drag.
    pub fn dragged(&self) -> bool {
        self.dragged
    }

    /// Left button down: stop any glide and start tracking.
    pub fn press(&mut self, position: Point<Pixels>) {
        let y = f32::from(position.y);
        self.fling = None;
        self.dragged = false;
        self.press = Some(Press {
            start_y: y,
            last_y: y,
            samples: VecDeque::from([(Instant::now(), y)]),
        });
    }

    /// Pointer moved with the button held. Returns true when the content scrolled.
    pub fn drag(&mut self, position: Point<Pixels>, handle: &ScrollHandle) -> bool {
        let Some(press) = &mut self.press else {
            return false;
        };
        let y = f32::from(position.y);
        let now = Instant::now();
        press.samples.push_back((now, y));
        while press
            .samples
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > SAMPLE_WINDOW)
        {
            press.samples.pop_front();
        }
        if !self.dragged && (y - press.start_y).abs() < THRESHOLD {
            return false;
        }
        self.dragged = true;
        let delta = y - press.last_y;
        press.last_y = y;
        scroll_by(handle, delta)
    }

    /// Button released (or the press ended outside): starts a glide when it was a flick.
    pub fn release<V: 'static>(
        &mut self,
        handle: &ScrollHandle,
        cx: &mut Context<V>,
        this: fn(&mut V) -> &mut DragScroll,
    ) {
        let Some(press) = self.press.take() else {
            return;
        };
        if !self.dragged {
            return;
        }
        let velocity = release_velocity(&press.samples, Instant::now());
        if velocity.abs() < MIN_SPEED {
            return;
        }
        let handle = handle.clone();
        self.fling = Some(cx.spawn(async move |view, cx| {
            let mut velocity = velocity.clamp(-MAX_SPEED, MAX_SPEED);
            let mut last = Instant::now();
            loop {
                cx.background_executor().timer(FRAME).await;
                let now = Instant::now();
                let dt = now.duration_since(last).as_secs_f32();
                last = now;
                let moved = scroll_by(&handle, velocity * dt);
                velocity *= FRICTION_PER_SEC.powf(dt);
                let alive = view.update(cx, |_, cx| cx.notify()).is_ok();
                if !alive || !moved || velocity.abs() < MIN_SPEED {
                    break;
                }
            }
            _ = view.update(cx, |view, _| this(view).fling = None);
        }));
    }
}

/// Speed (px/s, positive = content moves down) over the recent samples; 0 when the pointer
/// rested before release.
fn release_velocity(samples: &VecDeque<(Instant, f32)>, now: Instant) -> f32 {
    let recent: Vec<_> = samples
        .iter()
        .filter(|(t, _)| now.duration_since(*t) <= SAMPLE_WINDOW)
        .collect();
    let (Some((t0, y0)), Some((t1, y1))) = (recent.first(), recent.last()) else {
        return 0.;
    };
    let dt = t1.duration_since(*t0).as_secs_f32();
    if dt < 0.005 { 0. } else { (y1 - y0) / dt }
}

/// Moves the content by `delta` px (positive = towards the top of the list), clamped to the
/// scroll range. Returns false when already at the end in that direction.
fn scroll_by(handle: &ScrollHandle, delta: f32) -> bool {
    let offset = handle.offset();
    let max = f32::from(handle.max_offset().y);
    let y = (f32::from(offset.y) + delta).clamp(-max, 0.);
    if (y - f32::from(offset.y)).abs() < 0.01 {
        return false;
    }
    handle.set_offset(point(offset.x, px(y)));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn velocity_from_recent_samples() {
        let t = Instant::now();
        let samples = VecDeque::from([
            (t, 0.),
            (t + Duration::from_millis(20), 10.),
            (t + Duration::from_millis(40), 20.),
        ]);
        let v = release_velocity(&samples, t + Duration::from_millis(40));
        assert!((v - 500.).abs() < 1., "{v}");
    }

    #[test]
    fn resting_before_release_does_not_fling() {
        let t = Instant::now();
        let samples = VecDeque::from([(t, 0.), (t + Duration::from_millis(20), 30.)]);
        assert_eq!(
            release_velocity(&samples, t + Duration::from_millis(300)),
            0.
        );
    }

    #[test]
    fn friction_stops_a_fast_flick_within_two_seconds() {
        let mut v = MAX_SPEED;
        let mut elapsed = 0.;
        while v.abs() >= MIN_SPEED {
            v *= FRICTION_PER_SEC.powf(0.016);
            elapsed += 0.016;
        }
        assert!(elapsed < 2., "{elapsed}");
    }
}
