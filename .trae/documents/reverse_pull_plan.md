# 反向传输方案：远程桌面 → 本机

## 1. 问题与约束

**当前能力**：typepaste-rs 只能模拟键盘输入（enigo）把数据「键入」远程终端，**无法直接读取**远程终端输出。

**反向需求**：把远程桌面上的文件拷贝到本机。

**核心难点**：如何把远程文件的字节流带回本机？只能通过「远程终端输出 → 本地屏幕捕获 → 解析」这条链路，因为没有 SSH/网络通道。

## 2. 方案对比

| 方案 | 原理 | 优点 | 缺点 |
|------|------|------|------|
| **A. Base16 + OCR + 分片校验** | 远程把文件编码为 base16 分片输出，本地截图+OCR 读取，每片校验 md5，错则重传该片 | 与现有架构一致（复用 base16 编码、md5 校验）；base16 字符集 0-9a-f 最易 OCR；逐片重试保证正确 | 依赖 OCR 准确率；大文件需多次截图 |
| **B. QR 码序列** | 远程把文件编码为 QR 码序列逐帧显示，本地截图解码 | QR 内置纠错（最高 30%），比文本 OCR 鲁棒 | 单帧容量小（~3KB），大文件需几十上百帧；显示时序难控；终端显示 QR 困难 |
| **C. 剪贴板分片** | 远程把编码分片写入剪贴板，本地读取 | 实现简单 | 依赖远程桌面剪贴板双向同步（常不可靠）；覆盖用户剪贴板；剪贴板大小有限 |
| **D. 音频 FSK 调制解调** | 远程播放音频编码数据，本地录音解码 | 不依赖屏幕 | 实现极复杂；远程需扬声器、本地需麦克风；云桌面常无音频 |

**推荐方案 A**：与现有 typepaste 架构对称（正向是本地编码→键入远程，反向是远程编码→OCR 读回本地），复用编码/校验逻辑，base16 字符集对 OCR 最友好。

## 3. 方案 A 详细设计

### 3.1 传输协议

```
本地键入远程命令                    远程终端输出              本地动作
─────────────────────────────────────────────────────────────────────
1. 探测文件信息
   stat -c%s FILE; md5sum FILE  →   SIZE\nMD5\n          → OCR 读 size + 文件 md5

2. 分片输出（循环每片 i）
   clear; printf '%s' "$(sed ...)" →  [base16 分片内容]   → 截图 + OCR
                                      [分片 md5]             校验 md5，错则重传该片

3. 本地重组：拼接所有分片 → base16 解码 → 校验文件 md5
```

### 3.2 编码选择：Base16（十六进制）

**为什么选 base16 而非 base32/base64**：
- 字符集仅 `0-9a-f`，OCR 混淆最少
- 常见 OCR 混淆对在 base16 中大多不存在：`0/O`（无 O）、`1/l`（无 l）、`2/Z`（无 Z）、`5/S`（无 S）
- 唯一风险：`8/b` 混淆 → 由分片 md5 校验兜底，错了重传
- 编码效率：1 字节 → 2 字符（比 base32 的 1→1.6 略低，但 OCR 可靠性优先）

### 3.3 分片与校验

- **分片大小**：根据终端可视区域估算。典型终端 80 列 × 40 行 = 3200 字符 = 1600 字节 base16 数据。保守取每片 **1KB 原始数据**（=2048 hex 字符，约 26 行）。
- **每片格式**（远程输出两行）：
  ```
  <base16_content>
  <md5_of_content>
  ```
- **校验**：本地 OCR 后计算内容 md5，与第二行比对；不符则重新键入命令输出同一分片，重试 N 次。
- **最终校验**：全部分片拼接解码后，用步骤 1 获取的文件 md5 整体校验。

### 3.4 OCR 实现（macOS）

- **截图**：`screencapture -x /tmp/tp_pull.png`（全屏）或指定区域
- **OCR**：macOS **Vision 框架** `VNRecognizeTextRequest`，通过 Swift 子进程调用（原生、免第三方依赖、准确率高）
- **文本提取**：OCR 返回全部文本行，取倒数第二行（内容）和最后一行（md5）
- **OCR 容错**：
  - 内容行只保留 `[0-9a-f]` 字符（过滤 OCR 噪声）
  - md5 行同理（md5 也是 hex）
  - 若过滤后长度异常 → 判定失败 → 重试

### 3.5 远程命令设计

**步骤 1 - 探测**：
```bash
# bash
stat -c%s "FILE" 2>/dev/null; md5sum "FILE" 2>/dev/null | cut -d' ' -f1
```

**步骤 2 - 输出分片 i**（先在远程把文件转 base16 并按行切分）：
```bash
# 一次性准备：base16 编码并按 2048 字符/行切分到临时文件
xxd -p -c 2048 FILE > /tmp/tp_pull.b16
# 或 python3: python3 -c "import sys;print(open(sys.argv[1],'rb').read().hex())" FILE | fold -w 2048

# 逐片输出第 i 片（1-based）+ 其 md5
clear
sed -n 'Ip' /tmp/tp_pull.b16
sed -n 'Ip' /tmp/tp_pull.b16 | md5sum | cut -d' ' -f1
```

**Windows/PowerShell** 类似，用 `Format-Hex` 或 `python3` 编码。

## 4. 模块与文件改动

### 4.1 新增 `src/ocr.rs`（截图 + OCR）

```rust
/// 截取屏幕并 OCR，返回识别的文本行列表。
pub fn screenshot_ocr_lines() -> Result<Vec<String>, String>;

/// 从 OCR 文本行中提取 hex 内容行和 md5 校验行。
pub fn parse_hex_chunk(lines: &[String]) -> Option<(String, String)>;
```

### 4.2 新增 `src/pull.rs`（反向传输主逻辑）

```rust
pub fn run_pull(remote_path: &str, local_out: Option<&Path>, args: &Args, stop: &Arc<AtomicBool>) -> Result<(), String>;
```

内部流程：
1. 键入探测命令 → OCR 读取 size + 文件 md5
2. 键入编码+切分命令（远程准备 base16 分片文件）
3. 循环每片：键入 `sed` 输出命令 → 截图 OCR → 校验 → 重试/收集
4. 本地拼接分片 → base16 解码 → 写本地文件 → 校验 md5

### 4.3 修改 `src/cli.rs`

- `Args` 新增 `--pull: Option<String>`（远程文件路径）
- `main` 中 `--pull` 优先，调用 `run_pull`
- 新增 `--chunk-size`（分片大小，默认 1024）、`--max-retry`（单片最大重试，默认 5）

### 4.4 修改 `Cargo.toml`

- macOS OCR 走子进程（Swift + Vision），无需额外 crate
- 截图用系统 `screencapture`，无需 crate

## 5. 实现步骤

1. **OCR 模块**：实现 `screenshot_ocr_lines`（screencapture + Swift Vision），本地图片测试
2. **hex 分片解析**：从 OCR 行中提取内容+md5，单元测试
3. **探测阶段**：键入 stat+md5sum，OCR 读取文件大小和 md5
4. **分片循环**：逐片键入 sed 命令，OCR 读取，校验，重试
5. **重组与校验**：拼接解码，写文件，md5 总校验
6. **dry-run 支持**：打印将要键入的命令序列，不实际执行
7. **README**：补充 `--pull` 用法

## 6. 风险与应对

| 风险 | 应对 |
|------|------|
| OCR 识别错误 | base16 字符集最小化混淆；每片 md5 校验；失败重试；内容行只保留 hex 字符 |
| 终端输出被命令回显干扰 | 输出前 `clear`；取 OCR 文本最后两行（内容+md5） |
| 终端字体/配色导致 OCR 差 | 提示用户使用等宽字体、高对比度；默认黑色背景浅色文字 |
| 分片 md5 本身 OCR 错误 | md5 也是 hex，容错规则同内容行；若 md5 格式不对直接重试 |
| 大文件传输慢 | 每片约 1KB，1MB ≈ 1000 片，每片含键入+截图+OCR，预计比正向慢（正向纯键入不截图） |
| 远程无 xxd/python3 | 优先 xxd，回退 python3，再回退 `od -An -tx1` |
| 截图区域不对 | 默认全屏截图，Vision 处理整图文本 |

## 7. 验证方案

- **单元测试**：hex 行解析、md5 过滤
- **本地模拟**：在本机终端跑通「编码输出 → 截图 OCR → 校验」链路
- **真机自测**：小文件（几 KB）pull 到本机，md5 比对
- **大文件**：100KB 文件 pull，验证分片重组正确

## 8. 待定问题

1. 是否同时支持目录（远程 tar 打包后 pull，本地解压）？—— 建议先单文件，目录后续加
2. OCR 区域是否需要用户配置（截全屏 vs 指定区域）？—— 先全屏，后续优化
3. 是否需要进度显示？—— 建议加进度条（已传片数/总片数）
