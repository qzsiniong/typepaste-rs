# 反向传输：支持用户框选截图区域

## 背景
当前 `--pull` 截图策略：优先截前台窗口，回退全屏。问题：
- 窗口截图仍包含标题栏、标签栏、滚动条、shell 提示符等噪声，降低 OCR 准确率；
- 用户无法精确指定只截终端的输出区域。

`screencapture -i` 可让用户拖拽框选区域，但不返回坐标；反向传输需要对同一区域反复截图几十~上百次，因此必须**首次框选时记录坐标，后续用 `screencapture -R x,y,w,h` 复用**。

## 方案

### 1. 新增 CLI 参数
- `--pull-region`：启用交互式区域选择。首次截图前弹出全屏半透明遮罩，用户拖拽框选终端输出区域，坐标缓存到 `/tmp/tp_pull_region.txt`，后续截图复用。
- （可选）`--pull-region-x/y/w/h`：直接指定坐标，跳过交互选择（便于脚本化/复用上一次的区域）。

### 2. ocr.rs 改动
- 新增 `select_region() -> Result<(i32,i32,i32,i32), String>`：
  - 启动一个 Swift 子进程，创建**全屏、半透明、可接收鼠标**的 `NSWindow`（level 设为 `NSWindow.Level.screenSaver` 以上，`ignoresMouseEvents = false`）；
  - 监听 `mouseDown` / `mouseDragged` / `mouseUp`：在鼠标按下处开始绘制选区矩形（用 `CAShapeLayer` 或 `drawRect`），拖拽时实时更新，松开时把 `x,y,w,h` 打到 stdout 并退出；
  - Rust 端解析 stdout 得到坐标。
- 修改 `screenshot(path: &Path, region: Option<(i32,i32,i32,i32)>)`：
  - 若 `region = Some((x,y,w,h))` → `screencapture -x -R x,y,w,h path`；
  - 否则走原有前台窗口 / 全屏逻辑。
- 修改 `screenshot_ocr_lines` 增加 `region` 参数透传。
- 区域坐标缓存：`/tmp/tp_pull_region.txt`，内容 `x,y,w,h`。若文件存在且有效，直接读取复用，不再弹选择框。提供清理（用户可删文件重新选择）。

### 3. pull.rs / cli.rs 改动
- `PullArgs` 增加 `region: Option<(i32,i32,i32,i32)>` 与 `select_region: bool`；
- `run_pull` 开头：若 `select_region` 且缓存不存在 → 调用 `select_region()` 写入缓存，解析后存入 `PullArgs.region`；
- 所有 `screenshot_ocr_lines` 调用传入 `region`。
- cli.rs `Args` 增加 `--pull-region` 开关。

### 4. Swift 选区辅助（内联脚本）
```swift
import AppKit
let app = NSApplication.shared
let mask: NSWindow.StyleMask = [.borderless]
let win = NSWindow(contentRect: NSScreen.main!.frame, styleMask: mask, backing: .buffered, defer: false)
win.level = NSWindow.Level(rawValue: Int(CGWindowLevelForKey(.screenSaverWindow)) + 1)
win.backgroundColor = NSColor.black.withAlphaComponent(0.3)
win.ignoresMouseEvents = false
win.makeKeyAndOrderFront(nil)
// 自定义 NSView 处理鼠标事件，绘制选区
// mouseUp 时 print("\(x),\(y),\(w),\(h)") 然后 exit(0)
app.run()
```

### 5. 风险与处理
- **权限**：选区遮罩窗口需要屏幕录制权限（已有）和辅助功能权限（键盘模拟已有）；遮罩窗口本身无需额外权限。
- **多显示器**：`NSScreen.main` 取主屏；若终端在副屏，用户需把终端移到主屏或后续扩展支持选屏。
- **缓存失效**：用户移动/缩放终端窗口后区域会错位。提供提示："若终端窗口位置变化，请删除 /tmp/tp_pull_region.txt 重新框选"，或在 `--pull-region` 时始终重新选择。
- **坐标正确性**：`screencapture -R` 的坐标系是**屏幕点（points）**，与 `NSEvent` 鼠标坐标一致（注意菜单栏高度偏移：NSEvent 原点在左下角，screencapture -R 原点在左上角，需要 y = screenHeight - y - h）。

## 涉及文件
- `src/ocr.rs` — 新增 `select_region`、`screenshot` 加 region 参数、坐标缓存读写
- `src/pull.rs` — `PullArgs` 加 region/select_region，`run_pull` 触发选择
- `src/cli.rs` — `--pull-region` 参数
- `README.md` — 用法说明

## 验收
- `typepaste-rs --pull /remote/file --pull-region`：首次弹框选区，后续截图只取该区域，OCR 识别率提升。
- 无 `--pull-region` 时行为不变（窗口/全屏截图）。
