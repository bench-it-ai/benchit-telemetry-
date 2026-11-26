//! egui implementation of the layout [`Canvas`], plus font loading.
use crate::layout::{Align, Canvas, Face, Rgb, Style, DIM, LINE, W};
use eframe::egui::{
    self, epaint::text::TextWrapping, text::LayoutJob, Color32, FontData, FontDefinitions, FontFamily, FontId, Pos2,
    Rect, Shape, Stroke, TextFormat, Vec2,
};
use std::path::{Path, PathBuf};

pub fn color(c: Rgb) -> Color32 {
    let f = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color32::from_rgb(f(c[0]), f(c[1]), f(c[2]))
}

pub fn bg() -> Color32 {
    Color32::from_rgba_unmultiplied(5, 6, 7, 230)
}

fn family(face: Face) -> FontFamily {
    FontFamily::Name(
        match face {
            Face::Sans => "bt-sans",
            Face::SansMedium => "bt-sans-medium",
            Face::Mono => "bt-mono",
            Face::MonoLight => "bt-mono-light",
        }
        .into(),
    )
}

fn font_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(h) = std::env::var_os("HOME") {
        v.push(PathBuf::from(h).join(".local/share/fonts"));
    }
    v.push("/usr/local/share/fonts".into());
    v.push("/usr/share/fonts".into());
    v
}

fn find(dir: &Path, names: &[&str], depth: usize) -> Option<PathBuf> {
    let rd = std::fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            subdirs.push(p);
        } else if p.file_name().and_then(|n| n.to_str()).is_some_and(|n| names.contains(&n)) {
            return Some(p);
        }
    }
    if depth > 0 {
        for d in subdirs {
            if let Some(p) = find(&d, names, depth - 1) {
                return Some(p);
            }
        }
    }
    None
}

/// Register Inter / JetBrains Mono if installed; egui's bundled fonts remain as fallback.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fd = FontDefinitions::default();
    let wanted: [(&str, &[&str], FontFamily); 4] = [
        ("bt-sans", &["Inter-Regular.ttf", "Inter[opsz,wght].ttf"], FontFamily::Proportional),
        ("bt-sans-medium", &["Inter-Medium.ttf", "Inter-SemiBold.ttf", "Inter-Regular.ttf"], FontFamily::Proportional),
        ("bt-mono", &["JetBrainsMono-Regular.ttf", "JetBrainsMono[wght].ttf"], FontFamily::Monospace),
        ("bt-mono-light", &["JetBrainsMono-Light.ttf", "JetBrainsMono-Regular.ttf"], FontFamily::Monospace),
    ];
    let dirs = font_dirs();
    for (key, files, fallback) in wanted {
        let mut chain = Vec::new();
        for d in &dirs {
            if let Some(p) = find(d, files, 3) {
                if let Ok(bytes) = std::fs::read(&p) {
                    fd.font_data.insert(key.to_string(), std::sync::Arc::new(FontData::from_owned(bytes)));
                    chain.push(key.to_string());
                    break;
                }
            }
        }
        chain.extend(fd.families.get(&fallback).cloned().unwrap_or_default());
        fd.families.insert(FontFamily::Name(key.into()), chain);
    }
    ctx.set_fonts(fd);
}

pub struct Gui<'a> {
    pub painter: &'a egui::Painter,
}

impl Gui<'_> {
    fn job(&self, s: &str, st: &Style, max_w: Option<f32>) -> LayoutJob {
        let px = st.pt * 4.0 / 3.0;
        let text = if st.upper { s.to_uppercase() } else { s.to_string() };
        let fmt = TextFormat {
            font_id: FontId::new(px, family(st.face)),
            color: color(st.color),
            extra_letter_spacing: st.spacing * px,
            ..Default::default()
        };
        let mut job = LayoutJob::single_section(text, fmt);
        if let Some(w) = max_w {
            job.wrap = TextWrapping::truncate_at_width(w.max(1.0));
        }
        job
    }
}

impl Canvas for Gui<'_> {
    fn text(&mut self, x: f32, y: f32, s: &str, st: &Style) -> f32 {
        let galley = self.painter.layout_job(self.job(s, st, st.max_w));
        let w = galley.size().x;
        let x = match st.align {
            Align::Left => x,
            Align::Right => x - w,
            Align::Center => x - w / 2.0,
        };
        self.painter.galley(Pos2::new(x, y), galley, color(st.color));
        w
    }

    fn measure(&self, s: &str, st: &Style) -> (f32, f32) {
        let g = self.painter.layout_job(self.job(s, st, None));
        (g.size().x, g.size().y)
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Rgb) {
        self.painter.rect_filled(Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h)), 0.0, color(c));
    }

    fn hline(&mut self, y: f32) {
        self.painter.line_segment(
            [Pos2::new(crate::layout::PAD, y + 0.5), Pos2::new(W - crate::layout::PAD, y + 0.5)],
            Stroke::new(1.0, color(LINE)),
        );
    }

    fn dot(&mut self, x: f32, y: f32, r: f32, c: Rgb) {
        self.painter.circle_filled(Pos2::new(x, y), r, color(c));
    }

    fn spark(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        data: &[Option<f32>],
        lo: Option<f32>,
        hi: Option<f32>,
        c: Rgb,
    ) {
        self.painter
            .line_segment([Pos2::new(x, y + h + 0.5), Pos2::new(x + w, y + h + 0.5)], Stroke::new(1.0, color(LINE)));
        let vals: Vec<f32> = data.iter().flatten().copied().collect();
        if vals.len() < 2 {
            return;
        }
        let lo = lo.unwrap_or_else(|| vals.iter().copied().fold(f32::INFINITY, f32::min));
        let mut hi = hi.unwrap_or_else(|| vals.iter().copied().fold(f32::NEG_INFINITY, f32::max));
        if hi - lo < 1e-6 {
            hi = lo + 1.0;
        }
        let n = data.len();
        let step = w / (crate::model::HIST as f32 - 1.0);
        let x0 = x + w - (n as f32 - 1.0) * step;
        let stroke = Stroke::new(1.2, color(c).gamma_multiply(0.9));
        let mut seg: Vec<Pos2> = Vec::new();
        for (i, v) in data.iter().enumerate() {
            match v {
                Some(v) => {
                    let t = (v.clamp(lo, hi) - lo) / (hi - lo);
                    seg.push(Pos2::new(x0 + i as f32 * step, y + h - t * h));
                }
                None => {
                    if seg.len() > 1 {
                        self.painter.add(Shape::line(std::mem::take(&mut seg), stroke));
                    }
                    seg.clear();
                }
            }
        }
        if seg.len() > 1 {
            self.painter.add(Shape::line(seg, stroke));
        }
    }
}

/// Window chrome: translucent background, border and corner brackets.
pub fn draw_frame(p: &egui::Painter, h: f32) {
    p.rect_filled(Rect::from_min_size(Pos2::ZERO, Vec2::new(W, h)), 0.0, bg());
    let line = Stroke::new(1.0, color(LINE));
    p.rect_stroke(
        Rect::from_min_size(Pos2::new(0.5, 0.5), Vec2::new(W - 1.0, h - 1.0)),
        0.0,
        line,
        egui::StrokeKind::Inside,
    );
    let dim = Stroke::new(1.0, color(DIM));
    for (cx, cy, dx, dy) in [(0.0, 0.0, 1.0, 1.0), (W, 0.0, -1.0, 1.0), (0.0, h, 1.0, -1.0), (W, h, -1.0, -1.0)] {
        p.add(Shape::line(
            vec![
                Pos2::new(cx + dx * 1.5, cy + dy * 12.0),
                Pos2::new(cx + dx * 1.5, cy + dy * 1.5),
                Pos2::new(cx + dx * 12.0, cy + dy * 1.5),
            ],
            dim,
        ));
    }
}
