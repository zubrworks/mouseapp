//! Trackpad-like wheel scrolling: wheel steps become a smooth pixel
//! movement that, after a fast spin, glides on and slows down by friction,
//! like a swipe on a trackpad.
//!
//! The pixels are posted without gesture or momentum phases. Phased events
//! make apps treat the wheel as fingers on a trackpad: Safari then stretches
//! the page past its edge (rubber band, the page jumps back) and pulling
//! past the top reloads the page. Without phases the movement simply stops
//! at the edge.
//!
//! [`Glide`] only computes the pixels; the tap posts them and calls
//! [`Glide::frame`] every [`FRAME`] while [`Glide::active`] is true.

use std::time::{Duration, Instant};

/// Time between two animation frames (120 per second).
pub(super) const FRAME: Duration = Duration::from_micros(8_333);
/// A wheel step is covered in about this time (seconds) at constant speed.
const STEP_TIME: f64 = 0.1;
/// Exponential part of the glide friction: time constant in seconds.
const FRICTION_TIME: f64 = 0.35;
/// Constant part of the glide friction, px/s²: slow glides stop soon.
const FRICTION_DECEL: f64 = 1500.0;
/// A movement slower than this (px/s) stops without a glide.
const GLIDE_MIN: f64 = 300.0;
/// A glide below this speed (px/s) is over.
const GLIDE_STOP: f64 = 15.0;
/// Longest time step of one frame (s): a late frame does not jump.
const MAX_DT: f64 = 0.05;

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    Idle,
    /// The wheel is turning: `left` pixels still to cover at `speed` px/s
    /// (both with the sign of the movement).
    Moving {
        left: f64,
        speed: f64,
    },
    /// Gliding on at `velocity` px/s after the wheel stopped.
    Gliding {
        velocity: f64,
    },
}

/// The movement of the main wheel in trackpad mode.
#[derive(Debug)]
pub(super) struct Glide {
    state: State,
    /// Fraction of a pixel not posted yet.
    rest: f64,
    /// Time of the last frame or input.
    last: Option<Instant>,
}

impl Default for Glide {
    fn default() -> Self {
        Glide {
            state: State::Idle,
            rest: 0.0,
            last: None,
        }
    }
}

fn same_sign(a: f64, b: f64) -> bool {
    a.signum() == b.signum()
}

impl Glide {
    /// True while frames are needed (a movement or glide is running).
    pub fn active(&self) -> bool {
        self.state != State::Idle
    }

    /// Whole pixels of `v`, the remainder carried to the next frame.
    fn take(&mut self, v: f64) -> i32 {
        let total = self.rest + v;
        let whole = total.round();
        self.rest = total - whole;
        whole as i32
    }

    /// Seconds since the last frame or input (at most [`MAX_DT`]).
    fn elapsed(&mut self, now: Instant) -> f64 {
        let dt = self.last.map_or(FRAME.as_secs_f64(), |t| {
            now.saturating_duration_since(t).as_secs_f64()
        });
        self.last = Some(now);
        dt.min(MAX_DT)
    }

    /// A wheel event moved by `pixels` (already scaled and signed); returns
    /// the pixels to post now (0 = nothing). While the wheel turns the
    /// distance adds up; the opposite direction starts over at once; a
    /// glide in the same direction keeps its pace.
    pub fn input(&mut self, pixels: f64, now: Instant) -> i32 {
        if pixels == 0.0 || !pixels.is_finite() {
            return 0;
        }
        let speed = pixels / STEP_TIME;
        match self.state {
            State::Moving { left, speed: old } if same_sign(left, pixels) || left == 0.0 => {
                let left = left + pixels;
                // Never slower than now: a fast spin keeps its pace.
                let speed = pixels.signum() * (left.abs() / STEP_TIME).max(old.abs());
                self.state = State::Moving { left, speed };
                self.last.get_or_insert(now);
                0
            }
            State::Moving { .. } => {
                // Turned back: follow the new direction from the next frame.
                self.state = State::Moving {
                    left: pixels,
                    speed,
                };
                self.rest = 0.0;
                0
            }
            State::Gliding { velocity } => {
                let speed = if same_sign(velocity, pixels) {
                    pixels.signum() * speed.abs().max(velocity.abs())
                } else {
                    speed
                };
                self.start(pixels, speed, now)
            }
            State::Idle => self.start(pixels, speed, now),
        }
    }

    /// Starts a movement; its first frame is posted right away.
    fn start(&mut self, left: f64, speed: f64, now: Instant) -> i32 {
        self.rest = 0.0;
        self.last = Some(now);
        self.state = State::Moving { left, speed };
        self.advance(FRAME.as_secs_f64())
    }

    /// Moves on by `dt` seconds. Once the distance of the wheel is covered
    /// a fast movement glides on, a slow one stops.
    fn advance(&mut self, dt: f64) -> i32 {
        match self.state {
            State::Idle => 0,
            State::Moving { left, speed } => {
                let mut moved = speed * dt;
                let done = moved.abs() >= left.abs();
                if done {
                    moved = left;
                }
                let px = self.take(moved);
                self.state = if !done {
                    State::Moving {
                        left: left - moved,
                        speed,
                    }
                } else if speed.abs() >= GLIDE_MIN {
                    State::Gliding { velocity: speed }
                } else {
                    self.rest = 0.0;
                    State::Idle
                };
                px
            }
            State::Gliding { velocity } => {
                let next = slow_down(velocity, dt);
                if next == 0.0 {
                    self.state = State::Idle;
                    self.rest = 0.0;
                    return 0;
                }
                self.state = State::Gliding { velocity: next };
                // Distance at the mean speed of the frame.
                self.take((velocity + next) / 2.0 * dt)
            }
        }
    }

    /// One animation frame: the pixels to post (0 = nothing).
    pub fn frame(&mut self, now: Instant) -> i32 {
        let dt = self.elapsed(now);
        self.advance(dt)
    }

    /// Stops at once (settings changed, the program stops).
    pub fn stop(&mut self) {
        self.state = State::Idle;
        self.rest = 0.0;
    }
}

/// Wheel events closer than this belong to one spin. The free-spinning
/// wheel of the mouse keeps sending events for a while after the hand left.
const SPIN_GAP: Duration = Duration::from_millis(150);

/// Drops the rest of a spin once Cmd, Ctrl or Opt goes down during it: the
/// wheel events that follow would reach the app with that key (zoom, ...).
/// A wheel turned with the key held from the start is not affected.
#[derive(Debug, Default)]
pub(super) struct SpinGuard {
    /// Time of the last event and whether it had a key down.
    last: Option<(Instant, bool)>,
    dropping: bool,
}

impl SpinGuard {
    /// Call for every vertical wheel event of trackpad mode; `modifier` is
    /// true if Cmd, Ctrl or Opt is down. True = drop the event.
    pub fn event(&mut self, modifier: bool, now: Instant) -> bool {
        let prev = self
            .last
            .filter(|(t, _)| now.saturating_duration_since(*t) < SPIN_GAP);
        let drop = modifier && prev.is_some_and(|(_, had)| !had || self.dropping);
        self.dropping = drop;
        self.last = Some((now, modifier));
        drop
    }
}

/// Velocity after `dt` seconds of friction; 0 once it is below [`GLIDE_STOP`].
fn slow_down(velocity: f64, dt: f64) -> f64 {
    let v = velocity.abs() * (-dt / FRICTION_TIME).exp() - FRICTION_DECEL * dt;
    if v < GLIDE_STOP {
        0.0
    } else {
        velocity.signum() * v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs frames until the glide is idle; returns the posted pixels of
    /// every frame and the end time.
    fn run(g: &mut Glide, mut now: Instant) -> (Vec<i32>, Instant) {
        let mut all = Vec::new();
        for _ in 0..2000 {
            if !g.active() {
                return (all, now);
            }
            now += FRAME;
            all.push(g.frame(now));
        }
        panic!("glide never ended: {g:?}");
    }

    #[test]
    fn spin_guard_drops_the_spin_after_a_key_goes_down() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut g = SpinGuard::default();
        // A spin, then Cmd goes down: the rest of it is dropped.
        assert!(!g.event(false, t0));
        assert!(!g.event(false, t0 + ms(20)));
        assert!(g.event(true, t0 + ms(40)));
        assert!(g.event(true, t0 + ms(120)));
        assert!(g.event(true, t0 + ms(250)));
        // A pause ends the spin: a new turn with the key is let through,
        // and so is the rest of it.
        assert!(!g.event(true, t0 + ms(600)));
        assert!(!g.event(true, t0 + ms(620)));
        assert!(!g.event(true, t0 + ms(640)));
        // The key released: plain events again.
        assert!(!g.event(false, t0 + ms(660)));
    }

    #[test]
    fn spin_guard_leaves_a_deliberate_key_wheel_alone() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut g = SpinGuard::default();
        assert!(!g.event(true, t0));
        assert!(!g.event(true, t0 + ms(30)));
        assert!(!g.event(true, t0 + ms(60)));
        // A plain spin after the key was released is not touched.
        assert!(!g.event(false, t0 + ms(90)));
        assert!(!g.event(false, t0 + ms(120)));
        // Pressing the key again during it drops the rest.
        assert!(g.event(true, t0 + ms(150)));
        // Releasing the key during the spin ends the dropping.
        assert!(!g.event(false, t0 + ms(170)));
    }

    fn is_gliding(g: &Glide) -> bool {
        matches!(g.state, State::Gliding { .. })
    }

    #[test]
    fn slow_step_covers_its_distance_without_glide() {
        let t0 = Instant::now();
        let mut g = Glide::default();
        let first = g.input(24.0, t0);
        assert!(first > 0, "the first frame moves at once");
        let mut glided = false;
        let mut now = t0;
        let mut total = first;
        while g.active() {
            now += FRAME;
            total += g.frame(now);
            glided |= is_gliding(&g);
        }
        assert_eq!(total, 24);
        assert!(!glided);
        // About STEP_TIME, then still.
        assert!(now - t0 <= Duration::from_millis(130), "{:?}", now - t0);
    }

    #[test]
    fn fast_spin_glides_on_and_stops() {
        let mut now = Instant::now();
        let t0 = now;
        let mut g = Glide::default();
        let mut moved = 0;
        // 10 steps of 60 px every 16 ms (a fast spin).
        for _ in 0..10 {
            moved += g.input(60.0, now);
            for _ in 0..2 {
                now += FRAME;
                moved += g.frame(now);
            }
            assert!(!is_gliding(&g), "no glide while the wheel turns");
        }
        let (rest, end) = run(&mut g, now);
        let total = moved + rest.iter().sum::<i32>();
        // The wheel's distance plus a glide.
        assert!(total > 700, "total {total} px");
        assert!(rest.iter().all(|p| *p >= 0), "direction kept: {rest:?}");
        // The glide slows down to a stop within a few seconds.
        let tail: Vec<i32> = rest.iter().rev().take(20).copied().collect();
        assert!(tail.iter().all(|p| *p <= 3), "slow at the end: {tail:?}");
        assert!(end - t0 < Duration::from_secs(3), "{:?}", end - t0);
        assert!(!g.active());
    }

    #[test]
    fn glide_slows_down_every_frame() {
        let mut g = Glide {
            state: State::Gliding { velocity: -3000.0 },
            ..Glide::default()
        };
        let t0 = Instant::now();
        g.last = Some(t0);
        let (moves, _) = run(&mut g, t0);
        let moves: Vec<i32> = moves.into_iter().filter(|p| *p != 0).collect();
        assert!(moves.iter().all(|p| *p < 0), "direction kept: {moves:?}");
        assert!(
            moves.windows(2).all(|w| w[1].abs() <= w[0].abs() + 1),
            "slowing down: {moves:?}"
        );
    }

    #[test]
    fn friction_ends_the_glide() {
        assert_eq!(slow_down(GLIDE_STOP, 0.01), 0.0);
        assert_eq!(slow_down(-20.0, 0.01), 0.0);
        let v = slow_down(-2000.0, 0.01);
        assert!(v < -1900.0 && v > -2000.0, "{v}");
    }

    #[test]
    fn wheel_during_glide_keeps_or_reverses_it() {
        // Same direction: the glide's pace is kept.
        let mut g = Glide {
            state: State::Gliding { velocity: 2000.0 },
            ..Glide::default()
        };
        assert!(g.input(10.0, Instant::now()) >= 10);
        assert!(is_gliding(&g), "a short step goes on gliding");
        // Opposite direction: the glide stops and the wheel is followed.
        let mut g = Glide {
            state: State::Gliding { velocity: 2000.0 },
            ..Glide::default()
        };
        let t0 = Instant::now();
        let first = g.input(-24.0, t0);
        let (rest, _) = run(&mut g, t0);
        assert!(
            first <= 0 && rest.iter().all(|p| *p <= 0),
            "{first} {rest:?}"
        );
        assert_eq!(first + rest.iter().sum::<i32>(), -24);
    }

    #[test]
    fn turning_back_follows_at_once() {
        let t0 = Instant::now();
        let mut g = Glide::default();
        g.input(100.0, t0);
        assert_eq!(g.input(-24.0, t0 + FRAME), 0);
        let (moves, _) = run(&mut g, t0 + FRAME);
        assert!(moves.iter().all(|p| *p <= 0), "{moves:?}");
        assert_eq!(moves.iter().sum::<i32>(), -24);
    }

    #[test]
    fn stop_ends_at_once() {
        let mut g = Glide::default();
        g.input(500.0, Instant::now());
        g.stop();
        assert!(!g.active());
        assert_eq!(g.frame(Instant::now()), 0);
    }

    #[test]
    fn late_frame_does_not_jump() {
        let t0 = Instant::now();
        let mut g = Glide::default();
        g.input(1000.0, t0);
        // At most MAX_DT worth of movement (10000 px/s * 0.05 s).
        assert!(g.frame(t0 + Duration::from_secs(2)) <= 501);
    }

    #[test]
    fn fractions_carry_within_a_movement() {
        let t0 = Instant::now();
        let mut g = Glide::default();
        let mut total = 0;
        let mut now = t0;
        for _ in 0..5 {
            total += g.input(0.4, now);
            now += FRAME;
            total += g.frame(now);
        }
        total += run(&mut g, now).0.iter().sum::<i32>();
        assert_eq!(total, 2);
        assert_eq!(g.input(0.0, t0), 0);
        assert_eq!(g.input(f64::NAN, t0), 0);
    }
}
