# prepare\_remote\_chunks 同时输出 md5 改造计划

## 现状分析

当前反向传输（pull）流程：

1. `prepare_remote_chunks`：远程生成 `/tmp/tp_pull.b16`，**每行仅含 base16 分片内容**
2. `read_chunk_with_retry`：显示第 i 片时，分别执行两条命令：

   * `sed -n '{i}p' ...` 取内容（按 line\_width 换行 + 字符间距）

   * `sed -n '{i}p' ... | md5sum` **临时计算 md5**（再做字符间距）
3. OCR 识别内容行 + md5 行，`parse_hex_chunk` 从底部找 32 位 hex 行作为 md5

**问题**：md5 在读取时临时计算，每次显示都要跑一次 md5sum，且命令冗长。

## 目标

`prepare_remote_chunks` 生成文件时直接把 md5 拼到每行末尾，格式为：

```
<hex_content> <md5>
```

读取时直接取预计算的 md5，不再现场计算。

## 改动清单

### 1. `src/pull.rs` — `prepare_remote_chunks`

**Bash（xxd 路径）**：用 `while read` 循环逐行追加 md5：

```sh
xxd -p -c {chunk_bytes} {path} | while IFS= read -r l; do \
  printf '%s %s\n' "$l" "$(printf '%s' "$l" | md5sum | cut -d' ' -f1)"; \
done > /tmp/tp_pull.b16
```

> 键盘传输场景文件通常较小（KB 级），逐行 md5sum 开销可接受。

**Bash（python 回退）**：用 hashlib 一次性生成 `content md5`：

```python
import hashlib,sys
d=open(sys.argv[1],'rb').read(); c=int(sys.argv[2])
[print(d[i:i+c].hex(), hashlib.md5(d[i:i+c].hex().encode()).hexdigest())
 for i in range(0,len(d),c)]
```

**Powershell**：循环内对每片 hex 计算 MD5 后拼接：

```powershell
$hex = (-join ($chunk | ForEach-Object { $_.ToString('x2') }))
$md5 = ([System.Security.Cryptography.MD5]::Create().ComputeHash(
         [System.Text.Encoding]::UTF8.GetBytes($hex)) |
         ForEach-Object { $_.ToString('x2') }) -join ''
$sb.AppendLine("$hex $md5")
```

### 2. `src/pull.rs` — `read_chunk_with_retry`

不再现场计算 md5，改为从文件第 i 行拆分 content（字段1）和 md5（字段2），分别显示。**OCR 输出格式与之前完全一致**（内容行 + md5 行），因此 `parse_hex_chunk` 无需改动。

**Bash**：

```sh
sed -n '{i}p' /tmp/tp_pull.b16 | awk '{print $1}' | sed -E 's/.{{{lw}}}/&\n/g'{space_suffix}
sed -n '{i}p' /tmp/tp_pull.b16 | awk '{print $2}'{space_suffix}
```

**Powershell**：

```powershell
$l=(Get-Content /tmp/tp_pull.b16)[{idx}]; $p=$l -split ' '
$h=$p[0]; $m=$p[1]
$s=$h -replace '(.{{{lw}}})',('$1'+"`n"){space_replace}; $s; $m{space_replace}
```

### 3. `src/pull.rs` — `debug_chunk_diff`

`resources/tp_pull.txt` 现在每行是 `content md5`，对比时取第一个字段（content）：

```rust
let expected = content.lines().nth(i - 1)
    .map(|l| l.split_whitespace().next().unwrap_or("").trim())
    .unwrap_or("");
```

### 4. `resources/tp_pull.txt`

按新格式重新生成：每行 `hex_content md5`。可用脚本：

```sh
while IFS= read -r l; do
  printf '%s %s\n' "$l" "$(printf '%s' "$l" | md5sum | cut -d' ' -f1)"
done < <(xxd -p -c 50 resources/test.jpg) > resources/tp_pull.txt
```

### 5. `src/pull.rs` — `dry_run_pull`

更新打印文案，反映新格式：

```
[准备] xxd 分片并逐行追加 md5 → /tmp/tp_pull.b16（每行: content md5）
[循环] 逐片 clear; awk取content/md5 → 截图 OCR → 校验
```

## 不变项

* `parse_hex_chunk`、`parse_file_info`、`normalize_hex` 等 OCR 解析逻辑不变

* `probe_file_info` 不变（它是对整个文件做 md5，不是分片 md5）

* CLI 参数、PullArgs 结构不变

## 风险与处理

| 风险                             | 处理                                    |
| ------------------------------ | ------------------------------------- |
| bash `while read` 对大文件慢        | 键盘传输文件通常 ≤ 几百 KB，可接受；若需优化可切 python 路径 |
| awk `$1/$2` 切分在 content 含空格时出错 | content 是纯 hex，不含空格，安全                |
| `resources/tp_pull.txt` 未同步更新  | 必须同步重新生成，否则 debug 对比错位                |
| powershell MD5 计算返回大写          | 用 `.ToLower()` 统一小写（与 bash md5sum 一致） |

## 验证

* `cargo build` + `cargo test`（74 pass）+ `cargo clippy` 无警告

* `resources/tp_pull.txt` 每行格式正确：`<100 hex chars> <32 hex chars>`

* dry-run 输出文案正确

