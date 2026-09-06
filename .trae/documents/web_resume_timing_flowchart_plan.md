# Web 传输断点续传 + 耗时统计 + README 流程图 实施计划

## 需求概述

1. **断点续传**：receiver.html 将每分片落盘为独立文件（`{name}.p1`/`.p2`…），PROBE 时扫描目录中已存在的 `.p*` 文件，反馈位图给发送端，发送端跳过已存在的分片（落盘前已做 md5 校验，PROBE 无需再校验）。
2. **耗时统计**：接收端、发送端分别统计传输耗时并展示。
3. **README 流程图**：为 web 传输模式补充 mermaid 流程图。

## 仓库调研结论

### 现有 web 传输协议（`src/web.rs` + `assets/receiver.html`）

* 帧格式：`seq|total|raw_size|zflag|md5Chunk|b64[|name_b64]`，seq 0-based，STX(`{`/Ctrl+B)/ETX(`}`/Ctrl+C) 定界。

* `prepare()` 当前**逐片独立 zstd 压缩**，payload md5 = `md5_of_bytes(payload)`。

* 接收端 `handleFrame` 把所有片顺序写入**同一个** `FileSystemWritableFileStream`（单文件），`expectedSeq` 强制顺序。

* 反馈 JSON：`{"seq":i,"status":"...","md5":"...","err":"..."}`，`qr.rs` 解析为 `Feedback`。

* 现有 `--part-size` 分片模式（`run_chunked_transfer`）采用 `{uid}.p{n}` 每片独立文件，md5 校验后落盘，全部传完后批量校验+合并——本计划的 web 续传沿用此「每片独立文件」思路。

### 关键约束

* 通信：发送端→浏览器 靠键盘；浏览器→发送端 靠二维码（容量足够放下位图）。

* 每片落盘前已校验 `md5Chunk`，故 `.pN` 文件存在即等价于该片正确——PROBE 只需查存在性。

* `FileSystemAccess` 可列举目录文件、按文件名读写，适合 `.pN` 独立文件方案。

## 设计：prepare() 改为整体压缩后分片

### 新 prepare() 流程

```
raw = read(file)
compressed = zstd.compress(raw)        // 整体压缩
if compressed.len() < raw.len() * 0.95:
    payload_bytes = compressed; zflag = 1
else:
    payload_bytes = raw; zflag = 0
shards = split(payload_bytes, chunk_bytes)   // 按 chunk_bytes 切分
for each shard:
    md5 = md5_of_bytes(shard)
    b64 = base64(shard)
    frame body = "{seq}|{total}|{raw_size}|{zflag}|{md5}|{b64}[|{name_b64}]"
```

* `raw_size` = 原始文件字节数（接收端解压后校验用）。

* `md5Chunk` = 该片 payload（压缩流片段或原始片段）的 md5。

* 整体压缩比逐片压缩率更高，且接收端只需一次解压。

## 设计：接收端每分片独立落盘

### 数据帧处理（receiver.html `handleFrame`）

* `payload = b64ToBytes(b64)`；校验 `md5Hex(payload) === md5Chunk`，失败 RETRY。

* **不再逐片 zstd 解压**（压缩是整体的）。直接把 `payload` 写入 `{name}.p{seq+1}`（1-based，与 `run_chunked_transfer` 一致）。

* 维护 `received: Set<number>`（已成功落盘的 seq）。

* ACK：`feedback('OK', seq, md5Chunk, '')`。

* 当 `received.size === total` 时：

  1. 按 seq 顺序读取 `.p1`..`.pN`，拼接为 `bytes`。
  2. 若 `zflag=1`：`final = fzstd.decompress(bytes)`；否则 `final = bytes`。
  3. 校验 `final.length === raw_size`（可选）。
  4. 计算 `fullMd5 = md5Hex(final)`。
  5. 写入最终文件 `{name}`（覆盖）。
  6. `feedback('FINISH', total-1, fullMd5, '')`。

* 去掉 `expectedSeq` 顺序约束：接受任意 seq∈\[0,total)，写对应 `.p{seq+1}`。

## 设计：断点续传协议（PROBE）

### PROBE 帧

发送端在数据帧之前发送一个 PROBE 帧（seq = -1）：

```
-1|total|raw_size|zflag|name_b64|file_md5
```

* `file_md5` = 原始文件 md5（`prep.file_md5`），用于接收端判断完整文件是否已存在且正确。

* 不再携带分片 md5 清单（PROBE 不做分片 md5 校验）。

### 接收端 PROBE 处理

1. 解析 total / raw\_size / zflag / name / file\_md5。
2. 若目录中存在 `{name}`（完整文件）：读取并算 md5，若 == file\_md5 → 直接 `feedback('FINISH', total-1, file_md5, '')`（已完成）。
3. 否则扫描目录中 `{name}.p{N}` 文件：

   * 解析 N（1-based），设位图 bit `N-1 = 1`。

   * 位图转 hex 字符串（bit i 对应 seq i；byte0=bits0-7，低位在前）。
4. `feedback('PROBE', -1, bitmask_hex, '')`。

   * `md5` 字段承载位图 hex（PROBE 无真实 md5）。

### 发送端 PROBE 处理

* 新增 `Status::Probe`（qr.rs 解析 `"PROBE"`）。

* `await_probe` 轮询二维码：

  * `status=FINISH` → 整文件已在远端，比对 `fb.md5 == prep.file_md5` 后结束。

  * `status=PROBE` → 解析 `fb.md5` 为位图，得到 `present: Set<usize>`。

* 主循环遍历 `seq in 0..total`，跳过 `present` 中的 seq，只发送缺失分片（按 seq 升序）。

* 全部发完后等待 FINISH，校验整文件 md5。

### 位图容量

N 片 → ceil(N/8) 字节 → 2×ceil(N/8) hex 字符。N=10000 → 2500 hex 字符，QR v40-L 可容纳 \~7089 字节，绰绰有余。

## 协议流程图

### 发送端整体流程

```mermaid
flowchart TD
    Start([run_web]) --> Prep[prepare:<br/>整体 zstd 压缩 → 按 chunk 分片]
    Prep --> Ready[wait_ready 等待 READY]
    Ready --> ProbeSend[发送 PROBE 帧<br/>-1|total|raw_size|zflag|name_b64|file_md5]
    ProbeSend --> ProbeAck{await_probe}
    ProbeAck -->|FINISH| FullMatch{fb.md5 == prep.file_md5?}
    FullMatch -->|是| Done([完成 ✓])
    FullMatch -->|否| Err1([整文件 md5 不一致])
    ProbeAck -->|PROBE| ParseMask[解析位图 present 集合]
    ParseMask --> Loop{遍历 seq 0..total-1}
    Loop -->|seq in present| Skip[跳过该片]
    Loop -->|seq missing| Send[发送数据帧<br/>seq|total|raw_size|zflag|md5Chunk|b64]
    Skip --> Loop
    Send --> Ack{await_ack}
    Ack -->|OK| Next[继续下一片]
    Ack -->|RETRY| Retry[重传当前片]
    Ack -->|PAUSE| Pause[等待聚焦]
    Next --> Loop
    Retry --> Send
    Pause --> Send
    Loop -->|全部处理完| FinishWait[等待 FINISH]
    FinishWait --> Md5Check{fb.md5 == prep.file_md5?}
    Md5Check -->|是| Done
    Md5Check -->|否| Err2([整文件 md5 不一致])
```

### 接收端流程

```mermaid
flowchart TD
    Recv([收到帧]) --> IsProbe{seq == -1?}
    IsProbe -->|是| Probe[解析 total/raw_size/zflag/<br/>name/file_md5]
    Probe --> HasFull{完整文件 {name} 存在<br/>且 md5 匹配?}
    HasFull -->|是| FbFinish[反馈 FINISH]
    HasFull -->|否| Scan[扫描 {name}.p* 文件]
    Scan --> Mask[构建位图 bitmask_hex]
    Mask --> FbProbe[反馈 PROBE md5=bitmask_hex]
    IsProbe -->|否| Data[校验 md5Chunk]
    Data -->|失败| FbRetry[反馈 RETRY]
    Data -->|成功| Write[写入 {name}.p{seq+1}<br/>received.add seq]
    Write --> HasAll{received.size == total?}
    HasAll -->|否| FbOk[反馈 OK seq]
    HasAll -->|是| Concat[拼接 p1..pN]
    Concat --> Dec{zflag==1?}
    Dec -->|是| Decompress[zstd 解压]
    Dec -->|否| Raw[直接用]
    Decompress --> WriteFinal[写入 {name} + 算 md5]
    Raw --> WriteFinal
    WriteFinal --> FbFinish2[反馈 FINISH md5=fullMd5]
```

## 设计：耗时统计

### 接收端（receiver.html）

* `transferStart = Date.now()`：在首片成功落盘（received 从 0 变 1）时记录。

* FINISH 时 `elapsed = Date.now() - transferStart`，格式化输出到状态栏与数据面板：`传输完成 · 耗时 X.Xs`。

* 每片 ACK 时状态栏可选刷新「已用 X.Xs · 已收 N/total」。

### 发送端（web.rs）

* `wait_ready` 成功后 `let t0 = Instant::now()`。

* 最终完成时 `info!` 输出：`传输完成，耗时 {:.2}s（{total} 片，{raw_size} 字节，{:.1} KB/s）`。

* dry-run 不统计。

## 涉及文件与模块

| 文件                     | 变更                                                                                                               |
| ---------------------- | ---------------------------------------------------------------------------------------------------------------- |
| `src/web.rs`           | `prepare()` 改整体压缩后分片；新增 `build_probe_body` / `await_probe`；`run_web` 按位图跳过已存在分片；耗时统计；`Prepared` 加 `file_md5`（已有） |
| `assets/receiver.html` | `handleFrame` 识别 seq=-1 PROBE；每片写独立 `.p{seq+1}` 文件；全部到齐后拼接+解压+写最终文件+FINISH；去掉 expectedSeq 顺序约束；耗时统计              |
| `src/qr.rs`            | `Status` 加 `Probe`；`parse` 识别 `"PROBE"`；测试                                                                       |
| `README.md`            | 新增 web 传输模式流程图（mermaid），含整体压缩、分片文件、PROBE 续传                                                                      |

## 实施步骤（依赖顺序）

1. **qr.rs**：加 `Status::Probe` + 解析 + 单测。
2. **web.rs** **`prepare`**：改为整体 zstd 压缩后分片；`raw_size` 保持原始大小；帧格式不变。
3. **web.rs PROBE**：`build_probe_body` 拼 `-1|total|raw_size|zflag|name_b64|file_md5`；`await_probe` 处理 FINISH/PROBE，解析位图。
4. **web.rs** **`run_web`**：`wait_ready` 后发 PROBE；按位图跳过已存在分片；末片等 FINISH 校验 md5；加 `t0` 耗时统计。
5. **receiver.html**：PROBE 分支（完整文件校验 / 扫描 `.p*` 位图）；数据帧写 `.p{seq+1}`；`received` Set；全部到齐拼接+解压+写最终文件+FINISH；耗时统计。
6. **README.md**：新增 web 传输流程图章节。
7. **验证**：`cargo test` / `clippy` / dry-run。

## 验证

* `cargo test`：qr PROBE 解析；prepare 整体压缩+分片往返；帧格式；新增 PROBE 帧格式测试。

* `cargo clippy --all-targets` 无告警。

* dry-run：输出 PROBE 帧预览、续传位图跳过说明。

* 真机（可选）：传大文件中断后重跑，观察发送端「跳过已存在分片」、接收端从断点续写并最终拼接成功。

## 风险与处理

* **PROBE 位图过大**：N 极大时位图仍紧凑（N=10000 仅 2500 hex 字符），QR 可容纳。

* **完整文件存在但 md5 不符**：视为过期，忽略，回退到扫描 `.p*` 分片。

* **`.p*`** **文件残留（上次失败片）**：落盘前已 md5 校验，存在即正确；若写入中断产生空/残文件，下次 PROBE 会把它算入 present 但实际数据错误——缓解：写入用临时名 `{name}.p{N}.tmp`，md5 校验通过后原子 rename 为 `{name}.p{N}`。

* **整体压缩后分片不可独立解压**：必须全部到齐才能解压，与现有 FINISH 机制一致。

* **二维码** **`md5`** **字段承载位图**：与正常 OK/FINISH 的 md5 语义不冲突（PROBE 状态独用）。

