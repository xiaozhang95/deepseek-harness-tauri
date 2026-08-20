// dsh 服务启动器：安装器已把 vendor/dsh 运行时铺到安装目录 → 拉起内置 node 跑 dsh web → 就绪后切主窗口。

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, SystemTime};
use std::sync::Mutex;
use tauri::{AppHandle, Manager};

/// dsh web 注入在入口 HTML 里的启动标记：探测端口时区分"端口上是 DSH 服务"和"被别的程序占了"。
const DSH_BOOT_MARKER: &str = "__DSH_BOOT__";

/// 集中配置（工程根 config.json）。全部 Option：读不到/解析失败回退内置默认值。
/// rename_all = "camelCase"：JSON 是 camelCase（maxWaitMs…），结构体是 snake_case，不加会静默反序列化失败。
#[derive(serde::Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DshConfig {
    pub port: Option<u16>,        // dsh 服务端口
    pub max_wait_ms: Option<u64>, // 服务就绪最长等待
    pub poll_ms: Option<u64>,     // 健康轮询间隔
    pub data_home: Option<String>,  // DSH_HOME 路径模板（~/.dsh）
}

/// node 二进制的资源名：Windows 带 .exe，其他平台（macOS）无扩展名。
/// 与 tauri.{windows,macos}.conf.json 里 resources 的映射目标保持一致。
fn node_resource_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "node/node.exe"
    } else {
        "node/node"
    }
}

impl DshConfig {
    fn port(&self) -> u16 {
        self.port.unwrap_or(3090)
    }
    fn max_wait_ms(&self) -> u64 {
        self.max_wait_ms.unwrap_or(90_000)
    }
    fn poll_ms(&self) -> u64 {
        self.poll_ms.unwrap_or(800)
    }
    /// DSH_HOME（profiles / sessions / credentials 等用户数据）。
    fn data_home(&self) -> PathBuf {
        expand_path(self.data_home.as_deref().unwrap_or("~/.dsh"))
    }
}

/// 用户主目录环境变量：Windows 读 USERPROFILE，类 Unix 读 HOME。
fn home_env() -> String {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default()
}

/// 展开路径模板：`~` → 用户主目录，`%APPDATA%` → 系统 AppData（仅 Windows）。读不到按字面处理，不 panic。
fn expand_path(template: &str) -> PathBuf {
    let home = home_env();
    let expanded = if cfg!(windows) {
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        template.replace("~", &home).replace("%APPDATA%", &appdata)
    } else {
        // 非 Windows 不展开 %APPDATA%（调用方已保证不会带它走到这里）
        template.replace("~", &home)
    };
    PathBuf::from(expanded)
}

/// 读取集中配置：优先打包后的 resource_dir，其次工程根（开发模式）；全部失败回退默认值。
pub fn load_config(handle: &AppHandle) -> DshConfig {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = handle.path().resource_dir() {
        candidates.push(dir.join("config.json"));
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../config.json"));
    for path in candidates {
        if let Ok(text) = fs::read_to_string(&path) {
            if let Ok(cfg) = serde_json::from_str::<DshConfig>(&text) {
                return cfg;
            }
        }
    }
    DshConfig::default()
}

/// 最新一次推给 loading 页的状态文案。存一份是因为竞态：boot() 推文案时
/// loading 页脚本可能还没就绪（eval 落空），on_page_load 会读缓存重放，保证文案不丢。
static LOADING_STATUS: Mutex<String> = Mutex::new(String::new());

/// JS 单引号字符串转义（反斜杠 + 单引号），所有拼 JS 的地方共用。
pub fn js_escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\'', "\\'")
}

/// 向 loading 页推送一行状态提示（window.__dshLoading.setStatus）。
/// loading 窗口不存在/已销毁时静默忽略。
pub fn set_loading_status(handle: &AppHandle, text: &str) {
    *LOADING_STATUS.lock().unwrap() = text.to_string();
    if let Some(window) = handle.get_webview_window("loading") {
        let escaped = js_escape(text);
        let _ = window.eval(&format!(
            "window.__dshLoading && window.__dshLoading.setStatus('{escaped}');"
        ));
    }
}

/// 当前缓存的状态文案（空串 = 还没推送过），供 on_page_load 重放。
pub fn loading_status() -> String {
    LOADING_STATUS.lock().unwrap().clone()
}

/// 完整启动流程；返回服务子进程句柄（端口上已有服务时返回 None —— 直接接管）。
pub async fn boot(handle: &AppHandle) -> Result<Option<Child>, String> {
    let config = load_config(handle);
    let port = config.port();
    let max_wait_ms = config.max_wait_ms();
    let poll_ms = config.poll_ms();
    let node_exe = resolve_resource(handle, node_resource_name())?;
    // 运行时由安装器直接铺在 resource_dir/vendor/dsh（NSIS 安装阶段复制完成），
    // 应用不再解压——升级即自动刷新运行时，也没有首次启动的解压等待。
    let vendor_root = resolve_resource(handle, "vendor/dsh")?;
    let bin = vendor_root.join("node_modules/@deepseek-ai/dsh/lib/bin.js");
    let dsh_home = config.data_home();

    // 1) 校验运行时就位（缺失说明打包时漏了资源，直接报错而不是静默失败）
    if !bin.exists() {
        return Err(format!(
            "未找到内置运行时 {}；请先运行 build-runtime 装配运行时并重新打包",
            bin.display()
        ));
    }

    // 2) 端口探测：已有 DSH 服务直接接管（比如用户先手动启动了 dsh web）；
    //    端口被其他程序占用则报错（服务起不来）
    match probe_port(port) {
        Ok(true) => {
            eprintln!("[dsh-tauri] 端口 {port} 已有 DSH 服务，直接连接");
            navigate_to_ui(handle, port)?;
            return Ok(None);
        }
        Ok(false) => {
            return Err(format!("端口 {port} 已被其他程序占用，且响应页面不含 DSH 标记"));
        }
        Err(_) => { /* 端口空闲，继续启动 */ }
    }

    // 3) 启动服务子进程。为什么用随包捆绑的 node 而不是系统 Node：koffi（目录选择器）
    //    等 N-API 原生模块只兼容官方构建的 Node（ABI 一致），系统 Node 变体可能运行期崩溃。
    //    工作目录设为 vendor 根，保证相对路径解析和 `pnpm dsh web` 一致。
    set_loading_status(handle, "正在启动内置服务…");
    // macOS/Linux：安装器复制资源时可能丢可执行位，spawn 前兜底 chmod 755。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&node_exe, fs::Permissions::from_mode(0o755));
    }
    let mut command = Command::new(&node_exe);
    command
        // --expose-internals 是硬性要求：web profile 的 HMR 插件依赖 loader.internal，缺了启动即崩
        .arg("--expose-internals")
        .arg(&bin)
        .arg("web")
        .arg("--port")
        .arg(port.to_string())
        // --no-open：dsh web 默认会用系统默认浏览器打开 UI（面向 CLI 场景）；
        // Tauri 壳自己导航主窗口到该地址，不需要服务再开一个浏览器
        .arg("--no-open")
        .current_dir(&vendor_root)
        .env("DSH_HOME", &dsh_home)
        .stdin(std::process::Stdio::null());

    // 服务 stdout/stderr 重定向到 DSH_HOME 下的 dsh-service.log（append 保留历史）：
    // 安装目录（Program Files）不可写，日志必须落在用户可写目录。
    // 日志问题绝不能阻止服务启动——打开/克隆失败一律回退 null。
    let log_path = dsh_home.join("dsh-service.log");
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::OpenOptions::new().create(true).append(true).open(&log_path) {
        Ok(log_file) => {
            // stdout/stderr 各需独立句柄：同一个 File move 进 Stdio 后不能再用第二次（E0382），先 try_clone
            let stderr_log = log_file.try_clone().ok();
            command.stdout(std::process::Stdio::from(log_file));
            command.stderr(match stderr_log {
                Some(clone) => std::process::Stdio::from(clone),
                None => std::process::Stdio::null(),
            });
        }
        Err(_) => {
            command.stdout(std::process::Stdio::null());
            command.stderr(std::process::Stdio::null());
        }
    }
    // Windows：node.exe 是控制台程序，父进程无控制台时系统会弹一个黑窗——显式禁止
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("无法启动服务进程 {}: {e}", node_exe.display()))?;

    // 4) 健康轮询：每 poll_ms 探测一次，就绪后切主窗口；同时监视服务是否
    //    提前退出（启动即崩），超时报错。
    let started = SystemTime::now();
    loop {
        match probe_port(port) {
            Ok(true) => {
                navigate_to_ui(handle, port)?;
                return Ok(Some(child));
            }
            Ok(false) => {
                return Err(format!("端口 {port} 被其他程序占用，且不是 DSH 服务"));
            }
            Err(_) => { /* 还没就绪，继续轮询 */ }
        }
        if let Ok(Some(_)) = child.try_wait() {
            return Err("服务进程异常退出".into());
        }
        if started.elapsed().unwrap_or_default().as_millis() > max_wait_ms as u128 {
            return Err(format!("服务在 {} 秒内未就绪", max_wait_ms / 1000));
        }
        tokio::time::sleep(Duration::from_millis(poll_ms)).await;
    }
}

/// 服务就绪后的窗口切换：显示主窗口 → 同步一次标题栏主题（防白闪）→ 导航到
/// 本地服务 → 启动主题实时跟随 → 关 loading。loading 无边框、main 有系统标题栏。
fn navigate_to_ui(handle: &AppHandle, port: u16) -> Result<(), String> {
    let url: tauri::Url = format!("http://127.0.0.1:{port}")
        .parse()
        .map_err(|e| format!("无效的服务地址: {e}"))?;
    if let Some(window) = handle.get_webview_window("main") {
        // 先按用户设置同步一次标题栏，避免刚出现时和页面主题不一致（视觉闪变）
        let _ = window.set_theme(Some(tauri_theme(startup_theme(handle))));
        window
            .show()
            .map_err(|e| format!("显示主窗口失败: {e}"))?;
        let _ = window.set_focus();
        window
            .navigate(url)
            .map_err(|e| format!("导航到服务地址失败: {e}"))?;
    }
    start_theme_sync(handle);
    // 最后关 loading，避免它遮住主窗口刚显示的内容
    if let Some(loading) = handle.get_webview_window("loading") {
        let _ = loading.close();
    }
    Ok(())
}

/// "light" → Light，其余一律 Dark。
fn tauri_theme(theme: &str) -> tauri::Theme {
    if theme == "light" {
        tauri::Theme::Light
    } else {
        tauri::Theme::Dark
    }
}

/// 去掉 Windows verbatim 路径前缀（`\\?\C:\...` / `\\?\UNC\server\...`）。
/// Tauri 的 resource_dir() 内部经过 canonicalize，在 Windows 上返回 verbatim 形态；
/// Rust 文件 API 处理它没问题（存在性检查等照常工作），但把这种路径作为入口
/// 传给子进程（node）时，Node 的 realpathSync 会对裸盘符 `C:` 做 lstat 并抛
/// EISDIR（实测 Node 24 复现），服务启动即崩——所以跨进程边界前必须转普通路径。
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let text = path.as_os_str().to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        // `\\?\UNC\server\share\...` → `\\server\share\...`
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        // `\\?\C:\...` → `C:\...`
        return PathBuf::from(rest.to_string());
    }
    path
}

/// 定位资源文件：优先打包后的 resource_dir（安装目录下），
/// 否则回退到本工程 resources/（开发模式；由 build-runtime 装配）。
fn resolve_resource(handle: &AppHandle, relative: &str) -> Result<PathBuf, String> {
    if let Ok(dir) = handle.path().resource_dir() {
        let candidate = dir.join(relative);
        if candidate.exists() {
            return Ok(strip_verbatim_prefix(candidate));
        }
    }
    // 开发回退：src-tauri/../resources = 本工程 resources/
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("resources")
        .join(relative);
    if dev.exists() {
        return Ok(strip_verbatim_prefix(dev));
    }
    Err(format!(
        "找不到资源 {relative}（resource_dir 与本工程 resources/ 均不存在，请先运行 build-runtime 装配运行时）"
    ))
}

/// 启动主题：优先 ~/.dsh/settings.yaml 的 `ui-theme.preference`（用户设置），
/// 读不到回退系统配色（窗口 theme() 判断亮暗）。
pub fn startup_theme(handle: &AppHandle) -> &'static str {
    if let Some(pref) = settings_theme() {
        return pref;
    }
    if let Some(window) = handle.get_webview_window("loading") {
        if let Ok(tauri::Theme::Dark) = window.theme() {
            return "dark";
        }
        if let Ok(tauri::Theme::Light) = window.theme() {
            return "light";
        }
    }
    "dark"
}

/// 解析 ~/.dsh/settings.yaml 的 ui-theme.preference。
/// 用字符串扫描而不是 YAML 解析：避免额外依赖，且 settings 是应用自己写的、格式可控。
/// 切片要 clamp 边界（文件可能很短，越界会 panic）。
pub fn settings_theme() -> Option<&'static str> {
    let home = home_env();
    if home.is_empty() {
        return None;
    }
    let raw = fs::read_to_string(Path::new(&home).join(".dsh").join("settings.yaml")).ok()?;
    let idx = raw.find("ui-theme:")?;
    let tail = &raw[idx..raw.len().min(idx + 120)];
    let value = tail
        .split_once("preference:")?
        .1
        .split_whitespace()
        .next()?
        .trim_matches(|c| c == '"' || c == '\'');
    match value {
        "dark" => Some("dark"),
        "light" => Some("light"),
        _ => None,
    }
}

/// 探测端口：连上后发 HTTP GET，检查响应 HTML 是否含 DSH 启动标记——
/// 必须确认"端口上是 dsh"才敢接管/等待就绪，不能只看"端口能连上"。
fn probe_port(port: u16) -> std::io::Result<bool> {
    let addr = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
    // 所有 IO 都加超时：探测必须快速失败，否则轮询会卡在 connect 上
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(1200))?;
    stream.set_read_timeout(Some(Duration::from_millis(1200)))?;
    stream.set_write_timeout(Some(Duration::from_millis(1200)))?;
    write!(stream, "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")?;
    let mut buf = Vec::with_capacity(8192);
    let _ = stream.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    Ok(text.contains(DSH_BOOT_MARKER))
}

// --- 主窗口主题实时跟随 ------------------------------------------------------

/// 标题栏图标：暗色标题栏配浅色 logo，亮色标题栏配深色 logo。编译期嵌入二进制。
const ICON_DARK_PNG: &[u8] = include_bytes!("../../assets/icon.png");
const ICON_LIGHT_PNG: &[u8] = include_bytes!("../../assets/icon-light.png");

/// 启动 200ms 轮询：把 dsh UI 的实际配色（页面 color-scheme / data-ds-dark-theme）
/// 同步到窗口标题栏 + 背景色 + 图标。页面内切主题时窗口装饰不会自己变，需要壳层来"搬"。
/// 用"连续两次同值确认"再应用：服务刚启动时探针可能读到瞬时值，避免标题栏跟着闪。
fn start_theme_sync(handle: &AppHandle) {
    let Some(window) = handle.get_webview_window("main") else { return };

    // 轮询状态：current = 已应用的主题；pending = 待确认的主题。
    // Arc<Mutex<>> 包起来是因为回调闭包（eval 结果回来时执行）需要修改它，
    // 且闭包与轮询循环生命周期不同。
    #[derive(Default)]
    struct State {
        current: Option<String>,
        pending: Option<String>,
    }
    let state = std::sync::Arc::new(std::sync::Mutex::new(State::default()));

    tauri::async_runtime::spawn(async move {
        // 探针 JS：读 <html> 的内联 color-scheme；没有则看 body 的
        // data-ds-dark-theme 标记（dsh UI 的两种主题标记方式）；都没有返回空。
        const PROBE: &str = "document.documentElement.style.colorScheme || \
            (document.body && document.body.hasAttribute('data-ds-dark-theme') ? 'dark' : '') || ''";
        let mut ticker = tokio::time::interval(Duration::from_millis(200));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let win = window.clone();
            let st = state.clone();
            // eval_with_callback：执行 JS 并把结果（JSON 序列化字符串）传给回调。
            // 如果 eval 本身失败（窗口已销毁/页面加载中），返回 Err → 退出循环。
            if window
                .eval_with_callback(PROBE, move |value| {
                    let raw = serde_json::from_str::<String>(&value).unwrap_or_default();
                    // 探针明确返回 dark/light 才同步；空/其他值表示页面尚未就绪
                    if raw != "dark" && raw != "light" {
                        return;
                    }
                    let mut s = st.lock().unwrap();
                    if s.current.as_deref() == Some(&raw) {
                        s.pending = None; // 与已应用主题一致 → 清除待确认
                        return;
                    }
                    if s.pending.as_deref() == Some(&raw) {
                        // 连续两次轮询到相同值 → 正式应用
                        s.pending = None;
                        s.current = Some(raw.clone());
                        drop(s); // 先释放锁，避免在持锁状态调用窗口 API
                        apply_window_theme(&win, &raw);
                    } else {
                        s.pending = Some(raw); // 第一次发现变化 → 记录待确认
                    }
                })
                .is_err()
            {
                break; // 窗口已销毁
            }
        }
    });
}

/// 把确认后的主题应用到主窗口：标题栏 + 背景色 + 图标三个一起换。
fn apply_window_theme(window: &tauri::WebviewWindow, theme: &str) {
    let _ = window.set_theme(Some(tauri_theme(theme)));
    let bg = if theme == "light" {
        tauri::window::Color(255, 255, 255, 255)
    } else {
        tauri::window::Color(21, 21, 23, 255)
    };
    let _ = window.set_background_color(Some(bg));
    let icon_bytes = if theme == "light" {
        ICON_LIGHT_PNG
    } else {
        ICON_DARK_PNG
    };
    if let Ok(img) = tauri::image::Image::from_bytes(icon_bytes) {
        let _ = window.set_icon(img);
    }
}
