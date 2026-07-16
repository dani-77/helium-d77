//! CPU / RAM / battery / ALSA volume readers.
//!
//! No D-Bus, no external crates beyond what's already pulled in — CPU and
//! RAM come from `/proc`, battery from sysfs, volume by shelling out to
//! `amixer` (present on essentially every ALSA-enabled Linux system).

use std::fs;
use std::process::Command;

/// CPU utilization since the previous call, as a percentage.
///
/// Needs a `prev` slot the caller keeps around between calls (a fresh read
/// of `/proc/stat` alone can't tell you utilization — only the delta
/// between two reads can).
pub fn cpu_usage_percent(prev: &mut Option<(u64, u64)>) -> Option<u8> {
    let stat = fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().next()?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|f| f.parse().ok())
        .collect();
    if fields.len() < 4 {
        return None;
    }
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
    let total: u64 = fields.iter().sum();

    let result = match *prev {
        Some((prev_total, prev_idle)) => {
            let total_delta = total.saturating_sub(prev_total);
            let idle_delta = idle.saturating_sub(prev_idle);
            if total_delta == 0 {
                None
            } else {
                Some((100 * (total_delta.saturating_sub(idle_delta)) / total_delta) as u8)
            }
        }
        None => None,
    };
    *prev = Some((total, idle));
    result
}

/// RAM used, as a percentage of total.
pub fn ram_usage_percent() -> Option<u8> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = None;
    let mut available = None;
    for line in meminfo.lines() {
        if let Some(v) = line.strip_prefix("MemTotal:") {
            total = v.trim().split_whitespace().next()?.parse::<u64>().ok();
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            available = v.trim().split_whitespace().next()?.parse::<u64>().ok();
        }
    }
    let (total, available) = (total?, available?);
    if total == 0 {
        return None;
    }
    Some((100 * (total.saturating_sub(available)) / total) as u8)
}

pub struct BatteryInfo {
    pub percent: u8,
    pub charging: bool,
}

/// Reads the first `/sys/class/power_supply/*` device whose type is
/// "Battery" (name varies: BAT0, BAT1, ...).
pub fn battery() -> Option<BatteryInfo> {
    let entries = fs::read_dir("/sys/class/power_supply").ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let kind = fs::read_to_string(path.join("type")).ok()?;
        if kind.trim() != "Battery" {
            continue;
        }
        let percent: u8 = fs::read_to_string(path.join("capacity"))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let status = fs::read_to_string(path.join("status")).unwrap_or_default();
        return Some(BatteryInfo {
            percent,
            charging: status.trim() == "Charging",
        });
    }
    None
}

pub struct VolumeInfo {
    pub percent: u8,
    pub muted: bool,
}

/// Reads Master volume via `amixer` (ALSA). Parses lines like
/// `Front Left: Playback 32768 [50%] [on]`.
pub fn volume() -> Option<VolumeInfo> {
    let output = Command::new("amixer").args(["sget", "Master"]).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().find(|l| l.contains('%'))?;

    let percent: u8 = line
        .split('[')
        .nth(1)?
        .split('%')
        .next()?
        .parse()
        .ok()?;
    let muted = line.contains("[off]");
    Some(VolumeInfo { percent, muted })
}
