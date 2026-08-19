// build-runtime：dsh 运行时装配工具（独立 crate）。
// 从主仓库 + 网络装配两个大件到 resources/：dsh.zip（物化+裁剪+压缩）和独立 node。
// 用法见工程根 README.md。

use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use zip::write::SimpleFileOptions;
use zip::CompressionMethod;

// --- 路径 ----------------------------------------------------------------

/// 工程根目录：build-runtime/ 的上一级。
/// CARGO_MANIFEST_DIR 是编译时注入的"Cargo.toml 所在目录"（即 build-runtime/）。
fn root_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// 读取命令行参数 `--flag value` 的值。
fn arg_value(flag: &str) -> Option<String> {
    let args: Vec<String> = env::args().collect();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == flag {
            return iter.next().cloned();
        }
    }
    None
}

/// 主仓库路径：--repo 参数 > DSH_REPO 环境变量 > 默认工程上一级。
fn resolve_repo() -> PathBuf {
    arg_value("--repo")
        .or_else(|| env::var("DSH_REPO").ok())
        .map(PathBuf::from)
        .unwrap_or_else(|| root_dir().join(".."))
}

/// config.json 缺失 nodeVersion 字段时的回退版本。
const NODE_VERSION_FALLBACK: &str = "24.19.0";

/// 从集中配置（工程根 config.json）读 nodeVersion。
/// 读不到就回退默认版本——下载工具绝不能因为配置问题而崩。
fn read_node_version(root: &Path) -> String {
    if let Ok(text) = fs::read_to_string(root.join("config.json")) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(v) = value.get("nodeVersion").and_then(|x| x.as_str()) {
                return v.to_string();
            }
        }
    }
    NODE_VERSION_FALLBACK.to_string()
}

// --- 1) 物化：把依赖闭包复制成 FLAT 树 ------------------------------------
//
// 主仓库的依赖是用 pnpm 管理的：node_modules 里的包大多是"符号链接"指向
// store（真实文件在别处）。运行时需要一份自包含、可整体打包的副本——
// 所以这里做"物化"：沿着依赖图 BFS，把每个包的真实文件复制到
// resources/vendor/dsh/node_modules/ 下，布局模拟 npm 的 FLAT 树：
//   - 每个依赖按名字提升到 node_modules 根（@scope/name 保持两级）
//   - 同名不同版本的包，嵌套在引用它的包下面（与 npm 行为一致）

fn read_manifest(dir: &Path) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let text = fs::read_to_string(dir.join("package.json"))?;
    Ok(serde_json::from_str(&text)?)
}

/// 从 `from_real` 目录向上逐级查找依赖 `name`，返回其真实路径。
///
/// 模拟 Node 的模块解析：从当前目录开始，逐级向上找 `node_modules/<name>`，
/// 找到就 canonicalize（解析符号链接/junction 到真实文件位置），
/// 一直找到主仓库根为止。返回 None = 该依赖在 workspace 里没安装
/// （optional/peer 依赖常见）。
fn resolve_dep(from_real: &Path, name: &str, repo: &Path) -> Option<PathBuf> {
    let mut dir = from_real.to_path_buf();
    loop {
        let candidate = dir.join("node_modules").join(name);
        if candidate.join("package.json").exists() {
            return fs::canonicalize(&candidate).ok();
        }
        let parent = dir.parent()?.to_path_buf();
        if parent == dir || dir == repo {
            return None;
        }
        dir = parent;
    }
}

/// 复制单个包的文件（递归）。
///
/// 规则：跳过符号链接（链接指向的依赖由 BFS 统一处理）、跳过 node_modules
/// 子目录（依赖单独处理）。结果是一份"扁平"的包文件副本。
fn copy_tree(from: &Path, to: &Path) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue; // 依赖链接由 BFS 处理
        }
        let source = entry.path();
        let target = to.join(entry.file_name());
        if file_type.is_dir() {
            if entry.file_name() == "node_modules" {
                continue; // 依赖单独处理
            }
            copy_tree(&source, &target)?;
        } else if file_type.is_file() {
            fs::copy(&source, &target)?;
        }
    }
    Ok(())
}

/// 物化主流程：BFS 遍历依赖闭包，复制出 FLAT 树。
fn materialize_vendor_tree(repo: &Path, vendor_tree: &Path) -> Result<(), Box<dyn std::error::Error>> {
    // 物化的起点是主仓库编译好的 CLI（@deepseek-ai/dsh 包本体）。
    // 它必须先被 pnpm build 产出，否则没有可物化的东西。
    let cli_dir = repo.join("apps").join("cli");
    let cli_bin = cli_dir.join("lib").join("bin.js");
    if !cli_bin.exists() {
        return Err(format!(
            "主仓库 CLI 未构建（缺 {}）；请先在主仓库执行 `pnpm install && pnpm run build`",
            cli_bin.display()
        )
        .into());
    }

    let _ = fs::remove_dir_all(vendor_tree); // 旧树清理（只读文件等失败则忽略）
    let target_root = vendor_tree.join("node_modules");
    fs::create_dir_all(&target_root)?;

    // hoisted：包名 -> 已提升放置的真实目录（同名第一个占位）
    // target_rel：真实目录 -> 目标相对路径（版本冲突嵌套时定位"引用它的包"）
    let mut hoisted: HashMap<String, PathBuf> = HashMap::new();
    let mut target_rel: HashMap<PathBuf, String> = HashMap::new();
    let mut spots = 0usize;

    /// 把一个真实目录复制到目标相对路径；已存在则跳过（同名去重）。
    fn place(
        real: &Path,
        rel: &str,
        target_root: &Path,
        target_rel: &mut HashMap<PathBuf, String>,
        spots: &mut usize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let target = target_root.join(rel);
        if target.exists() {
            return Ok(());
        }
        *spots += 1;
        copy_tree(real, &target)?;
        target_rel.insert(real.to_path_buf(), rel.to_string());
        Ok(())
    }

    // 本地 CLI 作为 @deepseek-ai/dsh 落在 node_modules 根
    let cli_manifest = read_manifest(&cli_dir)?;
    let cli_real = fs::canonicalize(&cli_dir)?;
    place(&cli_real, "@deepseek-ai/dsh", &target_root, &mut target_rel, &mut spots)?;

    // 守卫：新 checkout 没跑 pnpm install 时 workspace 里没有依赖链接，
    // 此时物化只会得到一个空的 CLI 壳——提前报错而不是产出坏包。
    let first_dep = cli_manifest
        .get("dependencies")
        .and_then(|d| d.as_object())
        .and_then(|d| d.keys().next())
        .cloned();
    if let Some(dep) = first_dep {
        if resolve_dep(&cli_real, &dep, repo).is_none() {
            return Err("无法从主仓库 workspace 解析 CLI 依赖；请确认在主仓库执行过 `pnpm install`".into());
        }
    }

    // BFS 队列：从 CLI 出发，逐包解析依赖。processed 防止循环依赖死循环。
    let mut queue: VecDeque<PathBuf> = VecDeque::from([cli_real.clone()]);
    let mut processed: HashSet<PathBuf> = HashSet::from([cli_real]);
    let mut count = 0usize;

    while let Some(dir) = queue.pop_front() {
        count += 1;
        if count % 100 == 0 {
            eprintln!("[build-runtime] {count} packages processed, {spots} placed");
        }
        let manifest = match read_manifest(&dir) {
            Ok(m) => m,
            Err(_) => continue, // 读不到 package.json 的目录不参与依赖解析
        };
        // 三类依赖都要收集：dependencies + optionalDependencies + peerDependencies
        // （peer 依赖在发布包里通常已经由安装方提供，这里一并物化保证自包含）
        let mut deps: Vec<String> = Vec::new();
        for key in ["dependencies", "optionalDependencies", "peerDependencies"] {
            if let Some(obj) = manifest.get(key).and_then(|v| v.as_object()) {
                deps.extend(obj.keys().cloned());
            }
        }
        for name in deps {
            let real = match resolve_dep(&dir, &name, repo) {
                Some(r) => r,
                None => continue, // optional/peer 未安装，跳过
            };
            if !processed.contains(&real) {
                processed.insert(real.clone());
                queue.push_back(real.clone());
            }
            if !hoisted.contains_key(&name) {
                // 首次遇到：提升到 node_modules 根
                hoisted.insert(name.clone(), real.clone());
                place(&real, &name.replace('/', std::path::MAIN_SEPARATOR_STR), &target_root, &mut target_rel, &mut spots)?;
            } else if hoisted.get(&name) != Some(&real) {
                // 同名但真实目录不同 = 版本冲突：像 npm 一样嵌套在引用它的包下
                let requirer_rel = target_rel
                    .get(&dir)
                    .cloned()
                    .unwrap_or_else(|| "@deepseek-ai/dsh".to_string());
                let nested = format!("{requirer_rel}/node_modules/{}", name.replace('/', std::path::MAIN_SEPARATOR_STR));
                place(&real, &nested, &target_root, &mut target_rel, &mut spots)?;
            }
        }
    }

    let name = cli_manifest.get("name").and_then(|v| v.as_str()).unwrap_or("dsh");
    let version = cli_manifest.get("version").and_then(|v| v.as_str()).unwrap_or("?");
    println!("[build-runtime] 物化完成：{name}@{version} 依赖闭包 {spots} 个包 → resources/vendor/dsh");
    Ok(())
}

// --- 2) 裁剪：删掉运行时永远不会读的文件 -----------------------------------
//
// 物化出的树里有很多"打包才需要、运行时没用"的文件，全部删掉能让 dsh.zip 缩小约一半。

/// 运行时不需要的文件扩展名（CLI 跑的是编译后的 lib/*.js，源码/映射/文档都无用）。
const PRUNE_EXTS: &[&str] = &[
    ".pdb", ".map", ".ts", ".tsx", ".mts", ".cts", ".md", ".tsbuildinfo",
];

/// 递归裁剪。单个删除失败一律忽略（只读文件/被占用不影响整体）。
fn prune_vendor_tree(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut remove_dirs: Vec<PathBuf> = Vec::new();
    let mut entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()), // 目录读不到直接跳过
    };
    while let Some(entry) = entries.next().transpose()? {
        let full = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            // node-pty 的 third_party/build 是构建期目录，运行时不需要
            let is_node_pty = dir.ends_with(Path::new("node_modules").join("node-pty"));
            if (name == "third_party" || name == "build") && is_node_pty {
                remove_dirs.push(full);
                continue;
            }
            // @types/node 纯类型包，运行时不需要
            if name == "@types" {
                let _ = fs::remove_dir_all(full.join("node"));
                continue;
            }
            prune_vendor_tree(&full)?;
        } else if file_type.is_file() {
            let lower = name.to_lowercase();
            if PRUNE_EXTS.iter().any(|ext| lower.ends_with(ext)) {
                let _ = fs::remove_file(&full);
            }
        }
    }
    for dir_to_remove in remove_dirs {
        let _ = fs::remove_dir_all(&dir_to_remove);
    }
    Ok(())
}

// --- 3) 打包 dsh.zip --------------------------------------------------------

/// 收集目录下所有文件，返回 (完整路径, 相对 base 的 `/` 分隔名)。
/// zip 内部统一用 `/` 分隔，保证跨平台解压一致。
fn walk_files(dir: &Path, base: &Path, files: &mut Vec<(PathBuf, String)>) -> Result<(), Box<dyn std::error::Error>> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let full = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            walk_files(&full, base, files)?;
        } else if file_type.is_file() {
            let name = full
                .strip_prefix(base)
                .map_err(|e| format!("strip_prefix 失败: {e}"))?
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            files.push((full, name));
        }
    }
    Ok(())
}

/// 把裁剪后的 vendor 树压缩成单个 dsh.zip（deflate 压缩、UTF-8 文件名）。
/// 用 zip crate 而不是外部工具：纯 Rust、跨平台一致、无环境依赖。
fn bundle_vendor_zip(vendor_tree: &Path, out_zip: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut files = Vec::new();
    walk_files(vendor_tree, vendor_tree, &mut files)?;
    if let Some(parent) = out_zip.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = File::create(out_zip)?;
    let mut writer = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (full, name) in &files {
        writer.start_file(name.as_str(), options)?;
        let mut data = Vec::new();
        File::open(full)?.read_to_end(&mut data)?;
        writer.write_all(&data)?;
    }
    writer.finish()?;

    let zip_size = fs::metadata(out_zip)?.len() as f64 / 1024.0 / 1024.0;
    println!("[build-runtime] dsh.zip 生成：{} 个文件，{zip_size:.1} MB → {}", files.len(), out_zip.display());
    Ok(())
}

// --- 4) 下载独立 Node 运行时 -------------------------------------------------
//
// Windows → resources/node.exe（win-x64 zip，zip crate 解压）
// macOS   → resources/node（darwin-x64 / darwin-arm64 tar.gz，系统 tar 解压；
//           --node-target darwin-universal 时下载双架构并用 lipo 合成通用二进制，
//           配合 `cargo tauri build --target universal-apple-darwin` 使用）

/// 解压 zip 到目标目录（带路径穿越防护）。复用 zip crate 的解压能力。
fn extract_zip_entries(zip_path: &Path, dest: &Path) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(dest)?;
    let file = File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("打开 zip 失败: {e}"))?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| format!("读取 zip 条目 {i}: {e}"))?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().replace('\\', "/");
        if name.split('/').any(|seg| seg == ".." || seg.is_empty()) {
            continue; // 路径穿越防护
        }
        let out = dest.join(&name);
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out_file = File::create(&out)?;
        std::io::copy(&mut entry, &mut out_file)?;
    }
    Ok(())
}

/// 下载并读取一个 URL 的完整响应体。带最大读取量限制（防异常响应撑爆内存）
/// 和最小尺寸校验（小于 min_bytes 视为下载异常，比如镜像返回了错误页）。
fn download_to_vec(url: &str, min_bytes: usize) -> Result<Vec<u8>, String> {
    println!("[build-runtime] 下载 {url} ...");
    match ureq::get(url).call() {
        Ok(resp) => {
            let mut bytes: Vec<u8> = Vec::new();
            if let Err(e) = resp.into_reader().take(200_000_000).read_to_end(&mut bytes) {
                return Err(format!("读取失败（{e}）"));
            }
            if bytes.len() < min_bytes {
                return Err(format!("响应尺寸异常（{} 字节），疑似下载不完整", bytes.len()));
            }
            Ok(bytes)
        }
        Err(e) => Err(format!("请求失败（{e}）")),
    }
}

/// 下载独立 Node 运行时（按当前平台分发到不同实现）。
///
/// 为什么必须捆绑独立 Node：dsh 服务的原生模块（koffi 目录选择器）
/// 只兼容官方构建的 Node，系统装的 Node 不一定存在、也不保证 ABI 一致。
/// 版本来自 config.json（nodeVersion），默认 24.19.0。
///
/// 下载策略：npmmirror（国内快）优先，nodejs.org 官方兜底；
/// 已存在则跳过（幂等）。
fn fetch_node_runtime(out_dir: &Path, node_version: &str) -> Result<(), Box<dyn std::error::Error>> {
    if cfg!(windows) {
        fetch_node_windows(out_dir, node_version)
    } else if cfg!(target_os = "macos") {
        fetch_node_macos(out_dir, node_version)
    } else {
        println!("[build-runtime] 该平台的独立 Node 下载尚未实现，跳过（假定系统自带 node）");
        Ok(())
    }
}

/// Windows：下载 win-x64 zip，解出 node.exe。
fn fetch_node_windows(out_dir: &Path, node_version: &str) -> Result<(), Box<dyn std::error::Error>> {
    let node_exe = out_dir.join("node.exe");
    if node_exe.exists() {
        println!("[build-runtime] {} 已存在，跳过下载", node_exe.display());
        return Ok(());
    }
    let zip_path = out_dir.join(format!("node-v{node_version}-win-x64.zip"));
    let extract_dir = out_dir.join(format!("node-v{node_version}-win-x64"));
    let candidates = [
        format!("https://npmmirror.com/mirrors/node/v{node_version}/node-v{node_version}-win-x64.zip"),
        format!("https://nodejs.org/dist/v{node_version}/node-v{node_version}-win-x64.zip"),
    ];
    fs::create_dir_all(out_dir)?;

    let mut downloaded = false;
    for url in &candidates {
        match download_to_vec(url, 10_000_000) {
            Ok(bytes) => {
                fs::write(&zip_path, &bytes)?;
                downloaded = true;
                break;
            }
            Err(e) => println!("[build-runtime] {url} {e}，换下一个源"),
        }
    }
    if !downloaded {
        return Err("所有下载源失败；请手动下载 node.exe 放到 resources/node.exe".into());
    }

    extract_zip_entries(&zip_path, out_dir)?;
    let extracted = extract_dir.join("node.exe");
    if !extracted.exists() {
        return Err("node.zip 解压失败；请手动下载 node.exe 放到 resources/node.exe".into());
    }
    // 只要 node.exe，其余文件（目录、zip）清理掉
    fs::rename(&extracted, &node_exe)?;
    let _ = fs::remove_dir_all(&extract_dir);
    let _ = fs::remove_file(&zip_path);
    println!("[build-runtime] node.exe 就绪：v{node_version}");
    Ok(())
}

/// macOS：下载 darwin tar.gz，解出 node（无扩展名）。
///
/// 目标架构优先级：`--node-target` 参数 > 当前机器架构。
/// 可选值：darwin-x64 / darwin-arm64 / darwin-universal。
/// universal 模式下载两份架构的二进制并用系统 lipo 合并——产出的 node
/// 同时支持 Intel 和 Apple Silicon，配合 Tauri 的 universal-apple-darwin
/// 打包；代价是体积翻倍（~2x）。
fn fetch_node_macos(out_dir: &Path, node_version: &str) -> Result<(), Box<dyn std::error::Error>> {
    let node_out = out_dir.join("node");
    if node_out.exists() {
        println!("[build-runtime] {} 已存在，跳过下载", node_out.display());
        return Ok(());
    }
    fs::create_dir_all(out_dir)?;

    let mut target = if std::env::consts::ARCH == "aarch64" {
        "darwin-arm64"
    } else {
        "darwin-x64"
    }
    .to_string();
    if let Some(value) = arg_value("--node-target") {
        target = value;
    }

    if target == "darwin-universal" {
        let x64 = fetch_macos_node_arch(out_dir, node_version, "x64")?;
        let arm = fetch_macos_node_arch(out_dir, node_version, "arm64")?;
        // lipo 是 macOS 自带工具，把两个单架构二进制合成通用二进制
        let status = std::process::Command::new("lipo")
            .arg("-create")
            .arg(&x64)
            .arg(&arm)
            .arg("-output")
            .arg(&node_out)
            .status()
            .map_err(|e| format!("无法启动 lipo（macOS 系统应自带）: {e}"))?;
        if !status.success() {
            return Err("lipo 合并通用 node 二进制失败".into());
        }
        let _ = fs::remove_file(&x64);
        let _ = fs::remove_file(&arm);
    } else {
        let arch = target
            .strip_prefix("darwin-")
            .ok_or("--node-target 应形如 darwin-x64 / darwin-arm64 / darwin-universal")?;
        let bin = fetch_macos_node_arch(out_dir, node_version, arch)?;
        fs::rename(&bin, &node_out)?;
    }
    println!("[build-runtime] node 就绪：v{node_version}（{target}）");
    Ok(())
}

/// 下载并解压某个 macOS 架构的官方 node 发行包，返回解出的 node 二进制路径。
/// 用系统自带 tar 解压（macOS 必有 bsdtar），避免给本工具加 tar/flate2 依赖。
fn fetch_macos_node_arch(
    out_dir: &Path,
    node_version: &str,
    arch: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let pkg_name = format!("node-v{node_version}-darwin-{arch}");
    let tgz_path = out_dir.join(format!("{pkg_name}.tar.gz"));
    let extract_dir = out_dir.join(&pkg_name);
    let candidates = [
        format!("https://npmmirror.com/mirrors/node/v{node_version}/{pkg_name}.tar.gz"),
        format!("https://nodejs.org/dist/v{node_version}/{pkg_name}.tar.gz"),
    ];

    let mut downloaded = false;
    for url in &candidates {
        match download_to_vec(url, 15_000_000) {
            Ok(bytes) => {
                fs::write(&tgz_path, &bytes)?;
                downloaded = true;
                break;
            }
            Err(e) => println!("[build-runtime] {url} {e}，换下一个源"),
        }
    }
    if !downloaded {
        return Err(format!(
            "所有下载源失败；请手动下载 {pkg_name}.tar.gz，解出其中的 bin/node 放到 resources/"
        )
        .into());
    }

    // tar 解压保留可执行位——这点很关键，后面 spawn node 靠它
    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&tgz_path)
        .arg("-C")
        .arg(out_dir)
        .status()
        .map_err(|e| format!("无法启动 tar: {e}"))?;
    if !status.success() {
        return Err(format!("tar 解压 {pkg_name}.tar.gz 失败").into());
    }
    let _ = fs::remove_file(&tgz_path);

    let bin = extract_dir.join("bin").join("node");
    if !bin.exists() {
        return Err(format!("{pkg_name}.tar.gz 解压后未找到 bin/node").into());
    }
    // 把 node 挪出解压目录（保持文件名带架构后缀，universal 模式下避免两份同名冲突）
    let staged = out_dir.join(format!("node-darwin-{arch}"));
    fs::rename(&bin, &staged)?;
    let _ = fs::remove_dir_all(&extract_dir);
    Ok(staged)
}

// --- 主流程 -----------------------------------------------------------------

fn main() {
    if let Err(e) = run() {
        eprintln!("\n✘ {e}");
        std::process::exit(1);
    }
}

/// 主流程：校验主仓库 → 物化 → 裁剪 → 打包 zip → 下载 node。
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let root = root_dir();
    let repo = resolve_repo();
    println!("[build-runtime] 运行时源：主仓库 {}", repo.display());
    if !repo.join("package.json").exists() {
        return Err(format!(
            "主仓库不存在：{}\n请用 --repo <主仓库路径> 或 DSH_REPO 环境变量指定。",
            repo.display()
        )
        .into());
    }

    let vendor_tree = root.join("resources").join("vendor").join("dsh");
    let out_dir = root.join("resources");
    let out_zip = out_dir.join("dsh.zip");
    let node_version = read_node_version(&root);

    materialize_vendor_tree(&repo, &vendor_tree)?;
    prune_vendor_tree(&vendor_tree)?;
    bundle_vendor_zip(&vendor_tree, &out_zip)?;
    fetch_node_runtime(&out_dir, &node_version)?;

    let node_name = if cfg!(windows) { "node.exe" } else { "node" };
    println!("\n[build-runtime] 完成：resources/dsh.zip + resources/{node_name} 已就绪，可执行 cargo run / tauri build。");
    Ok(())
}
