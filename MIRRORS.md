# 镜像配置

## Rust（crates.io → rsproxy）

已配置在工程 `.cargo/config.toml`，无需操作。想全局生效就复制到 `~/.cargo/config.toml`：

```toml
[source.crates-io]
replace-with = "rsproxy-sparse"

[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"
```

## Tauri（NSIS 打包工具，GitHub 下载）

只有环境变量一种方式，按你的终端选一种。

**CMD**（先 set 再执行，当前窗口有效）：

```cmd
set TAURI_BUNDLER_TOOLS_GITHUB_MIRROR=https://ghfast.top/https://github.com
cargo tauri build
```

> CMD 的 `set` 等号两边不能有空格，值不加引号。

**CMD 永久生效**（写完重开终端）：

```cmd
setx TAURI_BUNDLER_TOOLS_GITHUB_MIRROR "https://ghfast.top/https://github.com"
```

**Git Bash**（单条命令直接带前缀）：

```sh
TAURI_BUNDLER_TOOLS_GITHUB_MIRROR="https://ghfast.top/https://github.com" cargo tauri build
```

**Git Bash 永久生效**：

```sh
echo 'export TAURI_BUNDLER_TOOLS_GITHUB_MIRROR="https://ghfast.top/https://github.com"' >> ~/.bashrc
```

镜像失效换：`https://gh-proxy.com/https://github.com`

> NSIS 下载一次后永久缓存到 `%LOCALAPPDATA%\tauri\`，之后打包不再联网，此变量闲置。
