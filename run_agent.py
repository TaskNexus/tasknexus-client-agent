#!/usr/bin/env python3
"""
TaskNexus Agent 入口点脚本

此脚本作为 PyInstaller 打包的入口点，
用于解决相对导入问题。
"""

import sys
import os

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
