"""
WebSocket 客户端模块

处理与 TaskNexus 服务器的 WebSocket 连接。
"""

import asyncio
import json
import logging
from typing import Callable, Optional, Any

import websockets
from websockets.exceptions import ConnectionClosed, WebSocketException

from .config import AgentConfig

logger = logging.getLogger("tasknexus.agent")


class AgentClient:
    """
    TaskNexus Agent WebSocket 客户端
    
    负责:
    - 建立和维护与服务器的 WebSocket 连接
    - 处理消息的发送和接收
    - 自动重连机制
    """
    
    def __init__(
        self,
        config: AgentConfig,
        on_task_dispatch: Optional[Callable[[dict], Any]] = None,
        on_connected: Optional[Callable[[], Any]] = None,
        on_disconnected: Optional[Callable[[], Any]] = None,
    ):
        self.config = config
        self.on_task_dispatch = on_task_dispatch
        self.on_connected = on_connected
        self.on_disconnected = on_disconnected
        
        self.websocket: Optional[websockets.WebSocketClientProtocol] = None
        self.connected = False
        self.running = False
        self._heartbeat_task: Optional[asyncio.Task] = None
        self._receive_task: Optional[asyncio.Task] = None
        self._reconnect_attempts = 0
    
    @property
    def ws_url(self) -> str:
        """构建带 token 的 WebSocket URL"""
        separator = "&" if "?" in self.config.server else "?"
        return f"{self.config.server}{separator}token={self.config.token}"
    
    async def connect(self) -> bool:
        """
        连接到服务器
        
        Returns:
            bool: 连接是否成功
        """
        try:
            logger.info(f"Connecting to {self.config.server}...")
            
            self.websocket = await websockets.connect(
                self.ws_url,
                ping_interval=20,
                ping_timeout=10,
                close_timeout=5,
            )
            
            self.connected = True
            self._reconnect_attempts = 0
            
            logger.info("Connected to server successfully")
            
            if self.on_connected:
                await self._safe_callback(self.on_connected)
            
            return True
            
        except Exception as e:
            logger.error(f"Failed to connect: {e}")
            self.connected = False
            return False
    
    async def disconnect(self):
        """断开连接"""
        self.running = False
        
        if self._heartbeat_task:
            self._heartbeat_task.cancel()
            try:
                await self._heartbeat_task
            except asyncio.CancelledError:
                pass
        
        if self._receive_task:
            self._receive_task.cancel()
            try:
                await self._receive_task
            except asyncio.CancelledError:
                pass
        
        if self.websocket:
            await self.websocket.close()
            self.websocket = None
        
        self.connected = False
        
        if self.on_disconnected:
            await self._safe_callback(self.on_disconnected)
        
        logger.info("Disconnected from server")
    
    async def run(self):
        """
        运行客户端主循环
        
        包含自动重连逻辑
        """
        self.running = True
        
        while self.running:
            # 尝试连接
            if not self.connected:
                success = await self.connect()
                
                if not success:
                    self._reconnect_attempts += 1
                    
                    if (self.config.max_reconnect_attempts > 0 and 
                        self._reconnect_attempts >= self.config.max_reconnect_attempts):
                        logger.error("Max reconnect attempts reached, giving up")
                        break
                    
                    wait_time = min(
                        self.config.reconnect_interval * (2 ** min(self._reconnect_attempts, 5)),
                        60  # 最大等待 60 秒
                    )
                    logger.info(f"Reconnecting in {wait_time} seconds... (attempt {self._reconnect_attempts})")
                    await asyncio.sleep(wait_time)
                    continue
            
            # 启动心跳和消息接收
            self._heartbeat_task = asyncio.create_task(self._heartbeat_loop())
            self._receive_task = asyncio.create_task(self._receive_loop())
            
            try:
                # 等待任意一个任务完成（通常是因为连接断开）
                done, pending = await asyncio.wait(
                    [self._heartbeat_task, self._receive_task],
                    return_when=asyncio.FIRST_COMPLETED,
                )
                
                # 取消其他任务
                for task in pending:
                    task.cancel()
                    try:
                        await task
                    except asyncio.CancelledError:
                        pass
                
            except asyncio.CancelledError:
                break
            
            self.connected = False
            
            if self.on_disconnected:
                await self._safe_callback(self.on_disconnected)
            
            if self.running:
                logger.warning("Connection lost, will reconnect...")
                await asyncio.sleep(self.config.reconnect_interval)
    
    async def send_message(self, message: dict):
        """发送消息到服务器"""
        if not self.websocket or not self.connected:
            logger.warning("Cannot send message: not connected")
            return
        
        try:
            await self.websocket.send(json.dumps(message))
        except Exception as e:
            logger.error(f"Failed to send message: {e}")
            self.connected = False
    
    async def send_heartbeat(self):
        """发送心跳消息"""
        message = {
            "type": "heartbeat",
            "system_info": self.config.get_system_info(),
        }
        await self.send_message(message)
    
    async def send_task_started(self, task_id: int):
        """发送任务开始通知"""
        await self.send_message({
            "type": "task_started",
            "task_id": task_id,
        })
    
    async def send_task_progress(self, task_id: int, output: str):
        """发送任务进度更新"""
        await self.send_message({
            "type": "task_progress",
            "task_id": task_id,
            "output": output,
        })
    
    async def send_task_completed(
        self, 
        task_id: int, 
        exit_code: int, 
        stdout: str, 
        stderr: str
    ):
        """发送任务完成通知"""
        await self.send_message({
            "type": "task_completed",
            "task_id": task_id,
            "exit_code": exit_code,
            "stdout": stdout,
            "stderr": stderr,
        })
    
    async def send_task_failed(self, task_id: int, error: str):
        """发送任务失败通知"""
        await self.send_message({
            "type": "task_failed",
            "task_id": task_id,
            "error": error,
        })
    
    async def _heartbeat_loop(self):
        """心跳循环"""
        while self.running and self.connected:
            try:
                await self.send_heartbeat()
                await asyncio.sleep(self.config.heartbeat_interval)
            except asyncio.CancelledError:
                break
            except Exception as e:
                logger.error(f"Heartbeat error: {e}")
                break
    
    async def _receive_loop(self):
        """消息接收循环"""
        try:
            async for message in self.websocket:
                try:
                    data = json.loads(message)
                    await self._handle_message(data)
                except json.JSONDecodeError:
                    logger.warning(f"Invalid JSON message: {message}")
                except Exception as e:
                    logger.error(f"Error handling message: {e}")
        except ConnectionClosed as e:
            logger.warning(f"Connection closed: {e}")
        except Exception as e:
            logger.error(f"Receive loop error: {e}")
    
    async def _handle_message(self, message: dict):
        """处理接收到的消息"""
        msg_type = message.get("type", "")
        
        if msg_type == "connected":
            logger.info(f"Server acknowledged connection: {message.get('message', '')}")
        
        elif msg_type == "heartbeat_ack":
            logger.debug(f"Heartbeat acknowledged at {message.get('server_time', '')}")
        
        elif msg_type == "task_dispatch":
            logger.info(f"Received task dispatch: {message.get('task_id')}")
            if self.on_task_dispatch:
                await self._safe_callback(self.on_task_dispatch, message)
        
        else:
            logger.debug(f"Unknown message type: {msg_type}")
    
    async def _safe_callback(self, callback: Callable, *args):
        """安全执行回调函数"""
        try:
            result = callback(*args)
            if asyncio.iscoroutine(result):
                await result
        except Exception as e:
            logger.error(f"Callback error: {e}")
