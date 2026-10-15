//! These tests use generated media, never user videos or an audio device.
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tpa::{
    cli::RenderMode,
    media::{DecodeEvent, Process, Tools, VideoDecoder, VideoFrame, audio_command, hidden_command},
    player::{Player, PlayerCommand, PlayerOptions, PlayerState},
};

fn tools() -> Option<Tools> {
    let tools = Tools {
        ffmpeg: std::env::var_os("TPA_FFMPEG")
            .map(PathBuf::from)
            .unwrap_or_else(|| "ffmpeg".into()),
        ffprobe: std::env::var_os("TPA_FFPROBE")
            .map(PathBuf::from)
            .unwrap_or_else(|| "ffprobe".into()),
    };
    if let Err(error) = tools.check() {
        assert!(
            std::env::var_os("TPA_REQUIRE_FFMPEG").is_none(),
            "FFmpeg required: {error:#}"
        );
        eprintln!("Skipping FFmpeg integration test: {error:#}");
        None
    } else {
        Some(tools)
    }
}

fn generate(tools: &Tools, path: &Path, delay: f64) {
    let output = hidden_command(&tools.ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i"])
        .arg(r"color=c=black:s=32x24:r=30:d=2,drawbox=color=white:t=fill:enable=between(t\,0.5\,0.6)")
        .arg("-itsoffset").arg(delay.to_string())
        .args(["-f", "lavfi", "-i"])
        .arg(r"aevalsrc=if(between(t\,0.5\,0.6)\,0.5*sin(2*PI*1000*t)\,0):s=48000:d=2")
        .args(["-c:v", "ffv1", "-c:a", "pcm_f32le"])
        .arg(path).stdout(Stdio::null()).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn frames(decoder: &VideoDecoder) -> Vec<VideoFrame> {
    let mut frames = Vec::new();
    loop {
        match decoder.frames.recv_timeout(Duration::from_secs(5)) {
            Ok(frame) => frames.push(frame),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            Err(error) => panic!("decoder timed out: {error}"),
        }
    }
    assert!(matches!(
        decoder.events.recv_timeout(Duration::from_secs(2)).unwrap(),
        DecodeEvent::Eof { .. }
    ));
    frames
}

fn audio_onset(tools: &Tools, info: &tpa::media::MediaInfo, offset: f64) -> f64 {
    let (process, mut reader) =
        Process::spawn(audio_command(tools, info, offset, 48000, 2).unwrap()).unwrap();
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    process.exit_checker()().unwrap();
    let first = bytes
        .as_chunks::<8>()
        .0
        .iter()
        .position(|frame| f32::from_le_bytes(frame[..4].try_into().unwrap()).abs() > 0.1)
        .unwrap();
    offset + first as f64 / 48000.0
}

#[test]
fn flash_beep_common_timeline_preserves_offset_and_seek() {
    let Some(tools) = tools() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("中文 空格 & $ 视频.mkv");
    generate(&tools, &path, 0.25);
    let info = tools.probe(&path).unwrap();
    verify_delayed_video(&tools, temp.path());
    assert!(info.audio_stream.is_some());
    for offset in [0.0, 0.4, 0.55] {
        let decoder = VideoDecoder::start(&tools, &info, offset, 7, 30, 16, 12).unwrap();
        let decoded = frames(&decoder);
        assert!(
            decoded
                .iter()
                .all(|f| f.epoch == 7 && f.rgb.len() == 16 * 12 * 3)
        );
        assert!((decoded[0].pts - offset).abs() < 1e-8);
        let flash = decoded.iter().find(|f| f.rgb[0] > 200).unwrap().pts;
        let beep = audio_onset(&tools, &info, offset);
        assert!(
            (flash - 0.5f64.max(offset)).abs() <= 1.0 / 30.0 + 0.001,
            "flash={flash}, offset={offset}"
        );
        assert!((beep - 0.75).abs() < 0.025, "beep={beep}, offset={offset}");
    }
}

fn verify_delayed_video(tools: &Tools, directory: &Path) {
    let path = directory.join("delayed-white.mkv");
    let output = hidden_command(&tools.ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-itsoffset",
            "0.25",
            "-i",
            "color=c=white:s=16x16:r=30:d=1",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=48000:cl=stereo",
            "-t",
            "1.25",
            "-fps_mode",
            "passthrough",
            "-c:v",
            "ffv1",
            "-c:a",
            "pcm_f32le",
        ])
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success());
    let info = tools.probe(&path).unwrap();
    assert!((info.video_start - 0.25).abs() < 0.034);
    for offset in [0.0, 0.1, 0.4] {
        let decoder = VideoDecoder::start(tools, &info, offset, 1, 30, 16, 16).unwrap();
        let decoded = frames(&decoder);
        assert!(!decoded.is_empty());
        if offset < 0.25 {
            assert_eq!(
                decoded[0].rgb[0], 0,
                "delayed first image was displayed early"
            );
        }
        let onset = decoded.iter().find(|frame| frame.rgb[0] > 200).unwrap().pts;
        assert!(
            (onset - offset.max(info.video_start)).abs() < 0.034,
            "onset={onset}, offset={offset}"
        );
    }
}

#[test]
fn synchronized_flash_and_beep_have_less_than_one_frame_error() {
    let Some(tools) = tools() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sync.mkv");
    generate(&tools, &path, 0.0);
    let info = tools.probe(&path).unwrap();
    let decoder = VideoDecoder::start(&tools, &info, 0.0, 1, 30, 16, 12).unwrap();
    let decoded = frames(&decoder);
    let flash = decoded.iter().find(|f| f.rgb[0] > 200).unwrap().pts;
    assert!((flash - audio_onset(&tools, &info, 0.0)).abs() < 1.0 / 30.0);
}

#[test]
fn bounded_queue_can_be_cancelled_without_deadlock() {
    let Some(tools) = tools() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("cancel.mkv");
    generate(&tools, &path, 0.0);
    let info = tools.probe(&path).unwrap();
    let decoder = VideoDecoder::start(&tools, &info, 0.0, 1, 30, 16, 12).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(decoder.frames.len() <= 6);
    let started = Instant::now();
    drop(decoder);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn player_pause_resize_seek_modes_and_eof() {
    let Some(tools) = tools() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("player.mkv");
    generate(&tools, &path, 0.25);
    let info = tools.probe(&path).unwrap();
    let mut player = Player::new(
        tools,
        info,
        PlayerOptions {
            no_audio: true,
            cols: 16,
            rows: 6,
            ..PlayerOptions::default()
        },
    )
    .unwrap();
    let started = Instant::now();
    while player.frame.is_none() {
        player.tick();
        assert!(player.state != PlayerState::Error, "{:?}", player.error);
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(5));
    }
    player.command(PlayerCommand::TogglePause);
    let paused = player.position();
    std::thread::sleep(Duration::from_millis(70));
    player.tick();
    assert!((player.position() - paused).abs() < 0.001);
    assert_eq!(player.state, PlayerState::Paused);
    let epoch = player.epoch;
    player.command(PlayerCommand::SetMode(RenderMode::Braille));
    assert_eq!(player.epoch, epoch);
    player.command(PlayerCommand::Resize {
        cols: 20,
        rows: 8,
        too_small: false,
    });
    assert!(player.epoch > epoch);
    for _ in 0..3 {
        player.command(PlayerCommand::SeekBy(0.5));
    }
    player.command(PlayerCommand::SeekBy(5.0));
    player.command(PlayerCommand::TogglePause);
    let deadline = Instant::now() + Duration::from_secs(6);
    while player.state != PlayerState::Ended {
        player.tick();
        assert!(player.state != PlayerState::Error, "{:?}", player.error);
        assert!(
            Instant::now() < deadline,
            "player did not end: {:?}, {}",
            player.state,
            player.position()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(player.frame.is_some());
    assert_eq!(player.frame.as_ref().unwrap().epoch, player.epoch);
    player.command(PlayerCommand::Restart);
    assert!(player.position() < 0.01);
}

#[test]
fn unsupported_and_corrupt_inputs_report_errors() {
    let Some(tools) = tools() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("broken.mp4");
    std::fs::write(&path, b"invalid media").unwrap();
    assert!(tools.probe(&path).is_err());
    assert!(tools.probe(&temp.path().join("missing.mp4")).is_err());
    let missing = Tools {
        ffmpeg: temp.path().join("missing-ffmpeg"),
        ffprobe: tools.ffprobe,
    };
    assert!(missing.check().is_err());
}

#[test]
fn common_containers_sar_rotation_and_variable_frame_rate() {
    let Some(tools) = tools() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let specs = [
        (
            "plain.mp4",
            "testsrc2=size=32x24:rate=24:duration=1",
            "mpeg4",
            vec![],
        ),
        (
            "plain.webm",
            "testsrc2=size=32x24:rate=24:duration=1",
            "libvpx",
            vec![],
        ),
        (
            "sar.mkv",
            "testsrc2=size=32x24:rate=24:duration=1,setsar=2/1",
            "ffv1",
            vec![],
        ),
        (
            "vfr.mkv",
            r"testsrc2=size=32x24:rate=30:duration=2,select=not(mod(n\,3))+gte(t\,1)",
            "ffv1",
            vec!["-fps_mode", "vfr"],
        ),
    ];
    for (name, input, codec, extra) in specs {
        let path = temp.path().join(name);
        let output = hidden_command(&tools.ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                input,
                "-c:v",
                codec,
            ])
            .args(extra)
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let info = tools.probe(&path).unwrap();
        assert!(info.audio_stream.is_none());
        if name == "sar.mkv" {
            assert!((info.display_aspect - 8.0 / 3.0).abs() < 0.01);
        }
        let decoder = VideoDecoder::start(&tools, &info, 0.0, 1, 30, 16, 12).unwrap();
        let decoded = frames(&decoder);
        assert!(!decoded.is_empty());
        assert!(
            decoded
                .windows(2)
                .all(|p| ((p[1].pts - p[0].pts) - 1.0 / 30.0).abs() < 1e-8)
        );
        if name == "vfr.mkv" {
            assert!(decoded.len() >= 59);
        }
    }
    let rotated = temp.path().join("rotated.mp4");
    let output = hidden_command(&tools.ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-display_rotation:v:0",
            "90",
            "-i",
        ])
        .arg(temp.path().join("plain.mp4"))
        .args(["-c", "copy"])
        .arg(&rotated)
        .output()
        .unwrap();
    assert!(output.status.success());
    let info = tools.probe(&rotated).unwrap();
    assert!((info.display_aspect - 0.75).abs() < 0.01);
    let decoder = VideoDecoder::start(&tools, &info, 0.0, 1, 30, 12, 16).unwrap();
    assert!(!frames(&decoder).is_empty());
}

#[test]
fn nonzero_container_origin_normalizes_both_tracks() {
    let Some(tools) = tools() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source.mkv");
    generate(&tools, &source, 0.25);
    let path = temp.path().join("shifted.mkv");
    let output = hidden_command(&tools.ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(&source)
        .args(["-c", "copy", "-output_ts_offset", "5"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success());
    let info = tools.probe(&path).unwrap();
    let decoder = VideoDecoder::start(&tools, &info, 0.4, 1, 30, 16, 12).unwrap();
    assert!(
        info.duration
            .is_some_and(|duration| (2.0..3.0).contains(&duration)),
        "{:?}",
        info.duration
    );
    let decoded = frames(&decoder);
    let flash = decoded.iter().find(|f| f.rgb[0] > 200).unwrap().pts;
    assert!((flash - 0.5).abs() < 0.034);
    assert!((audio_onset(&tools, &info, 0.4) - 0.75).abs() < 0.025);
}

#[test]
#[ignore = "requires a real audio output device; run explicitly on desktop"]
fn actual_audio_device_pause_seek_and_clock() {
    let tools = tools().expect("FFmpeg required");
    tpa::audio::device_description().expect("real audio device required");
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("device.mkv");
    generate(&tools, &path, 0.25);
    let info = tools.probe(&path).unwrap();
    let mut player = Player::new(
        tools,
        info,
        PlayerOptions {
            cols: 16,
            rows: 6,
            ..PlayerOptions::default()
        },
    )
    .unwrap();
    player.command(PlayerCommand::VolumeBy(-100));
    let deadline = Instant::now() + Duration::from_secs(5);
    while player.position() < 0.4 {
        player.tick();
        assert!(player.warning.is_none(), "{:?}", player.warning);
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    player.command(PlayerCommand::TogglePause);
    let position = player.position();
    std::thread::sleep(Duration::from_millis(100));
    player.tick();
    assert!((position - player.position()).abs() < 0.001);
    player.command(PlayerCommand::SeekBy(0.5));
    player.command(PlayerCommand::TogglePause);
    let deadline = Instant::now() + Duration::from_secs(5);
    while player.state != PlayerState::Ended {
        player.tick();
        assert!(player.warning.is_none(), "{:?}", player.warning);
        assert!(Instant::now() < deadline);
        // This fixture's audio ends 250 ms after its 2 s video. Holding the
        // final video frame during that audio tail is expected, not drift.
        if player.state == PlayerState::Playing
            && player.position() < 2.0
            && let Some(frame) = &player.frame
        {
            assert!((player.position() - frame.pts).abs() < 0.1);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        player.position() >= 2.2,
        "audio tail was lost from final progress: {}",
        player.position()
    );
}

#[test]
fn short_clip_keeps_one_frame_at_low_fps_and_raw_stream_can_seek() {
    let Some(tools) = tools() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let short = temp.path().join("short.mkv");
    let output = hidden_command(&tools.ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=red:s=16x16:r=30:d=0.1",
            "-c:v",
            "ffv1",
        ])
        .arg(&short)
        .output()
        .unwrap();
    assert!(output.status.success());
    let info = tools.probe(&short).unwrap();
    let decoder = VideoDecoder::start(&tools, &info, 0.0, 1, 1, 16, 16).unwrap();
    assert_eq!(frames(&decoder).len(), 1);
    // A seek inside a final source frame must retain it as decoder preroll.
    let slow = temp.path().join("slow.mkv");
    let output = hidden_command(&tools.ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=red:s=16x16:r=1:d=2",
            "-c:v",
            "ffv1",
        ])
        .arg(&slow)
        .output()
        .unwrap();
    assert!(output.status.success());
    let slow_info = tools.probe(&slow).unwrap();
    let decoder = VideoDecoder::start(&tools, &slow_info, 1.95, 1, 30, 16, 16).unwrap();
    assert!(!frames(&decoder).is_empty());
    let raw = temp.path().join("unknown-duration.h264");
    let output = hidden_command(&tools.ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=32x24:rate=30:duration=1",
            "-c:v",
            "libx264",
            "-f",
            "h264",
        ])
        .arg(&raw)
        .output()
        .unwrap();
    assert!(output.status.success());
    let info = tools.probe(&raw).unwrap();
    assert!(info.duration.is_none());
    for offset in [0.0, 0.4] {
        let decoder = VideoDecoder::start(&tools, &info, offset, 1, 30, 16, 12).unwrap();
        let decoded = frames(&decoder);
        assert!(!decoded.is_empty());
        assert_eq!(decoded[0].pts, offset);
    }
    let mut player = Player::new(
        tools,
        info,
        PlayerOptions {
            no_audio: true,
            cols: 16,
            rows: 6,
            ..PlayerOptions::default()
        },
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while player.state != PlayerState::Ended {
        player.tick();
        assert!(player.state != PlayerState::Error, "{:?}", player.error);
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        player.info.duration.is_none(),
        "discovered duration must not change the demuxer's seeking strategy"
    );
    player.command(PlayerCommand::SeekBy(-0.4));
    let deadline = Instant::now() + Duration::from_secs(5);
    while player.frame.is_none() {
        player.tick();
        assert!(player.state != PlayerState::Error, "{:?}", player.error);
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
}
