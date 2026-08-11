# dioxus-flow

一个业务无关的 Dioxus Web 节点画布组件：视口平移/缩放、节点拖拽、SVG 贝塞尔边。
调用方持有节点数据、布局与节点内容，画布只管视口交互和边的渲染。

## 特性

- 滚轮以指针为锚点缩放（`zoom_at`），拖拽平移，节点拖拽（3px 阈值区分点击）
- 边为三次贝塞尔曲线 + 箭头 marker，支持 `label`、`dashed`、`emphasis`（Highlight/Dim → CSS class）
- `fit_viewport` 纯函数：内容包围盒 → 自适应视口；`animate` 属性开启编程式视口过渡
- 性能：视口订阅隔离在 `ViewportFrame`（每帧只更新 3 个 div）；平移拖拽同步直写 DOM 样式，无渲染调度延迟

## 使用

```rust
use dioxus::prelude::*;
use dioxus_flow::*;

#[component]
fn Graph() -> Element {
    let mut viewport = use_signal(Viewport::default);
    let nodes = vec![FlowNode {
        id: NodeId::from("a"),
        position: Point::new(0.0, 0.0),
        size: Size::new(160.0, 64.0),
    }];
    rsx! {
        div { style: "width: 100%; height: 600px;",
            FlowCanvas {
                nodes,
                edges: vec![],
                viewport,
                render_node: move |id| rsx! { div { "{id.0}" } },
                on_node_move: |_| {},
                on_node_click: |_| {},
            }
        }
    }
}
```

边样式通过 CSS class 定制（调用方提供）：

```css
.flow-edge--dashed path { stroke-dasharray: 6 5; }
.flow-edge--highlight path { stroke-opacity: 1; stroke-width: 2.5px; }
.flow-edge--dim path { stroke-opacity: 0.12; }
```

节点内需要豁免拖拽的元素加 `data-flow-no-drag` 属性。

## 使用方

- [pal-companion](https://github.com/AzurIce/pal-companion) — 幻兽帕鲁配种路径规划图
- [xiv-companion](https://github.com/AzurIce/xiv-companion) — FFXIV 制作清单合成图

## License

MIT
