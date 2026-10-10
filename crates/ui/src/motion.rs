//! Small, reusable animation helpers over GPUI's animation primitives.
//!
//! Every helper takes a `reduce` flag (pass `cx.reduce_motion()`): when set it returns the element
//! in its resting state with no animation, so the system "reduce motion" setting is honoured in
//! one place. Animation ids must be unique per element; for one-shot transitions include a
//! counter in the id so each switch restarts the animation. Non-repeating animations stop
//! scheduling frames once they finish, repeating ones repaint at [`FPS_CAP`] for as long as they
//! show.

use std::time::Duration;

use gpui_kit::component::Icon;
use gpui_kit::{
    Animation, AnimationExt as _, AnyElement, ElementId, Hsla, IntoElement, Pixels, Rgba, Styled,
    ease_out_quint, linear, pulsating_between, radians,
};

/// Quick state changes: hover-adjacent fades, colour swaps.
pub const FAST: Duration = Duration::from_millis(200);
/// Position changes such as a sliding indicator.
pub const SLIDE: Duration = Duration::from_millis(200);
/// One revolution of a busy spinner or one loop of a busy icon.
pub const CYCLE: Duration = Duration::from_millis(1000);
/// A slow "breathing" glow.
pub const BREATHE: Duration = Duration::from_millis(2400);
/// Frame cap for animations that repaint continuously.
pub const FPS_CAP: f32 = 30.;

/// Rotates `icon` one full turn every `period`, forever.
pub fn spin(icon: Icon, id: impl Into<ElementId>, period: Duration, reduce: bool) -> AnyElement {
    if reduce {
        return icon.into_any_element();
    }
    icon.with_animation(
        id,
        Animation::new(period)
            .repeat()
            .with_easing(linear)
            .with_max_fps(FPS_CAP),
        |icon, t| icon.rotate(radians(t * std::f32::consts::TAU)),
    )
    .into_any_element()
}

/// Breathes `el`'s opacity between `min` and `max` every `period`, forever. Reduced: static at
/// `max`.
pub fn pulse<E>(
    el: E,
    id: impl Into<ElementId>,
    min: f32,
    max: f32,
    period: Duration,
    reduce: bool,
) -> AnyElement
where
    E: Styled + IntoElement + 'static,
{
    if reduce {
        return el.opacity(max).into_any_element();
    }
    el.with_animation(
        id,
        Animation::new(period)
            .repeat()
            .with_easing(pulsating_between(min, max))
            .with_max_fps(FPS_CAP),
        |el, alpha| el.opacity(alpha),
    )
    .into_any_element()
}

/// Fades `el` from transparent to opaque once over `dur`.
pub fn fade_in<E>(el: E, id: impl Into<ElementId>, dur: Duration, reduce: bool) -> AnyElement
where
    E: Styled + IntoElement + 'static,
{
    if reduce {
        return el.into_any_element();
    }
    el.with_animation(
        id,
        Animation::new(dur).with_easing(ease_out_quint()),
        |el, t| el.opacity(t),
    )
    .into_any_element()
}

/// Moves an absolutely positioned `el` vertically once over `dur`, its `top` going from `from` to
/// `to` (ease-out).
pub fn slide_y<E>(
    el: E,
    id: impl Into<ElementId>,
    from: Pixels,
    to: Pixels,
    dur: Duration,
    reduce: bool,
) -> AnyElement
where
    E: Styled + IntoElement + 'static,
{
    if reduce || from == to {
        return el.top(to).into_any_element();
    }
    el.with_animation(
        id,
        Animation::new(dur).with_easing(ease_out_quint()),
        move |el, t| el.top(from + (to - from) * t),
    )
    .into_any_element()
}

/// Runs `apply(el, t)` once over `dur` with `t` going 0 to 1 (ease-out) and stopping at 1;
/// reduced, `apply(el, 1.)` is applied directly. For one-off effects the other helpers don't
/// cover, such as a colour cross-fade with [`lerp_color`].
pub fn transition<E>(
    el: E,
    id: impl Into<ElementId>,
    dur: Duration,
    reduce: bool,
    apply: impl Fn(E, f32) -> E + 'static,
) -> AnyElement
where
    E: IntoElement + 'static,
{
    if reduce {
        return apply(el, 1.).into_any_element();
    }
    el.with_animation(id, Animation::new(dur).with_easing(ease_out_quint()), apply)
        .into_any_element()
}

/// Linear blend between two colours in RGB space (`t` 0 gives `a`, 1 gives `b`).
pub fn lerp_color(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let (a, b) = (Rgba::from(a), Rgba::from(b));
    let mix = |x: f32, y: f32| x + (y - x) * t;
    Hsla::from(Rgba {
        r: mix(a.r, b.r),
        g: mix(a.g, b.g),
        b: mix(a.b, b.b),
        a: mix(a.a, b.a),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::hsla;

    #[test]
    fn lerp_hits_both_ends() {
        let (a, b) = (hsla(0., 0., 0., 1.), hsla(0., 0., 1., 1.));
        assert_eq!(Rgba::from(lerp_color(a, b, 0.)), Rgba::from(a));
        let end = Rgba::from(lerp_color(a, b, 1.));
        assert!((end.r - 1.).abs() < 1e-4 && (end.a - 1.).abs() < 1e-4);
    }
}
