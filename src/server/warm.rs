//! 预热 shell 池：提前启动若干空闲的 PowerShell，请求到来时取一个执行，用完即弃并在后台补充。
//!
//! 每条命令仍在全新进程中运行（不共享 cd / 变量 / 环境），只是省掉 PowerShell 的冷启动时间。
//! 池为空、shell 类型不匹配或预热进程已失效时，调用方退回冷启动。

use crate::platform::{self, Shell};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

pub struct WarmPool {
    /// 预热的 shell 类型；None 表示不预热（非 PowerShell 或配置为 0）
    shell: Option<Shell>,
    size: usize,
    state: Mutex<State>,
}

/// 空闲进程与正在启动中的数量放在同一把锁里，计算缺口时不会重复补充。
#[derive(Default)]
struct State {
    idle: Vec<Child>,
    starting: usize,
}

impl WarmPool {
    pub fn new(shell: Shell, size: usize) -> Arc<Self> {
        let enabled = size > 0 && shell.is_powershell();
        Arc::new(Self {
            shell: enabled.then_some(shell),
            size: if enabled { size } else { 0 },
            state: Mutex::new(State::default()),
        })
    }

    pub fn shell(&self) -> Option<Shell> {
        self.shell
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn idle_count(&self) -> usize {
        self.lock().idle.len()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 取一个存活的预热 shell；无论是否取到，都触发后台补充。
    pub fn take(self: &Arc<Self>, shell: Shell) -> Option<Child> {
        if self.shell != Some(shell) {
            return None;
        }
        let got = {
            let mut st = self.lock();
            let mut got = None;
            while let Some(mut c) = st.idle.pop() {
                if matches!(c.try_wait(), Ok(None)) {
                    got = Some(c);
                    break;
                }
                let _ = c.wait();
            }
            got
        };
        self.refill();
        got
    }

    /// 后台补满到 size：缺几个就同时启动几个（多个会话同时取空池子时，尽快全部补回）。
    /// 启动失败的不重试，等下次 take 再补。
    pub fn refill(self: &Arc<Self>) {
        let Some(shell) = self.shell else { return };
        let missing = {
            let mut st = self.lock();
            let missing = self.size.saturating_sub(st.idle.len() + st.starting);
            st.starting += missing;
            missing
        };
        for _ in 0..missing {
            let me = Arc::clone(self);
            thread::spawn(move || {
                let child = spawn_warm(shell);
                let mut st = me.lock();
                st.starting -= 1;
                if let Some(c) = child {
                    st.idle.push(c);
                }
            });
        }
    }
}

fn spawn_warm(shell: Shell) -> Option<Child> {
    let mut c = platform::build_warm_command(shell)?;
    super::exec::apply_base_env(&mut c);
    c.current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()
}
