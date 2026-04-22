# TaskNexus Client Agent Linux 部署注意事项

本文整理了 TaskNexus 客户端代理在 Linux 服务器上的推荐部署方式，以及
通过 `systemd` 托管、远程执行 Python 项目时最常见的坑位和排查方法。

## 推荐目录结构

- Agent 可执行文件目录：`/var/local/tasknexus`
- Python 虚拟环境目录：`/var/local/tasknexus/.venv`
- 配置文件：`/var/local/tasknexus/config.yaml`
- 服务用户：建议使用专门的非 root 用户，例如 `tasknexus`
- 服务管理方式：`systemd`

把虚拟环境放在 Agent 目录内部，路径更清晰，也能避免把项目依赖装进系统
Python 环境。

## 必装依赖

部署前先安装 Python 虚拟环境相关包：

```bash
sudo apt update
sudo apt install -y python3-venv python3-full
```

在 Debian / Ubuntu 上，`python3-venv` 和 `python3-full` 很重要，因为
Agent 在执行 Python 任务前，可能需要根据 `requirements.txt` 安装依赖。

## 重要：不要依赖系统级 pip 安装

不要把 Python 包直接装进系统 Python。较新的 Debian / Ubuntu 默认启用了
PEP 668，下面这类命令会被直接拒绝：

```bash
pip install -r requirements.txt
python3 -m pip install -r requirements.txt
```

典型报错如下：

```text
externally-managed-environment
```

推荐做法：

- 为 Agent 创建独立的 Python 虚拟环境。
- 通过 `systemd` 的 `PATH` 和 `VIRTUAL_ENV` 让服务始终使用该虚拟环境。

不建议长期使用的临时方案：

- `--break-system-packages`
- `PIP_BREAK_SYSTEM_PACKAGES=1`

这些方式虽然能临时绕过限制，但会污染系统 Python，后续维护风险较高。

## 创建虚拟环境

如果 Agent 服务以 `tasknexus` 用户运行，可以这样创建：

```bash
sudo install -d -o tasknexus -g tasknexus /var/local/tasknexus
sudo -u tasknexus python3 -m venv /var/local/tasknexus/.venv
sudo -u tasknexus /var/local/tasknexus/.venv/bin/python -m pip install -U pip setuptools wheel
```

如果服务不是以 `tasknexus` 用户运行，请把上面的 `tasknexus` 替换成实际的
服务用户。

## 权限注意事项

虚拟环境目录必须对服务用户可写。

例如，下面这个命令对普通用户就是错误的：

```bash
sudo -u tasknexus python3 -m venv /venv
```

原因是 `/venv` 位于根目录下，普通用户没有权限在 `/` 下创建目录。

正确做法是把虚拟环境放到当前服务用户可写的路径，例如：

```bash
/var/local/tasknexus/.venv
```

## systemd 配置方式

如果服务已经存在，优先使用 `systemd` 的 drop-in 覆盖配置，而不是直接改原始
unit 文件。

先确认服务的准确名称：

```bash
systemctl list-unit-files --type=service | grep -i tasknexus
systemctl list-units --type=service --all | grep -i tasknexus
```

后续所有路径都必须使用 `systemctl` 查到的准确服务名。比如服务名是
`tasknexus.agent.service`，那 drop-in 目录也必须完全一致：

```bash
sudo mkdir -p /etc/systemd/system/tasknexus.agent.service.d
sudo tee /etc/systemd/system/tasknexus.agent.service.d/python-venv.conf >/dev/null <<'EOF'
[Service]
Environment="VIRTUAL_ENV=/var/local/tasknexus/.venv"
Environment="PATH=/var/local/tasknexus/.venv/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
EOF
```

然后重新加载并重启服务：

```bash
sudo systemctl daemon-reload
sudo systemctl restart tasknexus.agent.service
sudo systemctl status tasknexus.agent.service --no-pager -l
```

## 验证虚拟环境是否生效

先看服务是否已经拿到了正确的环境变量：

```bash
sudo systemctl show tasknexus.agent.service -p Environment
```

正常情况下，输出中应包含：

```text
VIRTUAL_ENV=/var/local/tasknexus/.venv
PATH=/var/local/tasknexus/.venv/bin:...
```

也可以进一步验证服务用户实际解析到的 `python3` 和 `pip`：

```bash
sudo -u tasknexus env PATH=/var/local/tasknexus/.venv/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin bash -lc 'which python3; python3 -m pip -V'
```

预期结果：

- `python3` 指向 `/var/local/tasknexus/.venv/bin/python3`
- `pip` 指向 `/var/local/tasknexus/.venv/lib/...`

## 推荐部署检查清单

1. 安装 `python3-venv` 和 `python3-full`。
2. 确认 Agent 服务实际运行用户。
3. 在 Agent 目录下创建专用 `.venv`。
4. 确认 `.venv` 目录归服务用户所有。
5. 通过 `systemd` drop-in 注入 `VIRTUAL_ENV` 和 `PATH`。
6. 执行 `systemctl daemon-reload`。
7. 重启服务。
8. 验证 `Environment`、`python3`、`pip` 是否都已切到虚拟环境。
9. 重新触发一次 Python 任务，确认不再出现 PEP 668 报错。

## 常见问题

### `externally-managed-environment`

原因：

- Agent 调用了系统 `pip`，而不是虚拟环境中的 `pip`。

处理：

- 确认服务的 `PATH` 以 `/var/local/tasknexus/.venv/bin` 开头。
- 修改 drop-in 后务必重启服务。

### `Error: [Errno 13] Permission denied: '/venv'`

原因：

- 虚拟环境目标目录对服务用户不可写。

处理：

- 把虚拟环境建在服务用户可写目录内，例如
  `/var/local/tasknexus/.venv`。

### 写入 `python-venv.conf` 时提示 `No such file or directory`

原因：

- drop-in 目录尚未创建。

处理：

```bash
sudo mkdir -p /etc/systemd/system/<service-name>.service.d
```

然后重新创建 drop-in 文件。

### 服务重启成功，但 `python3` 仍然指向 `/usr/bin/python3`

原因：

- drop-in 路径里用了错误的服务名。
- 没有执行 `daemon-reload`。
- 修改配置后没有重启服务。

处理：

- 重新用 `systemctl list-unit-files` 确认服务名。
- 重新执行 `sudo systemctl daemon-reload`。
- 重新执行 `sudo systemctl restart <service-name>`。

## 运维建议

- 尽量使用专门的非 root 服务用户运行 Agent。
- 可执行文件、配置文件和 `.venv` 最好放在同一目录树下，便于维护。
- 不建议依赖 shell profile 里的 `source venv/bin/activate` 来影响服务行为。
  对 `systemd` 服务来说，直接设置 `PATH` 和 `VIRTUAL_ENV` 更直观、也更稳定。
- 如果通过 SSH 远程维护服务器，遇到主机指纹变化时，先核对新指纹，再删除旧
  的 `known_hosts` 记录。
