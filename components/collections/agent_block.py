import logging
from django.db import transaction
from django.utils import timezone
from pipeline.component_framework.component import Component
from pipeline.core.flow.activity import Service, StaticIntervalGenerator

logger = logging.getLogger('django')

MAX_WAIT_FOR_WORKSPACE = 600  # 10 minutes default


class AgentBlockService(Service):
    """
    获取并锁定一个指定 label 的 workspace。
    后续 ClientAgent 节点可以使用该 workspace。
    """
    __need_schedule__ = True
    interval = StaticIntervalGenerator(2)
    
    def execute(self, data, parent_data):
        workspace_label = data.get_one_of_inputs('workspace_label', '')
        timeout = data.get_one_of_inputs('timeout', MAX_WAIT_FOR_WORKSPACE)
        
        data.set_outputs('_workspace_label', workspace_label)
        data.set_outputs('_timeout', int(timeout) if timeout else MAX_WAIT_FOR_WORKSPACE)
        data.set_outputs('_wait_start_time', timezone.now().isoformat())
        
        # 尝试立即获取 workspace
        workspace = self._try_acquire_workspace(workspace_label)
        
        if workspace:
            self._set_success_outputs(data, workspace)
            self.finish_schedule()
            return True
        else:
            logger.info(f"[AgentBlock] No available workspace with label '{workspace_label}', will wait...")
            return True
    
    def _try_acquire_workspace(self, workspace_label):
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
            # 使用事务锁定 workspace
            with transaction.atomic():
                ws = AgentWorkspace.objects.select_for_update(nowait=True).filter(
                    id=workspace.id,
                    status='IDLE'
                ).first()
                
                if ws:
                    ws.status = 'RUNNING'
                    ws.save(update_fields=['status'])
                    logger.info(f"[AgentBlock] Locked workspace {ws.name} (id={ws.id})")
                    return ws
        
        return None
    
    def _set_success_outputs(self, data, workspace):
        data.set_outputs('agent_id', workspace.agent.id)
        data.set_outputs('agent_workspace_id', workspace.id)
        data.set_outputs('agent_workspace_name', workspace.name)
        data.set_outputs('agent_name', workspace.agent.name)
        data.set_outputs('success', True)
    
    def schedule(self, data, parent_data, callback_data=None):
        from datetime import datetime
        
        # 检查是否已成功获取
        if data.get_one_of_outputs('success'):
            self.finish_schedule()
            return True
        
        # 检查超时
        wait_start_str = data.get_one_of_outputs('_wait_start_time')
        timeout = data.get_one_of_outputs('_timeout', MAX_WAIT_FOR_WORKSPACE)
        
        if wait_start_str:
            try:
                wait_start = datetime.fromisoformat(wait_start_str)
                elapsed = (timezone.now() - wait_start).total_seconds()
                
                if elapsed > timeout:
                    data.set_outputs('error_message', f'Timed out waiting for workspace after {timeout} seconds')
                    data.set_outputs('success', False)
                    self.finish_schedule()
                    return False
            except (ValueError, TypeError):
                pass
        
        # 重试获取 workspace
        workspace_label = data.get_one_of_outputs('_workspace_label', '')
        workspace = self._try_acquire_workspace(workspace_label)
        
        if workspace:
            self._set_success_outputs(data, workspace)
            self.finish_schedule()
            return True
        
        # 继续等待
        return True
    
    def inputs_format(self):
        return [
            self.InputItem(name='Workspace Label', key='workspace_label', type='string', required=False),
            self.InputItem(name='Timeout (s)', key='timeout', type='int', required=False),
        ]
    
    def outputs_format(self):
        return [
            self.OutputItem(name='Agent ID', key='agent_id', type='int'),
            self.OutputItem(name='Workspace ID', key='agent_workspace_id', type='int'),
            self.OutputItem(name='Workspace Name', key='agent_workspace_name', type='string'),
            self.OutputItem(name='Agent Name', key='agent_name', type='string'),
            self.OutputItem(name='Success', key='success', type='bool'),
            self.OutputItem(name='Error Message', key='error_message', type='string'),
        ]


class AgentBlockComponent(Component):
    name = 'Agent Block'
    code = 'agent_block'
    bound_service = AgentBlockService
    version = '1.0'
    category = 'ClientAgent'
    description = '获取并锁定一个工作空间，供后续 ClientAgent 节点使用'
