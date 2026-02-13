import logging
from pipeline.component_framework.component import Component
from pipeline.core.flow.activity import Service

logger = logging.getLogger('django')


class AgentReleaseService(Service):
    def execute(self, data, parent_data):
        from client_agents.models import AgentWorkspace
        workspace_id = data.get_one_of_inputs('workspace_id')
        if not workspace_id:
            data.outputs.ex_data = 'No workspace ID provided'
            return False
        try:
            workspace = AgentWorkspace.objects.get(id=workspace_id)
            if workspace.status == 'RUNNING':
                workspace.status = 'IDLE'
                workspace.current_task = None
                workspace.save(update_fields=['status', 'current_task'])
                return True
            else:
                return True
        except AgentWorkspace.DoesNotExist:
            return False
        except Exception as e:
            data.outputs.ex_data = str(e)
            return False
    
    def inputs_format(self):
        return [
            self.InputItem(name='Workspace ID', key='workspace_id', type='int', required=True),
        ]
    
    def outputs_format(self):
        return []


class AgentReleaseComponent(Component):
    name = 'Agent Release'
    code = 'agent_release'
    bound_service = AgentReleaseService
    version = '1.0'
    category = 'ClientAgent'
    description = '释放工作空间'
