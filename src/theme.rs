//! Visual identity for WyvernScan.
//!
//! Design direction: a wyvern's hide, read through a diagnostic instrument.
//! Surfaces are deep, near-black greens (the dark hide); the single accent
//! is wyvern-eye amber (used for state, not decoration), which sets off the
//! greens of the icon and the treemap. Monospace stays for anything
//! that is actually data (paths, byte counts), and panels stay flat with
//! hairline borders rather than card shadows or heavy rounding. The
//! treemap uses a small curated palette of hide, bronze, ember and
//! wing-membrane tones on flat tiles; the palette is deliberately muted so a
//! folder's color reads as a label, not noise.

use egui::{Color32, Rounding, Stroke};

pub mod colors {
    use super::Color32;

    pub const BG: Color32 = Color32::from_rgb(0x0D, 0x13, 0x11);
    pub const PANEL: Color32 = Color32::from_rgb(0x13, 0x1B, 0x18);
    pub const ELEVATED: Color32 = Color32::from_rgb(0x1B, 0x26, 0x22);
    pub const BORDER: Color32 = Color32::from_rgb(0x2B, 0x3B, 0x34);
    pub const TEXT: Color32 = Color32::from_rgb(0xE6, 0xEB, 0xE5);
    pub const MUTED: Color32 = Color32::from_rgb(0x8C, 0x9E, 0x95);
    /// Wyvern-eye amber: the one accent color, a complement to the greens.
    pub const ACCENT: Color32 = Color32::from_rgb(0xF2, 0xA9, 0x3B);
    /// Venom green, for navigable folder names so they read as links
    /// without competing with the amber accent.
    pub const OTHER_ACCENT: Color32 = Color32::from_rgb(0x6F, 0xC3, 0x8A);
    /// Ember red for destructive actions.
    pub const DANGER: Color32 = Color32::from_rgb(0xD9, 0x5A, 0x45);
    /// Flat, deliberately dull color for the synthetic "Other" bucket in
    /// the treemap, so it reads as "not a real single folder" at a glance.
    pub const OTHER_BUCKET: Color32 = Color32::from_rgb(0x2A, 0x33, 0x30);

    /// Curated treemap palette: the colors of a wyvern -- hide, bronze,
    /// ember and wing membrane. Directories cycle through this in order at
    /// each level, so siblings are visually distinct without looking
    /// randomly colored.
    pub const TREEMAP: [Color32; 6] = [
        Color32::from_rgb(0x2F, 0x7A, 0x62), // emerald hide
        Color32::from_rgb(0x9A, 0x72, 0x3C), // bronze
        Color32::from_rgb(0x5F, 0x7D, 0x45), // moss
        Color32::from_rgb(0x2F, 0x5E, 0x70), // deep teal
        Color32::from_rgb(0x9A, 0x55, 0x3A), // ember rust
        Color32::from_rgb(0x5E, 0x4B, 0x72), // wing membrane
    ];
}

pub fn apply(ctx: &egui::Context) {
    use colors::*;

    let mut visuals = egui::Visuals::dark();
    visuals.override_text_color = Some(TEXT);
    visuals.panel_fill = PANEL;
    visuals.window_fill = PANEL;
    visuals.faint_bg_color = ELEVATED;
    visuals.extreme_bg_color = BG;
    visuals.code_bg_color = ELEVATED;
    visuals.hyperlink_color = ACCENT;
    visuals.window_rounding = Rounding::same(6.0);
    visuals.menu_rounding = Rounding::same(6.0);

    visuals.widgets.noninteractive.bg_fill = PANEL;
    visuals.widgets.noninteractive.weak_bg_fill = PANEL;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, MUTED);
    visuals.widgets.noninteractive.rounding = Rounding::same(4.0);

    visuals.widgets.inactive.bg_fill = ELEVATED;
    visuals.widgets.inactive.weak_bg_fill = ELEVATED;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    visuals.widgets.inactive.rounding = Rounding::same(4.0);

    visuals.widgets.hovered.bg_fill = ELEVATED;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, TEXT);
    visuals.widgets.hovered.rounding = Rounding::same(4.0);

    visuals.widgets.active.bg_fill = ACCENT;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, BG);
    visuals.widgets.active.rounding = Rounding::same(4.0);

    visuals.widgets.open.bg_fill = ELEVATED;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0_f32, TEXT);
    visuals.widgets.open.rounding = Rounding::same(4.0);

    visuals.selection.bg_fill = ACCENT.linear_multiply(0.35);
    visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);

    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.window_margin = egui::Margin::same(14.0);
    style.spacing.indent = 16.0;
    ctx.set_style(style);
}
