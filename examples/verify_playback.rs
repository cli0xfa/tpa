//! Headless playback/soak verification using the same player and renderer as the TUI.
//! cargo run --release --example verify_playback -- video.mp4 1800
use anyhow::{Context, Result, ensure};
use ratatui::{Terminal, backend::TestBackend};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use tpa::{
    cli::ColorMode,
    media::Tools,
    player::{Player, PlayerCommand, PlayerOptions, PlayerState},
    render::VideoWidget,
};

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let path = PathBuf::from(
        args.get(1)
            .context("usage: verify_playback <video> [wall_seconds] [no-audio]")?,
    );
    let seconds: f64 = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(10.0);
    let tools = Tools {
        ffmpeg: "ffmpeg".into(),
        ffprobe: "ffprobe".into(),
    };
    let info = tools.probe(&path)?;
    let mut player = Player::new(
        tools,
        info,
        PlayerOptions {
            cols: 160,
            rows: 45,
            no_audio: args.iter().any(|a| a == "no-audio"),
            ..PlayerOptions::default()
        },
    )?;
    // Exercise the real output device and audio clock while keeping the fixture silent.
    player.command(PlayerCommand::VolumeBy(-100));
    let mut terminal = Terminal::new(TestBackend::new(160, 45))?;
    let started = Instant::now();
    let mut report = Instant::now();
    let mut count = 0u64;
    let mut total_render = Duration::ZERO;
    let mut maximum_render = Duration::ZERO;
    let mut previous_pts = None;
    let mut maximum_av_error = 0.0f64;
    let period = Duration::from_secs_f64(1.0 / 30.0);
    let mut next_tick = Instant::now();
    while started.elapsed().as_secs_f64() < seconds {
        player.tick();
        ensure!(player.state != PlayerState::Error, "{:?}", player.error);
        if let Some(video) = &player.frame
            && previous_pts != Some((video.epoch, video.pts))
        {
            if player.state == PlayerState::Playing {
                maximum_av_error = maximum_av_error.max((player.position() - video.pts).abs());
            }
            let render_start = Instant::now();
            terminal.draw(|frame| {
                frame.render_widget(
                    VideoWidget {
                        frame: video,
                        mode: player.mode,
                        colors: ColorMode::Truecolor,
                    },
                    frame.area(),
                )
            })?;
            let elapsed = render_start.elapsed();
            total_render += elapsed;
            maximum_render = maximum_render.max(elapsed);
            previous_pts = Some((video.epoch, video.pts));
            count += 1;
        }
        if player.state == PlayerState::Ended {
            player.command(PlayerCommand::Restart);
        }
        if report.elapsed() >= Duration::from_secs(60) {
            println!(
                "elapsed={:.0}s position={:.3}s state={:?} frames={} dropped={} warning={:?}",
                started.elapsed().as_secs_f64(),
                player.position(),
                player.state,
                count,
                player.dropped,
                player.warning
            );
            report = Instant::now();
        }
        next_tick += period;
        let now = Instant::now();
        if now.saturating_duration_since(next_tick) > period {
            next_tick = now;
        }
        std::thread::sleep(next_tick.saturating_duration_since(now));
    }
    println!(
        "elapsed={:.3}s frames={} fps={:.2} dropped={} render_mean_ms={:.3} render_max_ms={:.3} max_frame_clock_error_ms={:.3} warning={:?}",
        started.elapsed().as_secs_f64(),
        count,
        count as f64 / started.elapsed().as_secs_f64(),
        player.dropped,
        total_render.as_secs_f64() * 1000.0 / count.max(1) as f64,
        maximum_render.as_secs_f64() * 1000.0,
        maximum_av_error * 1000.0,
        player.warning
    );
    ensure!(count > 0, "no frames rendered");
    Ok(())
}
