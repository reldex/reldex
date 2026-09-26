//! Process and machine sampling for the M5.6 fetch benchmark.
//!
//! The same instrument the M5.1/M5.2 benches use, deliberately: one
//! PowerShell `Get-Process` per sample on Windows rather than a dependency or
//! `unsafe` FFI in a test (`AGENTS.md`, "Code quality"). A sample costs a
//! process spawn, so the benchmark takes one before and one after the timed
//! section, never inside it.
//!
//! - CPU is `TotalProcessorTime` (user + kernel) in 100 ns ticks. Windows
//!   advances it at the scheduler tick, so a difference is good to about
//!   ±16 ms; runs shorter than about a second carry that error visibly.
//! - Memory is the working set and the private commit (`PrivateMemorySize64`
//!   = `PagefileUsage`), each with its lifetime peak (`PeakWorkingSet64`,
//!   `PeakPagedMemorySize64` = `PeakPagefileUsage`).
//!
//! On Linux the same fields come from `/proc/self` (resident set for both
//! memory figures); elsewhere a sample is `None`. Only the Windows figures are
//! used in the M5.6 report.

use std::process::Command;
use std::time::Duration;

/// One reading of this process's CPU time and memory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcessSample {
    /// Working set, bytes.
    pub working_set: u64,
    /// Lifetime peak working set, bytes.
    pub peak_working_set: u64,
    /// Private commit, bytes.
    pub private: u64,
    /// Lifetime peak private commit, bytes.
    pub peak_private: u64,
    /// User plus kernel CPU time.
    pub cpu: Duration,
}

/// Samples this process, or `None` where the platform offers no cheap way.
pub fn sample_self() -> Option<ProcessSample> {
    if cfg!(windows) {
        sample_windows()
    } else if cfg!(target_os = "linux") {
        sample_linux()
    } else {
        None
    }
}

fn sample_windows() -> Option<ProcessSample> {
    let script = format!(
        "$p = Get-Process -Id {}; \"$($p.WorkingSet64) $($p.PeakWorkingSet64) \
         $($p.PrivateMemorySize64) $($p.PeakPagedMemorySize64) $($p.TotalProcessorTime.Ticks)\"",
        std::process::id()
    );
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let fields: Vec<u64> = text
        .split_whitespace()
        .filter_map(|field| field.parse().ok())
        .collect();
    let [working_set, peak_working_set, private, peak_private, ticks] = fields[..] else {
        return None;
    };
    Some(ProcessSample {
        working_set,
        peak_working_set,
        private,
        peak_private,
        cpu: Duration::from_nanos(ticks.saturating_mul(100)),
    })
}

fn sample_linux() -> Option<ProcessSample> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kib = |key: &str| -> Option<u64> {
        status
            .lines()
            .find(|line| line.starts_with(key))?
            .split_whitespace()
            .nth(1)?
            .parse::<u64>()
            .ok()
            .map(|value| value * 1024)
    };
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Fields 14 and 15 (utime, stime), counted after the parenthesised name.
    let after_name = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_name.split_whitespace().collect();
    let ticks: u64 = fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
    let resident = kib("VmRSS:")?;
    let peak = kib("VmHWM:")?;
    Some(ProcessSample {
        working_set: resident,
        peak_working_set: peak,
        private: resident,
        peak_private: peak,
        // USER_HZ is 100 on every mainstream Linux configuration.
        cpu: Duration::from_millis(ticks * 10),
    })
}

/// Build and toolchain processes running now, as `name=count` pairs, so a run
/// can be annotated with whatever else was competing for the machine.
///
/// `cargo` includes the one that launched this benchmark.
pub fn machine_state() -> String {
    const WATCHED: [&str; 6] = ["cargo", "rustc", "cl", "link", "clippy-driver", "ninja"];
    let listing = if cfg!(windows) {
        Command::new("tasklist")
            .args(["/FO", "CSV", "/NH"])
            .output()
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).to_lowercase())
    } else {
        Command::new("ps")
            .args(["-eo", "comm="])
            .output()
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).to_lowercase())
    };
    let Some(listing) = listing else {
        return "unknown".to_owned();
    };
    let names: Vec<&str> = listing
        .lines()
        .map(|line| {
            let first = line.split(',').next().unwrap_or("").trim_matches('"');
            first.strip_suffix(".exe").unwrap_or(first)
        })
        .collect();
    WATCHED
        .iter()
        .map(|watched| {
            let count = names.iter().filter(|name| **name == *watched).count();
            format!("{watched}={count}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The machine's CPU load in percent, averaged over its processors, when the
/// platform can say cheaply (Windows: `Win32_Processor.LoadPercentage`).
pub fn cpu_load_percent() -> Option<u32> {
    if !cfg!(windows) {
        return None;
    }
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-CimInstance Win32_Processor | Measure-Object -Property LoadPercentage \
             -Average).Average",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim()
        .parse::<f64>()
        .ok()
        .map(|value| value.round() as u32)
}
