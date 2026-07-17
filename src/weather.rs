//! Current weather condition + temperature, via wttr.in's plain-text API.
//!
//! No API key, no JSON parsing needed: `?format=%C+%t` already returns
//! exactly "<condition>  <temperature>" (e.g. "Clear  +20°C").

use std::process::Command;
use std::time::Duration;

pub struct WeatherInfo {
    pub condition: String,
    pub temperature: String,
}

pub fn status() -> Option<WeatherInfo> {
    let output = Command::new("curl")
        .args(["-s", "--max-time", "5", "https://wttr.in/?format=%C+%t"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.split_whitespace();
    let temperature = parts.next_back()?.to_string();
    let condition = parts.collect::<Vec<_>>().join(" ");
    if condition.is_empty() {
        return None;
    }
    Some(WeatherInfo { condition, temperature })
}

/// How often to re-check the weather — it doesn't change fast, and this is
/// a network call to a third-party service.
pub const POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);
