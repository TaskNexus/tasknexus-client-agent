import logging
from pipeline.component_framework.component import Component
from pipeline.core.flow.activity import Service

logger = logging.getLogger('django')


class AgentReleaseService(Service):
    """
    释放已锁定的 workspace。
    """
    
    def execute(self, data, parent_data):
        from client_agents.models import AgentWorkspace
        
        workspace_id = data.get_one_of_inputs('agent_workspace_id')
        
        if not workspace_id:
            data.set_outputs('success', False)
            data.set_outputs('message', 'No workspace ID provided')
            return False
        
        try:
            workspace = AgentWorkspace.objects.get(id=workspace_id)
            
            if workspace.status == 'RUNNING':
                workspace.status = 'IDLE'
                workspace.current_task = None
                workspace.save(update_fields=['status', 'current_task'])
                
                logger.info(f"[AgentRelease] Released workspace {workspace.name} (id={workspace.id})")
                data.set_outputs('success', True)
                data.set_outputs('message', f'Successfully released workspace {workspace.name}')
                return True
            else:
                logger.warning(f"[AgentRelease] Workspace {workspace.name} is not in RUNNING status (current: {workspace.status})")
                data.set_outputs('success', True)
                data.set_outputs('message', f'Workspace {workspace.name} was already in {workspace.status} status')
                return True
                
        except AgentWorkspace.DoesNotExist:
            logger.error(f"[AgentRelease] Workspace {workspace_id} not found")
            data.set_outputs('success', False)
            data.set_outputs('message', f'Workspace {workspace_id} not found')
            return False
        except Exception as e:
            logger.error(f"[AgentRelease] Failed to release workspace: {e}")
            data.set_outputs('success', False)
            data.set_outputs('message', str(e))
            return False
    
    def inputs_format(self):
        return [
            self.InputItem(name='Workspace ID', key='agent_workspace_id', type='int', required=True),
        ]
    
    def outputs_format(self):
        return [
            self.OutputItem(name='Success', key='success', type='bool'),
            self.OutputItem(name='Message', key='message', type='string'),
        ]


class AgentReleaseComponent(Component):
    name = 'Agent Release'
    code = 'agent_release'
    bound_service = AgentReleaseService
    version = '1.0'
    category = 'ClientAgent'
    description = '释放已锁定的工作空间'
