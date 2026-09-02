//! 反向传输：远程桌面 → 本机。
//!
//! 远程把文件编码为 base16 分片输出到终端，本地截图+OCR 读取，
//! 每片校验 md5，错则重传该片，最后拼接解码还原文件。

use std::format;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};

use crate::ocr::{
    parse_file_info, parse_hex_chunk, retry_wait_ms, screenshot_ocr_with_check, select_region,
    Region,
};
use crate::restore_script::Shell;
use crate::utils::md5_of_bytes;

/// 反向传输参数。
pub struct PullArgs {
    /// 远程文件路径。
    pub remote_path: String,
    /// 本地输出路径（默认取远程文件名）。
    pub local_out: Option<PathBuf>,
    /// 目标 shell。
    pub shell: Shell,
    /// 每片原始字节数（base16 后为 2 倍字符数）。
    pub chunk_bytes: usize,
    /// 单片最大重试次数。
    pub max_retry: usize,
    /// 每字符输入间隔（毫秒）。
    pub interval: u64,
    /// 倒计时（秒）。
    pub delay: u64,
    /// 预演模式。
    pub dry_run: bool,
    /// 是否启用交互式框选截图区域。
    pub select_region: bool,
    /// 截图区域（左上原点 points），为 None 时用窗口/全屏。
    pub region: Option<Region>,
    /// 每个字符前加空格（提升 OCR 分割准确率）。
    pub char_space: bool,
    /// 分片显示每行字符数。
    pub line_width: usize,
}

/// 反向传输主入口。
pub fn run_pull(
    args: &mut PullArgs,
    stop: &Arc<AtomicBool>,
    mut type_command: impl FnMut(&str, u64, &AtomicBool),
) -> Result<(), String> {
    if args.dry_run {
        return dry_run_pull(args);
    }

    // 交互式框选截图区域
    if args.select_region && args.region.is_none() {
        println!("  请在屏幕上框选终端输出区域（Esc 取消）...");
        let region = select_region()?;
        args.region = Some(region);
        println!("  截图区域：{region:?}");
    }

    println!("━━━ 反向传输（远程 → 本机）━━━");
    println!("  远程文件：{}", args.remote_path);
    println!("  分片大小：{} 字节", args.chunk_bytes);
    println!("  每行宽度：{} 字符", args.line_width);
    println!(
        "  字符间距：{}",
        if args.char_space { "开启" } else { "关闭" }
    );
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━");

    super_countdown(args.delay, stop);
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }

    // 步骤 1：探测文件大小和 md5
    let (file_size, file_md5) = probe_file_info(args, &mut type_command, stop)?;
    println!("文件大小：{file_size} 字节，md5：{file_md5}");

    // 步骤 2：远程把文件转 base16 并按 chunk_bytes*2 字符/行切分
    prepare_remote_chunks(args, &mut type_command, stop)?;
    std::thread::sleep(Duration::from_secs(3));

    let total_chunks = file_size.div_ceil(args.chunk_bytes);
    println!("共 {total_chunks} 片，开始逐片读取...");

    let pb = make_pull_progress(total_chunks as u64);
    let mut all_hex = String::with_capacity(file_size * 2);

    for i in 1..=total_chunks {
        if stop.load(Ordering::Relaxed) {
            pb.finish_and_clear();
            return Ok(());
        }
        let chunk_hex = read_chunk_with_retry(i, args, &mut type_command, stop)?;
        all_hex.push_str(&chunk_hex);
        pb.inc(1);
    }
    pb.finish();

    // 步骤 3：解码 base16 → 字节，写本地文件，校验 md5
    let bytes = hex_decode(&all_hex)?;
    let actual_md5 = md5_of_bytes(&bytes);
    if actual_md5 != file_md5 {
        return Err(format!(
            "文件 md5 校验失败：got={actual_md5} want={file_md5}"
        ));
    }

    let out_path = match &args.local_out {
        Some(p) => p.clone(),
        None => PathBuf::from(
            Path::new(&args.remote_path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "pulled_file".to_string()),
        ),
    };
    std::fs::write(&out_path, &bytes).map_err(|e| format!("写入本地文件失败：{e}"))?;

    println!(
        "\n✅ 已保存到 {}（{} 字节，md5 校验通过）",
        out_path.display(),
        bytes.len()
    );
    Ok(())
}

/// 倒计时（复用 cli 中的同名逻辑，此处独立实现避免循环依赖）。
fn super_countdown(delay: u64, stop: &Arc<AtomicBool>) {
    if delay == 0 {
        return;
    }
    for sec in (1..=delay).rev() {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        eprint!("\r  {sec} 秒后开始... ");
        std::thread::sleep(Duration::from_secs(1));
    }
    eprintln!();
}

/// 探测远程文件的大小和 md5。
fn probe_file_info(
    args: &PullArgs,
    type_command: &mut impl FnMut(&str, u64, &AtomicBool),
    stop: &Arc<AtomicBool>,
) -> Result<(usize, String), String> {
    let path = &args.remote_path;
    let space = if args.char_space { "1" } else { "0" };
    let cmd = match args.shell {
        Shell::Bash => {
            let q = quote(path);
            format!("bash typepaste-pull.sh probe {q}{path}{q} {space}\n")
        }
        Shell::Powershell => {
            let q = quote_powershell(path);
            format!("powershell -File typepaste-pull.ps1 probe {q}{path}{q} {space}\n")
        }
    };

    type_command(&cmd, args.interval, stop);
    std::thread::sleep(Duration::from_millis(2400));
    for round in 1..=args.max_retry {
        std::thread::sleep(Duration::from_millis(retry_wait_ms(round)));
        let check = |lines: &[String]| parse_file_info(lines).is_some();
        if let Some(lines) = screenshot_ocr_with_check(args.region, check)? {
            if let Some(info) = parse_file_info(&lines) {
                return Ok(info);
            }
        }
        eprintln!("  [探测] 第 {round} 轮所有 OCR 组合均未识别到文件信息，重试...");
    }
    Err("探测文件信息失败（OCR 多次未识别）".to_string())
}

/// 在远程把文件编码为 base16 并按 chunk 切分到临时文件。
/// 每行格式：`<hex_content> <md5>`，md5 在准备阶段一次性算好，读取时直接取用。
///
/// 已将内联命令提取到 typepaste-pull.sh / typepaste-pull.ps1，通过 --deploy-script 部署。
/// 此处仅键入短命令调用脚本，减少键盘输入量。
fn prepare_remote_chunks(
    args: &PullArgs,
    type_command: &mut impl FnMut(&str, u64, &AtomicBool),
    stop: &Arc<AtomicBool>,
) -> Result<(), String> {
    let chunk_bytes = args.chunk_bytes;
    let path = &args.remote_path;
    let cmd = match args.shell {
        Shell::Bash => {
            let q = quote(path);
            format!("bash typepaste-pull.sh prepare {q}{path}{q} {chunk_bytes}\n")
        }
        Shell::Powershell => {
            let q = quote_powershell(path);
            format!("powershell -File typepaste-pull.ps1 prepare {q}{path}{q} {chunk_bytes}\n")
        }
    };
    type_command(&cmd, args.interval, stop);
    std::thread::sleep(Duration::from_secs_f64(1.0));
    Ok(())
}

/// 调试：对比 OCR 识别结果与 resources/tp_pull.txt 中对应分片，打印逐字符差异。
///
/// `resources/tp_pull.txt` 的内容格式与远程 `/tmp/tp_pull.b16` 一致
/// （每行：`<hex_content> <md5>`）。对比时仅取第一个字段（hex 内容）。
/// 文件不存在或行数不足时静默跳过，不影响正常流程。
/// `ocr_hex` 为 None 表示 OCR 未解析出有效 hex，仅打印期望内容。
fn debug_chunk_diff(i: usize, ocr_hex: Option<&str>) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join("tp_pull.txt");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    let expected = match content.lines().nth(i - 1) {
        Some(l) => l.split_whitespace().next().unwrap_or("").trim(),
        None => return,
    };

    eprintln!("  [调试] 片 {i} 期望内容（resources/tp_pull.txt 第 {i} 行）:");
    eprintln!("    期望({:>4}): {}", expected.len(), expected);

    let actual = match ocr_hex {
        Some(h) => h,
        None => return,
    };
    if actual == expected {
        eprintln!("    实际与期望一致");
        return;
    }
    eprintln!("    实际({:>4}): {}", actual.len(), actual);

    // 逐字符差异标记：相同位置为空格，不同为 ^
    let max_len = expected.len().max(actual.len());
    let mut diff = String::with_capacity(max_len);
    let mut diff_count = 0usize;
    let e_chars: Vec<char> = expected.chars().collect();
    let a_chars: Vec<char> = actual.chars().collect();
    for idx in 0..max_len {
        let e = e_chars.get(idx);
        let a = a_chars.get(idx);
        match (e, a) {
            (Some(e), Some(a)) if e == a => diff.push(' '),
            _ => {
                diff.push('^');
                diff_count += 1;
            }
        }
    }
    eprintln!("    差异({:>4}): {}", diff_count, diff);
}

/// 读取第 i 片，带重试。
fn read_chunk_with_retry(
    i: usize,
    args: &PullArgs,
    type_command: &mut impl FnMut(&str, u64, &AtomicBool),
    stop: &Arc<AtomicBool>,
) -> Result<String, String> {
    let space = if args.char_space { "1" } else { "0" };
    let lw = args.line_width;
    let cmd = match args.shell {
        Shell::Bash => {
            format!("bash typepaste-pull.sh show {i} {lw} {space}\n")
        }
        Shell::Powershell => {
            format!("powershell -File typepaste-pull.ps1 show {i} {lw} {space}\n")
        }
    };

    type_command(&cmd, args.interval, stop);
    std::thread::sleep(Duration::from_millis(1500));
    for round in 1..=args.max_retry {
        if stop.load(Ordering::Relaxed) {
            return Err("已停止".to_string());
        }
        std::thread::sleep(Duration::from_millis(retry_wait_ms(round)));
        let check = |lines: &[String]| {
            parse_hex_chunk(lines)
                .map(|(hex, md5)| md5_of_bytes(hex.as_bytes()) == md5)
                .unwrap_or(false)
        };
        match screenshot_ocr_with_check(args.region, check)? {
            Some(lines) => {
                let (hex, _) = parse_hex_chunk(&lines).unwrap();
                return Ok(hex);
            }
            None => {
                eprintln!("  [片 {i}] 第 {round} 轮所有 OCR 组合均失败，重试...");
                debug_chunk_diff(i, None);
            }
        }
    }
    Err(format!("片 {i} 读取失败（重试 {0} 次）", args.max_retry))
}

/// hex 字符串解码为字节。
fn hex_decode(hex: &str) -> Result<Vec<u8>, String> {
    hex::decode(hex).map_err(|e| format!("base16 解码失败：{e}"))
}

/// bash 引号包裹。
fn quote(s: &str) -> &str {
    // 简化：用单引号包裹，假设路径不含单引号
    if s.contains('\'') {
        "\""
    } else {
        "'"
    }
}

/// powershell 引号包裹。
fn quote_powershell(_s: &str) -> &str {
    "'"
}

/// 创建反向传输进度条。
fn make_pull_progress(total: u64) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template("  反向传输 [{bar:40.cyan/blue}] {pos}/{len} 片 ({percent}%)")
            .unwrap()
            .progress_chars("=>-"),
    );
    pb
}

/// dry-run：打印将要执行的命令序列。
fn dry_run_pull(args: &PullArgs) -> Result<(), String> {
    let space = if args.char_space { "1" } else { "0" };
    let script = match args.shell {
        Shell::Bash => "bash typepaste-pull.sh",
        Shell::Powershell => "powershell -File typepaste-pull.ps1",
    };
    let q = match args.shell {
        Shell::Bash => quote(&args.remote_path),
        Shell::Powershell => quote_powershell(&args.remote_path),
    };
    println!("[dry-run] 反向传输：{} → 本地", args.remote_path);
    println!("  [探测] {script} probe {q}{}{q} {space}", args.remote_path);
    println!(
        "  [准备] {script} prepare {q}{}{q} {} → /tmp/tp_pull.b16（每行: content md5）",
        args.remote_path, args.chunk_bytes
    );
    println!(
        "  [循环] {script} show <i> {} {space} → 截图 OCR → 校验",
        args.line_width
    );
    println!("  [每行宽度] {} 字符", args.line_width);
    println!(
        "  [字符间距] {}",
        if args.char_space {
            "开启（每字符前加空格）"
        } else {
            "关闭"
        }
    );
    println!("  [重组] hex decode → 写文件 → md5 校验");
    println!("  提示：脚本需先用 --deploy-script 部署到目标机。");
    Ok(())
}
