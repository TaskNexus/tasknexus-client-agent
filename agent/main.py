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
        self.task_runner = TaskRunner(config.workdir)
        self.client: Optional[AgentClient] = None
        self.running_task = None  # Can be str (UUID) or int
    
    async def start(self):
        """启动 Agent"""
        logger.info(f"Starting TaskNexus Agent: {self.config.name}")
        logger.info(f"Server: {self.config.server}")
        logger.info(f"Work directory: {self.config.workdir}")
        
        # 确保工作目录存在
        self.config.workdir.mkdir(parents=True, exist_ok=True)
        
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
        command = message.get('command', '')
        client_repo_url = message.get('client_repo_url', '')
        client_repo_ref = message.get('client_repo_ref', 'main')
        parameters = message.get('parameters', {})
        environment = message.get('environment', {})
        timeout = message.get('timeout', 3600)
        working_dir = message.get('working_dir', '')
        
        logger.info(f"Received task {task_id}: {command}")
        
        if self.running_task:
            logger.warning(f"Already running task {self.running_task}, rejecting new task")
            await self.client.send_task_failed(
                task_id, 
                f"Agent is busy running task {self.running_task}"
            )
            return
        
        self.running_task = task_id
        
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
                client_repo_url=client_repo_url if client_repo_url else None,
                client_repo_ref=client_repo_ref,
                parameters=parameters,
                environment=environment,
                timeout=timeout,
                working_dir=working_dir if working_dir else None,
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
            self.running_task = None


@click.command()
@click.option(
    '--server', '-s',
    help='WebSocket 服务器地址 (例如: ws://localhost:8001/ws/agent/)'
)
@click.option(
    '--token', '-t',
    help='Agent 认证 Token'
)
@click.option(
    '--name', '-n',
    help='Agent 名称 (默认使用主机名)'
)
@click.option(
    '--workdir', '-w',
    help='工作目录 (默认: ./workdir)'
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
    token: Optional[str],
    name: Optional[str],
    workdir: Optional[str],
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
        token=token,
        name=name,
        workdir=workdir,
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
