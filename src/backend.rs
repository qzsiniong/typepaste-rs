//! 键盘/鼠标后端：基于 enigo 跨平台单引擎。
//!
//! 输入方式：`Keyboard::key(Key::Unicode(base), Click)` 真实按键 + 显式 Shift
//! press/release，规避云桌面 IME / Unicode 文本输入问题。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use enigo::{Direction, Enigo, Key, Keyboard, Mouse, Settings};

use crate::keymap::{get_key_info, KeyAction};

/// Shift 键 press/release 后等待硬件状态稳定的毫秒数。
///
/// enigo 在 macOS 下 per-event 的 Shift 标志在 `kCGHIDEventTap` 不可靠，真正起作用的
/// 是「硬件 Shift 状态」；Shift 事件发出后需等待其被系统处理，否则字符键早于 Shift
/// 生效 → `>` 变 `.`。复刻原 Python quartz 后端 `time.sleep(0.005)` 经验值。
#[allow(dead_code)]
const SHIFT_SETTLE_MS: u64 = 5;

#[cfg(target_os = "macos")]
fn shift_settle() {
    std::thread::sleep(std::time::Duration::from_millis(SHIFT_SETTLE_MS));
}

#[cfg(not(target_os = "macos"))]
fn shift_settle() {}

/// 输入后端，封装 enigo 实例。持有 stop 标志用于紧急停止。
pub struct Backend {
    enigo: Enigo,
    stop: Arc<AtomicBool>,
}

impl Backend {
    /// 初始化后端。失败返回错误描述（如无图形环境 / 无辅助功能权限）。
    pub fn new(stop: Arc<AtomicBool>) -> Result<Self, String> {
        let enigo = Enigo::new(&Settings::default()).map_err(|e| e.to_string())?;
        Ok(Backend { enigo, stop })
    }

    /// 模拟输入单个字符。
    ///
    /// 开头检查 stop 标志，触发时立即 `exit(130)` 终止程序（紧急停止）。
    /// 查 keymap 得基础字符与是否需 Shift；大写字母 / 特殊符号通过显式 Shift
    /// press/release 实现。未知字符（如非 ASCII）回退 `text()` Unicode 输入。
    pub fn send_char(&mut self, ch: char) {
        if self.stop.load(Ordering::Relaxed) {
            std::process::exit(130);
        }
        match get_key_info(ch) {
            Some(KeyAction::Char {
                #[allow(unused_variables)]
                base,
                shift,
                #[allow(unused_variables)]
                mac_keycode,
            }) => {
                if shift {
                    let _ = self.enigo.key(Key::Shift, Direction::Press);
                    shift_settle();
                }
                #[cfg(target_os = "macos")]
                {
                    // macOS 直接用 raw keycode，绕过 enigo 的 get_layoutdependent_keycode
                    // （后者遍历不 break，小键盘 `.`keycode=65 覆盖主键盘 47，导致 `>`→`.`）
                    let _ = self.enigo.raw(mac_keycode, Direction::Click);
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = self.enigo.key(Key::Unicode(base), Direction::Click);
                }
                if shift {
                    let _ = self.enigo.key(Key::Shift, Direction::Release);
                    shift_settle();
                }
            }
            Some(KeyAction::Return) => {
                let _ = self.enigo.key(Key::Return, Direction::Click);
            }
            Some(KeyAction::Tab) => {
                let _ = self.enigo.key(Key::Tab, Direction::Click);
            }
            None => {
                // 非 ASCII / 未知字符：回退 Unicode 文本输入
                let _ = self.enigo.text(&ch.to_string());
            }
        }
    }

    /// 当前鼠标位置（像素）。失败返回 None。
    pub fn mouse_location(&self) -> Option<(i32, i32)> {
        self.enigo.location().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    #[test]
    fn backend_new_or_skip() {
        // 无图形环境 / CI 下 enigo 初始化可能失败，则跳过。
        match Backend::new(Arc::new(AtomicBool::new(false))) {
            Ok(mut b) => {
                // 仅验证能调用 send_char 不 panic；不验证实际输入。
                b.send_char('a');
                b.send_char('!');
                b.send_char('\n');
                let _ = b.mouse_location();
            }
            Err(_) => {
                eprintln!("skip: enigo 不可用（CI/无图形环境）");
            }
        }
    }
}
