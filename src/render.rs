//! Tiny software canvas (replaces @napi-rs/canvas): rects + text via ab_glyph.
use anyhow::Result;
use image::{ImageBuffer, Rgba, RgbaImage};

pub type Rgb = [u8; 4];

pub fn hex(s: &str) -> Rgb {
    let s = s.trim_start_matches('#');
    let v = u32::from_str_radix(s, 16).unwrap_or(0);
    [(v >> 16) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8, 255]
}

#[derive(Clone, Copy, PartialEq)]
pub enum Face {
    Body,
    Bold,
    Mono,
    MonoBold,
}

pub struct Fonts {
    body: std::sync::Arc<ab_glyph::FontVec>,
    bold: std::sync::Arc<ab_glyph::FontVec>,
    mono: std::sync::Arc<ab_glyph::FontVec>,
    mono_bold: std::sync::Arc<ab_glyph::FontVec>,
}

const FONT_BODY: &[u8] = include_bytes!("../assets/fonts/DejaVuSans.ttf");
const FONT_BOLD: &[u8] = include_bytes!("../assets/fonts/DejaVuSans-Bold.ttf");
const FONT_MONO: &[u8] = include_bytes!("../assets/fonts/DejaVuSansMono.ttf");
const FONT_MONO_BOLD: &[u8] = include_bytes!("../assets/fonts/DejaVuSansMono-Bold.ttf");

fn load(bytes: &'static [u8]) -> Result<std::sync::Arc<ab_glyph::FontVec>> {
    Ok(std::sync::Arc::new(ab_glyph::FontVec::try_from_vec(bytes.to_vec())?))
}

impl Fonts {
    pub fn load() -> Result<Self> {
        Ok(Self {
            body: load(FONT_BODY)?,
            bold: load(FONT_BOLD)?,
            mono: load(FONT_MONO)?,
            mono_bold: load(FONT_MONO_BOLD)?,
        })
    }

    fn font(&self, face: Face) -> std::sync::Arc<ab_glyph::FontVec> {
        match face {
            Face::Body => self.body.clone(),
            Face::Bold => self.bold.clone(),
            Face::Mono => self.mono.clone(),
            Face::MonoBold => self.mono_bold.clone(),
        }
    }

    pub fn measure(&self, face: Face, size: f32, text: &str) -> f32 {
        use ab_glyph::{Font, ScaleFont};
        let font = self.font(face);
        let scaled = font.as_scaled(size);
        text.chars().map(|c| scaled.h_advance(scaled.glyph_id(c))).sum()
    }
}

pub struct Canvas {
    img: RgbaImage,
    pub fonts: Fonts,
}

impl Canvas {
    pub fn new(w: u32, h: u32, bg: Rgb) -> Result<Self> {
        let img: RgbaImage = ImageBuffer::from_pixel(w, h, Rgba(bg));
        Ok(Self { img, fonts: Fonts::load()? })
    }

    /// Wrap an already-composited RGBA buffer (photos, avatars) so text can be drawn on it.
    pub fn from_image(img: RgbaImage, fonts: Fonts) -> Self {
        Self { img, fonts }
    }

    pub fn width(&self) -> u32 {
        self.img.width()
    }

    pub fn fill(&mut self, x: i32, y: i32, w: u32, h: u32, color: Rgb) {
        let (iw, ih) = (self.img.width() as i32, self.img.height() as i32);
        for py in y.max(0)..(y + h as i32).min(ih) {
            for px in x.max(0)..(x + w as i32).min(iw) {
                self.img.put_pixel(px as u32, py as u32, Rgba(color));
            }
        }
    }

    /// x = left edge, y = baseline.
    pub fn text(&mut self, x: i32, y: i32, size: f32, face: Face, color: Rgb, text: &str) {
        use ab_glyph::{point, Font, ScaleFont};
        let font = self.fonts.font(face).clone();
        let scaled = font.as_scaled(size);
        let mut caret = x as f32;
        let baseline = y as f32;
        for ch in text.chars() {
            let glyph_id = scaled.glyph_id(ch);
            let glyph = glyph_id.with_scale_and_position(size, point(caret, baseline));
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|gx, gy, coverage| {
                    let px = bounds.min.x as i32 + gx as i32;
                    let py = bounds.min.y as i32 + gy as i32;
                    self.blend(px, py, color, coverage);
                });
            }
            caret += scaled.h_advance(glyph_id);
        }
    }

    fn blend(&mut self, x: i32, y: i32, color: Rgb, coverage: f32) {
        if x < 0 || y < 0 || x >= self.img.width() as i32 || y >= self.img.height() as i32 {
            return;
        }
        let px = self.img.get_pixel_mut(x as u32, y as u32);
        let a = coverage.clamp(0.0, 1.0);
        for i in 0..3 {
            px.0[i] = (px.0[i] as f32 * (1.0 - a) + color[i] as f32 * a) as u8;
        }
        px.0[3] = 255;
    }

    /// Right-aligned text: `right_x` is the right edge.
    pub fn text_right(&mut self, right_x: i32, y: i32, size: f32, face: Face, color: Rgb, text: &str) {
        let w = self.fonts.measure(face, size, text);
        self.text(right_x - w as i32, y, size, face, color, text);
    }

    /// Shrink with a trailing ellipsis until it fits max_w.
    pub fn fit(&self, face: Face, size: f32, text: &str, max_w: f32) -> String {
        if self.fonts.measure(face, size, text) <= max_w {
            return text.to_string();
        }
        let mut chars: Vec<char> = text.chars().collect();
        while !chars.is_empty() {
            chars.pop();
            let candidate: String = chars.iter().collect::<String>() + "…";
            if self.fonts.measure(face, size, &candidate) <= max_w {
                return candidate;
            }
        }
        "…".to_string()
    }

    /// Greedy word wrap; long words are split per char.
    pub fn wrap(&self, face: Face, size: f32, text: &str, max_w: f32) -> Vec<String> {
        let mut lines = Vec::new();
        let mut line = String::new();
        for word in text.split(' ') {
            if self.fonts.measure(face, size, word) <= max_w {
                let next = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
                if self.fonts.measure(face, size, &next) <= max_w {
                    line = next;
                } else {
                    if !line.is_empty() {
                        lines.push(std::mem::take(&mut line));
                    }
                    line = word.to_string();
                }
                continue;
            }
            for ch in word.chars() {
                let next = format!("{line}{ch}");
                if self.fonts.measure(face, size, &next) <= max_w {
                    line = next;
                } else {
                    if !line.is_empty() {
                        lines.push(std::mem::take(&mut line));
                    }
                    line = ch.to_string();
                }
            }
        }
        if !line.is_empty() {
            lines.push(line);
        }
        lines
    }

    pub fn png(&self) -> Result<Vec<u8>> {
        let mut buf = std::io::Cursor::new(Vec::new());
        self.img.write_to(&mut buf, image::ImageFormat::Png)?;
        Ok(buf.into_inner())
    }
}
