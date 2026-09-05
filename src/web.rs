//! VDI 网页接收端全自动传输（本机 → 浏览器）。
//!
//! 文件 → 按原始大小分片 → 每片独立 zstd 压缩（压缩无收益则跳过）→
//! base64 → 协议帧（STX/ETX 定界）→ 物理键盘投递；网页端 md5 校验后通过
//! 二维码返回状态，本机截屏解码驱动 ARQ 状态机（OK 下一片 / RETRY 重传 /
//! PAUSE 等待聚焦 / FINISH 结束）。
//!
//! 帧格式（`|` 分隔）：
//! `seq|total|rawSize|zstdFlag|md5Chunk|<base64 payload>[|<base64 filename>]`
//! - seq 0-based；zstdFlag=1 时 payload 为该原始分片的独立 zstd 流
//! - md5Chunk = payload（base64 解码后）的 md5
//! - 文件名仅 seq=0 携带（第 7 字段）

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use indicatif::{ProgressBar, ProgressStyle};

use crate::backend::Backend;
use crate::ocr::{select_region, FocusMonitor, Region};
use crate::qr::{self, Feedback, Status};
use crate::utils::{md5_of_bytes, set_global_pb, type_text_abortable};
use crate::{debug, error, info, warn};

/// 内嵌网页接收端（单文件、离线、零外部依赖）。
pub const RECEIVER_HTML: &str = include_str!("../assets/receiver.html");

/// 默认每片原始字节数。
pub const DEFAULT_WEB_CHUNK: usize = 1024;
/// zstd 压缩级别（平衡）。
const ZSTD_LEVEL: i32 = 3;
/// 压缩后总大小超过原始大小的此比例（95%）则放弃压缩。
const COMPRESS_USE_RATIO: f64 = 0.95;
/// 二维码轮询间隔。
const QR_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// 帧发送中途的二维码状态检查间隔（远端失焦/错误的兜底检测）。
const QR_MIDCHECK_INTERVAL: Duration = Duration::from_millis(700);
/// 发送后等待 ACK 的超时。
const ACK_TIMEOUT: Duration = Duration::from_secs(30);
/// 初始等待 READY 的超时。
const READY_TIMEOUT: Duration = Duration::from_secs(180);
/// PAUSE 状态等待重新聚焦的超时。
const PAUSE_TIMEOUT: Duration = Duration::from_secs(600);

/// 网页传输模式参数。
pub struct WebArgs {
    /// 待传输文件。
    pub file: PathBuf,
    /// 每片原始字节数。
    pub chunk_bytes: usize,
    /// 每字符输入间隔（毫秒）。
    pub interval: u64,
    /// 预演模式。
    pub dry_run: bool,
    /// 是否交互式框选二维码区域。
    pub select_region: bool,
    /// 二维码反馈区域（points）。
    pub region: Option<Region>,
    /// 可打印帧定界（`{`/`}` 替代 Ctrl+B/C），防 VDI 劫持 Ctrl 组合键。
    pub printable_frame: bool,
    /// 单片最大重试次数。
    pub max_retry: usize,
}

/// 一片预编码帧。
struct Frame {
    /// 帧体（STX 与 ETX 之间的文本）。
    body: String,
    /// 负载（base64 解码后）的 md5，用于逐片进度显示。
    md5: String,
}

/// 预处理结果。
struct Prepared {
    frames: Vec<Frame>,
    /// 原始文件总字节数。
    raw_size: usize,
    /// 原始文件 md5（FINISH 时整体校验）。
    file_md5: String,
}

/// 读取文件 → 分片 → 每片独立 zstd 压缩（或跳过）→ 组装协议帧。
fn prepare(file: &Path, chunk_bytes: usize) -> Result<Prepared, String> {
    let raw = fs::read(file).map_err(|e| format!("读取文件失败：{e}"))?;
    let raw_size = raw.len();
    let file_md5 = md5_of_bytes(&raw);
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "received.bin".to_string());
    let name_b64 = base64::engine::general_purpose::STANDARD.encode(name.as_bytes());

    let total = raw_size.div_ceil(chunk_bytes).max(1);

    // 先按每片独立压缩试算，整体无收益则全部走原始字节。
    let mut compressed_chunks: Vec<Vec<u8>> = Vec::with_capacity(total);
    let mut compressed_total = 0usize;
    for seq in 0..total {
        let start = seq * chunk_bytes;
        let end = (start + chunk_bytes).min(raw_size);
        let c = zstd::stream::encode_all(&raw[start..end], ZSTD_LEVEL)
            .map_err(|e| format!("zstd 压缩失败：{e}"))?;
        compressed_total += c.len();
        compressed_chunks.push(c);
    }
    let use_compress = (compressed_total as f64) < (raw_size as f64) * COMPRESS_USE_RATIO;
    let zflag = if use_compress { 1u8 } else { 0u8 };
    debug!(
        "压缩策略：{}（压缩后 {compressed_total}B / 原始 {raw_size}B）",
        if use_compress {
            "zstd 启用"
        } else {
            "高熵跳过压缩"
        }
    );

    let mut frames = Vec::with_capacity(total);
    for (seq, compressed) in compressed_chunks.iter().enumerate() {
        let start = seq * chunk_bytes;
        let end = (start + chunk_bytes).min(raw_size);
        let payload: &[u8] = if use_compress {
            compressed
        } else {
            &raw[start..end]
        };
        let md5 = md5_of_bytes(payload);
        let b64 = base64::engine::general_purpose::STANDARD.encode(payload);
        let mut body = format!("{seq}|{total}|{raw_size}|{zflag}|{md5}|{b64}");
        if seq == 0 {
            body.push('|');
            body.push_str(&name_b64);
        }
        frames.push(Frame { body, md5 });
    }

    Ok(Prepared {
        frames,
        raw_size,
        file_md5,
    })
}

/// 网页传输主入口。`backend` 为 None 时为 dry-run。
pub fn run_web(
    args: &mut WebArgs,
    stop: &Arc<AtomicBool>,
    backend: Option<&mut Backend>,
) -> Result<(), String> {
    if args.dry_run {
        return dry_run_web(args);
    }
    let backend = backend.ok_or("内部错误：dry-run 外必须提供 backend")?;

    // 交互式框选二维码区域
    if args.select_region && args.region.is_none() {
        info!("请在屏幕上框选网页右上角二维码反馈区域(Esc 取消)...");
        args.region = Some(select_region("拖拽框选二维码区域, Esc 取消")?);
    }

    let prep = prepare(&args.file, args.chunk_bytes)?;
    let total = prep.frames.len();
    info!(
        "文件 {}：{} 字节，{} 片（每片 {}B）",
        args.file.display(),
        prep.raw_size,
        total,
        args.chunk_bytes
    );

    // 等待接收端就绪
    info!("等待接收端就绪（请在 VDI 打开 receiver.html、授权目录并点击浏览器窗口）...");
    wait_ready(args.region, stop)?;

    // 启动本机前台 App 焦点监视器：READY 时前台即 VDI 窗口，以此为基线；
    // 帧发送过程中用户切走窗口可在 1 个字符内停止输入，避免脏数据
    let monitor = match FocusMonitor::start() {
        Ok(m) => Some(m),
        Err(e) => {
            warn!("焦点监视器启动失败（{e}），帧中失焦仅依赖二维码反馈（响应较慢）");
            None
        }
    };

    let pb = make_web_progress(total as u64);
    set_global_pb(Some(pb.clone()));

    for seq in 0..total {
        if stop.load(Ordering::Relaxed) {
            set_global_pb(None);
            pb.finish_and_clear();
            return Ok(());
        }
        // 发送前若页面失焦则先等待恢复，避免把整帧打到空处
        wait_while_paused(args.region, stop, monitor.as_ref())?;

        let mut attempt = 0usize;
        loop {
            if stop.load(Ordering::Relaxed) {
                set_global_pb(None);
                pb.finish_and_clear();
                return Ok(());
            }
            // 逐片进度显示：与分片传输一致（p 为 1-based 序号）
            info!("p{} >>> md5={}", seq + 1, prep.frames[seq].md5);
            let completed = send_frame(
                backend,
                &prep.frames[seq].body,
                args,
                stop,
                args.region,
                monitor.as_ref(),
                &pb,
            )?;
            if !completed {
                // 帧发送中途失焦：已立即停手，等待恢复后整帧重发（不计入重试）
                info!("检测到窗口失焦，已立即停止输入，等待重新聚焦...");
                wait_while_paused(args.region, stop, monitor.as_ref())?;
                continue;
            }

            match await_ack(args.region, seq, stop) {
                Ok(Ack::Acked) => {
                    pb.inc(1);
                    break;
                }
                Ok(Ack::Finished(fb)) => {
                    pb.inc(1);
                    set_global_pb(None);
                    pb.finish();
                    if fb.md5 == prep.file_md5 {
                        info!("传输完成，整文件 MD5 校验通过 ✓");
                    } else {
                        error!("整文件 MD5 不一致：本地 {}，远端 {}", prep.file_md5, fb.md5);
                        return Err("整文件 MD5 校验失败".to_string());
                    }
                    return Ok(());
                }
                Ok(Ack::Paused) => {
                    info!("页面失焦，暂停传输，等待重新聚焦...");
                    wait_while_paused(args.region, stop, monitor.as_ref())?;
                    // 失焦期间按键可能丢失，重发当前帧
                }
                Ok(Ack::Resend) => {
                    attempt += 1;
                    if attempt > args.max_retry {
                        set_global_pb(None);
                        return Err(format!(
                            "片 {seq} 重试 {attempt} 次仍失败（接收端无法确认）"
                        ));
                    }
                    warn!("片 {seq} 未确认（第 {attempt} 次重试），重发...");
                }
                Err(e) => {
                    attempt += 1;
                    if attempt > args.max_retry {
                        set_global_pb(None);
                        return Err(format!("片 {seq} 等待反馈失败：{e}"));
                    }
                    warn!("片 {seq} 等待反馈异常：{e}（第 {attempt} 次重试）");
                }
            }
        }
    }

    set_global_pb(None);
    pb.finish();
    // 末片未收到 FINISH（理论上 await_ack 会先返回 Finished）
    Err("传输结束但未收到接收端 FINISH 确认".to_string())
}

/// ACK 轮询结果。
enum Ack {
    /// 该片确认成功。
    Acked,
    /// 全部完成（末片）。
    Finished(Feedback),
    /// 页面失焦，需等待恢复后重发。
    Paused,
    /// 需重发当前帧（RETRY / READY 状态不匹配 / 超时）。
    Resend,
}

/// 发送一帧：RESET 清缓冲 → STX → 帧体 → ETX。
///
/// 帧体经可中止打字循环逐字符输入（带单片进度条）：每字符前检查
/// 1) 停止标志；2) 本机前台 App 焦点监视器（事件驱动，失焦后最多多打 1 个字符）；
/// 3) 每 700ms 一次二维码状态（远端失焦/致命错误兜底）。
///
/// 返回 `Ok(true)` 整帧发完；`Ok(false)` 中途失焦中止（ETX 未发送，接收端
/// 残留半帧，恢复后由下一帧的 Ctrl+A 清除）；`Err` 为致命错误。
fn send_frame(
    backend: &mut Backend,
    body: &str,
    args: &WebArgs,
    stop: &Arc<AtomicBool>,
    region: Option<Region>,
    monitor: Option<&FocusMonitor>,
    overall: &ProgressBar,
) -> Result<bool, String> {
    backend.send_reset(); // Ctrl+A：清空接收端残留半帧
    if args.printable_frame {
        backend.send_char('{');
    } else {
        backend.send_stx(); // Ctrl+B
    }

    // 中止条件：停止 / 本机失焦 / 二维码显示 PAUSE；致命错误经 fatal 传出
    let last_qr = std::cell::Cell::new(Instant::now());
    let fatal: std::cell::Cell<Option<String>> = std::cell::Cell::new(None);
    let abort = || -> bool {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        if let Some(m) = monitor {
            if !m.is_focused() {
                debug!("焦点监视器：前台 App 已切换，中止当前帧");
                return true;
            }
        }
        if last_qr.get().elapsed() >= QR_MIDCHECK_INTERVAL {
            last_qr.set(Instant::now());
            if let Ok(Some(fb)) = qr::read_feedback(region) {
                match fb.status {
                    Status::Pause => {
                        debug!("帧发送中收到 PAUSE，中止当前帧");
                        return true;
                    }
                    Status::Error => {
                        fatal.set(Some(format!("接收端错误：{}", fb.err)));
                        return true;
                    }
                    Status::Unauth => {
                        fatal.set(Some(
                            "接收端未授权保存目录（页面可能已刷新），请重新授权后开始传输"
                                .to_string(),
                        ));
                        return true;
                    }
                    _ => {}
                }
            }
        }
        false
    };

    let mut send_char = |ch| backend.send_char(ch);
    let completed = type_text_abortable(body, args.interval, &mut send_char, &abort);
    set_global_pb(Some(overall.clone()));
    if !completed {
        if let Some(e) = fatal.take() {
            return Err(e);
        }
        return Ok(false);
    }
    if args.printable_frame {
        backend.send_char('}');
    } else {
        backend.send_etx(); // Ctrl+C
    }
    Ok(true)
}

/// 发送后等待当前 seq 的 ACK。
fn await_ack(region: Option<Region>, seq: usize, stop: &Arc<AtomicBool>) -> Result<Ack, String> {
    let start = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err("已停止".to_string());
        }
        match qr::read_feedback(region) {
            Ok(Some(fb)) => match fb.status {
                Status::Ok if fb.seq as usize == seq => return Ok(Ack::Acked),
                Status::Finish if fb.seq as usize == seq => return Ok(Ack::Finished(fb)),
                Status::Error => {
                    error!("接收端错误：{}", fb.err);
                    return Err(format!("接收端错误：{}", fb.err));
                }
                // 传输中丢失授权（页面刷新/目录失效）：重发无法恢复，直接报错
                Status::Unauth => {
                    return Err(
                        "接收端未授权保存目录（页面可能已刷新），请重新授权后开始传输".to_string(),
                    )
                }
                Status::Pause => return Ok(Ack::Paused),
                Status::Retry => {
                    if fb.seq as usize == seq {
                        debug!("接收端请求重传片 {seq}：{}", fb.err);
                        return Ok(Ack::Resend);
                    }
                    // 序号不匹配：页面状态与发送端不一致（可能刷新过）
                    return Err(format!(
                        "接收端状态不一致（反馈 seq={}，当前 seq={seq}，{}），请刷新页面后重新开始",
                        fb.seq, fb.err
                    ));
                }
                // READY 或陈旧 OK：忽略继续等；超时后按重发处理
                _ => {}
            },
            Ok(None) => {}
            Err(e) => debug!("二维码读取异常：{e}"),
        }
        if start.elapsed() >= ACK_TIMEOUT {
            debug!("片 {seq} 等待 ACK 超时");
            return Ok(Ack::Resend);
        }
        std::thread::sleep(QR_POLL_INTERVAL);
    }
}

/// 初始等待接收端 READY。
///
/// `UNAUTH`（未授权目录）时持续等待并定期提示用户授权，不计入超时
/// （用户手动授权耗时不定）；长时间读不到任何有效反馈才按超时处理。
fn wait_ready(region: Option<Region>, stop: &Arc<AtomicBool>) -> Result<(), String> {
    let start = Instant::now();
    let mut last_seen = Instant::now();
    let mut last_unauth_hint = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err("已停止".to_string());
        }
        match qr::read_feedback(region) {
            Ok(Some(fb)) => {
                last_seen = Instant::now();
                match fb.status {
                    Status::Ready => return Ok(()),
                    Status::Error => return Err(format!("接收端错误：{}", fb.err)),
                    Status::Unauth if last_unauth_hint.elapsed() >= Duration::from_secs(5) => {
                        info!("等待接收端授权保存目录…（请在 VDI 浏览器页面点击「选择保存目录」）");
                        last_unauth_hint = Instant::now();
                    }
                    _ => {} // UNAUTH（节流期内）/ PAUSE / OK 等：继续等
                }
            }
            Ok(None) => {}
            Err(e) => debug!("二维码读取异常：{e}"),
        }
        // 超时基于「最后一次读到有效反馈」，UNAUTH/PAUSE 等持续反馈会刷新存活计时
        if last_seen.elapsed() >= READY_TIMEOUT || start.elapsed() >= READY_TIMEOUT * 4 {
            return Err(
                "等待接收端 READY 超时（请确认 receiver.html 已打开、二维码区域框选正确）"
                    .to_string(),
            );
        }
        std::thread::sleep(QR_POLL_INTERVAL);
    }
}

/// PAUSE 时持续等待，直到本机前台 App 回到基线 **且** 页面重新 READY/OK。
///
/// 本机焦点监视器事件驱动、延迟低，先用它快速判定；本机已聚焦但远端二维码
/// 仍为 PAUSE 时（焦点在 VDI 内但不在浏览器页面）继续等待二维码恢复。
fn wait_while_paused(
    region: Option<Region>,
    stop: &Arc<AtomicBool>,
    monitor: Option<&FocusMonitor>,
) -> Result<(), String> {
    let start = Instant::now();
    let mut hinted = false;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err("已停止".to_string());
        }
        // 本机前台 App 未切回：无需截图解码，快速空转等待
        if let Some(m) = monitor {
            if !m.is_focused() {
                if !hinted {
                    info!("已暂停：请切回 VDI 窗口，将自动恢复传输...");
                    hinted = true;
                }
                std::thread::sleep(QR_POLL_INTERVAL);
                continue;
            }
        }
        match qr::read_feedback(region) {
            Ok(Some(fb)) => match fb.status {
                Status::Ready | Status::Ok | Status::Finish => return Ok(()),
                Status::Error => return Err(format!("接收端错误：{}", fb.err)),
                // 传输中丢失授权（页面刷新/目录失效）：无法靠重发恢复，直接报错
                Status::Unauth => {
                    return Err(
                        "接收端未授权保存目录（页面可能已刷新），请重新授权后开始传输".to_string(),
                    )
                }
                Status::Pause | Status::Retry => {} // 继续等聚焦
            },
            Ok(None) => {}
            Err(e) => debug!("二维码读取异常：{e}"),
        }
        if start.elapsed() >= PAUSE_TIMEOUT {
            return Err("等待页面重新聚焦超时".to_string());
        }
        std::thread::sleep(QR_POLL_INTERVAL);
    }
}

/// 创建网页传输进度条。
fn make_web_progress(total: u64) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template("  网页传输 [{bar:40.cyan/blue}] {pos}/{len} 片 ({percent}%)")
            .unwrap()
            .progress_chars("=>-"),
    );
    pb
}

/// dry-run：打印帧概览，不输入。
fn dry_run_web(args: &WebArgs) -> Result<(), String> {
    let prep = prepare(&args.file, args.chunk_bytes)?;
    println!("[dry-run] 网页传输：{}", args.file.display());
    println!("  原始大小：{} 字节", prep.raw_size);
    println!(
        "  分片数量：{}（每片 {}B）",
        prep.frames.len(),
        args.chunk_bytes
    );
    println!(
        "  帧定界  ：{}",
        if args.printable_frame {
            "可打印 {{/}} （--web-printable-frame）"
        } else {
            "Ctrl+B / Ctrl+C"
        }
    );
    println!("  [流程] 等待二维码 READY → 逐片发送帧 → 二维码 OK/RETRY/PAUSE/FINISH");
    if let Some(first) = prep.frames.first() {
        let preview: String = first.body.chars().take(120).collect();
        println!("  帧 0 预览：{preview}...");
    }
    println!("  提示：receiver.html 需先用 --deploy-receiver 部署并在 VDI 浏览器打开。");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_file(name: &str, data: &[u8]) -> PathBuf {
        let p = std::env::temp_dir().join(name);
        fs::write(&p, data).unwrap();
        p
    }

    #[test]
    fn frame_format_and_roundtrip() {
        // 高熵随机数据 → 跳过压缩；验证帧结构与重组往返
        let data: Vec<u8> = (0..9000u32)
            .map(|i| i.wrapping_mul(2654435761) as u8)
            .collect();
        let p = tmp_file("tp_web_test_high.bin", &data);
        let prep = prepare(&p, 4096).unwrap();
        assert_eq!(prep.raw_size, 9000);
        assert_eq!(prep.frames.len(), 3);

        // 重组
        let mut reassembled = Vec::new();
        for (seq, f) in prep.frames.iter().enumerate() {
            let fields: Vec<&str> = f.body.split('|').collect();
            assert!(fields.len() >= 6, "片 {seq} 字段不足");
            assert_eq!(fields[0], seq.to_string());
            assert_eq!(fields[1], "3");
            assert_eq!(fields[2], "9000");
            let zflag: u8 = fields[3].parse().unwrap();
            let payload = base64::engine::general_purpose::STANDARD
                .decode(fields[5])
                .unwrap();
            assert_eq!(md5_of_bytes(&payload), fields[4], "片 {seq} md5 不匹配");
            let raw = if zflag == 1 {
                zstd::stream::decode_all(payload.as_slice()).unwrap()
            } else {
                payload
            };
            reassembled.extend_from_slice(&raw);
            // 文件名字段仅 seq=0
            if seq == 0 {
                assert_eq!(fields.len(), 7);
                let name = base64::engine::general_purpose::STANDARD
                    .decode(fields[6])
                    .unwrap();
                assert_eq!(String::from_utf8(name).unwrap(), "tp_web_test_high.bin");
            } else {
                assert_eq!(fields.len(), 6);
            }
        }
        assert_eq!(reassembled, data);
        assert_eq!(prep.file_md5, md5_of_bytes(&data));
    }

    #[test]
    fn text_file_uses_zstd() {
        // 高重复文本 → 压缩启用
        let data = b"hello typepaste web transfer ".repeat(500);
        let p = tmp_file("tp_web_test_text.txt", &data);
        let prep = prepare(&p, 4096).unwrap();
        let zflag: u8 = prep.frames[0]
            .body
            .split('|')
            .nth(3)
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(zflag, 1, "高重复文本应启用 zstd");

        // 重组验证
        let mut out = Vec::new();
        for f in &prep.frames {
            let fields: Vec<&str> = f.body.split('|').collect();
            let payload = base64::engine::general_purpose::STANDARD
                .decode(fields[5])
                .unwrap();
            out.extend_from_slice(&zstd::stream::decode_all(payload.as_slice()).unwrap());
        }
        assert_eq!(out, data);
    }

    #[test]
    fn small_file_single_chunk() {
        let data = b"hi";
        let p = tmp_file("tp_web_test_small.bin", data);
        let prep = prepare(&p, 4096).unwrap();
        assert_eq!(prep.frames.len(), 1);
        let fields: Vec<&str> = prep.frames[0].body.split('|').collect();
        assert_eq!(fields[0], "0");
        assert_eq!(fields[1], "1");
        // 小文件压缩通常无收益 → zflag=0（即使压缩，重组测试已覆盖）
        let payload = base64::engine::general_purpose::STANDARD
            .decode(fields[5])
            .unwrap();
        let zflag: u8 = fields[3].parse().unwrap();
        let raw = if zflag == 1 {
            zstd::stream::decode_all(payload.as_slice()).unwrap()
        } else {
            payload
        };
        assert_eq!(&raw, b"hi");
    }
}
