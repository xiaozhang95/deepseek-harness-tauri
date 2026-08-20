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
| `appName`     | DeepSeek Harness                                                                                       | 应用显示名（预留）                                                          |

运行时布局：`build-runtime` 把依赖闭包物化到 `resources/vendor/dsh/`，
`tauri.conf.json` 的 resources 目录映射在安装阶段把它铺到安装目录（无需首次启动解压），
服务日志落在 `~/.dsh/dsh-service.log`。
