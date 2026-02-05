#!/usr/bin/env python3
"""
本地构建脚本 - 用于在本地构建可执行文件

Usage:
    python build.py              # 构建当前平台的可执行文件
    python build.py --clean      # 清理构建目录后重新构建
    python build.py --onedir     # 构建为目录形式（而非单文件）
"""

import argparse
import platform
import shutil
import subprocess
import sys
from pathlib import Path


def get_platform_name():
    """获取当前平台名称"""
    system = platform.system().lower()
    machine = platform.machine().lower()
    
    if system == 'windows':
        return 'windows-amd64'
    elif system == 'linux':
        if 'arm' in machine or 'aarch64' in machine:
            return 'linux-arm64'
        return 'linux-amd64'
    elif system == 'darwin':
        if 'arm' in machine:
            return 'macos-arm64'
        return 'macos-amd64'
    else:
        return f'{system}-{machine}'


def clean_build_dirs():
    """清理构建目录"""
    dirs_to_clean = ['build', 'dist', '__pycache__']
    
    for dir_name in dirs_to_clean:
        dir_path = Path(dir_name)
        if dir_path.exists():
            print(f"清理: {dir_path}")
            shutil.rmtree(dir_path)
    
    # 清理 .spec 生成的临时文件
    for spec_file in Path('.').glob('*.spec.bak'):
        spec_file.unlink()


def install_dependencies():
    """安装构建依赖"""
    print("安装构建依赖...")
    subprocess.run([sys.executable, '-m', 'pip', 'install', 'pyinstaller'], check=True)
    subprocess.run([sys.executable, '-m', 'pip', 'install', '-e', '.'], check=True)


def build_executable(onedir=False):
    """构建可执行文件"""
    print(f"构建平台: {get_platform_name()}")
    
    if onedir:
        # 目录模式 - 适合调试
        data_sep = ';' if platform.system() == 'Windows' else ':'
        cmd = [
            sys.executable, '-m', 'PyInstaller',
            '--name', 'tasknexus-agent',
            '--onedir',
            '--console',
            f'--add-data=config.example.yaml{data_sep}.',
            f'--add-data=agent{data_sep}agent',
            '--hidden-import', 'websockets.legacy.client',
            '--hidden-import', 'asyncio.selector_events',
            '--hidden-import', 'git.cmd',
            '--hidden-import', 'git.repo',
            'run_agent.py'
        ]
    else:
        # 使用 spec 文件构建单文件
        cmd = [sys.executable, '-m', 'PyInstaller', 'tasknexus-agent.spec']
    
    print(f"执行: {' '.join(cmd)}")
    subprocess.run(cmd, check=True)


def verify_build():
    """验证构建结果"""
    if platform.system() == 'Windows':
        exe_path = Path('dist/tasknexus-agent.exe')
    else:
        exe_path = Path('dist/tasknexus-agent')
    
    if not exe_path.exists():
        print(f"错误: 未找到构建产物 {exe_path}")
        return False
    
    print(f"\n构建成功! 可执行文件: {exe_path}")
    print(f"文件大小: {exe_path.stat().st_size / 1024 / 1024:.2f} MB")
    
    # 测试运行
    print("\n测试运行 --help:")
    result = subprocess.run([str(exe_path), '--help'], capture_output=True, text=True)
    if result.returncode == 0:
        print("验证通过!")
        return True
    else:
        print(f"验证失败: {result.stderr}")
        return False


def main():
    parser = argparse.ArgumentParser(description='构建 tasknexus-client-agent 可执行文件')
    parser.add_argument('--clean', action='store_true', help='清理构建目录后重新构建')
    parser.add_argument('--onedir', action='store_true', help='构建为目录形式')
    parser.add_argument('--no-verify', action='store_true', help='跳过验证步骤')
    
    args = parser.parse_args()
    
    if args.clean:
        clean_build_dirs()
    
    install_dependencies()
    build_executable(onedir=args.onedir)
    
    if not args.no_verify:
        if not verify_build():
            sys.exit(1)
    
    print("\n构建完成!")


if __name__ == '__main__':
    main()
