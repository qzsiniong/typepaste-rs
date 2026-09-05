# VDI 网页接收端全自动传输 实施方案

## 需求概述

按设计文档实现全新「本机 → VDI 浏览器」全自动文件传输通道：

- **正向**：Rust 端 zstd 压缩 → 分片 → 协议帧 → 物理键盘模拟（Ctrl 组合键做帧定界）
- **反向**：VDI 网页将状态 JSON 渲染为固定位置二维码，本机截屏解码二维码驱动 ARQ 状态机
- **网页端**：`receiver.html` 单文件、零依赖、离线可用；全局 keydown 监听（无需输入框）；FileSystem API 直接落盘；失焦 PAUSE / 聚焦 READY

## 现状调研结论

| 现有能力 | 位置 | 复用方式 |
|---------|------|---------|
| 物理键盘模拟（enigo，含 Shift press/release、macOS raw keycode） | [backend.rs](file:///Users/qzs/code/labs/typepaste-rs/src/backend.rs) | 扩展：新增 Ctrl 组合键发送（STX/ETX/RESET） |
| 截屏（screencapture，支持区域 -R） | [ocr.rs](file:///Users/qzs/code/labs/typepaste-rs/src/ocr.rs#L176) `screenshot()` | 改为 `pub(crate)` 供二维码解码复用 |
| 交互式框选区域 | `ocr::select_region` | 直接复用，框选二维码区域 |
| 脚本 include_str! 嵌入 + 部署模式 | [restore_script.rs](file:///Users/qzs/code/labs/typepaste-rs/src/restore_script.rs#L42)、`--deploy-script` | receiver.html 同样 include_str! 嵌入，新增 `--deploy-receiver` |
| base64/md5 | encoder.rs、utils.rs | 帧 payload base64、分片 md5 |
| gzip 压缩 + 压缩率启发式 | cli.rs `gzip_compress`、`GZIP_USE_RATIO` | zstd 沿用同款「压缩无收益则跳过」策略 |
| 逐字符输入 + indicatif 进度条 | utils.rs `type_text` | 帧 payload 输入复用 |
| 日志宏 info!/debug!/warn!/error! + 全局进度条 | utils.rs | 新模块直接使用 |
| clap CLI 分发 | cli.rs `main()` | 新增 `--web <file>` 模式分支 |

**新增依赖**：
- `zstd = "0.13"`（压缩，C 源码捆绑编译，无系统依赖）
- `rqrr = "0.7"`（纯 Rust 二维码解码，配合现有 image 0.25；实现时验证 API，备选 `bardecoder`）

## 协议定稿（与文档对齐 + 两处工程化补充）

帧定界：**STX = Ctrl+B**、**ETX = Ctrl+C**、**RESET = Ctrl+A**（真实修饰键 press/release 发送；JS keydown 检测 `e.ctrlKey && e.key==='b'/'c'/'a'` 并 `preventDefault()`）。

帧体（`|` 分隔）：

```
seq|total|rawSize|zstdFlag|md5Chunk|<base64 payload>[|<base64 filename>   # 仅 seq=0 带第 7 字段文件名]
```

- `seq` 0-based；`total` 分片总数；`rawSize` 解压后文件总字节数；`zstdFlag` 1=payload 为 zstd 流 / 0=原始字节
- `md5Chunk` = **payload 字节**（base64 解码后、解压前）的 md5
- 补充 1：文件名通过 seq=0 帧第 7 字段传递（文档帧格式缺文件名，FileSystem 落盘必需）
- 补充 2：可打印帧定界回退 `--web-printable-frame`：用 `{` / `}` 替代 STX/ETX（base64 与头部字符集不含 `{}`），JS 端同时接受两种定界，防 VDI 客户端劫持 Ctrl 组合键

反馈二维码 JSON（与文档一致）：

```json
{"seq":0,"status":"OK","md5":"<分片或全文件md5>","err":""}
```

status：`READY`（页面就绪/聚焦空闲）、`OK`（分片 md5 校验通过）、`RETRY`（校验失败/请求重传）、`PAUSE`（窗口失焦）、`FINISH`（全部完成，md5=全文件 md5）、`ERROR`（致命错误，err 带原因）。

## 文件与模块

- **`assets/receiver.html`**（新建）：单文件网页接收端，内联 qrcode 生成库（qrcode-generator，MIT）、fzstd（纯 JS zstd 解压，MIT）、精简 md5 实现；零外部请求
- **`src/web.rs`**（新建）：VDI 模式主逻辑——压缩分片、帧组装、ARQ 状态机、焦点轮询、进度条
- **`src/qr.rs`**（新建）：截屏 → 灰度 → rqrr 解码 → JSON 解析（带超时轮询）
- **`src/backend.rs`**：新增 `send_stx()` / `send_etx()` / `send_reset()`（Key::Control press/release + b/c/a click）
- **`src/ocr.rs`**：`screenshot()` 改 `pub(crate)`
- **`src/cli.rs`**：新增参数与 `run_web_mode()`；新增 `--deploy-receiver [path]`
- **`src/main.rs`**：`mod web; mod qr;`
- **`Cargo.toml`**：加 zstd、rqrr

## 实施步骤

### 1. receiver.html（网页端）

1. 页面结构：大字号状态行 + 角落固定尺寸二维码 DOM（安静区留白、纠错级别 M）+ 「选择保存目录」按钮
2. 内联三个迷你库（下载 minified 源码内嵌，注释标注来源/License）：
   - qrcode-generator（渲染反馈二维码）
   - fzstd（zstd 解压）
   - md5（约 4KB 精简实现，分片与全文件校验）
3. 全局 `keydown` 监听（window，capture，preventDefault）：
   - Ctrl+B → 帧开始（重置当前帧缓冲）；Ctrl+C → 帧结束（提交解析）；Ctrl+A → RESET（清空缓冲）
   - 可打印模式：`{` → 帧开始，`}` → 帧结束
   - 其余 `e.key.length===1` 的可打印字符追加进帧缓冲
4. 帧解析：`split('|')` → base64 解码 payload（atob → Uint8Array）→ md5 比对：
   - 失败 → QR `RETRY`
   - 成功 → zstdFlag 则 fzstd 解压得原始分片；seq=0 时解码第 7 字段得文件名，`dirHandle.getFileHandle(name,{create:true})` → `createWritable()`；按序 `writer.write(chunk)`
   - 累计全文件 md5（增量 update）
   - `seq+1===total` → writer.close() → QR `FINISH`（md5=全文件 md5）；否则 QR `OK`
5. 焦点：`window.blur` → QR `PAUSE`；`focus` → QR `READY`；未授权目录时 QR `ERROR`（err 提示授权）
6. 每次状态变化重绘二维码 + 状态行；状态含序号防陈旧

### 2. backend.rs：Ctrl 组合键

```rust
pub fn send_stx(&mut self)   { self.ctrl_click('b'); }   // 帧开始
pub fn send_etx(&mut self)   { self.ctrl_click('c'); }   // 帧结束
pub fn send_reset(&mut self) { self.ctrl_click('a'); }   // 清空远端缓冲
fn ctrl_click(&mut self, key: char) {
    self.enigo.key(Key::Control, Direction::Press);
    // 复用 get_key_info 的物理键点击（小写字母无 Shift）
    self.enigo.key(Key::Unicode(key), Direction::Click); // 非 macOS；macOS 用 raw keycode
    self.enigo.key(Key::Control, Direction::Release);
}
```
macOS 走 `raw(mac_keycode)`（小写字母 keymap 已有 keycode）。

### 3. src/qr.rs：二维码截屏解码

- `read_feedback(region: Option<Region>) -> Result<Option<Feedback>, String>`：截屏到临时文件 → `image::open` → `to_luma8()` → `rqrr::Detector`/`PreparedImage` 检测解码 → serde 风格手工解析 JSON（不引 serde，字段极少，用字符串匹配提取 seq/status/md5/err）
- `wait_feedback(region, timeout, interval, stop) -> Result<Feedback, String>`：轮询直到解出二维码或超时（PAUSE 由上层处理）
- `Feedback { seq: i64, status: Status, md5: String, err: String }`，`Status` 枚举 READY/OK/RETRY/PAUSE/FINISH/ERROR

### 4. src/web.rs：发送端状态机

1. **准备**：读文件 → zstd level 3 压缩；压缩后 ≥ 原始 ×0.95 则放弃压缩（zstdFlag=0，沿用 GZIP_USE_RATIO 思路）→ 按 `--web-chunk`（默认 4096）分片 → 每片 base64
2. **帧组装**：`format!("{seq}|{total}|{raw_size}|{zflag}|{md5}|{b64}")`，seq=0 追加 `|{b64(filename)}`
3. **前置等待**：轮询二维码直到 `READY`（提示用户：打开 receiver.html、授权目录、点击浏览器窗口）
4. **ARQ 循环**（每片）：
   - `send_reset()`（Ctrl+A 清脏缓冲）→ 发送 STX → type_text 输入帧体（带进度条）→ ETX
   - 等待反馈（默认 2s 起轮询，上限 ~30s）：
     - `OK` 且 seq/md5 匹配 → 下一片
     - `RETRY` / seq 滞后 / 超时无二维码 → 重试该片（≤ max_retry 次），重试前 reset
     - `PAUSE` → 不清状态，持续轮询直到 READY，然后 reset + 重发当前帧（失焦期间按键丢失由重发恢复）
     - `FINISH` → 比对全文件 md5 与本地 md5，一致则成功
     - `ERROR` → 中止并打印 err
5. **可打印定界模式**：`--web-printable-frame` 时发 `{`/`}` 字符替代 Ctrl 组合
6. dry-run：打印帧序列与状态机流程，不输入

### 5. cli.rs / main.rs 接线

- 新参数：
  - `--web <FILE>`：网页接收模式（与 --pull/--deploy-script 互斥）
  - `--web-chunk <size>`：分片字节数，默认 4096（复用 parse_size）
  - `--web-region`：交互式框选二维码区域（复用 select_region；不指定则截前台窗口）
  - `--web-printable-frame`：用 `{}` 定界回退
- `--deploy-receiver [PATH]`：把内嵌 receiver.html 写到本地文件（默认 `./receiver.html`），供一次性送入 VDI
- main 分发分支调用 `web::run_web(...)`

## 验证

1. `cargo build` / `cargo test`（现有 75 测试不回归）/ `cargo clippy -- -D warnings`
2. 新增单元测试：帧组装/解析往返、分片切分与 total 计算、md5 一致性、Feedback JSON 解析、zstd 压缩/跳过启发式
3. **本地端到端手测**（核心验证）：
   - `cargo run -- --deploy-receiver /tmp/receiver.html`
   - 本机浏览器打开 /tmp/receiver.html，授权目录，框选二维码区域
   - `cargo run -- --web <测试文件> --web-region -v`，验证：文件落地、md5 一致、进度条正常
   - 测试文本文件（走 zstd）与已压缩文件（跳过压缩）
   - 测试中途切走窗口 → PAUSE → 切回 → 自动恢复续传
   - 测试 `--web-printable-frame` 模式
4. receiver.html 离线校验：断网打开页面功能完整（库全部内联）

## 风险与应对

- **VDI 客户端劫持 Ctrl 组合键**（如 Ctrl+C 被客户端映射）：`--web-printable-frame` 回退（`{}` 定界，JS 同时接受两种）
- **rqrr 与 image 0.25 API 兼容**：实现时先写最小解码例子验证；不兼容则换 `bardecoder`
- **二维码识别率（VDI 画面压缩）**：二维码大尺寸渲染 + 纠错 M + 安静区；解码失败轮询重试；区域框选避开屏幕缩放
- **FileSystem API 兼容性**：仅 Chromium 系支持；页面检测 `showDirectoryPicker` 不存在时 QR 报 ERROR 提示
- **失焦期间按键丢失**：不依赖中途暂停，失焦后整帧 reset + 重发（ARQ 天然覆盖）
- **zstd crate 编译**：捆绑 libzstd C 源码，需本机 C 编译器（macOS 自带 clang，与 leptess/ocrs 构建要求相同）
- **receiver.html 体积**（内联库约 30-40KB）：一次性部署，不影响传输协议
