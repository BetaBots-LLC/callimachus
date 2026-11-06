//! Wall and CPU time for one background job, for its `app_log!` line.
//!
//! Why: the installed app used 16 h of CPU in 46 h of uptime while every sample caught it
//! idle, and its log had nothing but "backend ready" lines, so there was no way to tell
//! which job (catch-up, watcher batches, semantic build, distill) burned it. Each job now
//! logs one line with its wall time, its CPU time and an item count.
//!
//! CPU time comes from `clock_gettime` (no new crate): the calling thread's clock for work
//! that runs on that thread (scans, housekeeping), the whole process's clock for work that
//! fans out to other threads (ONNX Runtime runs inference on its own intra-op pool, so the
//! caller's thread clock would miss almost all of a build). The process clock also counts
//! whatever else ran meanwhile, which the log line says. Windows reports wall time only.

use std::time::{Duration, Instant};

/// A started job clock.
pub(crate) struct JobClock {
    wall: Instant,
    thread: Option<Duration>,
    process: Option<Duration>,
}

impl JobClock {
    pub(crate) fn start() -> Self {
        Self {
            wall: Instant::now(),
            thread: cpu::thread(),
            process: cpu::process(),
        }
    }

    pub(crate) fn wall(&self) -> Duration {
        self.wall.elapsed()
    }

    /// CPU this thread spent since `start`, if the platform reports it.
    pub(crate) fn thread_cpu(&self) -> Option<Duration> {
        Some(cpu::thread()?.saturating_sub(self.thread?))
    }

    /// CPU every thread of the process spent since `start`, if the platform reports it.
    pub(crate) fn process_cpu(&self) -> Option<Duration> {
        Some(cpu::process()?.saturating_sub(self.process?))
    }

    /// "812 ms, 640 ms cpu" for work on the calling thread.
    pub(crate) fn thread_summary(&self) -> String {
        summary(self.wall(), self.thread_cpu(), "cpu")
    }

    /// "4.1 s, 61.0 s cpu (process)" for work that fans out to other threads.
    pub(crate) fn process_summary(&self) -> String {
        summary(self.wall(), self.process_cpu(), "cpu (process)")
    }
}

fn summary(wall: Duration, cpu: Option<Duration>, label: &str) -> String {
    match cpu {
        Some(cpu) => format!("{}, {} {label}", fmt_duration(wall), fmt_duration(cpu)),
        None => fmt_duration(wall),
    }
}

/// Milliseconds under 10 s, seconds with one decimal above.
pub(crate) fn fmt_duration(d: Duration) -> String {
    if d < Duration::from_secs(10) {
        format!("{} ms", d.as_millis())
    } else {
        format!("{:.1} s", d.as_secs_f64())
    }
}

#[cfg(all(
    target_pointer_width = "64",
    any(target_os = "macos", target_os = "linux")
))]
mod cpu {
    use std::time::Duration;

    /// `struct timespec` on 64-bit macOS and Linux: `time_t` and `long` are both 64-bit.
    #[repr(C)]
    struct Timespec {
        tv_sec: i64,
        tv_nsec: i64,
    }

    extern "C" {
        fn clock_gettime(clock: i32, tp: *mut Timespec) -> i32;
    }

    // <time.h> values (checked against the macOS SDK and glibc headers).
    #[cfg(target_os = "macos")]
    const PROCESS: i32 = 12;
    #[cfg(target_os = "macos")]
    const THREAD: i32 = 16;
    #[cfg(target_os = "linux")]
    const PROCESS: i32 = 2;
    #[cfg(target_os = "linux")]
    const THREAD: i32 = 3;

    fn read(clock: i32) -> Option<Duration> {
        let mut ts = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid, writable timespec for the duration of the call.
        let rc = unsafe { clock_gettime(clock, &mut ts) };
        (rc == 0 && ts.tv_sec >= 0 && (0..1_000_000_000).contains(&ts.tv_nsec))
            .then(|| Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
    }

    pub(super) fn thread() -> Option<Duration> {
        read(THREAD)
    }

    pub(super) fn process() -> Option<Duration> {
        read(PROCESS)
    }
}

#[cfg(not(all(
    target_pointer_width = "64",
    any(target_os = "macos", target_os = "linux")
)))]
mod cpu {
    use std::time::Duration;

    pub(super) fn thread() -> Option<Duration> {
        None
    }

    pub(super) fn process() -> Option<Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_short_and_long_durations() {
        assert_eq!(fmt_duration(Duration::from_millis(812)), "812 ms");
        assert_eq!(fmt_duration(Duration::from_millis(12_345)), "12.3 s");
        assert_eq!(summary(Duration::from_millis(5), None, "cpu"), "5 ms");
        assert_eq!(
            summary(
                Duration::from_millis(5),
                Some(Duration::from_millis(3)),
                "cpu"
            ),
            "5 ms, 3 ms cpu"
        );
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn thread_cpu_counts_busy_work() {
        let clock = JobClock::start();
        let mut x = 0u64;
        let until = Instant::now() + Duration::from_millis(30);
        while Instant::now() < until {
            x = x.wrapping_add(std::hint::black_box(1));
        }
        std::hint::black_box(x);
        let cpu = clock.thread_cpu().expect("thread clock");
        assert!(cpu >= Duration::from_millis(10), "{cpu:?}");
        assert!(clock.process_cpu().expect("process clock") >= cpu);
    }
}
