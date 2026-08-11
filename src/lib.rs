//! A small, business-agnostic node canvas for Dioxus Web.
//!
//! `dioxus-flow` owns viewport interactions and edge rendering while callers
//! keep node data, layout, and node content in their application state.
//!
//! # Performance design
//!
//! - The only component subscribed to the viewport signal is [`ViewportFrame`]
//!   (transform layer + zoom layer). Panning re-renders just those divs; the
//!   scene itself is memoized behind `PartialEq` props.
//! - Panning uses `transform: translate()` (compositor-only, no layout or
//!   repaint) and is also written **synchronously** inside the pointer
//!   handler, so the view tracks the cursor without waiting for Dioxus'
//!   async render scheduling. The signal is updated in the same handler, so
//!   the eventual re-render produces identical styles and never jumps.
//! - The dotted background lives inside the transformed layer (fixed 20px
//!   spacing in graph coordinates) instead of animating `background-position`
//!   — animating gradient backgrounds forces a full re-raster every frame.
//! - Zooming keeps CSS `zoom` so text stays sharp (re-raster per wheel tick
//!   is acceptable; it is not a per-pointer-frame path).
//! - Node dragging reports through `on_node_move`; how callers apply it
//!   (and its re-render cost) is the caller's choice.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use dioxus::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

static NEXT_CANVAS_ID: AtomicU64 = AtomicU64::new(1);
const NODE_DRAG_THRESHOLD_SQUARED: f64 = 9.0;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct NodeId(pub String);

impl From<String> for NodeId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for NodeId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

impl Size {
    pub const fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub x: f64,
    pub y: f64,
    pub zoom: f64,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            x: 24.0,
            y: 24.0,
            zoom: 1.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FlowNode {
    pub id: NodeId,
    pub position: Point,
    pub size: Size,
}

/// How an edge is emphasized relative to the rest of the graph.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EdgeEmphasis {
    #[default]
    Normal,
    /// Highlighted, e.g. part of the hovered node's ancestor chain.
    Highlight,
    /// Pushed into the background while some other chain is highlighted.
    Dim,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FlowEdge {
    pub id: String,
    pub source: NodeId,
    pub target: NodeId,
    pub source_offset: f64,
    pub target_offset: f64,
    pub label: Option<String>,
    /// Render with a dashed stroke (e.g. for a special edge kind).
    pub dashed: bool,
    pub emphasis: EdgeEmphasis,
}

impl FlowEdge {
    fn class(&self) -> String {
        let mut class = String::from("flow-edge");
        if self.dashed {
            class.push_str(" flow-edge--dashed");
        }
        match self.emphasis {
            EdgeEmphasis::Normal => {}
            EdgeEmphasis::Highlight => class.push_str(" flow-edge--highlight"),
            EdgeEmphasis::Dim => class.push_str(" flow-edge--dim"),
        }
        class
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NodeMove {
    pub id: NodeId,
    pub position: Point,
}

#[derive(Clone, Debug, PartialEq)]
enum DragState {
    Pan {
        start: Point,
        origin: Point,
    },
    Node {
        id: NodeId,
        start: Point,
        origin: Point,
        moved: bool,
    },
}

struct WindowReleaseListener {
    window: web_sys::Window,
    callback: Closure<dyn FnMut(web_sys::Event)>,
}

impl WindowReleaseListener {
    fn install(drag: Rc<RefCell<Option<DragState>>>) -> Option<Rc<Self>> {
        let window = web_sys::window()?;
        let callback = Closure::wrap(Box::new(move |_event: web_sys::Event| {
            *drag.borrow_mut() = None;
        }) as Box<dyn FnMut(_)>);
        for event_name in ["mouseup", "blur"] {
            let _ = window
                .add_event_listener_with_callback(event_name, callback.as_ref().unchecked_ref());
        }
        Some(Rc::new(Self { window, callback }))
    }
}

impl Drop for WindowReleaseListener {
    fn drop(&mut self) {
        for event_name in ["mouseup", "blur"] {
            let _ = self.window.remove_event_listener_with_callback(
                event_name,
                self.callback.as_ref().unchecked_ref(),
            );
        }
    }
}

pub fn clamp_zoom(zoom: f64, min_zoom: f64, max_zoom: f64) -> f64 {
    zoom.clamp(min_zoom, max_zoom)
}

pub fn zoom_at(viewport: Viewport, anchor: Point, next_zoom: f64) -> Viewport {
    let graph_x = (anchor.x - viewport.x) / viewport.zoom;
    let graph_y = (anchor.y - viewport.y) / viewport.zoom;
    Viewport {
        x: anchor.x - graph_x * next_zoom,
        y: anchor.y - graph_y * next_zoom,
        zoom: next_zoom,
    }
}

fn exceeds_node_drag_threshold(start: Point, current: Point) -> bool {
    let delta_x = current.x - start.x;
    let delta_y = current.y - start.y;
    delta_x * delta_x + delta_y * delta_y >= NODE_DRAG_THRESHOLD_SQUARED
}

pub fn fit_viewport(
    nodes: &[FlowNode],
    viewport_size: Size,
    padding: f64,
    min_zoom: f64,
    max_zoom: f64,
) -> Viewport {
    let Some(first) = nodes.first() else {
        return Viewport::default();
    };
    let mut min_x = first.position.x;
    let mut min_y = first.position.y;
    let mut max_x = first.position.x + first.size.width;
    let mut max_y = first.position.y + first.size.height;
    for node in &nodes[1..] {
        min_x = min_x.min(node.position.x);
        min_y = min_y.min(node.position.y);
        max_x = max_x.max(node.position.x + node.size.width);
        max_y = max_y.max(node.position.y + node.size.height);
    }
    let content_width = (max_x - min_x).max(1.0);
    let content_height = (max_y - min_y).max(1.0);
    let available_width = (viewport_size.width - padding * 2.0).max(1.0);
    let available_height = (viewport_size.height - padding * 2.0).max(1.0);
    let zoom = clamp_zoom(
        (available_width / content_width).min(available_height / content_height),
        min_zoom,
        max_zoom,
    );
    Viewport {
        x: (viewport_size.width - content_width * zoom) / 2.0 - min_x * zoom,
        y: (viewport_size.height - content_height * zoom) / 2.0 - min_y * zoom,
        zoom,
    }
}

fn client_point(event: &MouseEvent) -> Point {
    let coordinates = event.data().client_coordinates();
    Point::new(coordinates.x, coordinates.y)
}

#[component]
pub fn FlowCanvas(
    nodes: Vec<FlowNode>,
    edges: Vec<FlowEdge>,
    mut viewport: Signal<Viewport>,
    render_node: Callback<NodeId, Element>,
    on_node_move: EventHandler<NodeMove>,
    on_node_click: EventHandler<NodeId>,
    #[props(default = String::new())] class: String,
    #[props(default = 0.35)] min_zoom: f64,
    #[props(default = 2.0)] max_zoom: f64,
    #[props(default = String::from("#94a3b8"))] edge_color: String,
    /// Smoothly animate viewport changes (programmatic fit/reset).
    /// Keep disabled while the user is dragging.
    #[props(default = false)] animate: bool,
    #[props(default)] empty: Option<Element>,
) -> Element {
    let canvas_id = use_hook(|| NEXT_CANVAS_ID.fetch_add(1, Ordering::Relaxed));
    let marker_id = format!("dioxus-flow-arrow-{canvas_id}");
    let drag = use_hook(|| Rc::new(RefCell::new(None::<DragState>)));
    let drag_for_window = drag.clone();
    let _window_release_listener =
        use_hook(move || WindowReleaseListener::install(drag_for_window));
    let drag_for_canvas_down = drag.clone();
    let drag_for_canvas_move = drag.clone();
    let drag_for_canvas_up = drag.clone();
    let canvas_element = use_hook(|| Rc::new(RefCell::new(None::<web_sys::Element>)));
    let canvas_for_mount = canvas_element.clone();
    let canvas_for_wheel = canvas_element.clone();
    // 平移变换层的 DOM 引用（ViewportFrame 挂载时写入），用于拖拽期间同步直写样式
    let frame_element = use_hook(|| Rc::new(RefCell::new(None::<web_sys::HtmlElement>)));
    let frame_for_move = frame_element.clone();
    let is_empty = nodes.is_empty();
    // 注意：本组件刻意不读取 viewport signal（仅事件回调里写/读）。
    // 视口订阅隔离在 ViewportFrame 内，平移/缩放的每一帧只重渲染那三个 div，
    // 场景（FlowScene）与事件层完全不参与逐帧渲染。

    rsx! {
        div {
            class: class.clone(),
            style: "position: relative; width: 100%; height: 100%; min-height: 0; overflow: hidden; touch-action: none; user-select: none; cursor: grab;",
            onmounted: move |event| {
                *canvas_for_mount.borrow_mut() =
                    event.data().downcast::<web_sys::Element>().cloned();
            },
            onwheel: move |event| {
                let event_data = event.data();
                let Some(native) = event_data.downcast::<web_sys::WheelEvent>() else { return; };
                let Some(element) = canvas_for_wheel.borrow().clone() else { return; };
                let rect = element.get_bounding_client_rect();
                let anchor = Point::new(native.client_x() as f64 - rect.left(), native.client_y() as f64 - rect.top());
                let factor = (-native.delta_y() * 0.0015).exp();
                let current = viewport();
                let next_zoom = clamp_zoom(current.zoom * factor, min_zoom, max_zoom);
                event.prevent_default();
                event.stop_propagation();
                viewport.set(zoom_at(current, anchor, next_zoom));
            },
            onmousedown: move |event| {
                let event_data = event.data();
                let Some(native) = event_data.downcast::<web_sys::MouseEvent>() else { return; };
                if native.button() != 0 { return; }
                let point = client_point(&event);
                let current = viewport();
                event.prevent_default();
                *drag_for_canvas_down.borrow_mut() = Some(DragState::Pan {
                    start: point,
                    origin: Point::new(current.x, current.y),
                });
            },
            onmousemove: move |event| {
                let state = drag_for_canvas_move.borrow().clone();
                let Some(state) = state else { return; };
                let point = client_point(&event);
                match state {
                    DragState::Pan { start, origin, .. } => {
                        let current = viewport();
                        let next = Viewport {
                            x: origin.x + point.x - start.x,
                            y: origin.y + point.y - start.y,
                            zoom: current.zoom,
                        };
                        // 同步直写 transform（合成器层，无 layout/repaint），
                        // 消除渲染调度延迟；signal 随后渲染出相同样式，不会跳变。
                        if let Some(el) = frame_for_move.borrow().as_ref() {
                            let _ = el.style().set_property(
                                "transform",
                                &format!("translate({}px, {}px)", next.x, next.y),
                            );
                        }
                        viewport.set(next);
                    }
                    DragState::Node {
                        id,
                        start,
                        origin,
                        moved,
                    } => {
                        let delta_x = point.x - start.x;
                        let delta_y = point.y - start.y;
                        if !moved && !exceeds_node_drag_threshold(start, point) {
                            return;
                        }
                        if !moved
                            && let Some(DragState::Node { moved, .. }) =
                                drag_for_canvas_move.borrow_mut().as_mut()
                        {
                            *moved = true;
                        }
                        let zoom = viewport().zoom;
                        on_node_move.call(NodeMove {
                            id,
                            position: Point::new(
                                origin.x + delta_x / zoom,
                                origin.y + delta_y / zoom,
                            ),
                        });
                    }
                }
            },
            onmouseup: move |_| *drag_for_canvas_up.borrow_mut() = None,

            ViewportFrame {
                viewport,
                animate,
                frame_ref: frame_element.clone(),
                FlowScene {
                    nodes,
                    edges,
                    marker_id,
                    edge_color,
                    drag: drag.clone(),
                    render_node,
                    on_node_click,
                }
            }

            if is_empty {
                div { style: "position: absolute; inset: 0; display: flex; align-items: center; justify-content: center; pointer-events: none;", {empty} }
            }
        }
    }
}

/// 视口帧：唯一订阅 viewport signal 的组件。
/// 平移 = transform: translate（合成器层，无 layout/repaint）；
/// 缩放 = CSS zoom（重栅格化保证文字清晰）。
/// 点阵背景放在变换层内部（图坐标固定点距），随平移/缩放自然移动，
/// 不做 background-position/size 的逐帧更新（渐变重光栅是性能陷阱）。
#[component]
fn ViewportFrame(
    viewport: Signal<Viewport>,
    animate: bool,
    frame_ref: Rc<RefCell<Option<web_sys::HtmlElement>>>,
    children: Element,
) -> Element {
    let vp = viewport();
    let transition = if animate {
        "transition: transform 0.35s ease, zoom 0.35s ease;"
    } else {
        ""
    };
    rsx! {
        div {
            style: "position: absolute; left: 0; top: 0; transform: translate({vp.x}px, {vp.y}px); will-change: transform; {transition}",
            onmounted: move |event| {
                *frame_ref.borrow_mut() =
                    event.data().downcast::<web_sys::HtmlElement>().cloned();
            },
            div {
                style: "zoom: {vp.zoom}; {transition}",
                // 点阵背景：图坐标固定 20px 点距，覆盖以原点为中心的大范围区域
                div {
                    style: "position: absolute; left: -5000px; top: -5000px; width: 10000px; height: 10000px; pointer-events: none; background-image: radial-gradient(circle, color-mix(in srgb, currentColor 18%, transparent) 1px, transparent 1px); background-size: 20px 20px;",
                }
                {children}
            }
        }
    }
}

#[component]
fn FlowScene(
    nodes: Vec<FlowNode>,
    edges: Vec<FlowEdge>,
    marker_id: String,
    edge_color: String,
    drag: Rc<RefCell<Option<DragState>>>,
    render_node: Callback<NodeId, Element>,
    on_node_click: EventHandler<NodeId>,
) -> Element {
    let by_id = nodes
        .iter()
        .cloned()
        .map(|node| (node.id.clone(), node))
        .collect::<HashMap<_, _>>();

    rsx! {
        div {
            style: "position: absolute; left: 0; top: 0; contain: layout style;",
            svg {
                style: "position: absolute; left: 0; top: 0; overflow: visible; pointer-events: none;",
                width: "1",
                height: "1",
                defs {
                    marker {
                        id: marker_id.clone(),
                        marker_width: "10",
                        marker_height: "10",
                        ref_x: "9",
                        ref_y: "5",
                        orient: "auto",
                        path { d: "M0,0 L10,5 L0,10 Z", fill: edge_color.clone() }
                    }
                }
                for edge in edges.iter() {
                    if let (Some(source), Some(target)) = (by_id.get(&edge.source), by_id.get(&edge.target)) {
                        {
                            let x1 = source.position.x + source.size.width;
                            let y1 = source.position.y + source.size.height / 2.0 + edge.source_offset;
                            let x2 = target.position.x;
                            let y2 = target.position.y + target.size.height / 2.0 + edge.target_offset;
                            let mid_x = (x1 + x2) / 2.0;
                            let path = format!("M {x1} {y1} C {mid_x} {y1}, {mid_x} {y2}, {x2} {y2}");
                            let label_x = x1 + (x2 - x1) * 0.68 - 22.0;
                            let label_y = y1 + (y2 - y1) * 0.68 - 10.0;
                            rsx! {
                                g { key: "{edge.id}", class: edge.class(),
                                    path {
                                        d: path,
                                        fill: "none",
                                        stroke: edge_color.clone(),
                                        stroke_opacity: "0.68",
                                        stroke_width: "1.75",
                                        marker_end: "url(#{marker_id})",
                                    }
                                    if let Some(label) = edge.label.as_ref() {
                                        foreignObject { x: "{label_x}", y: "{label_y}", width: "44", height: "20",
                                            div { style: "display: flex; height: 20px; align-items: center; justify-content: center;",
                                                span { style: "border: 1px solid color-mix(in srgb, currentColor 20%, transparent); border-radius: 3px; background: color-mix(in srgb, Canvas 90%, transparent); padding: 2px 4px; color: color-mix(in srgb, currentColor 70%, transparent); font-size: 9px; font-weight: 500; line-height: 1; box-shadow: 0 1px 2px rgb(0 0 0 / 0.08);", {label.clone()} }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            for node in nodes.iter() {
                {
                    let node_for_drag = node.clone();
                    let id = node.id.clone();
                    let drag_for_node_down = drag.clone();
                    let drag_for_node_up = drag.clone();
                    rsx! {
                        div {
                            key: "{id.0}",
                            style: "position: absolute; left: {node.position.x}px; top: {node.position.y}px; width: {node.size.width}px; height: {node.size.height}px; cursor: grab;",
                            onmousedown: move |event| {
                                let event_data = event.data();
                                let Some(native) = event_data.downcast::<web_sys::MouseEvent>() else { return; };
                                if native.button() != 0 { return; }
                                event.stop_propagation();
                                let targets_no_drag = native
                                    .target()
                                    .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
                                    .and_then(|element| element.closest("[data-flow-no-drag]").ok().flatten())
                                    .is_some();
                                if targets_no_drag { return; }
                                *drag_for_node_down.borrow_mut() = Some(DragState::Node {
                                    id: node_for_drag.id.clone(),
                                    start: client_point(&event),
                                    origin: node_for_drag.position,
                                    moved: false,
                                });
                            },
                            onmouseup: move |_| {
                                let interaction = drag_for_node_up.borrow_mut().take();
                                if matches!(interaction, Some(DragState::Node { id: ref node_id, moved: false, .. }) if node_id == &id) {
                                    on_node_click.call(id.clone());
                                }
                            },
                            {render_node.call(node.id.clone())}
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_keeps_anchor_over_the_same_graph_point() {
        let viewport = Viewport {
            x: 20.0,
            y: 40.0,
            zoom: 1.0,
        };
        let anchor = Point::new(120.0, 90.0);
        let next = zoom_at(viewport, anchor, 2.0);
        assert_eq!(
            next,
            Viewport {
                x: -80.0,
                y: -10.0,
                zoom: 2.0
            }
        );
    }

    #[test]
    fn fit_centers_node_bounds() {
        let nodes = vec![FlowNode {
            id: NodeId::from("a"),
            position: Point::new(100.0, 50.0),
            size: Size::new(200.0, 100.0),
        }];
        let viewport = fit_viewport(&nodes, Size::new(500.0, 300.0), 50.0, 0.1, 4.0);
        assert_eq!(viewport.zoom, 2.0);
        assert_eq!(viewport.x, -150.0);
        assert_eq!(viewport.y, -50.0);
    }

    #[test]
    fn node_drag_requires_three_pixels_of_movement() {
        let start = Point::new(10.0, 10.0);
        assert!(!exceeds_node_drag_threshold(start, Point::new(12.0, 12.0)));
        assert!(exceeds_node_drag_threshold(start, Point::new(13.0, 10.0)));
    }

    #[test]
    fn edge_class_reflects_style_flags() {
        let edge = FlowEdge {
            dashed: true,
            emphasis: EdgeEmphasis::Highlight,
            ..Default::default()
        };
        assert_eq!(edge.class(), "flow-edge flow-edge--dashed flow-edge--highlight");
        assert_eq!(FlowEdge::default().class(), "flow-edge");
    }
}
