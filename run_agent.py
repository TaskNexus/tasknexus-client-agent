#!/usr/bin/env python3
"""
TaskNexus Agent 入口点脚本

此脚本作为 PyInstaller 打包的入口点，
用于解决相对导入问题。
"""

import sys
import os

# 修复 Windows 控制台编码问题
if sys.platform == 'win32':
    # 设置 stdout/stderr 为 UTF-8 编码
    if hasattr(sys.stdout, 'reconfigure'):
        sys.stdout.reconfigure(encoding='utf-8', errors='replace')
    if hasattr(sys.stderr, 'reconfigure'):
        sys.stderr.reconfigure(encoding='utf-8', errors='replace')
    # 设置环境变量
    os.environ.setdefault('PYTHONIOENCODING', 'utf-8')

# 确保 agent 包可以被正确导入
if getattr(sys, 'frozen', False):
    # 如果是 PyInstaller 打包的可执行文件
    application_path = os.path.dirname(sys.executable)
else:
    # 如果是直接运行脚本
    application_path = os.path.dirname(os.path.abspath(__file__))

# 将应用目录添加到 Python 路径
if application_path not in sys.path:
    sys.path.insert(0, application_path)

# 使用绝对导入启动主程序
from agent.main import main

if __name__ == '__main__':
    main()
