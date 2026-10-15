use crate::{
    audio::AudioOutput,
    cli::RenderMode,
    media::{DecodeEvent, MediaInfo, Tools, VideoDecoder, VideoFrame},
};
use anyhow::Result;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerState {
    Loading,
    Playing,
    Paused,
    Buffering,
    Ended,
    Error,
}

impl PlayerState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Loading => "加载中",
            Self::Playing => "播放",
            Self::Paused => "暂停",
            Self::Buffering => "缓冲中",
            Self::Ended => "已结束",
            Self::Error => "错误",
        }
    }
}

#[derive(Debug)]
pub enum PlayerCommand {
    TogglePause,
    SeekBy(f64),
    Restart,
    VolumeBy(i16),
    ToggleMute,
    SetMode(RenderMode),
    Resize {
        cols: u16,
        rows: u16,
        too_small: bool,
    },
}

#[derive(Debug)]
pub enum PlayerEvent {
    StateChanged(PlayerState),
    Warning(String),
    Error(String),
}

#[derive(Clone, Debug)]
pub struct PlayerOptions {
    pub fps: u32,
    pub mode: RenderMode,
    pub no_audio: bool,
    pub cols: u16,
    pub rows: u16,
    pub too_small: bool,
}

impl Default for PlayerOptions {
    fn default() -> Self {
        Self {
            fps: 30,
            mode: RenderMode::Half,
            no_audio: false,
            cols: 80,
            rows: 24,
            too_small: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct WallClock {
    position: f64,
    anchor: Instant,
    running: bool,
}
impl WallClock {
    pub fn new(position: f64) -> Self {
        Self {
            position,
            anchor: Instant::now(),
            running: false,
        }
    }
    pub fn position(&self) -> f64 {
        self.at(Instant::now())
    }
    fn at(&self, now: Instant) -> f64 {
        self.position
            + if self.running {
                now.saturating_duration_since(self.anchor).as_secs_f64()
            } else {
                0.0
            }
    }
    pub fn set_running(&mut self, running: bool) {
        self.set_running_at(running, Instant::now());
    }
    fn set_running_at(&mut self, running: bool, now: Instant) {
        self.position = self.at(now);
        self.anchor = now;
        self.running = running;
    }
}

pub struct Player {
    pub info: MediaInfo,
    pub state: PlayerState,
    pub frame: Option<VideoFrame>,
    pub mode: RenderMode,
    pub volume: u8,
    pub muted: bool,
    pub dropped: u64,
    pub warning: Option<String>,
    pub error: Option<String>,
    pub epoch: u64,
    tools: Tools,
    fps: u32,
    no_audio: bool,
    audio: Option<AudioOutput>,
    video: Option<VideoDecoder>,
    next_frame: Option<VideoFrame>,
    video_eof: bool,
    wall: WallClock,
    user_paused: bool,
    too_small: bool,
    ready: bool,
    cols: u16,
    rows: u16,
    paused_position: Option<f64>,
    display_end: f64,
}

impl Player {
    pub fn new(tools: Tools, info: MediaInfo, options: PlayerOptions) -> Result<Self> {
        let PlayerOptions {
            fps,
            mode,
            no_audio,
            cols,
            rows,
            too_small,
        } = options;
        let mut player = Self {
            info,
            tools,
            fps,
            mode,
            no_audio,
            cols,
            rows,
            too_small,
            state: PlayerState::Loading,
            frame: None,
            next_frame: None,
            video: None,
            audio: None,
            volume: 100,
            muted: false,
            dropped: 0,
            warning: None,
            error: None,
            epoch: 0,
            video_eof: false,
            wall: WallClock::new(0.0),
            user_paused: false,
            ready: false,
            paused_position: None,
            display_end: 0.0,
        };
        player.start_at(0.0)?;
        Ok(player)
    }

    fn start_at(&mut self, offset: f64) -> Result<()> {
        self.audio.take();
        self.video.take();
        self.epoch += 1;
        self.next_frame = None;
        self.frame = None;
        self.video_eof = false;
        self.ready = false;
        self.display_end = offset;
        self.error = None;
        self.wall = WallClock::new(offset);
        self.paused_position = Some(offset);
        self.state = PlayerState::Loading;
        self.video = Some(VideoDecoder::start(
            &self.tools,
            &self.info,
            offset,
            self.epoch,
            self.fps,
            self.cols.max(1) as u32 * 2,
            self.rows.max(1) as u32 * 4,
        )?);
        if !self.no_audio && self.info.audio_stream.is_some() {
            match AudioOutput::start(&self.tools, &self.info, offset, self.volume, self.muted) {
                Ok(output) => self.audio = Some(output),
                Err(error) => self.warning = Some(format!("声音不可用，继续无声播放：{error:#}")),
            }
        }
        Ok(())
    }

    pub fn position(&self) -> f64 {
        if self.state == PlayerState::Ended {
            return self.display_end;
        }
        self.paused_position.unwrap_or_else(|| {
            self.audio
                .as_ref()
                .map_or_else(|| self.wall.position(), AudioOutput::position)
        })
    }

    fn pause_outputs(&mut self, paused: bool) {
        if paused {
            let position = self.position();
            self.paused_position = Some(position);
        } else {
            self.paused_position = None;
        }
        if let Some(audio) = &self.audio {
            audio.set_active(!paused);
        }
        self.wall.set_running(!paused);
    }

    fn fail(&mut self, message: String, events: &mut Vec<PlayerEvent>) {
        self.pause_outputs(true);
        self.audio.take();
        self.video.take();
        self.state = PlayerState::Error;
        self.error = Some(message.clone());
        events.push(PlayerEvent::Error(message));
    }

    pub fn command(&mut self, command: PlayerCommand) -> Vec<PlayerEvent> {
        let mut events = Vec::new();
        let result: Result<()> = (|| {
            match command {
                PlayerCommand::TogglePause
                    if !matches!(self.state, PlayerState::Ended | PlayerState::Error) =>
                {
                    self.user_paused = !self.user_paused;
                    if self.ready {
                        self.pause_outputs(self.user_paused || self.too_small);
                    }
                }
                PlayerCommand::TogglePause => {}
                PlayerCommand::SeekBy(delta) => {
                    let upper = self
                        .info
                        .video_end
                        .or(self.info.duration)
                        .or_else(|| (self.state == PlayerState::Ended).then_some(self.display_end))
                        .map(|d| (d - 1.0 / self.fps as f64).max(0.0))
                        .unwrap_or(f64::MAX);
                    self.start_at((self.position() + delta).clamp(0.0, upper))?;
                }
                PlayerCommand::Restart => {
                    self.user_paused = false;
                    self.start_at(0.0)?;
                }
                PlayerCommand::VolumeBy(delta) => {
                    self.volume = (self.volume as i16 + delta).clamp(0, 100) as u8
                }
                PlayerCommand::ToggleMute => self.muted = !self.muted,
                PlayerCommand::SetMode(mode) => self.mode = mode,
                PlayerCommand::Resize {
                    cols,
                    rows,
                    too_small,
                } => {
                    let changed = self.cols != cols || self.rows != rows;
                    self.cols = cols;
                    self.rows = rows;
                    self.too_small = too_small;
                    if self.ready {
                        self.pause_outputs(
                            self.user_paused
                                || self.too_small
                                || matches!(self.state, PlayerState::Ended | PlayerState::Error),
                        );
                    }
                    if changed
                        && !too_small
                        && !matches!(self.state, PlayerState::Ended | PlayerState::Error)
                    {
                        let offset = self.position();
                        self.video.take();
                        self.epoch += 1;
                        self.next_frame = None;
                        self.video_eof = false;
                        self.video = Some(VideoDecoder::start(
                            &self.tools,
                            &self.info,
                            offset,
                            self.epoch,
                            self.fps,
                            cols.max(1) as u32 * 2,
                            rows.max(1) as u32 * 4,
                        )?);
                    }
                }
            }
            if let Some(audio) = &self.audio {
                audio.set_volume(self.volume, self.muted);
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.fail(format!("{error:#}"), &mut events);
        }
        events
    }

    pub fn tick(&mut self) -> Vec<PlayerEvent> {
        let mut events = Vec::new();
        if matches!(self.state, PlayerState::Ended | PlayerState::Error) {
            return events;
        }
        let old_state = self.state;
        if let Some(video) = &self.video {
            for event in video.events.try_iter() {
                match event {
                    DecodeEvent::Eof { epoch } if epoch == self.epoch => self.video_eof = true,
                    DecodeEvent::Error { epoch, message } if epoch == self.epoch => {
                        self.fail(message, &mut events);
                        return events;
                    }
                    _ => {}
                }
            }
        }
        let mut audio_finished = false;
        if let Some(audio) = &self.audio {
            for message in audio.errors.try_iter() {
                self.warning = Some(format!("声音不可用，继续无声播放：{message}"));
                events.push(PlayerEvent::Warning(self.warning.clone().unwrap()));
            }
            audio_finished = audio.failed() || audio.ended();
            if audio.failed() && self.warning.is_none() {
                // The backend error flag can become visible before its message
                // crosses the channel. Always explain a switch to silent mode.
                let message = "声音输出已中断，继续无声播放".to_owned();
                self.warning = Some(message.clone());
                events.push(PlayerEvent::Warning(message));
            }
        }
        if audio_finished {
            let position = self.position();
            self.audio.take();
            self.wall = WallClock::new(position);
            self.wall
                .set_running(self.ready && !self.user_paused && !self.too_small);
        }
        if self.next_frame.is_none() {
            self.next_frame = self.video.as_ref().and_then(|v| v.frames.try_recv().ok());
        }
        if !self.ready {
            if self.next_frame.is_some() && self.audio.as_ref().is_none_or(AudioOutput::ready) {
                self.ready = true;
                self.pause_outputs(self.user_paused || self.too_small);
            } else if self.video_eof && self.next_frame.is_none() {
                self.fail("视频轨没有可解码的帧".to_owned(), &mut events);
                return events;
            } else {
                return events;
            }
        }
        let position = self.position();
        let mut shown = 0u64;
        while self
            .next_frame
            .as_ref()
            .is_some_and(|f| f.pts <= position + 0.000_01)
        {
            let frame = self.next_frame.take().unwrap();
            if frame.epoch == self.epoch {
                self.display_end = frame.pts + 1.0 / self.fps as f64;
                self.frame = Some(frame);
                shown += 1;
            }
            self.next_frame = self.video.as_ref().and_then(|v| v.frames.try_recv().ok());
        }
        self.dropped += shown.saturating_sub(1);
        self.state = if self.user_paused || self.too_small {
            PlayerState::Paused
        } else if self.audio.as_ref().is_some_and(AudioOutput::buffering)
            || self.frame.is_none()
            || (self.next_frame.is_none() && !self.video_eof && position > self.display_end + 0.1)
        {
            PlayerState::Buffering
        } else {
            PlayerState::Playing
        };
        if self.video_eof
            && self.next_frame.is_none()
            && self.audio.is_none()
            && position >= self.display_end
        {
            self.pause_outputs(true);
            self.display_end = self.display_end.max(position);
            self.state = PlayerState::Ended;
            self.video.take();
        }
        if self.state != old_state {
            events.push(PlayerEvent::StateChanged(self.state));
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn monotonic_clock_pause_resume() {
        let start = Instant::now();
        let mut clock = WallClock {
            position: 5.0,
            anchor: start,
            running: true,
        };
        clock.set_running_at(false, start + Duration::from_secs(2));
        assert_eq!(clock.at(start + Duration::from_secs(10)), 7.0);
        clock.set_running_at(true, start + Duration::from_secs(10));
        assert_eq!(clock.at(start + Duration::from_secs(11)), 8.0);
    }
}
