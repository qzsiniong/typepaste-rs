# PowerShell 兼容性修复计划

## 问题

当目标机为 PowerShell 时，auto 模式和 deploy 模式生成的命令使用 bash heredoc 语法（`cat > file << 'EOF'`），PowerShell 不支持。截图显示：

```
cat > typepaste_20260825141151_Cargo.toml.gz.b32 << 'EOF'
```

PowerShell 报错：`重定向运算符后面缺少文件规范` 和 `"<"运算符是为将来使用而保留的`。

## 当前状态

- `--shell` 参数已存在，支持 `bash`（默认）和 `powershell`
- `Shell::Powershell` 仅用于 `auto_invoke_command` 生成调用还原脚本的命令
- `run_auto_mode` 和 `run_deploy` 的 cat heredoc 命令硬编码为 bash 语法
- `decode_cmd` 函数生成 bash 风格的解码命令

## 修改方案

### 1. restore_script.rs — 新增 shell 感知的命令生成

新增 4 个函数：

```rust
/// 生成 shell 适配的 heredoc/header 命令。
/// Bash:     cat > {file} << 'EOF'\n
/// PowerShell: $content = @'\n
pub fn heredoc_header(shell: Shell, file: &str) -> String

/// 生成 shell 适配的 heredoc/footer 命令。
/// Bash:     \nEOF\n
/// PowerShell: \n'@\nSet-Content -Path "{file}" -Value $content -NoNewline\n
pub fn heredoc_footer(shell: Shell, file: &str) -> String

/// 生成 shell 适配的解码命令（deploy 模式专用）。
/// Bash:     cat {file} | tr 'a-z' 'A-Z' | base32 -d > {out}
/// PowerShell: python3 -c "..." {file} {out}
pub fn decode_cmd_for_shell(shell: Shell, encoding: Encoding, encoded_file: &str, out_name: &str) -> String
```

PowerShell 解码命令用 Python（restore_powershell.ps1 已依赖 python3）：
- Base32: `python3 -c "import sys,base64;open(sys.argv[2],'wb').write(base64.b32decode(open(sys.argv[1]).read().upper()))" {file} {out}`
- Base64: `python3 -c "import sys,base64;open(sys.argv[2],'wb').write(base64.b64decode(open(sys.argv[1]).read()))" {file} {out}`
- Base16: `python3 -c "import sys;data=open(sys.argv[1]).read().strip().upper();open(sys.argv[2],'wb').write(bytes.fromhex(data))" {file} {out}`

同时在 `restore_script.rs` 中引入 `use crate::cli::Shell`（或改用独立的 Shell 枚举位置）。

### 2. cli.rs — 修改 auto_mode 和 deploy 使用 shell 适配命令

**run_auto_mode**：
```rust
// 之前
type_command(&format!("cat > {} << 'EOF'\n", payload.uid_full), interval, &mut send_char, stop);
type_command(&"\nEOF\n", interval, &mut send_char, stop);

// 之后
type_command(&heredoc_header(args.shell, &payload.uid_full), interval, &mut send_char, stop);
// ... type_text ...
type_command(&heredoc_footer(args.shell, &payload.uid_full), interval, &mut send_char, stop);
```

**run_deploy**：
```rust
// 之前
type_command(&format!("cat > {encoded_file} << 'EOF'\n"), interval, &mut send_char, stop);
type_command(&format!("\nEOF\n{cmd}\n"), interval, &mut send_char, stop);

// 之后
type_command(&heredoc_header(args.shell, &encoded_file), interval, &mut send_char, stop);
// ... type_text ...
type_command(&format!("{}\n{}", heredoc_footer(args.shell, &encoded_file), cmd), interval, &mut send_char, stop);
```

其中 `cmd` 改用 `decode_cmd_for_shell(args.shell, enc, &encoded_file, landing)`。

### 3. Shell 枚举位置

当前 `Shell` 在 `cli.rs` 中定义。`restore_script.rs` 需要引用它。

方案：将 `Shell` 枚举移至 `restore_script.rs`（或新建 `shell.rs`），`cli.rs` 从 `restore_script` 引用。

由于 `restore_script.rs` 已被 `cli.rs` 使用，且 `Shell` 与目标平台脚本相关，放在 `restore_script.rs` 合理。

### 4. 测试

在 `restore_script.rs` 的 tests 模块中新增：
- `heredoc_header_bash` — 验证 bash 头
- `heredoc_header_powershell` — 验证 PowerShell 头
- `heredoc_footer_bash` — 验证 bash 尾
- `heredoc_footer_powershell` — 验证 PowerShell 尾
- `decode_cmd_powershell_base32` — 验证 PowerShell base32 解码
- `decode_cmd_powershell_base64` — 验证 PowerShell base64 解码
- `decode_cmd_powershell_base16` — 验证 PowerShell base16 解码

在 `cli.rs` 的 tests 模块中修改 `auto_invoke_powershell` 测试（不变）。

## 涉及文件

- [restore_script.rs](file:///Users/qzs/code/labs/auto-type/typepaste-rs/src/restore_script.rs) — 新增 heredoc_header、heredoc_footer、decode_cmd_for_shell 函数；将 Shell 枚举从 cli.rs 移入
- [cli.rs](file:///Users/qzs/code/labs/auto-type/typepaste-rs/src/cli.rs) — run_auto_mode、run_deploy 使用新函数；Shell 枚举引用改为 restore_script
- [utils.rs](file:///Users/qzs/code/labs/auto-type/typepaste-rs/src/utils.rs) — 无需修改

## 验证

1. `cargo build` 编译通过
2. `cargo test` 全部通过
3. dry-run 验证：
   - `--shell bash`：输出 `cat > file << 'EOF'` 和 `EOF`
   - `--shell powershell`：输出 `$content = @'` 和 `'@` + `Set-Content`
4. 真机验证（PowerShell 目标机）：auto 模式输入正确的 PowerShell 命令
