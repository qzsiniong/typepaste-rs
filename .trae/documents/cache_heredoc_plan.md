# heredoc 数据缓存方案

## 1. 背景与目标

typepaste-rs 通过模拟键盘（enigo）逐字符把文件内容 `cat << EOF` 到远程桌面终端，速度极慢（每字符 5ms，1MB ≈ 5000秒）。重复传输相同文件/分片时浪费巨大。

**目标**：将所有 heredoc 写入的内容缓存到远程桌面的固定目录，以内容 md5 命名。再次传输相同内容时，先检查缓存是否存在，命中则跳过键入（仅建符号链接），大幅减少重复传输。

## 2. 核心流程

```
对每个待写入内容 content（如分片 pN 的编码串）：
  1. file_md5 = md5(content 的精确字节，含换行)
  2. 检查 {cache_dir}/{file_md5} 是否存在
     → 键入存在性命令，截图+OCR 读取结果
  3. 命中：ln -s {cache_dir}/{file_md5} {target_name}   （跳过键入！）
  4. 未命中：
     a. heredoc 写入 {cache_dir}/{ts}__{file_md5}.tmp
     b. md5sum 校验该 tmp 文件 == file_md5
     c. 通过 → mv {tmp} {cache_dir}/{file_md5}
     d. ln -s {cache_dir}/{file_md5} {target_name}
```

**缓存收益**：相同 part-size 重传同一文件时，所有分片命中缓存，仅需键入 symlink 命令（秒级 vs 原数十分钟）。

## 3. 关键设计决策

### 3.1 存在性检查与回读（OCR）

远程终端输出无法直接读取，必须靠 **截图 + OCR**。

**存在性命令**（输出强特征标记，便于 OCR 匹配）：
```bash
# bash
test -f {cache_dir}/{md5} && echo __TP_HIT__ || echo __TP_MISS__
```
```powershell
if (Test-Path {cache_dir}/{md5}) { '__TP_HIT__' } else { '__TP_MISS__' }
```

**OCR 实现（macOS 优先）**：
- 截图：`screencapture -x -R x,y,w,h /tmp/tp_shot.png` 或 `xcap` crate
- OCR：macOS **Vision 框架**（`VNRecognizeTextRequest`），通过 Swift 子进程调用
  - 无需第三方依赖，macOS 原生，准确率高
  - 备选：`tesseract` CLI（跨平台，需 `brew install tesseract`）
- 匹配：在 OCR 文本中查找 `__TP_HIT__` / `__TP_MISS__`

**OCR 可靠性保障**：
- 标记用全大写+下划线，OCR 易识别
- 键入前先 `clear`/`printf '\n\n\n'` 保证输出区干净
- 命令后等待 `CMD_SLEEP` 秒再截图
- OCR 失败/未匹配 → 降级为「未命中」（最坏情况多写一次，不影响正确性）

### 3.2 缓存键

- 键 = `md5(写入文件的精确字节)`，含 `type_text` 插入的 wrap 换行和 heredoc 末尾换行
- 校验时 `md5sum {tmp}` 必须 == 文件名中的 md5，确保写入无误后才 rename
- 文件名 32 位十六进制，无扩展名

### 3.3 缓存目录与链接方式

- `--cache-dir` 参数指定远程缓存目录，默认 `~/.typepaste_cache`
- 目标文件用**符号链接**指向缓存：`ln -s {cache_dir}/{md5} {uid}.pN`
  - `cat`/`md5sum` 等命令自动跟随 symlink，还原脚本无需改动
  - 备选：symlink 失败时降级 `cp`（FAT/无权限场景）

### 3.4 作用范围（分阶段）

| 阶段 | 缓存对象 | 收益 |
|------|---------|------|
| P0 | 分片文件 `{uid}.pN` | 最大（重传同文件全跳过） |
| P1 | helper 还原脚本 `{uid}__restore.sh` | 小（文件短） |
| P2 | deploy 的还原脚本本身 | 小（一次性） |

本方案先实现 P0，架构预留 P1/P2 扩展。

## 4. 模块与文件改动

### 4.1 新增 `src/ocr.rs`

```rust
/// 截取屏幕指定区域并 OCR，返回识别文本。
pub fn screenshot_and_ocr(region: Option<ScreenRegion>) -> Result<String, String>;

/// 在 OCR 文本中查找缓存标记，返回命中与否。
pub fn parse_cache_marker(ocr_text: &str) -> Option<bool>; // true=HIT, false=MISS
```

- macOS 实现：`screencapture` + Swift 脚本调用 Vision
- 失败返回 `Err`，调用方降级处理

### 4.2 新增 `src/cache.rs`

```rust
pub struct CacheWriter<'a> {
    cache_dir: String,
    shell: Shell,
    interval: u64,
    // ... backend 引用
}

impl CacheWriter {
    /// 缓存感知地写入 content 到 target_name。
    /// 命中缓存 → 仅 symlink；未命中 → 写 tmp + 校验 + rename + symlink。
    pub fn write_cached(
        &mut self,
        content: &str,
        target_name: &str,
    ) -> Result<CacheResult, String>;
}

pub enum CacheResult { Hit, Written, VerifiedFailed, }
```

内部步骤：
1. `file_md5 = md5_of_bytes(content.as_bytes())`
2. `check_exists(cache_dir, md5)` → 键入检查命令 → OCR
3. 命中：`symlink(cache_dir/md5, target)` 返回
4. 未命中：
   - heredoc 写 `{ts}__{md5}.tmp`
   - 键入 `md5sum` 校验命令 → OCR 读结果
   - 通过：`mv tmp md5`，`symlink md5 target`
   - 失败：`rm tmp`，返回错误（可重试）

### 4.3 修改 `src/cli.rs`

- `Args` 新增 `--cache-dir: Option<String>`
- `run_chunked_transfer` 中：当 `cache_dir` 有值时，用 `CacheWriter::write_cached` 替代直接 heredoc 写分片
- 分片的 `target_name` = `{uid_full}.p{idx}`
- `content` = 当前的 `part_content`（注意：需与 `type_text` 实际写入的字节一致，含 wrap 换行）

### 4.4 修改 `src/config.rs`

- 新增 `DEFAULT_CACHE_DIR = "~/.typepaste_cache"`（实际展开由 shell 处理）

### 4.5 修改 `Cargo.toml`

- 可选：添加 `xcap`（截图）、`arboard`（剪贴板备选）
- macOS OCR 走子进程，无需额外 crate

## 5. 实现步骤

1. **OCR 模块**：实现 `screenshot_and_ocr`（macOS Vision + 截图），写单元测试用本地图片验证
2. **缓存写入器**：实现 `check_exists` / `write_tmp` / `verify_md5` / `rename` / `symlink` 各步骤的命令生成
3. **集成分片传输**：在 `run_chunked_transfer` 循环中，有 `cache_dir` 时走 `write_cached`
4. **dry-run 支持**：打印缓存命中/写入决策，不实际键入
5. **降级路径**：OCR 失败 → 当作未命中；md5 校验失败 → 删除 tmp 报错
6. **README 更新**：补充缓存机制说明与 `--cache-dir` 用法

## 6. 风险与应对

| 风险 | 应对 |
|------|------|
| OCR 识别率低，把 MISS 判成 HIT | 标记用强特征串；HIT 需精确匹配 `__TP_HIT__`，模糊则按 MISS 处理（保守=多写，不丢数据） |
| 截图区域不对 | 默认截全屏，代价是 OCR 稍慢但可靠；后续可加区域配置 |
| symlink 在远程不可用 | 检测 `ln -s` 结果，失败降级 `cp` |
| OCR 引入延迟（每片 ~1-2s） | 仅未命中时需 OCR 校验；命中时也需一次 OCR 但跳过了数十秒的键入，净收益大 |
| 缓存目录权限/空间不足 | 首次写入前 `mkdir -p`；写入失败报错提示 |
| 多实例并发写同一缓存 | tmp 文件名含时间戳 `{ts}__{md5}.tmp`，rename 原子，最后写入者胜 |
| 缓存无限增长 | 暂不自动清理；后续加 `--cache-clean` 或按 mtime 淘汰 |

## 7. 验证方案

- **单元测试**：OCR 标记解析、缓存命令生成（不实际键入）
- **dry-run**：验证缓存命中/未命中的决策输出
- **真机自测**：同一文件传两次，第二次应全部分片命中缓存、秒级完成
- **正确性**：缓存命中后还原脚本校验通过、文件还原正确

## 8. 待定问题

1. OCR 后端：macOS Vision（推荐）vs tesseract（跨平台）？—— 建议 macOS 优先，预留 tesseract 后备
2. `--cache-dir` 是否默认开启？—— 建议默认关闭（需显式指定），避免 OCR 依赖影响现有用户
3. symlink 失败是否静默降级 cp？—— 建议降级并打印提示
