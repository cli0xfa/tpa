use crate::{
    cli::{ColorMode, RenderMode},
    media::VideoFrame,
};
use ratatui::{buffer::Buffer, layout::Rect, style::Color, widgets::Widget};

pub fn resolve_color(mode: ColorMode) -> ColorMode {
    if mode != ColorMode::Auto {
        return mode;
    }
    if std::env::var_os("NO_COLOR").is_some() || std::env::var("TERM").is_ok_and(|v| v == "dumb") {
        return ColorMode::Mono;
    }
    let colorterm = std::env::var("COLORTERM").unwrap_or_default();
    if colorterm == "truecolor" || colorterm == "24bit" || std::env::var_os("WT_SESSION").is_some()
    {
        ColorMode::Truecolor
    } else {
        ColorMode::Indexed
    }
}

/// Fit the physical display rectangle, not just the character count.
pub fn fit(area: Rect, display_aspect: f64, cell_aspect: f64) -> Rect {
    if area.width == 0 || area.height == 0 {
        return Rect::new(area.x, area.y, 0, 0);
    }
    let desired = display_aspect / cell_aspect;
    let (width, height) = if area.width as f64 / area.height as f64 > desired {
        (
            (area.height as f64 * desired)
                .round()
                .clamp(1.0, area.width as f64) as u16,
            area.height,
        )
    } else {
        (
            area.width,
            (area.width as f64 / desired)
                .round()
                .clamp(1.0, area.height as f64) as u16,
        )
    };
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

pub fn scale_rect(area: Rect, scale: f64) -> Rect {
    if area.is_empty() {
        return area;
    }
    let scale = scale.clamp(0.75, 1.0);
    let width = (area.width as f64 * scale)
        .round()
        .clamp(1.0, area.width as f64) as u16;
    let height = (area.height as f64 * scale)
        .round()
        .clamp(1.0, area.height as f64) as u16;
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

pub fn luma([r, g, b]: [u8; 3]) -> u16 {
    (54 * r as u16 + 183 * g as u16 + 19 * b as u16) / 256
}

fn average(pixels: &[[u8; 3]]) -> [u8; 3] {
    if pixels.is_empty() {
        return [0; 3];
    }
    let mut sum = [0u32; 3];
    for p in pixels {
        for c in 0..3 {
            sum[c] += p[c] as u32;
        }
    }
    sum.map(|s| (s / pixels.len() as u32) as u8)
}

pub fn color(rgb: [u8; 3], mode: ColorMode) -> Color {
    match mode {
        ColorMode::Mono => {
            if luma(rgb) >= 128 {
                Color::White
            } else {
                Color::Black
            }
        }
        ColorMode::Indexed => {
            // xterm's cube levels are 0, 95, 135, 175, 215, 255 rather than
            // evenly spaced 0..255; use their actual nearest-color boundaries.
            let q = rgb.map(|v| {
                if v < 48 {
                    0
                } else if v < 115 {
                    1
                } else {
                    (v - 35) / 40
                }
            });
            let cube = q.map(|v| if v == 0 { 0 } else { 55 + v as u32 * 40 });
            let gray_index = ((luma(rgb) as i32 - 8 + 5) / 10).clamp(0, 23) as u8;
            let gray = 8 + gray_index as u32 * 10;
            let error = |candidate: [u32; 3]| {
                rgb.iter()
                    .zip(candidate)
                    .map(|(v, c)| (*v as i32 - c as i32).pow(2))
                    .sum::<i32>()
            };
            if error([gray; 3]) < error(cube) {
                Color::Indexed(232 + gray_index)
            } else {
                Color::Indexed(16 + 36 * q[0] + 6 * q[1] + q[2])
            }
        }
        _ => Color::Rgb(rgb[0], rgb[1], rgb[2]),
    }
}

pub fn encode_cell(p: [[u8; 3]; 8], mode: RenderMode, colors: ColorMode) -> (char, Color, Color) {
    match mode {
        RenderMode::Half => (
            '▀',
            color(average(&p[..4]), colors),
            color(average(&p[4..]), colors),
        ),
        RenderMode::Ascii => {
            let rgb = average(&p);
            let ramp = b" .:-=+*#%@";
            let ch = ramp[luma(rgb) as usize * (ramp.len() - 1) / 255] as char;
            let fg = if colors == ColorMode::Mono {
                Color::White
            } else {
                color(rgb, colors)
            };
            (ch, fg, Color::Black)
        }
        RenderMode::Braille => {
            let threshold = p.iter().map(|p| luma(*p) as u32).sum::<u32>() / 8;
            let mut mask = 0u32;
            let mut bright = [[0; 3]; 8];
            let mut dark = [[0; 3]; 8];
            let (mut nb, mut nd) = (0, 0);
            // Row-major samples to Unicode braille bit positions.
            for (i, pixel) in p.into_iter().enumerate() {
                if luma(pixel) as u32 >= threshold {
                    mask |= 1 << [0, 3, 1, 4, 2, 5, 6, 7][i];
                    bright[nb] = pixel;
                    nb += 1;
                } else {
                    dark[nd] = pixel;
                    nd += 1;
                }
            }
            let fg = if colors == ColorMode::Mono {
                Color::White
            } else {
                color(average(&bright[..nb]), colors)
            };
            (
                char::from_u32(0x2800 + mask).unwrap(),
                fg,
                color(average(&dark[..nd]), colors),
            )
        }
    }
}

pub struct VideoWidget<'a> {
    pub frame: &'a VideoFrame,
    pub mode: RenderMode,
    pub colors: ColorMode,
}

impl Widget for VideoWidget<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        if area.is_empty()
            || self.frame.rgb.len() < self.frame.width as usize * self.frame.height as usize * 3
        {
            return;
        }
        let sample_width = area.width as u32 * 2;
        let sample_height = area.height as u32 * 4;
        let x_samples: Vec<u32> = (0..sample_width)
            .map(|x| (x * self.frame.width / sample_width).min(self.frame.width - 1))
            .collect();
        let y_samples: Vec<u32> = (0..sample_height)
            .map(|y| (y * self.frame.height / sample_height).min(self.frame.height - 1))
            .collect();
        let mut symbol = [0u8; 4];
        for row in 0..area.height {
            for col in 0..area.width {
                let mut pixels = [[0; 3]; 8];
                for y in 0..4 {
                    for x in 0..2 {
                        let sx = x_samples[col as usize * 2 + x as usize];
                        let sy = y_samples[row as usize * 4 + y as usize];
                        let index = (sy as usize * self.frame.width as usize + sx as usize) * 3;
                        pixels[(y * 2 + x) as usize]
                            .copy_from_slice(&self.frame.rgb[index..index + 3]);
                    }
                }
                let (ch, fg, bg) = encode_cell(pixels, self.mode, self.colors);
                buffer[(area.x + col, area.y + row)]
                    .set_symbol(ch.encode_utf8(&mut symbol))
                    .set_fg(fg)
                    .set_bg(bg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn half_preserves_both_colors() {
        let mut pixels = [[255, 0, 0]; 8];
        pixels[4..].fill([0, 0, 255]);
        assert_eq!(
            encode_cell(pixels, RenderMode::Half, ColorMode::Truecolor),
            ('▀', Color::Rgb(255, 0, 0), Color::Rgb(0, 0, 255))
        );
    }
    #[test]
    fn braille_dot_order_and_ascii_endpoints() {
        let mut pixels = [[0; 3]; 8];
        pixels[6] = [255; 3];
        assert_eq!(
            encode_cell(pixels, RenderMode::Braille, ColorMode::Truecolor).0,
            '\u{2840}'
        );
        assert_eq!(
            encode_cell([[0; 3]; 8], RenderMode::Ascii, ColorMode::Mono).0,
            ' '
        );
        assert_eq!(
            encode_cell([[255; 3]; 8], RenderMode::Ascii, ColorMode::Mono).0,
            '@'
        );
    }
    #[test]
    fn fit_preserves_physical_aspect() {
        assert_eq!(
            fit(Rect::new(0, 0, 160, 45), 16.0 / 9.0, 0.5),
            Rect::new(0, 0, 160, 45)
        );
        assert_eq!(
            fit(Rect::new(0, 0, 160, 45), 1.0, 0.5),
            Rect::new(35, 0, 90, 45)
        );
        assert_eq!(fit(Rect::new(0, 0, 0, 0), 1.0, 0.5).width, 0);
    }

    #[test]
    fn scaled_rect_stays_centered_and_within_bounds() {
        let area = Rect::new(10, 4, 160, 45);
        assert_eq!(scale_rect(area, 1.0), area);
        assert_eq!(scale_rect(area, 0.9), Rect::new(18, 6, 144, 41));
        assert_eq!(scale_rect(area, 0.1), Rect::new(30, 9, 120, 34));
        assert_eq!(scale_rect(Rect::default(), 0.9), Rect::default());
    }
    #[test]
    fn palette_handles_primaries_and_grays() {
        assert_eq!(color([100, 0, 0], ColorMode::Indexed), Color::Indexed(52));
        assert_eq!(color([255, 0, 0], ColorMode::Indexed), Color::Indexed(196));
        assert_eq!(color([128; 3], ColorMode::Indexed), Color::Indexed(244));
        assert_eq!(color([255; 3], ColorMode::Mono), Color::White);
    }
}
