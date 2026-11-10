//! Pacing: how fast DriveCraft sends requests.
//!
//! A server that is asked too much answers "slow down" and names a wait. Rushing
//! straight back in at the old speed earns a longer wait each time, so the total
//! comes out slower than a steady pace would have been. The pacer spaces requests
//! out evenly instead. It starts fast, and learns the server's tolerance from the
//! server itself:
//!
//! * when the server asks for a wait, the pace is cut to four fifths of what was
//!   actually being sent, and held there;
//! * for half a minute after a wait ends, requests go at half that pace, so the
//!   return is gentle;
//! * after a quarter of an hour without being asked to slow down, the pace creeps
//!   back up a little at a time to find out whether there is more room.
//!
//! The decisions are plain arithmetic on a clock that is passed in, so they can be
//! tested without waiting.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The fastest pace, in requests a second. Also the starting pace.
pub const MAX_RATE: f64 = 20.0;
/// The slowest pace the pacer will fall to.
pub const MIN_RATE: f64 = 0.5;
/// After a cut, the pace is not raised above the cut for this long.
const HOLD: Duration = Duration::from_secs(15 * 60);
/// Time between upward steps, and the size of a step.
const STEP_EVERY: Duration = Duration::from_secs(30);
const STEP: f64 = 1.05;
/// How much of the measured pace survives a cut.
const CUT: f64 = 0.8;
/// Requests thrown back together count as one cut.
const SAME_EVENT: Duration = Duration::from_secs(10);
/// After a wait, requests go at half pace for this long.
const EASE: Duration = Duration::from_secs(30);
/// How far back the pacer looks when measuring what was really being sent.
const MEASURE: Duration = Duration::from_secs(60);

struct State {
    rate: f64,
    ceiling: f64,
    next_slot: Instant,
    last_change: Instant,
    last_cut: Option<Instant>,
    ease_until: Option<Instant>,
    recent: VecDeque<Instant>,
    cuts: u64,
}

pub struct Pacer {
    state: Mutex<State>,
}

impl Pacer {
    pub fn new(now: Instant) -> Self {
        Self {
            state: Mutex::new(State {
                rate: MAX_RATE,
                ceiling: MAX_RATE,
                next_slot: now,
                last_change: now,
                last_cut: None,
                ease_until: None,
                recent: VecDeque::new(),
                cuts: 0,
            }),
        }
    }

    /// The pace now allowed, in requests a second.
    pub fn rate(&self) -> f64 {
        self.state.lock().unwrap().rate
    }

    /// How many times the pace was cut.
    pub fn cuts(&self) -> u64 {
        self.state.lock().unwrap().cuts
    }

    /// Take the next sending slot. Returns how long to wait before sending.
    pub fn reserve(&self, now: Instant) -> Duration {
        let mut s = self.state.lock().unwrap();
        if now.duration_since(s.last_change) >= STEP_EVERY {
            let held = s.last_cut.is_some_and(|c| now.duration_since(c) < HOLD);
            let limit = if held { s.ceiling } else { MAX_RATE };
            if s.rate < limit {
                s.rate = (s.rate * STEP).min(limit);
            }
            s.last_change = now;
        }
        let easing = s.ease_until.is_some_and(|u| now < u);
        let effective = if easing { s.rate / 2.0 } else { s.rate };
        let slot = s.next_slot.max(now);
        s.next_slot = slot + Duration::from_secs_f64(1.0 / effective);
        s.recent.push_back(slot);
        while s.recent.front().is_some_and(|t| now.duration_since(*t) > MEASURE) {
            s.recent.pop_front();
        }
        slot.duration_since(now)
    }

    /// The server asked for a wait of `wait`. Returns the new pace when this cut it;
    /// `None` when it belongs to a cut that was already made a moment ago.
    pub fn slow_down(&self, now: Instant, wait: Duration) -> Option<f64> {
        let mut s = self.state.lock().unwrap();
        let resume = now + wait;
        s.next_slot = s.next_slot.max(resume);
        s.ease_until = Some(s.ease_until.map_or(resume + EASE, |u| u.max(resume + EASE)));
        if s.last_cut.is_some_and(|c| now.duration_since(c) < SAME_EVENT) {
            return None;
        }
        // What was really being sent may be well below what was allowed, when the
        // requests themselves are slow. That is the pace the server objected to.
        // Shortly after an earlier cut the last minute is mostly waiting and says
        // nothing, so then the allowed pace is cut instead.
        let fresh = s.last_cut.is_none_or(|c| now.duration_since(c) > MEASURE * 2);
        let mut basis = s.rate;
        if fresh {
            let sent = s.recent.iter().filter(|t| **t <= now).count();
            if let Some(first) = s.recent.front() {
                let span = now.duration_since(*first).as_secs_f64().clamp(5.0, MEASURE.as_secs_f64());
                basis = basis.min((sent as f64 / span).max(1.0));
            }
        }
        s.rate = (basis * CUT).clamp(MIN_RATE, MAX_RATE);
        s.ceiling = s.rate;
        s.last_cut = Some(now);
        s.last_change = resume;
        s.cuts += 1;
        Some(s.rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    /// Send as fast as the pacer allows for `for_secs`; returns how many were sent.
    fn run(p: &Pacer, start: Instant, for_secs: f64) -> (u64, Instant) {
        let mut now = start;
        let end = start + secs(for_secs);
        let mut n = 0;
        loop {
            let at = now + p.reserve(now);
            if at >= end {
                return (n, end);
            }
            now = at;
            n += 1;
        }
    }

    #[test]
    fn spaces_requests_evenly_and_gives_no_credit_for_idle_time() {
        let t0 = Instant::now();
        let p = Pacer::new(t0);
        assert_eq!(p.reserve(t0), Duration::ZERO);
        let w = p.reserve(t0);
        assert!((w.as_secs_f64() - 0.05).abs() < 0.001, "{w:?}");
        // A long quiet spell does not allow a burst afterwards.
        let later = t0 + secs(20.0);
        assert_eq!(p.reserve(later), Duration::ZERO);
        assert!(p.reserve(later) > Duration::from_millis(40));
    }

    #[test]
    fn a_wait_cuts_the_pace_to_what_was_really_sent_and_eases_back() {
        let t0 = Instant::now();
        let p = Pacer::new(t0);
        // Requests are slow: only ten a second really go out.
        let mut now = t0;
        for _ in 0..600 {
            p.reserve(now);
            now += secs(0.1);
        }
        let new = p.slow_down(now, secs(40.0)).expect("cut");
        assert!((new - 8.0).abs() < 0.3, "four fifths of ten a second, got {new}");
        // Others thrown back in the same moment do not cut again.
        assert_eq!(p.slow_down(now + secs(1.0), secs(45.0)), None);
        assert_eq!(p.cuts(), 1);
        // Nothing is sent until the longest wait is over.
        let w = p.reserve(now + secs(2.0));
        assert!(w >= secs(43.9), "{w:?}");
        // The first half minute back is at half pace, then the full new pace.
        let back = now + secs(46.0);
        let (eased, t) = run(&p, back, 30.0);
        assert!((100..=135).contains(&eased), "about four a second for 30s, got {eased}");
        let (steady, _) = run(&p, t + secs(1.0), 30.0);
        assert!((225..=255).contains(&steady), "about eight a second for 30s, got {steady}");
    }

    #[test]
    fn the_pace_is_held_after_a_cut_then_creeps_back_up() {
        let t0 = Instant::now();
        let p = Pacer::new(t0);
        let mut now = t0;
        for _ in 0..300 {
            p.reserve(now);
            now += secs(0.2);
        }
        let cut = p.slow_down(now, secs(10.0)).unwrap();
        assert!((cut - 4.0).abs() < 0.2, "{cut}");
        // Ten minutes later it has not gone above the cut.
        let (_, t) = run(&p, now + secs(10.0), 600.0);
        assert!((p.rate() - cut).abs() < 1e-9);
        // Well past the hold it has started to look for more room.
        let (_, _) = run(&p, t, 600.0);
        assert!(p.rate() > cut * 1.2, "{}", p.rate());
        assert!(p.rate() <= MAX_RATE);
    }

    #[test]
    fn repeated_waits_keep_cutting_but_never_below_the_floor() {
        let t0 = Instant::now();
        let p = Pacer::new(t0);
        let mut now = t0;
        for _ in 0..40 {
            p.reserve(now);
            p.slow_down(now, secs(5.0));
            now += secs(20.0);
        }
        assert_eq!(p.rate(), MIN_RATE);
    }
}
