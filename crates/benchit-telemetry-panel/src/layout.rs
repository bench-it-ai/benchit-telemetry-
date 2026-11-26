//! Backend-independent panel layout. [`render`] walks the panel top to bottom against a
//! [`Canvas`] and returns the content height; with a measuring canvas this gives the window
//! height, with the egui canvas it draws. Heights never depend on text widths, so the two
//! passes always agree.
use crate::model::{Hist, Model};
use benchit_telemetry_core::model::Snapshot;

pub const W: f32 = 400.0;
pub const PAD: f32 = 16.0;
pub const IW: f32 = W - 2.0 * PAD;
const COLS: [f32; 3] = [PAD, PAD + IW / 3.0, PAD + 2.0 * IW / 3.0];
const COLW: f32 = IW / 3.0 - 8.0;

pub type Rgb = [f32; 3];
pub const FG: Rgb = [0.92, 0.93, 0.94];
pub const DIM: Rgb = [0.55, 0.58, 0.61];
pub const LINE: Rgb = [0.20, 0.22, 0.25];
pub const AMBER: Rgb = [1.0, 0.69, 0.0];
pub const RED: Rgb = [1.0, 0.25, 0.20];
pub const OK: Rgb = [0.62, 0.86, 1.0];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Face {
    Sans,
    SansMedium,
    Mono,
    MonoLight,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Align {
    Left,
    Right,
    Center,
}

#[derive(Clone, Copy, Debug)]
pub struct Style {
    /// Size in points (1 pt = 4/3 px).
    pub pt: f32,
    pub face: Face,
    pub color: Rgb,
    /// Letter spacing in em.
    pub spacing: f32,
    pub upper: bool,
    pub align: Align,
    pub max_w: Option<f32>,
}

impl Style {
    pub fn new(pt: f32, color: Rgb) -> Self {
        Self { pt, face: Face::Sans, color, spacing: 0.0, upper: true, align: Align::Left, max_w: None }
    }
    fn face(mut self, f: Face) -> Self {
        self.face = f;
        self
    }
    fn sp(mut self, s: f32) -> Self {
        self.spacing = s;
        self
    }
    fn raw(mut self) -> Self {
        self.upper = false;
        self
    }
    fn right(mut self) -> Self {
        self.align = Align::Right;
        self
    }
    fn max(mut self, w: f32) -> Self {
        self.max_w = Some(w);
        self
    }
}

pub trait Canvas {
    /// Draw text anchored at `(x, y)` (top of the line, aligned per style); returns the width.
    fn text(&mut self, x: f32, y: f32, s: &str, st: &Style) -> f32;
    /// `(width, height)` the text would take, without ellipsis.
    fn measure(&self, s: &str, st: &Style) -> (f32, f32);
    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Rgb);
    fn hline(&mut self, y: f32);
    fn dot(&mut self, x: f32, y: f32, r: f32, color: Rgb);
    /// Sparkline in the box `(x, y, w, h)`; `data` is oldest first, right-aligned.
    #[allow(clippy::too_many_arguments)]
    fn spark(&mut self, x: f32, y: f32, w: f32, h: f32, data: &[Option<f32>], lo: Option<f32>, hi: Option<f32>, c: Rgb);
}

pub fn lvl(v: Option<f64>, warn: f64, crit: f64, invert: bool) -> Rgb {
    let Some(v) = v else { return DIM };
    if invert {
        if v <= crit {
            RED
        } else if v <= warn {
            AMBER
        } else {
            FG
        }
    } else if v >= crit {
        RED
    } else if v >= warn {
        AMBER
    } else {
        FG
    }
}

fn f0(v: Option<f64>, nd: usize) -> String {
    v.map(|x| format!("{x:.nd$}")).unwrap_or_else(|| "--".into())
}

fn rate(v: Option<f64>) -> String {
    match v {
        None => "--".into(),
        Some(x) if x >= 100.0 => format!("{x:.0}"),
        Some(x) if x >= 1.0 => format!("{x:.1}"),
        Some(x) => format!("{x:.2}"),
    }
}

fn vals(h: &std::collections::VecDeque<Option<f32>>) -> Vec<Option<f32>> {
    h.iter().copied().collect()
}

struct R<'a, C: Canvas> {
    c: &'a mut C,
}

impl<C: Canvas> R<'_, C> {
    fn section(&mut self, y: f32, left: &str, right: Option<(&str, Rgb, Face)>) -> f32 {
        let lw = self.c.text(PAD, y, left, &Style::new(6.5, DIM).sp(0.3).max(IW * 0.62));
        if let Some((txt, color, face)) = right {
            let st = Style::new(6.5, color).face(face).raw().right().max((IW - lw - 12.0).max(8.0));
            self.c.text(W - PAD, y, txt, &st);
        }
        y + 14.0
    }

    /// Label, big value and unit in one of the three columns; returns the y below it.
    #[allow(clippy::too_many_arguments)]
    fn readout(&mut self, col: usize, y: f32, label: &str, value: &str, unit: &str, color: Rgb, size: f32) -> f32 {
        let x = COLS[col];
        let cw = if col < 2 { COLW } else { W - PAD - x };
        self.c.text(x, y, label, &Style::new(6.5, DIM).sp(0.25).max(cw));
        let ust = Style::new(7.0, DIM).sp(0.15);
        let uw = if unit.is_empty() { 0.0 } else { self.c.measure(unit, &ust).0 + 3.0 };
        let mut s = size;
        let vst = |pt: f32| Style::new(pt, color).face(Face::MonoLight).raw();
        while s > 10.0 && self.c.measure(value, &vst(s)).0 + uw > cw {
            s -= 1.0;
        }
        let (vw, vh) = self.c.measure(value, &vst(s));
        let box_h = (size * 1.62).floor();
        let vy = y + 12.0 + (box_h - vh);
        self.c.text(x, vy, value, &vst(s));
        if !unit.is_empty() {
            let (_, uh) = self.c.measure(unit, &ust);
            let room = (cw - vw - 3.0).max(8.0);
            self.c.text(x + vw + 3.0, vy + vh - uh - 0.18 * s * 4.0 / 3.0, unit, &ust.max(room));
        }
        y + 12.0 + box_h + 4.0
    }
}

fn short_cpu(m: &str) -> String {
    benchit_telemetry_core::panel::short_name(m)
}

pub fn render<C: Canvas>(c: &mut C, m: &Model, h: &Hist, compact: bool) -> f32 {
    let s: &Snapshot = m.snap;
    let mut r = R { c };
    // header
    let host = if s.host.is_empty() { "telemetry".to_string() } else { s.host.to_uppercase() };
    let upt = m.uptime_text();
    let mut uw = 0.0;
    if !upt.is_empty() {
        let st = Style::new(11.0, FG).face(Face::Mono).raw().right();
        uw = r.c.text(W - PAD, 10.0, &upt, &st);
    }
    let tmax = IW - uw - 12.0;
    let mut title = format!("{host} telemetry");
    let tst = Style::new(8.0, FG).face(Face::SansMedium).sp(0.25);
    if r.c.measure(&title, &tst).0 > tmax {
        title = host.clone();
    }
    r.c.text(PAD, 13.0, &title, &tst.max(tmax));
    let (st_txt, st_col) = if m.stale() {
        ("LINK LOST", RED)
    } else if s.flags.is_empty() {
        ("NOMINAL", FG)
    } else {
        ("CAUTION", AMBER)
    };
    r.c.dot(PAD + 3.0, 37.0, 2.5, st_col);
    let sw = r.c.text(PAD + 11.0, 31.0, st_txt, &Style::new(6.5, st_col).sp(0.3));
    let mut right = m.clock.clone();
    if let Some(rec) = m.rec {
        right = format!("REC {:.0}S · {} ROWS   {}", rec.interval_s, group(rec.rows), right);
    }
    r.c.text(W - PAD, 31.0, &right, &Style::new(6.5, DIM).sp(0.15).right().max(IW - sw - 24.0));
    r.c.hline(46.0);
    let mut y = 56.0;

    // CPU
    y = r.section(y, &format!("cpu  ·  {}", short_cpu(s.cpu.model.as_deref().unwrap_or("cpu"))), None);
    let yy = r.readout(0, y, "temp", &f0(s.cpu.temp_c, 0), "°C", lvl(s.cpu.temp_c, 85.0, 95.0, false), 22.0);
    match s.cpu.pkg_w {
        Some(w) => {
            r.readout(1, y, "pkg power", &f0(Some(w), 0), "W", FG, 22.0);
        }
        None => {
            r.readout(1, y, "pkg power", "n/a", "root", DIM, 22.0);
        }
    }
    r.readout(2, y, "load", &f0(s.cpu.util_pct, 0), "%", FG, 22.0);
    y = yy;
    if !compact {
        let mut bits = vec![format!("{} MHz", s.cpu.mhz.unwrap_or(0.0) as i64)];
        if let Some(t) = s.cpu.temp2_c {
            bits.push(format!("{} {:.1}°", if s.cpu.temp3_c.is_some() { "CCD1" } else { "HOT" }, t));
        }
        if let Some(t) = s.cpu.temp3_c {
            bits.push(format!("CCD2 {t:.1}°"));
        }
        bits.push(format!("LA {}", s.cpu.load1.map(|l| l.to_string()).unwrap_or_else(|| "--".into())));
        r.c.text(PAD, y, &bits.join("   "), &Style::new(6.5, DIM).face(Face::Mono).raw().max(IW));
        y += 14.0;
        let hot = s.cpu.temp_c.is_some_and(|t| t >= 85.0);
        let col = if hot { lvl(s.cpu.temp_c, 85.0, 95.0, false) } else { OK };
        r.c.spark(PAD, y, IW, 20.0, &vals(&h.cpu_temp), Some(30.0), Some(100.0), col);
        y += 28.0;
    }
    let fans = if s.fans.is_empty() {
        ("no sensor".to_string(), DIM)
    } else {
        (s.fans.iter().map(|f| format!("{} {}", f.name, f.rpm)).collect::<Vec<_>>().join(", "), FG)
    };
    y = r.section(y, "fans  ·  rpm", Some((&fans.0, fans.1, Face::Sans)));
    r.c.hline(y);
    y += 10.0;

    // GPUs
    for g in &s.gpus {
        let (vu, vt) = (g.vram_used_mb.unwrap_or(0.0), g.vram_total_mb.unwrap_or(0.0));
        let right = format!("VRAM {:.1} / {:.0} GB", vu / 1024.0, vt / 1024.0);
        y = r.section(
            y,
            &format!("gpu {}  ·  {}", g.idx, benchit_telemetry_core::panel::short_name(&g.name)),
            Some((&right, DIM, Face::Mono)),
        );
        let lim = g.power_limit_w.unwrap_or(0.0);
        let pw = g.power_w.unwrap_or(0.0);
        let pcol = if lim > 0.0 { lvl(Some(pw / lim * 100.0), 95.0, 101.0, false) } else { FG };
        let size = if compact { 16.0 } else { 18.0 };
        let yy = r.readout(0, y, "temp", &f0(g.temp_c, 0), "°C", lvl(g.temp_c, 83.0, 88.0, false), size);
        let unit = if lim > 0.0 { format!("/{lim:.0} W") } else { "W".into() };
        r.readout(1, y, "power", &f0(Some(pw), 0), &unit, pcol, size);
        r.readout(2, y, "util", &f0(g.util_pct, 0), "%", FG, size);
        y = yy;
        let frac = if vt > 0.0 { (vu / vt).min(1.0) } else { 0.0 };
        r.c.rect(PAD, y, IW, 3.0, LINE);
        let bar = if frac > 0.92 { lvl(Some(frac * 100.0), 92.0, 98.0, false) } else { OK };
        r.c.rect(PAD, y, IW * frac as f32, 3.0, bar);
        y += 8.0;
        if !compact {
            let mut bits = Vec::new();
            if let Some(v) = g.sm_mhz {
                bits.push(format!("SM {v:.0} MHz"));
            }
            if let Some(v) = g.mem_mhz {
                bits.push(format!("MEM {v:.0} MHz"));
            }
            if let Some(v) = g.fan_pct {
                bits.push(format!("FAN {v:.0}%"));
            }
            if let Some(p) = &g.pstate {
                bits.push(p.clone());
            }
            if !bits.is_empty() {
                r.c.text(PAD, y, &bits.join("   "), &Style::new(6.5, DIM).face(Face::Mono).raw().max(IW));
                y += 14.0;
            }
            let data = h.gpu_power.get(&g.idx).map(vals).unwrap_or_default();
            r.c.spark(PAD, y, IW, 14.0, &data, Some(0.0), (lim > 0.0).then_some(lim as f32), OK);
            y += 22.0;
        }
        r.c.hline(y);
        y += 10.0;
    }

    // Memory
    let used = s.mem.used_pct;
    let avail_pct = used.map(|u| 100.0 - u);
    let swu = s.mem.swap_used_mb.unwrap_or(0.0) / 1024.0;
    y = r.section(y, "memory", None);
    let yy = r.readout(0, y, "ram used", &f0(used, 0), "%", lvl(avail_pct, 10.0, 5.0, true), 22.0);
    r.readout(
        1,
        y,
        "avail",
        &f0(s.mem.avail_mb.map(|v| v / 1024.0), 0),
        &format!("/{:.0} GB", s.mem.total_mb.unwrap_or(0.0) / 1024.0),
        lvl(avail_pct, 10.0, 5.0, true),
        22.0,
    );
    r.readout(2, y, "swap used", &f0(Some(swu), 1), "GB", if swu > 4.0 { AMBER } else { FG }, 22.0);
    y = yy;
    if !compact {
        let (mf, si, so) = (s.mem.majflt_s.unwrap_or(0.0), s.mem.swpin_s.unwrap_or(0.0), s.mem.swpout_s.unwrap_or(0.0));
        let pres = mf + si + so;
        let col = if pres > 5000.0 {
            RED
        } else if pres > 500.0 {
            AMBER
        } else {
            DIM
        };
        let txt = format!("MAJFLT {mf:.0}/s   SWPIN {si:.0}/s   SWPOUT {so:.0}/s");
        r.c.text(PAD, y, &txt, &Style::new(6.5, col).face(Face::Mono).raw().max(IW));
        y += 14.0;
        r.c.spark(PAD, y, IW, 14.0, &vals(&h.mem_used), Some(0.0), Some(100.0), OK);
        y += 22.0;
    }
    r.c.hline(y);
    y += 10.0;

    // Storage + network
    if let Some(root) = s.fs("root") {
        y = r.section(y, "storage  ·  network", None);
        let (dv, du) = match s.fs("home") {
            Some(hm) => (hm.used_pct, "% /home"),
            None => (root.used_pct, "% /"),
        };
        let yy = r.readout(0, y, "disk used", &f0(Some(dv), 0), du, lvl(Some(dv), 85.0, 95.0, false), 16.0);
        let io = s.disk.read_mbs.unwrap_or(0.0) + s.disk.write_mbs.unwrap_or(0.0);
        r.readout(1, y, "disk r+w", &rate(Some(io)), "MB/s", FG, 16.0);
        let net = s.net.rx_mbs.unwrap_or(0.0) + s.net.tx_mbs.unwrap_or(0.0);
        r.readout(2, y, "net rx+tx", &rate(Some(net)), "MB/s", FG, 16.0);
        y = yy;
        if !compact {
            let mut parts = vec![format!("/ {:.0}/{:.0} GB", root.used_gb, root.total_gb)];
            if let Some(hm) = s.fs("home") {
                parts.push(format!("/home {:.0}/{:.0} GB", hm.used_gb, hm.total_gb));
            }
            parts.push(format!("R {} W {}", rate(s.disk.read_mbs), rate(s.disk.write_mbs)));
            parts.push(format!("RX {} TX {}", rate(s.net.rx_mbs), rate(s.net.tx_mbs)));
            r.c.text(PAD, y, &parts.join("   "), &Style::new(6.5, DIM).face(Face::Mono).raw().max(IW));
            y += 14.0;
        }
        r.c.hline(y);
        y += 10.0;
    }

    // Battery
    if let Some(b) = &s.battery {
        if b.pct.is_some() && b.status.as_deref().is_some_and(|x| !x.is_empty()) {
            let ac = match b.ac_online {
                Some(true) => "AC online",
                Some(false) => "on battery",
                None => "",
            };
            let right = format!("{}  {}", b.status.as_deref().unwrap_or(""), ac).trim().to_string();
            y = r.section(y, "battery", Some((&right, DIM, Face::Sans)));
            let yy = r.readout(0, y, "charge", &f0(b.pct, 0), "%", lvl(b.pct, 20.0, 10.0, true), 18.0);
            if let Some(w) = b.watts {
                let out = b.status.as_deref().is_some_and(|x| x.eq_ignore_ascii_case("discharging")) || w < 0.0;
                r.readout(
                    1,
                    y,
                    "power",
                    &f0(Some(w.abs()), 1),
                    if out { "W out" } else { "W" },
                    if out { AMBER } else { FG },
                    18.0,
                );
            }
            y = yy;
            r.c.hline(y);
            y += 10.0;
        }
    }

    // Power total
    let tot = s.gpu_total_w.unwrap_or(0.0) + s.cpu.pkg_w.unwrap_or(0.0);
    let label = if s.cpu.pkg_w.is_some() { "cpu + gpus" } else { "gpus only" };
    let plat = s.cpu.platform_w.map(|w| format!("platform {w:.0} W"));
    y = r.section(y, "power  ·  est. total", plat.as_deref().map(|p| (p, DIM, Face::Mono)));
    let yy = r.readout(0, y, label, &f0(Some(tot), 0), "W", FG, 22.0);
    if !compact {
        r.c.spark(COLS[1], y + 14.0, W - PAD - COLS[1], 26.0, &vals(&h.total_w), Some(0.0), None, OK);
    }
    y = yy;

    // footer
    let flags = s.flags.join(" ");
    let mut fw = 0.0;
    if !flags.is_empty() {
        fw = r.c.text(W - PAD, y, &flags, &Style::new(6.5, RED).face(Face::Mono).raw().right().max(IW * 0.6));
    }
    let top = s.top_proc.clone().unwrap_or_default();
    r.c.text(PAD, y, &format!("TOP {top}"), &Style::new(6.5, DIM).face(Face::Mono).raw().max(IW - fw - 10.0));
    y += 14.0;
    y + 8.0
}

/// 12345 -> "12,345"
pub fn group(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        #[allow(clippy::manual_is_multiple_of)]
        let at_group = i > 0 && (s.len() - i) % 3 == 0;
        if at_group {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

/// Canvas that only measures: text sizes are estimated, nothing is drawn.
pub struct Measure;

impl Canvas for Measure {
    fn text(&mut self, _: f32, _: f32, s: &str, st: &Style) -> f32 {
        self.measure(s, st).0
    }
    fn measure(&self, s: &str, st: &Style) -> (f32, f32) {
        let px = st.pt * 4.0 / 3.0;
        let adv = match st.face {
            Face::Mono | Face::MonoLight => 0.6,
            _ => 0.55,
        };
        (s.chars().count() as f32 * (px * adv + st.spacing * px), px * 1.3)
    }
    fn rect(&mut self, _: f32, _: f32, _: f32, _: f32, _: Rgb) {}
    fn hline(&mut self, _: f32) {}
    fn dot(&mut self, _: f32, _: f32, _: f32, _: Rgb) {}
    #[allow(clippy::too_many_arguments)]
    fn spark(&mut self, _: f32, _: f32, _: f32, _: f32, _: &[Option<f32>], _: Option<f32>, _: Option<f32>, _: Rgb) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use benchit_telemetry_core::model::*;

    fn model(snap: &Snapshot) -> Model<'_> {
        Model { snap, rec: None, age: 1.0, clock: "08:00:00 PST".into() }
    }

    fn snap(gpus: usize) -> Snapshot {
        Snapshot {
            host: "box".into(),
            cpu: Cpu { model: Some("AMD Ryzen 9".into()), temp_c: Some(50.0), ..Default::default() },
            gpus: (0..gpus as u32).map(|idx| Gpu { idx, name: "GPU".into(), ..Default::default() }).collect(),
            disk: Disk {
                filesystems: vec![Filesystem {
                    role: "root".into(),
                    path: "/".into(),
                    used_gb: 1.0,
                    total_gb: 2.0,
                    used_pct: 50.0,
                }],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn height_grows_with_gpus_and_shrinks_in_compact() {
        let h = Hist::default();
        let (a, b) = (snap(1), snap(2));
        let h1 = render(&mut Measure, &model(&a), &h, false);
        let h2 = render(&mut Measure, &model(&b), &h, false);
        let hc = render(&mut Measure, &model(&b), &h, true);
        assert!(h2 > h1 + 60.0, "{h1} {h2}");
        assert!(hc < h2 - 80.0, "{hc} {h2}");
    }

    #[test]
    fn grouping() {
        assert_eq!(group(0), "0");
        assert_eq!(group(1234567), "1,234,567");
        assert_eq!(group(999), "999");
    }
}
