import logging
from channels.layers import get_channel_layer
from asgiref.sync import async_to_sync
from django.db import transaction
from django.utils import timezone
from pipeline.component_framework.component import Component
from pipeline.core.flow.activity import Service, StaticIntervalGenerator

logger = logging.getLogger('django')

MAX_WAIT_FOR_WORKSPACE = 600  # 10 minutes default


class WorkspaceAcquireService(Service):
    __need_schedule__ = True
    interval = StaticIntervalGenerator(2)
    
    def execute(self, data, parent_data):
        from projects.models import Project

        workspace_label = data.get_one_of_inputs('workspace_label', '')
        timeout = data.get_one_of_inputs('timeout', MAX_WAIT_FOR_WORKSPACE)
        client_repo_url = data.get_one_of_inputs('client_repo_url', '')
        client_repo_ref = data.get_one_of_inputs('client_repo_ref', '')
        client_repo_token = ''
        pipeline_id = parent_data.get_one_of_inputs('pipeline_id', '')
        project_id = parent_data.get_one_of_inputs('project_id', '')

        # Read agent repo config from project extra_config as defaults
        if project_id:
            try:
                project = Project.objects.get(id=project_id)
                extra_config = project.extra_config or {}
                if not client_repo_url:
                    client_repo_url = extra_config.get('agent_repo_url', '')
                if not client_repo_ref:
                    client_repo_ref = extra_config.get('agent_repo_ref', 'main')
                client_repo_token = extra_config.get('agent_repo_token', '')
            except Project.DoesNotExist:
                logger.warning(f'Project {project_id} not found, skipping extra_config')

        if not client_repo_ref:
            client_repo_ref = 'main'

        data.set_outputs('_workspace_label', workspace_label)
        data.set_outputs('_timeout', int(timeout) if timeout else MAX_WAIT_FOR_WORKSPACE)
        data.set_outputs('_wait_start_time', timezone.now())
        data.set_outputs('_pipeline_id', pipeline_id)
        data.set_outputs('_client_repo_url', client_repo_url)
        data.set_outputs('_client_repo_ref', client_repo_ref)
        data.set_outputs('_client_repo_token', client_repo_token)
        # Phase: 'acquire' -> 'clone' -> 'done'
        data.set_outputs('_phase', 'acquire')
        
        # 尝试立即获取 workspace
        workspace = self._try_acquire_workspace(workspace_label, pipeline_id)
        
        if workspace:
            self._set_success_outputs(data, workspace)
            # If repo URL is configured, dispatch clone task
            if client_repo_url:
                data.set_outputs('_phase', 'clone')
                return self._dispatch_clone_task(data, workspace)
            else:
                data.set_outputs('_phase', 'done')
                self.finish_schedule()
                return True
        else:
            return True
    
    def _try_acquire_workspace(self, workspace_label, pipeline_id=''):
        from client_agents.models import AgentWorkspace
        
        base_qs = AgentWorkspace.objects.filter(
            status='IDLE',
            agent__status='ONLINE'
        )
        
        if workspace_label:
            workspace = base_qs.filter(
                labels__contains=[workspace_label]
            ).order_by('?').first()
        else:
            workspace = base_qs.order_by('?').first()
        
        if workspace:
            with transaction.atomic():
                ws = AgentWorkspace.objects.select_for_update(nowait=True).filter(
                    id=workspace.id,
                    status='IDLE'
                ).first()
                
                if ws:
                    ws.status = 'RUNNING'
                    ws.pipeline_id = pipeline_id
                    ws.save(update_fields=['status', 'pipeline_id'])
                    return ws
        
        return None
    
    def _set_success_outputs(self, data, workspace):
        data.set_outputs('workspace_id', workspace.id)
        data.set_outputs('workspace_name', workspace.name)
        data.set_outputs('agent_name', workspace.agent.name)

    def _dispatch_clone_task(self, data, workspace):
        """Dispatch a repo clone/update task to the agent."""
        from client_agents.models import AgentTask

        agent = workspace.agent
        client_repo_url = data.get_one_of_outputs('_client_repo_url', '')
        client_repo_ref = data.get_one_of_outputs('_client_repo_ref', 'main')
        client_repo_token = data.get_one_of_outputs('_client_repo_token', '')
        pipeline_id = data.get_one_of_outputs('_pipeline_id', '')

        # Use a no-op command; the Rust agent will clone/update the repo
        # before running the command, and 'echo' will succeed immediately.
        clone_command = 'echo "Repo setup complete"'

        try:
            agent_task = AgentTask.objects.create(
                agent=agent,
                workspace=workspace,
                pipeline_id=pipeline_id,
                client_repo_url=client_repo_url,
                client_repo_ref=client_repo_ref,
                command=clone_command,
                timeout=300,  # 5 minutes for clone
                status='DISPATCHED',
                dispatched_at=timezone.now(),
            )
            data.set_outputs('_clone_task_id', agent_task.id)
        except Exception as e:
            data.outputs.ex_data = f'Failed to create clone task: {e}'
            return False

        try:
            channel_layer = get_channel_layer()
            async_to_sync(channel_layer.group_send)(
                f"agent_{agent.id}",
                {
                    "type": "task_dispatch",
                    "task_id": agent_task.id,
                    "workspace_name": workspace.name,
                    "client_repo_url": client_repo_url,
                    "client_repo_ref": client_repo_ref,
                    "client_repo_token": client_repo_token,
                    "command": clone_command,
                    "timeout": 300,
                    "environment": agent.environment,
                }
            )
            return True
        except Exception as e:
            AgentTask.objects.filter(id=agent_task.id).update(
                status='FAILED',
                error_message=str(e),
                finished_at=timezone.now()
            )
            data.outputs.ex_data = f'Failed to dispatch clone task: {e}'
            return False

    def schedule(self, data, parent_data, callback_data=None):
        from client_agents.models import AgentTask

        phase = data.get_one_of_outputs('_phase', 'acquire')

        # Phase 1: waiting to acquire a workspace
        if phase == 'acquire':
            workspace_id = data.get_one_of_outputs('workspace_id')
            if workspace_id:
                # Already acquired (shouldn't happen, but handle it)
                data.set_outputs('_phase', 'done')
                self.finish_schedule()
                return True
            
            wait_start = data.get_one_of_outputs('_wait_start_time')
            timeout = data.get_one_of_outputs('_timeout', MAX_WAIT_FOR_WORKSPACE)
            
            if wait_start:
                elapsed = (timezone.now() - wait_start).total_seconds()
                if elapsed > timeout:
                    data.outputs.ex_data = f'Timed out waiting for workspace after {timeout} seconds'
                    self.finish_schedule()
                    return False

            workspace_label = data.get_one_of_outputs('_workspace_label', '')
            pipeline_id = data.get_one_of_outputs('_pipeline_id', '')
            workspace = self._try_acquire_workspace(workspace_label, pipeline_id)
            
            if workspace:
                self._set_success_outputs(data, workspace)
                client_repo_url = data.get_one_of_outputs('_client_repo_url', '')
                if client_repo_url:
                    data.set_outputs('_phase', 'clone')
                    return self._dispatch_clone_task(data, workspace)
                else:
                    data.set_outputs('_phase', 'done')
                    self.finish_schedule()
                    return True
            
            return True

        # Phase 2: waiting for clone/update task to complete
        if phase == 'clone':
            clone_task_id = data.get_one_of_outputs('_clone_task_id')
            if not clone_task_id:
                data.outputs.ex_data = 'Clone task ID not found'
                self.finish_schedule()
                return False

            try:
                task = AgentTask.objects.get(id=clone_task_id)
            except AgentTask.DoesNotExist:
                data.outputs.ex_data = 'Clone task not found in database'
                self.finish_schedule()
                return False

            if task.status == 'COMPLETED':
                data.set_outputs('_phase', 'done')
                self.finish_schedule()
                return True
            elif task.status in ['FAILED', 'TIMEOUT']:
                data.outputs.ex_data = f'Repo clone/update failed: {task.error_message or task.stderr}'
                self.finish_schedule()
                return False
            
            # Still running, keep waiting
            return True

        # Phase done (shouldn't reach here)
        self.finish_schedule()
        return True
    
    def inputs_format(self):
        return [
            self.InputItem(name='Workspace Label', key='workspace_label', type='string', required=False),
            self.InputItem(name='Timeout (s)', key='timeout', type='int', required=False),
            self.InputItem(name='Client Repo URL', key='client_repo_url', type='string', required=False),
            self.InputItem(name='Client Repo Ref', key='client_repo_ref', type='string', required=False),
        ]
    
    def outputs_format(self):
        return [
            self.OutputItem(name='Workspace ID', key='workspace_id', type='int'),
            self.OutputItem(name='Workspace Name', key='workspace_name', type='string'),
            self.OutputItem(name='Agent Name', key='agent_name', type='string'),
        ]


class WorkspaceAcquireComponent(Component):
    name = 'Workspace Acquire'
    code = 'workspace_acquire'
    bound_service = WorkspaceAcquireService
    version = '1.0'
    category = 'ClientAgent'
    description = '获取并锁定工作空间，并克隆/更新代码仓库'
