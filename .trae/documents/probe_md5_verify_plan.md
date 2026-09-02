# probe 添加 md5 校验计划

## 需求

在 probe 输出中增加第三行 `md5(file_size + file_md5)`，用于校验文件大小和文件 md5 的 OCR 识别是否正确。

1. 第一行：文件大小 `file_size`
2. 第二行：文件 md5 `file_md5`
3. 第三行：`md5(file_size + file_md5)`（校验和）

Rust 端解析三行后，验证 `md5(size_string + md5_string) == checksum`，不匹配则重试。

## 现状分析

### probe 输出（远程脚本）

当前 `probe` 输出两行：
- Line 1: ` ${size}`（大小，前导空格）
- Line 2: `${file_md5}` 经 `sed` 格式化（char_space=0 时首字符前加空格，=1 时每字符前加空格）

### OCR 解析（src/ocr.rs `parse_file_info`）

```rust
pub fn parse_file_info(lines: &[String]) -> Option<(usize, String)> {
    // 从底部向上扫描，找第一个 32-hex 行作为 md5，上一行作为 size
}
```

返回 `Option<(usize, String)>`（size, file_md5）。

### 调用方（src/pull.rs `probe_file_info`）

```rust
let check = |lines: &[String]| parse_file_info(lines).is_some();
// screenshot_ocr_with_check 内部用 check 判断是否成功
```

`parse_file_info` 返回 None 则触发重试。

## 修改方案

### 1. 远程脚本（4 个文件）

重构 `probe` 函数：先计算 size、file_md5、checksum 到变量，再格式化输出三行。

**bash 版（pull_linux.sh / pull_mac.sh / pull_gitbash.sh）：**

```bash
probe() {
  local path="$1"
  local space="$2"
  clear || true
  sleep .1
  echo
  # 1. 取大小（clean digits）
  local size
  size=$(stat -c '%s' "$path" 2>/dev/null || stat -f '%z' "$path" 2>/dev/null)
  # 2. 取 md5（clean 32-hex）
  local file_md5
  if command -v md5sum >/dev/null 2>&1; then
    file_md5=$(md5sum "$path" | cut -d' ' -f1)
  else
    file_md5=$(md5 -q "$path")
  fi
  # 3. 校验和 = md5(size + file_md5)
  local checksum
  if command -v md5sum >/dev/null 2>&1; then
    checksum=$(printf '%s' "${size}${file_md5}" | md5sum | cut -d' ' -f1)
  else
    checksum=$(printf '%s' "${size}${file_md5}" | md5 -q)
  fi
  # 4. 格式化输出三行
  echo " ${size}"
  local md5_sed
  if [ "$space" = "1" ]; then md5_sed='s/./ &/g'; else md5_sed='s/^./ &/'; fi
  printf '%s\n' "$file_md5" | sed -E "$md5_sed"
  printf '%s\n' "$checksum" | sed -E "$md5_sed"
}
```

注意：`echo " ${size}"` 保持大小前导空格的现有行为；md5 和 checksum 使用相同的 `md5_sed` 格式化。

**mac 版差异**：md5 命令选择逻辑保留原有的 `md5 -q` 优先于 `md5sum` 判断。

**powershell 版（pull_powershell.ps1）：**

```powershell
function Probe-File([string]$path, [bool]$charSpace) {
  Clear-Host
  Write-Output ''
  $size = (Get-Item -LiteralPath $path).Length
  $md5 = (Get-FileHash -LiteralPath $path -Algorithm MD5).Hash.ToLower()
  $combined = "$size$md5"
  $checksum = ([BitConverter]::ToString([System.Security.Cryptography.MD5]::Create().ComputeHash([System.Text.Encoding]::UTF8.GetBytes($combined))).Replace('-', '').ToLower())
  if ($charSpace) {
    Write-Output (' ' + $size)
    Write-Output ($md5 -replace '(.)', ' $1')
    Write-Output ($checksum -replace '(.)', ' $1')
  } else {
    Write-Output $size
    Write-Output $md5
    Write-Output $checksum
  }
}
```

### 2. OCR 解析（src/ocr.rs `parse_file_info`）

更新解析逻辑：从底部向上找连续两个 32-hex 行（file_md5 + checksum），再上一行为 size，校验 checksum。

```rust
use crate::utils::md5_of_bytes;

pub fn parse_file_info(lines: &[String]) -> Option<(usize, String)> {
    let non_empty: Vec<&str> = lines
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    // 从底部向上：找 checksum(32-hex) + file_md5(32-hex) + size(digits) 三连
    for i in (2..non_empty.len()).rev() {
        let checksum = normalize_hex(non_empty[i]);
        if checksum.len() != 32 {
            continue;
        }
        let file_md5 = normalize_hex(non_empty[i - 1]);
        if file_md5.len() != 32 {
            continue;
        }
        let size: usize = normalize_digits(non_empty[i - 2]).parse().ok()?;
        // 校验 md5(size + file_md5) == checksum
        let expected = md5_of_bytes(format!("{size}{file_md5}").as_bytes());
        if expected == checksum {
            return Some((size, file_md5));
        }
    }
    None
}
```

关键点：
- 大小用 `format!("{size}")` 转回十进制字符串，与远程 `stat` 输出的 clean digits 一致
- `normalize_digits` 和 `normalize_hex` 会去除空格和纠正 OCR 混淆字符
- 校验失败返回 None，触发 `probe_file_info` 重试

### 3. 测试更新（src/ocr.rs）

现有 `parse_file_info` 测试需补充第三行 checksum：

- `parse_file_info_normal`：增加正确的 checksum 行
- `parse_file_info_with_trailing_prompt`：增加 checksum 行
- `parse_file_info_digit_confusion`：增加 checksum 行
- 新增 `parse_file_info_checksum_mismatch`：checksum 错误时返回 None

计算 checksum 示例：size=1024, md5=`abcdef0123456789abcdef0123456789`，checksum = md5("1024abcdef0123456789abcdef0123456789")。

### 4. src/pull.rs `probe_file_info`

无需修改 — `check` 闭包 `parse_file_info(lines).is_some()` 现在隐含了 checksum 校验，OCR 识别错误会自动重试。

## 风险与注意事项

1. **大小字符串一致性**：远程 `stat -c '%s'` 输出无 leading zero 的十进制；Rust 端 `format!("{}", usize)` 同样无 leading zero。两者一致。
2. **char_space 格式化**：size 行始终前导空格；md5 和 checksum 行使用相同的 sed 格式化。`normalize_hex`/`normalize_digits` 会去除所有空格。
3. **mac 的 md5 命令选择**：保留原有 `md5 -q` 优先逻辑，但 size/md5/checksum 三个值需用同一个 md5 命令计算。
4. **向后兼容**：旧版脚本（无 checksum 行）的输出将无法通过新解析逻辑，需重新部署脚本。pull.rs 的 dry-run 提示已说明需先部署脚本。
