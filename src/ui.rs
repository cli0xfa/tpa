use crate::{
    browser::Browser,
    cli::{Cli, ColorMode, RenderMode},
    media::{MediaInfo, Tools},
    player::{Player, PlayerCommand, PlayerOptions, PlayerState},
    render::{VideoWidget, fit, resolve_color},
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
    help: bool,
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
        let mut app = Self {
            cli,
            tools,
            browser: Browser::new(directory)?,
            player: None,
            job: None,
            colors,
            help: false,
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
        self.help = false;
        self.browser.message = None;
        self.job = Some(ProbeJob::start(self.tools.clone(), path));
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
                            fps: self.cli.fps,
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
                "文件浏览器\n↑/↓ 选择 · Enter 打开 · Backspace 上级\n/ 筛选 · r 刷新 · Esc 清除筛选\n\n播放器\n空格 暂停 · ←/→ 跳转 5 秒\n↑/↓ 音量 · m 静音\n1 半块 · 2 盲文 · 3 ASCII\nf 隐藏控制栏 · Enter 结束后重播\nEsc 返回浏览器 · ? 帮助 · q/Ctrl+C 退出",
                62,
                17,
            );
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
        let viewport = fit(area, player.info.display_aspect, self.cli.cell_aspect);
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
                    "空格 暂停 · ←→ 跳转 · ↑↓ 音量 · 1/2/3 模式 · f 全屏 · ? 帮助 · Esc 返回"
                        .to_owned()
                });
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
    while !app.quit && !stop.load(Ordering::Acquire) {
        app.tick();
        let now = Instant::now();
        if now >= next_draw {
            session.terminal.draw(|frame| app.draw(frame))?;
            app.ticks += 1;
            let elapsed = app.fps_anchor.elapsed().as_secs_f64();
            if elapsed >= 1.0 {
                app.refresh_rate = app.ticks as f64 / elapsed;
                app.ticks = 0;
                app.fps_anchor = Instant::now();
            }
            let rate = if app.player.is_some() {
                app.cli.fps
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
}
