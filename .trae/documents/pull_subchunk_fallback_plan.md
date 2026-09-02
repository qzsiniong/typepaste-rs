# Pull 分片失败时子分片回退计划

## 需求

pull 反向传输时，如果某个分片在最大重试次数后仍失败，将该分片切分为更小的子分片重新读取，其它分片不变。

## 现状分析

### 远程脚本流程

1. `prepare <path> <chunk_bytes>` → `/tmp/tp_pull.b16`（每行：`<hex_content> <md5>`）
2. `show <i> <line_width> <space>` → 显示第 i 行的 hex 内容 + md5

### Rust 流程（`run_pull`）

1. probe → file\_size, file\_md5
2. prepare\_remote\_chunks
3. `total_chunks = file_size.div_ceil(chunk_bytes)`
4. 循环 i=1..total\_chunks：`read_chunk_with_retry(i)` → OCR + md5 校验
5. 全部拼接 → hex decode → 写文件 → md5 校验

### 问题

`read_chunk_with_retry` 在 max\_retry 次失败后直接返回 Err，导致整个 pull 失败。

## 修改方案

### 1. 远程脚本：新增 `subchunk` 命令 + `show` 支持指定文件

**`subchunk <i> <sub_chunk_bytes>`**（4 个脚本均添加）：

* 读取 `/tmp/tp_pull.b16` 第 i 行

* 提取 hex 内容（字段 1）

* 按 `sub_chunk_bytes * 2` 字符切分为子片段

* 每个子片段计算 md5，写入 `/tmp/tp_pull_sub.b16`（格式同 b16）

**`show`** **命令增加可选第 4 参数**：文件路径，默认 `/tmp/tp_pull.b16`。这样子分片读取时传入 `/tmp/tp_pull_sub.b16` 即可复用 show 逻辑，无需新增 `showsub` 命令。

bash 版：

```bash
show() {
  local i="$1"
  local lw="$2"
  local space="$3"
  local file="${4:-/tmp/tp_pull.b16}"
  ...
  sed -n "${i}p" "$file" | awk '{print $1}' | ...
  sed -n "${i}p" "$file" | awk '{print $2}' | ...
}
```

powershell 版：`Show-Chunk` 增加 `[string]$file = '/tmp/tp_pull.b16'` 参数。

### 2. Rust：`read_chunk_with_fallback` 包装重试 + 子分片

新增 `read_chunk_with_fallback(i, chunk_i_bytes, args, type_command, stop)`：

```
1. 先调用 read_chunk_with_retry(i) 常规读取
2. 成功 → 返回
3. 失败 → 渐进式子分片回退：
   sub_bytes = chunk_bytes / 2
   while sub_bytes >= 16 且 sub_bytes < chunk_i_bytes:
       调用 read_chunk_with_subsplit(i, chunk_i_bytes, sub_bytes, ...)
       成功 → 返回
       失败 → sub_bytes /= 2
4. 全部失败 → 返回 Err
```

**`read_chunk_with_subsplit(i, chunk_i_bytes, sub_bytes, ...)`**：

* 键入 `subchunk i sub_bytes` 命令，等待生成 `/tmp/tp_pull_sub.b16`

* `sub_count = chunk_i_bytes.div_ceil(sub_bytes)`

* 循环 j=1..sub\_count：`read_subchunk_with_retry(j, ...)` 读取子分片

* 拼接所有子分片 hex → 返回

**`read_subchunk_with_retry`**：与 `read_chunk_with_retry` 几乎相同，仅 `show` 命令第 4 参数传 `/tmp/tp_pull_sub.b16`。可复用 `read_chunk_with_retry` 的重试逻辑，通过参数区分 b16 文件。

### 3. `run_pull` 调整

```rust
for i in 1..=total_chunks {
    let chunk_i_bytes = if i == total_chunks {
        file_size - (i - 1) * args.chunk_bytes
    } else {
        args.chunk_bytes
    };
    let chunk_hex = read_chunk_with_fallback(i, chunk_i_bytes, args, &mut type_command, stop)?;
    all_hex.push_str(&chunk_hex);
    pb.inc(1);
}
```

进度条仍按原始分片数递增，子分片是内部细节。

### 4. `read_chunk_with_retry` 重构

为复用重试逻辑，给 `read_chunk_with_retry` 增加 `b16_file: Option<&str>` 参数（None 用默认），或提取内部重试循环为共享函数。`read_subchunk_with_retry` 直接调用并传入子文件路径。

## 涉及文件

| 文件                            | 改动                                                                                                            |
| ----------------------------- | ------------------------------------------------------------------------------------------------------------- |
| `scripts/pull_linux.sh`       | 新增 `subchunk`；`show` 加可选 file 参数                                                                              |
| `scripts/pull_mac.sh`         | 同上                                                                                                            |
| `scripts/pull_gitbash.sh`     | 同上                                                                                                            |
| `scripts/pull_powershell.ps1` | 同上                                                                                                            |
| `src/pull.rs`                 | 新增 `read_chunk_with_fallback`、`read_chunk_with_subsplit`；`read_chunk_with_retry` 加文件参数；`run_pull` 调用 fallback |

## 关键设计点

1. **子分片大小**：渐进式二分（`chunk_bytes/2` → `/4` → `/8`...），最小 16 字节，确保总有更小的粒度可试。
2. **子分片数量**：`chunk_i_bytes.div_ceil(sub_bytes)`，最后一个原始分片可能不足 `chunk_bytes`，用实际字节数计算。
3. **不影响其它分片**：`/tmp/tp_pull.b16` 保持不变，子分片写入独立的 `/tmp/tp_pull_sub.b16`。
4. **校验**：每个子分片独立 md5 校验，拼接后最终由文件级 md5 兜底。
5. **show 兼容性**：第 4 参数可选，旧调用（3 参数）行为不变。

## 风险与注意

1. **子分片生成等待**：`subchunk` 是纯字符串切分，比 `prepare` 快，等待 \~1 秒即可。
2. **多次回退开销**：每次回退需重新 `subchunk` + 重读所有子分片，子分片越多越慢。但渐进式二分确保大粒度先试，失败才缩小。
3. **末分片边界**：最后一个分片字节数可能小于 `chunk_bytes`，需用 `file_size - (i-1)*chunk_bytes` 计算实际大小。
4. **powershell 路径**：`/tmp/tp_pull_sub.b16` 在 Windows Git Bash/PowerShell 下需确认路径可达（原 b16 已用 `/tmp/`，保持一致）。

