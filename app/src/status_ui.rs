//! Pure mapping from `dsp_core::RealtimeStatus` to a GUI label and color.
//! No `iced` types appear here on purpose, so this is trivially
//! unit-testable without a display/GPU, on any platform.

use dsp_core::RealtimeStatus;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StatusColor {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

pub fn status_label(status: RealtimeStatus) -> &'static str {
    match status {
        RealtimeStatus::RealTime => "Real-time",
        RealtimeStatus::Degraded => "Degraded",
        RealtimeStatus::Overloaded => "Overloaded",
        RealtimeStatus::Bypass => "Bypass",
    }
}

/// RealTime=green, Degraded=yellow, Overloaded=red, Bypass=grey, per spec.
pub fn status_color(status: RealtimeStatus) -> StatusColor {
    match status {
        RealtimeStatus::RealTime => StatusColor { r: 0.20, g: 0.75, b: 0.30 },
        RealtimeStatus::Degraded => StatusColor { r: 0.90, g: 0.75, b: 0.15 },
        RealtimeStatus::Overloaded => StatusColor { r: 0.85, g: 0.20, b: 0.20 },
        RealtimeStatus::Bypass => StatusColor { r: 0.55, g: 0.55, b: 0.55 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_status_has_a_distinct_label() {
        let statuses = [
            RealtimeStatus::RealTime,
            RealtimeStatus::Degraded,
            RealtimeStatus::Overloaded,
            RealtimeStatus::Bypass,
        ];
        let labels: Vec<&str> = statuses.iter().map(|&s| status_label(s)).collect();
        for i in 0..labels.len() {
            for j in (i + 1)..labels.len() {
                assert_ne!(labels[i], labels[j], "labels for distinct statuses must differ");
            }
        }
    }

    #[test]
    fn realtime_is_green_dominant() {
        let c = status_color(RealtimeStatus::RealTime);
        assert!(c.g > c.r && c.g > c.b);
    }

    #[test]
    fn degraded_is_yellow_ish() {
        let c = status_color(RealtimeStatus::Degraded);
        assert!(c.r > 0.5 && c.g > 0.5 && c.b < c.r);
    }

    #[test]
    fn overloaded_is_red_dominant() {
        let c = status_color(RealtimeStatus::Overloaded);
        assert!(c.r > c.g && c.r > c.b);
    }

    #[test]
    fn bypass_is_neutral_grey() {
        let c = status_color(RealtimeStatus::Bypass);
        assert!((c.r - c.g).abs() < 0.01 && (c.g - c.b).abs() < 0.01);
    }

    #[test]
    fn every_color_channel_is_a_valid_unit_fraction() {
        for &s in &[
            RealtimeStatus::RealTime,
            RealtimeStatus::Degraded,
            RealtimeStatus::Overloaded,
            RealtimeStatus::Bypass,
        ] {
            let c = status_color(s);
            for channel in [c.r, c.g, c.b] {
                assert!((0.0..=1.0).contains(&channel), "channel out of [0,1]: {channel}");
            }
        }
    }
}
