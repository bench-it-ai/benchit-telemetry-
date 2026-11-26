//! Right-click context menu: Compact, Hide, Copy stats, Export CSV / JSON, PDF report, Quit.
//!
//! The panel window is only as large as its content, so the menu is drawn inside it (an egui
//! `Area`) and the window grows while the menu is open. Sub-menus would need room outside the
//! window, so the export ranges are inline buttons on each export row.
use crate::layout::{DIM, FG, W};
use crate::report::Format;
use benchit_telemetry_core::ExportRange;
use eframe::egui::{self, Button, Color32, FontId, Pos2, Rect, RichText, Vec2};

/// What the user picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    ToggleCompact,
    Hide,
    CopyStats,
    Export(Format, ExportRange),
    Quit,
}

/// Width of the menu.
pub const MENU_W: f32 = 268.0;
/// Height the window needs for the menu (rows plus frame), used before the first measured frame.
pub const MENU_H: f32 = 236.0;

/// Top-left corner for a menu of `size` opened at `click`, kept inside a `window`-sized area
/// (`margin` from every edge where possible).
pub fn place(click: Pos2, size: Vec2, window: Vec2, margin: f32) -> Pos2 {
    let clamp = |v: f32, len: f32, max: f32| v.min(max - len - margin).max(margin.min(max - len).max(0.0));
    Pos2::new(clamp(click.x, size.x, window.x), clamp(click.y, size.y, window.y))
}

/// Window height needed to show the menu (`menu_h`) below `content_h`-high panel content.
pub fn window_height(content_h: f32, menu_h: f32) -> f32 {
    content_h.max(menu_h + 16.0)
}

/// Dark widget style for the menu; call once at startup (the panel itself is painted directly).
pub fn style(ctx: &egui::Context) {
    ctx.global_style_mut(|s| {
        s.visuals = egui::Visuals::dark();
        s.visuals.window_fill = Color32::from_rgb(14, 16, 19);
        s.visuals.panel_fill = Color32::from_rgb(14, 16, 19);
        s.visuals.override_text_color = Some(crate::gui::color(FG));
        s.spacing.button_padding = Vec2::new(6.0, 3.0);
        s.spacing.item_spacing = Vec2::new(4.0, 3.0);
    });
}

/// Draw the menu at `pos`. Returns the picked action (if any) and the menu's rectangle.
/// `busy` disables the export rows while an export is being written.
pub fn show(ctx: &egui::Context, pos: Pos2, compact: bool, busy: bool) -> (Option<Action>, Rect) {
    let mut act = None;
    let resp = egui::Area::new(egui::Id::unique("bt-context-menu"))
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .constrain(false)
        .show(ctx, |ui| {
            egui::Frame::menu(ui.style()).fill(Color32::from_rgb(14, 16, 19)).show(ui, |ui| {
                ui.set_width(MENU_W - 14.0);
                let item = |ui: &mut egui::Ui, text: &str, hint: &str| {
                    ui.add_sized(
                        [MENU_W - 14.0, 20.0],
                        Button::new(text).right_text(RichText::new(hint).weak()).frame(false),
                    )
                    .clicked()
                };
                if item(ui, if compact { "Compact  [on]" } else { "Compact  [off]" }, "right-click") {
                    act = Some(Action::ToggleCompact);
                }
                if item(ui, "Hide panel", "--show restores") {
                    act = Some(Action::Hide);
                }
                if item(ui, "Copy stats", "clipboard") {
                    act = Some(Action::CopyStats);
                }
                ui.separator();
                for fmt in Format::ALL {
                    ui.horizontal(|ui| {
                        ui.add_sized([86.0, 20.0], egui::Label::new(fmt.label()));
                        for r in ExportRange::MENU {
                            if ui.add_enabled(!busy, Button::new(r.tag()).min_size(Vec2::new(36.0, 20.0))).clicked() {
                                act = Some(Action::Export(fmt, r));
                            }
                        }
                    });
                }
                ui.label(RichText::new("saved to ~/benchit-reports").size(10.5).color(crate::gui::color(DIM)));
                ui.separator();
                if item(ui, "Quit panel", "middle-click") {
                    act = Some(Action::Quit);
                }
            });
        });
    (act, resp.response.rect)
}

/// Transient status line at the bottom of the window (export saved, copied, errors).
pub fn toast(ctx: &egui::Context, height: f32, text: &str) {
    let p = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::unique("bt-toast")));
    let r = Rect::from_min_size(Pos2::new(6.0, height - 30.0), Vec2::new(W - 12.0, 24.0));
    p.rect_filled(r, 3.0, Color32::from_rgba_unmultiplied(20, 60, 95, 235));
    let font = FontId::proportional(12.0);
    let galley = p.layout(text.to_string(), font, Color32::WHITE, r.width() - 12.0);
    p.galley(Pos2::new(r.min.x + 6.0, r.center().y - galley.size().y / 2.0), galley, Color32::WHITE);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_stays_inside_the_window() {
        let size = Vec2::new(MENU_W, MENU_H);
        let win = Vec2::new(W, 600.0);
        assert_eq!(place(Pos2::new(20.0, 30.0), size, win, 8.0), Pos2::new(20.0, 30.0));
        let p = place(Pos2::new(390.0, 590.0), size, win, 8.0);
        assert_eq!(p, Pos2::new(W - MENU_W - 8.0, 600.0 - MENU_H - 8.0));
        assert_eq!(place(Pos2::new(-5.0, -5.0), size, win, 8.0), Pos2::new(8.0, 8.0));
        // window smaller than the menu: pin to the top-left corner
        assert_eq!(place(Pos2::new(50.0, 50.0), size, Vec2::new(100.0, 100.0), 8.0), Pos2::new(0.0, 0.0));
    }

    use std::collections::HashSet;

    fn pass(ctx: &egui::Context, events: Vec<egui::Event>, f: impl FnMut(&egui::Context)) {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(W, 700.0))),
            events,
            ..Default::default()
        };
        ctx.run_pass(input, f).drop_without_applying_deltas();
    }

    /// Click every few pixels of the menu (press + release, as a mouse would) and collect the
    /// actions that come back.
    fn reachable(busy: bool) -> HashSet<String> {
        let ctx = egui::Context::default();
        style(&ctx);
        let at = Pos2::new(40.0, 40.0);
        let mut rect = Rect::NOTHING;
        pass(&ctx, vec![], |ctx| rect = show(ctx, at, false, busy).1);
        assert!(rect.width() > 200.0 && rect.height() > 150.0, "{rect:?}");
        let mut got = HashSet::new();
        let mut y = rect.min.y + 2.0;
        while y < rect.max.y {
            let mut x = rect.min.x + 2.0;
            while x < rect.max.x {
                let p = Pos2::new(x, y);
                let button = |pressed| egui::Event::PointerButton {
                    pos: p,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: Default::default(),
                };
                for ev in [egui::Event::PointerMoved(p), button(true), button(false)] {
                    pass(&ctx, vec![ev], |ctx| {
                        if let (Some(a), _) = show(ctx, at, false, busy) {
                            got.insert(format!("{a:?}"));
                        }
                    });
                }
                x += 9.0;
            }
            y += 6.0;
        }
        got
    }

    #[test]
    fn every_item_is_clickable() {
        let got = reachable(false);
        let mut want: Vec<Action> = vec![Action::ToggleCompact, Action::Hide, Action::CopyStats, Action::Quit];
        for f in Format::ALL {
            for r in ExportRange::MENU {
                want.push(Action::Export(f, r));
            }
        }
        let want: HashSet<String> = want.iter().map(|a| format!("{a:?}")).collect();
        assert_eq!(want.len(), 16);
        assert_eq!(got, want);
    }

    #[test]
    fn exports_are_disabled_while_busy() {
        let got = reachable(true);
        assert!(got.iter().all(|a| !a.starts_with("Export")), "{got:?}");
        assert!(got.contains("CopyStats"));
    }

    #[test]
    fn window_grows_for_the_menu() {
        assert_eq!(window_height(500.0, MENU_H), 500.0);
        assert_eq!(window_height(120.0, MENU_H), MENU_H + 16.0);
    }
}
