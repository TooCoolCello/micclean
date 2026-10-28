//! Interactive graphs: the parametric EQ editor and the compressor and gate level curves.
//! They only run while their page is on screen, and the EQ curve is cached between frames.

use crate::dsp::{self, BANDS, Band, BandKind, Meters, Params, SPEC_BINS, SPEC_LO, SPEC_SPAN, Spec};
use crate::ui;
use eframe::egui::{self, Align2, Color32, CursorIcon, FontId, Mesh, PointerButton, Pos2, Rect, RichText, Sense, Shape, Stroke, Vec2};

const F_LO: f32 = 20.0;
const F_HI: f32 = 20_000.0;
/// The EQ graph spans +-18 dB.
const RANGE: f32 = 18.0;
/// EQ curve resolution, log-spaced over F_LO..F_HI.
const POINTS: usize = 240;
/// One colour per band, like Pro-Q.
const COLORS: [Color32; BANDS] = [
    Color32::from_rgb(255, 122, 92),
    Color32::from_rgb(245, 190, 60),
    Color32::from_rgb(110, 205, 120),
    Color32::from_rgb(80, 170, 250),
    Color32::from_rgb(190, 125, 250),
];
const AMBER: Color32 = Color32::from_rgb(240, 190, 50);

fn hz(f: f32) -> String {
    if f >= 1000.0 { format!("{:.2} kHz", f / 1000.0) } else { format!("{f:.0} Hz") }
}

/// Triangle strip between two polylines of the same length, for filled areas.
fn band_fill(top: impl Iterator<Item = (Pos2, Pos2)>, color: Color32) -> Shape {
    let mut mesh = Mesh::default();
    for (k, (a, b)) in top.enumerate() {
        mesh.colored_vertex(a, color);
        mesh.colored_vertex(b, color);
        if k > 0 {
            let v = 2 * k as u32;
            mesh.add_triangle(v - 2, v - 1, v);
            mesh.add_triangle(v - 1, v, v + 1);
        }
    }
    Shape::mesh(mesh)
}

/// Selection, drag and menu state of the EQ editor, plus its cached curves.
#[derive(Default)]
pub struct EqEditor {
    selected: Option<usize>,
    dragging: Option<usize>,
    menu: Option<usize>,
    menu_open: bool,
    menu_pos: Pos2,
    /// Band responses, rebuilt only when a band changes (not every frame).
    cache_key: Option<[Option<Band>; BANDS]>,
    curves: [Vec<f32>; BANDS],
    sum: Vec<f32>,
}

impl EqEditor {
    fn rebuild(&mut self, bands: &[Option<Band>; BANDS]) {
        if self.cache_key.as_ref() == Some(bands) {
            return;
        }
        let freqs: Vec<f32> = (0..POINTS).map(|i| F_LO * (F_HI / F_LO).powf(i as f32 / (POINTS - 1) as f32)).collect();
        self.sum = vec![0.0; POINTS];
        for (c, b) in self.curves.iter_mut().zip(bands) {
            *c = b.map_or_else(Vec::new, |b| dsp::band_response(&b, &freqs));
            if b.is_some_and(|b| b.on) {
                self.sum.iter_mut().zip(c.iter()).for_each(|(s, g)| *s += g);
            }
        }
        self.cache_key = Some(*bands);
    }

    /// Right-click empty space: add a bell. Right-click a band: its menu. Drag: frequency and
    /// gain. Scroll on a band: Q. The selected band's controls sit under the graph.
    pub fn show(&mut self, ui: &mut egui::Ui, bands: &mut [Option<Band>; BANDS], spec: &Spec) {
        let pal = ui::pal(ui);
        let live = ui.is_enabled();
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 230.0), Sense::click_and_drag());
        let span = (F_HI / F_LO).ln();
        let x_of = |f: f32| rect.left() + rect.width() * ((f / F_LO).ln() / span).clamp(0.0, 1.0);
        let f_of = |x: f32| F_LO * (F_HI / F_LO).powf(((x - rect.left()) / rect.width()).clamp(0.0, 1.0));
        let y_of = |db: f32| rect.center().y - rect.height() * 0.5 * (db / RANGE).clamp(-1.0, 1.0);
        let db_of = |y: f32| ((rect.center().y - y) / (rect.height() * 0.5) * RANGE).clamp(-RANGE, RANGE);
        let node = |b: &Band| Pos2::new(x_of(b.freq), y_of(if b.kind.has_gain() { b.gain_db } else { 0.0 }));
        let hit = |p: Pos2| {
            bands
                .iter()
                .enumerate()
                .filter_map(|(i, b)| b.map(|b| (i, node(&b).distance(p))))
                .filter(|&(_, d)| d < 12.0)
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| i)
        };

        let hovered = resp.hover_pos().and_then(hit);
        if resp.drag_started_by(PointerButton::Primary) {
            self.dragging = ui.input(|i| i.pointer.press_origin()).and_then(hit);
            self.selected = self.dragging.or(self.selected);
        }
        if resp.drag_stopped() {
            self.dragging = None;
        }
        if resp.clicked() {
            self.selected = hovered;
        }
        if let (Some(b), true) = (self.dragging.and_then(|i| bands[i].as_mut()), resp.dragged_by(PointerButton::Primary)) {
            // Move by the pointer delta so grabbing a node off-centre doesn't make it jump.
            let d = resp.drag_delta();
            b.freq = f_of(x_of(b.freq) + d.x);
            if b.kind.has_gain() {
                b.gain_db = db_of(y_of(b.gain_db) + d.y);
            }
        }
        if let Some(b) = hovered.and_then(|i| bands[i].as_mut()) {
            let s = ui.input(|i| i.smooth_scroll_delta.y);
            if s != 0.0 {
                b.q = (b.q * (s * 0.004).exp()).clamp(0.1, 18.0);
                ui.input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO); // don't scroll the page too
            }
        }
        if resp.secondary_clicked() {
            let pos = resp.hover_pos().unwrap_or(rect.center());
            match hovered {
                Some(i) => {
                    (self.menu, self.selected, self.menu_open, self.menu_pos) = (Some(i), Some(i), true, pos);
                }
                None => {
                    if let Some(slot) = bands.iter().position(Option::is_none) {
                        let gain_db = db_of(pos.y);
                        bands[slot] = Some(Band { kind: BandKind::Bell, freq: f_of(pos.x), gain_db, q: 1.0, on: true });
                        self.selected = Some(slot);
                    }
                }
            }
        }

        let mut remove = None;
        let menu = self.menu;
        egui::Popup::from_response(&resp)
            .kind(egui::PopupKind::Menu)
            .open_bool(&mut self.menu_open)
            .at_position(self.menu_pos)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                ui.set_min_width(260.0);
                if let Some(i) = menu
                    && let Some(b) = bands[i].as_mut()
                    && band_controls(ui, i, b)
                {
                    remove = Some(i);
                    ui.close();
                }
            });
        if let Some(i) = remove {
            bands[i] = None;
            self.selected = None;
        }
        self.rebuild(bands);

        let hot = self.dragging.or(hovered);
        if hot.is_some() {
            ui.ctx().set_cursor_icon(if self.dragging.is_some() { CursorIcon::Grabbing } else { CursorIcon::Grab });
        }

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, pal.field);
        for (f, label) in [(50.0, "50"), (100.0, "100"), (200.0, "200"), (500.0, "500"), (1e3, "1k"), (2e3, "2k"), (5e3, "5k"), (1e4, "10k")] {
            let x = x_of(f);
            painter.vline(x, rect.y_range(), Stroke::new(1.0, pal.divider));
            painter.text(Pos2::new(x + 3.0, rect.bottom() - 2.0), Align2::LEFT_BOTTOM, label, FontId::proportional(10.0), pal.text2);
        }
        for db in [-12.0, -6.0, 0.0, 6.0, 12.0] {
            let y = y_of(db);
            painter.hline(rect.x_range(), y, Stroke::new(1.0, if db == 0.0 { pal.control_stroke } else { pal.divider }));
            let t = if db == 0.0 { "0".to_string() } else { format!("{db:+.0}") };
            painter.text(Pos2::new(rect.right() - 4.0, y - 1.0), Align2::RIGHT_BOTTOM, t, FontId::proportional(10.0), pal.text2);
        }

        // Output spectrum behind the curve, on its own -90..0 dBFS scale.
        let y_spec = |db: f32| rect.bottom() - rect.height() * ((db + 90.0) / 90.0).clamp(0.0, 1.0);
        let spec_pts: Vec<Pos2> = (0..SPEC_BINS)
            .map(|i| Pos2::new(x_of(SPEC_LO * SPEC_SPAN.powf((i as f32 + 0.5) / SPEC_BINS as f32)), y_spec(spec.0[i])))
            .collect();
        painter.add(band_fill(spec_pts.iter().map(|&p| (p, Pos2::new(p.x, rect.bottom()))), pal.accent.gamma_multiply(0.18)));
        painter.add(Shape::line(spec_pts, Stroke::new(1.0, pal.accent.gamma_multiply(0.5))));

        let x_at = |k: usize| rect.left() + rect.width() * k as f32 / (POINTS - 1) as f32;
        if let Some(i) = self.selected.filter(|&i| bands[i].is_some()) {
            let zero = y_of(0.0);
            let area = self.curves[i].iter().enumerate().map(|(k, &g)| (Pos2::new(x_at(k), y_of(g)), Pos2::new(x_at(k), zero)));
            painter.add(band_fill(area, COLORS[i].gamma_multiply(0.22)));
        }
        let total: Vec<Pos2> = self.sum.iter().enumerate().map(|(k, &g)| Pos2::new(x_at(k), y_of(g))).collect();
        painter.add(Shape::line(total, Stroke::new(2.0, if live { pal.text } else { pal.text2.gamma_multiply(0.5) })));

        for (i, b) in bands.iter().enumerate() {
            let Some(b) = b else { continue };
            let lit = hot == Some(i) || self.selected == Some(i);
            let fill = if b.on { COLORS[i] } else { COLORS[i].gamma_multiply(0.35) };
            let p = node(b);
            painter.circle(p, if lit { 8.0 } else { 7.0 }, fill, Stroke::new(if lit { 2.0 } else { 0.0 }, pal.text));
            painter.text(p, Align2::CENTER_CENTER, (i + 1).to_string(), FontId::proportional(10.0), Color32::BLACK);
        }
        if let Some(b) = hot.and_then(|i| bands[i]) {
            let gain = if b.kind.has_gain() { format!(" · {:+.1} dB", b.gain_db) } else { String::new() };
            let text = format!("{} · {}{gain} · Q {:.2}", b.kind.name(), hz(b.freq), b.q);
            let galley = painter.layout_no_wrap(text, FontId::proportional(12.0), pal.text);
            let p = node(&b);
            let size = galley.size() + Vec2::new(12.0, 6.0);
            let x = p.x.clamp(rect.left() + size.x / 2.0, rect.right() - size.x / 2.0);
            let y = if p.y - 14.0 - size.y > rect.top() { p.y - 14.0 - size.y / 2.0 } else { p.y + 14.0 + size.y / 2.0 };
            let r = Rect::from_center_size(Pos2::new(x, y), size);
            painter.rect_filled(r, 4.0, pal.card);
            painter.galley(r.min + Vec2::new(6.0, 3.0), galley, pal.text);
        }

        ui.add_space(8.0);
        match self.selected {
            Some(i) if bands[i].is_some() => {
                if bands[i].as_mut().is_some_and(|b| band_controls(ui, i, b)) {
                    bands[i] = None;
                    self.selected = None;
                }
            }
            _ => {
                let used = bands.iter().flatten().count();
                let tip = if used < BANDS {
                    "Right-click the graph to add a band. Drag a band to move it, scroll on it for Q, right-click it for options."
                } else {
                    "All 5 bands in use. Right-click one to change or remove it."
                };
                ui.label(RichText::new(tip).small().color(pal.text2));
            }
        }
    }
}

/// Shape, frequency, gain, Q and on/off of one band: the right-click menu and the strip under
/// the graph. Returns true when Remove was clicked.
fn band_controls(ui: &mut egui::Ui, i: usize, b: &mut Band) -> bool {
    let mut remove = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("● Band {}", i + 1)).color(COLORS[i]).font(ui::semibold(14.0)));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            remove = ui.button("Remove").clicked();
            ui::toggle(ui, &mut b.on).on_hover_text("Bypass this band");
        });
    });
    ui.horizontal_wrapped(|ui| {
        for k in BandKind::ALL {
            ui.selectable_value(&mut b.kind, k, k.name());
        }
    });
    egui::Grid::new(("band", i)).num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
        ui.label("Frequency");
        ui.add(egui::Slider::new(&mut b.freq, F_LO..=F_HI).logarithmic(true).suffix(" Hz").fixed_decimals(0));
        ui.end_row();
        ui.label("Gain");
        ui.add_enabled(b.kind.has_gain(), egui::Slider::new(&mut b.gain_db, -RANGE..=RANGE).suffix(" dB").fixed_decimals(1));
        ui.end_row();
        ui.label("Q");
        ui.add(egui::Slider::new(&mut b.q, 0.1..=18.0).logarithmic(true).fixed_decimals(2));
        ui.end_row();
    });
    remove
}

/// Square input -> output level graph shared by the compressor and the gate. `curve` maps a
/// steady input level to the output, `dot` is the live (input, output) and `close` an extra
/// dashed threshold. Dragging across it moves the threshold, which is returned.
fn level_graph(
    ui: &mut egui::Ui,
    lo: f32,
    step: f32,
    thresh: f32,
    close: Option<f32>,
    curve: impl Fn(f32) -> f32,
    dot: Option<(f32, f32, Color32)>,
) -> Option<f32> {
    let pal = ui::pal(ui);
    let live = ui.is_enabled();
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(200.0), Sense::drag());
    let resp = resp.on_hover_cursor(CursorIcon::ResizeHorizontal);
    let x_of = |db: f32| rect.left() + rect.width() * ((db - lo) / -lo).clamp(0.0, 1.0);
    let y_of = |db: f32| rect.bottom() - rect.height() * ((db - lo) / -lo).clamp(0.0, 1.0);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, pal.field);
    let mut g = lo + step;
    while g < 0.0 {
        painter.vline(x_of(g), rect.y_range(), Stroke::new(1.0, pal.divider));
        painter.hline(rect.x_range(), y_of(g), Stroke::new(1.0, pal.divider));
        let t = format!("{g:.0}");
        painter.text(Pos2::new(x_of(g) + 2.0, rect.bottom() - 2.0), Align2::LEFT_BOTTOM, &t, FontId::proportional(10.0), pal.text2);
        painter.text(Pos2::new(rect.left() + 3.0, y_of(g) - 1.0), Align2::LEFT_BOTTOM, t, FontId::proportional(10.0), pal.text2);
        g += step;
    }
    painter.text(rect.right_bottom() + Vec2::new(-4.0, -2.0), Align2::RIGHT_BOTTOM, "in", FontId::proportional(10.0), pal.text2);
    painter.text(rect.left_top() + Vec2::new(4.0, 2.0), Align2::LEFT_TOP, "out", FontId::proportional(10.0), pal.text2);
    let dash = |db: f32, color: Color32| {
        painter.extend(Shape::dashed_line(&[Pos2::new(x_of(db), rect.top()), Pos2::new(x_of(db), rect.bottom())], Stroke::new(1.0, color), 4.0, 4.0));
    };
    painter.extend(Shape::dashed_line(&[rect.left_bottom(), rect.right_top()], Stroke::new(1.0, pal.control_stroke), 3.0, 3.0));
    if let Some(c) = close {
        dash(c, pal.text2);
    }
    dash(thresh, AMBER.gamma_multiply(0.7));
    let pts: Vec<Pos2> = (0..=(-lo as i32)).map(|k| lo + k as f32).map(|i| Pos2::new(x_of(i), y_of(curve(i)))).collect();
    painter.add(Shape::line(pts, Stroke::new(2.0, if live { AMBER } else { AMBER.gamma_multiply(0.3) })));
    if let Some((i, o, color)) = dot.filter(|&(i, ..)| live && i > lo) {
        painter.circle(Pos2::new(x_of(i), y_of(o)), 5.0, color, Stroke::new(1.5, pal.field));
    }
    let pos = resp.interact_pointer_pos().filter(|_| resp.dragged())?;
    Some((lo + (pos.x - rect.left()) / rect.width() * -lo).round())
}

/// Compressor curve with the live level, plus a gain-reduction bar. Returns a dragged threshold.
pub fn comp_graph(ui: &mut egui::Ui, p: &Params, m: &Meters) -> Option<f32> {
    let pal = ui::pal(ui);
    ui.horizontal(|ui| {
        let dot = (m.comp_in_db, m.comp_in_db - m.gr_db + p.comp_makeup_db, pal.text);
        let t = level_graph(ui, -60.0, 12.0, p.comp_threshold_db, None, |i| dsp::comp_curve_db(p, i), Some(dot));
        let (bar, _) = ui.allocate_exact_size(Vec2::new(10.0, 200.0), Sense::hover());
        ui.painter().rect_filled(bar, 3.0, pal.field);
        let h = bar.height() * (m.gr_db / 24.0).clamp(0.0, 1.0);
        ui.painter().rect_filled(Rect::from_min_size(bar.min, Vec2::new(bar.width(), h)), 3.0, AMBER);
        ui.vertical(|ui| {
            ui.label(RichText::new(format!("-{:.1} dB", m.gr_db)).font(ui::semibold(20.0)));
            ui.label(RichText::new("gain reduction").small().color(pal.text2));
            ui.add_space(8.0);
            ui.label(RichText::new("Drag across the graph to move the threshold.").small().color(pal.text2));
        });
        t
    })
    .inner
}

/// Gate curve (open above the threshold, "volume while muted" below) with the live level, which
/// turns green while the gate is open. Returns a dragged threshold.
pub fn gate_graph(ui: &mut egui::Ui, p: &Params, m: &Meters) -> Option<f32> {
    let pal = ui::pal(ui);
    ui.horizontal(|ui| {
        let thr = p.gate_threshold_db;
        let color = if m.gate_open { pal.good } else { pal.text2 };
        let dot = (m.gate_level_db, m.gate_level_db + m.gate_gain_db, color);
        let close = (p.gate_hysteresis_db > 0.0).then_some(thr - p.gate_hysteresis_db);
        let curve = |i: f32| if i >= thr { i } else { i + p.gate_floor_db };
        let t = level_graph(ui, -80.0, 20.0, thr, close, curve, Some(dot));
        ui.vertical(|ui| {
            let state = if m.gate_open { "Open" } else { "Muted" };
            ui.label(RichText::new(state).font(ui::semibold(20.0)).color(color));
            ui.label(RichText::new(m.reason).small().color(pal.text2));
            ui.add_space(8.0);
            ui.label(RichText::new("Yellow dashes: opens above. Grey dashes: closes below.").small().color(pal.text2));
            ui.label(RichText::new("Drag across the graph to move the threshold.").small().color(pal.text2));
        });
        t
    })
    .inner
}
