//! 预装插件安装：校验选中项、准备环境（pnpm/dsh shim、按需补齐捆绑 pnpm、
//! 停止运行中的服务），随后**逐个**调用 `dsh plugin --profile web add <spec>`，
//! 单个插件失败不阻断其余插件（issue #45）；全部结束后执行 Windows 极简模式
//! 专项修复。每个插件返回独立结果（id/成功/失败原因），前端可据此展示行内
//! 状态、失败汇总与按项重试。
//!
//! pnpm v11 对两类构建脚本默认不放行、缺白名单时报硬错误：
//! 1. git 托管插件的 `prepare` 构建（`ERR_PNPM_GIT_DEP_PREPARE_NOT_ALLOWED`）——
//!    其允许键（depPath = `name@<pkgResolutionId>`）随 pnpm 的克隆方式变化
//!    （git+ssh#sha / codeload tar.gz），无法预先确定；
//! 2. 传递依赖的原生构建（如 `node-pty`，`ERR_PNPM_IGNORED_BUILDS`）。
//! 因此在安装失败时从 pnpm 错误输出解析它建议的 `allowBuilds` 键，写入 profile
//! 的 `pnpm-workspace.yaml` 后重试，直至成功或无可解析项。逐插件安装时每次
//! 失败输出只对应一个插件，解析/重试更可靠。

use serde::Serialize;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};

use crate::config;
use crate::service::cli;
use crate::service::download;
use crate::service::download::Installable;
use crate::service::workflow;

use super::installed::{PREINSTALL_PROFILE, profile_dir};
use super::preset::{load_presets, PreinstallPluginInfo};
use super::process::{run_plugin_process, PreinstallLogPayload, PREINSTALL_LOG_EVENT};

/// 允许构建重试的上限。每次重试解决 pnpm 报出的一个允许键（git depPath 或
/// 传递构建包名），多个原生依赖各占一次，上限封顶防死循环。
const MAX_ALLOW_LIST_RETRIES: usize = 8;

/// 可安全用于插件安装的用户 pnpm 最低主版本。
///
/// pnpm 10+ 才从 `pnpm-workspace.yaml` 读取 `autoInstallPeers`（9 及更早只读
/// `.npmrc`），且 10+ 移除了 workspace-root 安装门槛（`ERR_PNPM_ADDING_TO_ROOT`
/// 是 8/9 行为）。低于此版本时插件安装必须改用捆绑版 pnpm，否则会出现
/// 自动合成 peer 后 `No matching version found for @deepseek-ai/...` 的假失败。
const MIN_TRUSTED_PNPM_MAJOR: u32 = 10;

/// 前端监听的行内安装状态事件名（按插件 id 推送 installing/success/failed）
pub(crate) const PREINSTALL_STATUS_EVENT: &str = "preinstall-plugin-status";

/// 行内安装状态事件载荷
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreinstallStatusPayload {
    pub id: String,
    /// installing | success | failed
    pub status: String,
    /// 失败原因（仅 failed 时携带）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 单个预装插件的安装结果（前端失败汇总与按项重试用）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreinstallResult {
    pub id: String,
    pub name: String,
    pub success: bool,
    /// 失败原因（成功时为 null）
    pub error: Option<String>,
}

/// 全局取消标记：`cancel` 命令置位后，安装循环在下一个插件前退出；
/// 已完成的插件结果保留，未开始的插件不再调度。
static CANCELLED: AtomicBool = AtomicBool::new(false);

/// 置位取消标记（供 cancel 模块在强杀进程树前调用）
pub(crate) fn mark_cancelled() {
    CANCELLED.store(true, Ordering::SeqCst);
}

/// 校验并逐个安装选中的预装插件：`dsh plugin --profile web add <spec>`。
///
/// 返回每个插件的独立结果；单个插件失败仅记入其自身结果，不阻断其余插件。
/// 仅环境级失败（shim 缺失、pnpm 补齐失败等）或用户取消时返回 `Err`。
pub async fn install(
    app_handle: &AppHandle,
    ids: &[String],
) -> Result<Vec<PreinstallResult>, String> {
    CANCELLED.store(false, Ordering::SeqCst);
    if ids.is_empty() {
        return Err("PREINSTALL_EMPTY: no plugins selected".to_string());
    }

    // 单次读取预设并构建查找表，提升算法效率至 O(N)
    let presets = load_presets(app_handle);
    let preset_map: HashMap<&str, &PreinstallPluginInfo> = presets
        .iter()
        .map(|p| (p.id.as_str(), p))
        .collect();

    // 校验选中项：未知 id 作为失败结果返回，但不阻断有效插件的安装。
    let mut selected: Vec<&PreinstallPluginInfo> = Vec::with_capacity(ids.len());
    let mut invalid_results = Vec::new();
    for id in ids {
        match preset_map.get(id.as_str()) {
            Some(p) => selected.push(p),
            None => {
                log::warn!("PREINSTALL_INVALID_ID: {id}");
                invalid_results.push(PreinstallResult {
                    id: id.clone(),
                    name: id.clone(),
                    success: false,
                    error: Some(format!("PREINSTALL_INVALID_ID: unknown plugin id {id}")),
                });
            }
        }
    }
    if selected.is_empty() {
        if invalid_results.is_empty() {
            return Err("PREINSTALL_EMPTY: no valid plugins selected".to_string());
        }
        return Ok(invalid_results);
    }

    // 确保 pnpm/dsh shim 存在
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

    // 选定/补齐安装用的 pnpm：返回是否应强制使用捆绑版（版本感知，见 ensure_pnpm）
    let prefer_bundled_pnpm = ensure_pnpm(app_handle, &window).await?;

    // 安装前停止运行中的服务，避免资源冲突
    if workflow::has_owned_process() {
        log::info!("Stopping running harness service before installing preinstall plugins");
        if let Err(e) = workflow::stop(app_handle.clone()).await {
            log::warn!("failed to stop harness before preinstall: {e}");
        }
    }

    // 构建环境变量（所有插件共用）
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
    // 用户 pnpm 过旧/不可探测时强制 pnpm shim 优先捆绑版，避免 8/9 的
    // autoInstallPeers 语义与 workspace-root gate 破坏插件安装（见 ensure_pnpm）
    if prefer_bundled_pnpm {
        envs.insert("DSH_PREFER_BUNDLED_PNPM".to_string(), "1".to_string());
    }

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

    let cwd = config::get_dsh_install_path(app_handle);

    // 公共参数前缀；每个插件追加自己的 spec
    let base_args = vec![
        dsh_bin.as_os_str().to_os_string(),
        OsString::from("plugin"),
        OsString::from("--profile"),
        OsString::from(PREINSTALL_PROFILE),
        OsString::from("add"),
    ];

    let mut results: Vec<PreinstallResult> = invalid_results;
    results.reserve(selected.len());
    for preset in selected {
        if CANCELLED.load(Ordering::SeqCst) {
            break;
        }

        let id = preset.id.as_str();
        log::info!("Installing preinstall plugin {id} (spec {})", preset.spec);
        let _ = window.emit(
            PREINSTALL_LOG_EVENT,
            PreinstallLogPayload {
                line: format!("[dsh] 开始安装 {id}…"),
            },
        );
        emit_status(&window, id, "installing", None);

        let mut args = base_args.clone();
        args.push(OsString::from(preset.spec.as_str()));

        let outcome = install_one(app_handle, &node, &args, &cwd, &envs, &window).await;

        if CANCELLED.load(Ordering::SeqCst) {
            // 用户取消：剩余插件不再调度，结果统一以取消错误返回
            log::info!("Preinstall cancelled while installing {id}");
            break;
        }

        match outcome {
            Ok(()) => {
                emit_status(&window, id, "success", None);
                log::info!("Preinstall plugin {id} installed successfully");
                results.push(PreinstallResult {
                    id: id.to_string(),
                    name: preset.name.clone(),
                    success: true,
                    error: None,
                });
            }
            Err(e) => {
                log::error!("Preinstall plugin {id} failed: {e}");
                let _ = window.emit(
                    PREINSTALL_LOG_EVENT,
                    PreinstallLogPayload {
                        line: format!("[dsh] 插件 {id} 安装失败"),
                    },
                );
                emit_status(&window, id, "failed", Some(e.as_str()));
                results.push(PreinstallResult {
                    id: id.to_string(),
                    name: preset.name.clone(),
                    success: false,
                    error: Some(e),
                });
            }
        }
    }

    if CANCELLED.load(Ordering::SeqCst) {
        return Err("PREINSTALL_CANCELLED: install cancelled by user".to_string());
    }

    // Windows 极简模式专项修复：仅当该项成功安装时执行（幂等）
    if results
        .iter()
        .any(|r| r.id == "dsh-win-terminal-inspector" && r.success)
    {
        if let Err(e) = workflow::win_inspector::apply(app_handle) {
            log::warn!("win inspector apply failed after install: {e}");
        }
    }

    let (ok, fail) = results
        .iter()
        .fold((0, 0), |(ok, fail), r| if r.success { (ok + 1, fail) } else { (ok, fail + 1) });
    log::info!("Preinstall finished: {ok} succeeded, {fail} failed");
    Ok(results)
}

/// 推送单个插件的行内安装状态事件
fn emit_status(window: &WebviewWindow, id: &str, status: &str, error: Option<&str>) {
    let _ = window.emit(
        PREINSTALL_STATUS_EVENT,
        PreinstallStatusPayload {
            id: id.to_string(),
            status: status.to_string(),
            error: error.map(|s| s.to_string()),
        },
    );
}

/// 安装单个插件：`dsh plugin add` 在 profile 目录里驱动 pnpm。pnpm v11 会拦下
/// git 托管插件的 prepare 构建与传递原生依赖（见模块头注），其允许键不可预知，
/// 因此失败时解析输出里印出的 `allowBuilds` 键写回 profile 的 pnpm-workspace.yaml
/// 后重试，直至成功、无可加键或用户取消。
async fn install_one(
    app_handle: &AppHandle,
    node: &Path,
    args: &[OsString],
    cwd: &Path,
    envs: &HashMap<String, String>,
    window: &WebviewWindow,
) -> Result<(), String> {
    let mut retries = 0usize;
    loop {
        let (code, captured) = run_plugin_process(node, args, cwd, envs, window).await?;
        if code == 0 {
            return Ok(());
        }
        if CANCELLED.load(Ordering::SeqCst) {
            return Err("PREINSTALL_CANCELLED: user cancelled".to_string());
        }

        let new_keys = parse_allowlist_keys(&captured);
        if new_keys.is_empty() || retries >= MAX_ALLOW_LIST_RETRIES {
            log::error!(
                "dsh plugin install failed with exit code {code}; no more allowBuilds entries to add"
            );
            return Err(format!("dsh plugin exited with code {code}"));
        }

        retries += 1;
        add_allow_build_keys(app_handle, &new_keys)?;
        log::info!("pnpm allowBuilds updated with {new_keys:?}, retrying ({retries})");
        let _ = window.emit(
            PREINSTALL_LOG_EVENT,
            PreinstallLogPayload {
                line: "[pnpm] 已放行插件构建（allowBuilds），重试安装…".to_string(),
            },
        );
    }
}

/// 确保插件安装使用的 pnpm 可用，返回是否应强制使用捆绑版
/// （true 时调用方注入 `DSH_PREFER_BUNDLED_PNPM=1`，pnpm shim 优先捆绑版）。
///
/// 版本感知策略，避免给已装正确 pnpm 的用户增加下载步骤：
/// - 捆绑版已存在 → 直接用捆绑版（零额外下载，确定性最强）；
/// - 用户 pnpm 主版本 ≥ MIN_TRUSTED_PNPM_MAJOR → 复用用户 pnpm，零额外步骤；
/// - 用户 pnpm 过旧（8/9：不读 pnpm-workspace.yaml 的 autoInstallPeers、有
///   workspace-root gate；corepack shim 在 Node 24 上还会 ERR_INVALID_THIS 崩溃）
///   或版本不可探测 → 下载捆绑版并强制使用。
async fn ensure_pnpm(app_handle: &AppHandle, window: &WebviewWindow) -> Result<bool, String> {
    if config::get_pnpm_binary_path(app_handle).exists() {
        return Ok(true);
    }

    match user_pnpm_major_version(app_handle) {
        Some(major) if major >= MIN_TRUSTED_PNPM_MAJOR => {
            log::info!("Reusing user-installed pnpm (major {major}) for plugin install");
            return Ok(false);
        }
        Some(major) => {
            log::warn!(
                "User pnpm major {major} < {MIN_TRUSTED_PNPM_MAJOR} (missing autoInstallPeers/workspace-root semantics), downloading bundled pnpm"
            );
        }
        None => {
            log::warn!("User pnpm version not detectable (broken/blocked shim?), downloading bundled pnpm");
        }
    }

    let _ = window.emit(
        PREINSTALL_LOG_EVENT,
        PreinstallLogPayload {
            line: "[pnpm] bundled pnpm not found, downloading before plugin install".to_string(),
        },
    );

    let tracker = download::ProgressTracker::new(window, 2);
    let url = download::Pnpm.get_download_url()?;
    let name = url.split('/').next_back().unwrap_or(&url).to_string();
    let buffer = download::download_file(&tracker, url)
        .await
        .map_err(|e| format!("PNPM_DOWNLOAD_FAILED: {e}"))?;
    download::verify_sha256(&buffer, config::PNPM_SHA256)
        .map_err(|e| format!("PNPM_INTEGRITY_FAILED: {e}"))?;
    let dest = download::Pnpm.get_install_path(app_handle);

    download::ensure_extract(&tracker, name, buffer, dest)
        .await
        .map_err(|e| format!("PNPM_EXTRACT_FAILED: {e}"))?;

    let _ = window.emit(
        PREINSTALL_LOG_EVENT,
        PreinstallLogPayload {
            line: "[pnpm] bundled pnpm ready".to_string(),
        },
    );
    Ok(true)
}

/// 用户 pnpm 主版本号（解析 `pnpm --version` 首个点分字段）；不存在或不可运行
/// （corepack shim 在 Node 24 上 ERR_INVALID_THIS 崩溃等）返回 None。
fn user_pnpm_major_version(app_handle: &AppHandle) -> Option<u32> {
    let pnpm = cli::find_user_pnpm(app_handle)?;
    let output = std::process::Command::new(&pnpm)
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.split('.').next()?.trim().parse::<u32>().ok()
}

/// 从 pnpm 失败输出中解析需写入 `allowBuilds` 的键集合：
/// - git 托管插件 prepare 被拦时，pnpm 会提示 `allowBuilds:\n  <depPath>: true`，
///   原样采纳 depPath（形式随克隆方式变化，只能是运行期报出的值）；
/// - 传递原生依赖被忽略构建（`Ignored build scripts:`）时，取其 `name@version` 的包名。
fn parse_allowlist_keys(output: &str) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    let lines: Vec<&str> = output.lines().collect();

    // 1) git depPath 允许键：跟随 `allowBuilds:` 示例行后的缩进 `<key>: true`。
    for (idx, line) in lines.iter().enumerate() {
        if line.trim() == "allowBuilds:" {
            if let Some(next) = lines.get(idx + 1) {
                if let Some(key) = extract_allow_line_key(next) {
                    if !keys.iter().any(|k| k == &key) {
                        keys.push(key);
                    }
                }
            }
        }
    }

    // 2) 传递原生构建包名：`Ignored build scripts: <name>@<ver>, ...`。
    for line in &lines {
        if let Some(sub) = line.split("Ignored build scripts:").nth(1) {
            for token in sub.split([',', ' ']) {
                let token = token.trim();
                if token.is_empty() {
                    continue;
                }
                let name = token.split('@').next().unwrap_or(token).trim();
                if !name.is_empty() && !keys.iter().any(|k| k == name) {
                    keys.push(name.to_string());
                }
            }
        }
    }

    keys
}

/// 若 `line` 形如 `  <key>: true`（有缩进），返回 `<key>`（去缩进与后缀）。
/// pnpm 报出的 depPath 键本身不带引号，这里只做剥离该行格式。
fn extract_allow_line_key(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if trimmed.len() == line.len() {
        return None; // 无缩进，不是白名单条目
    }
    let suffix = trimmed.strip_suffix(": true")?;
    let key = suffix.trim_end();
    if key.is_empty() {
        return None;
    }
    Some(key.to_string())
}

/// profile 下的 `pnpm-workspace.yaml` 路径（$DSH_HOME/profiles/web）
fn profile_workspace_path(app_handle: &AppHandle) -> PathBuf {
    profile_dir(app_handle).join("pnpm-workspace.yaml")
}

/// 把新的 `allowBuilds` 键合并写回 profile 的 `pnpm-workspace.yaml`。
///
/// dsh 的 `initProfile` 仅在文件缺失时创建（其模板无 `allowBuilds`），因此桌面端
/// 自行维护该块：缺失时按 dsh 模板补建基础设置并追加 `allowBuilds`；已有时按键
/// 去重合并。git depPath 键含 `@`/`/`/`:`/`#`，按 YAML 单引号标量写入避免误解析；
/// `false` 不应出现于此（我们只放行）。重复写入同一键是无害的（幂等）。
fn add_allow_build_keys(app_handle: &AppHandle, keys: &[String]) -> Result<(), String> {
    let path = profile_workspace_path(app_handle);
    let dir = path
        .parent()
        .ok_or("PREINSTALL_BAD_PROFILE_DIR: no profile dir")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("PREINSTALL_MKDIR: {e}"))?;

    let mut content = if path.exists() {
        std::fs::read_to_string(&path)
            .map_err(|e| format!("PREINSTALL_READ_WORKSPACE: {e}"))?
    } else {
        // 与 dsh `initProfile` 生成的基础模板保持一致（尚无 allowBuilds）
        "packages:\n  - .\n\nnodeLinker: hoisted\nautoInstallPeers: false\n".to_string()
    };

    let has_allow_builds = content
        .lines()
        .any(|l| l.trim_start().starts_with("allowBuilds:"));
    if !has_allow_builds {
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str("allowBuilds:\n");
    }

    // 收集已有 `  <key>: true` 条目（含单引号形式），避免重复。基础模板里
    // 的 `packages`/`nodeLinker`/`autoInstallPeers` 等行不会以 `: true` 结尾，
    // 天然被排除。
    let existing: Vec<String> = content
        .lines()
        .filter_map(|l| {
            let trimmed = l.trim_start();
            if trimmed.len() == l.len() {
                return None; // 非缩进行（顶层键）不参与
            }
            let suffix = trimmed.strip_suffix(": true")?;
            let key = suffix.trim().trim_matches(['\'', '"']);
            if key.is_empty() || key.contains(':') {
                return None;
            }
            Some(key.to_string())
        })
        .collect();

    let mut dirty = false;
    for key in keys {
        if existing.iter().any(|k| k == key) {
            continue;
        }
        // 单引号包裹键：git depPath 含 `:`/`#`/`@`，裸写会让 YAML 误解析
        content.push_str(&format!("  '{}': true\n", key.replace('\'', "''")));
        dirty = true;
    }
    if !dirty {
        return Ok(());
    }

    std::fs::write(&path, content).map_err(|e| format!("PREINSTALL_WRITE_WORKSPACE: {e}"))
}

#[cfg(test)]
mod tests {
    use super::{extract_allow_line_key, parse_allowlist_keys};

    #[test]
    fn parse_git_dep_path_key() {
        let out = "\
[ERR_PNPM_GIT_DEP_PREPARE_NOT_ALLOWED] Failed to prepare git-hosted package fetched from \"...\"
The git-hosted package \"dsh-better-sidebar@0.14.0\" needs to execute build scripts but is not in the \"allowBuilds\" allowlist.
...
allowBuilds:
  dsh-better-sidebar@git+ssh://git@github.com/omdsh-dev/DSH-better-sidebar.git#6c89: true
";
        let keys = parse_allowlist_keys(out);
        assert!(keys.contains(&"dsh-better-sidebar@git+ssh://git@github.com/omdsh-dev/DSH-better-sidebar.git#6c89".to_string()));
        assert!(!keys.contains(&"dsh-better-sidebar".to_string()));
    }

    #[test]
    fn parse_ignored_builds_name() {
        let out = "[ERR_PNPM_IGNORED_BUILDS] Ignored build scripts: node-pty@1.1.0\n";
        let keys = parse_allowlist_keys(out);
        assert_eq!(keys, vec!["node-pty".to_string()]);
    }

    #[test]
    fn parse_empty_when_irrelevant() {
        let out = "everything looks fine output\nno allowlist here\n";
        assert!(parse_allowlist_keys(out).is_empty());
    }

    #[test]
    fn allow_line_key_requires_indent() {
        let key = extract_allow_line_key("  node-pty: true");
        assert_eq!(key.as_deref(), Some("node-pty"));

        // 无缩进（顶层键）不应被当作白名单条目
        assert_eq!(extract_allow_line_key("packages:"), None);
        assert_eq!(extract_allow_line_key("allowBuilds:"), None);
    }
}
