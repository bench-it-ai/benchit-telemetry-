//! `benchit-panel`: always-on-top desktop telemetry panel.
//!
//! Display only: it reads the feed written by `benchit-telemetry daemon` (via
//! `benchit-telemetry-core`) and seeds its sparklines from the history database.
//! Left-drag moves, right-click toggles compact mode (remembered), middle-click quits.
//! It runs on X11 (XWayland under GNOME) because Wayland clients cannot position windows, and
//! renders with Mesa software GL so it never touches a benchmark GPU (`BENCHIT_PANEL_GPU=1`
//! opts out).
mod gui;
mod layout;
mod model;

use anyhow::Result;
use benchit_telemetry_core::panel::{read_feed, Feed};
use benchit_telemetry_core::{paths, timefmt};
use clap::Parser;
use eframe::egui::{self, Pos2, Rect, Vec2, ViewportCommand};
use layout::{Measure, W};
use model::{local_clock, Config, Hist, Model};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const MARGIN: f32 = 12.0;

#[derive(Parser)]
#[command(name = "benchit-panel", version, about = "Desktop telemetry panel for benchit-telemetry")]
struct Args {
    /// Data directory (default: $BENCHIT_TELEMETRY_DIR or ~/.local/state/benchit-telemetry)
    #[arg(long, env = "BENCHIT_TELEMETRY_DIR")]
    dir: Option<PathBuf>,
    /// Render one frame to this PNG and exit.
    #[arg(long)]
    snapshot: Option<PathBuf>,
    /// With --snapshot: render the compact layout.
    #[arg(long)]
    compact: bool,
    /// Print window and work-area geometry to stderr once a second.
    #[arg(long)]
    debug_geometry: bool,
}

/// `_NET_WORKAREA` of the (Xwayland) root window, in pixels.
fn x11_work_area() -> Option<Rect> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};
    let (conn, screen) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots[screen].root;
    let atom = conn.intern_atom(false, b"_NET_WORKAREA").ok()?.reply().ok()?.atom;
    let prop = conn.get_property(false, root, atom, AtomEnum::CARDINAL, 0, 4).ok()?.reply().ok()?;
    let v: Vec<u32> = prop.value32()?.collect();
    (v.len() >= 4)
        .then(|| Rect::from_min_size(Pos2::new(v[0] as f32, v[1] as f32), Vec2::new(v[2] as f32, v[3] as f32)))
}

struct Panel {
    dir: PathBuf,
    cfg: Config,
    cfg_path: PathBuf,
    feed: Option<Feed>,
    hist: Hist,
    compact: bool,
    auto_compact: bool,
    height: f32,
    work: Option<Rect>,
    work_checked: Instant,
    last_tick: Option<Instant>,
    clock: String,
    host: String,
    title_host: String,
    pos_seen: Option<(Pos2, Instant)>,
    snapshot: Option<PathBuf>,
    shot: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<egui::ColorImage>>>>,
    frames: u32,
    debug: bool,
}

impl Panel {
    fn model(&self) -> Option<Model<'_>> {
        let f = self.feed.as_ref()?;
        Some(Model {
            snap: &f.snapshot,
            rec: f.rec.as_ref(),
            age: (timefmt::now_f64() - f.snapshot.ts as f64).max(0.0),
            clock: self.clock.clone(),
        })
    }

    fn content_height(&self, compact: bool) -> f32 {
        match self.model() {
            Some(m) => layout::render(&mut Measure, &m, &self.hist, compact).ceil(),
            None => 120.0,
        }
    }

    /// Choose the layout (full, or compact if full would not fit) and return the window height.
    fn fit_height(&mut self, max_h: f32) -> f32 {
        let want = self.compact;
        let mut h = self.content_height(want);
        self.auto_compact = false;
        if !want && h > max_h {
            self.auto_compact = true;
            h = self.content_height(true);
        }
        h
    }

    fn refresh(&mut self) {
        if let Some(f) = read_feed(&self.dir) {
            let gap = f.rec.as_ref().map(|r| r.interval_s).filter(|g| *g > 0.0).unwrap_or(10.0);
            self.hist.observe(&f.snapshot, gap);
            self.host = f.snapshot.host.clone();
            self.feed = Some(f);
        }
        self.clock = local_clock();
    }

    fn max_height(&self, ppp: f32, monitor: Option<Vec2>) -> f32 {
        let wa = self.work.map(|r| r.height() / ppp).or(monitor.map(|m| m.y - 32.0)).unwrap_or(1000.0);
        wa - 2.0 * MARGIN
    }

    fn work_rect(&self, ppp: f32, monitor: Option<Vec2>) -> Rect {
        match self.work {
            Some(r) => Rect::from_min_max(r.min / ppp, r.max / ppp),
            None => Rect::from_min_size(
                Pos2::new(0.0, 32.0),
                monitor.unwrap_or(Vec2::new(1920.0, 1080.0)) - Vec2::new(0.0, 32.0),
            ),
        }
    }

    fn save_cfg(&self) {
        self.cfg.save_to(&self.cfg_path);
    }
}

impl eframe::App for Panel {
    fn clear_color(&self, _v: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.frames += 1;
        let ppp = ctx.pixels_per_point();
        let (outer, monitor, dragging, secondary, middle) = ctx.input(|i| {
            let vp = i.viewport();
            (
                vp.outer_rect,
                vp.monitor_size,
                i.pointer.primary_down(),
                i.pointer.button_pressed(egui::PointerButton::Secondary),
                i.pointer.button_pressed(egui::PointerButton::Middle),
            )
        });

        if self.last_tick.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
            self.last_tick = Some(Instant::now());
            self.refresh();
            if self.work_checked.elapsed() > Duration::from_secs(15) {
                self.work = x11_work_area().or(self.work);
                self.work_checked = Instant::now();
            }
        }

        if secondary {
            self.compact = !self.compact;
            self.cfg.compact = self.compact;
            self.save_cfg();
        }
        if middle {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }

        let max_h = self.max_height(ppp, monitor);
        let h = self.fit_height(max_h);
        if self.height != h {
            self.height = h;
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(W, h)));
        }
        // only on change: every viewport command schedules another frame
        if !self.host.is_empty() && self.title_host != self.host {
            self.title_host = self.host.clone();
            ctx.send_viewport_cmd(ViewportCommand::Title(format!("{} telemetry", self.host)));
        }

        // keep the window inside the work area and remember where the user put it
        if let (Some(o), false) = (outer, dragging) {
            let wa = self.work_rect(ppp, monitor);
            let target = Pos2::new(
                o.min.x.clamp(wa.min.x, (wa.max.x - W).max(wa.min.x)),
                o.min.y.clamp(wa.min.y, (wa.max.y - o.height()).max(wa.min.y)),
            );
            if (target - o.min).length() > 1.0 && self.snapshot.is_none() {
                ctx.send_viewport_cmd(ViewportCommand::OuterPosition(target));
            }
            match self.pos_seen {
                Some((p, t)) if (p - o.min).length() < 0.5 => {
                    if t.elapsed() > Duration::from_millis(1500)
                        && (self.cfg.x, self.cfg.y) != (Some(o.min.x), Some(o.min.y))
                        && self.snapshot.is_none()
                    {
                        self.cfg.x = Some(o.min.x);
                        self.cfg.y = Some(o.min.y);
                        self.save_cfg();
                    }
                }
                _ => self.pos_seen = Some((o.min, Instant::now())),
            }
            if self.debug {
                eprintln!(
                    "outer={:?} size={}x{} work={:?} compact={}{}",
                    o.min,
                    o.width(),
                    o.height(),
                    wa,
                    self.compact,
                    if self.auto_compact { " (auto)" } else { "" }
                );
            }
        }
        if ctx.input(|i| i.pointer.primary_pressed()) {
            ctx.send_viewport_cmd(ViewportCommand::StartDrag);
        }

        let painter = ctx.layer_painter(egui::LayerId::background());
        gui::draw_frame(&painter, self.height);
        match self.model() {
            Some(m) => {
                let mut g = gui::Gui { painter: &painter };
                layout::render(&mut g, &m, &self.hist, self.compact || self.auto_compact);
            }
            None => {
                let mut g = gui::Gui { painter: &painter };
                use layout::Canvas;
                g.text(
                    W / 2.0,
                    50.0,
                    "awaiting telemetry",
                    &layout::Style { align: layout::Align::Center, ..layout::Style::new(8.0, layout::DIM) },
                );
            }
        }

        if let Some(path) = self.snapshot.clone() {
            if self.frames == 6 {
                let slot = self.shot.clone();
                ctx.request_screenshot(move |img| {
                    if let Ok(mut g) = slot.lock() {
                        *g = Some(img);
                    }
                });
            }
            let shot = self.shot.lock().ok().and_then(|mut g| g.take());
            if let Some(img) = shot {
                match save_png(&path, &img) {
                    Ok(()) => println!("wrote {} {}x{}", path.display(), img.size[0], img.size[1]),
                    Err(e) => eprintln!("snapshot failed: {e:#}"),
                }
                ctx.send_viewport_cmd(ViewportCommand::Close);
            } else if self.frames > 120 {
                eprintln!("snapshot timed out");
                std::process::exit(1);
            }
            ctx.request_repaint_after(Duration::from_millis(60));
        } else {
            ctx.request_repaint_after(Duration::from_millis(1000));
        }
    }
}

/// Write `img` composited over a dark backdrop (the window itself is translucent).
fn save_png(path: &std::path::Path, img: &egui::ColorImage) -> Result<()> {
    let mut rgb = Vec::with_capacity(img.pixels.len() * 3);
    let back = [31u32, 33, 38];
    for p in &img.pixels {
        let a = p.a() as u32;
        for (c, b) in [p.r(), p.g(), p.b()].into_iter().zip(back) {
            // egui pixels are premultiplied
            rgb.push((c as u32 + b * (255 - a) / 255).min(255) as u8);
        }
    }
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, img.size[0] as u32, img.size[1] as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&rgb)?;
    Ok(())
}

fn prefer_x11_software() {
    // Single-threaded here (before the event loop), so mutating the environment is fine.
    std::env::set_var("WINIT_UNIX_BACKEND", "x11");
    std::env::remove_var("WAYLAND_DISPLAY");
    if std::env::var_os("BENCHIT_PANEL_GPU").is_none() {
        std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");
        std::env::set_var("__GLX_VENDOR_LIBRARY_NAME", "mesa");
        let mesa = "/usr/share/glvnd/egl_vendor.d/50_mesa.json";
        if std::path::Path::new(mesa).exists() && std::env::var_os("__EGL_VENDOR_LIBRARY_FILENAMES").is_none() {
            std::env::set_var("__EGL_VENDOR_LIBRARY_FILENAMES", mesa);
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    prefer_x11_software();
    let dir = args.dir.unwrap_or_else(paths::default_dir);
    let cfg_path = Config::path();
    let mut cfg = Config::load_from(&cfg_path);
    if args.snapshot.is_some() {
        cfg.compact = args.compact;
    }
    let hist = Hist::seed(&dir.join(paths::DB_FILE));
    let work = x11_work_area();
    let mut panel = Panel {
        dir,
        compact: cfg.compact,
        cfg,
        cfg_path,
        feed: None,
        hist,
        auto_compact: false,
        height: 0.0,
        work,
        work_checked: Instant::now(),
        last_tick: None,
        clock: String::new(),
        host: String::new(),
        title_host: String::new(),
        pos_seen: None,
        snapshot: args.snapshot,
        shot: Default::default(),
        frames: 0,
        debug: args.debug_geometry,
    };
    panel.refresh();
    let max_h = work.map(|r| r.height() - 2.0 * MARGIN).unwrap_or(1000.0);
    let h = panel.fit_height(max_h);
    panel.height = h;
    let start = match (panel.cfg.x, panel.cfg.y, work) {
        (Some(x), Some(y), Some(wa)) => {
            Pos2::new(x.clamp(wa.min.x, (wa.max.x - W).max(wa.min.x)), y.clamp(wa.min.y, (wa.max.y - h).max(wa.min.y)))
        }
        (Some(x), Some(y), None) => Pos2::new(x, y),
        (_, _, Some(wa)) => Pos2::new(wa.max.x - W - 24.0, wa.min.y + 24.0),
        _ => Pos2::new(100.0, 100.0),
    };
    let title = if panel.host.is_empty() { "telemetry".to_string() } else { format!("{} telemetry", panel.host) };
    let viewport = egui::ViewportBuilder::default()
        .with_title(title)
        .with_app_id("benchit-panel")
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(false)
        .with_always_on_top()
        .with_inner_size([W, h])
        .with_position(start)
        .with_window_type(egui::X11WindowType::Utility);
    let opts = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "benchit-panel",
        opts,
        Box::new(|cc| {
            gui::install_fonts(&cc.egui_ctx);
            Ok(Box::new(panel))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}
