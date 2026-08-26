//! 配置常量与参数校验。

use crate::encoder::Encoding;

/// 每字符输入间隔（毫秒）。
pub const DEFAULT_INTERVAL: u64 = 5;
pub const DEFAULT_DELAY: u64 = 5;
pub const MAX_FILE_SIZE: usize = 5 * 1024 * 1024;
/// auto 模式每条命令后等待目标终端执行（秒）。
pub const CMD_SLEEP: f64 = 0.5;
/// 仅当 gzip 后大小 < 原始大小 * 此值 才采用压缩。
pub const GZIP_USE_RATIO: f64 = 0.8;
/// 编码内容每 N 字符换行（解码时忽略换行）。
pub const WRAP_EVERY: usize = 100;
/// dry-run 预览真实内容时保留的前缀字符数（超过则截断）。
pub const DRY_RUN_PREVIEW: usize = 500;
/// 默认编码,非ASCII/归档/压缩 → 强制编码, 未指定时使用该默认值。
pub const DEFAULT_ENCODING: Encoding = Encoding::Base32;

/// 非负整数校验（供 clap value_parser）。
pub fn non_neg_int(s: &str) -> Result<u64, String> {
    let n: i64 = s.parse().map_err(|_| format!("需要整数，得到 {s}"))?;
    if n < 0 {
        return Err(format!("必须 >= 0，得到 {n}"));
    }
    Ok(n as u64)
}

/// 解析带后缀的大小字符串为字节数：`1m`/`2M`/`500k`/`500K`/`1048576` → usize。
/// 最小值 1，否则报错（沿用项目防御性校验约定）。供 clap value_parser。
pub fn parse_size(s: &str) -> Result<usize, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("大小不能为空".to_string());
    }
    let last = s.as_bytes()[s.len() - 1];
    let (num_str, mult): (&str, usize) = if last.is_ascii_digit() {
        (s, 1)
    } else {
        let (n, suf) = s.split_at(s.len() - 1);
        let m = match suf {
            "k" | "K" => 1024,
            "m" | "M" => 1024 * 1024,
            _ => return Err(format!("未知大小后缀 {suf:?}（支持 k/K/m/M）")),
        };
        (n, m)
    };
    let n: usize = num_str
        .parse()
        .map_err(|_| format!("需要整数，得到 {num_str}"))?;
    if n == 0 {
        return Err("大小必须 >= 1".to_string());
    }
    n.checked_mul(mult)
        .ok_or_else(|| format!("{n}×{mult} 溢出"))
}
