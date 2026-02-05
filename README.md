# TaskNexus Agent (Rust)

TaskNexus 客户端代理，用于连接 TaskNexus 服务器并执行远程任务。

## 特性

- 🚀 **高性能** - 使用 Rust 编写，二进制文件仅 ~2.5 MB
- 🔄 **自动重连** - 断线时自动尝试重新连接
- ❤️ **心跳检测** - 定期发送心跳保持连接
- 📁 **Git 支持** - 自动 clone/pull 项目仓库
- 🖥️ **跨平台** - 支持 Windows, Linux, macOS

## 安装

### 从 Release 下载

前往 [Releases](https://github.com/yourorg/TaskNexus/releases) 下载对应平台的二进制文件。

### 从源码编译

```bash
# 需要 Rust 1.70+
cargo build --release
```

## 使用方法

### 命令行参数

```bash
tasknexus-agent [OPTIONS]

Options:
  -s, --server <URL>           WebSocket 服务器地址
  -n, --name <NAME>            Agent 名称 (默认使用主机名)
  -w, --workspaces-path <DIR>  工作空间根目录 (默认: ./workspaces)
  -c, --config <FILE>          配置文件路径
  -l, --log-level <LEVEL>      日志级别 [default: INFO]
      --heartbeat <SECS>       心跳间隔秒数 [default: 30]
```

### 配置文件

复制 `config.example.yaml` 并根据需要修改：

```yaml
server: ws://localhost:8001/ws/agent/
name: My-Agent
workspaces_path: ./workspaces
log_level: INFO
heartbeat_interval: 30
```

### 环境变量

- `TASKNEXUS_SERVER` - WebSocket 服务器地址
- `TASKNEXUS_AGENT_NAME` - Agent 名称
- `TASKNEXUS_WORKSPACES_PATH` - 工作空间路径
- `TASKNEXUS_LOG_LEVEL` - 日志级别

配置优先级: 命令行参数 > 环境变量 > 配置文件 > 默认值

## 开发

```bash
# 运行测试
cargo test

# 开发模式运行
cargo run -- -s ws://localhost:8001/ws/agent/ -n dev-agent
```

## License

MIT
