//! CLI：参数解析、动作分发（deploy / transfer）、数据管线与三模式还原。
//!
//! 数据管线：归档(目录强制 zip) → 压缩(gzip，仅当 < 原始×阈值) →
//! 编码(归档/压缩强制，否则跟 --encode) → typewrite → 目标端还原。
//!
//! 三种还原模式：cat（默认 heredoc + 手动说明）、auto（调用已部署脚本）、
//! 手动（`--restore-script` 自定义脚本名）。

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::{format, print, println};

use clap::Parser;

use crate::backend::Backend;
use crate::config::{
    non_neg_int, parse_size, CMD_SLEEP, DEFAULT_DELAY, DEFAULT_ENCODING, DEFAULT_INTERVAL,
    GZIP_USE_RATIO, MAX_FILE_SIZE, WRAP_EVERY,
};
use crate::encoder::Encoding;
use crate::failsafe::start_failsafe_monitor;
use crate::keymap::get_key_info;
use crate::restore_script::{
    decode_cmd, decode_cmd_for_shell, heredoc_footer, heredoc_header, parse_ops, Shell, Target,
};
use crate::utils::{
    gzip_compress, md5_of_bytes, sanitize_filename, type_command, type_text, zip_directory,
};

#[derive(Parser, Debug)]
#[command(
    name = "typepaste-rs",
    version,
    about = "通过模拟键盘输入把文件/目录「粘贴」到目标机（如云桌面）"
)]
struct Args {
    /// 要传输的文件/目录（--deploy-script 时可省略）。
    file: Option<PathBuf>,

    /// 排除匹配的文件/目录（glob 模式，如 node_modules、*.tmp、.git）。可多次指定。
    #[arg(long = "exclude")]
    exclude: Vec<String>,

    /// 部署还原脚本到目标机（独立动作）。
    #[arg(long)]
    deploy_script: bool,

    /// 目标机平台变体（与 --deploy-script 配合）。
    #[arg(long, value_enum)]
    target: Option<Target>,

    /// 编码方式。deploy 默认 base32；传输不可输入字符/归档/压缩时强制编码。
    #[arg(long, value_enum)]
    encode: Option<Encoding>,

    /// auto 模式调用脚本的 shell。
    #[arg(long, value_enum, default_value_t = Shell::Bash)]
    shell: Shell,

    /// 自定义还原脚本名（auto 模式）。
    #[arg(long)]
    restore_script: Option<String>,

    /// 倒计时（秒）。
    #[arg(long, value_parser = non_neg_int, default_value_t = DEFAULT_DELAY)]
    delay: u64,

    /// 每字符间隔（毫秒）。
    #[arg(long, value_parser = non_neg_int, default_value_t = DEFAULT_INTERVAL)]
    interval: u64,

    /// 预演：仅打印决策/内容，不输入。
    #[arg(long)]
    dry_run: bool,

    /// 分片大小（如 2m、500k、字节数）。指定时启用分片传输；未指定时超过 5MB 报错。
    #[arg(long, value_parser = parse_size)]
    part_size: Option<usize>,

    /// 跳过指定分片（逗号分隔+范围，1-based，如 1,3-5）。仅分片模式生效。
    #[arg(long)]
    skip_parts: Option<String>,

    /// 只传输指定分片（逗号分隔+范围，1-based，如 2,4）。仅分片模式生效。
    #[arg(long)]
    only_parts: Option<String>,
}

/// 分片模式产物。
struct PartsInfo {
    /// 每片字符数（= 字节数，编码后为纯 ASCII）。
    part_size: usize,
    /// 总片数。
    total: usize,
    /// 每片 md5（纯编码字符，不含换行）。
    part_md5s: Vec<String>,
    /// 切分后的编码字符串。
    encoded_parts: Vec<String>,
}

/// 数据管线产物。
struct Payload {
    /// 原始字节（文件字节 / 目录 zip 归档字节）。MD5 校验对象。
    raw: Vec<u8>,
    /// 编码后的 ASCII 文本（若编码）。
    encoded: Option<String>,
    /// 实际编码。
    encoding: Option<Encoding>,
    is_dir: bool,
    #[allow(dead_code)]
    is_compressed: bool,
    /// 原始数据 MD5。
    local_md5: String,
    /// 承载管线状态的 uid 文件名。
    uid_full: String,
    // raw_len: usize,
    // gz_len: usize,
    compress_str: String,
    /// 分片信息（None=单次模式，Some=分片模式）。
    parts: Option<PartsInfo>,
}

impl Payload {
    /// 编码内容字符数（原文直输时为 raw 的 UTF-8 字符数）。
    fn char_count(&self) -> usize {
        match &self.encoded {
            Some(s) => s.chars().count(),
            None => std::str::from_utf8(&self.raw)
                .map(|s| s.chars().count())
                .unwrap_or(self.raw.len()),
        }
    }
}

/// CLI 入口。
pub fn main() {
    let args = Args::parse();

    let stop = Arc::new(AtomicBool::new(false));
    // Ctrl+C 紧急停止（与 fail-safe 鼠标监控共同置位）。
    let _ = ctrlc::set_handler({
        let stop = stop.clone();
        move || stop.store(true, Ordering::Relaxed)
    });

    let result = if args.deploy_script {
        run_deploy(&args, &stop)
    } else {
        match &args.file {
            Some(f) => run_transfer(f, &args, &stop),
            None => {
                eprintln!("错误：缺少 <file>（或使用 --deploy-script 部署还原脚本）");
                std::process::exit(2);
            }
        }
    };

    if let Err(e) = result {
        eprintln!("错误：{e}");
        std::process::exit(1);
    }
    if stop.load(Ordering::Relaxed) {
        std::process::exit(130);
    }
}

/// 生成 uid 基础名：`typepaste_{ts}_{sanitized_name}`。
fn generate_uid_base(file: &Path) -> String {
    let ts = chrono::Local::now().format("%Y%m%d%H%M%S").to_string();
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    format!("typepaste_{ts}_{}", sanitize_filename(&name))
}

/// 生成不含时间戳的 uid 基础名（分片模式专用）：`typepaste_{sanitized_name}`。
/// 无时间戳便于多次/断点续传定位同一文件。
fn generate_chunked_uid_base(file: &Path) -> String {
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    format!("typepaste_{}", sanitize_filename(&name))
}

/// 将编码字符串按 part_size 字符数切分为分片（编码后为纯 ASCII，字符数 = 字节数）。
fn split_into_parts(encoded: &str, part_size: usize) -> Vec<String> {
    let part_size = part_size.max(1);
    let chars: Vec<char> = encoded.chars().collect();
    chars
        .chunks(part_size)
        .map(|c| c.iter().collect())
        .collect()
}

/// 解析分片选择字符串为 1-based 索引集合。
/// "1,3-5" → {1,3,4,5}；"" 或空 → Ok(None)（不筛选）。
/// 校验：索引 ≥ 1，范围起始 ≤ 结束。
fn parse_part_selection(spec: &str) -> Result<Option<HashSet<usize>>, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Ok(None);
    }
    let mut set = HashSet::new();
    for token in spec.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if let Some((a, b)) = token.split_once('-') {
            let a: usize = a
                .trim()
                .parse()
                .map_err(|_| format!("分片号非法：{token}"))?;
            let b: usize = b
                .trim()
                .parse()
                .map_err(|_| format!("分片号非法：{token}"))?;
            if a == 0 || b == 0 {
                return Err(format!("分片号必须 ≥ 1：{token}"));
            }
            if a > b {
                return Err(format!("范围起始 > 结束：{token}"));
            }
            (a..=b).for_each(|n| {
                set.insert(n);
            });
        } else {
            let n: usize = token.parse().map_err(|_| format!("分片号非法：{token}"))?;
            if n == 0 {
                return Err(format!("分片号必须 ≥ 1：{token}"));
            }
            set.insert(n);
        }
    }
    if set.is_empty() {
        Ok(None)
    } else {
        Ok(Some(set))
    }
}

/// 构建数据管线产物。
fn build_payload(
    file: &Path,
    encode_opt: Option<Encoding>,
    excludes: &[glob::Pattern],
    part_size: Option<usize>,
) -> Result<Payload, String> {
    let uid: String = if part_size.is_some() {
        generate_chunked_uid_base(file)
    } else {
        generate_uid_base(file)
    };
    let is_dir = file.is_dir();

    // 1. 归档：目录→zip；文件→原字节。
    let raw: Vec<u8> = if is_dir {
        zip_directory(file, &uid, excludes).map_err(|e| format!("归档失败：{e}"))?
    } else {
        let mut f = std::fs::File::open(file).map_err(|e| format!("无法打开 {file:?}：{e}"))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)
            .map_err(|e| format!("读取失败：{e}"))?;
        buf
    };

    if raw.is_empty() {
        return Err("源数据为空".to_string());
    }
    if raw.len() > MAX_FILE_SIZE {
        return Err(format!(
            "源数据 {} 字节超过上限 {} 字节",
            raw.len(),
            MAX_FILE_SIZE
        ));
    }

    // 2. 压缩：仅当 gzip 后 < 原始 × GZIP_USE_RATIO 才采用。
    let gz = gzip_compress(&raw).map_err(|e| format!("gzip 失败：{e}"))?;
    let gz_len = gz.len();
    let raw_len = raw.len();
    let ratio = gz_len as f64 / raw_len as f64;
    let is_compressed = ratio < GZIP_USE_RATIO;
    let payload_bytes: &[u8] = if is_compressed { &gz } else { &raw };
    let compress_str = if is_compressed {
        format!(
            "是(gzip) ({}/{} {:.1}% < {:.1}%)",
            gz_len,
            raw_len,
            ratio * 100.0,
            GZIP_USE_RATIO * 100.0
        )
    } else {
        format!(
            "否（压缩率未达阈值 {}/{} {:.1}% >= {:.1}%)",
            gz_len,
            raw_len,
            ratio * 100.0,
            GZIP_USE_RATIO * 100.0
        )
    };

    // 3. 是否包含不可输入字符
    let contains_invalid = raw.iter().any(|&b| get_key_info(b as char).is_none());

    // 4. 编码判定：包含不可输入字符/归档/压缩/分片→强制（默认 base32）；否则跟 --encode。
    let forced = contains_invalid || is_dir || is_compressed || part_size.is_some();
    let encoding = if forced {
        encode_opt.or_else(|| {
            println!(
                "警告：未指定编码，使用默认编码 {}",
                DEFAULT_ENCODING.encoder().name()
            );
            Some(DEFAULT_ENCODING)
        })
    } else {
        encode_opt
    };
    let encoded = encoding.map(|e| e.encoder().encode(payload_bytes));

    // 5. uid_full：base + [.zip] + [.gz] + [.suffix]
    let mut uid_full = uid;
    if is_dir {
        uid_full.push_str(".zip");
    }
    if is_compressed {
        uid_full.push_str(".gz");
    }
    if let Some(enc) = encoding {
        uid_full.push('.');
        uid_full.push_str(enc.encoder().suffix());
    }

    // 6. MD5（原始 raw：文件字节 / zip 归档字节）。
    let local_md5 = md5_of_bytes(&raw);

    // 7. 分片切分（仅 part_size 指定时；强制编码确保 encoded 非空）。
    let parts = match (&encoded, part_size) {
        (Some(enc_str), Some(ps)) => {
            let encoded_parts = split_into_parts(enc_str, ps);
            let part_md5s = encoded_parts
                .iter()
                .map(|p| md5_of_bytes(p.as_bytes()))
                .collect();
            Some(PartsInfo {
                part_size: ps,
                total: encoded_parts.len(),
                part_md5s,
                encoded_parts,
            })
        }
        _ => None,
    };

    Ok(Payload {
        raw,
        encoded,
        encoding,
        is_dir,
        is_compressed,
        local_md5,
        uid_full,
        // raw_len,
        // gz_len,
        compress_str,
        parts,
    })
}

/// 数据传输动作。
fn run_transfer(file: &Path, args: &Args, stop: &Arc<AtomicBool>) -> Result<(), String> {
    let excludes: Vec<glob::Pattern> = args
        .exclude
        .iter()
        .filter_map(|s| {
            glob::Pattern::new(s)
                .map_err(|e| eprintln!("--exclude 模式无效 {s:?}：{e}"))
                .ok()
        })
        .collect();
    let payload = build_payload(file, args.encode, &excludes, args.part_size)?;

    // 打印管线决策。
    println!("━━━ typepaste-rs 数据管线 ━━━");
    println!("  源      ：{file:?}");
    println!(
        "  归档    ：{}",
        if payload.is_dir { "是(zip)" } else { "否" }
    );
    println!("  压缩    ：{}", payload.compress_str);
    println!(
        "  编码    ：{}",
        payload
            .encoding
            .map(|e| e.encoder().name())
            .unwrap_or("无（原文直输）")
    );
    println!("  uid     ：{}", payload.uid_full);
    println!("  MD5     ：{}", payload.local_md5);
    if let Some(p) = &payload.parts {
        println!(
            "  分片    ：{} 片 × {} 字符（共 {} 字符）",
            p.total,
            p.part_size,
            p.encoded_parts.iter().map(|s| s.len()).sum::<usize>()
        );
    }
    println!(
        "  字符数  ：{}（预估 ~{:.1} 秒）",
        payload.char_count(),
        payload.char_count() as f64 * args.interval as f64 / 1000.0
    );
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    match (&payload.encoding, &payload.parts) {
        (_, Some(parts)) => run_chunked_transfer(&payload, parts, args, stop),
        (None, None) => run_raw_transfer(&payload, args, stop),
        (Some(_), None) => run_auto_mode(&payload, args, stop),
    }
}

/// 原文直输（无编码，仅 UTF-8 文本）。
fn run_raw_transfer(payload: &Payload, args: &Args, stop: &Arc<AtomicBool>) -> Result<(), String> {
    let text = match std::str::from_utf8(&payload.raw) {
        Ok(s) => s.to_string(),
        Err(_) => {
            return Err(
                "源文件非 UTF-8 文本，无法原文直输；请使用 --encode base32/base64/base16"
                    .to_string(),
            );
        }
    };

    if args.dry_run {
        println!("[dry-run] 原文直输");
    } else {
        count_down(args.delay, stop);
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
    }

    let interval = if args.dry_run { 0u64 } else { args.interval };
    let mut backend = prepare_input(stop)?;
    let mut send_char = |ch| {
        if args.dry_run {
            print!("{ch}");
        } else {
            backend.send_char(ch);
        }
    };

    let elapsed = type_text(&text, interval, &mut send_char, 0, stop, args.dry_run);

    if !args.dry_run {
        println!(
            "\n✅ 已输入 {} 字符，耗时 {:.1} 秒",
            text.chars().count(),
            elapsed
        );
        println!("MD5（原始）：{}", payload.local_md5);
    }
    std::mem::forget(backend); // 跳过 enigo Drop（其 Drop 中 thread::sleep 会累积阻塞）
    Ok(())
}

/// auto 模式：cat heredoc + 编码内容 + 调用已部署还原脚本。
fn run_auto_mode(payload: &Payload, args: &Args, stop: &Arc<AtomicBool>) -> Result<(), String> {
    let encoded = payload.encoded.as_ref().unwrap();
    let invoke = auto_invoke_command(args, &payload.uid_full, &payload.local_md5, None);

    if args.dry_run {
        println!("[dry-run] auto 模式");
        println!("  调用命令：{invoke}");
    } else {
        count_down(args.delay, stop);
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
    }

    let interval = if args.dry_run { 0u64 } else { args.interval };
    let mut backend = prepare_input(stop)?;
    let mut send_char = |ch| {
        if args.dry_run {
            print!("{ch}");
        } else {
            backend.send_char(ch);
        }
    };

    // heredoc 头（短，dry-run 时 print 输出）
    type_command(
        &heredoc_header(args.shell, &payload.uid_full),
        interval,
        &mut send_char,
        stop,
    );

    // 编码内容（dry-run 时 type_text 只打印摘要，不输出内容）
    type_text(
        encoded,
        interval,
        &mut send_char,
        WRAP_EVERY,
        stop,
        args.dry_run,
    );

    // heredoc 尾（短，dry-run 时 print 输出）
    type_command(
        &heredoc_footer(args.shell, &payload.uid_full),
        interval,
        &mut send_char,
        stop,
    );

    // 等待目标终端执行（秒）
    std::thread::sleep(Duration::from_secs_f64(CMD_SLEEP));

    // 调用命令（短，dry-run 时 print 输出）
    type_command(&format!("{invoke}\n"), interval, &mut send_char, stop);
    if !args.dry_run {
        println!("\n✅ 已写入 {} 并触发还原脚本", payload.uid_full);
    }
    std::mem::forget(backend); // 跳过 enigo Drop（其 Drop 中 thread::sleep 会累积阻塞）
    Ok(())
}

/// auto 模式调用还原脚本的命令。
///
/// 单次模式：`part_md5s` = None，命令为 `<script> <uid_full> <local_md5>`。
/// 分片模式：`part_md5s` = Some(逗号串)，命令为 `<script> <uid_full> <local_md5> <part_md5s>`，
/// 目标端据此对所有分片做批量 md5 校验后合并还原。
fn auto_invoke_command(args: &Args, uid_full: &str, md5: &str, part_md5s: Option<&str>) -> String {
    let part_suffix = part_md5s.map(|m| format!(" {m}")).unwrap_or_default();
    if let Some(name) = &args.restore_script {
        format!("{name} {uid_full} {md5}{part_suffix}")
    } else {
        match args.shell {
            Shell::Bash => format!("bash typepaste-restore.sh {uid_full} {md5}{part_suffix}"),
            Shell::Powershell => {
                format!("powershell -File typepaste-restore.ps1 {uid_full} {md5}{part_suffix}")
            }
        }
    }
}

/// 分片模式：逐片 heredoc 写入 + 校验，最后一片触发合并还原。
fn run_chunked_transfer(
    payload: &Payload,
    parts: &PartsInfo,
    args: &Args,
    stop: &Arc<AtomicBool>,
) -> Result<(), String> {
    // 解析分片选择（--only-parts 优先于 --skip-parts）。
    let selection: Option<HashSet<usize>> = if let Some(spec) = &args.only_parts {
        Some(parse_part_selection(spec)?.ok_or("--only-parts 不能为空".to_string())?)
    } else if let Some(spec) = &args.skip_parts {
        let skip = parse_part_selection(spec)?.ok_or("--skip-parts 不能为空".to_string())?;
        Some((1..=parts.total).filter(|i| !skip.contains(i)).collect())
    } else {
        None
    };

    let to_send: Vec<usize> = match &selection {
        Some(set) => (1..=parts.total).filter(|i| set.contains(i)).collect(),
        None => (1..=parts.total).collect(),
    };

    if args.dry_run {
        println!(
            "[dry-run] 分片模式：{} 片，待传 {} 片",
            parts.total,
            to_send.len()
        );
        for &idx in &to_send {
            let uid_part = format!("{}.p{idx}", payload.uid_full);
            println!("  p{idx}: {uid_part} (md5={})", parts.part_md5s[idx - 1]);
        }
        let all_md5s = parts.part_md5s.join(",");
        let invoke =
            auto_invoke_command(args, &payload.uid_full, &payload.local_md5, Some(&all_md5s));
        println!("  批量校验+合并调用：\n{invoke}");
        return Ok(());
    }

    count_down(args.delay, stop);
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }

    let interval = args.interval;
    let mut backend = prepare_input(stop)?;
    let mut send_char = |ch| backend.send_char(ch);

    for &idx in &to_send {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let uid_part = format!("{}.p{idx}", payload.uid_full);
        let part_content = &parts.encoded_parts[idx - 1];
        let part_md5 = &parts.part_md5s[idx - 1];

        println!("p{idx} >>> md5={part_md5}");

        // heredoc 头
        type_command(
            &heredoc_header(args.shell, &uid_part),
            interval,
            &mut send_char,
            stop,
        );
        // 分片编码内容（带进度条）
        type_text(
            part_content,
            interval,
            &mut send_char,
            WRAP_EVERY,
            stop,
            false,
        );
        // heredoc 尾
        type_command(
            &heredoc_footer(args.shell, &uid_part),
            interval,
            &mut send_char,
            stop,
        );
        // 等待目标终端落盘
        std::thread::sleep(Duration::from_secs_f64(CMD_SLEEP));
    }

    // 所有分片传完后，调用一次脚本：传入所有分片 md5，批量校验+合并+还原
    let all_md5s = parts.part_md5s.join(",");
    let invoke = auto_invoke_command(args, &payload.uid_full, &payload.local_md5, Some(&all_md5s));
    type_command(&format!("{invoke}\n"), interval, &mut send_char, stop);

    println!(
        "\n✅ 已传输 {} 片（共 {} 片），uid 基础：{}",
        to_send.len(),
        parts.total,
        payload.uid_full
    );
    println!("（已调用还原脚本批量校验所有分片并合并还原）");
    std::mem::forget(backend); // 跳过 enigo Drop（其 Drop 中 thread::sleep 会累积阻塞）
    Ok(())
}

/// 部署还原脚本动作。
fn run_deploy(args: &Args, stop: &Arc<AtomicBool>) -> Result<(), String> {
    // 无 --target：预览所有平台变体。
    let target = match args.target {
        Some(t) => t,
        None => {
            for t in Target::all() {
                println!("=== {} ===", t.label());
                println!("{}", t.script());
                println!();
            }
            println!("提示：使用 --target <variant> 部署到目标机。");
            return Ok(());
        }
    };

    // 编码判定：--encode 未给→默认
    let enc = args.encode.unwrap_or(DEFAULT_ENCODING);

    let script = target.script();
    let landing = target.landing_name();

    println!("━━━ 部署还原脚本 ━━━");
    println!("  目标平台：{}", target.label());
    println!("  落地文件：{landing}");
    println!("  交付编码：{}", enc.encoder().name());
    println!("━━━━━━━━━━━━━━━━━━━━");

    if args.dry_run {
        println!("[dry-run] 将 typewrite 的内容：");
    } else {
        count_down(args.delay, stop);
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
    }

    let interval = if args.dry_run { 0u64 } else { args.interval };
    let suffix = enc.encoder().suffix();
    let encoded_file = format!("{landing}.{suffix}");
    let cat_script = enc.encoder().encode(script.as_bytes());
    let cmd = decode_cmd_for_shell(args.shell, enc, &encoded_file, landing);

    let mut backend = prepare_input(stop)?;
    let mut send_char = |ch| {
        if args.dry_run {
            print!("{ch}");
        } else {
            backend.send_char(ch);
        }
    };

    // 输入 heredoc 头。
    type_command(
        &heredoc_header(args.shell, &encoded_file),
        interval,
        &mut send_char,
        stop,
    );

    // 输入编码后的脚本。
    type_text(
        &cat_script,
        interval,
        &mut send_char,
        WRAP_EVERY,
        stop,
        args.dry_run,
    );

    // 输入 heredoc 尾并执行解码命令。
    type_command(
        &format!("{}\n{cmd}\n", heredoc_footer(args.shell, &encoded_file)),
        interval,
        &mut send_char,
        stop,
    );

    if !args.dry_run {
        println!("\n✅ 已部署 {landing}（目标机）");
    }
    std::mem::forget(backend); // 跳过 enigo Drop（其 Drop 中 thread::sleep 会累积阻塞）
    Ok(())
}

/// 据 uid_full 后缀生成目标机手动还原步骤（反向：decode→gunzip→md5→unzip）。
#[allow(dead_code)]
fn manual_restore_steps(uid_full: &str, md5: &str) -> Vec<String> {
    let ops = parse_ops(uid_full);
    let mut steps = Vec::new();
    let mut cur = uid_full.to_string();

    if let Some(enc) = ops.decode {
        let suffix = format!(".{}", enc.encoder().suffix());
        let out = cur[..cur.len() - suffix.len()].to_string();
        steps.push(decode_cmd(enc, &cur, &out));
        cur = out;
    }
    if ops.gunzip {
        steps.push(format!("gunzip {cur}"));
        cur = cur[..cur.len() - 3].to_string(); // 去 .gz
    }
    steps.push(format!("# md5sum {cur}  应为 {md5}"));
    if ops.unzip {
        steps.push(format!("unzip {cur}"));
    }
    steps
}

/// 初始化 backend 并启动 fail-safe 监控。
fn prepare_input(stop: &Arc<AtomicBool>) -> Result<Backend, String> {
    let backend = Backend::new(stop.clone()).map_err(|e| format!("输入后端初始化失败：{e}"))?;
    start_failsafe_monitor(stop.clone());
    Ok(backend)
}

/// 倒计时（秒）。期间可被 stop 中断。
fn count_down(seconds: u64, stop: &Arc<AtomicBool>) {
    if seconds == 0 {
        return;
    }
    print!("{seconds} 秒后开始输入");
    for n in (1..=seconds).rev() {
        print!("\r{n} 秒后开始输入...  ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        if stop.load(Ordering::Relaxed) {
            println!();
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    println!("\r开始输入！              ");
}

#[cfg(test)]
mod tests {
    use std::vec;

    use super::*;

    #[test]
    fn uid_base_sanitizes() {
        let base = generate_uid_base(Path::new("hello world.txt"));
        assert!(base.starts_with("typepaste_"));
        assert!(base.contains("hello_world.txt"));
    }

    #[test]
    fn manual_steps_plain_b32() {
        let steps = manual_restore_steps("typepaste_x_file.b32", "abc");
        assert!(steps[0].contains("base32 -d"));
        assert!(steps[1].contains("# md5sum"));
        assert!(!steps
            .iter()
            .any(|s| s.contains("gunzip") || s.contains("unzip")));
    }

    #[test]
    fn manual_steps_zip_gz_b32() {
        let steps = manual_restore_steps("typepaste_x_dir.zip.gz.b32", "abc");
        assert!(steps[0].contains("base32 -d"));
        assert!(steps.iter().any(|s| s.contains("gunzip")));
        assert!(steps.iter().any(|s| s.contains("unzip")));
    }

    #[test]
    fn auto_invoke_bash_default() {
        let args = Args {
            file: None,
            exclude: vec![],
            deploy_script: false,
            target: None,
            encode: None,
            shell: Shell::Bash,
            restore_script: None,
            delay: 0,
            interval: 0,
            dry_run: false,
            part_size: None,
            skip_parts: None,
            only_parts: None,
        };
        let cmd = auto_invoke_command(&args, "uid.b32", "md5", None);
        assert_eq!(cmd, "bash typepaste-restore.sh uid.b32 md5");
    }

    #[test]
    fn auto_invoke_custom_script() {
        let args = Args {
            file: None,
            exclude: vec![],
            deploy_script: false,
            target: None,
            encode: None,
            shell: Shell::Bash,
            restore_script: Some("myrestore.sh".to_string()),
            delay: 0,
            interval: 0,
            dry_run: false,
            part_size: None,
            skip_parts: None,
            only_parts: None,
        };
        let cmd = auto_invoke_command(&args, "uid.b32", "md5", None);
        assert_eq!(cmd, "myrestore.sh uid.b32 md5");
    }

    #[test]
    fn auto_invoke_powershell() {
        let args = Args {
            file: None,
            exclude: vec![],
            deploy_script: false,
            target: None,
            encode: None,
            shell: Shell::Powershell,
            restore_script: None,
            delay: 0,
            interval: 0,
            dry_run: false,
            part_size: None,
            skip_parts: None,
            only_parts: None,
        };
        let cmd = auto_invoke_command(&args, "uid.b32", "md5", None);
        assert_eq!(cmd, "powershell -File typepaste-restore.ps1 uid.b32 md5");
    }

    #[test]
    fn build_payload_text_file_no_encode() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("resources")
            .join("test.txt");
        if !path.exists() {
            return;
        }
        let payload = build_payload(&path, None, &[], None).unwrap();
        // 文本文件：通常 gzip 达阈值则压缩+强制编码；否则按 --encode(None)→原文直输。
        assert_eq!(
            payload.local_md5,
            md5_of_bytes(&std::fs::read(&path).unwrap())
        );
        // uid 后缀一致性
        if payload.is_compressed {
            assert!(payload.uid_full.ends_with(".gz.b32"));
        } else {
            assert!(!payload.uid_full.contains(".gz"));
        }
    }

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_size("1m").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("2M").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_size("500k").unwrap(), 500 * 1024);
        assert_eq!(parse_size("500K").unwrap(), 500 * 1024);
        assert_eq!(parse_size("1048576").unwrap(), 1048576);
        assert!(parse_size("0").is_err());
        assert!(parse_size("0m").is_err());
        assert!(parse_size("").is_err());
        assert!(parse_size("3g").is_err());
    }

    #[test]
    fn parse_part_selection_ranges() {
        assert_eq!(
            parse_part_selection("1,3-5").unwrap().unwrap(),
            [1usize, 3, 4, 5].into_iter().collect()
        );
        assert_eq!(
            parse_part_selection("2,4").unwrap().unwrap(),
            [2usize, 4].into_iter().collect()
        );
        assert_eq!(parse_part_selection("").unwrap(), None);
        assert_eq!(parse_part_selection("   ").unwrap(), None);
        assert!(parse_part_selection("0").is_err());
        assert!(parse_part_selection("a").is_err());
        assert!(parse_part_selection("5-3").is_err());
    }

    #[test]
    fn split_into_parts_boundaries() {
        // 整除：6 字符 / 3 → 2 片
        let parts = split_into_parts("abcdef", 3);
        assert_eq!(parts, vec!["abc", "def"]);
        // 余 1：7 字符 / 3 → 3 片（末片 1 字符）
        let parts = split_into_parts("abcdefg", 3);
        assert_eq!(parts, vec!["abc", "def", "g"]);
        // 单片：内容短于 part_size
        let parts = split_into_parts("ab", 5);
        assert_eq!(parts, vec!["ab"]);
    }

    #[test]
    fn chunked_uid_no_timestamp() {
        let base = generate_chunked_uid_base(Path::new("hello world.txt"));
        assert_eq!(base, "typepaste_hello_world.txt");
    }

    #[test]
    fn auto_invoke_chunked_last_part() {
        let args = Args {
            file: None,
            exclude: vec![],
            deploy_script: false,
            target: None,
            encode: None,
            shell: Shell::Bash,
            restore_script: None,
            delay: 0,
            interval: 0,
            dry_run: false,
            part_size: None,
            skip_parts: None,
            only_parts: None,
        };
        // 单次模式：不传 part_md5s
        let cmd = auto_invoke_command(&args, "uid.b32", "localmd5", None);
        assert_eq!(cmd, "bash typepaste-restore.sh uid.b32 localmd5");
        // 分片模式：传所有分片 md5 逗号串（uid 为 base，不带 .pN）
        let cmd = auto_invoke_command(&args, "uid.b32", "localmd5", Some("md5a,md5b,md5c"));
        assert_eq!(
            cmd,
            "bash typepaste-restore.sh uid.b32 localmd5 md5a,md5b,md5c"
        );
    }

    #[test]
    fn build_payload_chunked_forces_encoding_and_no_ts() {
        // 小文本文件，指定 --part-size 应强制编码 + uid 无时间戳 + parts.is_some()
        let tmp = std::env::temp_dir().join("typepaste_chunk_test.txt");
        std::fs::write(&tmp, b"hello typepaste chunked transfer test").unwrap();
        let payload = build_payload(&tmp, None, &[], Some(100)).unwrap();
        let _ = std::fs::remove_file(&tmp);
        // 强制编码
        assert!(payload.encoding.is_some());
        // uid 无时间戳（不含年份）
        assert!(!payload.uid_full.contains("2025"));
        assert!(!payload.uid_full.contains("2026"));
        assert!(payload
            .uid_full
            .starts_with("typepaste_typepaste_chunk_test"));
        assert!(payload.uid_full.ends_with(".b32"));
        // 分片信息
        let parts = payload.parts.expect("应有分片信息");
        assert!(parts.total >= 1);
        assert_eq!(parts.part_size, 100);
        assert_eq!(parts.part_md5s.len(), parts.total);
        assert_eq!(parts.encoded_parts.len(), parts.total);
        // 拼接所有分片应等于完整编码
        let joined: String = parts.encoded_parts.concat();
        assert_eq!(joined, payload.encoded.unwrap());
    }
}
