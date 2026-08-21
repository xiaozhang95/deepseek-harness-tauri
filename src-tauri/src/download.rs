// dsh 会话日志原生保存：WebView 不处理程序化 <a download>，由宿主完成
// 「另存为对话框 + 写盘」。前端注入脚本（download_patch.js）负责对话框与
// fetch，本命令只做 base64 解码 + 写文件，保持 Rust 侧最小、确定。

use base64::Engine;

/// 注入 dsh Web UI 页面的下载补丁脚本（原样 JS，含幂等守卫）。
pub const PATCH_JS: &str = include_str!("download_patch.js");

/// 把 base64 编码的会话导出归档写入用户选择的路径。
///
/// 前端（注入脚本）已通过 `plugin:dialog|save` 拿到目标路径；这里只解码落盘。
/// base64 而非二进制直传：低层 `__TAURI_INTERNALS__.invoke` 不携带
/// Uint8Array 二进制通道，且 vendored 前端没有 `@tauri-apps/api`。
#[tauri::command]
pub async fn save_session_log(path: String, data: String) -> Result<(), String> {
    eprintln!(
        "[dsh-tauri] save_session_log 被调用: {path} (base64 {} 字符)",
        data.len()
    );
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.as_bytes())
        .map_err(|e| format!("base64 解码失败: {e}"))?;
    let written = bytes.len();
    tokio::fs::write(&path, bytes)
        .await
        .map_err(|e| format!("写入 {} 失败: {e}", path))?;
    eprintln!("[dsh-tauri] 已写入 {written} 字节");
    Ok(())
}
