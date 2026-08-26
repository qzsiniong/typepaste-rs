//! 紧急停止监控：鼠标移至屏幕左上角(<=2px) 触发停止。
//!
//! Ctrl+C 由 `ctrlc` handler 兜底（在 cli 中设置），与这里共同置位共享的 `stop`。
//! 本模块启动 daemon 线程，独立创建 Enigo 读 `location()`，每 50ms 轮询一次。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::backend::Backend;

/// fail-safe 轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// 左上角触发阈值（2px 容差，对齐原 Python 实现）。
const CORNER_TOLERANCE: i32 = 2;

/// 启动 fail-safe daemon 线程：每 50ms 读鼠标位置，左上角触发即置 `stop`。
///
/// Enigo 初始化或读位置失败时打印一次警告并退出线程（Ctrl+C 仍可紧急停止）。
pub fn start_failsafe_monitor(stop: Arc<AtomicBool>) -> JoinHandle<()> {
    thread::Builder::new()
        .name("typepaste-failsafe".into())
        .spawn(move || monitor(stop))
        .expect("spawn failsafe thread")
}

fn monitor(stop: Arc<AtomicBool>) {
    // failsafe 监控用独立 backend 读鼠标位置，不输入字符，无需 stop 触发 exit。
    let stop_never = Arc::new(AtomicBool::new(false));
    let backend = match Backend::new(stop_never) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("⚠️  fail-safe 鼠标监控不可用（{e}），仅 Ctrl+C 可紧急停止");
            return;
        }
    };
    let mut warned = false;
    loop {
        if stop.load(Ordering::Relaxed) {
            std::mem::forget(backend); // 跳过 enigo Drop
            return;
        }
        match backend.mouse_location() {
            Some((x, y)) => {
                if x <= CORNER_TOLERANCE && y <= CORNER_TOLERANCE {
                    stop.store(true, Ordering::Relaxed);
                    eprintln!("\n🛑 紧急停止！鼠标移至左上角，已中止输入。");
                    std::mem::forget(backend); // 跳过 enigo Drop
                    return;
                }
            }
            None => {
                if !warned {
                    eprintln!("⚠️  无法读取鼠标位置，fail-safe 鼠标监控失效（Ctrl+C 仍可用）");
                    warned = true;
                }
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}
