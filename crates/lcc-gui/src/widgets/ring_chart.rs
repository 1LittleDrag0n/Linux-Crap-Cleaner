//! Ring chart (sunburst) of a folder: the inner ring is the folder's contents,
//! the outer rings go two levels deeper. Hover shows a segment's name and size
//! in the middle; clicking a folder opens it; clicking the middle goes up.
//!
//! Colors: the eight largest items get the eight categorical colors from the
//! validated reference palette (separate light and dark sets); deeper rings
//! reuse their parent's color, fainter; everything else is neutral gray. The
//! folder list beside the chart shows the same colors as swatches, so the
//! list is the chart's legend and table view.

use std::cell::RefCell;
use std::f64::consts::{FRAC_PI_2, TAU};
use std::ffi::OsString;
use std::rc::Rc;

use lcc_core::format::{count, human_size};
use lcc_core::scan::{DirNode, Entry, EntryKind};
use relm4::gtk::prelude::*;
use relm4::{adw, gtk};

/// Categorical palette, light mode (fixed order; it's the CVD-safety order).
const LIGHT: [u32; 8] = [
    0x2a78d6, 0xeb6834, 0x1baf7a, 0xeda100, 0xe87ba4, 0x008300, 0x6250d6, 0xe34948,
];
/// Same eight hues stepped for the dark surface.
const DARK: [u32; 8] = [
    0x3987e5, 0xd95926, 0x199e70, 0xc98500, 0xd55181, 0x008300, 0x9085e9, 0xe66767,
];
const NEUTRAL_LIGHT: u32 = 0xa9a8a1;
const NEUTRAL_DARK: u32 = 0x62615c;

/// Rings drawn: the folder's contents plus two levels deeper.
const RINGS: usize = 3;
/// Segments narrower than this (radians, about 0.7°) aren't drawn.
const MIN_ANGLE: f64 = 0.012;

/// Color slot for each entry, in the order given (largest first). The first
/// eight real items get slots 0 to 7; the "smaller files" bucket and the rest
/// get `None` (neutral).
pub fn assign_slots(entries: &[Entry<'_>]) -> Vec<Option<usize>> {
    let mut next = 0;
    entries
        .iter()
        .map(|e| {
            if e.kind == EntryKind::OtherFiles || e.size == 0 || next >= LIGHT.len() {
                None
            } else {
                next += 1;
                Some(next - 1)
            }
        })
        .collect()
}

/// RGB (0.0 to 1.0) for a slot in the current light/dark style.
pub fn slot_rgb(slot: Option<usize>, dark: bool) -> (f64, f64, f64) {
    let hex = match (slot, dark) {
        (Some(i), false) => LIGHT[i % LIGHT.len()],
        (Some(i), true) => DARK[i % DARK.len()],
        (None, false) => NEUTRAL_LIGHT,
        (None, true) => NEUTRAL_DARK,
    };
    (
        f64::from((hex >> 16) & 0xff) / 255.0,
        f64::from((hex >> 8) & 0xff) / 255.0,
        f64::from(hex & 0xff) / 255.0,
    )
}

/// A small rounded color square, used as the legend swatch in lists.
pub fn swatch(slot: Option<usize>) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder()
        .content_width(12)
        .content_height(12)
        .valign(gtk::Align::Center)
        .build();
    area.set_draw_func(move |_, cr, w, h| {
        let (r, g, b) = slot_rgb(slot, adw::StyleManager::default().is_dark());
        let (w, h) = (f64::from(w), f64::from(h));
        let radius = 3.0;
        cr.new_sub_path();
        cr.arc(w - radius, radius, radius, -FRAC_PI_2, 0.0);
        cr.arc(w - radius, h - radius, radius, 0.0, FRAC_PI_2);
        cr.arc(radius, h - radius, radius, FRAC_PI_2, FRAC_PI_2 * 2.0);
        cr.arc(radius, radius, radius, FRAC_PI_2 * 2.0, FRAC_PI_2 * 3.0);
        cr.close_path();
        cr.set_source_rgb(r, g, b);
        let _ = cr.fill();
    });
    area
}

#[derive(Debug, Clone)]
struct Segment {
    depth: usize,
    start: f64,
    end: f64,
    slot: Option<usize>,
    /// Path below the shown folder.
    path: Vec<OsString>,
    label: String,
    size: u64,
    is_folder: bool,
}

#[derive(Debug, Default)]
struct State {
    segments: Vec<Segment>,
    hover: Option<usize>,
    total: u64,
    files: u64,
    can_go_up: bool,
}

enum Hit {
    Center,
    Segment(usize),
    Nothing,
}

pub struct RingChart {
    root: gtk::Overlay,
    area: gtk::DrawingArea,
    state: Rc<RefCell<State>>,
    center_size: gtk::Label,
    center_caption: gtk::Label,
}

impl RingChart {
    /// `on_open` gets the path (below the shown folder) of a clicked folder;
    /// `on_up` runs when the middle is clicked.
    pub fn new(on_open: impl Fn(Vec<OsString>) + 'static, on_up: impl Fn() + 'static) -> Self {
        let state = Rc::new(RefCell::new(State::default()));

        let area = gtk::DrawingArea::builder()
            .content_width(340)
            .content_height(340)
            .hexpand(true)
            .vexpand(true)
            .build();
        area.update_property(&[gtk::accessible::Property::Label(
            "Ring chart of folder sizes. The list beside it shows the same items.",
        )]);

        let center_size = gtk::Label::builder()
            .css_classes(["title-2", "numeric"])
            .build();
        let center_caption = gtk::Label::builder()
            .css_classes(["caption", "dim-label"])
            .max_width_chars(16)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .justify(gtk::Justification::Center)
            .build();
        let center = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .can_target(false)
            .build();
        center.append(&center_size);
        center.append(&center_caption);

        let root = gtk::Overlay::builder()
            .child(&area)
            .width_request(240)
            .height_request(240)
            .build();
        root.add_overlay(&center);

        {
            let state = state.clone();
            area.set_draw_func(move |area, cr, w, h| draw(&state.borrow(), area, cr, w, h));
        }

        let chart = Self {
            root,
            area,
            state,
            center_size,
            center_caption,
        };
        chart.connect_input(Rc::new(on_open), Rc::new(on_up));

        // Redraw with the other palette when light/dark changes.
        let weak = chart.area.downgrade();
        adw::StyleManager::default().connect_dark_notify(move |_| {
            if let Some(area) = weak.upgrade() {
                area.queue_draw();
            }
        });

        chart
    }

    pub fn widget(&self) -> &gtk::Overlay {
        &self.root
    }

    /// Shows `node`. `can_go_up` makes the middle clickable.
    pub fn set_node(&self, node: &DirNode, can_go_up: bool) {
        {
            let mut st = self.state.borrow_mut();
            st.segments = build_segments(node);
            st.hover = None;
            st.total = node.size;
            st.files = node.file_count;
            st.can_go_up = can_go_up;
        }
        self.update_center();
        self.area.queue_draw();
    }

    fn update_center(&self) {
        update_center(
            &self.state.borrow(),
            &self.center_size,
            &self.center_caption,
        );
    }

    fn connect_input(&self, on_open: Rc<dyn Fn(Vec<OsString>)>, on_up: Rc<dyn Fn()>) {
        let motion = gtk::EventControllerMotion::new();
        {
            let refs = self.refs();
            motion.connect_motion(move |_, x, y| {
                let (hover, pointer) = {
                    let st = refs.state.borrow();
                    match hit_test(&st, &refs.area, x, y) {
                        Hit::Segment(i) => (Some(i), st.segments[i].is_folder),
                        Hit::Center => (None, st.can_go_up),
                        Hit::Nothing => (None, false),
                    }
                };
                refs.area.set_cursor_from_name(pointer.then_some("pointer"));
                if refs.state.borrow().hover != hover {
                    refs.state.borrow_mut().hover = hover;
                    refs.refresh();
                }
            });
        }
        {
            let refs = self.refs();
            motion.connect_leave(move |_| {
                refs.area.set_cursor_from_name(None);
                if refs.state.borrow().hover.is_some() {
                    refs.state.borrow_mut().hover = None;
                    refs.refresh();
                }
            });
        }
        self.area.add_controller(motion);

        let click = gtk::GestureClick::new();
        {
            let state = self.state.clone();
            let area = self.area.clone();
            click.connect_released(move |_, _, x, y| {
                let action = {
                    let st = state.borrow();
                    match hit_test(&st, &area, x, y) {
                        Hit::Segment(i) if st.segments[i].is_folder => {
                            Some(Ok(st.segments[i].path.clone()))
                        }
                        Hit::Center if st.can_go_up => Some(Err(())),
                        _ => None,
                    }
                };
                // Call back after releasing the borrow: the callback rebuilds
                // the chart.
                match action {
                    Some(Ok(path)) => on_open(path),
                    Some(Err(())) => on_up(),
                    None => {}
                }
            });
        }
        self.area.add_controller(click);
    }

    fn refs(&self) -> ChartRefs {
        ChartRefs {
            state: self.state.clone(),
            area: self.area.clone(),
            center_size: self.center_size.clone(),
            center_caption: self.center_caption.clone(),
        }
    }
}

/// Cloned handles for signal closures.
struct ChartRefs {
    state: Rc<RefCell<State>>,
    area: gtk::DrawingArea,
    center_size: gtk::Label,
    center_caption: gtk::Label,
}

impl ChartRefs {
    fn refresh(&self) {
        update_center(
            &self.state.borrow(),
            &self.center_size,
            &self.center_caption,
        );
        self.area.queue_draw();
    }
}

/// Middle text: the hovered segment, or the shown folder's total.
fn update_center(st: &State, size: &gtk::Label, caption: &gtk::Label) {
    match st.hover.and_then(|i| st.segments.get(i)) {
        Some(seg) => {
            size.set_label(&human_size(seg.size));
            caption.set_label(&seg.label);
        }
        None => {
            size.set_label(&human_size(st.total));
            let files = format!("{} files", count(st.files));
            caption.set_label(&if st.can_go_up {
                format!("{files}\nClick to go up")
            } else {
                files
            });
        }
    }
}

fn entry_label(e: &Entry<'_>) -> String {
    match e.kind {
        EntryKind::OtherFiles => format!("{} smaller files", count(e.file_count)),
        _ => e.name.to_string_lossy().into_owned(),
    }
}

fn build_segments(node: &DirNode) -> Vec<Segment> {
    let mut segments = Vec::new();
    if node.size == 0 {
        return segments;
    }
    let entries = node.entries();
    let slots = assign_slots(&entries);
    let mut angle = 0.0;
    for (e, slot) in entries.iter().zip(slots) {
        let span = TAU * e.size as f64 / node.size as f64;
        if span >= MIN_ANGLE {
            let path = vec![e.name.to_owned()];
            if let Some(child) = e.node {
                add_ring(child, 2, angle, span, slot, &path, &mut segments);
            }
            segments.push(Segment {
                depth: 1,
                start: angle,
                end: angle + span,
                slot,
                path,
                label: entry_label(e),
                size: e.size,
                is_folder: e.kind == EntryKind::Folder
                    && e.node.is_some_and(|n| n.skipped.is_none()),
            });
        }
        angle += span;
    }
    segments
}

fn add_ring(
    node: &DirNode,
    depth: usize,
    start: f64,
    span: f64,
    slot: Option<usize>,
    path: &[OsString],
    segments: &mut Vec<Segment>,
) {
    if depth > RINGS || node.size == 0 {
        return;
    }
    let mut angle = start;
    for e in node.entries() {
        let s = span * e.size as f64 / node.size as f64;
        if s < MIN_ANGLE {
            // Entries are sorted largest first, so the rest are smaller.
            break;
        }
        let mut child_path = path.to_vec();
        child_path.push(e.name.to_owned());
        if let Some(child) = e.node {
            add_ring(child, depth + 1, angle, s, slot, &child_path, segments);
        }
        segments.push(Segment {
            depth,
            start: angle,
            end: angle + s,
            slot: if e.kind == EntryKind::OtherFiles {
                None
            } else {
                slot
            },
            path: child_path,
            label: entry_label(&e),
            size: e.size,
            is_folder: e.kind == EntryKind::Folder && e.node.is_some_and(|n| n.skipped.is_none()),
        });
        angle += s;
    }
}

struct Geometry {
    cx: f64,
    cy: f64,
    hole: f64,
    ring: f64,
}

fn geometry(w: f64, h: f64) -> Geometry {
    let radius = (w.min(h) / 2.0 - 4.0).max(20.0);
    let hole = radius * 0.38;
    Geometry {
        cx: w / 2.0,
        cy: h / 2.0,
        hole,
        ring: (radius - hole) / RINGS as f64,
    }
}

fn hit_test(st: &State, area: &gtk::DrawingArea, x: f64, y: f64) -> Hit {
    let g = geometry(f64::from(area.width()), f64::from(area.height()));
    let (dx, dy) = (x - g.cx, y - g.cy);
    let dist = dx.hypot(dy);
    if dist < g.hole {
        return Hit::Center;
    }
    let depth = ((dist - g.hole) / g.ring).floor() as usize + 1;
    if depth > RINGS {
        return Hit::Nothing;
    }
    // Angle measured clockwise from 12 o'clock.
    let angle = (dy.atan2(dx) + FRAC_PI_2).rem_euclid(TAU);
    st.segments
        .iter()
        .position(|s| s.depth == depth && s.start <= angle && angle < s.end)
        .map_or(Hit::Nothing, Hit::Segment)
}

fn draw(st: &State, area: &gtk::DrawingArea, cr: &gtk::cairo::Context, w: i32, h: i32) {
    let g = geometry(f64::from(w), f64::from(h));
    let dark = adw::StyleManager::default().is_dark();

    // A faint track so an empty folder still shows a ring.
    let fg = area.color();
    cr.set_source_rgba(
        f64::from(fg.red()),
        f64::from(fg.green()),
        f64::from(fg.blue()),
        0.06,
    );
    cr.set_line_width(g.ring - 2.0);
    cr.arc(g.cx, g.cy, g.hole + g.ring / 2.0, 0.0, TAU);
    let _ = cr.stroke();

    for (i, seg) in st.segments.iter().enumerate() {
        let inner = g.hole + g.ring * (seg.depth - 1) as f64 + 1.0;
        let outer = inner + g.ring - 2.0;
        // About 1px of surface between neighbours.
        let gap = 0.8 / inner;
        let (a0, a1) = (seg.start + gap, seg.end - gap);
        if a1 <= a0 {
            continue;
        }
        segment_path(cr, &g, inner, outer, a0, a1);
        let (r, gr, b) = slot_rgb(seg.slot, dark);
        let alpha = match seg.depth {
            1 => 1.0,
            2 => 0.68,
            _ => 0.42,
        };
        let alpha = if st.hover == Some(i) { 1.0 } else { alpha };
        cr.set_source_rgba(r, gr, b, alpha);
        let _ = cr.fill();
    }

    // Outline the hovered segment in the text color.
    if let Some(seg) = st.hover.and_then(|i| st.segments.get(i)) {
        let inner = g.hole + g.ring * (seg.depth - 1) as f64 + 1.0;
        let outer = inner + g.ring - 2.0;
        segment_path(cr, &g, inner, outer, seg.start, seg.end);
        cr.set_source_rgba(
            f64::from(fg.red()),
            f64::from(fg.green()),
            f64::from(fg.blue()),
            0.9,
        );
        cr.set_line_width(2.0);
        let _ = cr.stroke();
    }
}

fn segment_path(cr: &gtk::cairo::Context, g: &Geometry, inner: f64, outer: f64, a0: f64, a1: f64) {
    cr.new_path();
    cr.arc(g.cx, g.cy, outer, a0 - FRAC_PI_2, a1 - FRAC_PI_2);
    cr.arc_negative(g.cx, g.cy, inner, a1 - FRAC_PI_2, a0 - FRAC_PI_2);
    cr.close_path();
}
