use anyhow::{Context, Result};
use clap::Parser;
use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tpa::{
    audio,
    cli::{Cli, Command},
    media::Tools,
    render, ui,
};

fn main() {
    if let Err(error) = entry() {
        eprintln!("tpa：{error:#}");
        std::process::exit(1);
    }
}

fn entry() -> Result<()> {
    let cli = Cli::parse();
    let tools = Tools {
        ffmpeg: cli.ffmpeg.clone(),
        ffprobe: cli.ffprobe.clone(),
    };
    if matches!(cli.command, Some(Command::Doctor)) {
        return doctor(&tools, &cli);
    }
    tools.check()?;
    let stop = Arc::new(AtomicBool::new(false));
    let signal = stop.clone();
    ctrlc::set_handler(move || signal.store(true, Ordering::Release))
        .context("无法设置退出信号处理")?;
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ui::restore_terminal();
        hook(info);
    }));
    let cwd = std::env::current_dir().context("无法读取工作目录")?;
    let (directory, initial) = match &cli.command {
        Some(Command::Browse { directory }) => (directory.clone().unwrap_or(cwd), None),
        Some(Command::Play { path }) => {
            let path = if path.is_absolute() {
                path.clone()
            } else {
                cwd.join(path)
            };
            (
                path.parent()
                    .filter(|p| p.is_dir())
                    .map(PathBuf::from)
                    .unwrap_or(cwd),
                Some(path),
            )
        }
        None => (cwd, None),
        Some(Command::Doctor) => unreachable!(),
    };
    ui::run(cli, directory, initial, stop)
}

fn doctor(tools: &Tools, cli: &Cli) -> Result<()> {
    println!("终端播放一切 · 环境诊断\n");
    let check = tools.check();
    match &check {
        Ok(versions) => {
            for version in versions {
                println!("[正常] {version}");
            }
        }
        Err(error) => println!("[缺失] {error:#}"),
    }
    println!(
        "[终端] 交互式输入={} 输出={}，颜色模式={:?}",
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        render::resolve_color(cli.color)
    );
    println!("[字体] 需要 UTF-8 等宽字体；请确认 ▀ ⣿ 可正常显示");
    match audio::device_description() {
        Ok(device) => println!("[音频] {device}"),
        Err(error) => println!("[音频] {error:#}；将使用无声播放"),
    }
    check.map(|_| ())
}
