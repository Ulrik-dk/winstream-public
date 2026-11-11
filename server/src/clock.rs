use std::{sync::OnceLock, time::Instant};

static START: OnceLock<Instant> = OnceLock::new();

pub fn init() {
    START.get_or_init(Instant::now);
}

pub fn now_ns() -> u64 {
    ns_at(Instant::now())
}

pub fn ns_at(t: Instant) -> u64 {
    t.saturating_duration_since(*START.get_or_init(Instant::now))
        .as_nanos() as u64
}
