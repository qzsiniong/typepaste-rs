# 用 leptess (Tesseract) 替换 macOS Vision OCR

## 背景与结论

当前 OCR 通过内联 Swift 脚本调用 macOS Vision 框架，存在两个痛点：

1. Vision 无法限制字符集，`6→ó`、`O→0`、`l→1` 等混淆只能靠后处理 `hex_confusion` 兜底，仍有误识别漏网；
2. 依赖 `swift` 进程 + CoreImage 预处理，每次 OCR 都要 fork 一个 Swift 解释器，开销大。

**目标**：用 `leptess`（Tesseract + Leptonica 的 Rust 绑定）重写 `ocr_image`，利用 Tesseract 的 `tessedit_char_whitelist` 强制只输出 `0-9a-f`，从根本上消除字符混淆；图像预处理改用 `image` crate（灰度 + 对比度 + 放大），全程纯 Rust 无外部进程。

### 关键调研结论

* `leptess = "0.14.0"`（crates.io 最新），需系统安装 `tesseract` + `leptonica`（macOS: `brew install tesseract leptonica pkg-config`）。

* 核心 API：

  * `leptess::LepTess::new(data_path: Option<&str>, lang: &str) -> Result<LepTess>`

  * `lt.set_image(path) -> Result<()>`

  * `lt.set_variable(leptess::Variable::TesseditCharWhitelist, "0123456789abcdef")` —— 字符白名单，只允许 hex 字符输出

  * `lt.set_variable(leptess::Variable::TesseditPagesegMode, "6")` —— PSM 6，假设单一文本块（适合终端多行文本）

  * `lt.get_utf8_text() -> Result<String>`

  * `lt.set_source_resolution(72)` —— 抑制截图 0 DPI 警告

* 预处理：`image = "0.25"` crate 提供 `grayscale()`、`adjust_contrast()`、`resize()`，纯 Rust 实现。

* `select_region()`（Swift 全屏遮罩选区）和 `screenshot()`（screencapture）**不变**，它们不是 OCR，仍需 macOS。

* `hex_confusion` / `normalize_hex` 作为安全网保留（白名单已大幅降低混淆，兜底无害）。

## 改动文件

### 1. `Cargo.toml`

新增依赖：

```toml
leptess = "0.14"
image = "0.25"
```

### 2. `src/ocr.rs`

* **删除** Swift 版 `ocr_image()`（内联 Vision + CoreImage 那段）。

* **新增** leptess 版 `ocr_image(path)`：

  1. 用 `image` crate 打开截图 → `grayscale()` → `adjust_contrast(2.5)` → `resize(2x, Nearest)`，保存到临时 PNG。
  2. 用 `std::sync::OnceLock<Mutex<LepTess>>` 懒加载 Tesseract 实例（避免每片重复初始化）：

     * `new(None, "eng")`

     * `set_variable(TesseditCharWhitelist, "0123456789abcdef")`

     * `set_variable(TesseditPagesegMode, "6")`
  3. `set_image(预处理后的临时图)` → `set_source_resolution(72)` → `get_utf8_text()`。
  4. 清理临时预处理文件，返回文本。

* `screenshot_ocr_lines`、`screenshot`、`frontmost_window_id`、`select_region` **保持不变**（仍 `#[cfg(target_os = "macos")]`）。

* `ocr_image` 可去掉 `#[cfg(target_os = "macos")]`（leptess 跨平台），但调用方 `screenshot_ocr_lines` 仍是 macOS-only，不影响。

* `hex_confusion`、`normalize_hex`、`normalize_digits`、`parse_hex_chunk`、`parse_file_info` 及测试**全部保留**。

### 3. `README.md`

* 更新"反向传输"章节的 OCR 说明：Vision → Tesseract (leptess)，强调字符白名单。

* 新增前置依赖：`brew install tesseract leptonica pkg-config`（macOS）。

* 如 Tesseract 未安装，给出明确报错提示。

## 风险与处理

| 风险                                         | 处理                                                                                 |
| ------------------------------------------ | ---------------------------------------------------------------------------------- |
| 系统未装 tesseract/leptonica，编译失败              | README 注明 `brew install tesseract leptonica pkg-config`；`LepTess::new` 失败时返回中文错误提示 |
| `adjust_contrast` 参数语义与 CoreImage 不同导致效果差异 | 先用 2.5，效果不佳时调参（0.0=全灰，1.0=原图，>1.0=增强）                                              |
| Tesseract 初始化慢（每片 \~100ms）                 | 用 `OnceLock<Mutex<LepTess>>` 全局只初始化一次                                              |
| tessdata 路径找不到（`new(None, "eng")` 失败）      | 错误信息提示检查 `tesseract --list-langs` 是否有 eng                                          |
| PSM 6 对多行折行文本分割不佳                          | 实测不行则改 PSM 3（全自动）或 11（稀疏文本）                                                        |
| `image` crate 增加编译时间                       | 可接受；后续可改用 leptonica 原生预处理去掉该依赖                                                     |

## 验证步骤

1. `cargo build` 编译通过（需已装 tesseract/leptonica）。
2. `cargo test` 现有 74 个测试全过（解析逻辑未变）。
3. `cargo clippy` 无新增警告。
4. 实机 `--pull --pull-region` 跑一个小文件，确认 OCR 识别不再出现 `ó/O/l` 混淆，md5 校验通过率提升。

