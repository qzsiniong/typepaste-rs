//! 还原脚本管理：内嵌 4 平台变体脚本 + uid 后缀解析 + shell 适配命令。

use std::format;

use clap::ValueEnum;

use crate::encoder::Encoding;

/// 目标机 shell 类型，决定 heredoc 和解码命令的语法。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Shell {
    /// bash 变体（linux/mac/gitbash）。
    Bash,
    /// powershell 变体。
    Powershell,
}

/// 目标机平台变体。
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Target {
    /// Linux（GNU base32/md5sum/xxd）。
    Linux,
    /// macOS（缺 base32/md5sum，用 python3/md5 回退）。
    Mac,
    /// Windows Git Bash（GNU 工具，Windows 路径）。
    Gitbash,
    /// Windows PowerShell。
    Powershell,
}

impl Target {
    pub fn label(self) -> &'static str {
        match self {
            Target::Linux => "linux",
            Target::Mac => "mac",
            Target::Gitbash => "gitbash",
            Target::Powershell => "powershell",
        }
    }

    /// 内嵌脚本内容。
    pub fn script(self) -> &'static str {
        match self {
            Target::Linux => include_str!("../scripts/restore_linux.sh"),
            Target::Mac => include_str!("../scripts/restore_mac.sh"),
            Target::Gitbash => include_str!("../scripts/restore_gitbash.sh"),
            Target::Powershell => include_str!("../scripts/restore_powershell.ps1"),
        }
    }

    /// 落地脚本文件名。
    pub fn landing_name(self) -> &'static str {
        match self {
            Target::Powershell => "typepaste-restore.ps1",
            _ => "typepaste-restore.sh",
        }
    }

    pub fn all() -> [Target; 4] {
        [
            Target::Linux,
            Target::Mac,
            Target::Gitbash,
            Target::Powershell,
        ]
    }
}

/// 解析 uid_full 后缀得到的还原操作序列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ops {
    /// 需要的解码（若末尾为 .b32/.b64/.b16）。
    pub decode: Option<Encoding>,
    /// 是否需要 gunzip（.gz）。
    pub gunzip: bool,
    /// 是否需要 unzip（.zip）。
    pub unzip: bool,
    /// 分片号（若末尾为 .p{n}）。
    pub part: Option<usize>,
}

/// 据 uid_full 的后缀解析还原操作（反向：.p{n} → decode → gunzip → unzip）。
pub fn parse_ops(uid_full: &str) -> Ops {
    let mut decode = None;
    let mut gunzip = false;
    let mut unzip = false;
    let mut part = None;

    let mut name = uid_full;
    // 1. 先剥末尾的 .p{n}（分片后缀）：要求字符串以「.p + 数字」结尾。
    let bytes = name.as_bytes();
    let mut i = bytes.len();
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i >= 2 && &bytes[i - 2..i] == b".p" && i < bytes.len() {
        part = Some(name[i..].parse::<usize>().unwrap());
        name = &name[..i - 2];
    }

    // 2. 编码后缀
    for (suf, enc) in [
        ("b32", Encoding::Base32),
        ("b64", Encoding::Base64),
        ("b16", Encoding::Base16),
    ] {
        let dot = format!(".{suf}");
        if name.ends_with(&dot) {
            decode = Some(enc);
            name = &name[..name.len() - dot.len()];
            break;
        }
        let _ = enc;
    }
    if name.ends_with(".gz") {
        gunzip = true;
        name = &name[..name.len() - 3];
    }
    if name.ends_with(".zip") {
        unzip = true;
        name = &name[..name.len() - 4];
    }
    let _ = name;
    Ops {
        decode,
        gunzip,
        unzip,
        part,
    }
}

/// 生成重建命令：把 `encoded_file` 解码为 `out_name`。
pub fn decode_cmd(encoding: Encoding, encoded_file: &str, out_name: &str) -> String {
    match encoding {
        Encoding::Base32 => {
            format!("cat {encoded_file} | tr 'a-z' 'A-Z' | base32 -d > {out_name}")
        }
        Encoding::Base64 => format!("base64 -d {encoded_file} > {out_name}"),
        Encoding::Base16 => format!("cat {encoded_file} | tr 'a-z' 'A-Z' | xxd -r -p > {out_name}"),
    }
}

/// 生成 shell 适配的 heredoc 头命令。
///
/// Bash:       `cat > {file} << 'EOF'\n`
/// PowerShell: `$content = @'\n`
pub fn heredoc_header(shell: Shell, file: &str) -> String {
    match shell {
        Shell::Bash => format!("cat > {file} << 'EOF'\n"),
        Shell::Powershell => "$content = @'\n".to_string(),
    }
}

/// 生成 shell 适配的 heredoc 尾命令。
///
/// Bash:       `\nEOF\n`
/// PowerShell: `\n'@\nSet-Content -Path "{file}" -Value $content -NoNewline\n`
pub fn heredoc_footer(shell: Shell, file: &str) -> String {
    match shell {
        Shell::Bash => "\nEOF\n".to_string(),
        Shell::Powershell => {
            format!("\n'@\nSet-Content -Path \"{file}\" -Value $content -NoNewline\n")
        }
    }
}

/// 生成 shell 适配的解码命令（deploy 模式专用）。
///
/// Bash:       同 `decode_cmd`（cat + tr + base32/xxd）
/// PowerShell: 用 python3 解码（restore_powershell.ps1 已依赖 python3）
pub fn decode_cmd_for_shell(
    shell: Shell,
    encoding: Encoding,
    encoded_file: &str,
    out_name: &str,
) -> String {
    match shell {
        Shell::Bash => decode_cmd(encoding, encoded_file, out_name),
        Shell::Powershell => match encoding {
            Encoding::Base32 => format!(
                "python3 -c \"import sys,base64;open(sys.argv[2],'wb').write(base64.b32decode(open(sys.argv[1]).read().upper()))\" {encoded_file} {out_name}"
            ),
            Encoding::Base64 => format!(
                "python3 -c \"import sys,base64;open(sys.argv[2],'wb').write(base64.b64decode(open(sys.argv[1]).read()))\" {encoded_file} {out_name}"
            ),
            Encoding::Base16 => format!(
                "python3 -c \"import sys;data=open(sys.argv[1]).read().strip().upper();open(sys.argv[2],'wb').write(bytes.fromhex(data))\" {encoded_file} {out_name}"
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_b32() {
        let ops = parse_ops("typepaste_20260822153616_file.b32");
        assert_eq!(ops.decode, Some(Encoding::Base32));
        assert!(!ops.gunzip);
        assert!(!ops.unzip);
    }

    #[test]
    fn parse_zip_b32() {
        let ops = parse_ops("typepaste_20260822153616_dir.zip.b32");
        assert_eq!(ops.decode, Some(Encoding::Base32));
        assert!(!ops.gunzip);
        assert!(ops.unzip);
    }

    #[test]
    fn parse_gz_b64() {
        let ops = parse_ops("typepaste_20260822153616_file.gz.b64");
        assert_eq!(ops.decode, Some(Encoding::Base64));
        assert!(ops.gunzip);
        assert!(!ops.unzip);
    }

    #[test]
    fn parse_zip_gz_b32() {
        let ops = parse_ops("typepaste_20260822153616_dir.zip.gz.b32");
        assert_eq!(ops.decode, Some(Encoding::Base32));
        assert!(ops.gunzip);
        assert!(ops.unzip);
    }

    #[test]
    fn parse_no_suffix() {
        let ops = parse_ops("typepaste_20260822153616_file");
        assert_eq!(ops.decode, None);
        assert!(!ops.gunzip);
        assert!(!ops.unzip);
        assert_eq!(ops.part, None);
    }

    #[test]
    fn parse_part_suffix_plain() {
        let ops = parse_ops("typepaste_file.b32.p3");
        assert_eq!(ops.part, Some(3));
        assert_eq!(ops.decode, Some(Encoding::Base32));
        assert!(!ops.gunzip);
        assert!(!ops.unzip);
    }

    #[test]
    fn parse_part_suffix_full_pipeline() {
        let ops = parse_ops("typepaste_dir.zip.gz.b32.p2");
        assert_eq!(ops.part, Some(2));
        assert_eq!(ops.decode, Some(Encoding::Base32));
        assert!(ops.gunzip);
        assert!(ops.unzip);
    }

    #[test]
    fn parse_part_suffix_not_trailing_digit() {
        // 文件名以 .b32 结尾（非数字），不识别为分片
        let ops = parse_ops("typepaste_release.p1.b32");
        assert_eq!(ops.part, None);
        assert_eq!(ops.decode, Some(Encoding::Base32));
    }

    #[test]
    fn decode_cmd_base32() {
        let cmd = decode_cmd(Encoding::Base32, "a.b32", "a");
        assert!(cmd.contains("tr 'a-z' 'A-Z'"));
        assert!(cmd.contains("base32 -d"));
        assert!(cmd.ends_with("> a"));
    }

    #[test]
    fn decode_cmd_base64() {
        let cmd = decode_cmd(Encoding::Base64, "a.b64", "a");
        assert_eq!(cmd, "base64 -d a.b64 > a");
    }

    #[test]
    fn decode_cmd_base16() {
        let cmd = decode_cmd(Encoding::Base16, "a.b16", "a");
        assert!(cmd.contains("xxd -r -p"));
    }

    #[test]
    fn target_variants_complete() {
        for t in Target::all() {
            assert!(!t.script().is_empty());
            assert!(t.landing_name().starts_with("typepaste-restore."));
        }
    }

    #[test]
    fn heredoc_header_bash() {
        let h = heredoc_header(Shell::Bash, "file.txt");
        assert_eq!(h, "cat > file.txt << 'EOF'\n");
    }

    #[test]
    fn heredoc_header_powershell() {
        let h = heredoc_header(Shell::Powershell, "file.txt");
        assert_eq!(h, "$content = @'\n");
    }

    #[test]
    fn heredoc_footer_bash() {
        let f = heredoc_footer(Shell::Bash, "file.txt");
        assert_eq!(f, "\nEOF\n");
    }

    #[test]
    fn heredoc_footer_powershell() {
        let f = heredoc_footer(Shell::Powershell, "file.txt");
        assert!(f.contains("'@"));
        assert!(f.contains("Set-Content -Path \"file.txt\""));
        assert!(f.contains("-NoNewline"));
    }

    #[test]
    fn decode_cmd_powershell_base32() {
        let cmd = decode_cmd_for_shell(Shell::Powershell, Encoding::Base32, "f.b32", "f");
        assert!(cmd.contains("python3"));
        assert!(cmd.contains("base64.b32decode"));
        assert!(cmd.contains("f.b32"));
        assert!(cmd.ends_with("f"));
    }

    #[test]
    fn decode_cmd_powershell_base64() {
        let cmd = decode_cmd_for_shell(Shell::Powershell, Encoding::Base64, "f.b64", "f");
        assert!(cmd.contains("python3"));
        assert!(cmd.contains("base64.b64decode"));
    }

    #[test]
    fn decode_cmd_powershell_base16() {
        let cmd = decode_cmd_for_shell(Shell::Powershell, Encoding::Base16, "f.b16", "f");
        assert!(cmd.contains("python3"));
        assert!(cmd.contains("bytes.fromhex"));
    }

    #[test]
    fn decode_cmd_for_shell_bash_delegates() {
        let cmd = decode_cmd_for_shell(Shell::Bash, Encoding::Base32, "f.b32", "f");
        assert_eq!(cmd, decode_cmd(Encoding::Base32, "f.b32", "f"));
    }
}
