"""
配置管理模块

处理 Agent 的配置，支持命令行参数、配置文件和环境变量。
"""

import os
import socket
import platform
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

import yaml


@dataclass
class AgentConfig:
    """Agent 配置类"""
    
    # 连接配置
    server: str = ""
    token: str = ""
    
    # Agent 信息
    name: str = field(default_factory=lambda: socket.gethostname())
    
    # 工作目录
    workdir: Path = field(default_factory=lambda: Path("./workdir"))
    
    # 日志配置
    log_level: str = "INFO"
    log_file: Optional[Path] = None
    
    # 心跳配置
    heartbeat_interval: int = 30  # 秒
    
    # 重连配置
    reconnect_interval: int = 5  # 秒
    max_reconnect_attempts: int = -1  # -1 表示无限重试
    
    # 任务配置
    task_timeout: int = 3600  # 默认任务超时(秒)
    
    def __post_init__(self):
        """初始化后处理"""
        if isinstance(self.workdir, str):
            self.workdir = Path(self.workdir)
        if isinstance(self.log_file, str):
            self.log_file = Path(self.log_file)
    
    @classmethod
    def from_file(cls, config_path: str) -> "AgentConfig":
        """从配置文件加载配置"""
        with open(config_path, 'r', encoding='utf-8') as f:
            data = yaml.safe_load(f)
        
        return cls(
            server=data.get('server', ''),
            token=data.get('token', ''),
            name=data.get('name', socket.gethostname()),
            workdir=Path(data.get('workdir', './workdir')),
            log_level=data.get('log_level', 'INFO'),
            log_file=Path(data['log_file']) if data.get('log_file') else None,
            heartbeat_interval=data.get('heartbeat_interval', 30),
            reconnect_interval=data.get('reconnect_interval', 5),
            max_reconnect_attempts=data.get('max_reconnect_attempts', -1),
            task_timeout=data.get('task_timeout', 3600),
        )
    
    @classmethod
    def from_env(cls) -> "AgentConfig":
        """从环境变量加载配置"""
        return cls(
            server=os.environ.get('TASKNEXUS_SERVER', ''),
            token=os.environ.get('TASKNEXUS_TOKEN', ''),
            name=os.environ.get('TASKNEXUS_AGENT_NAME', socket.gethostname()),
            workdir=Path(os.environ.get('TASKNEXUS_WORKDIR', './workdir')),
            log_level=os.environ.get('TASKNEXUS_LOG_LEVEL', 'INFO'),
            heartbeat_interval=int(os.environ.get('TASKNEXUS_HEARTBEAT_INTERVAL', '30')),
        )
    
    def validate(self) -> list[str]:
        """验证配置，返回错误列表"""
        errors = []
        
        if not self.server:
            errors.append("Server URL is required")
        if not self.token:
            errors.append("Token is required")
        if not self.server.startswith(('ws://', 'wss://')):
            errors.append("Server URL must start with ws:// or wss://")
        
        return errors
    
    def get_system_info(self) -> dict:
        """获取系统信息，用于心跳上报"""
        return {
            "hostname": socket.gethostname(),
            "platform": platform.system(),
            "platform_version": platform.version(),
            "platform_release": platform.release(),
            "architecture": platform.machine(),
            "python_version": platform.python_version(),
            "agent_version": "0.1.0",
            "ip_address": self._get_local_ip(),
        }
    
    def _get_local_ip(self) -> str:
        """获取本机 IP 地址"""
        try:
            # 创建一个 UDP socket 连接到外部地址来获取本机 IP
            s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            s.connect(("8.8.8.8", 80))
            ip = s.getsockname()[0]
            s.close()
            return ip
        except Exception:
            return "127.0.0.1"


def load_config(
    config_file: Optional[str] = None,
    server: Optional[str] = None,
    token: Optional[str] = None,
    name: Optional[str] = None,
    workdir: Optional[str] = None,
    log_level: Optional[str] = None,
    heartbeat_interval: Optional[int] = None,
) -> AgentConfig:
    """
    加载配置，优先级：命令行参数 > 配置文件 > 环境变量 > 默认值
    """
    # 从环境变量开始
    config = AgentConfig.from_env()
    
    # 如果有配置文件，覆盖
    if config_file and Path(config_file).exists():
        file_config = AgentConfig.from_file(config_file)
        if file_config.server:
            config.server = file_config.server
        if file_config.token:
            config.token = file_config.token
        if file_config.name != socket.gethostname():
            config.name = file_config.name
        if file_config.workdir != Path("./workdir"):
            config.workdir = file_config.workdir
        if file_config.log_level != "INFO":
            config.log_level = file_config.log_level
        if file_config.heartbeat_interval != 30:
            config.heartbeat_interval = file_config.heartbeat_interval
    
    # 命令行参数覆盖
    if server:
        config.server = server
    if token:
        config.token = token
    if name:
        config.name = name
    if workdir:
        config.workdir = Path(workdir)
    if log_level:
        config.log_level = log_level
    if heartbeat_interval:
        config.heartbeat_interval = heartbeat_interval
    
    return config
