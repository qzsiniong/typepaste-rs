# OCR 多引擎支持方案（修订版 v2）

## 需求（最终）

1. ~~添加命令行参数选择引擎~~ → **不要命令行参数**，内部自动轮换
2. 每轮**截图一次**后，**依次尝试所有（引擎×宽度）组合**直到成功；所有组合都失败才算本轮失败
3. 本轮失败后，**sleep 递减时间**，重新截图进入下一轮

## 引擎列表

1. **Tesseract (leptess)** — 当前实现
2. **ocrs** — 纯 Rust OCR，ONNX 模型自动下载，仅拉丁字符（适合 hex）
3. **Tesseract CLI** — shell 调用 `tesseract` 命令，无需 leptess 链接（兜底）

## 核心流程

```
for round in 1..=max_retry:
    if round > 1: sleep(retry_wait_ms(round))   // 递减等待
    screenshot → image_path                       // 本轮截图一次
    for (engine, width) in COMBOS:                // 依次尝试所有组合
        text = ocr_image(image_path, width, engine)
        if check(lines):                           // md5 校验通过
            return success
    // 所有组合失败，进入下一轮（重新截图）
```

## 改动文件

1. **Cargo.toml** — 新增 `ocrs = "0.9"`
2. **src/ocr.rs** — 多引擎 + 组合迭代 + 递减等待序列
3. **src/pull.rs** — 调用方式调整

## 实现步骤

### 1. Cargo.toml

```toml
ocrs = "0.9"
```

### 2. src/ocr.rs

#### 2.1 引擎枚举与组合列表

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OcrEngine {
    Tesseract,
    Ocrs,
    TesseractCli,
}

/// 尝试顺序：先在常用宽度(1000)试遍所有引擎，再换宽度。
const OCR_COMBOS: &[(OcrEngine, u32)] = &[
    (OcrEngine::Tesseract, 1000),
    (OcrEngine::Ocrs, 1000),
    (OcrEngine::TesseractCli, 1000),
    (OcrEngine::Tesseract, 900),
    (OcrEngine::Ocrs, 900),
    (OcrEngine::TesseractCli, 900),
    (OcrEngine::Tesseract, 1100),
    (OcrEngine::Ocrs, 1100),
    (OcrEngine::TesseractCli, 1100),
    (OcrEngine::Tesseract, 1200),
    (OcrEngine::Ocrs, 1200),
    (OcrEngine::TesseractCli, 1200),
    (OcrEngine::Tesseract, 1500),
    (OcrEngine::Ocrs, 1500),
    (OcrEngine::TesseractCli, 1500),
];
```

#### 2.2 统一 OCR 接口

```rust
fn ocr_image(path: &Path, target_width: u32, engine: OcrEngine) -> Result<String, String>
```

* **Tesseract**：复用现有 `tesseract_instance()` + leptess

* **Ocrs**：`ocrs_instance()` 全局复用，`allowed_chars="0123456789abcdef"`，`get_text()`

* **TesseractCli**：`Command::new("tesseract").args([path, "stdout", "--psm", "4", "-c", "tessedit_char_whitelist=0123456789abcdef"])`

#### 2.3 截图 + 组合尝试的对外接口

```rust
/// 截图一次，依次用所有（引擎×宽度）组合 OCR，
/// `check` 返回 true 的第一组结果行被返回。
/// 全部失败返回 Ok(None)，由调用方决定是否重新截图重试。
pub fn screenshot_ocr_with_check<F>(
    region: Option<Region>,
    check: F,
) -> Result<Option<Vec<String>>, String>
where F: Fn(&[String]) -> bool
```

内部：截图 → 遍历 `OCR_COMBOS` → 对每个组合 `ocr_image` → 调 `check` → 成功则返回。

#### 2.4 递减等待序列

```rust
/// 每轮（所有组合失败后）的等待时间，递减。
pub const OCR_RETRY_WAITS_MS: &[u64] = &[1000, 800, 600, 500, 400, 300, 250, 200, 200, 200];

pub fn retry_wait_ms(attempt: usize) -> u64 {
    OCR_RETRY_WAITS_MS[(attempt - 1).min(OCR_RETRY_WAITS_MS.len() - 1)]
}
```

#### 2.5 保留旧 `screenshot_ocr_lines` 兼容

可删除或改为内部调用新接口（传一个恒 false 的 check 取第一组）。直接删除更干净，pull.rs 改用新接口。

### 3. src/pull.rs

#### 3.1 read\_chunk\_with\_retry

```rust
type_command(&cmd, args.interval, stop);
std::thread::sleep(Duration::from_secs_f64(1.0));
for round in 1..=args.max_retry {
    if round > 1 {
        std::thread::sleep(Duration::from_millis(retry_wait_ms(round)));
    }
    let check = |lines: &[String]| {
        parse_hex_chunk(lines)
            .map(|(hex, md5)| md5_of_bytes(hex.as_bytes()) == md5)
            .unwrap_or(false)
    };
    match screenshot_ocr_with_check(args.region, check)? {
        Some(lines) => {
            let (hex, _) = parse_hex_chunk(&lines).unwrap();
            return Ok(hex);
        }
        None => {
            eprintln!("  [片 {i}] 第 {round} 轮所有 OCR 组合均失败，重试...");
        }
    }
}
```

#### 3.2 probe\_file\_info 同理

check 闭包改为 `parse_file_info(lines).is_some()`。

## 风险与处理

1. **ocrs 模型下载**：首次自动下载到 `~/.cache/ocrs`，需网络。无网络时该引擎 Err，跳过继续下一个组合。
2. **Tesseract CLI 不可用**：Err，跳过。
3. **组合数量**：15 组（3引擎×5宽度），全失败耗时约 3-10s/轮，可接受。
4. **后处理兼容**：所有引擎输出统一走 `normalize_hex` / `parse_hex_chunk`。

## 验证

1. `cargo build` 通过
2. `cargo test` 全过
3. `cargo clippy` 无新增警告

