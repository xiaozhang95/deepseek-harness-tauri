# Tauri打包

> 国内网络加速（crates / NSIS / node / pnpm 各类镜像）见 [MIRRORS.md](MIRRORS.md)。

## 常用命令

```sh
# 安装tauri-cli
cargo install tauri-cli

# 装配 dsh 运行时
cargo run --manifest-path build-runtime/Cargo.toml

# 生成图标
cargo tauri icon assets/icon.png

# 开发运行
cargo run --manifest-path src-tauri/Cargo.toml

# 打包安装
cargo tauri build
```

打包产物：`target/release/bundle/nsis`

## 配置说明（`config.json`）

| 字段          | 默认                                                                                                   | 说明                                                                        |
| ------------- | ------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------- |
| `port`        | 3090                                                                                                   | dsh 服务端口                                                                |
| `maxWaitMs`   | 90000                                                                                                  | 服务就绪最长等待                                                            |
| `pollMs`      | 800                                                                                                    | 健康轮询间隔                                                                |
| `nodeVersion` | 24.19.0                                                                                                | build-runtime 下载的独立 Node 版本                                          |
| `dataHome`    | `~/.dsh`                                                                                               | DSH_HOME（profiles/sessions 等用户数据，`~` = 用户主目录）                  |
| `extractDir`  | Windows: `%APPDATA%/DeepSeek Harness/dsh`；macOS: `~/Library/Application Support/DeepSeek Harness/dsh` | dsh.zip 解压目录（`%APPDATA%` 模板仅 Windows 生效，mac 上会回退平台默认值） |
| `appName`     | DeepSeek Harness                                                                                       | 应用显示名（预留）                                                          |
