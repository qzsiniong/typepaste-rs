//! 二维码反馈通道：截屏 → rqrr 解码 → 网页接收端状态 JSON 解析。
//!
//! 网页接收端（receiver.html）将状态渲染为固定位置二维码：
//! `{"seq":0,"status":"OK","md5":"...","err":""}`
//! 本机定时截屏解码，驱动 ARQ 状态机（下一片 / 重传 / 暂停 / 结束）。

use std::path::Path;

use crate::debug;
use crate::ocr::{screenshot, Region};

/// 网页接收端反馈状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// 页面就绪（聚焦、空闲，等待分片）。
    Ready,
    /// 分片 md5 校验通过。
    Ok,
    /// 分片校验失败，请求重传。
    Retry,
    /// 窗口失焦，暂停发送。
    Pause,
    /// 全部传输完成。
    Finish,
    /// 致命错误。
    Error,
    /// 尚未授权保存目录（发送端应继续等待，不传输、不报致命错误）。
    Unauth,
}

impl Status {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "READY" => Some(Status::Ready),
            "OK" => Some(Status::Ok),
            "RETRY" => Some(Status::Retry),
            "PAUSE" => Some(Status::Pause),
            "FINISH" => Some(Status::Finish),
            "ERROR" => Some(Status::Error),
            "UNAUTH" => Some(Status::Unauth),
            _ => None,
        }
    }
}

/// 解码后的反馈帧。
#[derive(Debug, Clone)]
pub struct Feedback {
    pub seq: i64,
    pub status: Status,
    pub md5: String,
    pub err: String,
}

/// 从二维码文本（JSON）解析反馈。字段极少，手工提取避免引入 serde。
pub fn parse_feedback(json: &str) -> Option<Feedback> {
    let status = Status::parse(json_field(json, "status")?.trim_matches('"'))?;
    let seq = json_field(json, "seq")?.parse().ok()?;
    let md5 = json_field(json, "md5")?.trim_matches('"').to_string();
    let err = json_field(json, "err")
        .unwrap_or("\"\"")
        .trim_matches('"')
        .to_string();
    Some(Feedback {
        seq,
        status,
        md5,
        err,
    })
}

/// 提取 `"key":value` 的 value 子串（字符串值含引号，数字不含）。
fn json_field<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\"");
    let start = json.find(&pat)? + pat.len();
    let rest = json[start..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    // 值到下一个逗号或对象结束为止
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    Some(rest[..end].trim())
}

/// 截屏并尝试解码二维码。无二维码返回 Ok(None)，截屏/解码异常返回 Err。
#[cfg(target_os = "macos")]
pub fn read_feedback(region: Option<Region>) -> Result<Option<Feedback>, String> {
    let ts = chrono::Local::now().format("%Y%m%d%H%M%S%f").to_string();
    let tmp = std::env::temp_dir().join(format!("tp_qr_{ts}.png"));
    debug!("二维码截图：{tmp:?}");
    screenshot(&tmp, region)?;
    let fb = decode_qr_file(&tmp)?;
    let _ = std::fs::remove_file(&tmp);
    debug!("二维码反馈：{fb:?}");
    Ok(fb)
}

#[cfg(not(target_os = "macos"))]
pub fn read_feedback(_region: Option<Region>) -> Result<Option<Feedback>, String> {
    Err("二维码反馈通道目前仅支持 macOS".to_string())
}

/// 从图片文件解码二维码并解析反馈。
fn decode_qr_file(path: &Path) -> Result<Option<Feedback>, String> {
    let img = image::open(path).map_err(|e| format!("二维码图片加载失败：{e}"))?;
    let gray = img.to_luma8();
    let mut prepared = rqrr::PreparedImage::prepare(gray);
    let grids = prepared.detect_grids();
    for grid in grids {
        match grid.decode() {
            Ok((_meta, content)) => {
                if let Some(fb) = parse_feedback(&content) {
                    return Ok(Some(fb));
                }
                crate::debug!("二维码内容无法解析为反馈：{content}");
            }
            Err(e) => {
                crate::debug!("二维码解码失败：{e}");
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ok_feedback() {
        let json = r#"{"seq":3,"status":"OK","md5":"abc123","err":""}"#;
        let fb = parse_feedback(json).unwrap();
        assert_eq!(fb.seq, 3);
        assert_eq!(fb.status, Status::Ok);
        assert_eq!(fb.md5, "abc123");
        assert_eq!(fb.err, "");
    }

    #[test]
    fn parse_all_statuses() {
        for (s, expect) in [
            ("READY", Status::Ready),
            ("OK", Status::Ok),
            ("RETRY", Status::Retry),
            ("PAUSE", Status::Pause),
            ("FINISH", Status::Finish),
            ("ERROR", Status::Error),
            ("UNAUTH", Status::Unauth),
        ] {
            let json = format!(r#"{{"seq":0,"status":"{s}","md5":"","err":""}}"#);
            let fb = parse_feedback(&json).unwrap();
            assert_eq!(fb.status, expect);
        }
    }

    #[test]
    fn parse_error_feedback_with_message() {
        let json = r#"{"seq":1,"status":"ERROR","md5":"","err":"未授权目录"}"#;
        let fb = parse_feedback(json).unwrap();
        assert_eq!(fb.status, Status::Error);
        assert_eq!(fb.err, "未授权目录");
    }

    #[test]
    fn parse_finish_with_full_md5() {
        let json =
            r#"{"seq":127,"status":"FINISH","md5":"d41d8cd98f00b204e9800998ecf8427e","err":""}"#;
        let fb = parse_feedback(json).unwrap();
        assert_eq!(fb.seq, 127);
        assert_eq!(fb.status, Status::Finish);
        assert_eq!(fb.md5.len(), 32);
    }

    #[test]
    fn parse_invalid_json() {
        assert!(parse_feedback("not json").is_none());
        assert!(parse_feedback(r#"{"seq":0,"status":"WAT","md5":"","err":""}"#).is_none());
        assert!(parse_feedback(r#"{"status":"OK","md5":"","err":""}"#).is_none());
    }

    #[test]
    fn json_field_extraction() {
        let json = r#"{"seq":5,"status":"OK","md5":"aa","err":""}"#;
        assert_eq!(json_field(json, "seq"), Some("5"));
        assert_eq!(json_field(json, "status"), Some("\"OK\""));
        assert_eq!(json_field(json, "md5"), Some("\"aa\""));
        assert!(json_field(json, "nope").is_none());
    }
}
