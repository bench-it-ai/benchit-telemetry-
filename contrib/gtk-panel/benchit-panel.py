#!/usr/bin/env python3
"""benchit-telemetry GTK desktop panel (content-sized layout).

Display-only: reads the flat panel feed `latest.json` written every tick by
`benchit-telemetry daemon` (the same values it stores in `metrics.db`) and seeds its
sparklines from that database. Data dir: $BENCHIT_TELEMETRY_DIR, else
~/.local/state/benchit-telemetry, else ~/telemetry.
The window height follows its content and is kept inside the monitor work area.
Left-drag: move (saved to ~/.config/benchit-panel.json). Right-click: compact. Middle-click: quit.
"""
import os, sys, json, time, socket, collections, math, re
os.environ.setdefault("GDK_BACKEND", "x11")
import gi
gi.require_version("Gtk", "3.0"); gi.require_version("PangoCairo", "1.0")
from gi.repository import Gtk, Gdk, GLib, Pango, PangoCairo
import cairo

HOME = os.path.expanduser("~")
def _data_dir():
    env = os.environ.get("BENCHIT_TELEMETRY_DIR")
    if env: return env
    for d in (os.path.join(os.environ.get("XDG_STATE_HOME", os.path.join(HOME, ".local", "state")), "benchit-telemetry"),
              os.path.join(HOME, "telemetry")):
        if os.path.exists(os.path.join(d, "latest.json")): return d
    return os.path.join(HOME, ".local", "state", "benchit-telemetry")
DATA = _data_dir()
SRC = os.path.join(DATA, "latest.json")
DB = os.path.join(DATA, "metrics.db")
CFG = os.path.join(HOME, ".config", "benchit-panel.json")
W = 400; PAD = 16; IW = W - 2 * PAD; MARGIN = 12
BG = (0.02, 0.024, 0.028, 0.90); FG = (0.92, 0.93, 0.94); DIM = (0.55, 0.58, 0.61)
LINE = (0.20, 0.22, 0.25); AMBER = (1.0, 0.69, 0.0); RED = (1.0, 0.25, 0.20); OK = (0.62, 0.86, 1.0)
SANS = "Inter"; MONO = "JetBrains Mono"
HIST = 150
COLS = [PAD, PAD + IW / 3, PAD + 2 * IW / 3]; COLW = IW / 3 - 8

def short_cpu(m):
    if not m: return "cpu"
    for p in ("AMD ", "Intel(R) ", "Intel ", "Core(TM) ", "CPU ", "Processor"): m = m.replace(p, "")
    return re.sub(r"\s+\d+-Core.*$", "", m).strip()

def short_gpu(n):
    return (n or "gpu").replace("NVIDIA GeForce ", "").replace("NVIDIA ", "").strip()

def lvl(v, warn, crit, invert=False):
    if v is None: return DIM
    if invert: return RED if v <= crit else AMBER if v <= warn else FG
    return RED if v >= crit else AMBER if v >= warn else FG

def f0(v, nd=0):
    if v is None: return "--"
    try: return f"{float(v):.{nd}f}"
    except (TypeError, ValueError): return "--"

def rate(v):
    if v is None: return "--"
    return f"{v:.0f}" if v >= 100 else f"{v:.1f}" if v >= 1 else f"{v:.2f}"

class Widget(Gtk.Window):
    def __init__(self, offscreen=False):
        super().__init__(type=Gtk.WindowType.TOPLEVEL)
        self.d = {}; self.last_ts = 0; self.H = 400
        self.h = collections.defaultdict(lambda: collections.deque(maxlen=HIST))
        self.cfg = self.load_cfg(); self.compact = bool(self.cfg.get("compact", False)); self.auto_compact = False
        self.seed_history()
        if offscreen: return
        self.set_title(f"{socket.gethostname().split('.')[0]} telemetry")
        self.set_decorated(False); self.set_keep_above(True); self.set_accept_focus(False)
        self.set_focus_on_map(False); self.set_skip_taskbar_hint(True); self.set_skip_pager_hint(True)
        self.set_type_hint(Gdk.WindowTypeHint.UTILITY); self.stick(); self.set_resizable(False)
        vis = self.get_screen().get_rgba_visual()
        if vis: self.set_visual(vis)
        self.set_app_paintable(True)
        self.da = Gtk.DrawingArea(); self.add(self.da); self.da.connect("draw", self.on_draw)
        self.add_events(Gdk.EventMask.BUTTON_PRESS_MASK)
        self.connect("button-press-event", self.on_press); self.connect("destroy", Gtk.main_quit)
        self.load(); self.fit(initial=True)
        GLib.timeout_add(1000, self.tick)

    # ---------- data ----------
    def load_cfg(self):
        for p in (CFG,):
            try: return json.load(open(p))
            except Exception: pass
        return {}
    def save_cfg(self):
        try:
            os.makedirs(os.path.dirname(CFG), exist_ok=True); json.dump(self.cfg, open(CFG, "w"))
        except Exception: pass

    def seed_history(self):
        try:
            import sqlite3
            con = sqlite3.connect(f"file:{DB}?mode=ro", uri=True, timeout=2)
            rows = con.execute("SELECT ts, cpu_temp_c, cpu_util_pct, mem_used_pct, est_total_w FROM samples "
                               "ORDER BY ts DESC LIMIT ?", (HIST,)).fetchall()[::-1]
            for ts, t, u, m, tot in rows:
                self.h["tctl"].append(t); self.h["util"].append(u); self.h["mem"].append(m); self.h["tot"].append(tot)
            if rows:
                for ts, idx, temp, pw in con.execute("SELECT ts, idx, temp_c, power_w FROM gpu_samples WHERE ts >= ? "
                                                    "ORDER BY ts", (rows[0][0],)):
                    self.h[f"g{idx}t"].append(temp); self.h[f"g{idx}p"].append(pw)
            con.close()
        except Exception: pass

    def load(self):
        try: d = json.load(open(SRC))
        except Exception: return False
        new = d.get("ts") != self.last_ts
        self.d = d
        if new and d.get("ts"):
            self.last_ts = d["ts"]
            gw = d.get("gpu_w"); cw = d.get("cpu_w")
            self.h["tctl"].append(d.get("tctl")); self.h["util"].append(d.get("cpu_util"))
            mp = d.get("mem_pct"); self.h["mem"].append(None if mp is None else 100 - mp)
            self.h["tot"].append((gw or 0) + (cw or 0))
            if d.get("bat_pct") is not None: self.h["bat"].append(d.get("bat_pct"))
            for g in d.get("gpus") or []:
                self.h[f"g{g['idx']}t"].append(g.get("temp")); self.h[f"g{g['idx']}p"].append(g.get("power"))
        return True

    # ---------- window geometry ----------
    def workarea(self):
        disp = Gdk.Display.get_default()
        x, y = self.get_position()
        mon = disp.get_monitor_at_point(x + W // 2, y + 20) or disp.get_primary_monitor() or disp.get_monitor(0)
        return mon.get_workarea()

    def measure(self):
        surf = cairo.RecordingSurface(cairo.CONTENT_COLOR_ALPHA, None)
        return int(math.ceil(self.render(cairo.Context(surf), measure=True)))

    def fit(self, initial=False):
        if initial and "x" in self.cfg: self.move(self.cfg["x"], self.cfg["y"])
        wa = self.workarea()
        H = self.measure()
        if not self.compact and H > wa.height - 2 * MARGIN:
            self.auto_compact = True; H = self.measure()
        elif self.auto_compact:
            self.auto_compact = False; H2 = self.measure()
            if H2 > wa.height - 2 * MARGIN: self.auto_compact = True
            else: H = H2
        if H != self.H or initial:
            self.H = H; self.set_size_request(W, H); self.resize(W, H)
        if initial and "x" not in self.cfg:
            x, y = wa.x + wa.width - W - 24, wa.y + 24
        else:
            x, y = self.get_position()
        nx = min(max(x, wa.x), wa.x + wa.width - W); ny = min(max(y, wa.y), wa.y + wa.height - self.H)
        if (nx, ny) != (x, y) or initial: self.move(nx, ny)

    def on_press(self, w, ev):
        if ev.button == 1:
            self.begin_move_drag(ev.button, int(ev.x_root), int(ev.y_root), ev.time)
            GLib.timeout_add(1500, self.save_pos)
        elif ev.button == 3:
            self.compact = not self.compact; self.cfg["compact"] = self.compact; self.save_cfg(); self.fit(); self.da.queue_draw()
        elif ev.button == 2:
            Gtk.main_quit()
        return True

    def save_pos(self):
        x, y = self.get_position(); self.cfg.update(x=x, y=y); self.save_cfg(); self.fit(); return False

    def tick(self):
        self.load(); self.fit(); self.da.queue_draw(); return True

    # ---------- drawing primitives ----------
    def lay(self, cr, s, size, font=SANS, weight="normal", spacing=0.0, upper=True, max_w=None):
        L = PangoCairo.create_layout(cr)
        s = s.upper() if upper else s
        sp = int(spacing * 1024 * size)
        L.set_markup(f'<span font_family="{font}" font_weight="{weight}" size="{int(size * 1024)}" letter_spacing="{sp}">'
                     f'{GLib.markup_escape_text(s)}</span>', -1)
        if max_w is not None:
            L.set_width(int(max(max_w, 1) * Pango.SCALE)); L.set_ellipsize(Pango.EllipsizeMode.END)
        return L

    def txt(self, cr, x, y, s, size, color, font=SANS, weight="normal", spacing=0.0, align="l", upper=True, max_w=None):
        L = self.lay(cr, s, size, font, weight, spacing, upper)
        w, h = L.get_pixel_size()
        if max_w is not None and w > max_w:
            L = self.lay(cr, s, size, font, weight, spacing, upper, max_w); w, h = L.get_pixel_size(); w = min(w, max_w)
        if align == "r": x -= w
        elif align == "c": x -= w / 2
        cr.set_source_rgb(*color); cr.move_to(x, y); PangoCairo.show_layout(cr, L)
        return w, h

    def width(self, cr, s, size, font=SANS, weight="normal", spacing=0.0, upper=True):
        return self.lay(cr, s, size, font, weight, spacing, upper).get_pixel_size()[0]

    def hline(self, cr, y):
        cr.set_source_rgb(*LINE); cr.set_line_width(1); cr.move_to(PAD, y + .5); cr.line_to(W - PAD, y + .5); cr.stroke()

    def spark(self, cr, x, y, w, h, data, lo=None, hi=None, color=OK):
        vals = [v for v in data if v is not None]
        cr.set_source_rgb(*LINE); cr.set_line_width(1); cr.move_to(x, y + h + .5); cr.line_to(x + w, y + h + .5); cr.stroke()
        if len(vals) < 2: return
        lo = min(vals) if lo is None else lo; hi = max(vals) if hi is None else hi
        if hi - lo < 1e-6: hi = lo + 1
        n = len(data); step = w / (HIST - 1); x0 = x + w - (n - 1) * step
        cr.set_source_rgba(color[0], color[1], color[2], 0.9); cr.set_line_width(1.2); first = True
        for i, v in enumerate(data):
            if v is None: continue
            px = x0 + i * step; py = y + h - (min(max(v, lo), hi) - lo) / (hi - lo) * h
            if first: cr.move_to(px, py); first = False
            else: cr.line_to(px, py)
        cr.stroke()

    def section(self, cr, y, left, right=None, rcolor=DIM, rfont=MONO):
        lw, _ = self.txt(cr, PAD, y, left, 6.5, DIM, spacing=0.3, max_w=IW * 0.62)
        if right:
            self.txt(cr, W - PAD, y, right, 6.5, rcolor, font=rfont, align="r", upper=False, max_w=IW - lw - 12)
        return y + 14

    def readout(self, cr, col, y, label, value, unit, color=FG, size=22):
        x = COLS[col]; cw = COLW if col < 2 else W - PAD - x
        self.txt(cr, x, y, label, 6.5, DIM, spacing=0.25, max_w=cw)
        s = size
        uw = self.width(cr, unit, 7, spacing=0.15) + 3 if unit else 0
        while s > 10 and self.width(cr, value, s, MONO, "300", upper=False) + uw > cw: s -= 1
        Lv = self.lay(cr, value, s, MONO, "300", upper=False); vw, vh = Lv.get_pixel_size()
        vy = y + 12
        cr.set_source_rgb(*color); cr.move_to(x, vy + (int(size * 1.62) - vh)); PangoCairo.show_layout(cr, Lv)
        if unit:
            Lu = self.lay(cr, unit, 7, spacing=0.15, max_w=max(cw - vw - 3, 8))
            base_v = vy + (int(size * 1.62) - vh) + Lv.get_baseline() / Pango.SCALE
            cr.set_source_rgb(*DIM); cr.move_to(x + vw + 3, base_v - Lu.get_baseline() / Pango.SCALE); PangoCairo.show_layout(cr, Lu)
        return y + 12 + int(size * 1.62) + 4

    # ---------- layout ----------
    def on_draw(self, w, cr):
        cr.set_operator(cairo.OPERATOR_CLEAR); cr.paint(); cr.set_operator(cairo.OPERATOR_OVER)
        self.render(cr)

    def render(self, cr, measure=False):
        d = self.d; H = self.H; compact = self.compact or self.auto_compact
        if not measure:
            cr.set_source_rgba(*BG); cr.rectangle(0, 0, W, H); cr.fill()
            cr.set_source_rgb(*LINE); cr.set_line_width(1); cr.rectangle(.5, .5, W - 1, H - 1); cr.stroke()
            cr.set_source_rgb(*DIM)
            for (cx, cy, dx, dy) in ((0, 0, 1, 1), (W, 0, -1, 1), (0, H, 1, -1), (W, H, -1, -1)):
                cr.move_to(cx + dx * 1.5, cy + dy * 12); cr.line_to(cx + dx * 1.5, cy + dy * 1.5); cr.line_to(cx + dx * 12, cy + dy * 1.5); cr.stroke()
        # header
        host = (d.get("host") or socket.gethostname().split(".")[0]).upper()
        up = d.get("uptime"); age = time.time() - d.get("ts", 0) if d.get("ts") else 999
        upt = ""
        if up is not None:
            u = int(float(up) + max(0, min(age, 60))); dd, r = divmod(u, 86400); hh, r = divmod(r, 3600); mm, ss = divmod(r, 60)
            upt = f"T+ {dd}d {hh:02d}:{mm:02d}:{ss:02d}" if dd else f"T+ {hh:02d}:{mm:02d}:{ss:02d}"
        uw = self.txt(cr, W - PAD, 10, upt, 11, FG, font=MONO, align="r", upper=False)[0] if upt else 0
        tmax = IW - uw - 12; title = f"{host} telemetry"
        if self.width(cr, title, 8, weight="500", spacing=0.25) > tmax: title = host
        self.txt(cr, PAD, 13, title, 8, FG, weight="500", spacing=0.25, max_w=tmax)
        stale = age > 10
        st = "LINK LOST" if stale else ("NOMINAL" if not d.get("flags") else "CAUTION")
        stc = RED if stale else (FG if st == "NOMINAL" else AMBER)
        cr.set_source_rgb(*stc); cr.arc(PAD + 3, 37, 2.5, 0, 2 * math.pi); cr.fill()
        sw, _ = self.txt(cr, PAD + 11, 31, st, 6.5, stc, spacing=0.3)
        rec = d.get("rec")
        right = time.strftime("%H:%M:%S %Z")
        if rec:
            right = f"REC {rec.get('interval_s', 0):.0f}S · {rec.get('rows', 0):,} ROWS   " + right
        self.txt(cr, W - PAD, 31, right, 6.5, DIM, spacing=0.15, align="r", max_w=IW - sw - 24)
        self.hline(cr, 46)
        y = 56
        if not d:
            self.txt(cr, W / 2, y + 10, "awaiting telemetry", 8, DIM, spacing=0.3, align="c"); return y + 40
        tctl = d.get("tctl"); cw = d.get("cpu_w"); gw = d.get("gpu_w"); mp = d.get("mem_pct"); sysw = d.get("sys_w")
        # CPU
        y = self.section(cr, y, f"cpu  ·  {short_cpu(d.get('cpu_model'))}")
        yy = self.readout(cr, 0, y, "temp", f0(tctl), "°C", lvl(tctl, 85, 95))
        if cw is not None: self.readout(cr, 1, y, "pkg power", f0(cw), "W")
        else: self.readout(cr, 1, y, "pkg power · root", "n/a", "", DIM)
        self.readout(cr, 2, y, "load", f0(d.get("cpu_util")), "%")
        y = yy
        if not compact:
            bits = [f"{d.get('cpu_mhz') or 0} MHz"]
            if d.get("tccd1") is not None: bits.append(f"{'CCD1' if d.get('tccd2') is not None else 'HOT'} {f0(d['tccd1'], 1)}°")
            if d.get("tccd2") is not None: bits.append(f"CCD2 {f0(d['tccd2'], 1)}°")
            bits.append(f"LA {d.get('load1')}")
            self.txt(cr, PAD, y, "   ".join(bits), 6.5, DIM, font=MONO, upper=False, max_w=IW); y += 14
            self.spark(cr, PAD, y, IW, 20, self.h["tctl"], 30, 100, lvl(tctl, 85, 95) if tctl and tctl >= 85 else OK); y += 28
        fl = d.get("fans_list") or []
        ftxt = ", ".join(f"{f['name']} {f['rpm']}" for f in fl) if fl else (d.get("fans") if d.get("fans") not in (None, "", "none") else "no sensor")
        y = self.section(cr, y, "fans  ·  rpm", ftxt, FG if fl else DIM, SANS)
        self.hline(cr, y); y += 10
        # GPUs
        for g in d.get("gpus") or []:
            i = g["idx"]; vt, vu = g.get("vram_total") or 0, g.get("vram") or 0
            y = self.section(cr, y, f"gpu {i}  ·  {short_gpu(g.get('name') or 'gpu')}", f"VRAM {vu / 1024:.1f} / {vt / 1024:.0f} GB")
            lim = g.get("limit") or 0; pwr = g.get("power") or 0
            pcol = lvl(pwr / lim * 100, 95, 101) if lim else FG
            size = 16 if compact else 18
            yy = self.readout(cr, 0, y, "temp", f0(g.get("temp")), "°C", lvl(g.get("temp"), 83, 88), size=size)
            self.readout(cr, 1, y, "power", f0(pwr), f"/{lim:.0f} W" if lim else "W", pcol, size=size)
            self.readout(cr, 2, y, "util", f0(g.get("util")), "%", size=size)
            y = yy
            frac = min(vu / max(vt, 1), 1.0)
            cr.set_source_rgb(*LINE); cr.rectangle(PAD, y, IW, 3); cr.fill()
            cr.set_source_rgb(*(lvl(frac * 100, 92, 98) if frac > 0.92 else OK)); cr.rectangle(PAD, y, IW * frac, 3); cr.fill()
            y += 8
            if not compact:
                bits = []
                if g.get("sm_mhz") is not None: bits.append(f"SM {g['sm_mhz']:.0f} MHz")
                if g.get("mem_mhz") is not None: bits.append(f"MEM {g['mem_mhz']:.0f} MHz")
                if g.get("fan_pct") is not None: bits.append(f"FAN {g['fan_pct']:.0f}%")
                if g.get("pstate"): bits.append(g["pstate"])
                if bits:
                    self.txt(cr, PAD, y, "   ".join(bits), 6.5, DIM, font=MONO, upper=False, max_w=IW); y += 14
                self.spark(cr, PAD, y, IW, 14, self.h[f"g{i}p"], 0, lim or None, OK); y += 22
            self.hline(cr, y); y += 10
        # Memory
        swu = ((d.get("swap_total") or 0) - (d.get("swap_free") or 0)) / 1024
        y = self.section(cr, y, "memory")
        yy = self.readout(cr, 0, y, "ram used", f0(None if mp is None else 100 - mp), "%", lvl(mp, 10, 5, invert=True))
        self.readout(cr, 1, y, "avail", f0((d.get("mem_avail") or 0) / 1024), f"/{(d.get('mem_total') or 0) / 1024:.0f} GB",
                     lvl(mp, 10, 5, invert=True))
        self.readout(cr, 2, y, "swap used", f0(swu, 1), "GB", AMBER if swu > 4 else FG)
        y = yy
        if not compact:
            pres = (d.get("majflt") or 0) + (d.get("swpin") or 0) + (d.get("swpout") or 0)
            self.txt(cr, PAD, y, f"MAJFLT {d.get('majflt', 0)}/s   SWPIN {d.get('swpin', 0)}/s   SWPOUT {d.get('swpout', 0)}/s", 6.5,
                     RED if pres > 5000 else AMBER if pres > 500 else DIM, font=MONO, upper=False, max_w=IW); y += 14
            self.spark(cr, PAD, y, IW, 14, self.h["mem"], 0, 100, OK); y += 22
        self.hline(cr, y); y += 10
        # Storage + network
        if d.get("root_used_pct") is not None:
            y = self.section(cr, y, "storage  ·  network")
            if d.get("home_used_pct") is not None:
                dv, du = f0(d.get("home_used_pct")), "% /home"
            else:
                dv, du = f0(d.get("root_used_pct")), "% /"
            yy = self.readout(cr, 0, y, "disk used", dv, du, lvl(float(dv) if dv != "--" else None, 85, 95), size=16)
            self.readout(cr, 1, y, "disk r+w", rate((d.get("disk_read_mbs") or 0) + (d.get("disk_write_mbs") or 0)), "MB/s", size=16)
            self.readout(cr, 2, y, "net rx+tx", rate((d.get("net_rx_mbs") or 0) + (d.get("net_tx_mbs") or 0)), "MB/s", size=16)
            y = yy
            if not compact:
                parts = [f"/ {f0(d.get('root_used_gb'))}/{f0(d.get('root_total_gb'))} GB"]
                if d.get("home_used_gb") is not None: parts.append(f"/home {f0(d.get('home_used_gb'))}/{f0(d.get('home_total_gb'))} GB")
                parts.append(f"R {rate(d.get('disk_read_mbs'))} W {rate(d.get('disk_write_mbs'))}")
                parts.append(f"RX {rate(d.get('net_rx_mbs'))} TX {rate(d.get('net_tx_mbs'))}")
                self.txt(cr, PAD, y, "   ".join(parts), 6.5, DIM, font=MONO, upper=False, max_w=IW); y += 14
            self.hline(cr, y); y += 10
        # Battery
        if d.get("bat_pct") is not None and d.get("bat_status") not in (None, "", "NA"):
            ac = d.get("ac_online")
            acs = "AC online" if ac == 1 else ("on battery" if ac == 0 else "")
            y = self.section(cr, y, "battery", f"{d.get('bat_status') or ''}  {acs}".strip(), DIM, SANS)
            pct = d.get("bat_pct"); bw = d.get("bat_w")
            yy = self.readout(cr, 0, y, "charge", f0(pct), "%", lvl(pct, 20, 10, invert=True), size=18)
            if bw is not None:
                out = (d.get("bat_status") or "").lower() == "discharging" or bw < 0
                self.readout(cr, 1, y, "power", f0(abs(bw), 1), "W out" if out else "W", AMBER if out else FG, size=18)
            y = yy; self.hline(cr, y); y += 10
        # Power total
        tot = (gw or 0) + (cw or 0)
        extra = "cpu + gpus" if cw is not None else "gpus only"
        y = self.section(cr, y, "power  ·  est. total", f"platform {sysw:.0f} W" if sysw is not None else None)
        yy = self.readout(cr, 0, y, extra, f0(tot), "W", size=22)
        if not compact: self.spark(cr, COLS[1], y + 14, W - PAD - COLS[1], 26, self.h["tot"], 0, None, OK)
        y = yy
        # footer
        top = d.get("top") or ""; flg = d.get("flags") or ""
        fw = self.txt(cr, W - PAD, y, flg, 6.5, RED, font=MONO, align="r", upper=False, max_w=IW * 0.6)[0] if flg else 0
        self.txt(cr, PAD, y, f"TOP {top}", 6.5, DIM, font=MONO, upper=False, max_w=IW - fw - 10)
        y += 14
        return y + 8

def snapshot(path, compact=False):
    w = Widget(offscreen=True); w.compact = compact; w.load()
    w.H = w.measure()
    surf = cairo.ImageSurface(cairo.FORMAT_ARGB32, W, w.H); cr = cairo.Context(surf)
    cr.set_source_rgb(0.12, 0.13, 0.15); cr.paint(); w.render(cr); surf.write_to_png(path); print("wrote", path, W, w.H)

if __name__ == "__main__":
    if len(sys.argv) > 2 and sys.argv[1] == "--snapshot":
        snapshot(sys.argv[2], compact="--compact" in sys.argv); sys.exit(0)
    win = Widget(); win.show_all(); Gtk.main()
