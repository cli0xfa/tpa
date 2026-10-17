use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender, bounded, select};
use serde::Deserialize;
use std::{
    collections::VecDeque,
    io::{BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub path: PathBuf,
    pub video_stream: usize,
    pub audio_stream: Option<usize>,
    pub duration: Option<f64>,
    pub video_start: f64,
    /// Video endpoint on the normalized media timeline, in seconds.
    pub video_end: Option<f64>,
    pub display_aspect: f64,
    pub rotation: f64,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub epoch: u64,
    pub pts: f64,
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
    /// Time spent waiting for this complete raw frame from the FFmpeg pipe.
    pub read_elapsed: Duration,
}

#[derive(Debug)]
pub enum DecodeEvent {
    Eof { epoch: u64 },
    Error { epoch: u64, message: String },
}

#[derive(Deserialize)]
struct Probe {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    format: Option<ProbeFormat>,
}
#[derive(Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
    start_time: Option<String>,
    format_name: Option<String>,
}
#[derive(Deserialize)]
struct ProbeStream {
    index: usize,
    codec_type: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    sample_aspect_ratio: Option<String>,
    display_aspect_ratio: Option<String>,
    duration: Option<String>,
    start_time: Option<String>,
    #[serde(default)]
    disposition: serde_json::Value,
    #[serde(default)]
    tags: serde_json::Value,
    #[serde(default)]
    side_data_list: Vec<serde_json::Value>,
}

pub fn hidden_command(program: &Path) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    command.stdin(Stdio::null());
    command
}

impl Tools {
    pub fn check(&self) -> Result<Vec<String>> {
        [&self.ffmpeg, &self.ffprobe]
            .into_iter()
            .map(|program| {
                let mut command = hidden_command(program);
                command.arg("-version");
                let output =
                    capture(command, Duration::from_secs(5), 64 * 1024).with_context(|| {
                        format!(
                            "无法运行 {}；请安装 FFmpeg，或用 --ffmpeg/--ffprobe 指定路径",
                            program.display()
                        )
                    })?;
                Ok(String::from_utf8_lossy(&output)
                    .lines()
                    .next()
                    .unwrap_or("未知版本")
                    .to_owned())
            })
            .collect()
    }

    pub fn probe(&self, path: &Path) -> Result<MediaInfo> {
        self.probe_cancellable(path, Arc::new(AtomicBool::new(false)))
    }

    pub fn probe_cancellable(&self, path: &Path, cancelled: Arc<AtomicBool>) -> Result<MediaInfo> {
        if !path.is_file() {
            bail!("不是本地文件：{}", path.display());
        }
        let path = path.canonicalize().context("无法访问视频文件")?;
        let mut command = hidden_command(&self.ffprobe);
        command
            .args([
                "-v",
                "error",
                "-show_streams",
                "-show_format",
                "-of",
                "json",
            ])
            .arg(&path);
        let json =
            capture_cancellable(command, Duration::from_secs(15), 4 * 1024 * 1024, cancelled)?;
        parse_probe(&path, &json)
    }
}

fn ratio(value: Option<&str>) -> Option<f64> {
    let (a, b) = value?.split_once(':')?;
    let r = a.parse::<f64>().ok()? / b.parse::<f64>().ok()?;
    (r.is_finite() && r > 0.0).then_some(r)
}

fn tag_timestamp(value: &str) -> Option<f64> {
    let mut parts = value.split(':');
    let hours: f64 = parts.next()?.parse().ok()?;
    let minutes: f64 = parts.next()?.parse().ok()?;
    let seconds: f64 = parts.next()?.parse().ok()?;
    let result = hours * 3600.0 + minutes * 60.0 + seconds;
    (parts.next().is_none() && result.is_finite() && result >= 0.0).then_some(result)
}

fn parse_probe(path: &Path, json: &[u8]) -> Result<MediaInfo> {
    let probe: Probe = serde_json::from_slice(json).context("无法读取 ffprobe 的媒体信息")?;
    let choose = |kind: &str| {
        let candidates = || {
            probe.streams.iter().filter(|s| {
                s.codec_type.as_deref() == Some(kind)
                    && s.disposition["attached_pic"].as_i64() != Some(1)
            })
        };
        candidates()
            .find(|s| s.disposition["default"].as_i64() == Some(1))
            .or_else(|| candidates().next())
    };
    let video = choose("video").context("文件没有可播放的视频轨")?;
    let width = video.width.filter(|v| *v > 0).context("视频宽度无效")?;
    let height = video.height.filter(|v| *v > 0).context("视频高度无效")?;
    let rotation = video
        .side_data_list
        .iter()
        .find_map(|s| s["rotation"].as_f64())
        .or_else(|| video.tags["rotate"].as_str()?.parse::<f64>().ok())
        .filter(|r| r.is_finite())
        .unwrap_or(0.0);
    let aspect = ratio(video.display_aspect_ratio.as_deref()).unwrap_or_else(|| {
        width as f64 / height as f64 * ratio(video.sample_aspect_ratio.as_deref()).unwrap_or(1.0)
    });
    let angle = rotation.to_radians();
    let display_aspect = (aspect * angle.cos().abs() + angle.sin().abs())
        / (aspect * angle.sin().abs() + angle.cos().abs());
    let audio = choose("audio");
    let number = |text: Option<&str>| text?.parse::<f64>().ok().filter(|v| v.is_finite());
    let origin = number(probe.format.as_ref().and_then(|f| f.start_time.as_deref())).unwrap_or(0.0);
    let video_start = (number(video.start_time.as_deref()).unwrap_or(origin) - origin).max(0.0);
    let video_end = number(video.duration.as_deref())
        .map(|length| length + number(video.start_time.as_deref()).unwrap_or(origin) - origin)
        .or_else(|| tag_timestamp(video.tags["DURATION"].as_str()?).map(|end| end - origin))
        .filter(|end| *end > 0.0);
    // Per-stream duration is a length. Matroska's Segment Duration, on the other
    // hand, is an endpoint on the segment timeline, including a nonzero origin.
    let lengths: Option<Vec<f64>> = [Some(video), audio]
        .into_iter()
        .flatten()
        .map(|s| {
            let length = number(s.duration.as_deref())?;
            Some(length + number(s.start_time.as_deref()).unwrap_or(origin) - origin)
        })
        .collect();
    let duration = lengths
        .and_then(|lengths| lengths.into_iter().reduce(f64::max))
        .or_else(|| {
            let format = probe.format.as_ref()?;
            let duration = number(format.duration.as_deref())?;
            let matroska = format
                .format_name
                .as_deref()
                .is_some_and(|name| name.split(',').any(|n| n == "matroska" || n == "webm"));
            Some(duration - if matroska { origin } else { 0.0 })
        })
        .filter(|d| *d > 0.0);
    Ok(MediaInfo {
        path: path.to_owned(),
        video_stream: video.index,
        audio_stream: audio.map(|s| s.index),
        duration,
        video_start,
        video_end,
        display_aspect,
        rotation,
        width,
        height,
    })
}

// Keep only a small tail of stderr, but drain the whole pipe so FFmpeg cannot block.
fn drain_log(reader: impl Read + Send + 'static, log: Arc<Mutex<VecDeque<u8>>>) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut chunk = [0; 1024];
        while let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                break;
            }
            if let Ok(mut tail) = log.lock() {
                tail.extend(&chunk[..n]);
                let excess = tail.len().saturating_sub(8192);
                tail.drain(..excess);
            }
        }
    })
}

pub struct Process {
    child: Arc<Mutex<Child>>,
    pub cancelled: Arc<AtomicBool>,
    cancel_tx: Option<Sender<()>>,
    pub cancel_rx: Receiver<()>,
    workers: Vec<JoinHandle<()>>,
    log: Arc<Mutex<VecDeque<u8>>>,
}

impl Process {
    pub fn spawn(mut command: Command) -> Result<(Self, impl Read + Send + 'static)> {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("无法启动媒体解码进程")?;
        let stdout = child.stdout.take().context("没有解码输出管道")?;
        let stderr = child.stderr.take().context("没有解码日志管道")?;
        let log = Arc::new(Mutex::new(VecDeque::new()));
        let log_worker = drain_log(stderr, log.clone());
        let (cancel_tx, cancel_rx) = bounded(1);
        Ok((
            Self {
                child: Arc::new(Mutex::new(child)),
                cancelled: Arc::new(AtomicBool::new(false)),
                cancel_tx: Some(cancel_tx),
                cancel_rx,
                workers: vec![log_worker],
                log,
            },
            stdout,
        ))
    }

    pub fn add_worker(&mut self, worker: JoinHandle<()>) {
        self.workers.push(worker);
    }

    /// Poll without holding the child mutex across a blocking wait (shutdown must be able to kill it).
    pub fn exit_checker(&self) -> impl Fn() -> Result<()> + Send + 'static {
        let child = self.child.clone();
        let log = self.log.clone();
        let cancelled = self.cancelled.clone();
        move || loop {
            if cancelled.load(Ordering::Acquire) {
                return Ok(());
            }
            if let Some(status) = child
                .lock()
                .map_err(|_| anyhow::anyhow!("进程锁损坏"))?
                .try_wait()?
            {
                if status.success() {
                    return Ok(());
                }
                let bytes: Vec<_> = log
                    .lock()
                    .map(|l| l.iter().copied().collect())
                    .unwrap_or_default();
                bail!(
                    "FFmpeg 解码失败 ({status})：{}",
                    String::from_utf8_lossy(&bytes)
                );
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.cancel_tx.take(); // disconnect wakes blocked sends
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

pub struct VideoDecoder {
    pub frames: Receiver<VideoFrame>,
    pub events: Receiver<DecodeEvent>,
    pub process: Process,
}

pub fn decode_command(tools: &Tools, info: &MediaInfo, offset: f64) -> Command {
    let mut command = hidden_command(&tools.ffmpeg);
    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-copyts",
        "-start_at_zero",
    ]);
    // Some elementary streams cannot seek even to zero. For unknown-duration
    // inputs, decode sequentially and trim on the shared normalized timeline.
    if offset > 0.0 && info.duration.is_some() {
        // Keep decoder preroll; fps/aresample trim it on the common timeline.
        // Input accurate-seek can discard the final frame inside its interval.
        command
            .arg("-ss")
            .arg(format!("{offset:.6}"))
            .arg("-noaccurate_seek");
    }
    command.arg("-i").arg(&info.path);
    command
}

pub fn audio_command(
    tools: &Tools,
    info: &MediaInfo,
    offset: f64,
    rate: u32,
    channels: u16,
) -> Result<Command> {
    let track = info.audio_stream.context("视频没有音轨")?;
    let mut command = decode_command(tools, info, offset);
    let trim = if info.duration.is_none() {
        format!("atrim=start={offset:.6},")
    } else {
        String::new()
    };
    command
        .arg("-map")
        .arg(format!("0:{track}"))
        .arg("-af")
        .arg(format!(
            "{trim}asetpts=PTS-{offset:.6}/TB,aresample={rate}:async=1:first_pts=0"
        ))
        .arg("-ac")
        .arg(channels.to_string())
        .arg("-ar")
        .arg(rate.to_string())
        .args([
            "-vn",
            "-sn",
            "-dn",
            "-c:a",
            "pcm_f32le",
            "-f",
            "f32le",
            "pipe:1",
        ]);
    Ok(command)
}

impl VideoDecoder {
    pub fn start(
        tools: &Tools,
        info: &MediaInfo,
        offset: f64,
        epoch: u64,
        fps: u32,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        anyhow::ensure!((1..=120).contains(&fps), "帧率必须在 1 到 120 之间");
        anyhow::ensure!(
            width > 0 && height > 0 && width as u64 * height as u64 <= 16_777_216,
            "终端画面尺寸超出支持范围"
        );
        let mut command = decode_command(tools, info, offset);
        let gap = (info.video_start - offset).max(0.0);
        let mut prefix_frames = (gap * fps as f64).round() as u64;
        let trim = if info.duration.is_none() {
            format!("trim=start={offset:.6},")
        } else {
            String::new()
        };
        command.arg("-map").arg(format!("0:{}", info.video_stream))
            .arg("-vf").arg(format!("{trim}setpts=PTS-{offset:.6}/TB,fps={fps}:start_time={gap:.9}:eof_action=pass,scale={width}:{height}:flags=bilinear,setsar=1"))
            .args(["-an", "-sn", "-dn", "-pix_fmt", "rgb24", "-f", "rawvideo", "pipe:1"]);
        let (mut process, mut stdout) = Process::spawn(command)?;
        let cancelled = process.cancelled.clone();
        let cancel = process.cancel_rx.clone();
        let check_exit = process.exit_checker();
        let (tx, frames) = bounded(6);
        let (event_tx, events) = bounded(1);
        process.add_worker(thread::spawn(move || {
            let size = width as usize * height as usize * 3;
            let mut index = 0u64;
            let outcome: Result<()> = (|| {
                loop {
                    let frame_started = Instant::now();
                    let mut rgb = vec![0; size];
                    // read_exact alone cannot distinguish clean EOF from a truncated final frame.
                    let mut filled = 0;
                    while filled < size {
                        match stdout.read(&mut rgb[filled..]) {
                            Ok(0) if filled == 0 => return check_exit(),
                            Ok(0) => bail!("视频帧输出不完整"),
                            Ok(n) => filled += n,
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(e) => return Err(e.into()),
                        }
                    }
                    if cancelled.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    let read_elapsed = frame_started.elapsed();
                    // Do not reveal a delayed video stream's first image early.
                    // Read its first real frame before padding, so codec errors
                    // are still reported immediately rather than after the gap.
                    while prefix_frames > 0 {
                        let blank = VideoFrame {
                            epoch,
                            pts: offset + index as f64 / fps as f64,
                            width,
                            height,
                            rgb: vec![0; size],
                            read_elapsed: Duration::ZERO,
                        };
                        select! {
                            recv(cancel) -> _ => return Ok(()),
                            send(tx, blank) -> result => if result.is_err() { return Ok(()); },
                        }
                        prefix_frames -= 1;
                        index += 1;
                    }
                    let frame = VideoFrame {
                        epoch,
                        pts: offset + index as f64 / fps as f64,
                        width,
                        height,
                        rgb,
                        read_elapsed,
                    };
                    select! {
                        recv(cancel) -> _ => return Ok(()),
                        send(tx, frame) -> result => if result.is_err() { return Ok(()); },
                    }
                    index += 1;
                }
            })();
            if !cancelled.load(Ordering::Acquire) {
                let event = match outcome {
                    Ok(()) => DecodeEvent::Eof { epoch },
                    Err(e) => DecodeEvent::Error {
                        epoch,
                        message: format!("{e:#}"),
                    },
                };
                let _ = event_tx.send(event);
            }
        }));
        Ok(Self {
            frames,
            events,
            process,
        })
    }
}

fn capture(command: Command, timeout: Duration, limit: usize) -> Result<Vec<u8>> {
    capture_cancellable(command, timeout, limit, Arc::new(AtomicBool::new(false)))
}

fn capture_cancellable(
    command: Command,
    timeout: Duration,
    limit: usize,
    cancelled: Arc<AtomicBool>,
) -> Result<Vec<u8>> {
    let (mut process, mut stdout) = Process::spawn(command)?;
    let (tx, rx) = bounded(1);
    process.add_worker(thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = (&mut stdout)
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = tx.send(result);
    }));
    let started = Instant::now();
    let bytes = loop {
        if cancelled.load(Ordering::Acquire) {
            bail!("已取消媒体探测");
        }
        if started.elapsed() > timeout {
            bail!("媒体工具响应超时");
        }
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(result) => break result?,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(error) => return Err(error.into()),
        }
    };
    if bytes.len() > limit {
        bail!("媒体工具输出过大");
    }
    loop {
        if let Some(status) = process
            .child
            .lock()
            .map_err(|_| anyhow::anyhow!("进程锁损坏"))?
            .try_wait()?
        {
            if status.success() {
                return Ok(bytes);
            }
            let log: Vec<_> = process
                .log
                .lock()
                .map(|l| l.iter().copied().collect())
                .unwrap_or_default();
            bail!("媒体工具运行失败：{}", String::from_utf8_lossy(&log));
        }
        if started.elapsed() > timeout {
            bail!("媒体工具响应超时");
        }
        if cancelled.load(Ordering::Acquire) {
            bail!("已取消媒体探测");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_tracks_rotation_and_sar() {
        let json = br#"{"streams":[
            {"index":0,"codec_type":"video","width":100,"height":100,"disposition":{"attached_pic":1}},
            {"index":1,"codec_type":"video","width":720,"height":576,"sample_aspect_ratio":"16:15","side_data_list":[{"rotation":90}],"disposition":{"default":1}},
            {"index":2,"codec_type":"audio"},{"index":3,"codec_type":"audio","disposition":{"default":1}}
        ],"format":{"duration":"12.5"}}"#;
        let info = parse_probe(Path::new("x.mkv"), json).unwrap();
        assert_eq!(info.video_stream, 1);
        assert_eq!(info.audio_stream, Some(3));
        assert!((info.display_aspect - 0.75).abs() < 1e-8);
        assert_eq!(info.duration, Some(12.5));
    }
    #[test]
    fn unknown_duration_and_missing_video() {
        let json = br#"{"streams":[{"index":0,"codec_type":"video","width":16,"height":9}]}"#;
        assert_eq!(parse_probe(Path::new("x"), json).unwrap().duration, None);
        assert!(parse_probe(Path::new("x"), br#"{"streams":[]}"#).is_err());
    }
}
