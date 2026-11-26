//! A small top-down page writer over `printpdf` for the panel's PDF report: A4 pages, the
//! built-in Helvetica/Courier fonts (no font files, no system libraries), text lines and simple
//! line charts. Text is reduced to printable ASCII because the built-in fonts only cover a
//! single-byte encoding.
use printpdf::{
    BuiltinFont, Color, Line, LinePoint, Mm, Op, PaintMode, PdfDocument, PdfFontHandle, PdfPage, PdfSaveOptions, Point,
    Pt, Rect, Rgb, TextItem,
};

const PAGE_W: f32 = 595.28;
const PAGE_H: f32 = 841.89;
const MARGIN: f32 = 48.0;
const CHART_H: f32 = 92.0;

const INK: (f32, f32, f32) = (0.10, 0.11, 0.13);
const MUTED: (f32, f32, f32) = (0.42, 0.45, 0.50);
const RULE: (f32, f32, f32) = (0.80, 0.82, 0.85);
const ACCENT: (f32, f32, f32) = (0.05, 0.42, 0.75);

fn rgb((r, g, b): (f32, f32, f32)) -> Color {
    Color::Rgb(Rgb { r, g, b, icc_profile: None })
}

/// Printable ASCII only (`°` becomes nothing before `C`/`F`, other characters become `?`).
pub fn ascii(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '°')
        .map(|c| match c {
            ' '..='~' => c,
            '·' | '•' => '-',
            '–' | '—' => '-',
            '\t' => ' ',
            _ => '?',
        })
        .collect()
}

pub struct Doc {
    title: String,
    pages: Vec<Vec<Op>>,
    ops: Vec<Op>,
    /// Distance of the next baseline from the top of the page.
    y: f32,
}

impl Doc {
    pub fn new(title: &str) -> Self {
        Self { title: ascii(title), pages: Vec::new(), ops: Vec::new(), y: MARGIN }
    }

    fn page_break(&mut self) {
        let ops = std::mem::take(&mut self.ops);
        self.pages.push(ops);
        self.y = MARGIN;
    }

    /// Make room for `h` points, starting a new page if needed.
    fn room(&mut self, h: f32) {
        if self.y + h > PAGE_H - MARGIN {
            self.page_break();
        }
    }

    fn put(&mut self, s: &str, size: f32, font: BuiltinFont, col: (f32, f32, f32)) {
        self.room(size * 1.4);
        self.y += size;
        let y = PAGE_H - self.y;
        self.text_at(MARGIN, y, s, size, font, col);
        self.y += size * 0.4;
    }

    fn text_at(&mut self, x: f32, y: f32, s: &str, size: f32, font: BuiltinFont, col: (f32, f32, f32)) {
        self.ops.extend([
            Op::StartTextSection,
            Op::SetTextCursor { pos: Point { x: Pt(x), y: Pt(y) } },
            Op::SetFont { font: PdfFontHandle::Builtin(font), size: Pt(size) },
            Op::SetFillColor { col: rgb(col) },
            Op::ShowText { items: vec![TextItem::Text(ascii(s))] },
            Op::EndTextSection,
        ]);
    }

    fn line(&mut self, pts: &[(f32, f32)], width: f32, col: (f32, f32, f32)) {
        if pts.len() < 2 {
            return;
        }
        self.ops.push(Op::SetOutlineColor { col: rgb(col) });
        self.ops.push(Op::SetOutlineThickness { pt: Pt(width) });
        self.ops.push(Op::DrawLine {
            line: Line {
                points: pts
                    .iter()
                    .map(|(x, y)| LinePoint { p: Point { x: Pt(*x), y: Pt(*y) }, bezier: false })
                    .collect(),
                is_closed: false,
            },
        });
    }

    pub fn heading(&mut self, s: &str, size: f32) {
        self.room(size * 1.4 + 8.0);
        self.put(s, size, BuiltinFont::HelveticaBold, INK);
        let y = PAGE_H - self.y;
        self.line(&[(MARGIN, y), (PAGE_W - MARGIN, y)], 0.6, RULE);
        self.y += 5.0;
    }

    pub fn text(&mut self, s: &str, size: f32) {
        self.put(s, size, BuiltinFont::Helvetica, INK);
    }

    pub fn mono(&mut self, s: &str, size: f32) {
        self.put(s, size, BuiltinFont::Courier, INK);
    }

    /// Keep the next `h` points together (start a new page if they would not fit).
    pub fn keep(&mut self, h: f32) {
        self.room(h);
    }

    pub fn gap(&mut self, h: f32) {
        self.y += h;
    }

    /// A titled line chart of `(ts, value)` points over `[t0, t1]`, with min/avg/max labels.
    pub fn chart(&mut self, title: &str, unit: &str, pts: &[(f64, f64)], t0: f64, t1: f64) {
        self.room(CHART_H + 34.0);
        self.put(title, 9.5, BuiltinFont::HelveticaBold, INK);
        let vals: Vec<f64> = pts.iter().map(|p| p.1).collect();
        let (mut lo, mut hi) = (
            vals.iter().copied().fold(f64::INFINITY, f64::min),
            vals.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        );
        let avg = vals.iter().sum::<f64>() / vals.len().max(1) as f64;
        let label = format!("min {lo:.1} {unit}   avg {avg:.1} {unit}   max {hi:.1} {unit}");
        self.put(&label, 7.5, BuiltinFont::Helvetica, MUTED);
        if lo >= 0.0 && lo < hi * 0.25 {
            lo = 0.0;
        }
        if hi - lo < 1e-6 {
            hi = lo + 1.0;
        }
        let (x0, w) = (MARGIN + 34.0, PAGE_W - 2.0 * MARGIN - 34.0);
        let top = PAGE_H - self.y - 2.0;
        let bottom = top - CHART_H;
        self.ops.push(Op::SetFillColor { col: rgb((0.97, 0.975, 0.98)) });
        self.ops.push(Op::DrawRectangle {
            rectangle: Rect {
                x: Pt(x0),
                y: Pt(bottom),
                width: Pt(w),
                height: Pt(CHART_H),
                mode: Some(PaintMode::Fill),
                winding_order: None,
            },
        });
        self.line(&[(x0, bottom), (x0 + w, bottom)], 0.6, RULE);
        let digits = if hi - lo < 10.0 { 1 } else { 0 };
        self.text_at(MARGIN, top - 7.0, &format!("{hi:.digits$}"), 7.0, BuiltinFont::Helvetica, MUTED);
        self.text_at(MARGIN, bottom, &format!("{lo:.digits$}"), 7.0, BuiltinFont::Helvetica, MUTED);
        let span = (t1 - t0).max(1.0);
        let gap = span / 60.0; // break the line across gaps (daemon stopped, suspend)
        let mut seg: Vec<(f32, f32)> = Vec::new();
        let mut prev_t = None;
        for (t, v) in pts {
            if prev_t.is_some_and(|p: f64| t - p > gap.max(1.0) * 3.0) {
                let s = std::mem::take(&mut seg);
                self.line(&s, 1.0, ACCENT);
            }
            prev_t = Some(*t);
            let x = x0 + (((t - t0) / span).clamp(0.0, 1.0) as f32) * w;
            let y = bottom + (((v - lo) / (hi - lo)).clamp(0.0, 1.0) as f32) * CHART_H;
            seg.push((x, y));
        }
        self.line(&seg, 1.0, ACCENT);
        self.y = PAGE_H - bottom + 14.0;
    }

    /// Serialise all pages (with a footer) to PDF bytes, dated `created` (unix seconds).
    pub fn finish(mut self, created: i64) -> Vec<u8> {
        self.page_break();
        let n = self.pages.len();
        let title = self.title.clone();
        let mut pages = Vec::with_capacity(n);
        for (i, mut ops) in std::mem::take(&mut self.pages).into_iter().enumerate() {
            let footer = format!("{title}  -  page {} of {n}", i + 1);
            self.ops.clear();
            self.text_at(MARGIN, MARGIN * 0.5, &footer, 7.0, BuiltinFont::Helvetica, MUTED);
            ops.append(&mut self.ops);
            pages.push(PdfPage::new(Mm(PAGE_W * 25.4 / 72.0), Mm(PAGE_H * 25.4 / 72.0), ops));
        }
        let mut doc = PdfDocument::new(&title);
        if let Ok(t) = printpdf::DateTime::from_unix_timestamp(created) {
            doc.metadata.info.creation_date = t;
            doc.metadata.info.modification_date = t;
            doc.metadata.info.metadata_date = t;
        }
        doc.metadata.info.creator = format!("benchit-panel {}", env!("CARGO_PKG_VERSION"));
        let mut warnings = Vec::new();
        doc.with_pages(pages).save(&PdfSaveOptions::default(), &mut warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_only() {
        assert_eq!(ascii("45°C · ok — x\tμ"), "45C - ok - x ?");
    }

    #[test]
    fn long_documents_break_pages() {
        let mut d = Doc::new("t");
        for i in 0..200 {
            d.mono(&format!("line {i}"), 9.0);
        }
        d.chart("c", "W", &[(0.0, 1.0), (10.0, 3.0), (20.0, 2.0)], 0.0, 20.0);
        assert!(d.pages.len() >= 3, "{} pages", d.pages.len());
        let b = d.finish(1_795_000_000);
        assert!(b.starts_with(b"%PDF-"));
    }
}
