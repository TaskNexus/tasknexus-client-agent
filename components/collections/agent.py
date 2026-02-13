import logging
from channels.layers import get_channel_layer
from asgiref.sync import async_to_sync
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
        
        workspace_id = data.get_one_of_inputs('workspace_id')
        command = data.get_one_of_inputs('command', '')
        timeout = data.get_one_of_inputs('timeout', 3600)
        client_repo_url = data.get_one_of_inputs('client_repo_url', '')
        client_repo_ref = data.get_one_of_inputs('client_repo_ref', 'main')
        pipeline_id = parent_data.get_one_of_inputs('pipeline_id', '')
        
        if not command:
            data.outputs.ex_data = 'No command provided'
            return False
        
        data.set_outputs('_command', command)
        data.set_outputs('_timeout', int(timeout) if timeout else 3600)
        data.set_outputs('_client_repo_url', client_repo_url)
        data.set_outputs('_client_repo_ref', client_repo_ref)
        data.set_outputs('_pipeline_id', pipeline_id)
        data.set_outputs('_wait_start_time', timezone.now().isoformat())
        
        try:
            workspace = AgentWorkspace.objects.get(id=workspace_id)
        except AgentWorkspace.DoesNotExist:
            data.outputs.ex_data = f'Workspace {workspace_id} not found'
            return False
            
        success = self._dispatch_task(data, workspace)
        return success if success else False
    
    def _dispatch_task(self, data, workspace):
        from client_agents.models import ClientAgent, AgentTask, AgentWorkspace
        
        agent = workspace.agent
        command = data.get_one_of_outputs('_command')
        timeout = data.get_one_of_outputs('_timeout', 3600)
        client_repo_url = data.get_one_of_outputs('_client_repo_url', '')
        client_repo_ref = data.get_one_of_outputs('_client_repo_ref', 'main')
        pipeline_id = data.get_one_of_outputs('_pipeline_id', '')
        
        try:
            agent_task = AgentTask.objects.create(
                agent=agent,
                workspace=workspace,
                pipeline_id=pipeline_id,
                client_repo_url=client_repo_url,
                client_repo_ref=client_repo_ref,
                command=command,
                timeout=timeout,
                status='DISPATCHED',
                dispatched_at=timezone.now(),
            )
            task_id = agent_task.id
            
            data.set_outputs('task_id', task_id)
            data.set_outputs('_dispatch_time', timezone.now().isoformat())
        except Exception as e:
            data.outputs.ex_data = str(e)
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
            return True
            
        except Exception as e:
            AgentTask.objects.filter(id=task_id).update(
                status='FAILED',
                error_message=str(e),
                finished_at=timezone.now()
            )
            data.outputs.ex_data = str(e)
            return False

    def schedule(self, data, parent_data, callback_data=None):
        from client_agents.models import AgentTask, AgentWorkspace
        from datetime import datetime
        
        task_id = data.get_one_of_outputs('task_id')
        if not task_id:
            data.outputs.ex_data = 'No task ID found'
            self.finish_schedule()
            return False
        
        try:
            task = AgentTask.objects.get(id=task_id)
        except AgentTask.DoesNotExist:
            data.outputs.ex_data = 'Task not found in database'
            self.finish_schedule()
            return False
        
        status = task.status
        
        if status in ['COMPLETED', 'FAILED', 'TIMEOUT']:
            data.set_outputs('exit_code', task.exit_code if task.exit_code is not None else -1)
            data.set_outputs('stdout', task.stdout)
            data.set_outputs('stderr', task.stderr)
            data.outputs.ex_data = task.error_message
            self.finish_schedule()
            return status == 'COMPLETED'
        
        if status in ['DISPATCHED', 'RUNNING']:
            # Check heartbeat timeout for running tasks
            if status == 'RUNNING' and task.last_heartbeat:
                heartbeat_elapsed = (timezone.now() - task.last_heartbeat).total_seconds()
                if heartbeat_elapsed > 60:  # 60 seconds heartbeat timeout
                    AgentTask.objects.filter(id=task_id).update(
                        status='FAILED',
                        error_message='Task heartbeat timeout',
                        finished_at=timezone.now()
                    )
                    data.outputs.ex_data = 'Task heartbeat timeout'
                    self.finish_schedule()
                    return False
            
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
                        
                        data.outputs.ex_data = f'Task timed out after {timeout} seconds'
                        self.finish_schedule()
                        return False
                except (ValueError, TypeError):
                    pass
        
        return True
    
    def inputs_format(self):
        return [
            self.InputItem(name='Workspace ID', key='workspace_id', type='int', required=True),
            self.InputItem(name='Command', key='command', type='string', required=True),
            self.InputItem(name='Timeout (s)', key='timeout', type='int', required=False),
            self.InputItem(name='Client Repo URL', key='client_repo_url', type='string', required=False),
            self.InputItem(name='Client Repo Ref', key='client_repo_ref', type='string', required=False),
        ]

    def outputs_format(self):
        return [
            self.OutputItem(name='Task ID', key='task_id', type='string'),
            self.OutputItem(name='Exit Code', key='exit_code', type='int'),
            self.OutputItem(name='Standard Output', key='stdout', type='string'),
            self.OutputItem(name='Standard Error', key='stderr', type='string'),
        ]

class ClientAgentComponent(Component):
    name = 'Client Agent'
    code = 'client_agent'
    bound_service = ClientAgentService
    version = '1.2'
    category = 'ClientAgent'
    description = '将命令分发给客户端代理执行'
