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
fn screenshot(path: &Path, region: Option<Region>) -> Result<(), String> {
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

/// 交互式框选屏幕区域，返回 `(x,y,w,h)`（左上原点，points，全局屏幕坐标）。
///
/// 启动 Swift 全屏半透明遮罩（覆盖所有显示器），用户拖拽选择矩形，
/// 松开鼠标后输出坐标。坐标已转换为 `screencapture -R` 所需的全局屏幕坐标。
#[cfg(target_os = "macos")]
pub fn select_region() -> Result<Region, String> {
    let swift_code = r#"
import AppKit

class RegionView: NSView {
    var startPoint: NSPoint?   // 全局 AppKit 坐标（左下原点）
    var currentPoint: NSPoint?
    var onSelect: ((NSPoint, NSPoint) -> Void)?

    // 将全局 AppKit 坐标转换为 screencapture 全局坐标（主屏幕左上原点，y 向下）
    func toScreenCapture(_ p: NSPoint) -> NSPoint {
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

    func toGlobal(_ event: NSEvent) -> NSPoint {
        let win = window!
        return NSPoint(x: win.frame.origin.x + event.locationInWindow.x,
                       y: win.frame.origin.y + event.locationInWindow.y)
    }

    override func draw(_ dirtyRect: NSRect) {
        super.draw(dirtyRect)
        NSColor.black.withAlphaComponent(0.35).setFill()
        dirtyRect.fill()
        guard let start = startPoint, let current = currentPoint else { return }
        // 将全局坐标转回窗口坐标用于绘制
        let win = window!
        let toWin: (NSPoint) -> NSPoint = { p in
            NSPoint(x: p.x - win.frame.origin.x, y: p.y - win.frame.origin.y)
        }
        let s = toWin(start), c = toWin(current)
        let rect = NSRect(x: min(s.x, c.x), y: min(s.y, c.y),
                          width: abs(c.x - s.x), height: abs(c.y - s.y))
        NSColor.white.withAlphaComponent(0.15).setFill()
        rect.fill()
        NSColor.white.setStroke()
        let path = NSBezierPath(rect: rect)
        path.lineWidth = 1.5
        path.stroke()
    }

    override func mouseDown(with event: NSEvent) {
        startPoint = toGlobal(event)
        currentPoint = startPoint
        needsDisplay = true
    }
    override func mouseDragged(with event: NSEvent) {
        currentPoint = toGlobal(event)
        needsDisplay = true
    }
    override func mouseUp(with event: NSEvent) {
        currentPoint = toGlobal(event)
        if let start = startPoint, let current = currentPoint {
            let w = abs(current.x - start.x), h = abs(current.y - start.y)
            if w > 5 && h > 5 { onSelect?(start, current) }
        }
    }
    override func keyDown(with event: NSEvent) {
        if event.keyCode == 53 { exit(0) } // Esc 取消
    }
}

let app = NSApplication.shared
let screens = NSScreen.screens
guard !screens.isEmpty else { exit(1) }
// 覆盖所有显示器的并集矩形
var union = NSRect.zero
for s in screens { union = union.union(s.frame) }
let win = NSWindow(contentRect: union, styleMask: [.borderless], backing: .buffered, defer: false)
win.level = NSWindow.Level(rawValue: Int(CGWindowLevelForKey(.screenSaverWindow)) + 1)
win.backgroundColor = NSColor.clear
win.ignoresMouseEvents = false
win.isOpaque = false
let view = RegionView(frame: union)
win.contentView = view
view.onSelect = { start, current in
    let sc1 = view.toScreenCapture(start)
    let sc2 = view.toScreenCapture(current)
    let x = Int(min(sc1.x, sc2.x).rounded())
    let y = Int(min(sc1.y, sc2.y).rounded())
    let w = Int(abs(sc1.x - sc2.x).rounded())
    let h = Int(abs(sc1.y - sc2.y).rounded())
    print("\(x),\(y),\(w),\(h)")
    exit(0)
}
win.makeKeyAndOrderFront(nil)
app.run()
"#;

    let output = Command::new("swift")
        .args(["-e", swift_code])
        .output()
        .map_err(|e| format!("swift(select_region) 执行失败：{e}"))?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(format!("区域选择失败：{err}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
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
pub fn select_region() -> Result<Region, String> {
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
