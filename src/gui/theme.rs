//! Оформление: шрифты (Segoe UI на Windows), цвета светлой и тёмной темы.

use eframe::egui::{
    self, style::HandleShape, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Shadow,
    Stroke, TextStyle, Theme, Visuals,
};
use std::sync::Arc;

#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    pub bg: Color32,
    pub card: Color32,
    pub border: Color32,
    pub text: Color32,
    pub weak: Color32,
    pub faint: Color32,
    pub track: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub on_accent: Color32,
    pub ram: Color32,
    pub vram: Color32,
    pub other: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub warn_bg: Color32,
    pub danger: Color32,
    pub danger_bg: Color32,
    pub read: Color32,
    pub write: Color32,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

pub const LIGHT: Palette = Palette {
    dark: false,
    bg: rgb(0xF3, 0xF4, 0xF7),
    card: rgb(0xFF, 0xFF, 0xFF),
    border: rgb(0xE3, 0xE5, 0xEA),
    text: rgb(0x1B, 0x1D, 0x22),
    weak: rgb(0x5E, 0x63, 0x6E),
    faint: rgb(0x93, 0x98, 0xA3),
    track: rgb(0xEC, 0xEE, 0xF2),
    accent: rgb(0x25, 0x63, 0xEB),
    accent_hover: rgb(0x1D, 0x4F, 0xD8),
    on_accent: rgb(0xFF, 0xFF, 0xFF),
    ram: rgb(0x0E, 0xA5, 0xE9),
    vram: rgb(0x8B, 0x5C, 0xF6),
    other: rgb(0xC4, 0xC8, 0xD0),
    ok: rgb(0x16, 0xA3, 0x4A),
    warn: rgb(0xB4, 0x53, 0x09),
    warn_bg: rgb(0xFF, 0xF7, 0xE6),
    danger: rgb(0xDC, 0x26, 0x26),
    danger_bg: rgb(0xFE, 0xF0, 0xF0),
    read: rgb(0x0E, 0xA5, 0xE9),
    write: rgb(0xF5, 0x9E, 0x0B),
};

pub const DARK: Palette = Palette {
    dark: true,
    bg: rgb(0x16, 0x17, 0x1B),
    card: rgb(0x20, 0x22, 0x28),
    border: rgb(0x2F, 0x32, 0x3A),
    text: rgb(0xE8, 0xE9, 0xEC),
    weak: rgb(0xA3, 0xA8, 0xB3),
    faint: rgb(0x70, 0x75, 0x80),
    track: rgb(0x2C, 0x2F, 0x36),
    accent: rgb(0x4C, 0x86, 0xF7),
    accent_hover: rgb(0x66, 0x99, 0xFA),
    on_accent: rgb(0xFF, 0xFF, 0xFF),
    ram: rgb(0x38, 0xBD, 0xF8),
    vram: rgb(0xA7, 0x8B, 0xFA),
    other: rgb(0x4A, 0x4E, 0x58),
    ok: rgb(0x4A, 0xDE, 0x80),
    warn: rgb(0xFB, 0xBF, 0x24),
    warn_bg: rgb(0x33, 0x2A, 0x14),
    danger: rgb(0xF8, 0x71, 0x71),
    danger_bg: rgb(0x3A, 0x1D, 0x1F),
    read: rgb(0x38, 0xBD, 0xF8),
    write: rgb(0xFB, 0xBF, 0x24),
};

pub fn palette(ctx: &egui::Context) -> Palette {
    if ctx.global_style().visuals.dark_mode {
        DARK
    } else {
        LIGHT
    }
}

/// Семейство полужирного шрифта для заголовков.
pub fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

pub fn font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

pub fn bold(size: f32) -> FontId {
    FontId::new(size, semibold())
}

fn load_font(paths: &[std::path::PathBuf]) -> Option<(String, Vec<u8>)> {
    for p in paths {
        if let Ok(b) = std::fs::read(p) {
            return Some((p.file_name()?.to_string_lossy().into_owned(), b));
        }
    }
    None
}

fn font_candidates() -> (Vec<std::path::PathBuf>, Vec<std::path::PathBuf>) {
    use std::path::PathBuf;
    if cfg!(windows) {
        let dir = std::env::var_os("WINDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows")).join("Fonts");
        (
            ["segoeui.ttf", "arial.ttf", "tahoma.ttf"].iter().map(|f| dir.join(f)).collect(),
            ["seguisb.ttf", "segoeuib.ttf", "arialbd.ttf", "tahomabd.ttf"].iter().map(|f| dir.join(f)).collect(),
        )
    } else {
        (
            [
                "/usr/share/fonts/liberation-sans/LiberationSans-Regular.ttf",
                "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
                "/usr/share/fonts/msttcore/arial.ttf",
            ]
            .iter()
            .map(PathBuf::from)
            .collect(),
            [
                "/usr/share/fonts/liberation-sans/LiberationSans-Bold.ttf",
                "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
                "/usr/share/fonts/msttcore/arialbd.ttf",
            ]
            .iter()
            .map(PathBuf::from)
            .collect(),
        )
    }
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let (regular, bold_paths) = font_candidates();
    let mut prop = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut semi = prop.clone();
    if let Some((name, data)) = load_font(&regular) {
        fonts.font_data.insert(name.clone(), Arc::new(FontData::from_owned(data)));
        prop.insert(0, name.clone());
        semi.insert(0, name);
    }
    if let Some((name, data)) = load_font(&bold_paths) {
        fonts.font_data.insert(name.clone(), Arc::new(FontData::from_owned(data)));
        semi.insert(0, name);
    }
    fonts.families.insert(FontFamily::Proportional, prop);
    fonts.families.insert(semibold(), semi);
    ctx.set_fonts(fonts);

    for (theme, p) in [(Theme::Light, LIGHT), (Theme::Dark, DARK)] {
        ctx.set_visuals_of(theme, visuals(&p));
        ctx.style_mut_of(theme, |s| {
            s.text_styles = [
                (TextStyle::Heading, bold(20.0)),
                (TextStyle::Body, font(14.5)),
                (TextStyle::Button, font(14.5)),
                (TextStyle::Small, font(12.5)),
                (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
            ]
            .into();
            s.spacing.item_spacing = egui::vec2(8.0, 8.0);
            s.spacing.button_padding = egui::vec2(14.0, 7.0);
            s.spacing.interact_size.y = 30.0;
            s.spacing.slider_rail_height = 6.0;
            s.spacing.combo_width = 120.0;
            s.spacing.icon_width = 18.0;
            s.spacing.icon_width_inner = 10.0;
            s.spacing.window_margin = Margin::same(20);
            s.interaction.selectable_labels = false;
        });
    }
}

fn visuals(p: &Palette) -> Visuals {
    let mut v = if p.dark { Visuals::dark() } else { Visuals::light() };
    let r8 = CornerRadius::same(8);
    v.panel_fill = p.bg;
    v.window_fill = p.card;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(14);
    v.window_shadow = Shadow { offset: [0, 10], blur: 30, spread: 0, color: Color32::from_black_alpha(if p.dark { 120 } else { 45 }) };
    v.popup_shadow = Shadow { offset: [0, 6], blur: 16, spread: 0, color: Color32::from_black_alpha(if p.dark { 100 } else { 35 }) };
    v.menu_corner_radius = r8;
    v.extreme_bg_color = if p.dark { rgb(0x18, 0x19, 0x1E) } else { rgb(0xF7, 0xF8, 0xFA) };
    v.faint_bg_color = p.track;
    v.code_bg_color = p.track;
    v.override_text_color = None;
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = p.accent;
    v.selection.stroke = Stroke::new(1.5, p.on_accent);
    v.slider_trailing_fill = true;
    v.handle_shape = HandleShape::Circle;
    v.striped = false;

    let btn = if p.dark { rgb(0x2B, 0x2E, 0x35) } else { rgb(0xEE, 0xF0, 0xF4) };
    let btn_hover = if p.dark { rgb(0x34, 0x38, 0x41) } else { rgb(0xE4, 0xE7, 0xEC) };
    let btn_active = if p.dark { rgb(0x3D, 0x41, 0x4B) } else { rgb(0xD9, 0xDD, 0xE4) };

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.card;
    w.noninteractive.weak_bg_fill = p.card;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = r8;

    w.inactive.bg_fill = btn;
    w.inactive.weak_bg_fill = btn;
    w.inactive.bg_stroke = Stroke::new(1.0, p.border);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = r8;
    w.inactive.expansion = 0.0;

    w.hovered.bg_fill = btn_hover;
    w.hovered.weak_bg_fill = btn_hover;
    w.hovered.bg_stroke = Stroke::new(1.0, p.accent.gamma_multiply(0.6));
    w.hovered.fg_stroke = Stroke::new(1.5, p.text);
    w.hovered.corner_radius = r8;
    w.hovered.expansion = 0.0;

    w.active.bg_fill = btn_active;
    w.active.weak_bg_fill = btn_active;
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(1.5, p.text);
    w.active.corner_radius = r8;
    w.active.expansion = 0.0;

    w.open.bg_fill = btn_hover;
    w.open.weak_bg_fill = btn_hover;
    w.open.bg_stroke = Stroke::new(1.0, p.accent);
    w.open.fg_stroke = Stroke::new(1.0, p.text);
    w.open.corner_radius = r8;
    v
}
