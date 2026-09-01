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

use crate::ocr::{parse_file_info, parse_hex_chunk, screenshot_ocr_lines};
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
}

/// 反向传输主入口。
pub fn run_pull(
    args: &PullArgs,
    stop: &Arc<AtomicBool>,
    mut type_command: impl FnMut(&str, u64, &AtomicBool),
) -> Result<(), String> {
    if args.dry_run {
        return dry_run_pull(args);
    }

    println!("━━━ 反向传输（远程 → 本机）━━━");
    println!("  远程文件：{}", args.remote_path);
    println!("  分片大小：{} 字节", args.chunk_bytes);
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
    let cmd = match args.shell {
        Shell::Bash => {
            let q = quote(path);
            format!(
                "clear; stat -c%s {q}{path}{q} 2>/dev/null; md5sum {q}{path}{q} 2>/dev/null | cut -d' ' -f1\n"
            )
        }
        Shell::Powershell => {
            let q = quote_powershell(path);
            format!(
                "Clear-Host; (Get-Item {q}{path}{q}).Length; (Get-FileHash {q}{path}{q} -Algorithm MD5).Hash.ToLower()\n"
            )
        }
    };

    for attempt in 1..=args.max_retry {
        type_command(&cmd, args.interval, stop);
        std::thread::sleep(Duration::from_secs_f64(1.0));
        let lines = screenshot_ocr_lines()?;
        if let Some(info) = parse_file_info(&lines) {
            return Ok(info);
        }
        // let shot = std::env::temp_dir().join(format!("tp_pull_{}.png", std::process::id()));
        eprintln!("  [探测] 第 {attempt} 次 OCR 未识别到文件信息，重试...");
        eprintln!(
            "    OCR 最后 6 行：{:?}",
            lines.iter().rev().take(6).collect::<Vec<_>>()
        );
        // eprintln!("    截图已保存：{}", shot.display());
    }
    Err("探测文件信息失败（OCR 多次未识别）".to_string())
}

/// 在远程把文件编码为 base16 并按 chunk 切分到临时文件。
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
            let hex_width = chunk_bytes * 2;
            format!(
                "if command -v xxd >/dev/null 2>&1; then xxd -p -c {hex_width} {q}{path}{q} > /tmp/tp_pull.b16; else python3 -c \"import sys;d=open(sys.argv[1],'rb').read();[print(d[i:i+{chunk_bytes}].hex()) for i in range(0,len(d),{chunk_bytes})]\" {q}{path}{q} > /tmp/tp_pull.b16; fi\n"
            )
        }
        Shell::Powershell => {
            let q = quote_powershell(path);
            format!(
                "$b=[System.IO.File]::ReadAllBytes({q}{path}{q}); $sb=New-Object System.Text.StringBuilder; for($i=0;$i -lt $b.Length;$i+={chunk_bytes}){{ $null=$sb.AppendLine((-join ($b[$i..([Math]::Min($i+{chunk_bytes}-1,$b.Length-1))] | ForEach-Object {{ $_.ToString('x2') }})) }}; [System.IO.File]::WriteAllText('/tmp/tp_pull.b16',$sb.ToString())\n"
            )
        }
    };
    type_command(&cmd, args.interval, stop);
    // 编码可能需要时间，等待
    std::thread::sleep(Duration::from_secs_f64(1.0));
    Ok(())
}

/// 读取第 i 片，带重试。
fn read_chunk_with_retry(
    i: usize,
    args: &PullArgs,
    type_command: &mut impl FnMut(&str, u64, &AtomicBool),
    stop: &Arc<AtomicBool>,
) -> Result<String, String> {
    let cmd = match args.shell {
        Shell::Bash => format!(
            "clear; sleep .5; sed -n '{i}p' /tmp/tp_pull.b16; sed -n '{i}p' /tmp/tp_pull.b16 | tr -d '\\n' | md5sum | cut -d' ' -f1\n",
        ),
        Shell::Powershell => {
            let idx = i - 1;
            format!(
                "Clear-Host; $l=(Get-Content /tmp/tp_pull.b16)[{idx}]; $l; ($l -replace '\\r|\\n','') | ForEach-Object {{ (Get-FileHash -InputStream ([System.IO.MemoryStream]::new([System.Text.Encoding]::UTF8.GetBytes($_))) -Algorithm MD5).Hash.ToLower() }}\n"
            )
        }
    };

    for attempt in 1..=args.max_retry {
        if stop.load(Ordering::Relaxed) {
            return Err("已停止".to_string());
        }
        type_command(&cmd, args.interval, stop);
        std::thread::sleep(Duration::from_secs_f64(2.0));
        let lines = screenshot_ocr_lines()?;
        if let Some((hex, md5)) = parse_hex_chunk(&lines) {
            let actual = md5_of_bytes(hex.as_bytes());
            if actual == md5 {
                return Ok(hex);
            }
            eprintln!("  [片 {i}] md5 不匹配（第 {attempt} 次），重试...");
            eprintln!("    got={actual} want={md5}");
        } else {
            eprintln!("  [片 {i}] OCR 未识别（第 {attempt} 次），重试...");
            eprintln!(
                "    OCR 最后 4 行：{:?}",
                lines.iter().rev().take(4).collect::<Vec<_>>()
            );
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
    println!("[dry-run] 反向传输：{} → 本地", args.remote_path);
    println!("  [探测] stat + md5sum");
    println!(
        "  [准备] xxd -p -c {} {} > /tmp/tp_pull.b16",
        args.chunk_bytes * 2,
        args.remote_path
    );
    println!("  [循环] 逐片 clear; sed + md5sum → 截图 OCR → 校验");
    println!("  [重组] hex decode → 写文件 → md5 校验");
    Ok(())
}
