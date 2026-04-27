# TaskNexus Agent (Rust)

TaskNexus 客户端代理，用于连接 TaskNexus 服务器并执行远程任务。

Linux 服务器通过 `systemd` 部署时的注意事项见 [LINUX_DEPLOYMENT.md](./LINUX_DEPLOYMENT.md)。

## 特性

- 🚀 **高性能** - 使用 Rust 编写，二进制文件仅 ~2.5 MB
- 🔄 **自动重连** - 断线时自动尝试重新连接
- ❤️ **心跳检测** - 定期发送心跳保持连接
- 📁 **Git 支持** - 自动 clone/pull 项目仓库
- 🖥️ **跨平台** - 支持 Windows, Linux, macOS
- ⚙️ **系统服务** - 支持以系统服务方式部署，开机自动启动、崩溃自动重启
- 🔄 **自更新** - 内建自更新功能，无需额外组件

## 安装

### 从 Release 下载

前往 [Releases](https://github.com/yourorg/TaskNexus/releases) 下载对应平台的二进制文件。

### 从源码编译

```bash
# 需要 Rust 1.70+
cargo build --release
```

## 使用方法

### 直接运行

```bash
tasknexus-agent run --config config.yaml
```

### 服务部署（推荐）

将 Agent 安装为系统服务，实现开机自动启动和崩溃自动恢复：

```bash
# 安装为系统服务（需要管理员/root 权限）
tasknexus-agent service install --config /absolute/path/to/config.yaml

# 启动服务
tasknexus-agent service start

# 查看服务状态
tasknexus-agent service status

# 停止服务
tasknexus-agent service stop

# 重启服务（修改系统环境变量后需要重启才能生效）
tasknexus-agent service restart

# 卸载服务
tasknexus-agent service uninstall
```

**平台说明:**

- **Windows**: 注册为 Windows Service（服务名: `tasknexus`），通过 SCM 管理，故障自动重启
- **Linux**: 生成 `systemd` unit 文件（`/etc/systemd/system/tasknexus-agent.service`），支持 `systemctl` 管理
- **macOS**: 生成 `launchd` plist 文件（`/Library/LaunchDaemons/com.tasknexus.agent.plist`），开机自动加载

> **注意:** 安装服务时 `--config` 参数必须使用**绝对路径**。

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
```

所有配置均通过配置文件管理，不支持命令行参数覆盖。

配置了 `http_proxy` / `https_proxy` / `no_proxy` 后，Agent 会在执行任务命令以及 `git clone` / `git fetch` 时自动注入 `HTTP_PROXY`、`HTTPS_PROXY`、`NO_PROXY` 以及对应的小写环境变量。

## 开发

```bash
# 运行测试
cargo test

# 开发模式运行
cargo run -- run --config config.yaml
```

## License

MIT
