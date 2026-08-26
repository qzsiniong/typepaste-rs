//! 编码器（全新设计）：trait 化纯编码 bytes → ASCII，不含 shell 命令。
//!
//! base32/base16 输出小写（减少 Shift 键使用）；base64 含 `+/=` 需 Shift。

use base64::Engine;

/// 编码器能力。
pub trait Encoder {
    /// 展示名，如 "Base32"。
    fn name(&self) -> &'static str;
    /// 文件后缀，如 "b32"。
    fn suffix(&self) -> &'static str;
    /// 将字节编码为 ASCII 字符串（32/16 转小写）。
    fn encode(&self, data: &[u8]) -> String;
}

pub struct Base32;
pub struct Base64;
pub struct Base16;

impl Encoder for Base32 {
    fn name(&self) -> &'static str {
        "Base32"
    }
    fn suffix(&self) -> &'static str {
        "b32"
    }
    fn encode(&self, data: &[u8]) -> String {
        data_encoding::BASE32.encode(data).to_lowercase()
    }
}

impl Encoder for Base64 {
    fn name(&self) -> &'static str {
        "Base64"
    }
    fn suffix(&self) -> &'static str {
        "b64"
    }
    fn encode(&self, data: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(data)
    }
}

impl Encoder for Base16 {
    fn name(&self) -> &'static str {
        "Base16"
    }
    fn suffix(&self) -> &'static str {
        "b16"
    }
    fn encode(&self, data: &[u8]) -> String {
        hex::encode(data)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Encoding {
    Base32,
    Base64,
    Base16,
}

impl Encoding {
    /// 解析编码名；非法返回 None。
    #[allow(dead_code)]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "base32" => Some(Encoding::Base32),
            "base64" => Some(Encoding::Base64),
            "base16" => Some(Encoding::Base16),
            _ => None,
        }
    }

    /// 从后缀反推编码。
    #[allow(dead_code)]
    pub fn from_suffix(suffix: &str) -> Option<Self> {
        match suffix {
            "b32" => Some(Encoding::Base32),
            "b64" => Some(Encoding::Base64),
            "b16" => Some(Encoding::Base16),
            _ => None,
        }
    }

    pub fn encoder(&self) -> Box<dyn Encoder> {
        match self {
            Encoding::Base32 => Box::new(Base32),
            Encoding::Base64 => Box::new(Base64),
            Encoding::Base16 => Box::new(Base16),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_lowercase_and_roundtrip() {
        let data = b"hello world";
        let enc = Base32.encode(data);
        // 保留标准 base32 的 `=` 填充（解码端 GNU base32 / python 均需）；
        // 仅字母部分小写以减少 Shift，填充符 `=` 仅为末尾少量字符。
        assert!(
            enc.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '='),
            "{enc}"
        );
        assert_eq!(enc, data_encoding::BASE32.encode(data).to_lowercase());
        let upper: String = enc.to_uppercase();
        let decoded = data_encoding::BASE32.decode(upper.as_bytes()).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn base64_standard() {
        let data = b"hello world";
        assert_eq!(
            Base64.encode(data),
            base64::engine::general_purpose::STANDARD.encode(data)
        );
        assert_eq!(Base64.suffix(), "b64");
    }

    #[test]
    fn base16_lowercase() {
        let data = b"hello world";
        let enc = Base16.encode(data);
        assert_eq!(enc, hex::encode(data));
        assert!(enc
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn names_and_suffixes() {
        assert_eq!(Base32.name(), "Base32");
        assert_eq!(Base32.suffix(), "b32");
        assert_eq!(Base64.name(), "Base64");
        assert_eq!(Base16.name(), "Base16");
        assert_eq!(Base16.suffix(), "b16");
    }

    #[test]
    fn parse_and_from_suffix() {
        assert_eq!(Encoding::parse("base32"), Some(Encoding::Base32));
        assert_eq!(Encoding::parse("base64"), Some(Encoding::Base64));
        assert_eq!(Encoding::parse("base16"), Some(Encoding::Base16));
        assert_eq!(Encoding::parse("nope"), None);
        assert_eq!(Encoding::from_suffix("b64"), Some(Encoding::Base64));
        assert_eq!(Encoding::from_suffix("zz"), None);
    }
}
