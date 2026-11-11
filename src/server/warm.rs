//! 预热 shell 池：提前启动若干空闲的 PowerShell，请求到来时取一个执行，用完即弃并在后台补充。
//!
//! 每条命令仍在全新进程中运行（不共享 cd / 变量 / 环境），只是省掉 PowerShell 的冷启动时间。
//! 池为空、shell 类型不匹配或预热进程已失效时，调用方退回冷启动。

use crate::platform::{self, Shell};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

pub struct WarmPool {
    /// 预热的 shell 类型；None 表示不预热（非 PowerShell 或配置为 0）
    shell: Option<Shell>,
    size: usize,
    idle: Mutex<Vec<Child>>,
    filling: AtomicBool,
}

impl WarmPool {
    pub fn new(shell: Shell, size: usize) -> Arc<Self> {
        let enabled = size > 0 && shell.is_powershell();
        Arc::new(Self {
            shell: enabled.then_some(shell),
            size: if enabled { size } else { 0 },
            idle: Mutex::new(Vec::new()),
            filling: AtomicBool::new(false),
        })
    }

    pub fn shell(&self) -> Option<Shell> {
        self.shell
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn idle_count(&self) -> usize {
        self.idle.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// 取一个存活的预热 shell；无论是否取到，都触发后台补充。
    pub fn take(self: &Arc<Self>, shell: Shell) -> Option<Child> {
        if self.shell != Some(shell) {
            return None;
        }
        let got = {
            let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
            let mut got = None;
            while let Some(mut c) = idle.pop() {
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

    /// 后台补满到 size。同一时刻只有一个补充线程；启动失败即停止，等下次 take 再试。
    pub fn refill(self: &Arc<Self>) {
        let Some(shell) = self.shell else { return };
        if self.filling.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = Arc::clone(self);
        thread::spawn(move || loop {
            while me.idle_count() < me.size {
                match spawn_warm(shell) {
                    Some(c) => me.idle.lock().unwrap_or_else(|e| e.into_inner()).push(c),
                    None => {
                        me.filling.store(false, Ordering::SeqCst);
                        return;
                    }
                }
            }
            me.filling.store(false, Ordering::SeqCst);
            // 退出前复查：期间若有 take 发生且未能抢到补充权，由本线程继续补
            if me.idle_count() >= me.size || me.filling.swap(true, Ordering::SeqCst) {
                return;
            }
        });
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
