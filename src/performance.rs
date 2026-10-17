use crate::cli::{ColorMode, QualityMode};
use std::time::{Duration, Instant};

const SCALE_STEPS: [f64; 4] = [1.0, 0.9, 0.8, 0.75];
const MIN_ADAPTIVE_FPS: u32 = 30;
const WINDOW: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QualitySnapshot {
    pub fps: u32,
    pub scale: f64,
    pub indexed_color: bool,
}

pub struct QualityController {
    mode: QualityMode,
    base_fps: u32,
    min_fps: u32,
    current_fps: u32,
    can_downgrade_color: bool,
    indexed_color: bool,
    scale_step: usize,
    window_start: Instant,
    samples: u32,
    slow_samples: u32,
    render_total: Duration,
    output_total: Duration,
    overload_windows: u8,
    stable_windows: u8,
}

impl QualityController {
    pub fn new(mode: QualityMode, fps: u32, can_downgrade_color: bool) -> Self {
        let now = Instant::now();
        Self {
            mode,
            base_fps: fps,
            min_fps: fps.min(MIN_ADAPTIVE_FPS),
            current_fps: fps,
            can_downgrade_color,
            indexed_color: false,
            scale_step: 0,
            window_start: now,
            samples: 0,
            slow_samples: 0,
            render_total: Duration::ZERO,
            output_total: Duration::ZERO,
            overload_windows: 0,
            stable_windows: 0,
        }
    }

    pub fn snapshot(&self) -> QualitySnapshot {
        QualitySnapshot {
            fps: self.current_fps,
            scale: SCALE_STEPS[self.scale_step],
            indexed_color: self.indexed_color,
        }
    }

    pub fn effective_color(&self, requested: ColorMode) -> ColorMode {
        if self.indexed_color && self.can_downgrade_color && requested == ColorMode::Truecolor {
            ColorMode::Indexed
        } else {
            requested
        }
    }

    /// Returns true when the effective playback settings changed.
    pub fn record(
        &mut self,
        now: Instant,
        render: Duration,
        output: Duration,
        scheduler_lateness: Duration,
        decoder_starved: bool,
    ) -> bool {
        if self.mode == QualityMode::Fixed {
            return false;
        }
        if self.samples == 0 {
            self.window_start = now;
        }
        let budget = Duration::from_secs_f64(1.0 / self.current_fps as f64);
        self.samples += 1;
        self.render_total += render;
        self.output_total += output;
        if render + output + scheduler_lateness > budget || decoder_starved {
            self.slow_samples += 1;
        }
        if now.saturating_duration_since(self.window_start) < WINDOW {
            return false;
        }

        let slow_ratio = self.slow_samples as f64 / self.samples as f64;
        let average = (self.render_total + self.output_total).as_secs_f64() / self.samples as f64;
        self.window_start = now;
        self.samples = 0;
        self.slow_samples = 0;
        self.render_total = Duration::ZERO;
        self.output_total = Duration::ZERO;

        if slow_ratio > 0.05 {
            self.overload_windows = self.overload_windows.saturating_add(1);
            self.stable_windows = 0;
        } else {
            self.overload_windows = 0;
            if slow_ratio == 0.0 && average < budget.as_secs_f64() * 0.7 {
                self.stable_windows = self.stable_windows.saturating_add(1);
            } else {
                self.stable_windows = 0;
            }
        }

        if self.overload_windows >= 3 {
            self.overload_windows = 0;
            self.stable_windows = 0;
            self.step_down()
        } else if self.stable_windows >= 10 {
            self.stable_windows = 0;
            self.step_up()
        } else {
            false
        }
    }

    fn step_down(&mut self) -> bool {
        if self.can_downgrade_color && !self.indexed_color {
            self.indexed_color = true;
        } else if self.scale_step + 1 < SCALE_STEPS.len() {
            self.scale_step += 1;
        } else if self.current_fps > self.min_fps {
            let reduced = (self.current_fps as f64 * 0.9).floor() as u32;
            self.current_fps = reduced.max(self.min_fps).min(self.current_fps - 1);
        } else {
            return false;
        }
        true
    }

    fn step_up(&mut self) -> bool {
        if self.current_fps < self.base_fps {
            let increased = (self.current_fps as f64 * 1.1).ceil() as u32;
            self.current_fps = increased.min(self.base_fps);
        } else if self.scale_step > 0 {
            self.scale_step -= 1;
        } else if self.indexed_color {
            self.indexed_color = false;
        } else {
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(controller: &mut QualityController, now: Instant, elapsed_ms: u64) -> bool {
        controller.record(
            now,
            Duration::from_millis(elapsed_ms),
            Duration::ZERO,
            Duration::ZERO,
            false,
        )
    }

    #[test]
    fn overload_requires_three_windows_and_degrades_in_order() {
        let mut controller = QualityController::new(QualityMode::Auto, 60, true);
        let start = Instant::now();
        assert!(!tick(&mut controller, start, 20));
        let mut cursor = start;
        for window in 1..=3 {
            cursor += WINDOW;
            let changed = tick(&mut controller, cursor, 20);
            assert_eq!(changed, window == 3);
            if window < 3 {
                tick(&mut controller, cursor, 20);
            }
        }
        assert_eq!(controller.snapshot().fps, 60);
        assert_eq!(
            controller.effective_color(ColorMode::Truecolor),
            ColorMode::Indexed
        );

        // The last call at t=3s triggered the change and started the next window.
        for step in 1..=3 {
            for window in 1..=3 {
                tick(&mut controller, cursor, 20);
                cursor += WINDOW;
                let changed = tick(&mut controller, cursor, 20);
                assert_eq!(changed, window == 3);
            }
            assert_eq!(controller.snapshot().scale, SCALE_STEPS[step as usize]);
        }
        for window in 1..=3 {
            tick(&mut controller, cursor, 20);
            cursor += WINDOW;
            let changed = tick(&mut controller, cursor, 20);
            assert_eq!(changed, window == 3);
        }
        assert_eq!(controller.snapshot().fps, 54);
    }

    #[test]
    fn ten_stable_windows_restore_one_quality_step() {
        let mut controller = QualityController::new(QualityMode::Auto, 60, false);
        let start = Instant::now();
        tick(&mut controller, start, 20);
        let mut cursor = start;
        for window in 1..=3 {
            cursor += WINDOW;
            let changed = tick(&mut controller, cursor, 20);
            assert_eq!(changed, window == 3);
            if window < 3 {
                tick(&mut controller, cursor, 20);
            }
        }
        assert_eq!(controller.snapshot().scale, 0.9);

        // Start the first stable window at the moment quality was reduced.
        tick(&mut controller, cursor, 2);
        for window in 1..=10 {
            cursor += WINDOW;
            let changed = tick(&mut controller, cursor, 2);
            assert_eq!(changed, window == 10);
            if window < 10 {
                tick(&mut controller, cursor, 2);
            }
        }
        assert_eq!(controller.snapshot().scale, 1.0);
    }

    #[test]
    fn fixed_quality_never_changes_and_auto_color_respects_explicit_choice() {
        let mut fixed = QualityController::new(QualityMode::Fixed, 120, true);
        let start = Instant::now();
        for second in 1..=5 {
            assert!(!tick(&mut fixed, start + WINDOW * second, 100));
        }
        assert_eq!(fixed.snapshot().fps, 120);

        let auto = QualityController::new(QualityMode::Auto, 60, false);
        assert_eq!(
            auto.effective_color(ColorMode::Truecolor),
            ColorMode::Truecolor
        );
    }

    #[test]
    fn recovery_restores_fps_then_size_then_color() {
        let mut controller = QualityController::new(QualityMode::Auto, 60, true);
        controller.indexed_color = true;
        controller.scale_step = 3;
        controller.current_fps = 54;

        assert!(controller.step_up());
        assert_eq!(controller.snapshot().fps, 60);
        assert_eq!(controller.snapshot().scale, 0.75);
        assert!(controller.snapshot().indexed_color);
        assert!(controller.step_up());
        assert_eq!(controller.snapshot().scale, 0.8);
        assert!(controller.step_up());
        assert_eq!(controller.snapshot().scale, 0.9);
        assert!(controller.step_up());
        assert_eq!(controller.snapshot().scale, 1.0);
        assert!(controller.step_up());
        assert!(!controller.snapshot().indexed_color);
        assert!(!controller.step_up());
    }

    #[test]
    fn adaptive_frame_rate_stops_at_thirty_or_the_lower_requested_target() {
        let mut controller = QualityController::new(QualityMode::Auto, 60, false);
        controller.scale_step = 3;
        for _ in 0..20 {
            controller.step_down();
        }
        assert_eq!(controller.snapshot().fps, 30);

        let mut low_target = QualityController::new(QualityMode::Auto, 24, false);
        low_target.scale_step = 3;
        for _ in 0..20 {
            low_target.step_down();
        }
        assert_eq!(low_target.snapshot().fps, 24);
    }
}
