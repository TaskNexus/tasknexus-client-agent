# TaskNexus Agent (Rust)

TaskNexus 客户端代理，用于连接 TaskNexus 服务器并执行远程任务。

Linux 服务器通过 `systemd` 部署时的注意事项见 [LINUX_DEPLOYMENT.md](./LINUX_DEPLOYMENT.md)。

## 特性

- 🚀 **高性能** - 使用 Rust 编写，二进制文件仅 ~2.5 MB
- 🔄 **自动重连** - 断线时自动尝试重新连接
- ❤️ **心跳检测** - 定期发送心跳保持连接
- 📁 **Git 支持** - 自动 clone/pull 项目仓库
- 🖥️ **跨平台** - 支持 Windows, Linux, macOS
- 🚀 **开机自启动** - 支持开机自动启动 Agent

## 安装

### 从 Release 下载

前往 [Releases](https://github.com/yourorg/TaskNexus/releases) 下载对应平台的二进制文件。

### 从源码编译

```bash
# 需要 Rust 1.70+
cargo build --release
```

## 使用方法

### 启动 Agent

```bash
tasknexus-agent --config config.yaml
```

### 配置文件

复制 `config.example.yaml` 并根据需要修改：

```yaml
# 服务器配置
server: ws://localhost:8001/ws/agent/
name: My-Agent
workspaces_path: ./workspaces

# 日志配置
log_level: INFO

# 连接配置
heartbeat_interval: 30
reconnect_interval: 5

# 代理配置（可选）
# http_proxy: http://127.0.0.1:7890
# https_proxy: http://127.0.0.1:7890
# no_proxy: localhost,127.0.0.1,.internal.example.com

# 开机自启动
autostart:
  enabled: true
  args: []
```

所有配置均通过配置文件管理，不支持命令行参数覆盖。

配置了 `http_proxy` / `https_proxy` / `no_proxy` 后，Agent 会在执行任务命令以及 `git clone` / `git fetch` 时自动注入 `HTTP_PROXY`、`HTTPS_PROXY`、`NO_PROXY` 以及对应的小写环境变量。

### 开机自启动

Agent 运行时会根据配置文件自动应用开机自启动设置：

```yaml
autostart:
  enabled: true  # 是否启用开机自启动
  args: []       # 启动时的额外参数
```

**平台说明:**
- **Windows**: 添加到注册表 `HKCU\SOFTWARE\Microsoft\Windows\CurrentVersion\Run`
- **Linux**: 创建 XDG Autostart `.desktop` 文件
- **macOS**: 创建 Launch Agent `.plist` 文件

## 开发

```bash
# 运行测试
cargo test

# 开发模式运行
cargo run -- -s ws://localhost:8001/ws/agent/ -n dev-agent
```

## License

MIT
