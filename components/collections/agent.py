import logging
from channels.layers import get_channel_layer
from asgiref.sync import async_to_sync
from django.db import transaction
from django.utils import timezone
from pipeline.component_framework.component import Component
from pipeline.core.flow.activity import Service, StaticIntervalGenerator

logger = logging.getLogger('django')

MAX_WAIT_FOR_AGENT = 600  # 10 minutes

class ClientAgentService(Service):
    __need_schedule__ = True
    interval = StaticIntervalGenerator(2)
    
    def execute(self, data, parent_data):
        from client_agents.models import ClientAgent, AgentWorkspace
        
        # 新增：支持复用已锁定的 workspace
        agent_workspace_id = data.get_one_of_inputs('agent_workspace_id')
        
        workspace_label = data.get_one_of_inputs('workspace_label', '')
        command = data.get_one_of_inputs('command', '')
        timeout = data.get_one_of_inputs('timeout', 3600)
        client_repo_url = data.get_one_of_inputs('client_repo_url', '')
        client_repo_ref = data.get_one_of_inputs('client_repo_ref', 'main')
        
        if not command:
            data.set_outputs('error_message', 'No command provided')
            data.set_outputs('success', False)
            return False
        
        data.set_outputs('_workspace_label', workspace_label)
        data.set_outputs('_command', command)
        data.set_outputs('_timeout', int(timeout) if timeout else 3600)
        data.set_outputs('_client_repo_url', client_repo_url)
        data.set_outputs('_client_repo_ref', client_repo_ref)
        data.set_outputs('_wait_start_time', timezone.now().isoformat())
        
        # 判断是否复用已锁定的 workspace
        if agent_workspace_id:
            data.set_outputs('_use_blocked_workspace', True)
            data.set_outputs('_blocked_workspace_id', agent_workspace_id)
            
            # 尝试获取锁并分发任务
            success = self._try_dispatch_with_lock(data, agent_workspace_id)
            if success is None:
                # 需要等待，在 schedule 中继续
                logger.info(f"Workspace {agent_workspace_id} is busy, will wait...")
                return True
            return success
        else:
            data.set_outputs('_use_blocked_workspace', False)
            
            workspace = self._try_acquire_workspace(workspace_label)
            
            if workspace:
                success = self._dispatch_task(data, workspace)
                if not success:
                    return False
                return True
            else:
                logger.info("No available workspace, will wait in schedule method")
                return True
    
    def _try_dispatch_with_lock(self, data, workspace_id):
        """
        尝试获取 workspace 的任务锁并分发任务。
        返回 True/False 表示成功/失败，返回 None 表示需要等待。
        """
        from client_agents.models import AgentWorkspace
        
        try:
            with transaction.atomic():
                ws = AgentWorkspace.objects.select_for_update(nowait=True).get(id=workspace_id)
                
                if ws.current_task is not None:
                    # workspace 正在被其他任务使用
                    return None
                
                # 获取锁成功，分发任务
                return self._dispatch_task_locked(data, ws)
        except AgentWorkspace.DoesNotExist:
            data.set_outputs('error_message', f'Workspace {workspace_id} not found')
            data.set_outputs('success', False)
            return False
        except Exception as e:
            # 锁被占用或其他错误，继续等待
            logger.debug(f"Could not acquire lock for workspace {workspace_id}: {e}")
            return None
    
    def _dispatch_task_locked(self, data, workspace):
        """在已获取事务锁的情况下分发任务"""
        from client_agents.models import AgentTask
        
        agent = workspace.agent
        
        command = data.get_one_of_outputs('_command')
        timeout = data.get_one_of_outputs('_timeout', 3600)
        client_repo_url = data.get_one_of_outputs('_client_repo_url', '')
        client_repo_ref = data.get_one_of_outputs('_client_repo_ref', 'main')
        
        try:
            agent_task = AgentTask.objects.create(
                agent=agent,
                workspace=workspace,
                client_repo_url=client_repo_url,
                client_repo_ref=client_repo_ref,
                command=command,
                timeout=timeout,
                status='DISPATCHED',
                dispatched_at=timezone.now(),
            )
            task_id = agent_task.id
            
            # 设置 current_task 作为锁
            workspace.current_task = agent_task
            workspace.save(update_fields=['current_task'])
            
            data.set_outputs('task_id', task_id)
            data.set_outputs('agent_name', agent.name)
            data.set_outputs('workspace_id', workspace.id)
            data.set_outputs('workspace_name', workspace.name)
            data.set_outputs('_dispatch_time', timezone.now().isoformat())
            
            logger.info(f"Created AgentTask {task_id} for agent {agent.name}, workspace {workspace.name}")
            
        except Exception as e:
            logger.error(f"Failed to create AgentTask: {e}")
            data.set_outputs('error_message', str(e))
            data.set_outputs('success', False)
            return False
        
        # 发送到 agent
        try:
            channel_layer = get_channel_layer()
            async_to_sync(channel_layer.group_send)(
                f"agent_{agent.id}",
                {
                    "type": "task_dispatch",
                    "task_id": task_id,
                    "workspace_name": workspace.name,
                    "client_repo_url": client_repo_url,
                    "client_repo_ref": client_repo_ref,
                    "command": command,
                    "timeout": timeout,
                    "environment": agent.environment,
                }
            )
            logger.info(f"Dispatched task {task_id} to agent {agent.name} (workspace={workspace.name})")
            return True
            
        except Exception as e:
            logger.error(f"Failed to dispatch task to agent: {e}")
            AgentTask.objects.filter(id=task_id).update(
                status='FAILED',
                error_message=str(e),
                finished_at=timezone.now()
            )
            workspace.current_task = None
            workspace.save(update_fields=['current_task'])
            
            data.set_outputs('error_message', str(e))
            data.set_outputs('success', False)
            return False
    
    def _try_acquire_workspace(self, workspace_label):
        from client_agents.models import ClientAgent, AgentWorkspace
        
        base_qs = AgentWorkspace.objects.filter(
            status='IDLE',
            agent__status='ONLINE'
        )
        
        if workspace_label:
            workspace = base_qs.filter(
                labels__contains=[workspace_label]
            ).order_by('?').first()
            
            if workspace:
                return workspace
            
            running_exists = AgentWorkspace.objects.filter(
                status='RUNNING',
                agent__status='ONLINE',
                labels__contains=[workspace_label]
            ).exists()
            
            if running_exists:
                logger.info(f"Workspaces with label '{workspace_label}' are RUNNING, will wait...")
                return None
            
            logger.warning(f"No workspace found with label '{workspace_label}'")
            return None
        else:
            return base_qs.order_by('?').first()
    
    def _dispatch_task(self, data, workspace):
        from client_agents.models import ClientAgent, AgentTask, AgentWorkspace
        
        agent = workspace.agent
        
        workspace.status = 'RUNNING'
        workspace.save(update_fields=['status'])
        
        command = data.get_one_of_outputs('_command')
        timeout = data.get_one_of_outputs('_timeout', 3600)
        client_repo_url = data.get_one_of_outputs('_client_repo_url', '')
        client_repo_ref = data.get_one_of_outputs('_client_repo_ref', 'main')
        
        try:
            agent_task = AgentTask.objects.create(
                agent=agent,
                workspace=workspace,
                client_repo_url=client_repo_url,
                client_repo_ref=client_repo_ref,
                command=command,
                timeout=timeout,
                status='DISPATCHED',
                dispatched_at=timezone.now(),
            )
            task_id = agent_task.id
            
            data.set_outputs('task_id', task_id)
            data.set_outputs('agent_name', agent.name)
            data.set_outputs('workspace_id', workspace.id)
            data.set_outputs('workspace_name', workspace.name)
            data.set_outputs('_dispatch_time', timezone.now().isoformat())
            
            logger.info(f"Created AgentTask {task_id} for agent {agent.name}, workspace {workspace.name}")
            
        except Exception as e:
            logger.error(f"Failed to create AgentTask: {e}")
            workspace.status = 'IDLE'
            workspace.save(update_fields=['status'])
            data.set_outputs('error_message', str(e))
            data.set_outputs('success', False)
            return False
        
        try:
            channel_layer = get_channel_layer()
            async_to_sync(channel_layer.group_send)(
                f"agent_{agent.id}",
                {
                    "type": "task_dispatch",
                    "task_id": task_id,
                    "workspace_name": workspace.name,
                    "client_repo_url": client_repo_url,
                    "client_repo_ref": client_repo_ref,
                    "command": command,
                    "timeout": timeout,
                    "environment": agent.environment,
                }
            )
            logger.info(f"Dispatched task {task_id} to agent {agent.name} (workspace={workspace.name})")
            return True
            
        except Exception as e:
            logger.error(f"Failed to dispatch task to agent: {e}")
            AgentTask.objects.filter(id=task_id).update(
                status='FAILED',
                error_message=str(e),
                finished_at=timezone.now()
            )
            workspace.status = 'IDLE'
            workspace.save(update_fields=['status'])
            
            data.set_outputs('error_message', str(e))
            data.set_outputs('success', False)
            return False
    
    def schedule(self, data, parent_data, callback_data=None):
        from client_agents.models import ClientAgent, AgentWorkspace
        
        task_id = data.get_one_of_outputs('task_id')
        
        if not task_id:
            use_blocked = data.get_one_of_outputs('_use_blocked_workspace', False)
            
            if use_blocked:
                return self._wait_for_blocked_workspace(data)
            else:
                return self._wait_for_workspace(data)
        
        return self._poll_task_result(data)
    
    def _wait_for_blocked_workspace(self, data):
        """等待已锁定的 workspace 空闲"""
        from datetime import datetime
        
        workspace_id = data.get_one_of_outputs('_blocked_workspace_id')
        wait_start_str = data.get_one_of_outputs('_wait_start_time')
        
        # 检查超时
        if wait_start_str:
            try:
                wait_start = datetime.fromisoformat(wait_start_str)
                elapsed = (timezone.now() - wait_start).total_seconds()
                
                if elapsed > MAX_WAIT_FOR_AGENT:
                    data.set_outputs('error_message', f'Timed out waiting for workspace lock after {MAX_WAIT_FOR_AGENT} seconds')
                    data.set_outputs('success', False)
                    self.finish_schedule()
                    return False
            except (ValueError, TypeError):
                pass
        
        # 尝试获取锁并分发
        success = self._try_dispatch_with_lock(data, workspace_id)
        
        if success is None:
            # 继续等待
            return True
        elif success:
            return True
        else:
            self.finish_schedule()
            return False
    
    def _wait_for_workspace(self, data):
        from client_agents.models import AgentWorkspace
        from datetime import datetime
        
        workspace_label = data.get_one_of_outputs('_workspace_label', '')
        wait_start_str = data.get_one_of_outputs('_wait_start_time')
        
        if wait_start_str:
            try:
                wait_start = datetime.fromisoformat(wait_start_str)
                elapsed = (timezone.now() - wait_start).total_seconds()
                
                if elapsed > MAX_WAIT_FOR_AGENT:
                    data.set_outputs('error_message', f'Timed out waiting for available workspace after {MAX_WAIT_FOR_AGENT} seconds')
                    data.set_outputs('success', False)
                    self.finish_schedule()
                    return False
            except (ValueError, TypeError):
                pass
        
        workspace = self._try_acquire_workspace(workspace_label)
        
        if workspace:
            success = self._dispatch_task(data, workspace)
            if not success:
                self.finish_schedule()
                return False
            return True
        else:
            return True
    
    def _poll_task_result(self, data):
        from client_agents.models import AgentTask, AgentWorkspace
        from datetime import datetime
        
        task_id = data.get_one_of_outputs('task_id')
        if not task_id:
            data.set_outputs('error_message', 'No task ID found')
            data.set_outputs('success', False)
            self.finish_schedule()
            return False
        
        try:
            task = AgentTask.objects.get(id=task_id)
        except AgentTask.DoesNotExist:
            self._release_workspace(data)
            data.set_outputs('error_message', 'Task not found in database')
            data.set_outputs('success', False)
            self.finish_schedule()
            return False
        
        status = task.status
        
        if status in ['COMPLETED', 'FAILED', 'TIMEOUT']:
            data.set_outputs('exit_code', task.exit_code if task.exit_code is not None else -1)
            data.set_outputs('stdout', task.stdout)
            data.set_outputs('stderr', task.stderr)
            data.set_outputs('result', task.result)
            data.set_outputs('error_message', task.error_message)
            data.set_outputs('success', status == 'COMPLETED')
            
            self._release_workspace(data)
            
            self.finish_schedule()
            return status == 'COMPLETED'
        
        if status in ['DISPATCHED', 'RUNNING']:
            dispatch_time_str = data.get_one_of_outputs('_dispatch_time')
            timeout = data.get_one_of_outputs('_timeout', 3600)
            
            if dispatch_time_str:
                try:
                    dispatch_time = datetime.fromisoformat(dispatch_time_str)
                    elapsed = (timezone.now() - dispatch_time).total_seconds()
                    
                    if elapsed > timeout:
                        AgentTask.objects.filter(id=task_id).update(
                            status='TIMEOUT',
                            error_message=f'Task timed out after {timeout} seconds',
                            finished_at=timezone.now()
                        )
                        
                        data.set_outputs('error_message', f'Task timed out after {timeout} seconds')
                        data.set_outputs('success', False)
                        
                        self._release_workspace(data)
                        
                        self.finish_schedule()
                        return False
                except (ValueError, TypeError):
                    pass
        
        return True
    
    def _release_workspace(self, data):
        """
        释放 workspace。
        如果是复用已锁定的 workspace，只清除 current_task，不改变 status。
        """
        from client_agents.models import AgentWorkspace
        
        use_blocked = data.get_one_of_outputs('_use_blocked_workspace', False)
        workspace_id = data.get_one_of_outputs('workspace_id')
        
        if workspace_id:
            try:
                workspace = AgentWorkspace.objects.get(id=workspace_id)
                
                if use_blocked:
                    # 只清除 current_task，保持 status=RUNNING
                    workspace.current_task = None
                    workspace.save(update_fields=['current_task'])
                    logger.info(f"Cleared current_task for workspace {workspace.name} (blocked mode)")
                else:
                    # 常规模式，释放 workspace
                    if workspace.status == 'RUNNING':
                        workspace.status = 'IDLE'
                        workspace.current_task = None
                        workspace.save(update_fields=['status', 'current_task'])
                        logger.info(f"Released workspace lock for {workspace.name}")
            except AgentWorkspace.DoesNotExist:
                pass

    def inputs_format(self):
        return [
            self.InputItem(name='Workspace ID', key='agent_workspace_id', type='int', required=False),
            self.InputItem(name='Workspace Label', key='workspace_label', type='string', required=False),
            self.InputItem(name='Command', key='command', type='string', required=True),
            self.InputItem(name='Timeout (s)', key='timeout', type='int', required=False),
            self.InputItem(name='Client Repo URL', key='client_repo_url', type='string', required=False),
            self.InputItem(name='Client Repo Ref', key='client_repo_ref', type='string', required=False),
        ]

    def outputs_format(self):
        return [
            self.OutputItem(name='Exit Code', key='exit_code', type='int'),
            self.OutputItem(name='Standard Output', key='stdout', type='string'),
            self.OutputItem(name='Standard Error', key='stderr', type='string'),
            self.OutputItem(name='Result', key='result', type='object'),
            self.OutputItem(name='Error Message', key='error_message', type='string'),
            self.OutputItem(name='Success', key='success', type='bool'),
            self.OutputItem(name='Agent Name', key='agent_name', type='string'),
            self.OutputItem(name='Workspace Name', key='workspace_name', type='string'),
            self.OutputItem(name='Task ID', key='task_id', type='string'),
        ]


class ClientAgentComponent(Component):
    name = 'Client Agent'
    code = 'client_agent'
    bound_service = ClientAgentService
    version = '1.2'
    category = 'ClientAgent'
    description = '将命令分发给客户端代理执行，支持复用已锁定的工作空间'
