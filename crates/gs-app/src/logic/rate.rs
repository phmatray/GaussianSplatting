//! Rate of a counter that is sampled at irregular moments.

/// Measures how fast a monotonic counter grows, from the samples the page
/// happens to see.
///
/// The pipeline's own `Stats::tick_hz` averages the instantaneous rate of
/// every tick. When ticks come in bursts (the worker runs two, then waits
/// for the page), that average is far above the number of ticks per second;
/// the tick counter and the page's clock do not have this problem, and they
/// keep working when Stats messages are skipped.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RateMeter {
    /// Clock and counter at the start of the current window.
    start: Option<(f64, u64)>,
    rate: Option<f32>,
}

impl RateMeter {
    /// A window is closed once it is this long, in milliseconds.
    pub const WINDOW_MS: f64 = 1000.0;
    /// A gap this long between two samples means the stream stopped (a
    /// lost link, a suspended tab): the measure starts over.
    pub const STALE_MS: f64 = 4000.0;

    /// Records `count` seen at clock `now_ms` and returns the rate per
    /// second, once a first window is complete.
    pub fn push(&mut self, now_ms: f64, count: u64) -> Option<f32> {
        match self.start {
            Some((then_ms, then)) if count >= then && now_ms >= then_ms && now_ms - then_ms <= Self::STALE_MS => {
                let elapsed = now_ms - then_ms;
                if elapsed >= Self::WINDOW_MS {
                    let rate = ((count - then) as f64 * 1000.0 / elapsed) as f32;
                    // Light smoothing: the figure is read by a person.
                    self.rate = Some(self.rate.map_or(rate, |old| 0.5 * old + 0.5 * rate));
                    self.start = Some((now_ms, count));
                }
            }
            // The first sample, a counter that went back (a new pipeline)
            // or a stream that had stopped.
            _ => {
                self.start = Some((now_ms, count));
                self.rate = None;
            }
        }
        self.rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steady_ticks_give_their_rate() {
        let mut meter = RateMeter::default();
        let mut rate = None;
        for tick in 0..100u64 {
            rate = meter.push(tick as f64 * 50.0, tick);
        }
        assert!((rate.unwrap() - 20.0).abs() < 0.01, "{rate:?}");
    }

    #[test]
    fn bursts_do_not_inflate_the_rate() {
        // Two ticks 5 ms apart, then 120 ms of waiting: 16 ticks per second,
        // where the mean of the instantaneous rates would say about 100.
        let mut meter = RateMeter::default();
        let (mut now, mut tick, mut rate) = (0.0, 0u64, None);
        for _ in 0..80 {
            for gap in [5.0, 120.0] {
                tick += 1;
                now += gap;
                rate = meter.push(now, tick);
            }
        }
        assert!((rate.unwrap() - 16.0).abs() < 0.5, "{rate:?}");
    }

    #[test]
    fn skipped_samples_do_not_matter() {
        // Stats are latest-wins: the page may see one in five.
        let mut meter = RateMeter::default();
        let mut rate = None;
        for tick in (0..400u64).step_by(5) {
            rate = meter.push(tick as f64 * 50.0, tick);
        }
        assert!((rate.unwrap() - 20.0).abs() < 0.01, "{rate:?}");
    }

    #[test]
    fn nothing_before_a_full_window() {
        let mut meter = RateMeter::default();
        assert_eq!(meter.push(0.0, 10), None);
        assert_eq!(meter.push(500.0, 20), None);
        assert!(meter.push(1000.0, 30).is_some());
    }

    #[test]
    fn restarts_and_gaps_start_over() {
        let mut meter = RateMeter::default();
        for tick in 0..60u64 {
            meter.push(tick as f64 * 50.0, tick);
        }
        assert!(meter.push(3000.0, 60).is_some());
        // A new pipeline counts from zero again.
        assert_eq!(meter.push(3050.0, 1), None);
        assert_eq!(meter.push(3100.0, 2), None);
        assert!(meter.push(4100.0, 22).is_some());
        // A long silence is not a slow rate.
        assert_eq!(meter.push(60_000.0, 23), None);
        // A clock that jumps back is ignored as well.
        assert_eq!(meter.push(100.0, 24), None);
    }
}
