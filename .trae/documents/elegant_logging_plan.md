# 优雅日志输出 实施方案

## 现状调研

当前项目**无任何日志框架**（Cargo.toml 无 `log`/`tracing` 依赖），全靠裸 `println!` / `eprintln!` 输出：

* **99 处** `println!`/`eprintln!` 调用，分布在 6 个文件

* `eprintln!`（30 处）：错误信息、OCR 调试（截图路径、引擎耗时、预处理耗时）、警告

* `println!`（69 处）：用户信息、dry-run 命令、数据管线展示

* indicatif `ProgressBar` **默认输出到 stderr**，而 OCR 过程中的 `eprintln!` 也写 stderr → **进度条被打乱/闪烁/残留**

* **无日志级别控制**：截图路径、OCR 引擎耗时等调试信息无条件输出，正常使用时很吵

* OCR 在多线程中跑，`eprintln!` 输出可能交错

### 核心问题

1. **进度条冲突**：`eprintln!` 直接写 stderr，绕过 indicatif 的绘制机制，进度条花屏
2. **无分级**：调试信息总是输出，无法安静运行
3. **stdout/stderr 职责不清**：用户信息和调试信息混在一起

## 方案选型

不引入 `log`/`tracing` 依赖（对 CLI 工具过重），采用**轻量自研日志层**：

* 全局日志级别（`--verbose` 控制 `debug` 是否可见）

* 全局可选进度条引用（日志在进度条运行时自动用 `pb.println` 输出，避免花屏）

* 4 个宏：`info!` / `debug!` / `warn!` / `error!`

## 修改文件与模块

* `src/utils.rs`：定义 `LogLevel` 枚举、全局级别、全局进度条、日志宏

* `src/cli.rs`：PullArgs / 全局参数加 `verbose` 字段；启动时设置全局日志级别

* `src/pull.rs`：进度条注册到全局；`eprintln!` → 对应级别宏

* `src/ocr.rs`：OCR 过程的 `eprintln!` → `debug!`/`info!`；截图保存路径等改 `debug!`

* `src/failsafe.rs`、`src/backend.rs`：`eprintln!` → `warn!`/`error!`

## 实施步骤

### 1. `utils.rs`：日志基础设施

```rust
/// 日志级别。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel { Error = 0, Warn = 1, Info = 2, Debug = 3 }

static LOG_LEVEL: AtomicU8 = AtomicU8::new(LogLevel::Info as u8);

/// 全局进度条（Some 时日志走 pb.println，否则走 eprintln）。
static GLOBAL_PB: OnceLock<Mutex<Option<ProgressBar>>> = OnceLock::new();

pub fn set_log_level(level: LogLevel) { ... }
pub fn global_pb() -> &'static Mutex<Option<ProgressBar>> { ... }
```

日志宏统一入口函数：

```rust
pub fn log(level: LogLevel, msg: &str) {
    if (level as u8) > LOG_LEVEL.load(Ordering::Relaxed) { return; }
    let prefix = match level { ... };
    let line = format!("{prefix}{msg}");
    if let Some(pb) = global_pb().lock().unwrap().as_ref() {
        pb.println(line);   // indicatif 自动暂停进度条 → 打印 → 恢复
    } else {
        eprintln!("{line}");
    }
}
```

宏定义：

```rust
macro_rules! info  { ($($arg:tt)*) => { crate::utils::log(crate::utils::LogLevel::Info,  &format!($($arg)*)) }; }
macro_rules! debug { ($($arg:tt)*) => { crate::utils::log(crate::utils::LogLevel::Debug, &format!($($arg)*)) }; }
macro_rules! warn  { ($($arg:tt)*) => { crate::utils::log(crate::utils::LogLevel::Warn,  &format!($($arg)*)) }; }
macro_rules! error { ($($arg:tt)*) => { crate::utils::log(crate::utils::LogLevel::Error, &format!($($arg)*)) }; }
```

### 2. 全局进度条注册

`make_pull_progress` / `make_progress_bar` 创建 pb 后注册到全局：

```rust
*global_pb().lock().unwrap() = Some(pb.clone());
```

进度条 `finish()`/`finish_and_clear()` 后清空：

```rust
*global_pb().lock().unwrap() = None;
```

### 3. CLI 加 `--verbose`

`PullArgs` 加 `pub verbose: bool`，clap 定义 `-v/--verbose`。
启动时：`set_log_level(if args.verbose { LogLevel::Debug } else { LogLevel::Info })`。
Push 路径同理加 `verbose`。

### 4. 分级替换现有输出

| 当前                                | 替换为           | 说明              |
| --------------------------------- | ------------- | --------------- |
| `eprintln!("错误：...")`             | `error!`      | 错误              |
| `eprintln!("⚠️ fail-safe ...")`   | `warn!`       | 警告              |
| `eprintln!("截图已保存: ...")`         | `debug!`      | 调试（默认隐藏）        |
| `eprintln!("引擎 X 完成（Yms）")`       | `debug!`      | 调试（默认隐藏）        |
| `eprintln!("预处理图片已保存: ...")`      | `debug!`      | 调试              |
| `eprintln!("  [片 i] 第 N 轮...重试")` | `info!`       | 用户可见的进度         |
| `eprintln!("预处理失败...")`           | `warn!`       | 警告              |
| `println!`（数据管线/dry-run）          | 保持 `println!` | stdout 输出，可管道消费 |

### 5. OCR 线程中的日志

OCR 引擎在 `std::thread::spawn` 中运行。`debug!` 宏内部调用全局 `log()`，线程安全（`AtomicU8` + `Mutex<Option<ProgressBar>>`）。
`pb.println` 内部有锁，多线程调用安全。

## 验证

* `cargo build` / `cargo test`（75 passed）/ `cargo clippy -- -D warnings`

* 手动验证：`--pull` 不带 `-v` 时无调试噪音，进度条不花屏；带 `-v` 时显示 OCR 耗时等

* 确认 stdout（dry-run 等）不受影响

## 风险

* **全局进度条锁竞争**：`pb.println` 内部加锁，高频日志可能有性能影响。但 OCR 本身耗时秒级，日志频率低，可忽略。

* **宏名称冲突**：Rust 无内置 `info!`/`debug!` 宏，但需确认不与依赖（如 `anyhow`）冲突。用 `crate::utils::` 全路径避免歧义。

* **stdout 数据被误改**：dry-run、probe 结果等必须保持 `println!`，不能改为日志宏（否则写到 stderr 破坏管道）。替换时需逐一甄别。

