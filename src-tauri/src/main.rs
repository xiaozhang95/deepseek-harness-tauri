// DeepSeek Harness · Tauri 壳主进程入口：单实例防多开、启动内嵌服务、退出回收服务子进程。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod download;
mod server;

use std::process::Child;
use std::sync::Mutex;
use tauri::Manager;

/// 内嵌 dsh 服务子进程句柄：Mutex 保证线程安全；None = 还没启动 / 端口上已有服务。
struct ServerHandle(Mutex<Option<Child>>);

fn main() {
    let app = tauri::Builder::default()
        // 单实例插件：第二次启动时把已有窗口带到前台（避免两个实例抢同一端口）
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        // 原生「另存为」对话框：session log 导出在 WebView 里走宿主保存
        .plugin(tauri_plugin_dialog::init())
        .manage(ServerHandle(Mutex::new(None)))
        // 会话日志原生保存命令（base64 解码 + 写盘）
        .invoke_handler(tauri::generate_handler![download::save_session_log])
        // 全局"页面加载完成"回调：loading 页注入主题 + 重放启动状态文案；
        // dsh Web UI 页注入会话日志原生保存补丁（幂等）
        .on_page_load(|webview, payload| {
            // 导航到 127.0.0.1（dsh Web UI）后跳过，不污染应用页面；
            // 下载补丁由 server::start_download_patch_sync 轮询注入（on_page_load
            // 对 navigate 目标页是否触发不可靠）
            if payload
                .url()
                .host_str()
                .is_some_and(|h| h == "127.0.0.1")
            {
                return;
            }
            // 注入启动主题并调用 loading 页 applyTheme()
            let theme = server::startup_theme(webview.app_handle());
            let _ = webview.eval(&format!(
                "window.__dshTheme = '{theme}'; \
                 if (window.__dshLoading) window.__dshLoading.applyTheme();"
            ));
            // 重放启动状态文案：boot() 推送时页面脚本可能还没就绪（eval 落空），这里补一次
            let status = server::loading_status();
            if !status.is_empty() {
                let escaped = server::js_escape(&status);
                let _ = webview.eval(&format!(
                    "if (window.__dshLoading) window.__dshLoading.setStatus('{escaped}');"
                ));
            }
        })
        .setup(|app| {
            let handle = app.handle().clone();

            // loading 窗口底色对齐启动主题：页面渲染前窗口背景就生效，防启动瞬间闪白/闪黑
            if let Some(loading) = handle.get_webview_window("loading") {
                let bg = if server::startup_theme(&handle) == "light" {
                    tauri::window::Color(255, 255, 255, 255)
                } else {
                    tauri::window::Color(21, 21, 23, 255)
                };
                let _ = loading.set_background_color(Some(bg));
            }

            // 异步启动服务（解压可能持续几秒到几十秒，放后台让窗口立刻显示）
            tauri::async_runtime::spawn(async move {
                match server::boot(&handle).await {
                    Ok(child) => {
                        // 保存子进程句柄，退出时回收（None = 端口上已有服务）
                        *handle.state::<ServerHandle>().0.lock().unwrap() = child;
                    }
                    Err(error) => {
                        eprintln!("[dsh-tauri] boot failed: {error}");
                        // 启动失败时 main 窗口还隐藏着，把错误显示在 loading 页上
                        if let Some(window) = handle.get_webview_window("loading") {
                            let escaped = server::js_escape(&format!("启动失败：{error}"));
                            let _ = window.eval(&format!(
                                "document.getElementById('status').textContent = '启动失败';\
                                 document.getElementById('error').textContent = '{escaped}';"
                            ));
                        }
                    }
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build tauri application");

    // 退出事件：杀掉服务子进程树
    app.run(|app_handle, event| {
        if let tauri::RunEvent::Exit = event {
            if let Some(child) = app_handle.state::<ServerHandle>().0.lock().unwrap().take() {
                let _ = kill_tree(child);
            }
        }
    });
}

/// 终止服务进程树。Windows 上 Child::kill() 只杀 Node 主进程，它派生的
/// worker 会残留，所以用 `taskkill /T /F` 递归杀整棵树；其他平台直接 kill。
fn kill_tree(child: Child) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        let pid = child.id();
        let mut cmd = std::process::Command::new("taskkill");
        cmd.args(["/pid", &pid.to_string(), "/T", "/F"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // taskkill 也是控制台程序：不设 CREATE_NO_WINDOW，退出时窗口会闪一下黑窗
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let _ = cmd.spawn();
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let mut child = child;
        child.kill()
    }
}
