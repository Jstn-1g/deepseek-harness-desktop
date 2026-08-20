use crate::config;
use crate::service::cli;
use crate::service::download::{self, Installable};
use crate::service::plugin;
use crate::service::update;
use crate::service::workflow;
use tauri::AppHandle;
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_opener::OpenerExt;

/// 按当前设置同步命令行集成（shim + PATH 注册）。
///
/// 安装/更新流程的收尾步骤，失败只记日志、不阻断主流程。
fn sync_cli_link(app_handle: &AppHandle) {
    let setting = config::get_store_dat_setting(app_handle);
    let result = if setting.cli_link_enabled {
        cli::ensure(app_handle)
    } else {
        cli::remove(app_handle)
    };
    if let Err(e) = result {
        log::warn!("cli link sync failed: {e}");
    }
}

/// 一键安装依赖（Node.js 运行时 + 打包的 Harness 发行版）
///
/// 返回是否真正执行了安装/更新：`true` 表示本次调用落盘了运行时（前端
/// 需重启服务以加载新版本），`false` 表示未发生任何安装（已是最新、记录
/// 自愈，或 GitHub 限流无法校验完整性而保持本地安装——此时前端不应重启、
/// 也不应丢弃“有新版本”提示，而应提示稍后重试）。
///
/// 启动逻辑由前端显式调用 `launch_harness` 完成，避免重复拉起进程。
#[tauri::command]
pub async fn install_dependencies(app_handle: AppHandle) -> Result<bool, String> {
    if workflow::status::get_status() == workflow::status::Status::Installing {
        log::info!("Installation process already running, skipping");
        return Ok(false);
    }

    // 以实际安装状态为准：本地安装与 GitHub 最新 release 的 commit hash
    // 不一致时，说明上游 pkg 有更新/修复，需要自动重新下载。
    let node_ok = download::Nodejs.check_installed(&app_handle);
    let dsh_files_ok = download::Dsh.check_installed(&app_handle);
    // pnpm 是 dsh plugin 子命令的运行时依赖（v0.3.0 起随环境安装）；老版本
    // 升级后 `installed` 已为 true 会跳过环境安装，捆绑 pnpm 可能从未落盘，
    // 需一并纳入"已就绪"判定，缺失时由 workflow::install 按任务补齐。
    let pnpm_ok = download::Pnpm.check_installed(&app_handle);

    // 启动自愈捷径：记录显示未安装、但运行时文件已全部在盘。常见于桌面端自更新
    // 安装器强杀进程，或上次启动时核心文件短暂缺失被 workflow::start 复位
    // `installed`（一旦复位，此后每次启动都会走进安装分支）。此时直接补记
    // installed 收尾：不做联网核对、绝不整包重下——联网核对可能把「记录滞后」
    // 误判为真更新，而重下整目录在 Windows 上极易破坏 node_modules（历史 issue：
    // 重解压后启动报找不到 @deepseek-ai/dsh-client-ui-settings）。真更新一律由
    // 启动后的 check_dsh_update 提示用户手动安装，启动路径不该自行下载。
    if node_ok && dsh_files_ok && pnpm_ok {
        let setting = config::get_store_dat_setting(&app_handle);
        if !setting.installed {
            log::info!(
                "Runtime files already present although store says not installed, healing installed flag"
            );
            let mut setting = config::get_store_dat_setting(&app_handle);
            setting.installed = true;
            config::set_store_dat_setting(&app_handle, setting);
            sync_cli_link(&app_handle);
            return Ok(false);
        }
    }

    let dsh_latest = download::fetch_latest_dsh_pkg_info().await;

    // 已安装文件在盘时，用 resolve_update 甄别「记录滞后」与「真更新」：
    // 记录滞后（HealUpToDate）只修正 store 记录、绝不整包重下。否则会把一个
    // 可用的 node_modules 整目录删除重解压，Windows 上原生模块 DLL 锁/重解压
    // 很容易留下破损安装，导致启动报找不到 @deepseek-ai/dsh-client-ui-settings
    // 或 HARNESS_NOT_FOUND。仅在真更新（UpdateAvailable）时才允许重新下载。
    let dsh_need_install = match &dsh_latest {
        Ok(latest) if dsh_files_ok => {
            let record_commit = config::get_dsh_pkg_commit(&app_handle);
            let record_tag = config::get_dsh_pkg_tag(&app_handle);
            let installed_version = config::get_dsh_version(&app_handle);
            // 老记录没有 tag，反查 pkg 仓库 tags 列表确认记录对应的发布版本；
            // 反查失败时由 resolve_update 回退到“以实际文件为准”的保守分支
            let legacy_tags = if record_tag.is_none() {
                download::fetch_dsh_pkg_tags().await.unwrap_or_default()
            } else {
                Vec::new()
            };
            match download::resolve_update(
                record_commit.as_deref(),
                record_tag.as_deref(),
                installed_version.as_deref(),
                latest,
                &legacy_tags,
            ) {
                // 安装文件已是最新 release，只是记录滞后：修正记录后下次
                // 启动直接走 commit 快速比对，不再误判、也绝不整包重下
                download::UpdateCheck::UpToDate
                | download::UpdateCheck::HealUpToDate => {
                    if record_commit.as_deref() != Some(latest.commit.as_str()) {
                        log::info!(
                            "Installed Harness files already at latest release, healing stale record: {} ({})",
                            latest.tag,
                            latest.commit
                        );
                        config::set_dsh_pkg_commit(&app_handle, latest.commit.clone());
                        config::set_dsh_pkg_tag(&app_handle, latest.tag.clone());
                    }
                    false
                }
                download::UpdateCheck::UpdateAvailable => {
                    // 有新版但 GitHub API 限流拿不到可信源码摘要时，不自动整包重下
                    // （无法校验完整性，Windows 上重解压还易损坏 node_modules）。
                    // 保持本地安装，更新提示由启动后的 check_dsh_update 给出，稍后可重试。
                    if latest.digest.is_none() {
                        log::warn!(
                            "New dsh release {} found but trusted digest unavailable (API rate-limited), keeping local install",
                            latest.tag
                        );
                        false
                    } else {
                        true
                    }
                }
            }
        }
        // 核心文件缺失（首次安装或目录被清空）→ 需要安装
        Ok(_) => true,
        Err(e) => {
            // 网络不可用或 GitHub API 限流时保留本地安装，不阻塞启动
            log::warn!(
                "Failed to check latest dsh release info, keeping local install: {}",
                e
            );
            !dsh_files_ok
        }
    };

    if node_ok && !dsh_need_install && pnpm_ok {
        log::info!("Dependencies already installed and up to date, skipping installation");
        let mut setting = config::get_store_dat_setting(&app_handle);
        if !setting.installed {
            setting.installed = true;
            config::set_store_dat_setting(&app_handle, setting);
        }
        sync_cli_link(&app_handle);
        return Ok(false);
    }

    log::info!("Dependencies missing or outdated, starting installation process");
    workflow::status::set_status(workflow::status::Status::Installing);
    workflow::status::emit_status(&app_handle);
    // 返回 dsh 是否真正落盘更新：仅重装 Node/pnpm 或全部任务被跳过（例如
    // 版本相同仅记录滞后）时为 false，前端据此决定是否重启页面/保留更新提示
    let updated = workflow::install(&app_handle, dsh_latest.ok()).await?;
    log::debug!("Installation completed, marked as installed");
    let mut setting = config::get_store_dat_setting(&app_handle);
    setting.installed = true;
    config::set_store_dat_setting(&app_handle, setting);
    sync_cli_link(&app_handle);
    Ok(updated)
}

/// 静默检查是否有新版 Harness 可用（只查不装，供进入页面后后台调用）
///
/// 以“实际安装文件”为准核对，而不是只看本地记录：记录可能因安装时 API
/// 失败或外围途径更新而滞后于文件，此时修正记录并免打扰；同版本热修
/// （版本相同但 commit 不同）仍正常提示。
#[tauri::command]
pub async fn check_dsh_update(
    app_handle: AppHandle,
) -> Result<Option<download::LatestDshPkg>, String> {
    // 本地没有安装时无需提示更新
    let dsh_files_ok = download::Dsh.check_installed(&app_handle);
    if !dsh_files_ok {
        return Ok(None);
    }

    let latest = download::fetch_latest_dsh_pkg_info().await?;
    let record_commit = config::get_dsh_pkg_commit(&app_handle);
    let record_tag = config::get_dsh_pkg_tag(&app_handle);
    let installed_version = config::get_dsh_version(&app_handle);

    // 老记录没有 tag，反查 pkg 仓库 tags 列表确认记录对应的发布版本；
    // 反查失败时由 resolve_update 回退到“以实际文件为准”的保守分支
    let legacy_tags = if record_tag.is_none() {
        download::fetch_dsh_pkg_tags().await.unwrap_or_default()
    } else {
        Vec::new()
    };

    match download::resolve_update(
        record_commit.as_deref(),
        record_tag.as_deref(),
        installed_version.as_deref(),
        &latest,
        &legacy_tags,
    ) {
        download::UpdateCheck::UpToDate => Ok(None),
        download::UpdateCheck::UpdateAvailable => Ok(Some(latest)),
        download::UpdateCheck::HealUpToDate => {
            // 安装文件已是最新 release，只是记录滞后：修正记录后下次启动
            // 直接走 commit 比对快速路径，不再误报
            log::info!(
                "Installed Harness files already at latest release, healing stale record: {} ({})",
                latest.tag,
                latest.commit
            );
            config::set_dsh_pkg_commit(&app_handle, latest.commit.clone());
            config::set_dsh_pkg_tag(&app_handle, latest.tag.clone());
            Ok(None)
        }
    }
}

/// 启动 Harness 服务
#[tauri::command]
pub async fn launch_harness(app_handle: AppHandle) -> Result<(), String> {
    workflow::launch(app_handle).await
}

/// 停止 Harness 服务
#[tauri::command]
pub async fn shutdown_harness(app_handle: AppHandle) -> Result<(), String> {
    workflow::stop(app_handle).await
}

/// 重启 Harness 服务
#[tauri::command]
pub async fn restart_harness(app_handle: AppHandle) -> Result<(), String> {
    workflow::restart(app_handle).await
}

/// 获取当前 Harness 服务状态
#[tauri::command]
pub fn get_dsh_status() -> workflow::status::Status {
    workflow::status::get_status()
}

/// 获取预装插件列表（含已安装检测结果），首次启动引导界面渲染用
#[tauri::command]
pub async fn get_preinstall_plugins(
    app_handle: AppHandle,
) -> Result<Vec<plugin::PreinstallPlugin>, String> {
    Ok(plugin::list(&app_handle))
}

/// 逐个安装选中的预装插件（`dsh plugin --profile web add <spec>`），
/// 进程输出实时通过 `preinstall-log` 事件推送，行内状态通过
/// `preinstall-plugin-status` 事件推送；返回每个插件的独立结果（单个插件
/// 失败不阻断其余插件）。无环境级错误（含部分失败）时标记引导完成并记录
/// 预设指纹；用户取消或环境准备失败时返回 `Err` 不标记。
#[tauri::command]
pub async fn install_preinstall_plugins(
    app_handle: AppHandle,
    ids: Vec<String>,
) -> Result<Vec<plugin::PreinstallResult>, String> {
    let results = plugin::install(&app_handle, &ids).await?;
    let mut setting = config::get_store_dat_setting(&app_handle);
    setting.preinstall_done = true;
    if let Some(hash) = plugin::current_preset_hash(&app_handle) {
        setting.preset_hash = Some(hash);
    }
    config::set_store_dat_setting(&app_handle, setting);
    Ok(results)
}

/// 取消正在进行的预装插件安装（网络抖动/限流卡住时用户点“取消”）。
#[tauri::command]
pub async fn cancel_preinstall_plugins(app_handle: AppHandle) {
    plugin::cancel(&app_handle).await;
}

/// 跳过预装插件引导：记录状态与预设指纹，之后不再弹出（除非清单内容变更）
#[tauri::command]
pub async fn skip_preinstall_plugins(app_handle: AppHandle) -> Result<(), String> {
    let mut setting = config::get_store_dat_setting(&app_handle);
    setting.preinstall_done = true;
    if let Some(hash) = plugin::current_preset_hash(&app_handle) {
        setting.preset_hash = Some(hash);
    }
    config::set_store_dat_setting(&app_handle, setting);
    Ok(())
}

/// 是否有新的预装插件需要引导：预设清单内容与上次记录不一致（或老用户无基线）。
/// 资源文件每次安装都被强制覆盖不可比对，只能比对 app-data 里记录的内容指纹。
#[tauri::command]
pub fn get_preinstall_pending(app_handle: AppHandle) -> Result<bool, String> {
    Ok(plugin::preinstall_pending(&app_handle))
}

/// 在系统浏览器中打开预装插件的仓库地址（仅允许预装清单内的 id）
#[tauri::command]
pub async fn open_preinstall_repo(app_handle: AppHandle, id: String) -> Result<(), String> {
    let url = plugin::repo_url_of(&app_handle, &id)
        .ok_or_else(|| format!("PREINSTALL_INVALID_ID: {id}"))?;
    app_handle
        .opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

/// 当前 profile 已安装插件列表（含解析后的元信息），`use-dsh-plugins` 首次加载用；
/// 之后 Rust 侧监控插件文件，变化时通过 `dsh-plugins-updated` 事件实时推送。
#[tauri::command]
pub fn get_dsh_plugins(app_handle: AppHandle) -> Vec<plugin::DshPlugin> {
    plugin::watch::list(&app_handle)
}

/// 升级已安装插件到最新版（恢复坏版本崩溃，issue #44）：`dsh plugin --profile web add <id>`。
/// 成功后调用方触发服务重启以加载新 bundle。
#[tauri::command]
pub async fn upgrade_dsh_plugin(app_handle: AppHandle, id: String) -> Result<(), String> {
    plugin::upgrade(&app_handle, &id).await
}

/// 卸载已安装插件（恢复坏版本崩溃，issue #44）：`dsh plugin --profile web remove <id>`。
/// 成功后调用方触发服务重启以让其余插件正常加载。
#[tauri::command]
pub async fn remove_dsh_plugin(app_handle: AppHandle, id: String) -> Result<(), String> {
    plugin::remove(&app_handle, &id).await
}


/// 健康检查（通过 Rust 代理，避免 WebView CORS 问题）
#[tauri::command]
pub async fn proxy_health_check(app_handle: AppHandle) -> Result<String, String> {
    let port = config::get_store_dat_setting(&app_handle).port;
    workflow::proxy_health_check(port).await
}

/// 运行时/版本/诊断信息（侧边栏展示）
#[tauri::command]
pub async fn get_runtime_info(app_handle: AppHandle) -> Result<config::RuntimeInfo, String> {
    let port = config::get_store_dat_setting(&app_handle).port;
    Ok(config::runtime_info(&app_handle, port))
}

/// 运行时文件是否已全部在盘（Node / Dsh / pnpm 三件套，纯本地检查、无网络）。
///
/// 判定条件与 `install_dependencies` 的「启动自愈」捷径完全一致：桌面端自更新
/// （MSI 强杀进程）后 store 可能被复位或损坏显示「未安装」，但运行时文件其实
/// 已就绪——此时前端跳过安装/下载界面，交给 install_dependencies 内部自愈
/// 补记 installed 后直接启动，避免自动重开时闪现误导用户的安装界面。
#[tauri::command]
pub fn runtime_ready(app_handle: AppHandle) -> bool {
    download::Nodejs.check_installed(&app_handle)
        && download::Dsh.check_installed(&app_handle)
        && download::Pnpm.check_installed(&app_handle)
}

/// 当前桌面端配置
#[tauri::command]
pub async fn get_app_config(app_handle: AppHandle) -> Result<config::Setting, String> {
    Ok(config::get_store_dat_setting(&app_handle))
}

/// 更新桌面端配置
#[tauri::command]
pub async fn update_app_config(
    app_handle: AppHandle,
    port: Option<u16>,
    auto_start: Option<bool>,
    cli_link_enabled: Option<bool>,
) -> Result<config::Setting, String> {
    let mut setting = config::get_store_dat_setting(&app_handle);
    if let Some(port) = port {
        if port == 0 {
            return Err("port must be a positive number".to_string());
        }
        setting.port = port;
    }
    if let Some(auto_start) = auto_start {
        setting.auto_start = auto_start;
    }
    // 命令行集成：先执行文件系统/PATH 操作，成功后再持久化开关，
    // 失败时配置保持不变，避免"开关已开但 shim 未生成"的不一致状态。
    if let Some(enabled) = cli_link_enabled {
        if enabled {
            cli::ensure(&app_handle)?;
        } else {
            cli::remove(&app_handle)?;
        }
        setting.cli_link_enabled = enabled;
    }
    config::set_store_dat_setting(&app_handle, setting.clone());
    Ok(setting)
}

/// 命令行集成状态（shim 文件与 PATH 注册情况）
#[tauri::command]
pub fn get_cli_link_status(app_handle: AppHandle) -> Result<cli::CliLinkStatus, String> {
    Ok(cli::get_status(&app_handle))
}

/// 在系统浏览器中打开 Harness 界面
#[tauri::command]
pub async fn open_in_browser(app_handle: AppHandle) -> Result<(), String> {
    let url = config::get_dsh_service_url(config::get_store_dat_setting(&app_handle).port);
    app_handle
        .opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

/// 复制 Harness 服务地址到剪贴板
#[tauri::command]
pub async fn copy_service_url(app_handle: AppHandle) -> Result<(), String> {
    let url = config::get_dsh_service_url(config::get_store_dat_setting(&app_handle).port);
    app_handle
        .clipboard()
        .write_text(url)
        .map_err(|e| e.to_string())
}

/// 在系统文件管理器中定位指定文件（Session 日志下载完成后的"在文件夹中显示"）
#[tauri::command]
pub fn reveal_in_folder(path: String) -> Result<(), String> {
    tauri_plugin_opener::reveal_item_in_dir(&path)
        .map_err(|e| format!("REVEAL_FAILED: {e}"))
}

/// 在系统文件管理器中打开数据目录（官方 $DSH_HOME，即 ~/.dsh）
#[tauri::command]
pub async fn reveal_data_dir(app_handle: AppHandle) -> Result<(), String> {
    let dsh_home = config::get_dsh_data_path(&app_handle);
    // 目录可能尚未创建（全新安装），先建好再打开，避免资源管理器报路径不存在
    std::fs::create_dir_all(&dsh_home).map_err(|e| e.to_string())?;

    if cfg!(windows) {
        std::process::Command::new("explorer")
            .arg(&dsh_home)
            .spawn()
            .map_err(|e| e.to_string())?;
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open")
            .arg(&dsh_home)
            .spawn()
            .map_err(|e| e.to_string())?;
    } else {
        std::process::Command::new("xdg-open")
            .arg(&dsh_home)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 读取 dsh 服务日志
#[tauri::command]
pub async fn read_service_logs(
    app_handle: AppHandle,
    max_bytes: Option<usize>,
) -> Result<String, String> {
    let log_path = config::get_service_log_path(&app_handle);
    if !log_path.exists() {
        return Ok(String::new());
    }

    let content = std::fs::read_to_string(&log_path).map_err(|e| e.to_string())?;
    let max_bytes = max_bytes.unwrap_or(64 * 1024);
    if content.len() <= max_bytes {
        Ok(content)
    } else {
        Ok(content[content.len() - max_bytes..].to_string())
    }
}

/// 清空 dsh 服务日志
#[tauri::command]
pub async fn clear_service_logs(app_handle: AppHandle) -> Result<(), String> {
    let log_path = config::get_service_log_path(&app_handle);
    std::fs::write(&log_path, "").map_err(|e| e.to_string())
}

/// 读取运行日志（DSH 服务日志 + 桌面端 Rust 运行日志），格式化为便于
/// 反馈/报障复制的纯文本块：`### 环境信息`、`### 服务日志` 与 `### 运行日志` 三段。
///
/// 服务日志来自 `logs/dsh-web.log`，运行日志来自 `logs/desktop.log`
/// （桌面端自身 `logger::init` 每次启动落盘，见 logger/mod.rs）。
/// 每段取末尾最多 `MAX_LINES` 行，避免粘贴内容超出 GitHub issue 长度上限。
#[tauri::command]
pub async fn read_run_logs(app_handle: AppHandle) -> Result<String, String> {
    const MAX_LINES: usize = 100;

    let base = config::get_base_dir(&app_handle);
    let service = config::get_service_log_path(&app_handle);
    let desktop = base.join("logs").join("desktop.log");

    let read_tail = |path: &std::path::Path| -> String {
        if !path.exists() {
            return String::new();
        }
        let content = std::fs::read_to_string(path).unwrap_or_default();
        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(MAX_LINES);
        lines[start..].join("\n")
    };

    // 环境信息：桌面端应用版本、dsh 发行版本、Node 版本与系统平台/架构，便于报障时快速定位环境差异
    let dsh_version = config::get_dsh_version(&app_handle)
        .map(|v| format!("dsh: {v}\n"))
        .unwrap_or_default();
    let env_text = format!(
        "app: {}\n{}node: {}\nos: {} ({})",
        app_handle.package_info().version,
        dsh_version,
        config::get_active_node_version(),
        std::env::consts::OS,
        std::env::consts::ARCH,
    );

    let service_text = read_tail(&service);
    let desktop_text = read_tail(&desktop);

    Ok(format!(
        "### 环境信息\n\n{}\n\n### 服务日志\n\n```\n{}\n```\n\n### 运行日志\n\n```\n{}\n```",
        env_text,
        service_text.trim_end(),
        desktop_text.trim_end()
    ))
}

/// 保存界面语言偏好
#[tauri::command]
pub fn set_language(app_handle: AppHandle, lang: String) {
    let mut setting = config::get_store_dat_setting(&app_handle);
    setting.language = lang.clone();
    config::set_store_dat_setting(&app_handle, setting);
    config::i18n::set_language(match lang.as_str() {
        "en" | "en-US" => config::i18n::Lang::En,
        _ => config::i18n::Lang::Zh,
    });
}

/// 切换侧边栏（布局状态保存在前端，保留该命令以对齐参考实现）
#[tauri::command]
pub async fn toggle_sidebar() -> Result<bool, String> {
    Ok(true)
}

/// 当前 dsh 主题偏好（light/dark/system），用于让桌面外壳跟随内嵌页面主题
#[tauri::command]
pub fn get_dsh_theme(app_handle: AppHandle) -> config::DshTheme {
    config::get_dsh_theme(&app_handle)
}

/// 检查桌面端自身是否有新版本（含安装包是否已下载）
#[tauri::command]
pub async fn check_desktop_update(
    app_handle: AppHandle,
) -> Result<Option<update::DesktopUpdateInfo>, String> {
    update::check(&app_handle).await
}

/// 下载桌面端新版本安装包；已下载则直接返回。进度通过 `desktop-update-progress` 事件推送
#[tauri::command]
pub async fn download_desktop_update(
    app_handle: AppHandle,
) -> Result<update::DesktopUpdateInfo, String> {
    update::download(&app_handle).await
}

/// 打开已下载的桌面端安装包（exe/msi/dmg...，交给系统默认处理器）
#[tauri::command]
pub async fn open_desktop_installer(app_handle: AppHandle, path: String) -> Result<(), String> {
    update::open_installer(&app_handle, path).await
}

/// 关于对话框信息（版本 / 发布时间 / 版权 / 仓库）
#[tauri::command]
pub async fn get_desktop_about() -> Result<update::DesktopAboutInfo, String> {
    Ok(update::about().await)
}

/// 在系统浏览器中打开任意 http(s) 链接（更新说明 / 关于对话框仓库链接等）
#[tauri::command]
pub async fn open_external_url(app_handle: AppHandle, url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!("EXTERNAL_URL_INVALID: {url}"));
    }
    app_handle
        .opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}
