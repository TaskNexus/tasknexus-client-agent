# -*- mode: python ; coding: utf-8 -*-
"""
PyInstaller spec file for tasknexus-client-agent

Usage:
    pyinstaller tasknexus-agent.spec

This will create a single executable file in the dist/ directory.
"""

import sys
from pathlib import Path

# Get the base directory
base_dir = Path(SPECPATH)

# Analysis configuration
a = Analysis(
    [str(base_dir / 'agent' / 'main.py')],
    pathex=[str(base_dir)],
    binaries=[],
    datas=[
        # Include example config file
        (str(base_dir / 'config.example.yaml'), '.'),
    ],
    hiddenimports=[
        # Ensure all agent modules are included
        'agent',
        'agent.main',
        'agent.client',
        'agent.config',
        'agent.executor',
        # Dependencies
        'websockets',
        'websockets.client',
        'websockets.legacy',
        'websockets.legacy.client',
        'git',
        'click',
        'yaml',
        'aiofiles',
        # Standard library async support
        'asyncio',
        'asyncio.selector_events',
    ],
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=[
        # Exclude unnecessary modules to reduce size
        'tkinter',
        'matplotlib',
        'numpy',
        'pandas',
        'scipy',
        'PIL',
        'cv2',
    ],
    win_no_prefer_redirects=False,
    win_private_assemblies=False,
    cipher=None,
    noarchive=False,
)

# Create PYZ archive
pyz = PYZ(a.pure, a.zipped_data, cipher=None)

# Create the executable
exe = EXE(
    pyz,
    a.scripts,
    a.binaries,
    a.zipfiles,
    a.datas,
    [],
    name='tasknexus-agent',
    debug=False,
    bootloader_ignore_signals=False,
    strip=False,
    upx=True,
    upx_exclude=[],
    runtime_tmpdir=None,
    console=True,  # CLI application, needs console
    disable_windowed_traceback=False,
    argv_emulation=False,
    target_arch=None,
    codesign_identity=None,
    entitlements_file=None,
    # Icon (optional - add if you have one)
    # icon='icon.ico',
)
