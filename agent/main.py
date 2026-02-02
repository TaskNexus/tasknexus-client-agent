"""
TaskNexus Agent 主入口

提供命令行接口和 Agent 运行逻辑。
"""

import asyncio
import logging
import sys
from pathlib import Path
from typing import Optional

import click

from .config import AgentConfig, load_config
from .client import AgentClient
from .executor import TaskRunner

# 配置日志
def setup_logging(log_level: str, log_file: Optional[Path] = None):
    """配置日志"""
    level = getattr(logging, log_level.upper(), logging.INFO)
    
    # 创建格式化器
    formatter = logging.Formatter(
        '%(asctime)s - %(name)s - %(levelname)s - %(message)s',
        datefmt='%Y-%m-%d %H:%M:%S'
    )
    
    # 配置根日志
    root_logger = logging.getLogger()
    root_logger.setLevel(level)
    
    # 控制台处理器
    console_handler = logging.StreamHandler(sys.stdout)
    console_handler.setLevel(level)
    console_handler.setFormatter(formatter)
    root_logger.addHandler(console_handler)
    
    # 文件处理器
    if log_file:
        log_file.parent.mkdir(parents=True, exist_ok=True)
        file_handler = logging.FileHandler(log_file, encoding='utf-8')
        file_handler.setLevel(level)
        file_handler.setFormatter(formatter)
        root_logger.addHandler(file_handler)
    
    # 设置第三方库日志级别
    logging.getLogger('websockets').setLevel(logging.WARNING)
    logging.getLogger('git').setLevel(logging.WARNING)


logger = logging.getLogger("tasknexus.agent")


class Agent:
    """
    TaskNexus Agent 主类
    
    协调各个组件的工作。
    """
    
    def __init__(self, config: AgentConfig):
        self.config = config
        self.task_runner = TaskRunner(config.workspaces_path)
        self.client: Optional[AgentClient] = None
        self.running_tasks = {}  # workspace_name -> task_id
    
    async def start(self):
        """启动 Agent"""
        logger.info(f"Starting TaskNexus Agent: {self.config.name}")
        logger.info(f"Server: {self.config.server}")
        logger.info(f"Workspaces path: {self.config.workspaces_path}")
        
        # 确保工作目录存在
        self.config.workspaces_path.mkdir(parents=True, exist_ok=True)
        
        # 创建 WebSocket 客户端
        self.client = AgentClient(
            config=self.config,
            on_task_dispatch=self._handle_task_dispatch,
            on_connected=self._handle_connected,
            on_disconnected=self._handle_disconnected,
        )
        
        # 运行客户端
        await self.client.run()
    
    async def stop(self):
        """停止 Agent"""
        logger.info("Stopping TaskNexus Agent...")
        if self.client:
            await self.client.disconnect()
    
    async def _handle_connected(self):
        """处理连接成功"""
        logger.info("Connected to TaskNexus server")
    
    async def _handle_disconnected(self):
        """处理断开连接"""
        logger.warning("Disconnected from TaskNexus server")
    
    async def _handle_task_dispatch(self, message: dict):
        """处理任务分发"""
        task_id = message.get('task_id')
        workspace_name = message.get('workspace_name', 'default')
        command = message.get('command', '')
        client_repo_url = message.get('client_repo_url', '')
        client_repo_ref = message.get('client_repo_ref', 'main')
        timeout = message.get('timeout', 3600)
        
        logger.info(f"Received task {task_id} for workspace '{workspace_name}': {command}")
        logger.info(f"[DEBUG] Received message keys: {list(message.keys())}")
        logger.info(f"[DEBUG] client_repo_url='{client_repo_url}', client_repo_ref='{client_repo_ref}'")
        
        # 检查该 workspace 是否已有运行中的任务
        if workspace_name in self.running_tasks:
            logger.warning(f"Workspace '{workspace_name}' already running task {self.running_tasks[workspace_name]}, rejecting new task")
            await self.client.send_task_failed(
                task_id, 
                f"Workspace '{workspace_name}' is busy running task {self.running_tasks[workspace_name]}"
            )
            return
        
        self.running_tasks[workspace_name] = task_id
        
        try:
            # 通知任务开始
            await self.client.send_task_started(task_id)
            
            # 输出回调
            async def on_output(line: str, is_stderr: bool):
                await self.client.send_task_progress(task_id, line)
            
            # 执行任务
            result = await self.task_runner.run_task(
                task_id=task_id,
                command=command,
                workspace_name=workspace_name,
                client_repo_url=client_repo_url if client_repo_url else None,
                client_repo_ref=client_repo_ref,
                timeout=timeout,
                on_output=on_output,
            )
            
            # 发送结果
            if result.exit_code == 0:
                await self.client.send_task_completed(
                    task_id,
                    result.exit_code,
                    result.stdout,
                    result.stderr,
                )
                logger.info(f"Task {task_id} completed successfully")
            else:
                if result.timed_out:
                    await self.client.send_task_failed(
                        task_id,
                        f"Task timed out after {timeout} seconds"
                    )
                else:
                    await self.client.send_task_completed(
                        task_id,
                        result.exit_code,
                        result.stdout,
                        result.stderr,
                    )
                logger.warning(f"Task {task_id} failed with exit code {result.exit_code}")
                
        except Exception as e:
            logger.error(f"Task {task_id} execution error: {e}")
            await self.client.send_task_failed(task_id, str(e))
        
        finally:
            if workspace_name in self.running_tasks:
                del self.running_tasks[workspace_name]


@click.command()
@click.option(
    '--server', '-s',
    help='WebSocket 服务器地址 (例如: ws://localhost:8001/ws/agent/)'
)
@click.option(
    '--name', '-n',
    help='Agent 名称 (默认使用主机名)'
)
@click.option(
    '--workspaces-path', '-w',
    help='工作空间根目录 (默认: ./workspaces)'
)
@click.option(
    '--config', '-c', 'config_file',
    type=click.Path(exists=True),
    help='配置文件路径'
)
@click.option(
    '--log-level', '-l',
    type=click.Choice(['DEBUG', 'INFO', 'WARNING', 'ERROR'], case_sensitive=False),
    default='INFO',
    help='日志级别'
)
@click.option(
    '--heartbeat', '-h', 'heartbeat_interval',
    type=int,
    default=30,
    help='心跳间隔(秒)'
)
def main(
    server: Optional[str],
    name: Optional[str],
    workspaces_path: Optional[str],
    config_file: Optional[str],
    log_level: str,
    heartbeat_interval: int,
):
    """
    TaskNexus Agent - 客户端代理
    
    连接到 TaskNexus 服务器，接收并执行任务。
    """
    # 加载配置
    config = load_config(
        config_file=config_file,
        server=server,
        name=name,
        workspaces_path=workspaces_path,
        log_level=log_level,
        heartbeat_interval=heartbeat_interval,
    )
    
    # 配置日志
    setup_logging(config.log_level, config.log_file)
    
    # 验证配置
    errors = config.validate()
    if errors:
        for error in errors:
            click.echo(f"配置错误: {error}", err=True)
        sys.exit(1)
    
    # 创建并运行 Agent
    agent = Agent(config)
    
    try:
        asyncio.run(agent.start())
    except KeyboardInterrupt:
        click.echo("\n正在停止 Agent...")
        asyncio.run(agent.stop())
    except Exception as e:
        logger.error(f"Agent 运行失败: {e}")
        sys.exit(1)


if __name__ == '__main__':
    main()
