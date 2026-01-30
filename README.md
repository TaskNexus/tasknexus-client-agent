# TaskNexus Agent

TaskNexus 客户端代理应用程序，用于连接 TaskNexus 服务器并执行远程任务。

## 功能特性

- 🔌 **WebSocket 连接** - 与服务器保持实时双向通信
- 💓 **心跳机制** - 自动维持连接状态
- 📦 **Git 仓库管理** - 自动拉取和更新任务脚本仓库  
- ⚡ **命令执行** - 在本地环境执行服务器分发的任务
- 📊 **结果上报** - 实时上报任务执行状态和结果

## 安装

### 从源码安装

```bash
cd tasknexus_agent
pip install -e .
```

### 使用 pip 安装

```bash
pip install tasknexus-agent
```

## 使用方法

### 命令行启动

```bash
# 基本启动
tasknexus-agent --server ws://localhost:8001/ws/agent/ --token YOUR_TOKEN

# 完整参数
tasknexus-agent \
    --server ws://your-server:8001/ws/agent/ \
    --token YOUR_AGENT_TOKEN \
    --name my-agent \
    --workdir /path/to/workdir \
    --log-level INFO
```

### 使用配置文件

创建 `config.yaml`:

```yaml
server: ws://localhost:8001/ws/agent/
token: YOUR_AGENT_TOKEN
name: my-agent
workdir: ./workdir
log_level: INFO
heartbeat_interval: 30
```

然后启动:

```bash
tasknexus-agent --config config.yaml
```

## 配置选项

| 参数          | 环境变量               | 默认值      | 描述                 |
| ------------- | ---------------------- | ----------- | -------------------- |
| `--server`    | `TASKNEXUS_SERVER`     | -           | WebSocket 服务器地址 |
| `--token`     | `TASKNEXUS_TOKEN`      | -           | Agent 认证 Token     |
| `--name`      | `TASKNEXUS_AGENT_NAME` | hostname    | Agent 名称           |
| `--workdir`   | `TASKNEXUS_WORKDIR`    | `./workdir` | 工作目录             |
| `--log-level` | `TASKNEXUS_LOG_LEVEL`  | `INFO`      | 日志级别             |
| `--heartbeat` | -                      | `30`        | 心跳间隔(秒)         |

## 工作原理

1. **连接** - Agent 使用 Token 连接到 TaskNexus WebSocket 服务
2. **注册** - 发送系统信息，服务器记录 Agent 状态为在线
3. **心跳** - 定期发送心跳消息保持连接
4. **任务接收** - 接收服务器分发的任务
5. **脚本拉取** - 根据任务配置，克隆或更新 Git 仓库
6. **命令执行** - 在指定目录执行命令
7. **结果上报** - 将执行结果发送回服务器

## 开发

### 运行测试

```bash
pip install -e ".[dev]"
pytest
```

### 代码格式化

```bash
black agent/
```

## 许可证

MIT License
