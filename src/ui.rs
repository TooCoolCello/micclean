//! Windows 11 (Fluent) look for egui: the system's own fonts and accent colour, light and dark
//! palettes, a title bar that matches, and the few widgets egui lacks (switch, cards, nav items).

use eframe::egui::{
    self, Align, Color32, FontData, FontDefinitions, FontFamily, FontId, Layout, Margin, Pos2, Rect, Response,
    RichText, Role, Sense, Stroke, StrokeKind, TextStyle, Theme, Ui, Vec2, WidgetInfo,
};
use std::sync::{Arc, OnceLock};

static ACCENT: OnceLock<Color32> = OnceLock::new();

/// Segoe Fluent Icons code points.
pub mod icon {
    pub const HOME: &str = "\u{E80F}";
    pub const MIC: &str = "\u{E720}";
    pub const SPEAKERS: &str = "\u{E7F5}";
    pub const HEADPHONE: &str = "\u{E7F6}";
    pub const VOLUME: &str = "\u{E767}";
    pub const MUTE: &str = "\u{E74F}";
    pub const LEARN: &str = "\u{E7BE}";
    pub const EQ: &str = "\u{E9E9}";
    pub const RECORD: &str = "\u{E7C8}";
    pub const PLAY: &str = "\u{E768}";
    pub const STOP: &str = "\u{E71A}";
    pub const SETTINGS: &str = "\u{E713}";
    pub const DELETE: &str = "\u{E74D}";
    pub const REFRESH: &str = "\u{E72C}";
    pub const INFO: &str = "\u{E946}";
    pub const MUSIC: &str = "\u{E8D6}";
    pub const POWER: &str = "\u{E7E8}";
    pub const MENU: &str = "\u{E700}";
    pub const CLOSE: &str = "\u{E711}";
    pub const WARNING: &str = "\u{E7BA}";
    pub const FOLDER: &str = "\u{E838}";
}

pub struct Palette {
    pub bg: Color32,
    pub card: Color32,
    pub card_stroke: Color32,
    pub control: Color32,
    pub control_hover: Color32,
    pub control_pressed: Color32,
    pub control_stroke: Color32,
    pub field: Color32,
    pub divider: Color32,
    pub text: Color32,
    pub text2: Color32,
    pub nav_hover: Color32,
    pub nav_selected: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    pub good: Color32,
}

impl Palette {
    pub fn of(dark: bool) -> Self {
        let base = *ACCENT.get().unwrap_or(&Color32::from_rgb(0, 120, 212));
        let g = Color32::from_gray;
        if dark {
            Self {
                bg: g(32),
                card: g(43),
                card_stroke: g(29),
                control: g(45),
                control_hover: g(50),
                control_pressed: g(39),
                control_stroke: g(58),
                field: g(28),
                divider: g(33),
                text: g(255),
                text2: g(197),
                nav_hover: g(41),
                nav_selected: g(45),
                // Windows lightens the accent on dark backgrounds.
                accent: base.lerp_to_gamma(Color32::WHITE, 0.45),
                on_accent: g(0),
                good: Color32::from_rgb(108, 203, 95),
            }
        } else {
            Self {
                bg: g(243),
                card: g(251),
                card_stroke: g(229),
                control: g(251),
                control_hover: g(246),
                control_pressed: g(245),
                control_stroke: g(222),
                field: g(255),
                divider: g(234),
                text: g(27),
                text2: g(95),
                nav_hover: g(234),
                nav_selected: g(234),
                accent: base.lerp_to_gamma(Color32::BLACK, 0.15),
                on_accent: g(255),
                good: Color32::from_rgb(15, 123, 15),
            }
        }
    }
}

pub fn pal(ui: &Ui) -> Palette {
    Palette::of(ui.visuals().dark_mode)
}

/// Same palette from the context, for dialogs drawn outside any Ui.
pub fn pal_ctx(ctx: &egui::Context) -> Palette {
    Palette::of(ctx.theme() == egui::Theme::Dark)
}

/// Fonts, accent and both palettes. Fonts come from C:\Windows\Fonts, so nothing is bundled.
pub fn install(ctx: &egui::Context) {
    let _ = ACCENT.set(system_accent().unwrap_or(Color32::from_rgb(0, 120, 212)));

    let mut fonts = FontDefinitions::default();
    let fallback = fonts.families[&FontFamily::Proportional].clone();
    let mut add = |key: &str, files: &[&str]| -> Vec<String> {
        let found = files.iter().find_map(|f| std::fs::read(format!(r"C:\Windows\Fonts\{f}")).ok());
        let mut list = fallback.clone();
        if let Some(bytes) = found {
            fonts.font_data.insert(key.into(), Arc::new(FontData::from_owned(bytes)));
            list.insert(0, key.into());
        }
        list
    };
    // Windows 10 lacks the Win11 files; the older Segoe ones look nearly the same.
    let body = add("ui", &["SegUIVar.ttf", "segoeui.ttf"]);
    let semibold = add("semibold", &["seguisb.ttf"]);
    let icons = add("icons", &["SegoeIcons.ttf", "segmdl2.ttf"]);
    fonts.families.insert(FontFamily::Proportional, body);
    fonts.families.insert(FontFamily::Name("semibold".into()), semibold);
    fonts.families.insert(FontFamily::Name("icons".into()), icons);
    ctx.set_fonts(fonts);

    for theme in [Theme::Dark, Theme::Light] {
        ctx.style_mut_of(theme, |s| {
            let p = Palette::of(theme == Theme::Dark);
            s.text_styles = [
                (TextStyle::Small, FontId::proportional(12.0)),
                (TextStyle::Body, FontId::proportional(14.0)),
                (TextStyle::Button, FontId::proportional(14.0)),
                (TextStyle::Monospace, FontId::monospace(13.0)),
                (TextStyle::Heading, FontId::new(28.0, FontFamily::Name("semibold".into()))),
            ]
            .into();
            s.spacing.item_spacing = Vec2::new(8.0, 6.0);
            s.spacing.button_padding = Vec2::new(12.0, 5.0);
            s.spacing.interact_size.y = 30.0;
            s.spacing.slider_width = 180.0;
            s.spacing.slider_rail_height = 4.0;
            s.spacing.combo_width = 260.0;

            let v = &mut s.visuals;
            v.dark_mode = theme == Theme::Dark;
            v.panel_fill = p.bg;
            v.window_fill = p.card;
            v.window_stroke = Stroke::new(1.0, p.card_stroke);
            v.window_corner_radius = 8.into();
            v.menu_corner_radius = 8.into();
            v.extreme_bg_color = p.field;
            v.text_edit_bg_color = Some(p.field);
            v.selection.bg_fill = p.accent;
            v.selection.stroke = Stroke::new(1.0, p.on_accent);
            v.hyperlink_color = p.accent;
            v.slider_trailing_fill = true;
            v.override_text_color = Some(p.text);
            let w = &mut v.widgets;
            for (wv, fill) in [
                (&mut w.noninteractive, p.card),
                (&mut w.inactive, p.control),
                (&mut w.hovered, p.control_hover),
                (&mut w.active, p.control_pressed),
                (&mut w.open, p.control_hover),
            ] {
                wv.bg_fill = fill;
                wv.weak_bg_fill = fill;
                wv.bg_stroke = Stroke::new(1.0, p.control_stroke);
                wv.fg_stroke = Stroke::new(1.0, p.text);
                wv.corner_radius = 4.into();
                wv.expansion = 0.0;
            }
            w.noninteractive.bg_stroke = Stroke::new(1.0, p.divider);
        });
    }
}

/// Windows' accent colour (HKCU\...\DWM\AccentColor is 0xAABBGGRR).
fn system_accent() -> Option<Color32> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let (key, name) = (wide(r"Software\Microsoft\Windows\DWM"), wide("AccentColor"));
    let (mut data, mut size) = (0u32, 4u32);
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    (rc == 0).then(|| Color32::from_rgb(data as u8, (data >> 8) as u8, (data >> 16) as u8))
}

/// Dark/light title bar, painted the same colour as the window so it reads as one surface.
pub fn title_bar(hwnd: isize, dark: bool) {
    use windows_sys::Win32::Graphics::Dwm::{DWMWA_CAPTION_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
    let bg = Palette::of(dark).bg;
    let dark = dark as i32;
    let colorref = bg.r() as u32 | (bg.g() as u32) << 8 | (bg.b() as u32) << 16;
    // Both are no-ops on Windows 10 builds that lack them, which is fine.
    unsafe {
        DwmSetWindowAttribute(hwnd as _, DWMWA_USE_IMMERSIVE_DARK_MODE as u32, (&dark as *const i32).cast(), 4);
        DwmSetWindowAttribute(hwnd as _, DWMWA_CAPTION_COLOR as u32, (&colorref as *const u32).cast(), 4);
    }
}

pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("icons".into()))
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

/// Fluent toggle switch.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(40.0, 20.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    resp.widget_info(|| WidgetInfo::selected(Role::Switch, ui.is_enabled(), *on, ""));
    if ui.is_rect_visible(rect) {
        let p = pal(ui);
        let t = ui.ctx().animate_bool_responsive(resp.id, *on);
        let r = rect.height() / 2.0;
        let painter = ui.painter();
        if *on {
            painter.rect_filled(rect, r, p.accent);
        } else {
            painter.rect(rect, r, p.control, Stroke::new(1.0, p.text2), StrokeKind::Inside);
        }
        let knob = if *on { p.on_accent } else { p.text2 };
        let x = egui::lerp((rect.left() + r)..=(rect.right() - r), t);
        painter.circle_filled(Pos2::new(x, rect.center().y), if resp.hovered() { 7.0 } else { 6.0 }, knob);
    }
    resp
}

/// Rounded card on the window background.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = pal(ui);
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.card_stroke))
        .corner_radius(6)
        .inner_margin(Margin::symmetric(16, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// Settings-app row: icon, title and grey description on the left, control on the right.
pub fn header(ui: &mut Ui, icon: &str, title: &str, desc: &str, control: impl FnOnce(&mut Ui)) {
    let p = pal(ui);
    ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 44.0), Layout::right_to_left(Align::Center), |ui| {
        control(ui);
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            if !icon.is_empty() {
                ui.label(RichText::new(icon).font(icon_font(18.0)));
                ui.add_space(8.0);
            }
            // Text wraps in whatever the control left over; unwrapped it would widen the page
            // and push the controls off the right edge.
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.add(egui::Label::new(title).truncate());
                if !desc.is_empty() {
                    ui.add(egui::Label::new(RichText::new(desc).small().color(p.text2)).wrap());
                }
            });
        });
    });
}

/// One card holding one setting.
pub fn setting(ui: &mut Ui, icon: &str, title: &str, desc: &str, control: impl FnOnce(&mut Ui)) {
    card(ui, |ui| header(ui, icon, title, desc, control));
}

/// A card with an optional on/off switch in its header and sub-settings below,
/// greyed out while the switch is off.
pub fn group(ui: &mut Ui, icon: &str, title: &str, desc: &str, on: Option<&mut bool>, body: impl FnOnce(&mut Ui)) {
    card(ui, |ui| {
        let mut enabled = true;
        header(ui, icon, title, desc, |ui| {
            if let Some(on) = on {
                toggle(ui, on);
                enabled = *on;
            }
        });
        ui.add_enabled_ui(enabled, body);
    });
}

/// A labelled line inside a group, with a divider above it.
pub fn row(ui: &mut Ui, label: &str, control: impl FnOnce(&mut Ui)) {
    let p = pal(ui);
    let top = ui.cursor().top();
    let x = ui.max_rect().x_range();
    ui.painter().hline(x, top, Stroke::new(1.0, p.divider));
    ui.add_space(2.0);
    ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 36.0), Layout::right_to_left(Align::Center), |ui| {
        control(ui);
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| ui.add(egui::Label::new(label).truncate()));
    });
}

/// Filled accent button for the main action on a page.
pub fn accent_button(ui: &mut Ui, enabled: bool, text: impl Into<String>) -> Response {
    let p = pal(ui);
    ui.add_enabled(enabled, egui::Button::new(RichText::new(text).color(p.on_accent)).fill(p.accent))
}

/// Icon + text button.
pub fn icon_button(ui: &mut Ui, enabled: bool, icon: &str, text: &str) -> Response {
    let mut job = egui::text::LayoutJob::default();
    let color = ui.visuals().text_color();
    job.append(icon, 0.0, egui::TextFormat::simple(icon_font(14.0), color));
    job.append(text, 8.0, egui::TextFormat::simple(FontId::proportional(14.0), color));
    ui.add_enabled(enabled, egui::Button::new(job))
}

/// Thin rounded level meter, -60..0 dB.
pub fn level(ui: &mut Ui, db: f32) {
    let p = pal(ui);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), Sense::hover());
    ui.painter().rect_filled(rect, 3.0, p.control_stroke);
    let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    if frac > 0.0 {
        let fill = Rect::from_min_size(rect.min, Vec2::new(rect.width() * frac, rect.height()));
        let col = if db > -1.0 { Color32::from_rgb(232, 72, 72) } else { p.accent };
        ui.painter().rect_filled(fill, 3.0, col);
    }
}

/// Small rounded status label.
pub fn pill(ui: &mut Ui, text: &str, fill: Color32, color: Color32) {
    egui::Frame::new()
        .fill(fill)
        .corner_radius(10)
        .inner_margin(Margin::symmetric(10, 2))
        .show(ui, |ui| ui.label(RichText::new(text).small().color(color)));
}

/// NavigationView item: subtle fill when selected or hovered, accent bar on the selected one.
pub fn nav_item(ui: &mut Ui, icon: &str, label: &str, selected: bool, compact: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 36.0), Sense::click());
    resp.widget_info(|| WidgetInfo::selected(Role::Tab, true, selected, label));
    let p = pal(ui);
    let painter = ui.painter();
    if selected || resp.hovered() {
        painter.rect_filled(rect, 4.0, if selected { p.nav_selected } else { p.nav_hover });
    }
    if selected {
        let bar = Rect::from_center_size(Pos2::new(rect.left() + 1.5, rect.center().y), Vec2::new(3.0, 16.0));
        painter.rect_filled(bar, 1.5, p.accent);
    }
    let y = rect.center().y;
    painter.text(Pos2::new(rect.left() + 16.0, y), egui::Align2::LEFT_CENTER, icon, icon_font(16.0), p.text);
    if !compact {
        painter.text(Pos2::new(rect.left() + 48.0, y), egui::Align2::LEFT_CENTER, label, FontId::proportional(14.0), p.text);
    }
    if compact { resp.on_hover_text(label) } else { resp }
}
