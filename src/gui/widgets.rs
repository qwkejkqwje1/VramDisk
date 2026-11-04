//! Элементы интерфейса: карточки, полосы, кнопки, переключатели, значки.
//! Значки рисуются линиями — без шрифтов с эмодзи.

use super::theme::{bold, font, Palette};
use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, CornerRadius, CursorIcon, Layout, Margin, Mesh, Painter, Pos2, Rect,
    Response, RichText, Sense, Shadow, Shape, Stroke, StrokeKind, Ui,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Gpu,
    Ram,
    Disk,
    Sliders,
    Warning,
    Check,
    Cross,
    Bolt,
    Folder,
    Power,
    Info,
    Activity,
    Contrast,
}

/// Рисует значок в квадрате `rect` (макет 20×20).
pub fn paint_icon(p: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let s = rect.width() / 20.0;
    let o = rect.min;
    let pt = |x: f32, y: f32| pos2(o.x + x * s, o.y + y * s);
    let st = Stroke::new((1.6 * s).max(1.0), color);
    let thin = Stroke::new((1.3 * s).max(1.0), color);
    let rr = |r: f32| CornerRadius::same((r * s).round().clamp(0.0, 255.0) as u8);
    match icon {
        Icon::Gpu => {
            p.rect_stroke(Rect::from_min_max(pt(2.0, 4.5), pt(18.0, 14.5)), rr(2.0), st, StrokeKind::Middle);
            p.circle_stroke(pt(7.5, 9.5), 3.0 * s, st);
            p.circle_filled(pt(7.5, 9.5), 1.0 * s, color);
            p.rect_filled(Rect::from_min_max(pt(12.5, 7.5), pt(15.5, 11.5)), rr(0.8), color);
            for x in [5.0, 7.5, 10.0, 12.5] {
                p.line_segment([pt(x, 14.5), pt(x, 17.0)], thin);
            }
        }
        Icon::Ram => {
            p.rect_stroke(Rect::from_min_max(pt(1.5, 5.5), pt(18.5, 12.5)), rr(1.5), st, StrokeKind::Middle);
            for x in [3.8, 8.3, 12.8] {
                p.rect_filled(Rect::from_min_max(pt(x, 7.6), pt(x + 3.4, 10.4)), rr(0.6), color);
            }
            for i in 0..8 {
                let x = 3.0 + i as f32 * 2.0;
                p.line_segment([pt(x, 12.5), pt(x, 15.0)], thin);
            }
        }
        Icon::Disk => {
            p.rect_stroke(Rect::from_min_max(pt(2.5, 4.0), pt(17.5, 16.0)), rr(2.5), st, StrokeKind::Middle);
            p.line_segment([pt(2.5, 11.0), pt(17.5, 11.0)], thin);
            p.circle_filled(pt(14.3, 13.6), 1.1 * s, color);
            p.line_segment([pt(5.0, 13.6), pt(9.5, 13.6)], thin);
        }
        Icon::Sliders => {
            for (y, k) in [(5.5, 7.0), (10.0, 13.0), (14.5, 9.0)] {
                p.line_segment([pt(2.5, y), pt(17.5, y)], thin);
                p.circle_filled(pt(k, y), 2.4 * s, color);
            }
        }
        Icon::Warning => {
            let tri = vec![pt(10.0, 2.5), pt(18.2, 16.8), pt(1.8, 16.8)];
            p.add(Shape::closed_line(tri, st));
            p.line_segment([pt(10.0, 7.5), pt(10.0, 11.8)], st);
            p.circle_filled(pt(10.0, 14.4), 1.05 * s, color);
        }
        Icon::Check => {
            p.add(Shape::line(vec![pt(4.0, 10.5), pt(8.3, 14.6), pt(16.0, 5.8)], Stroke::new(2.2 * s, color)));
        }
        Icon::Cross => {
            let st2 = Stroke::new(2.2 * s, color);
            p.line_segment([pt(5.5, 5.5), pt(14.5, 14.5)], st2);
            p.line_segment([pt(14.5, 5.5), pt(5.5, 14.5)], st2);
        }
        Icon::Bolt => {
            let pts = vec![pt(11.5, 1.5), pt(4.0, 11.2), pt(9.2, 11.2), pt(8.3, 18.5), pt(16.0, 8.4), pt(10.6, 8.4)];
            p.add(Shape::closed_line(pts.clone(), thin));
            fill_polygon(p, &pts, color);
        }
        Icon::Folder => {
            let pts = vec![pt(2.5, 5.0), pt(7.8, 5.0), pt(9.6, 7.2), pt(17.5, 7.2), pt(17.5, 15.5), pt(2.5, 15.5)];
            p.add(Shape::closed_line(pts, st));
        }
        Icon::Power => {
            let c = pt(10.0, 10.8);
            let r = 6.6 * s;
            let mut arc = Vec::new();
            for i in 0..=24 {
                let a = (-60.0f32 + i as f32 * (300.0 / 24.0)).to_radians() - std::f32::consts::FRAC_PI_2;
                arc.push(pos2(c.x + r * a.cos(), c.y + r * a.sin()));
            }
            // дуга с разрывом сверху
            let arc: Vec<Pos2> = arc.into_iter().rev().collect();
            p.add(Shape::line(arc, st));
            p.line_segment([pt(10.0, 2.5), pt(10.0, 9.5)], st);
        }
        Icon::Info => {
            p.circle_stroke(pt(10.0, 10.0), 7.6 * s, st);
            p.line_segment([pt(10.0, 9.0), pt(10.0, 14.2)], st);
            p.circle_filled(pt(10.0, 6.2), 1.05 * s, color);
        }
        Icon::Contrast => {
            let c = pt(10.0, 10.0);
            let r = 7.2 * s;
            p.circle_stroke(c, r, st);
            let half: Vec<Pos2> = (0..=16)
                .map(|i| {
                    let a = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 16.0;
                    pos2(c.x + r * a.cos(), c.y + r * a.sin())
                })
                .collect();
            p.add(Shape::convex_polygon(half, color, Stroke::NONE));
        }
        Icon::Activity => {
            let pts = vec![pt(1.5, 10.5), pt(5.5, 10.5), pt(8.0, 4.0), pt(12.0, 16.0), pt(14.5, 10.5), pt(18.5, 10.5)];
            p.add(Shape::line(pts, st));
        }
    }
}

/// Заливка простого многоугольника (веером от первой вершины; для «молнии» достаточно).
fn fill_polygon(p: &Painter, pts: &[Pos2], color: Color32) {
    let mut m = Mesh::default();
    for &q in pts {
        m.colored_vertex(q, color);
    }
    // Молния не выпуклая: две половины выпуклые.
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 5);
    m.add_triangle(5, 2, 4);
    m.add_triangle(2, 3, 4);
    p.add(Shape::mesh(m));
}

pub fn icon(ui: &mut Ui, icon: Icon, size: f32, color: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    paint_icon(ui.painter(), rect, icon, color);
    resp
}

/// Карточка на всю ширину.
pub fn card<R>(ui: &mut Ui, pal: &Palette, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(pal.card)
        .stroke(Stroke::new(1.0, pal.border))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(18, 16))
        .shadow(Shadow { offset: [0, 1], blur: 4, spread: 0, color: Color32::from_black_alpha(if pal.dark { 40 } else { 10 }) })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// Цветная плашка (предупреждение, ошибка).
pub fn banner<R>(ui: &mut Ui, fg: Color32, bg: Color32, ic: Icon, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(bg)
        .stroke(Stroke::new(1.0, fg.gamma_multiply(0.35)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                icon(ui, ic, 18.0, fg);
                ui.vertical(|ui| add(ui)).inner
            })
            .inner
        })
        .inner
}

/// Заголовок карточки: значок, название, справа — пояснение.
pub fn card_title(ui: &mut Ui, pal: &Palette, ic: Icon, color: Color32, title: &str, right: Option<RichText>) {
    ui.horizontal(|ui| {
        icon(ui, ic, 20.0, color);
        ui.add_space(2.0);
        ui.label(RichText::new(title).font(bold(15.5)).color(pal.text));
        if let Some(r) = right {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(r);
            });
        }
    });
}

/// Полоса из цветных частей: доли от 0 до 1 подряд, остаток — дорожка.
pub fn bar(ui: &mut Ui, pal: &Palette, height: f32, parts: &[(f64, Color32)]) -> Rect {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let p = ui.painter();
    let r = (height / 2.0).round() as u8;
    p.rect_filled(rect, CornerRadius::same(r), pal.track);
    let mut x = 0.0f64;
    let n = parts.len();
    for (i, &(frac, color)) in parts.iter().enumerate() {
        let frac = frac.clamp(0.0, 1.0 - x);
        if frac <= 0.0 {
            continue;
        }
        let x0 = rect.left() + (x as f32) * rect.width();
        let x1 = rect.left() + ((x + frac) as f32) * rect.width();
        let first = x <= 0.0;
        let last = x + frac >= 0.9999 || i == n;
        let cr = CornerRadius {
            nw: if first { r } else { 0 },
            sw: if first { r } else { 0 },
            ne: if last { r } else { 0 },
            se: if last { r } else { 0 },
        };
        let seg = Rect::from_min_max(pos2(x0, rect.top()), pos2(x1.max(x0 + 1.0), rect.bottom()));
        p.rect_filled(seg, cr, color);
        x += frac;
    }
    rect
}

/// Маленькая цветная точка + текст (легенда).
pub fn legend(ui: &mut Ui, pal: &Palette, color: Color32, text: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 4.5, color);
        ui.label(RichText::new(text).font(font(13.0)).color(pal.weak));
    });
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    Primary,
    Secondary,
    Danger,
}

/// Крупная кнопка заданной ширины.
pub fn button(ui: &mut Ui, pal: &Palette, kind: ButtonKind, text: &str, ic: Option<Icon>, width: f32, enabled: bool) -> Response {
    let h = 42.0;
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(width, h), sense);
    let hovered = enabled && resp.hovered();
    let pressed = enabled && resp.is_pointer_button_down_on();
    let (fill, fg, stroke) = match kind {
        ButtonKind::Primary => {
            let f = if !enabled {
                pal.other
            } else if pressed {
                pal.accent_hover.gamma_multiply(0.9)
            } else if hovered {
                pal.accent_hover
            } else {
                pal.accent
            };
            (f, if enabled { pal.on_accent } else { pal.card }, Stroke::NONE)
        }
        ButtonKind::Secondary => {
            let base = if pal.dark { Color32::from_rgb(0x2B, 0x2E, 0x35) } else { Color32::from_rgb(0xEE, 0xF0, 0xF4) };
            let f = if pressed {
                base.gamma_multiply(0.9)
            } else if hovered {
                if pal.dark { Color32::from_rgb(0x34, 0x38, 0x41) } else { Color32::from_rgb(0xE4, 0xE7, 0xEC) }
            } else {
                base
            };
            (f, if enabled { pal.text } else { pal.faint }, Stroke::new(1.0, pal.border))
        }
        ButtonKind::Danger => {
            let f = if pressed {
                pal.danger.gamma_multiply(0.25)
            } else if hovered {
                pal.danger_bg
            } else {
                pal.card
            };
            (f, if enabled { pal.danger } else { pal.faint }, Stroke::new(1.0, pal.danger.gamma_multiply(0.5)))
        }
    };
    let p = ui.painter();
    p.rect(rect, CornerRadius::same(10), fill, stroke, StrokeKind::Inside);
    let galley = ui.painter().layout_no_wrap(text.to_string(), bold(14.5), fg);
    let icon_w = if ic.is_some() { 18.0 + 8.0 } else { 0.0 };
    let total = galley.size().x + icon_w;
    let x0 = rect.center().x - total / 2.0;
    if let Some(ic) = ic {
        paint_icon(p, Rect::from_min_size(pos2(x0, rect.center().y - 9.0), vec2(18.0, 18.0)), ic, fg);
    }
    p.galley(pos2(x0 + icon_w, rect.center().y - galley.size().y / 2.0), galley, fg);
    if hovered {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    resp
}

/// Переключатель «вкл/выкл».
pub fn toggle(ui: &mut Ui, pal: &Palette, on: &mut bool, enabled: bool) -> Response {
    let size = vec2(40.0, 22.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    if enabled && resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let off_c = if pal.dark { Color32::from_rgb(0x45, 0x49, 0x53) } else { Color32::from_rgb(0xC9, 0xCD, 0xD5) };
    let mut bg = lerp_color(off_c, pal.accent, t);
    if !enabled {
        bg = bg.gamma_multiply(0.5);
    }
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(11), bg);
    let cx = egui::lerp((rect.left() + 11.0)..=(rect.right() - 11.0), t);
    p.circle_filled(pos2(cx, rect.center().y), 8.0, Color32::WHITE);
    if enabled && resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    resp
}

pub fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// Строка параметра: название и пояснение слева, переключатель справа.
pub fn toggle_row(ui: &mut Ui, pal: &Palette, title: &str, hint: &str, on: &mut bool, enabled: bool) -> Response {
    let mut resp = None;
    ui.horizontal(|ui| {
        let w = ui.available_width() - 52.0;
        ui.vertical(|ui| {
            ui.set_width(w);
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.label(RichText::new(title).color(if enabled { pal.text } else { pal.faint }));
            if !hint.is_empty() {
                ui.label(RichText::new(hint).font(font(12.5)).color(pal.faint));
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            resp = Some(toggle(ui, pal, on, enabled));
        });
    });
    resp.unwrap()
}

/// Плашка состояния в шапке.
pub fn pill(ui: &mut Ui, text: &str, fg: Color32, bg: Color32, dot: bool) {
    let galley = ui.painter().layout_no_wrap(text.to_string(), bold(12.5), fg);
    let pad = vec2(10.0, 4.0);
    let dot_w = if dot { 12.0 } else { 0.0 };
    let size = galley.size() + pad * 2.0 + vec2(dot_w, 0.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same((size.y / 2.0) as u8), bg);
    if dot {
        p.circle_filled(pos2(rect.left() + pad.x + 3.5, rect.center().y), 3.5, fg);
    }
    p.galley(pos2(rect.left() + pad.x + dot_w, rect.top() + pad.y), galley, fg);
}

/// График скорости чтения и записи за последнюю минуту.
pub fn sparkline(ui: &mut Ui, pal: &Palette, data: &[(f32, f32)], height: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(8), pal.track.gamma_multiply(if pal.dark { 0.7 } else { 0.6 }));
    if data.len() < 2 {
        return;
    }
    let max = data.iter().map(|d| d.0.max(d.1)).fold(8.0 * 1048576.0, f32::max) * 1.15;
    let n = data.len();
    let inner = rect.shrink2(vec2(0.0, 4.0));
    let x_of = |i: usize| inner.left() + inner.width() * i as f32 / (n - 1) as f32;
    let y_of = |v: f32| inner.bottom() - (v / max).clamp(0.0, 1.0) * inner.height();
    for (series, color) in [(1usize, pal.write), (0usize, pal.read)] {
        let pts: Vec<Pos2> =
            (0..n).map(|i| pos2(x_of(i), y_of(if series == 0 { data[i].0 } else { data[i].1 }))).collect();
        let mut m = Mesh::default();
        let fill = color.gamma_multiply(0.16);
        for (i, q) in pts.iter().enumerate() {
            m.colored_vertex(*q, fill);
            m.colored_vertex(pos2(q.x, rect.bottom()), fill);
            if i > 0 {
                let b = (i * 2) as u32;
                m.add_triangle(b - 2, b - 1, b);
                m.add_triangle(b - 1, b, b + 1);
            }
        }
        p.add(Shape::mesh(m));
        p.add(Shape::line(pts, Stroke::new(1.6, color)));
    }
}

/// Строка шага создания диска.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StepMark {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

pub fn step_row(ui: &mut Ui, pal: &Palette, mark: StepMark, text: &str, detail: Option<&str>) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
        let c = rect.center();
        let p = ui.painter();
        match mark {
            StepMark::Done => {
                p.circle_filled(c, 10.0, pal.ok);
                paint_icon(p, rect.shrink(4.0), Icon::Check, Color32::WHITE);
            }
            StepMark::Failed => {
                p.circle_filled(c, 10.0, pal.danger);
                paint_icon(p, rect.shrink(5.0), Icon::Cross, Color32::WHITE);
            }
            StepMark::Running => {
                let t = ui.input(|i| i.time) as f32;
                p.circle_stroke(c, 8.5, Stroke::new(2.5, pal.accent.gamma_multiply(0.2)));
                let mut arc = Vec::new();
                for i in 0..=16 {
                    let a = t * 5.0 + i as f32 * 0.27;
                    arc.push(pos2(c.x + 8.5 * a.cos(), c.y + 8.5 * a.sin()));
                }
                p.add(Shape::line(arc, Stroke::new(2.5, pal.accent)));
                ui.ctx().request_repaint();
            }
            StepMark::Pending | StepMark::Skipped => {
                p.circle_stroke(c, 8.5, Stroke::new(1.5, pal.other));
            }
        }
        let color = match mark {
            StepMark::Pending | StepMark::Skipped => pal.faint,
            StepMark::Failed => pal.danger,
            _ => pal.text,
        };
        let f = if mark == StepMark::Running { bold(14.5) } else { font(14.5) };
        ui.label(RichText::new(text).font(f).color(color));
        if let Some(d) = detail {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(d).font(font(13.0)).color(pal.weak));
            });
        }
    });
}

/// Подпись мелким шрифтом и значение покрупнее.
pub fn stat(ui: &mut Ui, pal: &Palette, label: &str, value: &str, color: Option<Color32>) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if let Some(c) = color {
                let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 4.0, c);
            }
            ui.label(RichText::new(label).font(font(12.5)).color(pal.weak));
        });
        ui.label(RichText::new(value).font(bold(20.0)).color(pal.text));
    });
}

/// Выбор из нескольких вариантов кнопками подряд.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, pal: &Palette, value: &mut T, options: &[(T, &str)], enabled: bool) -> bool {
    let mut changed = false;
    egui::Frame::new()
        .fill(pal.track)
        .corner_radius(CornerRadius::same(9))
        .inner_margin(Margin::same(3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for &(opt, label) in options {
                    let sel = *value == opt;
                    let galley = ui.painter().layout_no_wrap(label.to_string(), if sel { bold(13.5) } else { font(13.5) }, pal.text);
                    let size = vec2(galley.size().x + 28.0, 26.0);
                    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
                    let p = ui.painter();
                    if sel {
                        p.rect(
                            rect,
                            CornerRadius::same(7),
                            pal.card,
                            Stroke::new(1.0, pal.border),
                            StrokeKind::Inside,
                        );
                    } else if enabled && resp.hovered() {
                        p.rect_filled(rect, CornerRadius::same(7), pal.card.gamma_multiply(0.5));
                    }
                    let color = if !enabled { pal.faint } else if sel { pal.text } else { pal.weak };
                    p.text(rect.center(), Align2::CENTER_CENTER, label, if sel { bold(13.5) } else { font(13.5) }, color);
                    if enabled && resp.clicked() && !sel {
                        *value = opt;
                        changed = true;
                    }
                    if enabled && resp.hovered() {
                        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                    }
                }
            });
        });
    changed
}

/// Затухание к цвету фона сверху вниз (край прокручиваемой области).
pub fn fade_down(p: &Painter, rect: Rect, bg: Color32) {
    let mut m = Mesh::default();
    let clear = Color32::from_rgba_premultiplied(0, 0, 0, 0);
    m.colored_vertex(rect.left_top(), clear);
    m.colored_vertex(rect.right_top(), clear);
    m.colored_vertex(rect.left_bottom(), bg);
    m.colored_vertex(rect.right_bottom(), bg);
    m.add_triangle(0, 1, 2);
    m.add_triangle(1, 2, 3);
    p.add(Shape::mesh(m));
}

/// Значок приложения (тот же рисунок, что у vramdisk.exe).
pub fn app_badge(ui: &mut Ui, tex: &egui::TextureHandle, size: f32) {
    ui.add(egui::Image::new((tex.id(), vec2(size, size))));
}

pub fn load_app_icon(ctx: &egui::Context) -> egui::TextureHandle {
    let rgba = include_bytes!("../../assets/icon_128.rgba");
    let img = egui::ColorImage::from_rgba_unmultiplied([128, 128], rgba);
    let opts = egui::TextureOptions { mipmap_mode: Some(egui::TextureFilter::Linear), ..egui::TextureOptions::LINEAR };
    ctx.load_texture("app-icon", img, opts)
}

/// Ползунок размера: дорожка, цветная заливка до ручки, белая ручка.
/// Щелчок по дорожке переносит ручку, стрелки клавиатуры — шаг.
pub fn slider(ui: &mut Ui, pal: &Palette, value: &mut f64, max: f64, step: f64, color: Color32, enabled: bool) -> Response {
    let enabled = enabled && max > 0.0;
    let sense = if enabled { Sense::click_and_drag() } else { Sense::hover() };
    let (rect, mut resp) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), sense);
    let r = 9.0;
    let (x0, x1) = (rect.left() + r, rect.right() - r);
    if enabled {
        let mut v = *value;
        if let Some(p) = resp.interact_pointer_pos() {
            let t = ((p.x - x0) / (x1 - x0).max(1.0)).clamp(0.0, 1.0) as f64;
            v = (t * max / step).round() * step;
        }
        if resp.has_focus() {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    resp.id,
                    egui::EventFilter { horizontal_arrows: true, vertical_arrows: true, ..Default::default() },
                )
            });
            let (dec, inc) = ui.input(|i| {
                (
                    i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::ArrowDown),
                    i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::ArrowUp),
                )
            });
            if dec {
                v = ((v / step).round() - 1.0) * step;
            }
            if inc {
                v = ((v / step).round() + 1.0) * step;
            }
        }
        let v = v.clamp(0.0, max);
        if (v - *value).abs() > 1e-9 {
            *value = v;
            resp.mark_changed();
        }
    }
    let t = if max > 0.0 { (*value / max).clamp(0.0, 1.0) as f32 } else { 0.0 };
    let c = pos2(egui::lerp(x0..=x1, t), rect.center().y);
    let p = ui.painter();
    let rail = Rect::from_min_max(pos2(rect.left(), c.y - 3.0), pos2(rect.right(), c.y + 3.0));
    p.rect_filled(rail, CornerRadius::same(3), pal.track);
    if enabled {
        p.rect_filled(Rect::from_min_max(rail.min, pos2(c.x, rail.max.y)), CornerRadius::same(3), color);
        let dragged = resp.dragged();
        let hot = dragged || resp.hovered();
        let rad = if dragged { r + 1.0 } else { r };
        p.circle_filled(c + vec2(0.0, 1.0), rad + 1.0, Color32::from_black_alpha(if pal.dark { 90 } else { 34 }));
        p.circle(c, rad, Color32::WHITE, Stroke::new(if hot { 3.0 } else { 2.0 }, color));
        if resp.has_focus() {
            p.circle_stroke(c, rad + 4.0, Stroke::new(1.5, color.gamma_multiply(0.55)));
        }
        if hot {
            ui.ctx().set_cursor_icon(if dragged { CursorIcon::Grabbing } else { CursorIcon::Grab });
        }
    } else {
        p.circle(c, r - 1.0, pal.card, Stroke::new(1.5, pal.other));
    }
    resp
}
