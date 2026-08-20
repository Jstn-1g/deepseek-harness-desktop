//! 取消正在进行的预装插件安装。
//!
//! Windows 下按命令行特征（`plugin --profile web add`）查找由本应用安装目录下
//! node 拉起的进程树并强制结束（`taskkill /T /F`），随后向前端推送
//! `preinstall-cancelled` 事件；非 Windows 平台没有隐藏控制台争用问题，直接忽略。

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

#[cfg(windows)]
use std::process::{Command, Stdio};

#[cfg(windows)]
use crate::config;

/// 前端监听“安装已取消”事件名
const PREINSTALL_CANCEL_EVENT: &str = "preinstall-cancelled";

/// 取消事件载荷（预留扩展字段）
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreinstallCancelPayload {}

/// 取消正在进行的预装插件安装
pub async fn cancel(app_handle: &AppHandle) {
    // 先置位全局取消标记：安装循环在下一个插件前退出，不再调度剩余插件；
    // Windows 下随后强杀当前进程树。
    super::install::mark_cancelled();

    if !cfg!(windows) {
        return;
    }

    let Some(window) = app_handle.get_webview_window("main") else {
        return;
    };

    #[cfg(windows)]
    {
        let base = config::get_dsh_install_path(app_handle)
            .to_string_lossy()
            .replace('\\', "\\\\");
        let ps_cmd = format!(
            "Get-CimInstance Win32_Process -Filter \"Name='node.exe'\" | Where-Object {{ ($_.CommandLine -like '*plugin*--profile*web*add*') -and ($_.ExecutablePath -like '{base}\\*') }} | ForEach-Object {{ taskkill /PID $_.ProcessId /T /F 2>$null }}"
        );

        let mut cmd = Command::new("powershell");
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &ps_cmd,
        ]);
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());

        if let Err(e) = cmd.output() {
            log::warn!("failed to run preinstall cancel: {e}");
        }
    }

    let _ = window.emit(PREINSTALL_CANCEL_EVENT, PreinstallCancelPayload {});
}