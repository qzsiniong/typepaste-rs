# enigo Drop 累积阻塞问题与修复

## 现象

正常模式（非 dry-run）输入完成后，程序卡住不退出，需手动 Ctrl+C 才能结束。

- dry-run 模式正常退出（退出码 0）
- 正常模式能看到 `✅ 已写入...` 提示
- failsafe 监控线程和 ctrlc handler 都已注释，仍卡住
- 在 `run_auto_mode` 返回前加 debug 可打印，但 main 末尾的 `exit(0)` 之前卡住

## 根因

enigo 0.3 在 macOS 上的 `Drop::drop` 实现中有一段累积 sleep 逻辑：

```rust
// enigo-0.3.0/src/macos/macos_impl.rs#L1101
impl Drop for Enigo {
    fn drop(&mut self) {
        if self.release_keys_when_dropped {
            // 释放按住的键...
        }

        // DO NOT REMOVE THE SLEEP
        // This sleep is needed because all events that have not been
        // processed until this point would just get ignored when the
        // struct is dropped
        self.update_wait_time();
        thread::sleep(self.last_event.1.saturating_sub(Duration::from_millis(20)));
    }
}
```

`last_event.1` 是累积等待时间，由 `update_wait_time` 维护：

```rust
fn update_wait_time(&mut self) {
    let now = Instant::now();
    let wait_time = self
        .last_event
        .1
        .saturating_sub(self.last_event.0.elapsed())
        + Duration::from_millis(20);
    self.last_event = (now, wait_time);
}
```

初始 `last_event.1 = Duration::from_secs(0)`。每次按键调用 `update_wait_time` 后：
- `wait_time = max(0, prev_wait - elapsed) + 20ms`

当输入间隔（interval）大于 20ms 时，`elapsed > prev_wait`，`saturating_sub` 归零，`wait_time = 20ms`，不累积。
当输入间隔小于 20ms 或批量快速输入时，`elapsed < prev_wait`，`wait_time` 持续累积。

大量字符（如 base32 编码后的几 KB）快速输入后，`last_event.1` 累积到数秒甚至更大值，Drop 时的 `thread::sleep` 阻塞进程，导致卡住。

## 执行路径

1. `run_auto_mode`（或其他 run_* 函数）输入完成
2. 打印 `✅ 已写入...`
3. 函数返回 `Ok(())`
4. **函数局部变量 `backend`（Enigo 实例）被 drop** ← enigo Drop 执行，`thread::sleep` 阻塞
5. main 接收返回值，走到 `std::process::exit(0)` ← 永远到不了

`std::process::exit(0)` 本可绕过析构，但它在 Drop 之后才执行，无法生效。

## 修复

在三个 run_* 函数返回前，用 `std::mem::forget(backend)` 跳过 enigo 的 Drop：

```rust
// src/cli.rs
fn run_auto_mode(...) -> Result<(), String> {
    let mut backend = prepare_input(stop)?;
    // ... 输入逻辑 ...
    std::mem::forget(backend); // 跳过 enigo Drop（其 Drop 中 thread::sleep 会累积阻塞）
    Ok(())
}
```

同样应用于 `run_raw_transfer`、`run_deploy` 和 `failsafe::monitor`。

## 为什么 `mem::forget` 安全

- 进程即将 `std::process::exit(0)` 退出，无需释放资源
- enigo Drop 的 `release_keys_when_dropped` 默认 false（`Settings::default()`），跳过 Drop 不会导致按键卡住
- Drop 中的 sleep 是 enigo 为保证事件被系统处理而加的，但进程退出时无需保证

## 涉及文件

- [src/cli.rs](file:///Users/qzs/code/labs/auto-type/typepaste-rs/src/cli.rs) — 三个 run_* 函数
- [src/failsafe.rs](file:///Users/qzs/code/labs/auto-type/typepaste-rs/src/failsafe.rs) — monitor 函数的两个 return 点
