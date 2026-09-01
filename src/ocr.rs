//! 屏幕截图 + OCR（macOS Vision 框架）。
//!
//! 反向传输（远程→本机）时，远程终端输出的文本需要通过截图+OCR 读取。
//! 为提高识别率：
//! 1. 优先截取前台窗口（而非全屏），避免菜单栏/Dock/其他窗口干扰；
//! 2. Swift 侧做图像预处理：灰度化、对比度增强、2x 放大，文字更锐利；
//! 3. Vision 请求关闭语言纠正、限定英文识别；
//! 4. 识别后做字符混淆纠正（如 `ó`→`6`、`O`→`0`），再校验 md5。

use std::path::Path;
use std::process::Command;

/// 截取屏幕并 OCR，返回识别的文本行列表（按从上到下顺序）。
///
/// 仅 macOS 实现；非 macOS 返回错误。
/// 截图保存到 /tmp/tp_pull_<ts>.png（不删除），便于调试时查看实际捕获内容。
pub fn screenshot_ocr_lines() -> Result<Vec<String>, String> {
    #[cfg(target_os = "macos")]
    {
        // 年月日时分秒毫秒级时间戳，用于文件名避免冲突
        let ts = chrono::Local::now().format("%Y%m%d%H%M%S%f").to_string();
        let tmp = std::env::temp_dir().join(format!("tp_pull_{ts}.png"));
        screenshot(&tmp)?;
        eprintln!("    截图已保存：{}", tmp.display());
        let text = ocr_image(&tmp)?;
        // 将识别到的文本保存到文件，方便调试时查看
        let ocr = std::env::temp_dir().join(format!("tp_pull_{ts}.ocr"));
        std::fs::write(&ocr, &text).map_err(|e| format!("写入 OCR 文件失败：{e}"))?;
        eprintln!("    OCR 识别已保存：{}", ocr.display());
        Ok(text.lines().map(|l| l.to_string()).collect())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("OCR 仅支持 macOS".to_string())
    }
}

/// 截取前台窗口到指定路径；失败则回退全屏。
#[cfg(target_os = "macos")]
fn screenshot(path: &Path) -> Result<(), String> {
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

/// 用 macOS Vision 框架对图片做 OCR，返回全部文本。
///
/// 通过 Swift 脚本内联调用 VNRecognizeTextRequest，并做灰度+对比度+放大预处理。
#[cfg(target_os = "macos")]
fn ocr_image(path: &Path) -> Result<String, String> {
    let path_str = path.to_string_lossy().to_string();
    let swift_code = format!(
        r#"
import Foundation
import Vision
import AppKit
import CoreImage

let path = "{path}"
guard let img = NSImage(contentsOfFile: path),
      let cg = img.cgImage(forProposedRect: nil, context: nil, hints: nil) else {{
    fputs("ERR:load\n", stderr)
    exit(1)
}}

// 图像预处理：灰度 + 高对比度 + 放大，让文字更锐利
let ci = CIImage(cgImage: cg)
let filter = CIFilter(name: "CIColorControls")
filter?.setValue(ci, forKey: kCIInputImageKey)
filter?.setValue(0.0, forKey: kCIInputSaturationKey)   // 灰度
filter?.setValue(2.5, forKey: kCIInputContrastKey)      // 高对比度
let filtered = filter?.outputImage ?? ci

// 2x 放大（小字体 OCR 更准）
let scaled = filtered.transformed(by: CGAffineTransform(scaleX: 2.0, y: 2.0))

let context = CIContext()
guard let outCg = context.createCGImage(scaled, from: scaled.extent) else {{
    fputs("ERR:proc\n", stderr)
    exit(1)
}}

let req = VNRecognizeTextRequest()
req.recognitionLevel = .accurate
req.usesLanguageCorrection = false
req.recognitionLanguages = ["en-US"]
if #available(macOS 11.0, *) {{
    req.minimumTextHeight = 0.01
}}
let handler = VNImageRequestHandler(cgImage: outCg, options: [:])
do {{
    try handler.perform([req])
}} catch {{
    fputs("ERR:vision\n", stderr)
    exit(1)
}}
let obs = req.results ?? []
let lines = obs.compactMap {{ $0.topCandidates(1).first?.string }}
print(lines.joined(separator: "\n"))
"#,
        path = path_str.replace('\\', "\\\\").replace('"', "\\\"")
    );

    let output = Command::new("swift")
        .args(["-e", &swift_code])
        .output()
        .map_err(|e| format!("swift 执行失败：{e}"))?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(format!("OCR 失败：{err}"));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
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

/// 从 OCR 文本行中提取 base16 分片内容和其 md5 校验值。
///
/// 远程输出：hex 内容行 + 该内容 md5 行（32 位 hex）。
/// 终端会在命令输出后追加 shell 提示符，因此从底部向上扫描：
/// 第一个恰好 32 位 hex 的行是 md5，其上一行（纠正 hex 后）是内容。
pub fn parse_hex_chunk(lines: &[String]) -> Option<(String, String)> {
    let non_empty: Vec<&str> = lines
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    // 从底部向上找第一个 32 位 hex 行（md5）
    for i in (1..non_empty.len()).rev() {
        let md5 = normalize_hex(non_empty[i]);
        if md5.len() == 32 {
            let content = normalize_hex(non_empty[i - 1]);
            if !content.is_empty() {
                return Some((content, md5));
            }
        }
    }
    None
}

/// 从 OCR 文本行中尝试解析文件大小和 md5（探测阶段）。
///
/// 远程输出：文件大小（数字）+ 文件 md5（32 位 hex）。
/// 从底部向上扫描找 md5 行，其上一行纠正数字后为大小。
pub fn parse_file_info(lines: &[String]) -> Option<(usize, String)> {
    let non_empty: Vec<&str> = lines
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    for i in (1..non_empty.len()).rev() {
        let md5 = normalize_hex(non_empty[i]);
        if md5.len() == 32 {
            let size: usize = normalize_digits(non_empty[i - 1]).parse().ok()?;
            return Some((size, md5));
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
        ];
        let (size, md5) = parse_file_info(&lines).unwrap();
        assert_eq!(size, 1024);
        assert_eq!(md5.len(), 32);
    }

    #[test]
    fn parse_file_info_with_trailing_prompt() {
        let lines = vec![
            "stat -c%s /tmp/test.txt".to_string(),
            "1024".to_string(),
            "abcdef0123456789abcdef0123456789".to_string(),
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
        ];
        let (size, _) = parse_file_info(&lines).unwrap();
        assert_eq!(size, 1620);
    }
}
