use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum RenderMode {
    #[default]
    Half,
    Braille,
    Ascii,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum ColorMode {
    #[default]
    Auto,
    Truecolor,
    #[value(name = "256")]
    Indexed,
    Mono,
}

#[derive(Clone, Debug, Parser)]
#[command(
    name = "tpa",
    version,
    about = "终端播放一切 · 彩色 Unicode 视频播放器"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    #[arg(long, global = true, default_value = "ffmpeg")]
    pub ffmpeg: PathBuf,
    #[arg(long, global = true, default_value = "ffprobe")]
    pub ffprobe: PathBuf,
    #[arg(long, global = true, value_enum, default_value = "half")]
    pub mode: RenderMode,
    #[arg(long, global = true, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=60))]
    pub fps: u32,
    #[arg(long, global = true, value_enum, default_value = "auto")]
    pub color: ColorMode,
    #[arg(long, global = true, default_value = "0.5", value_parser = parse_aspect)]
    pub cell_aspect: f64,
    #[arg(long, global = true)]
    pub no_audio: bool,
}

#[derive(Clone, Debug, Subcommand)]
pub enum Command {
    /// 浏览目录中的本地视频
    Browse { directory: Option<PathBuf> },
    /// 直接播放本地视频
    Play { path: PathBuf },
    /// 检查媒体工具、终端环境和音频设备
    Doctor,
}

fn parse_aspect(value: &str) -> Result<f64, String> {
    let parsed: f64 = value.parse().map_err(|_| "请输入有效数值".to_owned())?;
    if parsed.is_finite() && (0.1..=2.0).contains(&parsed) {
        Ok(parsed)
    } else {
        Err("字符格宽高比必须在 0.1 到 2.0 之间".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn global_options_work_after_subcommand() {
        let cli = Cli::try_parse_from([
            "tpa",
            "play",
            "中文 视频.mp4",
            "--mode",
            "braille",
            "--fps",
            "24",
        ])
        .unwrap();
        assert_eq!(cli.mode, RenderMode::Braille);
        assert_eq!(cli.fps, 24);
    }
    #[test]
    fn invalid_limits_are_rejected() {
        for args in [
            vec!["tpa", "--fps", "0"],
            vec!["tpa", "--fps", "61"],
            vec!["tpa", "--cell-aspect", "NaN"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }
}
