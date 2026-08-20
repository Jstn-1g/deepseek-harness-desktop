//! 插件升级/卸载（恢复坏版本崩溃，issue #44）。
//!
//! 当已安装插件命中已知坏版本（见 `watch::KNOWN_BROKEN`）导致客户端 boot 整页
//! 崩溃时，桌面端提供「升级到最新版 / 卸载」两个恢复动作：通过
//! `dsh plugin --profile web add|remove <pkg>`（pnpm 转发器）更新或移除依赖，
//! 随后前端触发重启服务，让其他插件正常加载、界面恢复。
//!
//! 与 [`super::install`]（面向预设清单的新装引导）不同，这里面向「已安装插件的
//! 维护」，按包名操作，不涉及预设勾选态。复用 `process::run_plugin_process`
//! 的子进程编排（隐藏控制台 + 输出实时推送）。

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, Manager};

use crate::config;
use crate::service::cli;
use crate::service::workflow;

use super::installed::{profile_dir, PREINSTALL_PROFILE};
use super::process::{run_plugin_process, PreinstallLogPayload, PREINSTALL_LOG_EVENT};

/// 升级插件到最新版：`dsh plugin --profile web add <pkg>`。
pub async fn upgrade(app_handle: &AppHandle, id: &str) -> Result<(), String> {
    run_manage(app_handle, id, "add").await
}

/// 卸载插件：`dsh plugin --profile web remove <pkg>`。
pub async fn remove(app_handle: &AppHandle, id: &str) -> Result<(), String> {
    run_manage(app_handle, id, "remove").await
}

/// 在 profile 目录执行一次 `dsh plugin --profile web <verb> <pkg>`，等待退出。
///
/// 仅环境级失败（shim 缺失、node/dsh 不在盘）或命令非零退出时返回 `Err`；
/// 成功返回后调用方触发服务重启。
async fn run_manage(app_handle: &AppHandle, id: &str, verb: &str) -> Result<(), String> {
    cli::ensure_shims(app_handle)?;

    let node = config::get_node_binary_path(app_handle);
    let dsh_bin = config::get_dsh_binary_path(app_handle);
    if !node.exists() {
        return Err("NODE_NOT_FOUND: Node.js runtime missing".to_string());
    }
    if !dsh_bin.exists() {
        return Err("HARNESS_NOT_FOUND: dsh CLI missing".to_string());
    }

    let window = app_handle
        .get_webview_window("main")
        .ok_or("WINDOW_NOT_FOUND: main window missing")?;

    // 操作前停掉运行中的服务，避免资源冲突（与安装流程一致）
    if workflow::has_owned_process() {
        log::info!("Stopping running harness service before plugin {verb} of {id}");
        if let Err(e) = workflow::stop(app_handle.clone()).await {
            log::warn!("failed to stop harness before plugin {verb}: {e}");
        }
    }

    let bin_dir = cli::get_bin_dir(app_handle);
    let mut envs = HashMap::from([
        (
            "DSH_HOME".to_string(),
            config::get_dsh_data_path(app_handle)
                .to_string_lossy()
                .into_owned(),
        ),
        ("DSH_TELEMETRY_DISABLED".to_string(), "1".to_string()),
        ("NO_COLOR".to_string(), "1".to_string()),
    ]);

    let mut paths = vec![bin_dir];
    if let Some(node_dir) = node.parent() {
        paths.push(node_dir.to_path_buf());
    }
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    if let Ok(joined) = std::env::join_paths(paths) {
        envs.insert("PATH".to_string(), joined.to_string_lossy().into_owned());
    }

    let cwd: PathBuf = profile_dir(app_handle);
    let args = vec![
        dsh_bin.as_os_str().to_os_string(),
        OsString::from("plugin"),
        OsString::from("--profile"),
        OsString::from(PREINSTALL_PROFILE),
        OsString::from(verb),
        OsString::from(id),
    ];

    let _ = window.emit(
        PREINSTALL_LOG_EVENT,
        PreinstallLogPayload {
            line: format!("[dsh] plugin {verb} {id}…"),
        },
    );

    let (code, _captured) = run_plugin_process(&node, &args, &cwd, &envs, &window).await?;
    if code == 0 {
        log::info!("dsh plugin {verb} {id} succeeded");
        return Ok(());
    }

    log::error!("dsh plugin {verb} {id} failed with exit code {code}");
    Err(format!("PLUGIN_{verb}_FAILED: dsh plugin exited with code {code}"))
}
