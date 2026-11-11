use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use super::Settings;

const MAX_BACKLOG: u32 = 2;
const SKIP_BACKLOG: u32 = 8;
const PROBE_INTERVAL: Duration = Duration::from_secs(1);
const UP: f64 = 1.01;
const FAST_UP: f64 = 1.03;
const DOWN: f64 = 0.97;
const MAX_DOWN: f64 = 0.85;
const HOLD: f64 = 0.8;

pub struct RateControl {
    bitrate: u32,
    cap: u32,
    in_flight: VecDeque<(u64, Instant)>,
    backlog: Duration,
    last_probe: Option<Instant>,
    congested: bool,
    target: Duration,
    fps: f64,
    max_bpp: f64,
    min_bitrate: u32,
    latency: Duration,
}

impl RateControl {
    pub fn new(width: u32, height: u32, settings: Settings) -> Self {
        let fps = settings.fps as f64;
        let pixels = width as f64 * height as f64 * fps;
        let min_bitrate = settings.min_bitrate;
        let cap = ((pixels * settings.max_bpp) as u32).max(min_bitrate);
        let bitrate = ((pixels * settings.start_bpp) as u32).clamp(min_bitrate, cap);
        RateControl {
            bitrate,
            cap,
            in_flight: VecDeque::new(),
            backlog: Duration::ZERO,
            last_probe: None,
            congested: false,
            target: settings.latency,
            fps,
            max_bpp: settings.max_bpp,
            min_bitrate,
            latency: Duration::ZERO,
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        let pixels = width as f64 * height as f64 * self.fps;
        self.cap = ((pixels * self.max_bpp) as u32).max(self.min_bitrate);
        self.bitrate = self.bitrate.min(self.cap);
    }

    pub fn bitrate(&self) -> u32 {
        self.bitrate
    }

    pub fn backlog(&self) -> Duration {
        self.backlog
    }

    pub fn latency(&self) -> Duration {
        self.latency
    }

    pub fn backlog_at(&self, now: Instant) -> Duration {
        self.in_flight.front().map_or(Duration::ZERO, |&(_, sent)| {
            now.saturating_duration_since(sent)
        })
    }

    pub fn should_skip(&mut self, now: Instant) -> bool {
        let limit = self.target * SKIP_BACKLOG + Duration::from_millis(proto::FEEDBACK_INTERVAL_MS);
        if self.backlog_at(now) <= limit {
            return false;
        }
        if self
            .last_probe
            .is_none_or(|t| now.saturating_duration_since(t) >= PROBE_INTERVAL)
        {
            self.last_probe = Some(now);
            return false;
        }
        true
    }

    pub fn on_sent(&mut self, frame_id: u64, now: Instant) {
        self.in_flight.push_back((frame_id, now));
        if self.in_flight.len() > 600 {
            self.in_flight.pop_front();
        }
    }

    pub fn on_feedback(
        &mut self,
        received: u32,
        shown: u32,
        last_frame_id: u64,
        latency: Duration,
        now: Instant,
    ) -> Option<u32> {
        while self
            .in_flight
            .front()
            .is_some_and(|&(id, _)| id <= last_frame_id)
        {
            self.in_flight.pop_front();
        }
        let before = self.backlog;
        self.backlog = self
            .in_flight
            .front()
            .map_or(Duration::ZERO, |&(_, sent)| now - sent);
        if received == 0 {
            return None;
        }
        if !latency.is_zero() {
            self.latency = latency;
        }
        let measured = !latency.is_zero();
        let piled_up = self.backlog > self.target * MAX_BACKLOG;
        let factor = if measured && latency > self.target {
            (self.target.as_secs_f64() / latency.as_secs_f64()).clamp(MAX_DOWN, DOWN)
        } else if shown < received || (piled_up && self.backlog >= before) {
            DOWN
        } else if piled_up || (measured && latency > self.target.mul_f64(HOLD)) {
            return None;
        } else if self.congested {
            UP
        } else {
            FAST_UP
        };
        if factor < 1.0 {
            self.congested = true;
        }
        let new = ((self.bitrate as f64 * factor) as u32).clamp(self.min_bitrate, self.cap);
        (new != self.bitrate).then(|| {
            self.bitrate = new;
            new
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FPS: f64 = 60.0;

    const TARGET: Duration = Duration::from_millis(15);
    const SETTINGS: Settings = Settings {
        fps: 60,
        latency: TARGET,
        start_bpp: 0.08,
        max_bpp: 1.0,
        min_bitrate: 2_000_000,
    };

    struct Queued {
        id: u64,
        left: f64,
        bits: f64,
        sent: Instant,
    }

    struct Link {
        capacity: f64,
        base: Duration,
        decode_per_mbit: Duration,
        queue: VecDeque<Queued>,
        next_id: u64,
        now: Instant,
        sent: u32,
        latencies: Vec<Duration>,
        reports_latency: bool,
    }

    impl Link {
        fn new(capacity: f64) -> Self {
            Link {
                capacity,
                base: Duration::from_millis(2),
                decode_per_mbit: Duration::ZERO,
                queue: VecDeque::new(),
                next_id: 0,
                now: Instant::now(),
                sent: 0,
                latencies: Vec::new(),
                reports_latency: false,
            }
        }

        fn run(&mut self, rc: &mut RateControl, seconds: f64) -> Vec<u32> {
            let frame = Duration::from_secs_f64(1.0 / FPS);
            let mut rates = Vec::new();
            for _ in 0..(seconds * 10.0) as usize {
                let (mut received, mut last, mut sum) = (0, None, Duration::ZERO);
                for _ in 0..6 {
                    if !rc.should_skip(self.now) {
                        let bits = rc.bitrate() as f64 / FPS;
                        self.queue.push_back(Queued {
                            id: self.next_id,
                            left: bits,
                            bits,
                            sent: self.now,
                        });
                        rc.on_sent(self.next_id, self.now);
                        self.sent += 1;
                    }
                    self.next_id += 1;
                    let budget = self.capacity / FPS;
                    let mut spent = 0.0;
                    while let Some(q) = self.queue.front_mut() {
                        if q.left > budget - spent {
                            q.left -= budget - spent;
                            break;
                        }
                        spent += q.left;
                        let at = self.now + Duration::from_secs_f64(spent / self.capacity);
                        let latency =
                            at - q.sent + self.base + self.decode_per_mbit.mul_f64(q.bits / 1e6);
                        sum += latency;
                        self.latencies.push(latency);
                        received += 1;
                        last = Some(q.id);
                        self.queue.pop_front();
                    }
                    self.now += frame;
                }
                if let Some(last) = last {
                    let latency = if self.reports_latency {
                        sum / received
                    } else {
                        Duration::ZERO
                    };
                    rc.on_feedback(received, received, last, latency, self.now);
                }
                rates.push(rc.bitrate());
            }
            rates
        }
    }

    fn largest_step(rates: &[u32]) -> f64 {
        rates
            .windows(2)
            .map(|w| w[1] as f64 / w[0] as f64 - 1.0)
            .fold(0.0, f64::max)
    }

    #[test]
    fn climbs_to_the_cap_on_a_fast_link() {
        let mut rc = RateControl::new(2560, 1440, SETTINGS);
        let rates = Link::new(500e6).run(&mut rc, 10.0);
        assert_eq!(rc.bitrate(), rc.cap);
        assert!(largest_step(&rates) <= 0.031);
    }

    #[test]
    fn settles_on_a_slow_link_in_small_steps() {
        let mut rc = RateControl::new(2560, 1440, SETTINGS);
        let mut link = Link::new(30e6);
        link.run(&mut rc, 30.0);
        let rates = link.run(&mut rc, 60.0);
        let (lo, hi) = (*rates.iter().min().unwrap(), *rates.iter().max().unwrap());
        assert!(lo > 24_000_000 && hi < 32_000_000, "{lo}..{hi}");
        assert!(largest_step(&rates) <= 0.031);
    }

    #[test]
    fn follows_a_slower_and_then_faster_link() {
        let mut rc = RateControl::new(2560, 1440, SETTINGS);
        let mut link = Link::new(500e6);
        link.run(&mut rc, 15.0);
        link.capacity = 15e6;
        let rates = link.run(&mut rc, 20.0);
        let (lowest, last) = (*rates.iter().min().unwrap(), *rates.last().unwrap());
        assert!(last < 16_000_000, "ended at {last}");
        assert!(lowest > 9_000_000, "fell to {lowest}");
        link.capacity = 500e6;
        let rates = link.run(&mut rc, 30.0);
        assert_eq!(*rates.last().unwrap(), rc.cap);
    }

    #[test]
    fn total_stall_queues_little_and_recovers() {
        let mut rc = RateControl::new(2560, 1440, SETTINGS);
        let mut link = Link::new(60e6);
        link.run(&mut rc, 10.0);
        let before = link.sent;
        link.capacity = 0.0;
        link.run(&mut rc, 2.0);
        let during = link.sent - before;
        assert!(during <= 17, "{during} frames queued during a 2 s stall");
        assert!(rc.backlog_at(link.now) > Duration::from_millis(1900));
        link.capacity = 60e6;
        link.run(&mut rc, 0.5);
        assert!(
            rc.backlog_at(link.now) < TARGET * MAX_BACKLOG,
            "backlog {:?}",
            rc.backlog_at(link.now)
        );
        assert!(
            link.queue.len() <= 2,
            "{} frames still queued",
            link.queue.len()
        );
    }

    #[test]
    fn keeps_probing_without_reports() {
        let mut rc = RateControl::new(1280, 720, SETTINGS);
        let mut now = Instant::now();
        let mut sent = 0;
        for id in 0..600 {
            if !rc.should_skip(now) {
                rc.on_sent(id, now);
                sent += 1;
            }
            now += Duration::from_secs_f64(1.0 / FPS);
        }
        assert!((15..=26).contains(&sent), "{sent} frames sent");
    }

    #[test]
    fn resize_keeps_the_bitrate() {
        let mut rc = RateControl::new(2560, 1440, SETTINGS);
        let mut link = Link::new(15e6);
        link.run(&mut rc, 30.0);
        let before = rc.bitrate();
        rc.resize(1280, 1440);
        assert_eq!(rc.bitrate(), before);
        rc.resize(2560, 1440);
        assert_eq!(rc.bitrate(), before);
        rc.resize(640, 360);
        assert_eq!(rc.bitrate(), rc.cap);
    }

    #[test]
    fn holds_the_latency_target() {
        let mut rc = RateControl::new(2560, 1440, SETTINGS);
        let mut link = Link::new(500e6);
        link.reports_latency = true;
        link.decode_per_mbit = Duration::from_millis(4);
        link.run(&mut rc, 20.0);
        link.latencies.clear();
        let rates = link.run(&mut rc, 30.0);
        let worst = *link.latencies.iter().max().unwrap();
        assert!(worst <= TARGET.mul_f64(1.1), "latency up to {worst:?}");
        let lo = *rates.iter().min().unwrap() as f64 / 1e6;
        assert!(lo > 100.0, "fell to {lo:.0} Mbit/s");
        assert!(rc.bitrate() < rc.cap);
    }

    #[test]
    fn falls_fast_when_far_over_target() {
        let mut rc = RateControl::new(2560, 1440, SETTINGS);
        let mut link = Link::new(500e6);
        link.reports_latency = true;
        link.decode_per_mbit = Duration::from_millis(4);
        link.run(&mut rc, 20.0);
        link.decode_per_mbit = Duration::from_millis(12);
        link.run(&mut rc, 1.0);
        assert!(
            rc.latency() <= TARGET.mul_f64(1.05),
            "still {:?} after 1 s",
            rc.latency()
        );
    }

    #[test]
    fn a_slow_link_keeps_frames_under_the_target() {
        let mut rc = RateControl::new(2560, 1440, SETTINGS);
        let mut link = Link::new(30e6);
        link.reports_latency = true;
        link.run(&mut rc, 30.0);
        link.latencies.clear();
        link.run(&mut rc, 30.0);
        let worst = *link.latencies.iter().max().unwrap();
        assert!(worst <= TARGET.mul_f64(1.1), "latency up to {worst:?}");
    }

    #[test]
    fn no_reports_no_changes() {
        let mut rc = RateControl::new(2560, 1440, SETTINGS);
        let before = rc.bitrate();
        assert_eq!(
            rc.on_feedback(0, 0, 0, Duration::ZERO, Instant::now()),
            None
        );
        assert_eq!(rc.bitrate(), before);
    }
}
