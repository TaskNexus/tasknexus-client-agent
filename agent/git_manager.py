"""
Git 仓库管理模块

处理任务脚本仓库的克隆和更新。
"""

import logging
import os
from pathlib import Path
from typing import Optional

try:
    import git
    from git import Repo, GitCommandError
    GIT_AVAILABLE = True
except ImportError:
    GIT_AVAILABLE = False

logger = logging.getLogger("tasknexus.agent")


class GitManager:
    """
    Git 仓库管理器
    
    负责:
    - 克隆任务脚本仓库
    - 更新到指定分支或提交
    - 管理多个仓库
    """
    
    def __init__(self, workdir: Path):
        """
        初始化 Git 管理器
        
        Args:
            workdir: 工作目录，仓库将克隆到此目录下
        """
        if not GIT_AVAILABLE:
            raise RuntimeError("GitPython is required but not installed. Run: pip install gitpython")
        
        self.workdir = workdir
        self.repos_dir = workdir / "repos"
        self.repos_dir.mkdir(parents=True, exist_ok=True)
    
    def get_repo_path(self, repo_url: str) -> Path:
        """
        根据仓库 URL 获取本地路径
        
        Args:
            repo_url: Git 仓库 URL
        
        Returns:
            本地仓库路径
        """
        # 从 URL 提取仓库名称
        repo_name = self._extract_repo_name(repo_url)
        return self.repos_dir / repo_name
    
    def ensure_repo(
        self, 
        repo_url: str, 
        ref: str = "main",
        force_update: bool = False
    ) -> Path:
        """
        确保仓库存在并切换到指定版本
        
        Args:
            repo_url: Git 仓库 URL
            ref: 分支名称、标签或提交哈希
            force_update: 是否强制更新（丢弃本地更改）
        
        Returns:
            仓库本地路径
        """
        repo_path = self.get_repo_path(repo_url)
        
        if repo_path.exists():
            logger.info(f"Repository exists at {repo_path}, updating...")
            self._update_repo(repo_path, repo_url, ref, force_update)
        else:
            logger.info(f"Cloning repository {repo_url} to {repo_path}...")
            self._clone_repo(repo_url, repo_path, ref)
        
        return repo_path
    
    def _clone_repo(self, repo_url: str, repo_path: Path, ref: str = "main"):
        """
        克隆仓库
        
        Args:
            repo_url: Git 仓库 URL
            repo_path: 本地路径
            ref: 分支或提交
        """
        try:
            # 克隆仓库
            repo = Repo.clone_from(
                repo_url, 
                repo_path,
                branch=ref if self._is_branch_name(ref) else None,
            )
            
            # 如果 ref 不是分支名（可能是提交哈希），切换到该提交
            if not self._is_branch_name(ref) and len(ref) >= 7:
                repo.git.checkout(ref)
            
            logger.info(f"Successfully cloned {repo_url}")
            
        except GitCommandError as e:
            logger.error(f"Failed to clone repository: {e}")
            raise RuntimeError(f"Git clone failed: {e}")
    
    def _update_repo(
        self, 
        repo_path: Path, 
        repo_url: str, 
        ref: str, 
        force_update: bool
    ):
        """
        更新已存在的仓库
        
        Args:
            repo_path: 本地仓库路径
            repo_url: 远程仓库 URL
            ref: 目标分支或提交
            force_update: 是否强制更新
        """
        try:
            repo = Repo(repo_path)
            
            # 确保远程 URL 正确
            if 'origin' in [r.name for r in repo.remotes]:
                origin = repo.remotes.origin
                if origin.url != repo_url:
                    origin.set_url(repo_url)
            else:
                repo.create_remote('origin', repo_url)
            
            origin = repo.remotes.origin
            
            # 获取最新
            logger.debug("Fetching latest changes...")
            origin.fetch()
            
            # 如果强制更新，丢弃本地更改
            if force_update:
                repo.git.reset('--hard')
                repo.git.clean('-fd')
            
            # 切换到目标 ref
            if self._is_branch_name(ref):
                # 分支名称
                if ref in [h.name for h in repo.heads]:
                    # 本地分支存在
                    repo.heads[ref].checkout()
                    repo.git.pull('origin', ref)
                else:
                    # 检出远程分支
                    repo.git.checkout('-b', ref, f'origin/{ref}')
            else:
                # 可能是提交哈希
                repo.git.checkout(ref)
            
            logger.info(f"Repository updated to {ref}")
            
        except GitCommandError as e:
            logger.error(f"Failed to update repository: {e}")
            raise RuntimeError(f"Git update failed: {e}")
    
    def _extract_repo_name(self, repo_url: str) -> str:
        """
        从 URL 提取仓库名称
        
        Args:
            repo_url: Git 仓库 URL
        
        Returns:
            仓库名称
        """
        # 移除 .git 后缀
        url = repo_url.rstrip('/')
        if url.endswith('.git'):
            url = url[:-4]
        
        # 获取最后一个路径部分
        return url.split('/')[-1]
    
    def _is_branch_name(self, ref: str) -> bool:
        """
        判断 ref 是否可能是分支名称
        
        简单启发式：
        - 如果包含 /，可能是分支名
        - 如果全是十六进制字符且长度 >= 7，可能是提交哈希
        - 其他情况假设是分支名
        """
        # 常见分支名
        if ref in ['main', 'master', 'develop', 'dev']:
            return True
        
        # 检查是否像提交哈希
        if len(ref) >= 7 and all(c in '0123456789abcdef' for c in ref.lower()):
            return False
        
        return True
    
    def cleanup_repo(self, repo_url: str):
        """
        删除仓库
        
        Args:
            repo_url: Git 仓库 URL
        """
        import shutil
        
        repo_path = self.get_repo_path(repo_url)
        if repo_path.exists():
            shutil.rmtree(repo_path)
            logger.info(f"Removed repository at {repo_path}")
