use crate::{
    browser::Browser,
    cli::{Cli, ColorMode, QualityMode, RenderMode},
    media::{MediaInfo, Tools},
    performance::{QualityController, QualitySnapshot},
    player::{Player, PlayerCommand, PlayerOptions, PlayerState},
    render::{VideoWidget, fit, resolve_color, scale_rect},
};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, bounded};
use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap},
};
use std::{
    io::{self, BufWriter, IsTerminal, Stdout},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

type Tui = Terminal<CrosstermBackend<BufWriter<Stdout>>>;

pub fn restore_terminal() {
    let _ = terminal::disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        crossterm::style::ResetColor,
        Show,
        LeaveAlternateScreen
    );
}

struct TerminalSession {
    terminal: Tui,
}
impl TerminalSession {
    fn new() -> Result<Self> {
        anyhow::ensure!(
            io::stdin().is_terminal() && io::stdout().is_terminal(),
            "播放需要交互式终端；请在终端中运行 tpa（诊断可用 tpa doctor）"
        );
        terminal::enable_raw_mode().context("无法启用终端原始输入模式")?;
        let result = (|| {
            let mut stdout = io::stdout();
            execute!(stdout, EnterAlternateScreen, Hide)?;
            let mut terminal = Terminal::new(CrosstermBackend::new(BufWriter::with_capacity(
                1024 * 1024,
                stdout,
            )))?;
            terminal.clear()?;
            Ok(Self { terminal })
        })();
        if result.is_err() {
            restore_terminal();
        }
        result
    }
}
impl Drop for TerminalSession {
    fn drop(&mut self) {
        restore_terminal();
    }
}

struct ProbeJob {
    receiver: Receiver<Result<MediaInfo>>,
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    name: String,
}
impl ProbeJob {
    fn start(tools: Tools, path: PathBuf) -> Self {
        let (tx, receiver) = bounded(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let token = cancelled.clone();
        let name = path.display().to_string();
        let worker = thread::spawn(move || {
            let _ = tx.send(tools.probe_cancellable(&path, token));
        });
        Self {
            receiver,
            cancelled,
            worker: Some(worker),
            name,
        }
    }
}
impl Drop for ProbeJob {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub struct App {
    cli: Cli,
    tools: Tools,
    browser: Browser,
    pub player: Option<Player>,
    job: Option<ProbeJob>,
    colors: ColorMode,
    base_colors: ColorMode,
    quality: QualityController,
    render_ms: f64,
    output_ms: f64,
    scheduler_late_ms: f64,
    decode_read_ms: f64,
    metrics_anchor: Instant,
    render_total: Duration,
    output_total: Duration,
    scheduler_late_total: Duration,
    metrics_samples: u64,
    help: bool,
    settings_menu: bool,
    settings_selection: usize,
    hidden_controls: bool,
    pending_resize: Option<Instant>,
    area: Rect,
    ticks: u64,
    fps_anchor: Instant,
    refresh_rate: f64,
    quit: bool,
}

impl App {
    pub fn new(cli: Cli, directory: PathBuf, initial: Option<PathBuf>) -> Result<Self> {
        let tools = Tools {
            ffmpeg: cli.ffmpeg.clone(),
            ffprobe: cli.ffprobe.clone(),
        };
        let colors = resolve_color(cli.color);
        let quality = QualityController::new(
            cli.quality,
            cli.target_fps(),
            cli.color == ColorMode::Auto && colors == ColorMode::Truecolor,
        );
        let mut app = Self {
            cli,
            tools,
            browser: Browser::new(directory)?,
            player: None,
            job: None,
            colors,
            base_colors: colors,
            quality,
            render_ms: 0.0,
            output_ms: 0.0,
            scheduler_late_ms: 0.0,
            decode_read_ms: 0.0,
            metrics_anchor: Instant::now(),
            render_total: Duration::ZERO,
            output_total: Duration::ZERO,
            scheduler_late_total: Duration::ZERO,
            metrics_samples: 0,
            help: false,
            settings_menu: false,
            settings_selection: 0,
            hidden_controls: false,
            pending_resize: None,
            area: Rect::default(),
            ticks: 0,
            fps_anchor: Instant::now(),
            refresh_rate: 0.0,
            quit: false,
        };
        if let Some(path) = initial {
            app.open(path);
        }
        Ok(app)
    }

    fn open(&mut self, path: PathBuf) {
        self.player.take();
        self.job.take();
        self.reset_quality();
        self.help = false;
        self.settings_menu = false;
        self.browser.message = None;
        self.job = Some(ProbeJob::start(self.tools.clone(), path));
    }

    fn reset_quality(&mut self) {
        self.quality = QualityController::new(
            self.cli.quality,
            self.cli.target_fps(),
            self.cli.color == ColorMode::Auto && self.base_colors == ColorMode::Truecolor,
        );
        self.colors = self.base_colors;
        self.render_ms = 0.0;
        self.output_ms = 0.0;
        self.scheduler_late_ms = 0.0;
        self.decode_read_ms = 0.0;
        self.metrics_anchor = Instant::now();
        self.render_total = Duration::ZERO;
        self.output_total = Duration::ZERO;
        self.scheduler_late_total = Duration::ZERO;
        self.metrics_samples = 0;
    }

    fn adjust_target_fps(&mut self, delta: i32) {
        let fps = (self.cli.target_fps() as i32 + delta).clamp(1, 120) as u32;
        self.cli.fps = Some(fps);
        self.reset_quality();
        self.apply_quality_settings();
    }

    fn set_quality_mode(&mut self, mode: QualityMode) {
        if self.cli.quality == mode {
            return;
        }
        self.cli.quality = mode;
        self.reset_quality();
        self.apply_quality_settings();
    }

    fn apply_quality_settings(&mut self) {
        self.colors = self.quality.effective_color(self.base_colors);
        let QualitySnapshot { fps, scale, .. } = self.quality.snapshot();
        if let Some(player) = &mut self.player {
            player.command(PlayerCommand::SetPerformance { fps, scale });
        }
    }

    fn handle_settings_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc | KeyCode::Char('s') => self.settings_menu = false,
            KeyCode::Up => self.settings_selection = self.settings_selection.saturating_sub(1),
            KeyCode::Down => self.settings_selection = (self.settings_selection + 1).min(1),
            KeyCode::Left | KeyCode::Char('-') => {
                if self.settings_selection == 0 {
                    self.adjust_target_fps(-5);
                } else {
                    self.toggle_quality_mode();
                }
            }
            KeyCode::Right | KeyCode::Char('+') | KeyCode::Char('=') => {
                if self.settings_selection == 0 {
                    self.adjust_target_fps(5);
                } else {
                    self.toggle_quality_mode();
                }
            }
            KeyCode::Enter if self.settings_selection == 1 => self.toggle_quality_mode(),
            _ => {}
        }
    }

    fn toggle_quality_mode(&mut self) {
        self.set_quality_mode(match self.cli.quality {
            QualityMode::Auto => QualityMode::Fixed,
            QualityMode::Fixed => QualityMode::Auto,
        });
    }

    fn record_frame_times(
        &mut self,
        render: Duration,
        output: Duration,
        scheduler_lateness: Duration,
        now: Instant,
    ) {
        self.render_total += render;
        self.output_total += output;
        self.scheduler_late_total += scheduler_lateness;
        self.metrics_samples += 1;
        if now.saturating_duration_since(self.metrics_anchor) >= Duration::from_secs(1) {
            let samples = self.metrics_samples.max(1) as f64;
            self.render_ms = self.render_total.as_secs_f64() * 1000.0 / samples;
            self.output_ms = self.output_total.as_secs_f64() * 1000.0 / samples;
            self.scheduler_late_ms = self.scheduler_late_total.as_secs_f64() * 1000.0 / samples;
            self.decode_read_ms = self
                .player
                .as_ref()
                .map_or(0.0, |player| player.decode_read_ms);
            self.metrics_anchor = now;
            self.render_total = Duration::ZERO;
            self.output_total = Duration::ZERO;
            self.scheduler_late_total = Duration::ZERO;
            self.metrics_samples = 0;
        }

        let active = self.player.as_ref().is_some_and(|player| {
            matches!(player.state, PlayerState::Playing | PlayerState::Buffering)
        });
        if !active {
            return;
        }
        let decoder_starved = self
            .player
            .as_ref()
            .is_some_and(|player| player.decoder_starved);
        if self
            .quality
            .record(now, render, output, scheduler_lateness, decoder_starved)
        {
            self.colors = self.quality.effective_color(self.base_colors);
            let QualitySnapshot { fps, scale, .. } = self.quality.snapshot();
            if let Some(player) = &mut self.player {
                player.command(PlayerCommand::SetPerformance { fps, scale });
            }
        }
    }

    pub fn tick(&mut self) {
        if let Some(result) = self
            .job
            .as_ref()
            .and_then(|job| job.receiver.try_recv().ok())
        {
            self.job.take();
            match result {
                Ok(info) => {
                    let viewport = fit(
                        video_area(self.area, self.hidden_controls),
                        info.display_aspect,
                        self.cli.cell_aspect,
                    );
                    match Player::new(
                        self.tools.clone(),
                        info,
                        PlayerOptions {
                            fps: self.quality.snapshot().fps,
                            mode: self.cli.mode,
                            no_audio: self.cli.no_audio,
                            cols: viewport.width,
                            rows: viewport.height,
                            too_small: too_small(self.area),
                        },
                    ) {
                        Ok(player) => self.player = Some(player),
                        Err(error) => self.browser.message = Some(format!("打开失败：{error:#}")),
                    }
                }
                Err(error) => self.browser.message = Some(format!("打开失败：{error:#}")),
            }
        }
        if self
            .pending_resize
            .is_some_and(|t| t.elapsed() >= Duration::from_millis(150))
        {
            self.pending_resize = None;
            if let Some(player) = &mut self.player {
                let viewport = fit(
                    video_area(self.area, self.hidden_controls),
                    player.info.display_aspect,
                    self.cli.cell_aspect,
                );
                player.command(PlayerCommand::Resize {
                    cols: viewport.width,
                    rows: viewport.height,
                    too_small: too_small(self.area),
                });
            }
        }
        if let Some(player) = &mut self.player {
            player.tick();
        }
    }

    pub fn key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        if self.browser.filtering && self.player.is_none() && self.job.is_none() {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.browser.filtering = false,
                KeyCode::Backspace => {
                    self.browser.filter.pop();
                    self.browser.selected = 0;
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.browser.filter.push(c);
                    self.browser.selected = 0;
                }
                _ => {}
            }
            return;
        }
        if key.code == KeyCode::Char('q') {
            self.quit = true;
            return;
        }
        if self.settings_menu {
            self.handle_settings_key(key.code);
            return;
        }
        if key.code == KeyCode::Char('?') {
            self.help = !self.help;
            return;
        }
        if self.help {
            if key.code == KeyCode::Esc {
                self.help = false;
            }
            return;
        }
        if self.job.is_some() {
            if key.code == KeyCode::Esc {
                self.job.take();
            }
            return;
        }
        if let Some(player) = &mut self.player {
            let command = match key.code {
                KeyCode::Char(' ') => Some(PlayerCommand::TogglePause),
                KeyCode::Left => Some(PlayerCommand::SeekBy(-5.0)),
                KeyCode::Right => Some(PlayerCommand::SeekBy(5.0)),
                KeyCode::Up => Some(PlayerCommand::VolumeBy(5)),
                KeyCode::Down => Some(PlayerCommand::VolumeBy(-5)),
                KeyCode::Char('m') => Some(PlayerCommand::ToggleMute),
                KeyCode::Char('1') => Some(PlayerCommand::SetMode(RenderMode::Half)),
                KeyCode::Char('2') => Some(PlayerCommand::SetMode(RenderMode::Braille)),
                KeyCode::Char('3') => Some(PlayerCommand::SetMode(RenderMode::Ascii)),
                KeyCode::Enter
                    if matches!(player.state, PlayerState::Ended | PlayerState::Error) =>
                {
                    Some(PlayerCommand::Restart)
                }
                KeyCode::Char('f') => {
                    self.hidden_controls = !self.hidden_controls;
                    self.pending_resize = Some(Instant::now());
                    None
                }
                KeyCode::Char('s') => {
                    self.settings_menu = true;
                    self.settings_selection = 0;
                    None
                }
                KeyCode::Esc => {
                    self.cli.mode = player.mode;
                    self.player.take();
                    self.hidden_controls = false;
                    None
                }
                _ => None,
            };
            if let Some(command) = command
                && let Some(player) = &mut self.player
            {
                player.command(command);
            }
        } else {
            match key.code {
                KeyCode::Up => self.browser.move_selection(-1),
                KeyCode::Down => self.browser.move_selection(1),
                KeyCode::Backspace => self.browser.parent(),
                KeyCode::Char('/') => self.browser.filtering = true,
                KeyCode::Char('s') => {
                    self.settings_menu = true;
                    self.settings_selection = 0;
                }
                KeyCode::Char('r') => {
                    if let Err(error) = self.browser.reload() {
                        self.browser.message = Some(format!("{error:#}"));
                    }
                }
                KeyCode::Esc => {
                    self.browser.filter.clear();
                    self.browser.message = None;
                    self.browser.selected = 0;
                }
                KeyCode::Enter => {
                    let selected = self
                        .browser
                        .visible()
                        .get(self.browser.selected)
                        .map(|e| (e.path.clone(), e.directory));
                    if let Some((path, directory)) = selected {
                        if directory {
                            self.browser.enter(path);
                        } else {
                            self.open(path);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if self.area != area {
            self.area = area;
            self.pending_resize = Some(Instant::now());
            // Pause immediately when too small, even before resize debounce expires.
            if too_small(area)
                && let Some(player) = &mut self.player
            {
                player.command(PlayerCommand::Resize {
                    cols: 1,
                    rows: 1,
                    too_small: true,
                });
            }
        }
        frame.render_widget(
            Block::default().style(Style::default().bg(Color::Black).fg(Color::White)),
            area,
        );
        if too_small(area) {
            frame.render_widget(
                Paragraph::new("终端窗口过小\n请扩大到至少 40 列 × 12 行\nq 退出")
                    .wrap(Wrap { trim: false }),
                area,
            );
            return;
        }
        if let Some(player) = &self.player {
            self.draw_player(frame, player);
        } else {
            self.draw_browser(frame);
        }
        if let Some(job) = &self.job {
            popup(
                frame,
                "正在读取视频",
                &format!("{}\n\nEsc 取消 · q 退出", safe_text(&job.name)),
                60,
                9,
            );
        }
        if self.help {
            popup(
                frame,
                "快捷键",
                "文件浏览器\n↑/↓ 选择 · Enter 打开 · Backspace 上级\n/ 筛选 · r 刷新 · Esc 清除筛选\n\n播放器\n空格 暂停 · ←/→ 跳转 5 秒\n↑/↓ 音量 · m 静音\n1 半块 · 2 盲文 · 3 ASCII\nf 隐藏控制栏 · Enter 结束后重播\nEsc 返回浏览器 · s 播放设置\n? 帮助 · q/Ctrl+C 退出",
                62,
                17,
            );
        }
        if self.settings_menu {
            self.draw_settings(frame);
        }
    }

    fn draw_browser(&self, frame: &mut Frame) {
        let sections = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Min(2),
            Constraint::Length(3),
        ])
        .split(frame.area());
        frame.render_widget(
            Paragraph::new("终端播放一切  /  TPA")
                .style(
                    Style::default()
                        .fg(accent(self.colors))
                        .add_modifier(Modifier::BOLD),
                )
                .block(Block::default().borders(Borders::BOTTOM)),
            sections[0],
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(safe_text(&self.browser.directory.display().to_string())),
                Line::from(format!(
                    "筛选{} {}",
                    if self.browser.filtering {
                        "（输入中）"
                    } else {
                        ""
                    },
                    safe_text(&self.browser.filter)
                )),
            ]),
            sections[1],
        );
        let entries = self.browser.visible();
        let items: Vec<_> = entries
            .iter()
            .map(|entry| {
                ListItem::new(format!(
                    "{} {}",
                    if entry.directory {
                        "[目录]"
                    } else {
                        "[视频]"
                    },
                    safe_text(&entry.name)
                ))
            })
            .collect();
        let mut state = ListState::default().with_selected(
            (!entries.is_empty())
                .then_some(self.browser.selected.min(entries.len().saturating_sub(1))),
        );
        if items.is_empty() {
            frame.render_widget(
                Paragraph::new("没有匹配的视频。可用 tpa play <路径> 打开其他扩展名的文件。"),
                sections[2],
            );
        } else {
            frame.render_stateful_widget(
                List::new(items)
                    .highlight_symbol("› ")
                    .highlight_style(Style::default().bg(accent(self.colors)).fg(Color::Black)),
                sections[2],
                &mut state,
            );
        }
        let footer = self
            .browser
            .message
            .as_deref()
            .map(safe_text)
            .unwrap_or_else(|| {
                "↑↓ 选择 · Enter 打开 · Backspace 上级 · / 筛选 · ? 帮助 · q 退出".to_owned()
            });
        frame.render_widget(
            Paragraph::new(footer)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::TOP)),
            sections[3],
        );
    }

    fn draw_player(&self, frame: &mut Frame, player: &Player) {
        let area = video_area(frame.area(), self.hidden_controls);
        let viewport = scale_rect(
            fit(area, player.info.display_aspect, self.cli.cell_aspect),
            player.render_scale,
        );
        if let Some(video) = &player.frame {
            frame.render_widget(
                VideoWidget {
                    frame: video,
                    mode: player.mode,
                    colors: self.colors,
                },
                viewport,
            );
        } else {
            frame.render_widget(Paragraph::new(player.state.label()).centered(), viewport);
        }
        if !self.hidden_controls {
            let sections = Layout::vertical([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(frame.area());
            let title = player
                .info
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("TPA · ", Style::default().fg(accent(self.colors))),
                    Span::raw(safe_text(&title)),
                ])),
                sections[0],
            );
            let position = player.position();
            let duration = if player.state == PlayerState::Ended {
                Some(position)
            } else {
                player.info.duration
            };
            let progress = duration
                .map(|duration| position / duration)
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            let label = format!(
                "{} / {}",
                timestamp(position),
                duration.map(timestamp).unwrap_or_else(|| "未知".to_owned())
            );
            frame.render_widget(
                Gauge::default()
                    .gauge_style(Style::default().fg(accent(self.colors)).bg(Color::DarkGray))
                    .ratio(progress)
                    .label(label),
                sections[2],
            );
            let mode = match player.mode {
                RenderMode::Half => "半块",
                RenderMode::Braille => "盲文",
                RenderMode::Ascii => "ASCII",
            };
            let message = player
                .error
                .as_deref()
                .or(player.warning.as_deref())
                .map(safe_text)
                .unwrap_or_else(|| {
                    "空格 暂停 · ←→ 跳转 · ↑↓ 音量 · 1/2/3 模式 · s 设置 · f 全屏 · ? 帮助 · Esc 返回"
                        .to_owned()
                });
            let quality = self.quality.snapshot();
            let quality_label = match self.cli.quality {
                QualityMode::Fixed => "固定画质".to_owned(),
                QualityMode::Auto => {
                    let mut changes = Vec::new();
                    if quality.indexed_color {
                        changes.push("256 色".to_owned());
                    }
                    if quality.scale < 1.0 {
                        changes.push(format!("画面 {:.0}%", quality.scale * 100.0));
                    }
                    if quality.fps < self.cli.target_fps() {
                        changes.push(format!("{} FPS", quality.fps));
                    }
                    if changes.is_empty() {
                        "自动画质".to_owned()
                    } else {
                        format!("自适应 {}", changes.join(" / "))
                    }
                }
            };
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(format!(
                        "{} · {} · 音量 {}%{} · 刷新 {:.1} FPS · 丢帧 {}",
                        player.state.label(),
                        mode,
                        player.volume,
                        if player.muted { "（静音）" } else { "" },
                        self.refresh_rate,
                        player.dropped
                    )),
                    Line::from(format!(
                        "渲染 {:.1} ms · 终端 {:.1} ms · 调度迟 {:.1} ms · 管线取帧 {:.1} ms · {}",
                        self.render_ms,
                        self.output_ms,
                        self.scheduler_late_ms,
                        self.decode_read_ms,
                        quality_label
                    )),
                    Line::from(message),
                ])
                .wrap(Wrap { trim: false }),
                sections[3],
            );
        }
        if let Some(error) = &player.error {
            popup(
                frame,
                "播放失败",
                &format!("{}\n\nEnter 重试 · Esc 返回", safe_text(error)),
                65,
                12,
            );
        }
    }

    fn draw_settings(&self, frame: &mut Frame) {
        let area = frame.area();
        let width = 58.min(area.width.saturating_sub(2));
        let height = 13.min(area.height.saturating_sub(2));
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .title("播放设置")
            .borders(Borders::ALL)
            .style(Style::default().bg(Color::Black).fg(Color::White));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        let quality = self.quality.snapshot();
        let quality_name = match self.cli.quality {
            QualityMode::Auto => "自动适配",
            QualityMode::Fixed => "固定画质",
        };
        let color_name = if quality.indexed_color {
            "256 色"
        } else {
            match self.colors {
                ColorMode::Truecolor => "真彩色",
                ColorMode::Indexed => "256 色",
                ColorMode::Mono => "单色",
                ColorMode::Auto => "自动颜色",
            }
        };
        let effective = format!(
            "{} FPS · 画面 {:.0}% · {color_name}",
            quality.fps,
            quality.scale * 100.0
        );
        let items = vec![
            ListItem::new(format!("目标帧率    {} FPS", self.cli.target_fps())),
            ListItem::new(format!("画质策略    {quality_name}")),
        ];
        let mut state = ListState::default().with_selected(Some(self.settings_selection));
        let sections = Layout::vertical([Constraint::Length(4), Constraint::Min(1)]).split(inner);
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol("› ")
                .highlight_style(Style::default().bg(accent(self.colors)).fg(Color::Black)),
            sections[0],
            &mut state,
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(format!("当前输出：{effective}")),
                Line::from("↑/↓ 选择 · ←/→ 调整 · Enter 切换画质"),
                Line::from("5 FPS/步 · s/Esc 关闭"),
            ])
            .wrap(Wrap { trim: false }),
            sections[1],
        );
    }
}

fn accent(colors: ColorMode) -> Color {
    if colors == ColorMode::Mono {
        Color::White
    } else {
        Color::Cyan
    }
}
fn too_small(area: Rect) -> bool {
    area.width < 40 || area.height < 12
}
fn video_area(area: Rect, hidden: bool) -> Rect {
    if hidden {
        area
    } else {
        Rect::new(
            area.x,
            area.y + 2,
            area.width,
            area.height.saturating_sub(6),
        )
    }
}
fn timestamp(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}
fn safe_text(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() && c != '\n' {
                '�'
            } else {
                c
            }
        })
        .collect()
}
fn popup(frame: &mut Frame, title: &str, text: &str, width: u16, height: u16) {
    let area = frame.area();
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(Block::default().title(title).borders(Borders::ALL))
            .style(Style::default().bg(Color::Black).fg(Color::White)),
        popup,
    );
}

pub fn run(
    cli: Cli,
    directory: PathBuf,
    initial: Option<PathBuf>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    // We resolve NO_COLOR ourselves into black/white rendering. Disabling ANSI
    // styles in Crossterm would erase foreground/background contrast entirely,
    // and would also ignore an explicit --color truecolor override.
    crossterm::style::Colored::set_ansi_color_disabled(false);
    let mut session = TerminalSession::new()?;
    let mut app = App::new(cli, directory, initial)?;
    let mut next_draw = Instant::now();
    let mut timer_resolution = None;
    while !app.quit && !stop.load(Ordering::Acquire) {
        app.tick();
        if app.player.is_some() {
            timer_resolution.get_or_insert_with(TimerResolution::new);
        } else {
            timer_resolution = None;
        }
        let now = Instant::now();
        if now >= next_draw {
            let lateness = now.saturating_duration_since(next_draw);
            let draw_started = Instant::now();
            let mut render_time = Duration::ZERO;
            session.terminal.draw(|frame| {
                let render_started = Instant::now();
                app.draw(frame);
                render_time = render_started.elapsed();
            })?;
            let total = draw_started.elapsed();
            let output_time = total.saturating_sub(render_time);
            app.record_frame_times(render_time, output_time, lateness, Instant::now());
            app.ticks += 1;
            let elapsed = app.fps_anchor.elapsed().as_secs_f64();
            if elapsed >= 1.0 {
                app.refresh_rate = app.ticks as f64 / elapsed;
                app.ticks = 0;
                app.fps_anchor = Instant::now();
            }
            let rate = if app.player.is_some() {
                app.quality.snapshot().fps
            } else {
                10
            };
            // Preserve cadence across Windows' coarse input-wait timer ticks.
            // Scheduling from `now` each time compounds every late wake-up.
            let period = Duration::from_secs_f64(1.0 / rate as f64);
            next_draw += period;
            if Instant::now().saturating_duration_since(next_draw) > period {
                next_draw = Instant::now() + period;
            }
        }
        let timeout = next_draw
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(5));
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) => {
                    app.key(key);
                    next_draw = Instant::now();
                }
                Event::Resize(_, _) => {
                    app.pending_resize = Some(Instant::now());
                    next_draw = Instant::now();
                }
                _ => {}
            }
        }
    }
    drop(app); // clean up media before restoring the shell
    Ok(())
}

#[cfg(windows)]
struct TimerResolution(bool);

#[cfg(windows)]
#[link(name = "winmm")]
unsafe extern "system" {
    #[link_name = "timeBeginPeriod"]
    fn time_begin_period(period: u32) -> u32;
    #[link_name = "timeEndPeriod"]
    fn time_end_period(period: u32) -> u32;
}

#[cfg(windows)]
impl TimerResolution {
    fn new() -> Self {
        // SAFETY: winmm's timer period API has no pointer arguments and is paired in Drop.
        Self(unsafe { time_begin_period(1) == 0 })
    }
}

#[cfg(windows)]
impl Drop for TimerResolution {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: this balances the successful timeBeginPeriod call in new().
            let _ = unsafe { time_end_period(1) };
        }
    }
}

#[cfg(not(windows))]
struct TimerResolution;

#[cfg(not(windows))]
impl TimerResolution {
    fn new() -> Self {
        Self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::CellWidth;
    #[test]
    fn browser_and_help_render_without_terminal() {
        let temp = tempfile::tempdir().unwrap();
        let cli = <Cli as clap::Parser>::try_parse_from(["tpa"]).unwrap();
        let mut app = App::new(cli, temp.path().to_owned(), None).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        app.key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut symbols = String::new();
        // TestBackend retains hidden continuation cells from the underlying view.
        // Read visible glyphs exactly as a terminal would, skipping their width.
        for y in 0..24 {
            let mut x = 0;
            while x < 80 {
                let cell = &buffer[(x, y)];
                symbols.push_str(cell.symbol());
                x += cell.cell_width().max(1);
            }
        }
        assert!(symbols.contains("快捷键"));
    }
    #[test]
    fn filter_accepts_q_and_release_events_do_not_repeat() {
        let temp = tempfile::tempdir().unwrap();
        let cli = <Cli as clap::Parser>::try_parse_from(["tpa"]).unwrap();
        let mut app = App::new(cli, temp.path().to_owned(), None).unwrap();
        app.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!app.quit);
        assert_eq!(app.browser.filter, "q");
        app.key(KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ));
        assert_eq!(app.browser.filter, "q");
    }

    #[test]
    fn settings_menu_changes_frame_rate_and_quality_during_playback() {
        let temp = tempfile::tempdir().unwrap();
        let cli = <Cli as clap::Parser>::try_parse_from(["tpa", "--fps", "60"]).unwrap();
        let mut app = App::new(cli, temp.path().to_owned(), None).unwrap();
        app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert!(app.settings_menu);

        app.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.cli.target_fps(), 65);
        assert_eq!(app.quality.snapshot().fps, 65);

        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.cli.quality, QualityMode::Fixed);
        assert_eq!(app.quality.snapshot().fps, 65);

        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.settings_menu);
    }

    #[test]
    fn settings_menu_renders_current_runtime_values() {
        let temp = tempfile::tempdir().unwrap();
        let cli = <Cli as clap::Parser>::try_parse_from(["tpa", "--fps", "60"]).unwrap();
        let mut app = App::new(cli, temp.path().to_owned(), None).unwrap();
        app.settings_menu = true;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut symbols = String::new();
        for y in 0..24 {
            let mut x = 0;
            while x < 80 {
                let cell = &buffer[(x, y)];
                symbols.push_str(cell.symbol());
                x += cell.cell_width().max(1);
            }
        }
        assert!(symbols.contains("播放设置"));
        assert!(symbols.contains("目标帧率"));
        assert!(symbols.contains("60 FPS"));
        assert!(symbols.contains("自动适配"));
    }
}
