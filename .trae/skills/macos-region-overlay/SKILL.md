---
name: "macos-region-overlay"
description: "Build/modify a full-screen interactive region-selection overlay on macOS by embedding Swift in Rust (run via `swift -e`): per-monitor windows, keyboard (Esc) handling, crosshair/focus/hints. Invoke when adding or fixing interactive screen region/screenshot selection, full-screen crosshair overlays, or multi-monitor coordinate capture."
---

# macOS 全屏区域选择遮罩（Rust 内嵌 Swift）

在 typepaste-rs 这类无 GUI 框架的 Rust CLI 中，需要交互式框选屏幕区域时，
参考实现为 [src/ocr.rs](../../../src/ocr.rs) 的 `select_region()`：把 Swift 源码
放进 `r#"..."#` 原始字符串，用 `Command::new("swift").args(["-e", code])` 执行，
stdout 输出 `x,y,w,h`，Rust 解析。

## 必须遵守的坑（都在真机上踩过）

### 1. 多显示器：每个显示器一个独立 NSWindow

- **禁止**把 `NSScreen.screens` 的 frame 求并集（`union`）只建一个跨屏窗口。
  跨屏 borderless 窗口的鼠标事件只路由到窗口所属屏幕，副屏看得到遮罩但点击/拖拽无反应。
- 正确做法：`for screen in NSScreen.screens` 逐屏 `NSWindow(contentRect: screen.frame, ...)`，
  每窗挂自己的 view；view 的全局坐标 = `window.frame.origin + event.locationInWindow`，
  多窗口下天然正确，无需额外处理。

### 2. Esc/键盘事件收不到（三层都要做）

1. borderless 窗口默认 `canBecomeKey == false`，`makeKeyAndOrderFront` 无效。
   必须子类化：`class KeyWindow: NSWindow { override var canBecomeKey: Bool { true }; override var canBecomeMain: Bool { true } }`。
2. `swift -e` 进程默认不是前台 GUI App：启动时
   `app.setActivationPolicy(.accessory); app.activate(ignoringOtherApps: true)`。
3. Esc 处理放在**窗口级** `keyDown`（不依赖视图第一响应者链）；view 仍重写
   `acceptsFirstResponder = true` 并 `win.makeFirstResponder(view)` 双保险。
   Esc 的 keyCode 是 53，处理后 `exit(0)`。
4. Rust 端：Esc 时 stdout 为空，必须把**空输出识别为"已取消"**，
   不能报"输出格式错误"。

### 3. 鼠标移动追踪需要 NSTrackingArea
- `mouseMoved` 默认不触发。重写 `updateTrackingAreas()` 添加 tracking area；
  options **必须同时含** `.mouseMoved` 和 `.mouseEnteredAndExited`——后者缺失时
  `mouseExited` 永远不触发（表现为鼠标移出屏幕后聚焦边框/十字线不消退）。
  另加 `.activeAlways, .inVisibleRect`；先 `removeTrackingArea` 旧的再建新的。
- `mouseExited` 里清空 hover 状态，避免十字线/聚焦边框残留在离屏鼠标位置。
- 十字光标：重写 `resetCursorRects()` → `addCursorRect(bounds, cursor: .crosshair)`。

### 3.5 框选后可调整（不立即退出）
- mouseUp **不要**直接输出坐标：置 `selected=true` 进入可调整状态，等待确认
  （回车 keyCode 36、双击选区内部、或「确定」NSButton）；Esc（53）/「取消」按钮退出空输出。
- 已选区状态下 mouseDown 命中判定（角点阈值约 12pt，用 `hypot` 算距离）：
  角点 → resizing（固定对角点，被拖角点跟随鼠标）；
  矩形内部 → moving（记录按下锚点与初始两角，拖拽加位移）；
  外部 → 丢弃旧选区重新 creating。
- 已选区绘制四角手柄（8pt 方块）；调整提示文字贴在选框上沿中部（不画在屏幕中心）；
  「确定/取消」NSButton 作为子视图贴在选框下方（空间不足翻到上方），拖拽期间隐藏，
  每屏各一份，仅在选区与该屏 bounds 相交时显示（`updateButtons()` 在 mouseUp/hover 事件里调用）。
- 光标反馈：`mouseMoved` 里按命中结果 `NSCursor.set()`（内部 openHand、拖动 closedHand、
  角点对角调整、默认 crosshair；`resetCursorRects` 默认 crosshair）。AppKit **无公开对角
  resize 光标**：用 SF Symbol `arrow.up.right.and.arrow.down.left`（↗↙，左下/右上角）和
  `arrow.up.left.and.arrow.down.right`（↖↘，右下/左上角）经
  `NSImage(systemSymbolName:)` + `NSCursor(image:hotSpot:)` 构造；拖拽期间按起始角点锁定
  光标（鼠标可能漂出角点命中范围）。
- 十字准线：调整大小/移动选区期间、以及光标在选区内部或边框/手柄吸附范围内时隐藏；
  其余情况（空闲在选区外、正在新建框选）跟随鼠标显示。
- 尺寸/坐标标签常显：拖拽中光标旁显示 `w × h  x: .. y: ..`；选区确定后 `w × h` 与
  调整提示贴在选框上沿中部；空闲时光标旁始终显示当前全局坐标（screencapture 坐标系）。

### 3.6 多显示器 UI 状态必须共享
- 每个屏一个 NSWindow/NSView，但选区/模式/手柄等**交互状态放在共享的 SelectionState 对象里**
  （坐标全用全局 AppKit 坐标），所有 view 持同一引用。否则在 A 屏开始框选时，B/C 屏的
  中心提示、按钮等不会同步更新。
- **状态变化（mouseDown/mouseDragged/mouseUp）后必须遍历 `NSApp.windows` 重绘所有视图**
  （`contentView.needsDisplay = true` + 更新按钮），否则未接收事件的屏幕不重绘：表现为
  其它屏幕中心提示残留、跨屏选框不显示。按钮/提示的显隐用「状态 + 本屏 bounds 是否相交」
  在 draw/updateButtons 中派生，不要按屏各存一份。
- 首次点击必须能直接拖拽：重写视图 `acceptsFirstMouse(for:) -> true`，否则非激活 App 的
  首次点击只用于激活窗口，表现为「先点一下、再拖拽才生效」。
- 角点调整时固定点在 mouseDown 时一次性记录（对角点 = `corners[(hit+2)%4]`），拖拽中
  直接用；不要每次从当前矩形推导，否则角点拖过对侧后索引翻转、矩形异常。

### 4. 坐标系：AppKit ↔ screencapture

- AppKit/`NSScreen.frame`：全局坐标，原点在**主屏幕左下角**，y 向上；副屏 origin.y 可为负。
- `screencapture -R x,y,w,h` 与 `CGDisplayBounds(id)`：原点在**主屏左上角**，y 向下。
- 转换：对全局点 p，找到包含它的 screen，取其 `NSScreenNumber` →
  `CGDisplayBounds(id)`；`lx = p.x - screen.frame.origin.x`；
  `lyTop = screen.frame.height - (p.y - screen.frame.origin.y)`；
  输出 `(b.origin.x + lx, b.origin.y + lyTop)`。

## 交互元素（macshot 风格，用户已确认）

- 半透明黑遮罩（`NSColor.black.withAlphaComponent(0.35)` 填 `dirtyRect`）。
- 十字准线：横贯/纵贯全屏的 0.5px 白线，跟随鼠标（未拖拽）/拖拽点。
- 每屏边框：鼠标所在屏（或正在拖拽的屏）`systemBlue` 4px 高亮，其余屏白色 25% 1px。
  聚焦条件 `hoverPoint != nil || startPoint != nil`。
- 光标旁圆角深色标签：拖拽中显示 `w × h`；未拖拽显示全局坐标 `x: .. y: ..`。
  用 `monospacedDigitSystemFont`；靠近屏幕边缘时翻转到光标另一侧。
- 屏幕中心提示文字（开始拖拽后隐藏）：`NSFont.systemFont(ofSize: 20)` + 圆角黑底。
- 选框：白色 1.5px 描边 + 白色 15% 填充；w、h 均 > 5pt 才算有效选择。
- 窗口层级：`CGWindowLevelForKey(.screenSaverWindow) + 1`；`isOpaque = false`、
  `backgroundColor = .clear`、`ignoresMouseEvents = false`。

## Rust 侧约定
- 签名 `select_region(hint: &str)`：`hint` 为屏幕中心提示语，注入 Swift 前转义
  `\` 和 `"`，替换代码中的 `__HINT__` 占位符；不同调用方传不同提示（如二维码/终端区域）。
- 成功（回车/双击确认）：println `x,y,w,h`（整数，screencapture 全局坐标）后 `exit(0)`。
- 取消：`exit(0)` 且无输出 → Rust 返回 `Err("已取消区域选择")`。
- 解析前先 `trim()`，空串走取消分支；split(',') 必须恰为 4 段且全部可 parse。

## 验证方式（不弹窗、快速）

改动内嵌 Swift 后，先抽出来做类型检查，再编译 Rust：

```bash
python3 - <<'PY'
import re
src = open('src/ocr.rs').read()
m = re.search(r'let swift_code = r#"(.*?)"#;', src, re.S)
open('/tmp/tp_select_region.swift','w').write(m.group(1))
PY
swiftc -typecheck /tmp/tp_select_region.swift && rm -f /tmp/tp_select_region.swift
cargo build
```

交互逻辑（拖拽/Esc/多屏聚焦）无法 typecheck 覆盖，需真机跑
`--pull-region` 或 `--web-region` 手测。
