//! 屏幕截图 + OCR（多引擎：Tesseract / ocrs / Tesseract CLI）。
//!
//! 反向传输（远程→本机）时，远程终端输出的文本需要通过截图+OCR 读取。
//! 为提高识别率：
//! 1. 优先截取前台窗口（而非全屏），避免菜单栏/Dock/其他窗口干扰；
//! 2. 图像预处理：灰度化 + 缩放到合适宽度（Tesseract LSTM 对高分辨率敏感，过宽反而识别下降）；
//! 3. 每轮截图后依次尝试所有（引擎×宽度）组合，直到 md5 校验通过；
//! 4. 所有引擎字符白名单限定 `0-9a-f`，从根本上消除字符混淆；
//! 5. 识别后做字符混淆纠正（兜底），再校验 md5。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Instant;

use crate::utils::md5_of_bytes;
use crate::{debug, info, warn};

/// 屏幕坐标区域（左上原点，points）。
pub type Region = (i32, i32, i32, i32);

/// OCR 引擎。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OcrEngine {
    /// leptess（Rust 绑定 Tesseract）。
    Tesseract,
    /// 纯 Rust ocrs（ONNX，自动下载模型）。
    Ocrs,
    /// 调用系统 `tesseract` 命令行。
    TesseractCli,
}

impl OcrEngine {
    fn name(&self) -> &'static str {
        match self {
            OcrEngine::Tesseract => "tesseract",
            OcrEngine::Ocrs => "ocrs",
            OcrEngine::TesseractCli => "tesseract-cli",
        }
    }
}

/// 每轮截图后依次尝试的引擎顺序。
const OCR_ENGINES: &[OcrEngine] = &[
    OcrEngine::Tesseract,
    OcrEngine::Ocrs,
    OcrEngine::TesseractCli,
];

/// 每轮截图后依次尝试的目标宽度（像素）。
///
/// 每个宽度只做一次图像缩放预处理，再依次用所有引擎识别，避免重复缩放。
const OCR_WIDTHS: &[u32] = &[1000, 900, 1100];

/// 每轮（所有组合失败后）的等待时间（毫秒），递减。
///
/// 重试的目的是等待远程终端渲染完成，首等待较长，后续递减。
pub const OCR_RETRY_WAITS_MS: &[u64] = &[10, 500, 200, 100];

/// 根据轮次（从1开始）返回该轮重试前的等待毫秒数。
pub fn retry_wait_ms(round: usize) -> u64 {
    OCR_RETRY_WAITS_MS[(round - 1).min(OCR_RETRY_WAITS_MS.len() - 1)]
}

/// 截图一次，依次用所有（引擎×宽度）组合做 OCR，返回首个使 `check` 通过的结果行。
///
/// 仅 macOS 实现；非 macOS 返回错误。
/// 若传入 `region`，只截该矩形区域；否则截前台窗口（回退全屏）。
///
/// 流程：截图 → 遍历 [`OCR_WIDTHS`]，每个宽度只缩放一次图片 → 再遍历 [`OCR_ENGINES`]
/// 在同一张缩放图上识别 → 调 `check` → 成功则返回 `Ok(Some(lines))`；
/// 所有组合均失败返回 `Ok(None)`，由调用方决定是否重新截图重试。
pub fn screenshot_ocr_with_check<F>(
    region: Option<Region>,
    check: F,
) -> Result<Option<Vec<String>>, String>
where
    F: Fn(&[String]) -> bool + Sync + Send + 'static,
{
    #[cfg(target_os = "macos")]
    {
        let ts = chrono::Local::now().format("%Y%m%d%H%M%S%f").to_string();
        let tmp = std::env::temp_dir().join(format!("tp_pull_{ts}.png"));
        screenshot(&tmp, region)?;
        debug!("截图已保存: {}", tmp.display());

        let check = Arc::new(check);
        for &target_width in OCR_WIDTHS {
            // 每个宽度只预处理（缩放）一次
            let proc_start = Instant::now();
            let (proc_path, gray_img) = match preprocess_image(&tmp, target_width) {
                Ok(v) => v,
                Err(e) => {
                    warn!("预处理失败（宽度 {target_width}px）：{e}，跳过");
                    continue;
                }
            };
            debug!(
                "目标宽度: {target_width}px（预处理 {:.0}ms）",
                proc_start.elapsed().as_secs_f64() * 1000.0
            );
            // 同一宽度下，各引擎并行识别；首个 check 通过的引擎立即通过 channel 返回，
            // 其余引擎检测到 done 后不再写结果（其 OCR 仍在后台跑完，但不阻塞主线程）。
            let done = Arc::new(AtomicBool::new(false));
            let proc_path = Arc::new(proc_path);
            let gray_img = Arc::new(gray_img);
            let (tx, rx) = mpsc::channel::<Vec<String>>();

            let mut handles = Vec::new();
            for &engine in OCR_ENGINES {
                let tx = tx.clone();
                let done = done.clone();
                let proc_path = proc_path.clone();
                let gray_img = gray_img.clone();
                let check = check.clone();
                handles.push(std::thread::spawn(move || {
                    if done.load(Ordering::Relaxed) {
                        return;
                    }
                    let ocr_start = Instant::now();
                    let text = match ocr_with_engine(&proc_path, &gray_img, engine) {
                        Ok(t) => t,
                        Err(e) => {
                            warn!(
                                "引擎 {} 失败（{:.0}ms）：{e}，跳过",
                                engine.name(),
                                ocr_start.elapsed().as_secs_f64() * 1000.0
                            );
                            return;
                        }
                    };
                    debug!(
                        "引擎 {} 完成（{:.0}ms）",
                        engine.name(),
                        ocr_start.elapsed().as_secs_f64() * 1000.0
                    );

                    if done.load(Ordering::Relaxed) {
                        return;
                    }
                    let lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
                    if check(&lines) {
                        info!("引擎 {} 通过 check", engine.name());
                        done.store(true, Ordering::Relaxed);
                        let _ = tx.send(lines);
                    }
                }));
            }
            drop(tx);

            match rx.recv() {
                Ok(lines) => return Ok(Some(lines)),
                Err(_) => {
                    // 所有引擎均未通过 check，等待线程结束后尝试下一宽度
                    for h in handles {
                        let _ = h.join();
                    }
                }
            }
        }
        Ok(None)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (region, check);
        Err("OCR 仅支持 macOS".to_string())
    }
}

/// 截图到指定路径。
///
/// 若 `region = Some((x,y,w,h))`，只截该矩形区域（左上原点，points）；
/// 否则优先截前台窗口，失败回退全屏。
#[cfg(target_os = "macos")]
pub fn screenshot(path: &Path, region: Option<Region>) -> Result<(), String> {
    if let Some((x, y, w, h)) = region {
        let rect = format!("{x},{y},{w},{h}");
        let status = Command::new("screencapture")
            .args(["-x", "-R", &rect])
            .arg(path)
            .status()
            .map_err(|e| format!("screencapture(region) 执行失败：{e}"))?;
        if !status.success() {
            return Err(format!("screencapture -R 退出码：{status:?}"));
        }
        return Ok(());
    }
    // 尝试获取前台窗口 ID，只截该窗口（去噪）
    if let Ok(wid) = frontmost_window_id() {
        let status = Command::new("screencapture")
            .args(["-x", "-o", "-l", &wid])
            .arg(path)
            .status()
            .map_err(|e| format!("screencapture(window) 执行失败：{e}"))?;
        if status.success() {
            return Ok(());
        }
    }
    // 回退：全屏
    let status = Command::new("screencapture")
        .args(["-x", "-t", "png"])
        .arg(path)
        .status()
        .map_err(|e| format!("screencapture 执行失败：{e}"))?;
    if !status.success() {
        return Err(format!("screencapture 退出码：{status:?}"));
    }
    Ok(())
}

/// 获取前台窗口 ID（通过 AppleScript，需要辅助功能权限）。
#[cfg(target_os = "macos")]
fn frontmost_window_id() -> Result<String, String> {
    let output = Command::new("osascript")
        .arg("-e")
        .arg("tell application \"System Events\" to get id of window 1 of (first process whose frontmost is true)")
        .output()
        .map_err(|e| format!("osascript 执行失败：{e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// 本机前台 App 焦点监视器（macOS）。
///
/// 常驻一个 Swift 进程，通过 `NSWorkspace.didActivateApplicationNotification`
/// 事件驱动地报告前台 App 变化（无轮询、延迟约几十毫秒）。传输开始时前台 App
/// 为基线（VDI 客户端）；打字循环每字符检查 `is_focused()`，一旦用户切走
/// 立即停止输入，避免键盘命令打到其他窗口产生脏数据。
#[cfg(target_os = "macos")]
pub struct FocusMonitor {
    focused: Arc<AtomicBool>,
    child: Mutex<Option<std::process::Child>>,
}

#[cfg(target_os = "macos")]
impl FocusMonitor {
    /// 启动监视器：以当前前台 App 为基线，之后前台 App 变化即失焦。
    pub fn start() -> Result<Self, String> {
        let swift_code = r#"
import AppKit
let ws = NSWorkspace.shared
if let f = ws.frontmostApplication {
    print("BASE:" + (f.bundleIdentifier ?? ""))
    fflush(stdout)
}
ws.notificationCenter.addObserver(
    forName: NSWorkspace.didActivateApplicationNotification,
    object: nil, queue: nil) { note in
    if let a = note.userInfo?[NSWorkspace.applicationUserInfoKey] as? NSRunningApplication {
        print("ACT:" + (a.bundleIdentifier ?? ""))
        fflush(stdout)
    }
}
RunLoop.main.run()
"#;
        let mut child = Command::new("swift")
            .args(["-e", swift_code])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("焦点监视器启动失败：{e}"))?;
        let stdout = child.stdout.take().ok_or("焦点监视器无 stdout")?;
        // Rust 端立即捕获基线前台 App（osascript 约百毫秒），避免 swift 编译
        // 启动（约数秒）期间基线缺失的盲区
        let baseline_now = Command::new("osascript")
            .arg("-e")
            .arg("tell application \"System Events\" to get bundle identifier of (first process whose frontmost is true)")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty());
        let focused = Arc::new(AtomicBool::new(true));
        let f2 = focused.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            let reader = std::io::BufReader::new(stdout);
            // 基线：优先 osascript 即时结果，swift 的 BASE 行作为确认/兜底
            let mut baseline: Option<String> = baseline_now;
            for line in reader.lines().map_while(Result::ok) {
                if let Some(id) = line.strip_prefix("BASE:") {
                    if baseline.is_none() {
                        baseline = Some(id.to_string());
                    }
                    f2.store(true, Ordering::Relaxed);
                } else if let Some(id) = line.strip_prefix("ACT:") {
                    // 首次激活即基线（兜底 BASE 缺失）；与基线不同视为失焦
                    if baseline.is_none() {
                        baseline = Some(id.to_string());
                    }
                    f2.store(baseline.as_deref() == Some(id), Ordering::Relaxed);
                }
            }
        });
        Ok(FocusMonitor {
            focused,
            child: Mutex::new(Some(child)),
        })
    }

    /// 前台 App 是否仍为传输开始时的基线 App。
    pub fn is_focused(&self) -> bool {
        self.focused.load(Ordering::Relaxed)
    }
}

#[cfg(target_os = "macos")]
impl Drop for FocusMonitor {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub struct FocusMonitor;

#[cfg(not(target_os = "macos"))]
impl FocusMonitor {
    pub fn is_focused(&self) -> bool {
        true
    }
}

/// 交互式框选屏幕区域，返回 `(x,y,w,h)`（左上原点，points，全局屏幕坐标）。
///
/// 启动 Swift 全屏半透明遮罩（覆盖所有显示器）：拖拽框选后可拖角调整大小、
/// 拖动内部移动、外部按下重选，回车/双击确认，Esc 取消。
/// `hint` 为屏幕中心提示文本。坐标已转换为 `screencapture -R` 所需的全局屏幕坐标。
#[cfg(target_os = "macos")]
pub fn select_region(hint: &str) -> Result<Region, String> {
    let swift_code = r#"
import AppKit

// 屏幕中心提示语（由 Rust 调用方注入，替换 __HINT__ 占位符）
let hintText = "__HINT__"

// borderless 窗口默认 canBecomeKey=false，收不到键盘事件（Esc 取消无效）；
// 子类化放开限制，并在窗口级直接响应 Esc，不依赖视图是否为第一响应者。
class KeyWindow: NSWindow {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { true }
    override func keyDown(with event: NSEvent) {
        if event.keyCode == 53 { exit(0) } // Esc 取消
        super.keyDown(with: event)
    }
}

// 交互模式：空闲 / 新建框选 / 移动选区 / 角点调整大小
enum DragMode { case idle, creating, moving, resizing }

// 选区状态在所有显示器视图间共享（坐标均为全局 AppKit 坐标）
class SelectionState {
    var a: NSPoint?
    var b: NSPoint?
    var selected = false        // 松开后选区已确定（可调整 / 按钮确认）
    var mode: DragMode = .idle
    var resizeCorner = -1       // 调整大小的角点索引（0左下 1右下 2右上 3左上）
    var fixedPoint = NSPoint.zero  // 调整大小时固定的对角点（按下时记录，拖过对侧不翻转）
    var dragAnchor = NSPoint.zero
    var moveA0 = NSPoint.zero, moveB0 = NSPoint.zero  // 移动起始的两角
    var onSelect: ((NSPoint, NSPoint) -> Void)?
}

// 全局 AppKit 坐标（左下原点）→ screencapture 全局坐标（主屏左上原点，y 向下）
func globalToScreenCapture(_ p: NSPoint) -> NSPoint {
    for screen in NSScreen.screens {
        if NSPointInRect(p, screen.frame) {
            let num = screen.deviceDescription[NSDeviceDescriptionKey(rawValue: "NSScreenNumber")] as! CGDirectDisplayID
            let b = CGDisplayBounds(num)
            let lx = p.x - screen.frame.origin.x
            let lyTop = screen.frame.height - (p.y - screen.frame.origin.y)
            return NSPoint(x: b.origin.x + lx, y: b.origin.y + lyTop)
        }
    }
    if let m = NSScreen.main { return NSPoint(x: p.x, y: m.frame.height - p.y) }
    return p
}

class RegionView: NSView {
    let st: SelectionState
    var hoverPoint: NSPoint?    // 本视图鼠标位置（全局），用于十字准线、坐标提示与聚焦边框
    var tracking: NSTrackingArea?
    var btnOK: NSButton!
    var btnCancel: NSButton!

    let handleHit: CGFloat = 12  // 角点吸附阈值（points）

    init(frame: NSRect, st: SelectionState) {
        self.st = st
        super.init(frame: frame)
        setupButtons()
    }
    required init?(coder: NSCoder) { fatalError("init(coder:) 未实现") }

    override var acceptsFirstResponder: Bool { true }
    // 非激活 App 的首次点击直接投递到视图（否则首次点击只用于激活窗口，表现为「先点一下才能拖」）
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    func toGlobal(_ event: NSEvent) -> NSPoint {
        let win = window!
        return NSPoint(x: win.frame.origin.x + event.locationInWindow.x,
                       y: win.frame.origin.y + event.locationInWindow.y)
    }

    // 选区矩形（全局坐标）
    func rectGlobal() -> NSRect? {
        guard let a = st.a, let b = st.b else { return nil }
        return NSRect(x: min(a.x, b.x), y: min(a.y, b.y),
                      width: abs(a.x - b.x), height: abs(a.y - b.y))
    }
    // 四角（全局，AppKit 左下原点）：0左下 1右下 2右上 3左上
    func corners(_ r: NSRect) -> [NSPoint] {
        return [NSPoint(x: r.minX, y: r.minY), NSPoint(x: r.maxX, y: r.minY),
                NSPoint(x: r.maxX, y: r.maxY), NSPoint(x: r.minX, y: r.maxY)]
    }

    func setupButtons() {
        btnOK = NSButton(title: "确定", target: self, action: #selector(confirmBtn(_:)))
        btnCancel = NSButton(title: "取消", target: self, action: #selector(cancelBtn(_:)))
        for b in [btnOK, btnCancel] {
            b!.bezelStyle = .rounded
            b!.isHidden = true
            addSubview(b!)
        }
    }
    @objc func confirmBtn(_ sender: Any?) { confirm() }
    @objc func cancelBtn(_ sender: Any?) { exit(0) }

    // 按钮贴在选框下方（空间不足则上方）；选区不在本屏或正在拖拽时隐藏
    func updateButtons() {
        guard let win = window else { return }
        var local = NSRect.zero
        let show: Bool
        if st.selected && st.mode == .idle, let r = rectGlobal() {
            local = NSRect(x: r.minX - win.frame.origin.x, y: r.minY - win.frame.origin.y,
                           width: r.width, height: r.height)
            show = local.intersects(bounds)
        } else {
            show = false
        }
        btnOK.isHidden = !show
        btnCancel.isHidden = !show
        guard show else { return }
        let h: CGFloat = 28, w: CGFloat = 84, gap: CGFloat = 10
        let total = w * 2 + gap
        var bx = local.midX - total / 2
        bx = max(8, min(bx, bounds.width - total - 8))
        var by = local.minY - 12 - h
        if by < 8 { by = local.maxY + 12 }
        if by + h > bounds.height - 8 { by = bounds.height - h - 8 }
        btnCancel.frame = NSRect(x: bx, y: by, width: w, height: h)
        btnOK.frame = NSRect(x: bx + w + gap, y: by, width: w, height: h)
    }

    override func draw(_ dirtyRect: NSRect) {
        super.draw(dirtyRect)
        NSColor.black.withAlphaComponent(0.35).setFill()
        dirtyRect.fill()

        // 整屏边框：鼠标所在屏幕（或正在操作的屏幕）高亮聚焦，其余屏幕淡色边框
        let focused = hoverPoint != nil || st.mode != .idle
        let border = NSBezierPath(rect: bounds.insetBy(dx: focused ? 2 : 1, dy: focused ? 2 : 1))
        border.lineWidth = focused ? 4 : 1
        (focused ? NSColor.systemBlue : NSColor.white.withAlphaComponent(0.25)).setStroke()
        border.stroke()

        let win = window!
        let toWin: (NSPoint) -> NSPoint = { p in
            NSPoint(x: p.x - win.frame.origin.x, y: p.y - win.frame.origin.y)
        }

        // 未开始任何框选前：每屏中心显示调用方提示语；一旦开始框选（st.a != nil）全部清除
        if st.a == nil {
            drawCenterHint(hintText)
        }

        // 十字准线：调整大小/移动中隐藏；光标在选区内部或边框/手柄上隐藏；其余显示
        let crossPoint: NSPoint?
        switch st.mode {
        case .resizing, .moving: crossPoint = nil
        case .creating: crossPoint = st.b
        case .idle: crossPoint = hoverPoint
        }
        var crossHidden = false
        if st.mode == .resizing || st.mode == .moving {
            crossHidden = true
        } else if let cp = crossPoint, st.selected, let r = rectGlobal() {
            if NSPointInRect(cp, r)
                || corners(r).contains(where: { hypot($0.x - cp.x, $0.y - cp.y) <= handleHit }) {
                crossHidden = true
            }
        }
        if !crossHidden, let cross = crossPoint {
            let cp = toWin(cross)
            NSColor.white.withAlphaComponent(0.6).setStroke()
            let vline = NSBezierPath(); vline.lineWidth = 0.5
            vline.move(to: NSPoint(x: cp.x, y: 0)); vline.line(to: NSPoint(x: cp.x, y: bounds.height))
            vline.stroke()
            let hline = NSBezierPath(); hline.lineWidth = 0.5
            hline.move(to: NSPoint(x: 0, y: cp.y)); hline.line(to: NSPoint(x: bounds.width, y: cp.y))
            hline.stroke()
        }

        if let r = rectGlobal() {
            let s = toWin(NSPoint(x: r.minX, y: r.minY))
            let rect = NSRect(x: s.x, y: s.y, width: r.width, height: r.height)
            NSColor.white.withAlphaComponent(0.15).setFill()
            rect.fill()
            NSColor.white.setStroke()
            let path = NSBezierPath(rect: rect)
            path.lineWidth = 1.5
            path.stroke()

            // 已选区：四角绘制调整手柄
            if st.selected {
                for cp in corners(r) {
                    let wp = toWin(cp)
                    let h = NSRect(x: wp.x - 4, y: wp.y - 4, width: 8, height: 8)
                    NSColor.systemBlue.setFill()
                    NSBezierPath(rect: h).fill()
                    NSColor.white.setStroke()
                    let hs = NSBezierPath(rect: h); hs.lineWidth = 1; hs.stroke()
                }
            }

            let w = Int(r.width.rounded()), h = Int(r.height.rounded())
            if st.mode != .idle, let b = st.b {
                // 拖拽中：光标旁显示选区尺寸 + 当前坐标
                let sc = globalToScreenCapture(b)
                drawLabel("\(w) × \(h)   x: \(Int(sc.x.rounded()))  y: \(Int(sc.y.rounded()))",
                          near: toWin(b))
            } else if st.selected {
                // 选区确定：尺寸与调整提示贴在选框上沿中部（常显）
                drawLabel("\(w) × \(h) · 拖角调整 · 内部移动 · 双击或确定确认",
                          near: NSPoint(x: rect.midX, y: rect.maxY))
            }
        }

        // 空闲时光标旁始终显示当前全局坐标（与最终输出一致）
        if st.mode == .idle, let hover = hoverPoint {
            let sc = globalToScreenCapture(hover)
            drawLabel("x: \(Int(sc.x.rounded()))  y: \(Int(sc.y.rounded()))", near: toWin(hover))
        }
    }

    // 深色圆角背景 + 等宽白字标签，自动避开屏幕边缘
    func drawLabel(_ text: String, near p: NSPoint) {
        let attrs: [NSAttributedString.Key: Any] = [
            .font: NSFont.monospacedDigitSystemFont(ofSize: 13, weight: .medium),
            .foregroundColor: NSColor.white
        ]
        let textSize = (text as NSString).size(withAttributes: attrs)
        let padX: CGFloat = 8, padY: CGFloat = 4
        let labelW = textSize.width + padX * 2
        let labelH = textSize.height + padY * 2
        var lx = p.x + 14
        var ly = p.y + 14
        if lx + labelW > bounds.width - 4 { lx = p.x - 14 - labelW }
        if ly + labelH > bounds.height - 4 { ly = p.y - 14 - labelH }
        lx = max(lx, 4); ly = max(ly, 4)
        let bg = NSRect(x: lx, y: ly, width: labelW, height: labelH)
        NSColor.black.withAlphaComponent(0.78).setFill()
        NSBezierPath(roundedRect: bg, xRadius: 5, yRadius: 5).fill()
        (text as NSString).draw(at: NSPoint(x: lx + padX, y: ly + padY), withAttributes: attrs)
    }

    // 屏幕中心的操作提示：深色圆角背景 + 白色大字
    func drawCenterHint(_ text: String) {
        let attrs: [NSAttributedString.Key: Any] = [
            .font: NSFont.systemFont(ofSize: 20, weight: .medium),
            .foregroundColor: NSColor.white
        ]
        let textSize = (text as NSString).size(withAttributes: attrs)
        let padX: CGFloat = 20, padY: CGFloat = 12
        let labelW = textSize.width + padX * 2
        let labelH = textSize.height + padY * 2
        let bg = NSRect(x: (bounds.width - labelW) / 2, y: (bounds.height - labelH) / 2,
                        width: labelW, height: labelH)
        NSColor.black.withAlphaComponent(0.72).setFill()
        NSBezierPath(roundedRect: bg, xRadius: 10, yRadius: 10).fill()
        NSColor.white.withAlphaComponent(0.9).setStroke()
        let outline = NSBezierPath(roundedRect: bg, xRadius: 10, yRadius: 10)
        outline.lineWidth = 1
        outline.stroke()
        (text as NSString).draw(at: NSPoint(x: bg.minX + padX, y: bg.minY + padY), withAttributes: attrs)
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let ta = tracking { removeTrackingArea(ta) }
        // 必须含 .mouseEnteredAndExited，否则 mouseExited 不触发（鼠标离屏后边框不淡）
        tracking = NSTrackingArea(rect: bounds,
                                  options: [.mouseMoved, .mouseEnteredAndExited, .activeAlways, .inVisibleRect],
                                  owner: self, userInfo: nil)
        addTrackingArea(tracking!)
    }
    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .crosshair)
    }
    // 角点调整光标：AppKit 无公开对角 resize 光标，用 SF Symbol 对角箭头构造
    // ↗↙ 用于左下(0)/右上(2)角，↖↘ 用于右下(1)/左上(3)角
    static let diagNESW: NSCursor = makeDiagCursor("arrow.up.right.and.arrow.down.left")
    static let diagNWSE: NSCursor = makeDiagCursor("arrow.up.left.and.arrow.down.right")
    static func makeDiagCursor(_ sym: String) -> NSCursor {
        if let img = NSImage(systemSymbolName: sym, accessibilityDescription: nil)?
            .withSymbolConfiguration(NSImage.SymbolConfiguration(pointSize: 16, weight: .black)) {
            return NSCursor(image: img, hotSpot: NSPoint(x: img.size.width / 2, y: img.size.height / 2))
        }
        return .crosshair
    }
    // 根据命中位置返回光标：角点对角箭头、选区内部 openHand（拖动时 closedHand）
    func cursorFor(_ p: NSPoint) -> NSCursor {
        if st.selected, let r = rectGlobal() {
            if let hit = corners(r).firstIndex(where: { hypot($0.x - p.x, $0.y - p.y) <= handleHit }) {
                return (hit == 0 || hit == 2) ? RegionView.diagNESW : RegionView.diagNWSE
            }
            if NSPointInRect(p, r) { return NSCursor.openHand }
        }
        return NSCursor.crosshair
    }
    override func mouseEntered(with event: NSEvent) {
        hoverPoint = toGlobal(event)
        updateButtons()
        needsDisplay = true
    }
    override func mouseMoved(with event: NSEvent) {
        let p = toGlobal(event)
        hoverPoint = p
        if st.mode == .idle { cursorFor(p).set(); updateButtons() }
        needsDisplay = true
    }
    override func mouseExited(with event: NSEvent) {
        hoverPoint = nil
        updateButtons()
        needsDisplay = true
    }

    // 状态变化后刷新所有显示器视图：选区为全局坐标，任屏操作都要让各屏同步
    // 重绘（清除其它屏幕中心提示、显示跨屏选框/按钮）
    func refreshAll() {
        for w in NSApp.windows {
            if let v = w.contentView as? RegionView {
                v.updateButtons()
                v.needsDisplay = true
            }
        }
    }

    override func mouseDown(with event: NSEvent) {
        window?.makeKey()
        let p = toGlobal(event)
        if st.selected, let r = rectGlobal() {
            // 双击选区内部：直接确认
            if event.clickCount == 2 && NSPointInRect(p, r) { confirm(); return }
            // 角点命中 → 调整大小；内部命中 → 移动；外部 → 重新框选
            if let hit = corners(r).firstIndex(where: { hypot($0.x - p.x, $0.y - p.y) <= handleHit }) {
                st.mode = .resizing
                st.resizeCorner = hit
                st.fixedPoint = corners(r)[(hit + 2) % 4]  // 固定对角点
            } else if NSPointInRect(p, r) {
                st.mode = .moving
                st.dragAnchor = p
                st.moveA0 = st.a!; st.moveB0 = st.b!
            } else {
                st.mode = .creating
                st.selected = false
                st.a = p; st.b = p
            }
        } else {
            st.mode = .creating
            st.selected = false
            st.a = p; st.b = p
        }
        refreshAll()
    }
    override func mouseDragged(with event: NSEvent) {
        let p = toGlobal(event)
        switch st.mode {
        case .creating:
            st.b = p
        case .moving:
            let d = NSPoint(x: p.x - st.dragAnchor.x, y: p.y - st.dragAnchor.y)
            st.a = NSPoint(x: st.moveA0.x + d.x, y: st.moveA0.y + d.y)
            st.b = NSPoint(x: st.moveB0.x + d.x, y: st.moveB0.y + d.y)
            NSCursor.closedHand.set()
        case .resizing:
            // 固定对角点，移动被拖角点（固定点按下时已记录，拖过对侧也不会翻转）
            st.a = st.fixedPoint; st.b = p
            // 按起始角点锁定对角调整光标（拖拽中鼠标可能离开角点命中范围）
            let c = st.resizeCorner
            ((c == 0 || c == 2) ? RegionView.diagNESW : RegionView.diagNWSE).set()
        case .idle:
            break
        }
        refreshAll()
    }
    override func mouseUp(with event: NSEvent) {
        if st.mode == .creating {
            if let r = rectGlobal(), r.width > 5 && r.height > 5 {
                st.selected = true   // 松开后进入可调整状态，等待按钮/回车确认
            } else {
                st.a = nil; st.b = nil
            }
        }
        st.mode = .idle
        refreshAll()
    }

    func confirm() {
        guard st.selected, let a = st.a, let b = st.b else { return }
        st.onSelect?(a, b)
    }
    override func keyDown(with event: NSEvent) {
        if event.keyCode == 53 { exit(0) }            // Esc 取消
        if event.keyCode == 36 { confirm() }          // 回车确认
    }
}

let app = NSApplication.shared
// swift -e 启动的进程默认不是前台 GUI App，不激活则收不到键盘事件（Esc 无效）
app.setActivationPolicy(.accessory)
app.activate(ignoringOtherApps: true)
let screens = NSScreen.screens
guard !screens.isEmpty else { exit(1) }

// 每个显示器创建独立遮罩窗口：单个跨多屏的 NSWindow 在副屏区域无法接收
// 鼠标事件（事件路由只落在窗口所属屏幕），逐屏建窗保证所有显示器均可框选。
// 所有视图共享一个 SelectionState（选区为全局坐标，任屏操作各屏同步）。
let st = SelectionState()
st.onSelect = { start, current in
    let sc1 = globalToScreenCapture(start)
    let sc2 = globalToScreenCapture(current)
    let x = Int(min(sc1.x, sc2.x).rounded())
    let y = Int(min(sc1.y, sc2.y).rounded())
    let w = Int(abs(sc1.x - sc2.x).rounded())
    let h = Int(abs(sc1.y - sc2.y).rounded())
    print("\(x),\(y),\(w),\(h)")
    exit(0)
}
var windows: [KeyWindow] = []
for screen in screens {
    let win = KeyWindow(contentRect: screen.frame, styleMask: [.borderless], backing: .buffered, defer: false)
    win.level = NSWindow.Level(rawValue: Int(CGWindowLevelForKey(.screenSaverWindow)) + 1)
    win.backgroundColor = NSColor.clear
    win.ignoresMouseEvents = false
    win.isOpaque = false
    let view = RegionView(frame: NSRect(origin: .zero, size: screen.frame.size), st: st)
    win.contentView = view
    win.makeFirstResponder(view)
    win.makeKeyAndOrderFront(nil)
    windows.append(win)
}
app.run()
"#;
    // 注入中心提示语（转义反斜杠与引号，防止破坏 Swift 字符串）
    let hint_esc = hint.replace('\\', "\\\\").replace('"', "\\\"");
    let swift_code = swift_code.replace("__HINT__", &hint_esc);

    let output = Command::new("swift")
        .args(["-e", &swift_code])
        .output()
        .map_err(|e| format!("swift(select_region) 执行失败：{e}"))?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(format!("区域选择失败：{err}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        // Esc 取消：Swift 端 exit(0) 且无坐标输出
        return Err("已取消区域选择".to_string());
    }
    let parts: Vec<&str> = trimmed.split(',').collect();
    if parts.len() != 4 {
        return Err(format!("区域选择输出格式错误：{trimmed}"));
    }
    let nums: Vec<i32> = parts.iter().filter_map(|p| p.trim().parse().ok()).collect();
    if nums.len() != 4 {
        return Err(format!("区域选择坐标解析失败：{trimmed}"));
    }
    Ok((nums[0], nums[1], nums[2], nums[3]))
}

#[cfg(not(target_os = "macos"))]
pub fn select_region(_hint: &str) -> Result<Region, String> {
    Err("区域选择仅支持 macOS".to_string())
}

/// 图像预处理：灰度 + 缩放到 `target_width`（保持比例），保存到临时文件。
///
/// 返回（预处理图片路径，灰度图内存对象），供不同引擎复用，避免每个引擎重复缩放。
///
/// `target_width` 影响识别率：过高分辨率（如 2000+px）反而导致
/// 细字符（`1`）被吞没；不同尺寸识别结果不同，重试时切换尺寸可提高成功率。
fn preprocess_image(path: &Path, target_width: u32) -> Result<(PathBuf, image::GrayImage), String> {
    let img = image::open(path).map_err(|e| format!("图片加载失败：{e}"))?;
    let (w, h) = (img.width(), img.height());
    let new_w = target_width.max(100);
    let new_h = ((h as u64) * (new_w as u64) / (w as u64).max(1)) as u32;
    let processed =
        img.grayscale()
            .resize_exact(new_w, new_h.max(1), image::imageops::FilterType::Triangle);
    let gray = processed.to_luma8();

    let ts = chrono::Local::now().format("%Y%m%d%H%M%S%f").to_string();
    let proc_path = std::env::temp_dir().join(format!("tp_ocr_proc_{ts}.png"));
    // 保存纯灰度图（Luma8，无 alpha 通道），避免 leptonica 读取时做
    // gray+alpha → RGBA 转换并输出 "Info in pixReadStreamPng" 噪音。
    gray.save(&proc_path)
        .map_err(|e| format!("预处理图片保存失败：{e}"))?;
    debug!("预处理图片已保存: {}", proc_path.display());
    Ok((proc_path, gray))
}

/// 按引擎分派到不同后端（Tesseract / ocrs / Tesseract CLI），各后端均限定
/// 字符白名单 `0123456789abcdef`。`proc_path` 为已缩放的灰度图文件，
/// `gray_img` 为同一张图的内存对象（ocrs 直接使用，避免重复读取文件）。
fn ocr_with_engine(
    proc_path: &Path,
    gray_img: &image::GrayImage,
    engine: OcrEngine,
) -> Result<String, String> {
    match engine {
        OcrEngine::Tesseract => ocr_with_leptess(proc_path),
        OcrEngine::Ocrs => ocr_with_ocrs(gray_img),
        OcrEngine::TesseractCli => ocr_with_tesseract_cli(proc_path),
    }
}

/// 用 leptess（Rust 绑定 Tesseract）做 OCR。
fn ocr_with_leptess(proc_path: &Path) -> Result<String, String> {
    let tess = tesseract_instance()?;
    let mut tess = tess
        .lock()
        .map_err(|_| "Tesseract 实例锁失败".to_string())?;
    tess.set_image(proc_path)
        .map_err(|e| format!("设置图片失败：{e}"))?;
    tess.set_source_resolution(300);
    tess.get_utf8_text()
        .map_err(|e| format!("OCR 识别失败：{e}"))
}

/// 用 ocrs（纯 Rust）做 OCR，字符限定为 hex。
fn ocr_with_ocrs(img: &image::GrayImage) -> Result<String, String> {
    let engine = ocrs_instance()?;
    let src = ocrs::ImageSource::from_bytes(img.as_raw(), img.dimensions())
        .map_err(|e| format!("ocrs ImageSource 失败：{e}"))?;
    let input = engine
        .prepare_input(src)
        .map_err(|e| format!("ocrs prepare_input 失败：{e}"))?;
    engine
        .get_text(&input)
        .map_err(|e| format!("ocrs 识别失败：{e}"))
}

/// 调用系统 `tesseract` 命令行做 OCR。
fn ocr_with_tesseract_cli(proc_path: &Path) -> Result<String, String> {
    let output = Command::new("tesseract")
        .args([
            proc_path.to_str().unwrap_or(""),
            "stdout",
            "--psm",
            "4",
            "-c",
            "tessedit_char_whitelist=0123456789abcdef",
        ])
        .output()
        .map_err(|e| format!("tesseract 命令执行失败（请确认已安装 tesseract）：{e}"))?;
    if !output.status.success() {
        return Err(format!(
            "tesseract 退出码：{:?}，stderr：{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// 全局 Tesseract(leptess) 实例，懒加载并复用。
fn tesseract_instance() -> Result<&'static Mutex<leptess::LepTess>, String> {
    static INSTANCE: OnceLock<Mutex<leptess::LepTess>> = OnceLock::new();
    if let Some(lock) = INSTANCE.get() {
        return Ok(lock);
    }
    let mut lt = leptess::LepTess::new(None, "eng")
        .map_err(|e| format!("Tesseract 初始化失败（请确认已安装 tesseract + eng 语言包）：{e}"))?;
    lt.set_variable(leptess::Variable::TesseditCharWhitelist, "0123456789abcdef")
        .map_err(|e| format!("设置字符白名单失败：{e}"))?;
    lt.set_variable(leptess::Variable::TesseditPagesegMode, "4")
        .map_err(|e| format!("设置页面分割模式失败：{e}"))?;
    lt.set_variable(leptess::Variable::TextordNoiseRejwords, "0")
        .map_err(|e| format!("设置噪声过滤失败：{e}"))?;
    lt.set_variable(leptess::Variable::TextordNoiseRejrows, "0")
        .map_err(|e| format!("设置行噪声过滤失败：{e}"))?;
    Ok(INSTANCE.get_or_init(|| Mutex::new(lt)))
}

/// 全局 ocrs 引擎实例，懒加载并复用。
///
/// 首次调用会自动下载检测/识别模型（.rten）到 `~/.cache/ocrs`，需网络。
fn ocrs_instance() -> Result<&'static ocrs::OcrEngine, String> {
    static INSTANCE: OnceLock<ocrs::OcrEngine> = OnceLock::new();
    if let Some(engine) = INSTANCE.get() {
        return Ok(engine);
    }

    // 模型缓存目录：~/.cache/ocrs
    let cache_dir = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(".cache")
        .join("ocrs");
    std::fs::create_dir_all(&cache_dir).map_err(|e| format!("创建 ocrs 模型缓存目录失败：{e}"))?;

    let det_path = cache_dir.join("text-detection.rten");
    let rec_path = cache_dir.join("text-recognition.rten");
    const DET_URL: &str = "https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.rten";
    const REC_URL: &str = "https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.rten";

    download_if_missing(&det_path, DET_URL)?;
    download_if_missing(&rec_path, REC_URL)?;

    let detection_model =
        rten::Model::load_file(&det_path).map_err(|e| format!("加载 ocrs 检测模型失败：{e}"))?;
    let recognition_model =
        rten::Model::load_file(&rec_path).map_err(|e| format!("加载 ocrs 识别模型失败：{e}"))?;

    let params = ocrs::OcrEngineParams {
        detection_model: Some(detection_model),
        recognition_model: Some(recognition_model),
        allowed_chars: Some("0123456789abcdef".to_string()),
        ..Default::default()
    };
    let engine = ocrs::OcrEngine::new(params).map_err(|e| format!("ocrs 引擎初始化失败：{e}"))?;
    Ok(INSTANCE.get_or_init(|| engine))
}

/// 若文件不存在，用 curl 从 `url` 下载到 `path`。
fn download_if_missing(path: &Path, url: &str) -> Result<(), String> {
    if path.exists() {
        return Ok(());
    }
    info!(
        "下载 ocrs 模型：{}",
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    let status = Command::new("curl")
        .args(["-sL", "-o"])
        .arg(path)
        .arg(url)
        .status()
        .map_err(|e| {
            format!(
                "curl 执行失败（请确认已安装 curl 或手动下载模型到 {}）：{e}",
                path.display()
            )
        })?;
    if !status.success() {
        // 下载失败时清理可能的空文件
        let _ = std::fs::remove_file(path);
        return Err(format!("模型下载失败（退出码：{status:?}），URL：{url}"));
    }
    Ok(())
}

/// 字符混淆纠正表：把 OCR 常见误识别字符映射回 hex 字符集（0-9a-f）。
///
/// 只处理明显的视觉混淆；歧义字符（如 `B` 可能是 `8` 或 `b`）交给 md5 校验兜底。
fn hex_confusion(c: char) -> Option<char> {
    Some(match c {
        // 数字
        '0'..='9' => c,
        // 小写 hex 字母
        'a'..='f' => c,
        // 大写 hex 字母 → 小写
        'A' => 'a',
        'B' => 'b',
        'C' => 'c',
        'D' => 'd',
        'E' => 'e',
        'F' => 'f',
        // 常见混淆（注意：大写 A-F 已在上一行转为小写 hex）
        'O' | 'o' | 'Ø' | '∅' | 'Q' => '0',
        'l' | 'I' | '|' | 'i' | '!' | 'ı' | 'Í' => '1',
        'Z' | 'z' | 'Ž' | 'ž' => '2',
        'S' | 's' | 'Š' | 'š' | '$' => '5',
        'ó' | 'ò' | 'õ' | 'ô' | 'ö' | 'ő' | 'G' => '6',
        'g' | 'q' | 'ğ' => '9',
        // 其余非 hex 字符丢弃
        _ => return None,
    })
}

/// 把字符串过滤并纠正为仅含 hex 字符（0-9a-f）。
fn normalize_hex(s: &str) -> String {
    s.chars().filter_map(hex_confusion).collect()
}

/// 把字符串过滤并纠正为仅含数字字符（0-9）。
fn normalize_digits(s: &str) -> String {
    s.chars()
        .filter_map(|c| match c {
            '0'..='9' => Some(c),
            'O' | 'o' | 'Ø' | '∅' | 'Q' => Some('0'),
            'l' | 'I' | '|' | 'i' | '!' | 'ı' => Some('1'),
            'Z' | 'z' => Some('2'),
            'S' | 's' | '$' => Some('5'),
            'ó' | 'ò' | 'õ' | 'ô' | 'ö' | 'G' => Some('6'),
            'g' | 'q' => Some('9'),
            _ => None,
        })
        .collect()
}

/// 判断一行是否像 hex 内容段：非空白字符中至少 70% 能被纠正为 hex。
///
/// 真正的内容行（xxd 输出）几乎全是 hex 字符；命令回显/提示符含大量标点和
/// 非 hex 字母，normalize 后保留比例低，以此区分。同时容忍少量 OCR 误识别
/// （个别字符被识别成非 hex 字符），不会因此截断内容。
fn is_hex_content_line(s: &str) -> bool {
    let non_space: usize = s.chars().filter(|c| !c.is_whitespace()).count();
    if non_space == 0 {
        return false;
    }
    let kept = normalize_hex(s).len();
    kept * 10 >= non_space * 7
}

/// 从 OCR 文本行中提取 base16 分片内容和其 md5 校验值。
///
/// 远程输出：hex 内容行 + 该内容 md5 行（32 位 hex）。
/// 终端会在命令输出后追加 shell 提示符，且长 hex 行会自动折行成多个视觉行。
/// 因此从底部向上扫描：第一个恰好 32 位 hex 的行是 md5，再向上收集所有
/// 连续的内容行拼接为内容，遇到非内容行（命令回显、提示符等）停止。
pub fn parse_hex_chunk(lines: &[String]) -> Option<(String, String)> {
    let non_empty: Vec<&str> = lines
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    // 从底部向上找第一个 32 位 hex 行（md5）
    for i in (1..non_empty.len()).rev() {
        // println!("{} ==> {}", i, non_empty[i]);
        let md5 = normalize_hex(non_empty[i]);
        if md5.len() == 32 {
            // 向上收集所有连续的内容行（终端自动换行可能把内容拆成多行）
            let mut content = String::new();
            for j in (0..i).rev() {
                // println!("{} ==> {}", j, non_empty[j]);
                if !is_hex_content_line(non_empty[j]) {
                    break; // 遇到命令回显、提示符等非内容行停止
                }
                content.insert_str(0, &normalize_hex(non_empty[j]));
            }
            if !content.is_empty() {
                return Some((content, md5));
            }
        }
    }
    None
}

/// 从 OCR 文本行中尝试解析文件大小和 md5（探测阶段）。
///
/// 远程输出三行：文件大小（数字）、文件 md5（32 位 hex）、校验和 md5(size+md5)（32 位 hex）。
/// 从底部向上扫描：找底部 32-hex 行为 checksum，其上一行同为 32-hex 为 file_md5，再上一行为 size；
/// 校验 md5(size + file_md5) == checksum 通过才返回，否则返回 None 触发重试。
pub fn parse_file_info(lines: &[String]) -> Option<(usize, String)> {
    let non_empty: Vec<&str> = lines
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    for i in (2..non_empty.len()).rev() {
        let checksum = normalize_hex(non_empty[i]);
        if checksum.len() != 32 {
            continue;
        }
        let file_md5 = normalize_hex(non_empty[i - 1]);
        if file_md5.len() != 32 {
            continue;
        }
        let size: usize = normalize_digits(non_empty[i - 2]).parse().ok()?;
        let expected = md5_of_bytes(format!("{size}{file_md5}").as_bytes());
        if expected == checksum {
            return Some((size, file_md5));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_chunk_normal() {
        let lines = vec![
            "some prompt noise".to_string(),
            "68656c6c6f".to_string(),
            "5d41402abc4b2a76b9719d911017c592".to_string(),
        ];
        let (content, md5) = parse_hex_chunk(&lines).unwrap();
        assert_eq!(content, "68656c6c6f");
        assert_eq!(md5.len(), 32);
    }

    #[test]
    fn parse_hex_chunk_with_trailing_prompt() {
        let lines = vec![
            "clear; sed -n '1p' file".to_string(),
            "68656c6c6f".to_string(),
            "5d41402abc4b2a76b9719d911017c592".to_string(),
            "user@host:~$".to_string(),
        ];
        let (content, md5) = parse_hex_chunk(&lines).unwrap();
        assert_eq!(content, "68656c6c6f");
        assert_eq!(md5, "5d41402abc4b2a76b9719d911017c592");
    }

    #[test]
    fn parse_hex_chunk_filters_noise() {
        let lines = vec![
            "68 65 6c 6c 6f".to_string(),
            "5d41402abc4b2a76b9719d911017c592".to_string(),
        ];
        let (content, _) = parse_hex_chunk(&lines).unwrap();
        assert_eq!(content, "68656c6c6f");
    }

    #[test]
    fn parse_hex_chunk_wrapped_lines() {
        // 长 hex 行被终端自动折成多个视觉行，OCR 返回多行
        let lines = vec![
            "68656c6c6f".to_string(),
            "776f726c64".to_string(),
            "5d41402abc4b2a76b9719d911017c592".to_string(),
            "user@host:~$".to_string(),
        ];
        let (content, md5) = parse_hex_chunk(&lines).unwrap();
        assert_eq!(content, "68656c6c6f776f726c64");
        assert_eq!(md5, "5d41402abc4b2a76b9719d911017c592");
    }

    #[test]
    fn parse_hex_chunk_wrapped_with_noise_above() {
        // 内容折行 + 上方有命令回显噪声
        let lines = vec![
            "clear; sed -n '1p' file".to_string(),
            "68656c6c6f".to_string(),
            "776f726c64".to_string(),
            "5d41402abc4b2a76b9719d911017c592".to_string(),
            "$".to_string(),
        ];
        let (content, _) = parse_hex_chunk(&lines).unwrap();
        assert_eq!(content, "68656c6c6f776f726c64");
    }

    #[test]
    fn parse_hex_chunk_corrects_confusion() {
        // 6 被识别成 ó，O 被识别成 O
        let lines = vec![
            "6ó656c6c6f".to_string(), // 实际应为 68656c6c6f（8 被识别？此处测 ó→6）
            "5d41402abc4b2a76b9719d911017c592".to_string(),
        ];
        let (content, _) = parse_hex_chunk(&lines).unwrap();
        assert_eq!(content, "66656c6c6f");
    }

    #[test]
    fn parse_hex_chunk_o_to_zero() {
        let lines = vec![
            "O".to_string(), // O→0
            "5d41402abc4b2a76b9719d911017c592".to_string(),
        ];
        let (content, _) = parse_hex_chunk(&lines).unwrap();
        assert_eq!(content, "0");
    }

    #[test]
    fn parse_hex_chunk_md5_wrong_len() {
        let lines = vec!["68656c6c6f".to_string(), "5d41402abc4b2a76".to_string()];
        assert!(parse_hex_chunk(&lines).is_none());
    }

    #[test]
    fn parse_file_info_normal() {
        let lines = vec![
            "1024".to_string(),
            "abcdef0123456789abcdef0123456789".to_string(),
            "17e00497124c8f0ae9a581cbfab00312".to_string(), // md5("1024" + file_md5)
        ];
        let (size, md5) = parse_file_info(&lines).unwrap();
        assert_eq!(size, 1024);
        assert_eq!(md5, "abcdef0123456789abcdef0123456789");
    }

    #[test]
    fn parse_file_info_with_trailing_prompt() {
        let lines = vec![
            "stat -c%s /tmp/test.txt".to_string(),
            "1024".to_string(),
            "abcdef0123456789abcdef0123456789".to_string(),
            "17e00497124c8f0ae9a581cbfab00312".to_string(), // md5("1024" + file_md5)
            "$".to_string(),
        ];
        let (size, md5) = parse_file_info(&lines).unwrap();
        assert_eq!(size, 1024);
        assert_eq!(md5, "abcdef0123456789abcdef0123456789");
    }

    #[test]
    fn parse_file_info_digit_confusion() {
        // 6→ó, 0→O, 1→l
        let lines = vec![
            "ló2O".to_string(), // 应为 1620
            "abcdef0123456789abcdef0123456789".to_string(),
            "d69bf443a2c38a48b7cf76bf512f91cf".to_string(), // md5("1620" + file_md5)
        ];
        let (size, md5) = parse_file_info(&lines).unwrap();
        assert_eq!(size, 1620);
        assert_eq!(md5, "abcdef0123456789abcdef0123456789");
    }

    #[test]
    fn parse_file_info_checksum_mismatch() {
        // checksum 错误时应返回 None
        let lines = vec![
            "1024".to_string(),
            "abcdef0123456789abcdef0123456789".to_string(),
            "00000000000000000000000000000000".to_string(), // 错误的 checksum
        ];
        assert!(parse_file_info(&lines).is_none());
    }

    #[test]
    fn parse_file_info_missing_checksum() {
        // 仅两行（旧格式）应返回 None
        let lines = vec![
            "1024".to_string(),
            "abcdef0123456789abcdef0123456789".to_string(),
        ];
        assert!(parse_file_info(&lines).is_none());
    }
}
