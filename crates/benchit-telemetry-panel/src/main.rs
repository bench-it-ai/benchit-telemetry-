//! `benchit-panel`: always-on-top desktop telemetry panel.
//!
//! Display only: it reads the feed written by `benchit-telemetry daemon` (via
//! `benchit-telemetry-core`) and seeds its sparklines from the history database.
//! Left-drag moves, middle-click quits, right-click opens a menu: Compact (remembered), Hide
//! (`benchit-panel --show` brings it back), Copy stats, Export CSV / JSON and PDF report over
//! 1h/24h/7d/30d (saved under `~/benchit-reports/`, see [`report`]) and Quit.
//! `benchit-panel --export csv|json|pdf [--range 24h]` writes the same files without a window.
//! It runs on X11 (XWayland under GNOME) because Wayland clients cannot position windows, and
//! renders with Mesa software GL so it never touches a benchmark GPU (`BENCHIT_PANEL_GPU=1`
//! opts out).
mod gui;
mod layout;
mod menu;
mod model;
mod pdf;
mod report;

use anyhow::Result;
use benchit_telemetry_core::panel::{read_feed, Feed};
use benchit_telemetry_core::{paths, timefmt};
use clap::Parser;
use eframe::egui::{self, Pos2, Rect, Vec2, ViewportCommand};
use layout::{Measure, W};
use model::{local_clock, Config, Hist, Model};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
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
    /// With --snapshot: render with the right-click menu open.
    #[arg(long)]
    menu: bool,
    /// Print window and work-area geometry to stderr once a second.
    #[arg(long)]
    debug_geometry: bool,
    /// Write an export (csv, json or pdf) of the live snapshot and stored history, print its
    /// path and exit (no window).
    #[arg(long, value_name = "FORMAT", value_parser = parse_format)]
    export: Option<report::Format>,
    /// With --export: history window (1h, 24h, 7d, 30d, or any <n>h / <n>d).
    #[arg(long, default_value = "24h", value_parser = parse_range)]
    range: benchit_telemetry_core::ExportRange,
    /// With --export: output directory (default: $BENCHIT_REPORTS_DIR or ~/benchit-reports).
    #[arg(long)]
    out_dir: Option<PathBuf>,
    /// Show a running panel that was hidden from its menu, then exit.
    #[arg(long)]
    show: bool,
}

fn parse_format(s: &str) -> Result<report::Format, String> {
    report::Format::parse(s).ok_or_else(|| format!("unknown format {s:?} (csv, json or pdf)"))
}

fn parse_range(s: &str) -> Result<benchit_telemetry_core::ExportRange, String> {
    report::parse_range(s).ok_or_else(|| format!("bad range {s:?} (e.g. 1h, 24h, 7d, 30d)"))
}

/// Set by SIGUSR1 (`benchit-panel --show`): un-hide the window.
static SHOW: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigusr1(_: libc::c_int) {
    SHOW.store(true, Ordering::SeqCst);
}

/// `$XDG_RUNTIME_DIR/benchit-panel.pid` (or `/tmp/benchit-panel-<uid>.pid`).
fn pid_file() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d).join("benchit-panel.pid"),
        // SAFETY: getuid has no preconditions.
        None => std::env::temp_dir().join(format!("benchit-panel-{}.pid", unsafe { libc::getuid() })),
    }
}

/// `--show`: signal the panel named in the pid file (only if that pid really is benchit-panel).
fn signal_show() -> Result<()> {
    let p = pid_file();
    let pid: i32 = std::fs::read_to_string(&p)
        .map_err(|e| anyhow::anyhow!("no running panel ({}: {e})", p.display()))?
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("bad pid file {}", p.display()))?;
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
    if !comm.trim().starts_with("benchit-panel") {
        anyhow::bail!("no running panel (pid {pid} from {} is not benchit-panel)", p.display());
    }
    // SAFETY: plain kill(2) with a validated pid.
    if unsafe { libc::kill(pid, libc::SIGUSR1) } != 0 {
        anyhow::bail!("signal pid {pid}: {}", std::io::Error::last_os_error());
    }
    Ok(())
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

/// Ask the window manager to keep this process's windows above others and on every workspace.
///
/// `ViewportBuilder::with_always_on_top()` is not enough on GNOME/XWayland: mutter ignores the
/// `_NET_WM_STATE_ABOVE` property when it is set before the window is mapped, so the panel
/// stays in the normal stack (hidden behind maximized apps) and only on the workspace where it
/// was created. The old GTK panel did this with `set_keep_above(True)` and `stick()`. A native
/// Wayland egui window would not help: GNOME has no layer-shell, and Wayland clients cannot
/// position themselves. Returns whether at least one of our windows now has `_NET_WM_STATE_ABOVE`.
fn x11_apply_panel_hints() -> bool {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{
        AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt, EventMask, StackMode, CLIENT_MESSAGE_EVENT,
    };
    let Ok((conn, screen)) = x11rb::connect(None) else { return false };
    let root = conn.setup().roots[screen].root;
    let intern = |n: &[u8]| conn.intern_atom(false, n).ok().and_then(|c| c.reply().ok()).map(|r| r.atom);
    let (Some(net_client), Some(net_pid), Some(net_state), Some(net_desk), Some(above), Some(sticky)) = (
        intern(b"_NET_CLIENT_LIST"),
        intern(b"_NET_WM_PID"),
        intern(b"_NET_WM_STATE"),
        intern(b"_NET_WM_DESKTOP"),
        intern(b"_NET_WM_STATE_ABOVE"),
        intern(b"_NET_WM_STATE_STICKY"),
    ) else {
        return false;
    };
    let Ok(prop) = conn.get_property(false, root, net_client, AtomEnum::WINDOW, 0, 256) else { return false };
    let Ok(prop) = prop.reply() else { return false };
    let Some(wins) = prop.value32() else { return false };
    let me = std::process::id();
    let mut ok = false;
    for w in wins {
        let pid = conn
            .get_property(false, w, net_pid, AtomEnum::CARDINAL, 0, 1)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|p| p.value32().and_then(|mut v| v.next()));
        if pid != Some(me) {
            continue;
        }
        // EWMH requires a ClientMessage to the root (setting the property ourselves is ignored).
        let send = |ty, data: [u32; 5]| {
            let ev = ClientMessageEvent {
                response_type: CLIENT_MESSAGE_EVENT,
                format: 32,
                sequence: 0,
                window: w,
                type_: ty,
                data: data.into(),
            };
            let mask = EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT;
            conn.send_event(false, root, mask, ev).ok()
        };
        // 1 = _NET_WM_STATE_ADD; source indication 1 = application
        let _ = send(net_state, [1, above, sticky, 1, 0]);
        let _ = send(net_desk, [0xFFFF_FFFF, 1, 0, 0, 0]);
        let _ = conn.configure_window(w, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
        let _ = conn.flush();
        let has_above = conn
            .get_property(false, w, net_state, AtomEnum::ATOM, 0, 16)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|p| p.value32().map(|v| v.collect::<Vec<_>>()))
            .is_some_and(|v| v.contains(&above));
        ok |= has_above;
    }
    ok
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
    /// Last time we (re)applied always-on-top / sticky hints.
    hints_at: Option<Instant>,
    /// Open context menu: where it was opened, and its last drawn rectangle.
    menu: Option<(Pos2, Option<Rect>)>,
    /// Status line shown at the bottom for a few seconds.
    toast: Option<(String, Instant)>,
    /// Export being written in the background.
    job: Option<mpsc::Receiver<Result<PathBuf, String>>>,
    hidden: bool,
    reports_dir: PathBuf,
    /// Temperature unit for copied stats and reports (`panel.toml`).
    unit: benchit_telemetry_core::TempUnit,
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

    fn say(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        eprintln!("benchit-panel: {msg}");
        self.toast = Some((msg, Instant::now()));
    }

    fn act(&mut self, ctx: &egui::Context, a: menu::Action) {
        self.menu = None;
        match a {
            menu::Action::ToggleCompact => {
                self.compact = !self.compact;
                self.cfg.compact = self.compact;
                self.save_cfg();
            }
            menu::Action::Hide => {
                self.hidden = true;
                eprintln!("benchit-panel: hidden; `benchit-panel --show` brings it back");
                ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            }
            menu::Action::CopyStats => match self.feed.as_ref() {
                Some(f) => {
                    ctx.copy_text(report::stats_text(&f.snapshot, self.unit));
                    self.say("stats copied to the clipboard");
                }
                None => self.say("no telemetry to copy yet"),
            },
            menu::Action::Export(format, range) => {
                if self.job.is_some() {
                    return;
                }
                let req = report::Request {
                    format,
                    range,
                    data_dir: self.dir.clone(),
                    out_dir: self.reports_dir.clone(),
                    now: timefmt::now(),
                    snapshot: self.feed.as_ref().map(|f| f.snapshot.clone()),
                    rec: self.feed.as_ref().and_then(|f| f.rec.clone()),
                    unit: self.unit,
                };
                let (tx, rx) = mpsc::channel();
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    let res = report::run(&req).map_err(|e| format!("{e:#}"));
                    if let Ok(p) = &res {
                        report::notify(&format!("benchit {} saved", req.format.ext()), &p.display().to_string());
                    }
                    let _ = tx.send(res);
                    ctx.request_repaint();
                });
                self.job = Some(rx);
                self.say(format!("writing {} ({})...", format.ext(), range.tag()));
            }
            menu::Action::Quit => ctx.send_viewport_cmd(ViewportCommand::Close),
        }
    }

    /// Un-hide after SIGUSR1 (`benchit-panel --show`).
    fn apply_show(&mut self, ctx: &egui::Context) {
        if SHOW.swap(false, Ordering::SeqCst) && self.hidden {
            self.hidden = false;
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            self.hints_at = None;
        }
    }

    fn poll_job(&mut self) {
        let Some(rx) = &self.job else { return };
        match rx.try_recv() {
            Ok(Ok(p)) => {
                self.job = None;
                self.say(format!("saved {}", report::display_path(&p)));
            }
            Ok(Err(e)) => {
                self.job = None;
                self.say(format!("export failed: {e}"));
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.job = None;
                self.say("export failed");
            }
        }
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
        let (outer, monitor, dragging, secondary, middle, pointer, escape) = ctx.input(|i| {
            let vp = i.viewport();
            (
                vp.outer_rect,
                vp.monitor_size,
                i.pointer.primary_down(),
                i.pointer.button_pressed(egui::PointerButton::Secondary),
                i.pointer.button_pressed(egui::PointerButton::Middle),
                i.pointer.interact_pos(),
                i.key_pressed(egui::Key::Escape),
            )
        });

        self.apply_show(ctx);
        self.poll_job();

        if self.last_tick.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
            self.last_tick = Some(Instant::now());
            self.refresh();
            if self.work_checked.elapsed() > Duration::from_secs(15) {
                self.work = x11_work_area().or(self.work);
                self.work_checked = Instant::now();
            }
        }

        // GNOME/XWayland drops pre-map always-on-top; (re)apply after the window exists
        // and every 15 s in case mutter later forgets the state.
        if self.snapshot.is_none()
            && self.hints_at.is_none_or(|t| {
                t.elapsed() > Duration::from_secs(15) || (self.frames < 30 && t.elapsed() > Duration::from_millis(200))
            })
            && (x11_apply_panel_hints() || self.frames > 8)
        {
            self.hints_at = Some(Instant::now());
        }

        if secondary {
            self.menu = match self.menu {
                Some(_) => None,
                None => Some((pointer.unwrap_or(Pos2::new(MARGIN, MARGIN)), None)),
            };
        }
        if escape {
            self.menu = None;
        }
        if middle {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }

        let max_h = self.max_height(ppp, monitor);
        let mut h = self.fit_height(max_h);
        if self.menu.is_some() {
            let menu_h = self.menu.and_then(|(_, r)| r).map_or(menu::MENU_H, |r| r.height());
            h = menu::window_height(h, menu_h).min(max_h.max(h));
        }
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
        let primary_pressed = ctx.input(|i| i.pointer.primary_pressed());
        if primary_pressed && self.menu.is_none() {
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

        if let Some((at, last)) = self.menu {
            let size = last.map_or(Vec2::new(menu::MENU_W, menu::MENU_H), |r| r.size());
            let pos = menu::place(at, size, Vec2::new(W, self.height), 8.0);
            let busy = self.job.is_some();
            let (picked, rect) = menu::show(ctx, pos, self.compact, busy);
            if let Some(m) = self.menu.as_mut() {
                m.1 = Some(rect);
            }
            match picked {
                Some(a) => self.act(ctx, a),
                // a click outside the menu closes it
                None if primary_pressed && pointer.is_some_and(|p| !rect.contains(p)) && last.is_some() => {
                    self.menu = None
                }
                None => {}
            }
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if let Some((msg, at)) = &self.toast {
            if at.elapsed() < Duration::from_secs(8) {
                menu::toast(ctx, self.height, msg);
            } else {
                self.toast = None;
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
    if args.show {
        return signal_show();
    }
    let dir = args.dir.clone().unwrap_or_else(paths::default_dir);
    if let Some(format) = args.export {
        let feed = read_feed(&dir);
        let req = report::Request {
            format,
            range: args.range,
            data_dir: dir,
            out_dir: args.out_dir.unwrap_or_else(report::reports_dir),
            now: timefmt::now(),
            snapshot: feed.as_ref().map(|f| f.snapshot.clone()),
            rec: feed.and_then(|f| f.rec),
            unit: benchit_telemetry_core::PanelPrefs::load().temp_unit,
        };
        println!("{}", report::run(&req)?.display());
        return Ok(());
    }
    prefer_x11_software();
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
        hints_at: None,
        menu: None,
        toast: None,
        job: None,
        hidden: false,
        reports_dir: report::reports_dir(),
        unit: benchit_telemetry_core::PanelPrefs::load().temp_unit,
    };
    if args.menu && panel.snapshot.is_some() {
        panel.menu = Some((Pos2::new(40.0, 60.0), None));
    }
    if panel.snapshot.is_none() {
        // SAFETY: the handler only stores to an atomic.
        unsafe {
            libc::signal(libc::SIGUSR1, on_sigusr1 as *const () as libc::sighandler_t);
        }
        let _ = std::fs::write(pid_file(), format!("{}\n", std::process::id()));
    }
    panel.refresh();
    let max_h = work.map(|r| r.height() - 2.0 * MARGIN).unwrap_or(1000.0);
    let mut h = panel.fit_height(max_h);
    if panel.menu.is_some() {
        h = menu::window_height(h, menu::MENU_H);
    }
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
            menu::style(&cc.egui_ctx);
            Ok(Box::new(panel))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use benchit_telemetry_core::model::Snapshot;

    fn panel(dir: &std::path::Path, reports: &std::path::Path) -> Panel {
        let snapshot = Snapshot { ts: timefmt::now(), host: "rig-2".into(), ..Default::default() };
        Panel {
            dir: dir.to_path_buf(),
            cfg: Config::default(),
            cfg_path: dir.join("panel.json"),
            feed: Some(Feed { snapshot, rec: None }),
            hist: Hist::default(),
            compact: false,
            auto_compact: false,
            height: 400.0,
            work: None,
            work_checked: Instant::now(),
            last_tick: None,
            clock: String::new(),
            host: String::new(),
            title_host: String::new(),
            pos_seen: None,
            snapshot: None,
            shot: Default::default(),
            frames: 0,
            debug: false,
            hints_at: None,
            menu: Some((Pos2::new(10.0, 10.0), None)),
            toast: None,
            job: None,
            hidden: false,
            reports_dir: reports.to_path_buf(),
            unit: Default::default(),
        }
    }

    /// What one frame asked of the platform and of the root viewport.
    struct Out {
        platform: Vec<egui::OutputCommand>,
        viewport: Vec<ViewportCommand>,
    }

    fn pass(ctx: &egui::Context, f: impl FnMut(&egui::Context)) -> Out {
        let out = ctx.run_pass(egui::RawInput::default(), f);
        let res = Out {
            platform: out.platform_output.commands.clone(),
            viewport: out.viewport_output.get(&egui::ViewportId::ROOT).map(|v| v.commands.clone()).unwrap_or_default(),
        };
        out.drop_without_applying_deltas();
        res
    }

    fn viewport_cmds(out: &Out) -> Vec<ViewportCommand> {
        out.viewport.clone()
    }

    #[test]
    fn copy_stats_puts_the_text_on_the_clipboard() {
        let d = tempfile::tempdir().unwrap();
        let mut p = panel(d.path(), d.path());
        let ctx = egui::Context::default();
        let out = pass(&ctx, |ctx| p.act(ctx, menu::Action::CopyStats));
        let copied: Vec<&String> = out
            .platform
            .iter()
            .filter_map(|c| match c {
                egui::OutputCommand::CopyText(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(copied.len(), 1);
        assert!(copied[0].starts_with("RIG-2"), "{}", copied[0]);
        assert!(p.menu.is_none(), "an action closes the menu");
        assert!(p.toast.as_ref().is_some_and(|(m, _)| m.contains("copied")));
    }

    #[test]
    fn toggle_compact_is_remembered() {
        let d = tempfile::tempdir().unwrap();
        let mut p = panel(d.path(), d.path());
        let ctx = egui::Context::default();
        pass(&ctx, |ctx| p.act(ctx, menu::Action::ToggleCompact));
        assert!(p.compact);
        assert!(Config::load_from(&d.path().join("panel.json")).compact);
    }

    #[test]
    fn hide_then_show_signal() {
        let d = tempfile::tempdir().unwrap();
        let mut p = panel(d.path(), d.path());
        let ctx = egui::Context::default();
        let out = pass(&ctx, |ctx| p.act(ctx, menu::Action::Hide));
        assert!(p.hidden);
        assert!(viewport_cmds(&out).contains(&ViewportCommand::Visible(false)));
        // SIGUSR1, as sent by `benchit-panel --show`
        on_sigusr1(libc::SIGUSR1);
        let out = pass(&ctx, |ctx| p.apply_show(ctx));
        assert!(!p.hidden);
        assert!(viewport_cmds(&out).contains(&ViewportCommand::Visible(true)));
    }

    #[test]
    fn quit_closes_the_window() {
        let d = tempfile::tempdir().unwrap();
        let mut p = panel(d.path(), d.path());
        let ctx = egui::Context::default();
        let out = pass(&ctx, |ctx| p.act(ctx, menu::Action::Quit));
        assert!(viewport_cmds(&out).contains(&ViewportCommand::Close));
    }

    #[test]
    fn export_runs_in_the_background_and_reports_the_path() {
        let d = tempfile::tempdir().unwrap();
        let reports = d.path().join("reports");
        let mut p = panel(d.path(), &reports);
        let ctx = egui::Context::default();
        let action = menu::Action::Export(report::Format::Json, benchit_telemetry_core::ExportRange::Hours(1));
        pass(&ctx, |ctx| p.act(ctx, action));
        assert!(p.job.is_some());
        let t0 = Instant::now();
        while p.job.is_some() && t0.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(20));
            p.poll_job();
        }
        let msg = p.toast.as_ref().map(|(m, _)| m.clone()).unwrap_or_default();
        assert!(msg.starts_with("saved "), "{msg}");
        let files: Vec<_> = std::fs::read_dir(&reports).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(files[0].path()).unwrap()).unwrap();
        assert_eq!(v["current"]["host"], "rig-2");
    }
}
