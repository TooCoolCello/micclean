//! Interactive graphs and visualizers: the parametric EQ editor, compressor and gate curves,
//! meters, histories and the signal-chain boxes. They only run while their page is on screen,
//! and the EQ curve is cached between frames.

use crate::dsp::{self, BANDS, Band, BandKind, Meters, Params, SPEC_BINS, SPEC_LO, SPEC_SPAN, Spec};
use crate::ui;
use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontId, Mesh, PointerButton, Pos2, Rect, RichText, Sense, Shape, Stroke, StrokeKind, UiBuilder,
    Vec2,
};
use std::time::{Duration, Instant};

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
pub const AMBER: Color32 = Color32::from_rgb(240, 190, 50);

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

/// Scrolling history sampled every `every`, kept GUI-side in a fixed array (no allocation, and
/// the audio thread never sees it).
pub struct History<T: Copy, const N: usize> {
    v: [T; N],
    pos: usize,
    next: Instant,
    every: Duration,
}

impl<T: Copy, const N: usize> History<T, N> {
    pub fn new(fill: T, every_ms: u64) -> Self {
        Self { v: [fill; N], pos: 0, next: Instant::now(), every: Duration::from_millis(every_ms) }
    }

    pub fn push(&mut self, x: T, now: Instant) {
        if now < self.next {
            return;
        }
        self.next = now + self.every;
        self.v[self.pos] = x;
        self.pos = (self.pos + 1) % N;
    }

    /// Oldest first.
    fn iter(&self) -> impl Iterator<Item = T> + '_ {
        self.v[self.pos..].iter().chain(&self.v[..self.pos]).copied()
    }
}

/// Horizontal bar from `lo` to `hi` with an optional marker line (a threshold or a ceiling).
pub fn meter(ui: &mut egui::Ui, width: f32, value: f32, lo: f32, hi: f32, mark: Option<f32>, color: Color32) {
    let pal = ui::pal(ui);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 8.0), Sense::hover());
    let frac = |v: f32| ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
    ui.painter().rect_filled(rect, 4.0, pal.control_stroke);
    let f = frac(value);
    if f > 0.0 {
        ui.painter().rect_filled(Rect::from_min_size(rect.min, Vec2::new(rect.width() * f, rect.height())), 4.0, color);
    }
    if let Some(m) = mark {
        let x = rect.left() + rect.width() * frac(m);
        ui.painter().vline(x, rect.y_range().expand(3.0), Stroke::new(2.0, pal.text));
    }
}

/// Full-width level bar over a shaded band for where it should sit, e.g. a good talking level.
pub fn zone_meter(ui: &mut egui::Ui, width: f32, value: f32, lo: f32, hi: f32, zone: (f32, f32), color: Color32) {
    let pal = ui::pal(ui);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 12.0), Sense::hover());
    let x = |v: f32| rect.left() + rect.width() * ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
    let painter = ui.painter();
    painter.rect_filled(rect, 6.0, pal.control_stroke);
    painter.rect_filled(Rect::from_x_y_ranges(x(zone.0)..=x(zone.1), rect.y_range()), 0.0, pal.good.gamma_multiply(0.35));
    let y = rect.center().y;
    if value > lo {
        painter.rect_filled(Rect::from_x_y_ranges(rect.left()..=x(value), y - 3.0..=y + 3.0), 3.0, color);
    }
}

/// A row with a meter and a readout, e.g. "Sibilance  [=====|   ]  -18 dB".
#[allow(clippy::too_many_arguments)]
pub fn meter_row(ui: &mut egui::Ui, label: &str, value: f32, lo: f32, hi: f32, mark: Option<f32>, color: Color32, text: &str) {
    ui::row(ui, label, |ui| {
        ui.add_sized([72.0, 18.0], egui::Label::new(RichText::new(text).small()));
        let w = ui.available_width().min(260.0);
        meter(ui, w, value, lo, hi, mark, color);
    });
}

/// Mic spectrum against the denoised one, with what noise removal took out shaded red.
pub fn denoise_view(ui: &mut egui::Ui, m: &Meters) {
    let pal = ui::pal(ui);
    let live = ui.is_enabled();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 110.0), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, pal.field);
    let x = |i: usize| rect.left() + rect.width() * (i as f32 + 0.5) / SPEC_BINS as f32;
    let y = |db: f32| rect.bottom() - rect.height() * ((db + 90.0) / 90.0).clamp(0.0, 1.0);
    let clean = |i: usize| if live { m.spec_clean.0[i] } else { m.spec_in.0[i] };
    let removed = (0..SPEC_BINS).map(|i| (Pos2::new(x(i), y(m.spec_in.0[i].max(clean(i)))), Pos2::new(x(i), y(clean(i)))));
    painter.add(band_fill(removed, Color32::from_rgb(232, 72, 72).gamma_multiply(0.35)));
    let floor = (0..SPEC_BINS).map(|i| (Pos2::new(x(i), y(clean(i))), Pos2::new(x(i), rect.bottom())));
    painter.add(band_fill(floor, pal.accent.gamma_multiply(0.3)));
    painter.add(Shape::line((0..SPEC_BINS).map(|i| Pos2::new(x(i), y(m.spec_in.0[i]))).collect(), Stroke::new(1.0, pal.text2)));
    for (f, label) in [(100.0, "100"), (1000.0, "1k"), (10_000.0, "10k")] {
        let fx = rect.left() + rect.width() * (f / SPEC_LO).ln() / SPEC_SPAN.ln();
        painter.vline(fx, rect.y_range(), Stroke::new(1.0, pal.divider));
        painter.text(Pos2::new(fx + 3.0, rect.bottom() - 2.0), Align2::LEFT_BOTTOM, label, FontId::proportional(10.0), pal.text2);
    }
    let text = if live { format!("Removing {:.0} dB", m.denoise_db) } else { "Off".into() };
    painter.text(rect.left_top() + Vec2::new(8.0, 6.0), Align2::LEFT_TOP, text, FontId::proportional(13.0), pal.text);
    painter.text(rect.right_top() + Vec2::new(-8.0, 6.0), Align2::RIGHT_TOP, "grey: mic · blue: kept · red: removed", FontId::proportional(10.0), pal.text2);
}

/// Line graph of a history, oldest on the left. `zero` draws a reference line there.
pub fn history_graph<const N: usize>(ui: &mut egui::Ui, h: &History<f32, N>, lo: f32, hi: f32, zero: Option<f32>, color: Color32, span: &str) {
    let pal = ui::pal(ui);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 80.0), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, pal.field);
    let y = |v: f32| rect.bottom() - rect.height() * ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
    if let Some(z) = zero {
        painter.hline(rect.x_range(), y(z), Stroke::new(1.0, pal.control_stroke));
    }
    let pts: Vec<Pos2> = h.iter().enumerate().map(|(k, v)| Pos2::new(rect.left() + rect.width() * k as f32 / (N - 1) as f32, y(v))).collect();
    painter.add(Shape::line(pts, Stroke::new(2.0, if ui.is_enabled() { color } else { pal.text2 })));
    let small = FontId::proportional(10.0);
    painter.text(rect.left_top() + Vec2::new(4.0, 2.0), Align2::LEFT_TOP, format!("{hi:+.0} dB"), small.clone(), pal.text2);
    painter.text(rect.left_bottom() + Vec2::new(4.0, -2.0), Align2::LEFT_BOTTOM, format!("{lo:+.0} dB"), small.clone(), pal.text2);
    painter.text(rect.right_bottom() + Vec2::new(-4.0, -2.0), Align2::RIGHT_BOTTOM, span, small, pal.text2);
}

/// "A3 +12 cents" for a frequency.
pub fn note_name(f: f32) -> String {
    const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    let midi = 69.0 + 12.0 * (f / 440.0).log2();
    let n = midi.round() as i32;
    format!("{}{} {:+.0}¢", NAMES[n.rem_euclid(12) as usize], n.div_euclid(12) - 1, (midi - n as f32) * 100.0)
}

/// The detected pitch over the last few seconds on a note grid. Green while the gate was open.
pub fn pitch_view<const N: usize>(ui: &mut egui::Ui, h: &History<(f32, bool), N>, m: &Meters, span: &str) {
    let pal = ui::pal(ui);
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_width(110.0);
            let (note, hz) = m.pitch_hz.map_or(("-".to_string(), String::new()), |f| (note_name(f), format!("{f:.0} Hz")));
            ui.label(RichText::new(note).font(ui::semibold(22.0)));
            ui.label(RichText::new(hz).small().color(pal.text2));
            let state = match m.reason {
                "singing" => "holding your note",
                "voice" | "learned voice" => "voice",
                _ if m.pitch_hz.is_some() => "pitched, not voice",
                _ => "no pitch",
            };
            ui.label(RichText::new(state).small().color(if m.gate_open { pal.good } else { pal.text2 }));
        });
        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 120.0), Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, pal.field);
        let (lo, hi) = (65.0f32.log2(), 1100.0f32.log2());
        let y = |f: f32| rect.bottom() - rect.height() * ((f.log2() - lo) / (hi - lo));
        for (octave, f) in [(2, 65.41f32), (3, 130.81), (4, 261.63), (5, 523.25), (6, 1046.5)] {
            painter.hline(rect.x_range(), y(f), Stroke::new(1.0, pal.divider));
            painter.text(Pos2::new(rect.left() + 3.0, y(f) - 1.0), Align2::LEFT_BOTTOM, format!("C{octave}"), FontId::proportional(10.0), pal.text2);
        }
        let x = |k: usize| rect.left() + rect.width() * k as f32 / (N - 1) as f32;
        let mut prev: Option<(Pos2, bool)> = None;
        for (k, (f, open)) in h.iter().enumerate() {
            let p = (f.is_finite()).then(|| Pos2::new(x(k), y(f)));
            if let (Some((a, _)), Some(b)) = (prev, p) {
                // A jump of over half an octave is a new note, not a line to draw.
                if (a.y - b.y).abs() < rect.height() * 0.5 / (hi - lo) {
                    painter.line_segment([a, b], Stroke::new(2.5, if open { pal.good } else { pal.text2 }));
                }
            }
            prev = p.map(|p| (p, open));
        }
        painter.text(rect.right_bottom() + Vec2::new(-4.0, -2.0), Align2::RIGHT_BOTTOM, span, FontId::proportional(10.0), pal.text2);
    });
}

/// One box of the signal-chain strip: name, activity light, live value and (if it can be
/// switched) its on/off toggle. Clicking the box elsewhere opens its settings; the open one
/// gets an accent outline.
pub fn stage_box(
    ui: &mut egui::Ui,
    w: f32,
    name: &str,
    on: Option<&mut bool>,
    selected: bool,
    active: bool,
    level: f32,
    value: &str,
) -> egui::Response {
    let pal = ui::pal(ui);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 64.0), Sense::click());
    let painter = ui.painter();
    let fill = if resp.hovered() { pal.control_hover } else { pal.card };
    let stroke = if selected { Stroke::new(2.0, pal.accent) } else { Stroke::new(1.0, pal.card_stroke) };
    painter.rect(rect, 6.0, fill, stroke, StrokeKind::Inside);
    let text = if active { pal.text } else { pal.text2 };
    painter.text(rect.left_top() + Vec2::new(10.0, 8.0), Align2::LEFT_TOP, name, ui::semibold(13.0), text);
    let light = if active { pal.good.gamma_multiply(0.2 + 0.8 * level.clamp(0.0, 1.0)) } else { pal.control_stroke };
    painter.circle_filled(Pos2::new(rect.right() - 12.0, rect.top() + 15.0), 5.0, light);
    painter.text(rect.left_bottom() + Vec2::new(10.0, -9.0), Align2::LEFT_BOTTOM, value, FontId::proportional(11.0), pal.text2);
    if let Some(on) = on {
        let r = Rect::from_min_size(Pos2::new(rect.right() - 50.0, rect.bottom() - 28.0), Vec2::new(40.0, 20.0));
        // A child that isn't allocated in the row, so the toggle doesn't push the next box.
        ui::toggle(&mut ui.new_child(UiBuilder::new().max_rect(r)), on);
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}
