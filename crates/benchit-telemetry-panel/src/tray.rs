//! Taskbar indicator: a StatusNotifierItem (KDE / AppIndicator tray protocol) whose label is the
//! live CPU and GPU temperature, e.g. `CPU 62° GPU 55°/48°`.
//!
//! Left-click shows or hides the panel window; the indicator menu mirrors the panel menu
//! (Show/Hide panel, Compact, Copy stats, exports, Quit). It runs inside the panel process on
//! ksni's own thread and talks to the egui loop over a channel, so the panel can always be
//! brought back from the taskbar. GNOME Shell needs the "AppIndicator and KStatusNotifierItem
//! Support" extension to display it; without a tray host the item simply waits for one.
use crate::menu::Action;
use crate::report::Format;
use benchit_telemetry_core::model::Snapshot;
use benchit_telemetry_core::{ExportRange, TempUnit};
use ksni::menu::{CheckmarkItem, StandardItem, SubMenu};
use ksni::{Category, Icon, MenuItem, ToolTip};
use std::sync::mpsc::Sender;
use std::sync::Arc;

/// Feed older than this is shown as stale instead of as current temperatures.
pub const STALE_S: f64 = 60.0;

/// What the indicator asks the panel to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    /// Left-click / "Show panel" / "Hide panel": flip window visibility.
    ToggleWindow,
    /// A panel-menu action.
    Do(Action),
}

/// Everything the indicator displays; the panel pushes a new one when it changes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct View {
    pub label: String,
    pub tip: String,
    pub hidden: bool,
    pub compact: bool,
    pub busy: bool,
}

fn deg(unit: TempUnit, c: Option<f64>) -> String {
    match c {
        Some(_) => format!("{}°", unit.format(c, 0)),
        None => "--".into(),
    }
}

/// Taskbar label: `CPU 62° GPU 55°`, one GPU value per card (`GPU 55°/48°`), `--` if unknown.
pub fn label(snap: Option<&Snapshot>, age_s: f64, unit: TempUnit) -> String {
    let Some(s) = snap else { return "CPU -- GPU --".into() };
    if age_s > STALE_S {
        return "telemetry stale".into();
    }
    let cpu = deg(unit, s.cpu.temp_c);
    if s.gpus.is_empty() {
        return format!("CPU {cpu}");
    }
    let gpus: Vec<String> = s.gpus.iter().map(|g| deg(unit, g.temp_c)).collect();
    format!("CPU {cpu} GPU {}", gpus.join("/"))
}

/// Tooltip body: one line for the CPU and one per GPU, with power where known.
pub fn tip(snap: Option<&Snapshot>, age_s: f64, unit: TempUnit) -> String {
    let Some(s) = snap else { return "waiting for benchit-telemetry".into() };
    let w = |v: Option<f64>| v.map(|v| format!(", {v:.0} W")).unwrap_or_default();
    let mut lines = vec![format!("CPU {}{}{}", unit.format(s.cpu.temp_c, 0), unit.label(), w(s.cpu.pkg_w))];
    for (i, g) in s.gpus.iter().enumerate() {
        lines.push(format!("GPU{i} {} {}{}{}", g.name, unit.format(g.temp_c, 0), unit.label(), w(g.power_w)));
    }
    if age_s > STALE_S {
        lines.push(format!("last sample {age_s:.0} s ago"));
    }
    lines.join("\n")
}

/// 22x22 ARGB32 icon: three rising bars (so the item is recognisable even without a label).
pub fn icon() -> Icon {
    const N: i32 = 22;
    let mut data = vec![0u8; (N * N * 4) as usize];
    let bars = [(3, 12), (9, 7), (15, 2)];
    for y in 0..N {
        for x in 0..N {
            let on = bars.iter().any(|&(x0, top)| x >= x0 && x < x0 + 4 && y >= top && y < 20);
            if on {
                let i = ((y * N + x) * 4) as usize;
                data[i..i + 4].copy_from_slice(&[255, 236, 239, 244]);
            }
        }
    }
    Icon { width: N, height: N, data }
}

/// The ksni tray item.
pub struct Indicator {
    pub view: View,
    tx: Sender<Cmd>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Indicator {
    /// `wake` is called after every command so the (possibly hidden) panel processes it.
    pub fn new(view: View, tx: Sender<Cmd>, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self { view, tx, wake }
    }

    fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
        (self.wake)();
    }
}

fn item(label: impl Into<String>, enabled: bool, cmd: Cmd) -> MenuItem<Indicator> {
    StandardItem {
        label: label.into(),
        enabled,
        activate: Box::new(move |t: &mut Indicator| t.send(cmd)),
        ..Default::default()
    }
    .into()
}

impl ksni::Tray for Indicator {
    fn id(&self) -> String {
        "benchit-panel".into()
    }

    fn category(&self) -> Category {
        Category::Hardware
    }

    fn title(&self) -> String {
        format!("benchit telemetry: {}", self.view.label)
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![icon()]
    }

    fn x_ayatana_label(&self) -> String {
        self.view.label.clone()
    }

    fn x_ayatana_label_guide(&self) -> String {
        "CPU 100° GPU 100°/100°".into()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip { title: "benchit telemetry".into(), description: self.view.tip.clone(), ..Default::default() }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Cmd::ToggleWindow);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let v = &self.view;
        let mut items = vec![
            item(if v.hidden { "Show panel" } else { "Hide panel" }, true, Cmd::ToggleWindow),
            CheckmarkItem {
                label: "Compact".into(),
                checked: v.compact,
                activate: Box::new(|t: &mut Indicator| t.send(Cmd::Do(Action::ToggleCompact))),
                ..Default::default()
            }
            .into(),
            item("Copy stats", true, Cmd::Do(Action::CopyStats)),
            MenuItem::Separator,
        ];
        for f in Format::ALL {
            items.push(
                SubMenu {
                    label: f.label().into(),
                    enabled: !v.busy,
                    submenu: ExportRange::MENU
                        .iter()
                        .map(|&r| item(format!("Last {}", r.tag()), !v.busy, Cmd::Do(Action::Export(f, r))))
                        .collect(),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(MenuItem::Separator);
        items.push(item("Quit", true, Cmd::Do(Action::Quit)));
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use benchit_telemetry_core::model::Gpu;
    use std::sync::mpsc;

    fn snap(cpu: Option<f64>, gpus: &[Option<f64>]) -> Snapshot {
        let mut s = Snapshot::default();
        s.cpu.temp_c = cpu;
        s.gpus = gpus.iter().map(|&t| Gpu { temp_c: t, name: "RTX".into(), ..Default::default() }).collect();
        s
    }

    #[test]
    fn labels() {
        let c = TempUnit::C;
        assert_eq!(label(None, 0.0, c), "CPU -- GPU --");
        assert_eq!(label(Some(&snap(Some(61.6), &[Some(55.0)])), 1.0, c), "CPU 62° GPU 55°");
        assert_eq!(label(Some(&snap(Some(61.6), &[Some(55.0), None])), 1.0, c), "CPU 62° GPU 55°/--");
        assert_eq!(label(Some(&snap(Some(50.0), &[])), 1.0, c), "CPU 50°");
        assert_eq!(label(Some(&snap(Some(100.0), &[Some(0.0)])), 1.0, TempUnit::F), "CPU 212° GPU 32°");
        assert_eq!(label(Some(&snap(Some(50.0), &[])), STALE_S + 1.0, c), "telemetry stale");
        let t = tip(Some(&snap(Some(50.0), &[Some(40.0)])), 1.0, c);
        assert!(t.contains("CPU 50°C") && t.contains("GPU0 RTX 40°C"), "{t}");
    }

    #[test]
    fn icon_is_argb_22px() {
        let i = icon();
        assert_eq!(i.data.len(), 22 * 22 * 4);
        assert!(i.data.chunks(4).any(|p| p[0] == 255));
    }

    /// Activate every menu entry (and the icon) and collect what reaches the panel.
    fn fire_all(view: View) -> (Vec<String>, Vec<Cmd>, usize) {
        let (tx, rx) = mpsc::channel();
        let woke = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w = woke.clone();
        let mut t = Indicator::new(
            view,
            tx,
            Arc::new(move || {
                w.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }),
        );
        let mut labels = Vec::new();
        fn walk(items: Vec<MenuItem<Indicator>>, t: &mut Indicator, labels: &mut Vec<String>) {
            for it in items {
                match it {
                    MenuItem::Standard(s) => {
                        labels.push(s.label.clone());
                        if s.enabled {
                            (s.activate)(t);
                        }
                    }
                    MenuItem::Checkmark(c) => {
                        labels.push(format!("{}={}", c.label, c.checked));
                        (c.activate)(t);
                    }
                    MenuItem::SubMenu(m) => {
                        labels.push(m.label.clone());
                        walk(m.submenu, t, labels);
                    }
                    _ => {}
                }
            }
        }
        let items = ksni::Tray::menu(&t);
        walk(items, &mut t, &mut labels);
        ksni::Tray::activate(&mut t, 0, 0);
        let cmds: Vec<Cmd> = rx.try_iter().collect();
        (labels, cmds, woke.load(std::sync::atomic::Ordering::SeqCst))
    }

    #[test]
    fn menu_mirrors_the_panel_menu() {
        let (labels, cmds, woke) = fire_all(View { hidden: true, compact: true, ..Default::default() });
        assert_eq!(labels[0], "Show panel");
        assert!(labels.contains(&"Compact=true".to_string()));
        assert!(labels.contains(&"PDF report".to_string()) && labels.contains(&"Last 30d".to_string()));
        // show/hide, compact, copy, 3 formats x 4 ranges, quit, plus the icon click
        assert_eq!(cmds.len(), 1 + 1 + 1 + 12 + 1 + 1);
        assert_eq!(woke, cmds.len());
        assert_eq!(cmds[0], Cmd::ToggleWindow);
        assert!(cmds.contains(&Cmd::Do(Action::Export(Format::Pdf, ExportRange::Days(7)))));
        assert!(cmds.contains(&Cmd::Do(Action::Quit)));
        assert_eq!(*cmds.last().unwrap(), Cmd::ToggleWindow);
    }

    #[test]
    fn exports_disabled_while_busy() {
        let (labels, cmds, _) = fire_all(View { busy: true, ..Default::default() });
        assert_eq!(labels[0], "Hide panel");
        assert!(cmds.iter().all(|c| !matches!(c, Cmd::Do(Action::Export(..)))));
        assert_eq!(cmds.len(), 5);
    }
}
