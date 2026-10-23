//! Gesture button: turns press, movement and release into a click or one of
//! four directions.

/// Result of a finished gesture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gesture {
    Click,
    Up,
    Down,
    Left,
    Right,
}

impl Gesture {
    /// The `GestureSet` slot of this gesture.
    pub fn key(self) -> &'static str {
        crate::config::GestureSet::SLOTS[self as usize]
    }
}

/// The dominant axis must be at least this many times the other one;
/// anything closer to a diagonal is no gesture.
const AXIS_RATIO: i32 = 2;
/// Up and down open Exposé / Mission Control, so they need this much more
/// movement than left and right: `threshold * NUM / DEN`.
const VERTICAL_NUM: i32 = 3;
const VERTICAL_DEN: i32 = 2;
/// Up and down also need at least this many movement reports: a jolt when
/// the button is pressed arrives as one or two big reports (e.g. one with
/// y=200), a real swipe as dozens. Left and right do not need it (fast
/// switching of desktops).
pub const MIN_VERTICAL_MOVES: u32 = 5;

/// Accumulates movement while the gesture button is held.
///
/// The direction is decided by the first movement that is clear enough: the
/// axis (and direction) is locked as soon as one axis passes its threshold
/// and dominates the other, and later movement does not change it.
#[derive(Debug)]
pub struct Tracker {
    threshold: i32,
    held: bool,
    x: i32,
    y: i32,
    /// Movement reports since the press.
    reports: u32,
    /// Horizontal movement produces repeated steps while the button is held.
    continuous: bool,
    /// A horizontal stepping session has started for the current press.
    stepping: bool,
    /// Movement not yet converted into steps.
    acc: i32,
    /// The direction locked for the current press.
    locked: Option<Gesture>,
    /// The last release had movement, but no clear direction.
    ambiguous: bool,
}

impl Tracker {
    /// `threshold`: minimal movement (raw units) that counts as a gesture.
    pub fn new(threshold: u16) -> Self {
        Self {
            threshold: i32::from(threshold.max(1)),
            held: false,
            x: 0,
            y: 0,
            reports: 0,
            continuous: false,
            stepping: false,
            acc: 0,
            locked: None,
            ambiguous: false,
        }
    }

    /// In continuous mode horizontal movement yields steps from `motion`
    /// (like Options+ app switching) instead of one gesture on release.
    pub fn set_continuous(&mut self, on: bool) {
        self.continuous = on;
    }

    pub fn press(&mut self) {
        self.held = true;
        self.x = 0;
        self.y = 0;
        self.reports = 0;
        self.stepping = false;
        self.acc = 0;
        self.locked = None;
        self.ambiguous = false;
    }

    /// Total movement of the current (or last) press: `(x, y)`.
    pub fn sums(&self) -> (i32, i32) {
        (self.x, self.y)
    }

    /// True if the last release had movement without a clear direction.
    pub fn ambiguous(&self) -> bool {
        self.ambiguous
    }

    /// Movement needed for up and down.
    fn vertical_threshold(&self) -> i32 {
        self.threshold * VERTICAL_NUM / VERTICAL_DEN
    }

    /// Locks the direction once the movement so far is clear enough.
    fn try_lock(&mut self) {
        let (ax, ay) = (self.x.abs(), self.y.abs());
        if ay >= self.vertical_threshold()
            && ay >= AXIS_RATIO * ax
            && self.reports >= MIN_VERTICAL_MOVES
        {
            // Raw Y grows downwards (towards the user).
            self.locked = Some(if self.y > 0 { Gesture::Down } else { Gesture::Up });
        } else if ax >= self.threshold && (self.continuous || ax >= AXIS_RATIO * ay) {
            // In continuous mode any horizontal movement over the threshold
            // that is not clearly vertical starts the steps.
            self.locked = Some(if self.x > 0 { Gesture::Right } else { Gesture::Left });
        }
    }

    /// Adds movement. In continuous mode returns the number of horizontal
    /// steps (positive = right, negative = left); the first step comes with
    /// the first horizontal move of `threshold` units, then one per
    /// `threshold` units. Otherwise always 0.
    pub fn motion(&mut self, dx: i16, dy: i16) -> i32 {
        if !self.held {
            return 0;
        }
        self.reports = self.reports.saturating_add(1);
        self.x = self.x.saturating_add(i32::from(dx));
        self.y = self.y.saturating_add(i32::from(dy));
        if !self.continuous {
            if self.locked.is_none() {
                self.try_lock();
            }
            return 0;
        }
        if self.stepping {
            self.acc += i32::from(dx);
            let steps = self.acc / self.threshold;
            self.acc -= steps * self.threshold;
            return steps;
        }
        if self.locked.is_none() {
            self.try_lock();
            if let Some(g @ (Gesture::Left | Gesture::Right)) = self.locked {
                self.stepping = true;
                self.acc = 0;
                return if g == Gesture::Right { 1 } else { -1 };
            }
        }
        0
    }

    /// Ends the gesture; `None` if the button was not held or nothing is to
    /// be run (steps already done, or no clear direction: see `ambiguous`).
    pub fn release(&mut self) -> Option<Gesture> {
        if !std::mem::take(&mut self.held) {
            return None;
        }
        self.ambiguous = false;
        if std::mem::take(&mut self.stepping) {
            return None; // already handled as steps
        }
        if let Some(g) = self.locked {
            return Some(g);
        }
        if self.x.abs().max(self.y.abs()) < self.threshold {
            // A click fires on release, however long the button was held.
            return Some(Gesture::Click);
        }
        self.ambiguous = true;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_match_the_slots() {
        use crate::config::GestureSet;
        let all = [Gesture::Click, Gesture::Up, Gesture::Down, Gesture::Left, Gesture::Right];
        let mut set = GestureSet::standard();
        for (g, slot) in all.into_iter().zip(GestureSet::SLOTS) {
            assert_eq!(g.key(), slot);
            assert!(set.get(slot).is_some());
            *set.slot_mut(slot).unwrap() = None;
            assert_eq!(set.get(slot), None);
        }
    }

    /// A movement of `(dx, dy)` as `MIN_VERTICAL_MOVES` reports.
    fn split(dx: i16, dy: i16) -> Vec<(i16, i16)> {
        let n = MIN_VERTICAL_MOVES as i16;
        let mut v = vec![(dx / n, dy / n); n as usize - 1];
        v.push((dx - dx / n * (n - 1), dy - dy / n * (n - 1)));
        v
    }

    fn run(moves: &[(i16, i16)]) -> Option<Gesture> {
        let mut t = Tracker::new(50);
        t.press();
        for &(dx, dy) in moves {
            t.motion(dx, dy);
        }
        t.release()
    }

    #[test]
    fn click_when_small_movement() {
        assert_eq!(run(&[]), Some(Gesture::Click));
        assert_eq!(run(&[(10, -10), (5, 5)]), Some(Gesture::Click));
    }

    #[test]
    fn four_directions() {
        assert_eq!(run(&[(30, 5), (30, 0)]), Some(Gesture::Right));
        assert_eq!(run(&[(-60, 10)]), Some(Gesture::Left));
        assert_eq!(run(&split(5, -80)), Some(Gesture::Up));
        assert_eq!(run(&split(-5, 80)), Some(Gesture::Down));
    }

    #[test]
    fn continuous_steps() {
        let mut t = Tracker::new(50);
        t.set_continuous(true);
        t.press();
        assert_eq!(t.motion(30, 0), 0);
        assert_eq!(t.motion(30, 0), 1); // first step at threshold
        assert_eq!(t.motion(40, 0), 0);
        assert_eq!(t.motion(70, 0), 2); // 110 units = 2 steps
        assert_eq!(t.motion(-60, 0), -1);
        assert_eq!(t.release(), None);
        // Vertical gesture still works in continuous mode.
        t.press();
        for (dx, dy) in split(5, -80) {
            t.motion(dx, dy);
        }
        assert_eq!(t.release(), Some(Gesture::Up));
        t.press();
        assert_eq!(t.release(), Some(Gesture::Click));
    }

    #[test]
    fn hold_without_movement_clicks_only_on_release() {
        let mut t = Tracker::new(50);
        t.set_continuous(true);
        t.press();
        // Nothing happens while held, however long; only release clicks.
        assert_eq!(t.motion(5, -5), 0);
        assert_eq!(t.release(), Some(Gesture::Click));
        assert_eq!(t.release(), None);
    }

    #[test]
    fn sideways_hold_gives_steps_and_no_click() {
        let mut t = Tracker::new(50);
        t.set_continuous(true);
        t.press();
        assert_eq!(t.motion(60, 0), 1);
        assert_eq!(t.motion(-110, 0), -2);
        assert_eq!(t.release(), None);
        // Vertical swipe: no click either, direction on release.
        t.press();
        for (dx, dy) in split(0, -80) {
            t.motion(dx, dy);
        }
        assert_eq!(t.release(), Some(Gesture::Up));
    }

    #[test]
    fn ignores_motion_when_not_held_and_resets() {
        let mut t = Tracker::new(50);
        t.motion(500, 0);
        assert_eq!(t.release(), None);
        t.press();
        t.motion(100, 0);
        assert_eq!(t.release(), Some(Gesture::Right));
        t.press();
        assert_eq!(t.release(), Some(Gesture::Click));
    }

    #[test]
    fn diagonal_is_no_action() {
        assert_eq!(run(&[(60, 60)]), None);
        assert_eq!(run(&[(60, -70), (10, -10)]), None);
        assert_eq!(run(&[(-80, 60)]), None);
        let mut t = Tracker::new(50);
        t.press();
        t.motion(60, 60);
        assert_eq!(t.release(), None);
        assert!(t.ambiguous());
        assert_eq!(t.sums(), (60, 60));
        t.press();
        assert_eq!(t.release(), Some(Gesture::Click));
        assert!(!t.ambiguous());
    }

    #[test]
    fn slight_slant_horizontal_is_left_right() {
        assert_eq!(run(&[(80, 35)]), Some(Gesture::Right));
        assert_eq!(run(&[(-80, -35)]), Some(Gesture::Left));
        assert_eq!(run(&[(40, 20), (40, 20)]), Some(Gesture::Right));
        // Slight slant in vertical needs the larger vertical threshold too.
        assert_eq!(run(&split(20, -90)), Some(Gesture::Up));
    }

    #[test]
    fn vertical_needs_more_than_horizontal() {
        // 74 < 1.5 * 50: nothing, but 74 sideways is already a gesture.
        assert_eq!(run(&split(0, 74)), None);
        assert_eq!(run(&split(0, -74)), None);
        assert_eq!(run(&[(74, 0)]), Some(Gesture::Right));
        assert_eq!(run(&split(0, 75)), Some(Gesture::Down));
        assert_eq!(run(&split(0, -75)), Some(Gesture::Up));
    }

    #[test]
    fn direction_comes_from_the_first_clear_movement() {
        // Locked to the right; coming back and moving down changes nothing.
        assert_eq!(run(&[(60, 0), (-60, 0), (0, 200)]), Some(Gesture::Right));
        // Locked to down; a long sideways move afterwards changes nothing.
        let mut moves = split(0, 80);
        moves.push((300, 0));
        assert_eq!(run(&moves), Some(Gesture::Down));
    }

    #[test]
    fn a_vertical_gesture_needs_enough_reports() {
        // One jolt of y=200 (a real case from the log) is no "down".
        assert_eq!(run(&[(-5, 200)]), None);
        assert_eq!(run(&[(0, -200)]), None);
        assert_eq!(run(&[(0, 100), (0, 100), (0, 100), (0, 100)]), None);
        // Six reports of 40 are "down", the fifth report locks it.
        assert_eq!(run(&[(0, 40); 6]), Some(Gesture::Down));
        assert_eq!(run(&[(0, -40); 6]), Some(Gesture::Up));
        // Left and right do not need that.
        assert_eq!(run(&[(-200, 5)]), Some(Gesture::Left));
        // A jolt first does not block a real swipe in the other direction.
        let mut moves = vec![(0, 100)];
        moves.extend([(-60, 0); 4]);
        assert_eq!(run(&moves), Some(Gesture::Left));
    }

    #[test]
    fn continuous_diagonal_down_steps_sideways() {
        let mut t = Tracker::new(50);
        t.set_continuous(true);
        t.press();
        assert_eq!(t.motion(30, 35), 0);
        assert_eq!(t.motion(30, 35), 1); // x=60 over the threshold, y=70 < 75
        assert_eq!(t.motion(-60, 0), -1);
        assert_eq!(t.motion(0, 300), 0); // no vertical gesture after steps
        assert_eq!(t.release(), None);
        assert!(!t.ambiguous());
    }

    #[test]
    fn continuous_vertical_first_is_vertical() {
        let mut t = Tracker::new(50);
        t.set_continuous(true);
        t.press();
        for (dx, dy) in split(10, 80) {
            assert_eq!(t.motion(dx, dy), 0);
        }
        assert_eq!(t.motion(200, 0), 0);
        assert_eq!(t.release(), Some(Gesture::Down));
        // Weak vertical movement and nothing else: no action.
        t.press();
        for (dx, dy) in split(0, 60) {
            t.motion(dx, dy);
        }
        assert_eq!(t.release(), None);
        assert!(t.ambiguous());
    }
}
