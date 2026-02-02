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
    
    def __init__(self, workspaces_path: Path, executor: Optional[CommandExecutor] = None):
        """
        初始化任务运行器
        
        Args:
            workspaces_path: 工作空间根目录
            executor: 命令执行器实例
        """
        self.workspaces_path = workspaces_path
        self.executor = executor or CommandExecutor()
        self.workspaces_path.mkdir(parents=True, exist_ok=True)
        
        # 延迟导入，避免循环依赖
        from .git_manager import GitManager
        self.git_manager = GitManager(workspaces_path)
    
    async def run_task(
        self,
        task_id,  # Can be str (UUID) or int
        command: str,
        workspace_name: str = "default",
        client_repo_url: Optional[str] = None,
        client_repo_ref: str = "main",
        timeout: int = 3600,
        on_output: Optional[callable] = None,
    ) -> ExecutionResult:
        """
        运行任务
        
        Args:
            task_id: 任务 ID
            command: 要执行的命令
            workspace_name: 工作空间名称
            client_repo_url: Git 仓库 URL
            client_repo_ref: 分支或提交
            timeout: 超时时间
            on_output: 输出回调
        
        Returns:
            ExecutionResult: 执行结果
        """
        logger.info(f"Running task {task_id} in workspace '{workspace_name}': {command}")
        logger.info(f"[DEBUG] client_repo_url={client_repo_url}, client_repo_ref={client_repo_ref}")
        
        # 获取/创建工作空间目录
        workspace_dir = self.workspaces_path / workspace_name
        workspace_dir.mkdir(parents=True, exist_ok=True)
        
        # 确定执行目录
        exec_dir = workspace_dir
        
        if client_repo_url:
            # 从 URL 提取仓库名称
            repo_name = client_repo_url.rstrip('/').split('/')[-1]
            if repo_name.endswith('.git'):
                repo_name = repo_name[:-4]
            
            repo_path = workspace_dir / repo_name
            
            if not repo_path.exists():
                # 自动 clone 仓库
                logger.info(f"Cloning repository {client_repo_url} to {repo_path}")
                try:
                    clone_result = await self._clone_repo(client_repo_url, repo_path, client_repo_ref)
                    if clone_result.exit_code != 0:
                        return clone_result
                except Exception as e:
                    logger.error(f"Failed to clone repository: {e}")
                    return ExecutionResult(
                        exit_code=-1,
                        stdout='',
                        stderr=f"Failed to clone repository: {e}",
                    )
            else:
                # 尝试 pull 最新代码
                logger.info(f"Updating repository: {repo_path}")
                try:
                    await self._update_repo(repo_path, client_repo_ref)
                except Exception as e:
                    logger.warning(f"Failed to update repository (will continue anyway): {e}")
            
            exec_dir = repo_path
        
        # 设置环境变量
        task_env = {
            'TASKNEXUS_TASK_ID': str(task_id),
            'TASKNEXUS_WORKSPACE': workspace_name,
        }
        
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
    
    async def _clone_repo(self, repo_url: str, target_path: Path, ref: str = "main") -> ExecutionResult:
        """Clone a git repository"""
        # Use just the directory name as target since we're setting working_dir to parent
        repo_name = target_path.name
        clone_cmd = f"git clone --depth 1 --branch {ref} {repo_url} {repo_name}"
        logger.info(f"Cloning: {clone_cmd} (in {target_path.parent})")
        return await self.executor.execute(
            command=clone_cmd,
            working_dir=target_path.parent,
            timeout=300,  # 5 minutes for clone
        )
    
    async def _update_repo(self, repo_path: Path, ref: str = "main") -> ExecutionResult:
        """Update a git repository"""
        # Fetch and reset to the latest
        update_cmd = f"git fetch origin {ref} && git reset --hard origin/{ref}"
        return await self.executor.execute(
            command=update_cmd,
            working_dir=repo_path,
            timeout=120,  # 2 minutes for update
        )

