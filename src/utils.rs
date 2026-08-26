//! 工具：MD5、zip 归档、gzip 压缩、进度条、逐字符输入。

use std::format;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::{Instant, SystemTime};

use glob::Pattern;
use indicatif::{ProgressBar, ProgressState, ProgressStyle};
use md5::{Digest, Md5};

use crate::config::DRY_RUN_PREVIEW;

/// 字节数组的 MD5（十六进制小写）。
pub fn md5_of_bytes(data: &[u8]) -> String {
    format!("{:x}", Md5::digest(data))
}

/// 清理文件名：保留 `a-zA-Z0-9._-`，其余替换为 `_`；全 `_` 回退 `file`；截断 50。
pub fn sanitize_filename(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.trim_matches('_').is_empty() {
        s = "file".to_string();
    }
    if s.len() > 50 {
        s.truncate(50);
    }
    s
}

/// 文件的 MD5（8192 字节分块流式）。
#[allow(dead_code)]
pub fn md5_of_file(path: &Path) -> std::io::Result<String> {
    let mut h = Md5::new();
    let mut f = std::fs::File::open(path)?;
    let mut buf = [0u8; 8192];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}

/// 将 `SystemTime` 转换为 zip `DateTime`（DOS 时间，秒精度，年份范围 1980..=2107）。
/// 超出范围时回退到 1980-01-01 00:00:00。
fn system_time_to_zip_datetime(t: SystemTime) -> zip::DateTime {
    use chrono::{DateTime, Datelike, Timelike, Utc};
    let dt: DateTime<Utc> = t.into();
    let year = (dt.year() as i64).clamp(1980, 2107) as u16;
    let month = dt.month() as u8;
    let day = dt.day() as u8;
    let hour = dt.hour() as u8;
    let minute = dt.minute() as u8;
    let second = (dt.second() as u8).min(59);
    zip::DateTime::from_date_and_time(year, month, day, hour, minute, second).unwrap_or_default()
}

/// 将目录归档为 zip 字节流（保留顶层目录名和相对路径结构）。
///
/// 为保证归档可重现（同一目录两次归档 md5 一致）：
/// - 按文件名排序遍历，消除文件系统目录项顺序差异；
/// - 每个条目使用文件实际 mtime 写入 zip 时间戳，避免默认「当前时间」导致字节流变化。
pub fn zip_directory(
    dir_path: &Path,
    dir_name: &str,
    excludes: &[Pattern],
) -> std::io::Result<Vec<u8>> {
    let buf: std::io::Cursor<Vec<u8>> = std::io::Cursor::new(Vec::new());
    let mut zw = zip::ZipWriter::new(buf);

    let mut it = walkdir::WalkDir::new(dir_path)
        .sort_by_file_name()
        .into_iter();
    while let Some(entry) = it.next() {
        let entry = entry?;
        let full = entry.path();
        let rel = full.strip_prefix(dir_path).unwrap_or(full);

        // 排除匹配的文件名/目录名
        let basename = entry.file_name().to_string_lossy();
        if excludes.iter().any(|p| p.matches(&basename)) {
            // 目录跳过整棵子树
            if entry.file_type().is_dir() {
                it.skip_current_dir();
            }
            eprintln!("排除：{:?}", full);
            continue;
        }

        let arcname = if rel.as_os_str().is_empty() {
            dir_name.to_string()
        } else {
            format!("{}/{}", dir_name, rel.to_string_lossy())
        };

        // 使用文件实际 mtime，保证归档可重现
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .last_modified_time(system_time_to_zip_datetime(mtime));

        if entry.file_type().is_dir() {
            zw.add_directory(arcname, options).map_err(io_err)?;
        } else {
            zw.start_file(arcname, options).map_err(io_err)?;
            let mut f = std::fs::File::open(full)?;
            let mut data = Vec::new();
            f.read_to_end(&mut data)?;
            zw.write_all(&data)?;
        }
    }
    let buf = zw.finish().map_err(io_err)?;
    Ok(buf.into_inner())
}

fn io_err(e: zip::result::ZipError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
}

/// gzip 压缩字节。
pub fn gzip_compress(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data)?;
    encoder.finish()
}

/// 纯文本进度条（测试用）。
#[allow(dead_code)]
pub fn progress_bar(done: usize, total: usize, bar_len: usize) -> String {
    let ratio = if total == 0 {
        1.0
    } else {
        (done as f64 / total as f64).clamp(0.0, 1.0)
    };
    let filled = (bar_len as f64 * ratio) as usize;
    let bar: String = "█".repeat(filled) + &"░".repeat(bar_len - filled);
    format!("[{}] {:5.1}% ({}/{})", bar, ratio * 100.0, done, total)
}

/// 创建 indicatif 进度条。
fn make_progress_bar(total: u64) -> ProgressBar {
    const PROGRESS_TEMPLATE: &str =
        "{elapsed_precise}({eta}s) [{bar:30.cyan/blue}] {percent:>5}% ({pos}/{len}) {per_sec:>5}字符/秒";

    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template(PROGRESS_TEMPLATE)
            .unwrap()
            .with_key(
                "eta",
                |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                    write!(w, "{:.1}", state.eta().as_secs_f64()).unwrap()
                },
            )
            .with_key(
                "percent",
                |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                    write!(w, "{:.1}", state.fraction() * 100f32).unwrap()
                },
            )
            .with_key(
                "per_sec",
                |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                    write!(w, "{:.1}", state.per_sec()).unwrap()
                },
            )
            .progress_chars("█░"),
    );
    pb.enable_steady_tick(std::time::Duration::from_millis(500));
    pb
}

/// 逐字符模拟键盘输入，带 indicatif 进度条（时间节流）。
///
/// `interval` 单位为毫秒。`wrap_every > 0` 时每该数量个字符插入换行
/// （不计入进度，最后一个不插）。返回耗时（秒）。失败时打印错误并退出。
///
/// `dry_run=true` 时不创建进度条、不调用 `send_char`，打印内容前缀
/// （超过 `DRY_RUN_PREVIEW` 字符则截断），避免控制台刷屏。返回 0.0。
pub fn type_text<F: FnMut(char)>(
    text: &str,
    interval: u64,
    send_char: &mut F,
    wrap_every: usize,
    stop: &AtomicBool,
    dry_run: bool,
) -> f64 {
    let _ = stop; // stop 检查已移至 Backend::send_char，参数保留以兼容签名
    let total = text.chars().count();

    // dry-run：不创建进度条，不调用 send_char，打印前缀内容（超过阈值截断）。
    // 保留 wrap_every 换行逻辑（与正常输入一致，便于预览编码内容布局）。
    if dry_run {
        let chars: Vec<char> = text.chars().collect();
        let total = chars.len();
        let preview_count = total.min(DRY_RUN_PREVIEW);
        let mut i = 0usize;
        for ch in chars.iter().copied().take(preview_count) {
            send_char(ch);
            i += 1;
            if wrap_every > 0 && i % wrap_every == 0 && i < preview_count {
                send_char('\n');
            }
        }
        if total > DRY_RUN_PREVIEW {
            let text = format!(
                "\n…（已截断，共 {} 字符，剩余 {} 字符）",
                total,
                total - DRY_RUN_PREVIEW
            );
            text.chars().for_each(send_char);
        }
        return 0.0;
    }

    let start = Instant::now();
    let pb = make_progress_bar(total as u64);

    let mut i = 0usize;
    for ch in text.chars() {
        if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| send_char(ch))) {
            pb.abandon();
            eprintln!(
                "\n❌ 输入失败（第 {}/{} 字符 '{}'）：{:?}",
                i + 1,
                total,
                ch,
                e
            );
            eprintln!("   可能原因：辅助功能权限被撤销、目标窗口失焦、后端异常");
            std::process::exit(1);
        }
        i += 1;
        if interval > 0 {
            std::thread::sleep(std::time::Duration::from_millis(interval));
        }
        pb.inc(1);
        if wrap_every > 0 && i % wrap_every == 0 && i < total {
            send_char('\n');
            if interval > 0 {
                std::thread::sleep(std::time::Duration::from_millis(interval));
            }
        }
    }
    pb.finish();
    start.elapsed().as_secs_f64()
}

/// 逐字输入命令字符串（无进度条），用于 cat 头/EOF/调用命令。
/// `interval` 单位为毫秒。stop 检查由 backend.send_char 负责。
pub fn type_command<F: FnMut(char)>(
    cmd: &str,
    interval: u64,
    send_char: &mut F,
    stop: &AtomicBool,
) {
    let _ = stop; // stop 检查已移至 Backend::send_char，参数保留以兼容签名
    for ch in cmd.chars() {
        send_char(ch);
        if interval > 0 {
            std::thread::sleep(std::time::Duration::from_millis(interval));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::vec;

    use super::*;

    #[test]
    fn md5_known_value() {
        assert_eq!(md5_of_bytes(b"hello"), "5d41402abc4b2a76b9719d911017c592");
    }

    #[test]
    fn md5_file_matches_bytes() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("resources")
            .join("test.jpg");
        if !path.exists() {
            return;
        }
        let data = std::fs::read(&path).unwrap();
        assert_eq!(md5_of_file(&path).unwrap(), md5_of_bytes(&data));
    }

    #[test]
    fn progress_bar_boundaries() {
        assert!(
            progress_bar(0, 100, 30).contains("0.0%")
                && progress_bar(0, 100, 30).contains("(0/100)")
        );
        assert!(progress_bar(50, 100, 30).contains("50.0%"));
        assert!(
            progress_bar(100, 100, 30).contains("100.0%")
                && progress_bar(100, 100, 30).contains("(100/100)")
        );
    }

    #[test]
    fn progress_bar_clamps_overflow() {
        assert!(progress_bar(150, 100, 30).contains("100.0%"));
    }

    #[test]
    fn progress_bar_zero_total() {
        assert!(progress_bar(0, 0, 30).contains("100.0%"));
    }

    #[test]
    fn progress_bar_format_tokens() {
        let s = progress_bar(3, 10, 30);
        for t in ["[", "]", "%", "(", "/", "3/10"] {
            assert!(s.contains(t), "missing {t} in {s}");
        }
    }

    // 上面的闭包写法不易取回，改用单独测试用例直接断言。
    #[test]
    fn type_text_records_all_chars() {
        let text = "Hello World ABC 123";
        let mut recorded = Vec::new();
        let stop = AtomicBool::new(false);
        let elapsed = type_text(text, 0, &mut |ch| recorded.push(ch), 0, &stop, false);
        assert_eq!(recorded, text.chars().collect::<Vec<_>>());
        assert!(elapsed >= 0.0);
    }

    #[test]
    fn type_text_count_matches_len() {
        let mut recorded = Vec::new();
        let stop = AtomicBool::new(false);
        type_text("abcdef", 0, &mut |ch| recorded.push(ch), 0, &stop, false);
        assert_eq!(recorded.len(), 6);
    }

    #[test]
    fn type_text_wrap_inserts_newline() {
        let mut recorded = Vec::new();
        let stop = AtomicBool::new(false);
        type_text(
            "abcdefghij",
            0,
            &mut |ch| recorded.push(ch),
            3,
            &stop,
            false,
        );
        // 每 3 字插换行：abc\ndef\nghi (最后不插) -> 'j' 后无换行
        assert_eq!(recorded.iter().filter(|&&c| c == '\n').count(), 3);
        assert_eq!(recorded.iter().filter(|&&c| c != '\n').count(), 10);
    }

    #[test]
    fn gzip_roundtrip() {
        let data = b"hello world hello world hello world";
        let compressed = gzip_compress(data).unwrap();
        // 解压验证
        let mut decoder = flate2::read::GzDecoder::new(&compressed[..]);
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn sanitize_replaces_unsafe() {
        assert_eq!(sanitize_filename("hello world.txt"), "hello_world.txt");
    }

    #[test]
    fn sanitize_fallback_file() {
        assert_eq!(sanitize_filename("中文"), "file");
        assert_eq!(sanitize_filename("   "), "file");
    }

    #[test]
    fn sanitize_truncates() {
        let long = "a".repeat(80);
        let s = sanitize_filename(&long);
        assert_eq!(s.len(), 50);
    }

    #[test]
    fn sanitize_keeps_safe_chars() {
        assert_eq!(sanitize_filename("a1.2-3_4"), "a1.2-3_4");
    }

    #[test]
    fn zip_directory_some_dir_multi_times_has_same_md5() {
        use std::fs;
        let tmp = std::env::temp_dir().join("typepaste_zip_test");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("test.txt"), b"test").unwrap();

        let excludes: Vec<Pattern> = vec![];
        let data1 = zip_directory(&tmp, "root", &excludes).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let data2 = zip_directory(&tmp, "root", &excludes).unwrap();
        let _ = fs::remove_dir_all(&tmp);

        let md5_1 = md5_of_bytes(&data1);
        let md5_2 = md5_of_bytes(&data2);
        assert_eq!(md5_1, md5_2);
    }

    #[test]
    fn zip_directory_excludes_matching_files() {
        use std::fs;
        let tmp = std::env::temp_dir().join("typepaste_zip_exclude_test");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("sub")).unwrap();
        fs::write(tmp.join("keep.txt"), b"keep").unwrap();
        fs::write(tmp.join("skip.tmp"), b"skip").unwrap();
        fs::write(tmp.join("sub/keep.txt"), b"keep2").unwrap();
        fs::write(tmp.join("sub/skip.tmp"), b"skip2").unwrap();

        let excludes = vec![glob::Pattern::new("*.tmp").unwrap()];
        let zip_bytes = zip_directory(&tmp, "root", &excludes).unwrap();
        let _ = fs::remove_dir_all(&tmp);

        // 解压验证 .tmp 文件被排除
        let mut reader = std::io::Cursor::new(&zip_bytes);
        let mut archive = zip::ZipArchive::new(&mut reader).unwrap();
        let mut names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();
        assert!(names.iter().any(|n| n.contains("keep.txt")));
        assert!(!names.iter().any(|n| n.contains("skip.tmp")));
    }

    #[test]
    fn zip_directory_excludes_directory_subtree() {
        use std::fs;
        let tmp = std::env::temp_dir().join("typepaste_zip_exclude_dir_test");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("node_modules")).unwrap();
        fs::create_dir_all(tmp.join("src")).unwrap();
        fs::write(tmp.join("node_modules/pkg.json"), b"{}").unwrap();
        fs::write(tmp.join("src/main.rs"), b"fn main(){}").unwrap();

        let excludes = vec![glob::Pattern::new("node_modules").unwrap()];
        let zip_bytes = zip_directory(&tmp, "root", &excludes).unwrap();
        let _ = fs::remove_dir_all(&tmp);

        let mut reader = std::io::Cursor::new(&zip_bytes);
        let mut archive = zip::ZipArchive::new(&mut reader).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.iter().any(|n| n.contains("main.rs")));
        assert!(!names.iter().any(|n| n.contains("node_modules")));
        assert!(!names.iter().any(|n| n.contains("pkg.json")));
    }
}
