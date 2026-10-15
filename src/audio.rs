use crate::media::{MediaInfo, Process, Tools, audio_command};
use anyhow::{Context, Result, bail};
use cpal::{
    FromSample, SampleFormat, SizedSample, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use crossbeam_channel::{Receiver, bounded};
use rtrb::{Consumer, RingBuffer};
use std::{
    io::Read,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub fn device_description() -> Result<String> {
    let device = cpal::default_host()
        .default_output_device()
        .context("没有默认音频输出设备")?;
    let config = device.default_output_config()?;
    Ok(format!(
        "{} · {} Hz · {} 声道 · {}",
        device.id()?,
        config.sample_rate(),
        config.channels(),
        config.sample_format()
    ))
}

#[derive(Default)]
struct ClockSnapshot {
    sequence: AtomicU64,
    start_frame: AtomicU64,
    end_frame: AtomicU64,
    playback_ns: AtomicU64,
}

impl ClockSnapshot {
    fn write(&self, start: u64, end: u64, playback_ns: u64) {
        self.sequence.fetch_add(1, Ordering::AcqRel);
        self.start_frame.store(start, Ordering::Relaxed);
        self.end_frame.store(end, Ordering::Relaxed);
        self.playback_ns.store(playback_ns, Ordering::Relaxed);
        self.sequence.fetch_add(1, Ordering::Release);
    }
    fn read(&self) -> (u64, u64, u64) {
        loop {
            let before = self.sequence.load(Ordering::Acquire);
            if !before.is_multiple_of(2) {
                std::hint::spin_loop();
                continue;
            }
            let result = (
                self.start_frame.load(Ordering::Relaxed),
                self.end_frame.load(Ordering::Relaxed),
                self.playback_ns.load(Ordering::Relaxed),
            );
            // Keep the snapshot loads before the final sequence check on ARM
            // as well as x86. An acquire load alone only orders later loads.
            std::sync::atomic::fence(Ordering::Acquire);
            if before == self.sequence.load(Ordering::Relaxed) {
                return result;
            }
        }
    }
}

#[derive(Default)]
struct Shared {
    active: AtomicBool,
    volume: AtomicU32,
    muted: AtomicBool,
    underrun: AtomicBool,
    eof: AtomicBool,
    failed: AtomicBool,
    produced: AtomicU64,
    consumed: AtomicU64,
    clock: ClockSnapshot,
}

/// Interpolate only through real media samples, never through underrun silence.
fn clock_frames(start: u64, end: u64, playback_ns: u64, now_ns: u64, rate: u32) -> f64 {
    let elapsed = now_ns.saturating_sub(playback_ns) as f64 / 1e9;
    (start as f64 + elapsed * rate as f64).min(end as f64)
}

pub struct AudioOutput {
    stream: Option<Stream>,
    _process: Process,
    shared: Arc<Shared>,
    pub errors: Receiver<String>,
    baseline: Instant,
    offset: f64,
    rate: u32,
}

impl AudioOutput {
    pub fn start(
        tools: &Tools,
        info: &MediaInfo,
        offset: f64,
        volume: u8,
        muted: bool,
    ) -> Result<Self> {
        let device = cpal::default_host()
            .default_output_device()
            .context("没有音频输出设备")?;
        let supported = device
            .default_output_config()
            .context("无法读取音频设备格式")?;
        let format = supported.sample_format();
        let mut config = supported.config();
        // Request a short bounded device buffer when the backend advertises a range.
        if let cpal::SupportedBufferSize::Range { min, max } = supported.buffer_size() {
            config.buffer_size = cpal::BufferSize::Fixed(512u32.clamp(*min, *max));
        }
        let rate = config.sample_rate;
        let channels = config.channels;
        if channels == 0 || rate == 0 {
            bail!("音频设备格式无效");
        }
        let (mut producer, consumer) = RingBuffer::new(rate as usize * channels as usize / 2);
        let shared = Arc::new(Shared::default());
        shared.volume.store(volume.into(), Ordering::Relaxed);
        shared.muted.store(muted, Ordering::Relaxed);
        shared.underrun.store(true, Ordering::Relaxed);
        let baseline = Instant::now();
        let (error_tx, errors) = bounded(4);
        let (mut process, mut reader) =
            Process::spawn(audio_command(tools, info, offset, rate, channels)?)?;
        let cancelled = process.cancelled.clone();
        let state = shared.clone();
        let decode_error_tx = error_tx.clone();
        let check_exit = process.exit_checker();
        process.add_worker(thread::spawn(move || {
            let result: Result<()> = (|| {
                let mut bytes = vec![0u8; 1024 * channels as usize * 4];
                loop {
                    let mut filled = 0;
                    while filled < bytes.len() {
                        match reader.read(&mut bytes[filled..]) {
                            Ok(0) => break,
                            Ok(n) => filled += n,
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(e) => return Err(e.into()),
                        }
                    }
                    if filled % (4 * channels as usize) != 0 {
                        bail!("音频采样输出不完整");
                    }
                    for frame in bytes[..filled].chunks_exact(4 * channels as usize) {
                        while producer.slots() < channels as usize {
                            if cancelled.load(Ordering::Acquire) {
                                return Ok(());
                            }
                            thread::sleep(Duration::from_millis(2));
                        }
                        if cancelled.load(Ordering::Acquire) {
                            return Ok(());
                        }
                        for sample in frame.chunks_exact(4) {
                            let value = f32::from_le_bytes(sample.try_into().unwrap());
                            // Callback cannot consume an incomplete channel frame.
                            producer
                                .push(if value.is_finite() {
                                    value.clamp(-1.0, 1.0)
                                } else {
                                    0.0
                                })
                                .map_err(|_| anyhow::anyhow!("音频缓冲写入失败"))?;
                        }
                        state.produced.fetch_add(1, Ordering::Release);
                    }
                    if filled < bytes.len() {
                        return check_exit();
                    }
                }
            })();
            if let Err(error) = result
                && !cancelled.load(Ordering::Acquire)
            {
                state.failed.store(true, Ordering::Release);
                let _ = decode_error_tx.try_send(format!("{error:#}"));
            }
            state.eof.store(true, Ordering::Release);
        }));

        macro_rules! build {
            ($sample:ty) => {
                build_stream::<$sample>(
                    &device,
                    config,
                    consumer,
                    shared.clone(),
                    baseline,
                    error_tx,
                )?
            };
        }
        let stream = match format {
            SampleFormat::F32 => build!(f32),
            SampleFormat::F64 => build!(f64),
            SampleFormat::I8 => build!(i8),
            SampleFormat::I16 => build!(i16),
            SampleFormat::I24 => build!(cpal::I24),
            SampleFormat::I32 => build!(i32),
            SampleFormat::I64 => build!(i64),
            SampleFormat::U8 => build!(u8),
            SampleFormat::U16 => build!(u16),
            SampleFormat::U32 => build!(u32),
            SampleFormat::U64 => build!(u64),
            _ => bail!("暂不支持音频设备的 {format} 格式"),
        };
        stream.play().context("无法启动音频输出")?;
        Ok(Self {
            stream: Some(stream),
            _process: process,
            shared,
            errors,
            baseline,
            offset,
            rate,
        })
    }

    pub fn set_active(&self, active: bool) {
        self.shared.active.store(active, Ordering::Release);
    }
    pub fn set_volume(&self, volume: u8, muted: bool) {
        self.shared.volume.store(volume.into(), Ordering::Relaxed);
        self.shared.muted.store(muted, Ordering::Relaxed);
    }
    pub fn ready(&self) -> bool {
        let buffered = self
            .shared
            .produced
            .load(Ordering::Acquire)
            .saturating_sub(self.shared.consumed.load(Ordering::Acquire));
        buffered >= self.rate as u64 / 10 || self.shared.eof.load(Ordering::Acquire)
    }
    pub fn buffering(&self) -> bool {
        self.shared.underrun.load(Ordering::Acquire) && !self.ended()
    }
    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Acquire)
    }
    pub fn position(&self) -> f64 {
        let (start, end, playback_ns) = self.shared.clock.read();
        self.offset
            + clock_frames(
                start,
                end,
                playback_ns,
                self.baseline.elapsed().as_nanos() as u64,
                self.rate,
            ) / self.rate as f64
    }
    pub fn ended(&self) -> bool {
        self.shared.eof.load(Ordering::Acquire)
            && self.shared.consumed.load(Ordering::Acquire)
                >= self.shared.produced.load(Ordering::Acquire)
            && self.position() + 0.000_01
                >= self.offset
                    + self.shared.produced.load(Ordering::Acquire) as f64 / self.rate as f64
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        self.set_active(false);
        self.stream.take(); // drop callback before the decoder and its producer
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    mut consumer: Consumer<f32>,
    shared: Arc<Shared>,
    baseline: Instant,
    errors: crossbeam_channel::Sender<String>,
) -> Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let rate = config.sample_rate;
    let on_error = shared.clone();
    Ok(device.build_output_stream(
        config,
        move |output: &mut [T], info| {
            output.fill(T::from_sample(0.0));
            let start = shared.consumed.load(Ordering::Relaxed);
            let active = shared.active.load(Ordering::Acquire);
            let needs_buffer = shared.underrun.load(Ordering::Relaxed)
                && consumer.slots() < rate as usize * channels / 10
                && !shared.eof.load(Ordering::Acquire);
            let mut frames = 0u64;
            if active && !needs_buffer {
                let gain = if shared.muted.load(Ordering::Relaxed) {
                    0.0
                } else {
                    shared.volume.load(Ordering::Relaxed) as f32 / 100.0
                };
                for frame in output.chunks_exact_mut(channels) {
                    if consumer.slots() < channels {
                        break;
                    }
                    for sample in frame {
                        *sample = T::from_sample(consumer.pop().unwrap_or(0.0) * gain);
                    }
                    frames += 1;
                }
                shared
                    .underrun
                    .store(frames < (output.len() / channels) as u64, Ordering::Release);
            }
            let timestamp = info.timestamp();
            let latency = timestamp.playback.duration_since(timestamp.callback);
            let playback_ns = baseline
                .elapsed()
                .as_nanos()
                .saturating_add(latency.as_nanos())
                .min(u64::MAX as u128) as u64;
            shared.clock.write(start, start + frames, playback_ns);
            shared.consumed.store(start + frames, Ordering::Release);
        },
        move |error| {
            // Recoverable backend notifications should not disable playback.
            if !matches!(
                error.kind(),
                cpal::ErrorKind::Xrun | cpal::ErrorKind::RealtimeDenied
            ) {
                on_error.failed.store(true, Ordering::Release);
                let _ = errors.try_send(error.to_string());
            }
        },
        None,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clock_accounts_for_latency_and_freezes_on_silence() {
        assert_eq!(clock_frames(0, 480, 20_000_000, 0, 48000), 0.0);
        assert_eq!(clock_frames(0, 480, 20_000_000, 25_000_000, 48000), 240.0);
        assert_eq!(clock_frames(0, 480, 20_000_000, 100_000_000, 48000), 480.0);
        assert_eq!(
            clock_frames(480, 480, 20_000_000, 100_000_000, 48000),
            480.0
        );
    }

    #[test]
    fn concurrent_clock_reads_never_mix_callback_snapshots() {
        let clock = Arc::new(ClockSnapshot::default());
        let writer_clock = clock.clone();
        let writer = std::thread::spawn(move || {
            for frame in 1..100_000 {
                writer_clock.write(frame, frame + 100, frame * 1_000);
            }
        });
        for _ in 0..100_000 {
            let (start, end, time) = clock.read();
            if start != 0 {
                assert_eq!(end, start + 100);
                assert_eq!(time, start * 1_000);
            }
        }
        writer.join().unwrap();
    }
}
