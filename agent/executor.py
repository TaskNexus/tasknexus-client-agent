"""
命令执行模块

在本地环境中执行服务器分发的命令。
"""

import asyncio
import logging
import os
import subprocess
import sys
from pathlib import Path
from typing import Dict, Optional, Tuple
from dataclasses import dataclass

logger = logging.getLogger("tasknexus.agent")


@dataclass
class ExecutionResult:
    """命令执行结果"""
    exit_code: int
    stdout: str
    stderr: str
    timed_out: bool = False


class CommandExecutor:
    """
    命令执行器
    
    负责在本地环境执行命令并收集结果。
    """
    
    def __init__(self, default_timeout: int = 3600):
        """
        初始化执行器
        
        Args:
            default_timeout: 默认超时时间(秒)
        """
        self.default_timeout = default_timeout
    
    async def execute(
        self,
        command: str,
        working_dir: Optional[Path] = None,
        environment: Optional[Dict[str, str]] = None,
        timeout: Optional[int] = None,
        on_output: Optional[callable] = None,
    ) -> ExecutionResult:
        """
        异步执行命令
        
        Args:
            command: 要执行的命令
            working_dir: 工作目录
            environment: 附加环境变量
            timeout: 超时时间(秒)
            on_output: 输出回调函数，用于实时上报
        
        Returns:
            ExecutionResult: 执行结果
        """
        timeout = timeout or self.default_timeout
        
        # 准备环境变量
        env = os.environ.copy()
        if environment:
            env.update(environment)
        
        # 准备工作目录
        cwd = str(working_dir) if working_dir else None
        
        logger.info(f"Executing command: {command}")
        logger.info(f"Working directory: {cwd}")
        
        try:
            # 创建子进程
            process = await asyncio.create_subprocess_shell(
                command,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                cwd=cwd,
                env=env,
            )
            
            # 收集输出
            stdout_chunks = []
            stderr_chunks = []
            
            async def read_stream(stream, chunks, is_stderr=False):
                """读取流并收集输出"""
                while True:
                    line = await stream.readline()
                    if not line:
                        break
                    
                    decoded = line.decode('utf-8', errors='replace')
                    chunks.append(decoded)
                    
                    # 调用输出回调
                    if on_output:
                        try:
                            await on_output(decoded, is_stderr)
                        except Exception as e:
                            logger.debug(f"Output callback error: {e}")
            
            try:
                # 同时读取 stdout 和 stderr
                await asyncio.wait_for(
                    asyncio.gather(
                        read_stream(process.stdout, stdout_chunks, False),
                        read_stream(process.stderr, stderr_chunks, True),
                    ),
                    timeout=timeout
                )
                
                # 等待进程结束
                await asyncio.wait_for(process.wait(), timeout=5)
                
            except asyncio.TimeoutError:
                logger.warning(f"Command timed out after {timeout} seconds")
                process.kill()
                await process.wait()
                
                return ExecutionResult(
                    exit_code=-1,
                    stdout=''.join(stdout_chunks),
                    stderr=f"Command timed out after {timeout} seconds\n" + ''.join(stderr_chunks),
                    timed_out=True,
                )
            
            return ExecutionResult(
                exit_code=process.returncode,
                stdout=''.join(stdout_chunks),
                stderr=''.join(stderr_chunks),
            )
            
        except Exception as e:
            logger.error(f"Command execution failed: {e}")
            return ExecutionResult(
                exit_code=-1,
                stdout='',
                stderr=str(e),
            )
    
    def execute_sync(
        self,
        command: str,
        working_dir: Optional[Path] = None,
        environment: Optional[Dict[str, str]] = None,
        timeout: Optional[int] = None,
    ) -> ExecutionResult:
        """
        同步执行命令
        
        Args:
            command: 要执行的命令
            working_dir: 工作目录
            environment: 附加环境变量
            timeout: 超时时间(秒)
        
        Returns:
            ExecutionResult: 执行结果
        """
        timeout = timeout or self.default_timeout
        
        # 准备环境变量
        env = os.environ.copy()
        if environment:
            env.update(environment)
        
        # 准备工作目录
        cwd = str(working_dir) if working_dir else None
        
        logger.info(f"Executing command (sync): {command}")
        
        try:
            result = subprocess.run(
                command,
                shell=True,
                capture_output=True,
                text=True,
                cwd=cwd,
                env=env,
                timeout=timeout,
            )
            
            return ExecutionResult(
                exit_code=result.returncode,
                stdout=result.stdout,
                stderr=result.stderr,
            )
            
        except subprocess.TimeoutExpired as e:
            return ExecutionResult(
                exit_code=-1,
                stdout=e.stdout or '',
                stderr=f"Command timed out after {timeout} seconds\n" + (e.stderr or ''),
                timed_out=True,
            )
        except Exception as e:
            return ExecutionResult(
                exit_code=-1,
                stdout='',
                stderr=str(e),
            )


class TaskRunner:
    """
    任务运行器
    
    组合 Git 管理器和命令执行器来运行完整任务。
    """
    
    def __init__(self, workdir: Path, executor: Optional[CommandExecutor] = None):
        """
        初始化任务运行器
        
        Args:
            workdir: 工作目录
            executor: 命令执行器实例
        """
        self.workdir = workdir
        self.executor = executor or CommandExecutor()
        self.repos_dir = workdir / "repos"
        self.repos_dir.mkdir(parents=True, exist_ok=True)
        
        # 延迟导入，避免循环依赖
        from .git_manager import GitManager
        self.git_manager = GitManager(workdir)
    
    async def run_task(
        self,
        task_id,  # Can be str (UUID) or int
        command: str,
        client_repo_url: Optional[str] = None,
        client_repo_ref: str = "main",
        parameters: Optional[Dict] = None,
        environment: Optional[Dict[str, str]] = None,
        timeout: int = 3600,
        working_dir: Optional[str] = None,
        on_output: Optional[callable] = None,
    ) -> ExecutionResult:
        """
        运行任务
        
        Args:
            task_id: 任务 ID
            command: 要执行的命令
            client_repo_url: Git 仓库 URL（用于确定仓库名称）
            client_repo_ref: 分支或提交（测试模式下忽略）
            parameters: 命令参数（JSON 格式）
            environment: 环境变量
            timeout: 超时时间
            working_dir: 工作目录（相对于仓库根目录）
            on_output: 输出回调
        
        Returns:
            ExecutionResult: 执行结果
        """
        logger.info(f"Running task {task_id}: {command}")
        
        # 确定执行目录 - 直接使用 repos 目录下的仓库
        exec_dir = self.repos_dir
        
        if client_repo_url:
            # 从 URL 提取仓库名称
            repo_name = client_repo_url.rstrip('/').split('/')[-1]
            if repo_name.endswith('.git'):
                repo_name = repo_name[:-4]
            
            repo_path = self.repos_dir / repo_name
            
            if repo_path.exists():
                logger.info(f"Using repo: {repo_path}")
                exec_dir = repo_path
            else:
                logger.warning(f"Repo not found: {repo_path}, using repos_dir")
        
        # 如果指定了工作目录，切换到该目录
        if working_dir:
            exec_dir = exec_dir / working_dir
            if not exec_dir.exists():
                logger.error(f"Working directory does not exist: {exec_dir}")
                return ExecutionResult(
                    exit_code=-1,
                    stdout='',
                    stderr=f"Working directory does not exist: {working_dir}",
                )
        
        # 合并环境变量
        task_env = environment or {}
        task_env['TASKNEXUS_TASK_ID'] = str(task_id)
        
        # 如果有参数，将其添加到环境变量
        if parameters:
            for key, value in parameters.items():
                if isinstance(value, (str, int, float, bool)):
                    task_env[f"PARAM_{key.upper()}"] = str(value)
        
        # 执行命令
        result = await self.executor.execute(
            command=command,
            working_dir=exec_dir,
            environment=task_env,
            timeout=timeout,
            on_output=on_output,
        )
        
        # 输出调试信息
        if result.exit_code != 0:
            logger.error(f"Command failed with exit code {result.exit_code}")
            if result.stderr:
                logger.error(f"stderr: {result.stderr[:500]}")
        
        return result
