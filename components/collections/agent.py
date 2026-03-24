import logging
import json
from django.utils import timezone
from pipeline.component_framework.component import Component
from pipeline.core.flow.activity import Service, StaticIntervalGenerator
from pipeline.core.flow.io import StringItemSchema, ObjectItemSchema
from components.schemas import ExtendedArraySchema, ExtendedStringSchema, ExtendedObjectSchema
from client_agents.dispatch_stream import publish_dispatch_event
from workflows.workspace import (
    WORKSPACE_MODE_NONE,
    WORKSPACE_MODE_SANDBOX,
    WORKSPACE_MODE_WORKSPACE,
    SYSTEM_WORKSPACE_INPUT_KEYS,
)

logger = logging.getLogger('django')

MAX_WAIT_FOR_AGENT = 600  # 10 minutes
HEARTBEAT_TIMEOUT_SECONDS = 120
HEARTBEAT_TIMEOUT_RETRY = 3
EXECUTION_MODE_COMMAND = 'command'
EXECUTION_MODE_CODE = 'code'
ALLOWED_CODE_LANGUAGES = {'shell', 'python'}
ROOT_SYSTEM_INPUT_KEYS = {
    'project_id',
    'pipeline_id',
    'task_created_by',
    'task_create_username',
    'task_started_at',
    *SYSTEM_WORKSPACE_INPUT_KEYS,
}

class ClientAgentService(Service):
    __need_schedule__ = True
    interval = StaticIntervalGenerator(2)

    @staticmethod
    def _normalize_env_parameters(raw_value):
        if isinstance(raw_value, str):
            raw_value = raw_value.strip()
            if not raw_value:
                return {}
            try:
                raw_value = json.loads(raw_value)
            except json.JSONDecodeError:
                return {}

        normalized = {}

        if isinstance(raw_value, dict):
            for key, value in raw_value.items():
                key = str(key).strip()
                if not key:
                    continue
                normalized[key] = value
            return normalized

        if isinstance(raw_value, list):
            for item in raw_value:
                if not isinstance(item, dict):
                    continue
                key = str(item.get('key', '')).strip()
                if not key:
                    continue
                normalized[key] = item.get('value')

        return normalized

    @staticmethod
    def _stringify_environment_value(value):
        if value is None:
            return ''
        if isinstance(value, str):
            return value
        if isinstance(value, (dict, list)):
            return json.dumps(value, ensure_ascii=False)
        return str(value)

    @classmethod
    def _collect_declared_environment(cls, parent_data):
        raw_inputs = getattr(parent_data, 'inputs', {}) or {}
        if not isinstance(raw_inputs, dict):
            return {}

        declared_environment = {}
        for key, value in raw_inputs.items():
            normalized_key = str(key or '').strip()
            if not normalized_key:
                continue
            if normalized_key in ROOT_SYSTEM_INPUT_KEYS:
                continue
            if normalized_key.startswith('tn_') or normalized_key.startswith('__'):
                continue
            declared_environment[normalized_key] = value

        return declared_environment

    @staticmethod
    def _collect_project_global_environment(extra_config):
        from projects.env_params import get_normalized_global_params

        project_environment = {}
        for param in get_normalized_global_params(extra_config):
            key = str(param.get('key', '') or '').strip()
            if not key:
                continue
            project_environment[key] = param.get('default')

        return project_environment

    @staticmethod
    def _normalize_execution_mode(raw_mode):
        mode = str(raw_mode or '').strip().lower()
        if mode == EXECUTION_MODE_CODE:
            return EXECUTION_MODE_CODE
        return EXECUTION_MODE_COMMAND

    @staticmethod
    def _normalize_code_input(raw_value):
        if isinstance(raw_value, str):
            raw_value = raw_value.strip()
            if not raw_value:
                return {'language': 'shell', 'content': ''}
            try:
                raw_value = json.loads(raw_value)
            except json.JSONDecodeError:
                return {'language': 'shell', 'content': raw_value}

        if not isinstance(raw_value, dict):
            return {'language': 'shell', 'content': ''}

        if raw_value.get('language') is None or str(raw_value.get('language')).strip() == '':
            language = 'shell'
        else:
            language = str(raw_value.get('language')).strip().lower()

        content = raw_value.get('content', '')
        if content is None:
            content = ''
        content = str(content)

        return {
            'language': language,
            'content': content,
        }

    @staticmethod
    def _normalize_output_bindings(raw_value):
        if isinstance(raw_value, str):
            raw_value = raw_value.strip()
            if not raw_value:
                return []
            try:
                raw_value = json.loads(raw_value)
            except json.JSONDecodeError:
                return []

        if not isinstance(raw_value, list):
            return []

        normalized = []
        for item in raw_value:
            if not isinstance(item, dict):
                continue
            output_key = str(item.get('output_key') or item.get('outputKey') or '').strip()
            source = str(item.get('source') or '').strip()
            if not output_key or not source:
                continue
            normalized.append({
                'output_key': output_key,
                'source': source,
            })
        return normalized

    @staticmethod
    def _resolve_output_binding_source(source, *, task_id=None, exit_code=None, stdout='', stderr='', result=None):
        result = result if isinstance(result, dict) else {}
        normalized_source = str(source or '').strip()
        if not normalized_source:
            return None

        if normalized_source == 'task_id':
            return task_id
        if normalized_source == 'exit_code':
            return exit_code
        if normalized_source == 'stdout':
            return stdout
        if normalized_source == 'stderr':
            return stderr
        if normalized_source == 'result':
            return result

        if normalized_source.startswith('result.'):
            current = result
            for segment in normalized_source.split('.')[1:]:
                key = segment.strip()
                if not key:
                    return None
                if isinstance(current, dict):
                    if key not in current:
                        return None
                    current = current[key]
                    continue
                if isinstance(current, list) and key.isdigit():
                    index = int(key)
                    if index < 0 or index >= len(current):
                        return None
                    current = current[index]
                    continue
                return None
            return current

        return None

    def _apply_output_bindings(self, data, bindings, *, task_id=None, exit_code=None, stdout='', stderr='', result=None):
        normalized_bindings = self._normalize_output_bindings(bindings)
        if not normalized_bindings:
            return

        for binding in normalized_bindings:
            resolved = self._resolve_output_binding_source(
                binding.get('source'),
                task_id=task_id,
                exit_code=exit_code,
                stdout=stdout,
                stderr=stderr,
                result=result,
            )
            data.set_outputs(binding['output_key'], resolved)
    
    def execute(self, data, parent_data):
        from client_agents.models import AgentWorkspace, ClientAgent
        from projects.models import Project
        
        execution_mode = self._normalize_execution_mode(data.get_one_of_inputs('execution_mode', EXECUTION_MODE_COMMAND))
        command = str(data.get_one_of_inputs('command', '') or '').strip()
        code = self._normalize_code_input(data.get_one_of_inputs('code', {}))
        timeout = data.get_one_of_inputs('timeout', 3600)
        parameters = data.get_one_of_inputs('parameters', [])
        output_bindings = data.get_one_of_inputs('__plugin_output_bindings', [])
        declared_environment = self._collect_declared_environment(parent_data)
        pipeline_id = parent_data.get_one_of_inputs('pipeline_id', '')
        project_id = parent_data.get_one_of_inputs('project_id', '')
        workspace_mode = str(data.get_one_of_inputs('workspace_mode', WORKSPACE_MODE_NONE) or WORKSPACE_MODE_NONE).strip().upper()
        workspace_id = data.get_one_of_inputs('__workspace_id')
        workspace_name = str(data.get_one_of_inputs('__workspace_name', '') or '').strip()
        agent_name = str(data.get_one_of_inputs('__agent_name', '') or '').strip()
        webhook_data = parent_data.get_one_of_inputs('webhook_payload', {})

        if execution_mode == EXECUTION_MODE_COMMAND and not command:
            data.outputs.ex_data = 'Command is required when execution mode is command'
            return False

        if execution_mode == EXECUTION_MODE_CODE and not code.get('content', '').strip():
            data.outputs.ex_data = 'Code content is required when execution mode is code'
            return False

        if execution_mode == EXECUTION_MODE_CODE and code.get('language') not in ALLOWED_CODE_LANGUAGES:
            data.outputs.ex_data = f'Unsupported code language: {code.get("language")}'
            return False

        # 从项目配置读取仓库信息
        client_repo_url = ''
        client_repo_ref = 'main'
        client_repo_token = ''
        project_environment = {}
        if project_id:
            try:
                project = Project.objects.get(id=project_id)
                extra_config = project.extra_config or {}
                client_repo_url = extra_config.get('agent_repo_url', '')
                client_repo_ref = extra_config.get('agent_repo_ref', 'main')
                client_repo_token = extra_config.get('agent_repo_token', '')
                project_environment = self._collect_project_global_environment(extra_config)
            except Project.DoesNotExist:
                pass
        
        display_command = command
        if execution_mode == EXECUTION_MODE_CODE:
            display_command = f'[inline_code:{code.get("language", "shell")}]'

        data.set_outputs('_execution_mode', execution_mode)
        data.set_outputs('_command', command)
        data.set_outputs('_display_command', display_command)
        data.set_outputs('_code', code)
        data.set_outputs('_timeout', int(timeout) if timeout else 3600)
        data.set_outputs('_pipeline_id', pipeline_id)
        data.set_outputs('_client_repo_url', client_repo_url)
        data.set_outputs('_client_repo_ref', client_repo_ref)
        data.set_outputs('_client_repo_token', client_repo_token)
        data.set_outputs('_project_environment', self._normalize_env_parameters(project_environment))
        data.set_outputs('_declared_environment', self._normalize_env_parameters(declared_environment))
        data.set_outputs('_parameters', self._normalize_env_parameters(parameters))
        data.set_outputs('_plugin_output_bindings', self._normalize_output_bindings(output_bindings))
        data.set_outputs('_wait_start_time', timezone.now().isoformat())
        data.set_outputs('_hb_timeout_miss_count', 0)
        data.set_outputs('_hb_last_seen_heartbeat', '')
        data.set_outputs('_tn_webhook_data', webhook_data)

        if workspace_mode == WORKSPACE_MODE_WORKSPACE:
            if not workspace_id:
                data.outputs.ex_data = 'System workspace_id is required when workspace_mode is WORKSPACE'
                return False
            try:
                workspace = AgentWorkspace.objects.get(id=workspace_id)
            except AgentWorkspace.DoesNotExist:
                data.outputs.ex_data = f'Workspace {workspace_id} not found'
                return False
            success = self._dispatch_task(data, workspace=workspace, workspace_name=workspace.name, agent=workspace.agent)
            return success if success else False

        if workspace_mode == WORKSPACE_MODE_SANDBOX:
            if not workspace_name or not agent_name:
                data.outputs.ex_data = 'System workspace_name and agent_name are required when workspace_mode is SANDBOX'
                return False
            try:
                agent = ClientAgent.objects.get(name=agent_name)
            except ClientAgent.DoesNotExist:
                data.outputs.ex_data = f'Client agent "{agent_name}" not found'
                return False
            if agent.status != 'ONLINE':
                data.outputs.ex_data = f'Client agent "{agent_name}" is offline'
                return False
            success = self._dispatch_task(data, workspace=None, workspace_name=workspace_name, agent=agent)
            return success if success else False

        data.outputs.ex_data = f'workspace_mode {workspace_mode} does not provide an executable workspace'
        return False
    
    def _dispatch_task(self, data, *, workspace, workspace_name, agent):
        from client_agents.models import AgentTask

        execution_mode = self._normalize_execution_mode(data.get_one_of_outputs('_execution_mode', EXECUTION_MODE_COMMAND))
        command = data.get_one_of_outputs('_command')
        display_command = str(data.get_one_of_outputs('_display_command', command) or '')
        code = self._normalize_code_input(data.get_one_of_outputs('_code', {}))
        timeout = data.get_one_of_outputs('_timeout', 3600)
        pipeline_id = data.get_one_of_outputs('_pipeline_id', '')
        client_repo_url = data.get_one_of_outputs('_client_repo_url', '')
        client_repo_ref = data.get_one_of_outputs('_client_repo_ref', 'main')
        client_repo_token = data.get_one_of_outputs('_client_repo_token', '')
        webhook_data = {"webhook_payload": self._normalize_env_parameters(data.get_one_of_outputs('_tn_webhook_data', {}))}
        project_environment = self._normalize_env_parameters(data.get_one_of_outputs('_project_environment', {}))
        declared_environment = self._normalize_env_parameters(data.get_one_of_outputs('_declared_environment', {}))
        task_parameters = self._normalize_env_parameters(data.get_one_of_outputs('_parameters', {}))
        base_environment = self._normalize_env_parameters(agent.environment)

        merged_environment = {
            key: self._stringify_environment_value(value)
            for key, value in {
                **webhook_data,
                **project_environment,
                **base_environment,
                **declared_environment,
                **task_parameters,
            }.items()
        }
        
        try:
            agent_task = AgentTask.objects.create(
                agent=agent,
                workspace=workspace,
                pipeline_id=pipeline_id,
                command=display_command,
                timeout=timeout,
                status='PENDING',
            )
            task_id = agent_task.id
            
            data.set_outputs('task_id', task_id)
            data.set_outputs('_dispatch_time', timezone.now().isoformat())
        except Exception as e:
            data.outputs.ex_data = str(e)
            return False
        
        try:
            publish_dispatch_event(
                task_id=task_id,
                agent_id=agent.id,
                payload={
                    "type": "task_dispatch",
                    "task_id": task_id,
                    "workspace_name": workspace_name,
                    "client_repo_url": client_repo_url,
                    "client_repo_ref": client_repo_ref,
                    "client_repo_token": client_repo_token,
                    "execution_mode": execution_mode,
                    "command": command,
                    "code": code,
                    "timeout": timeout,
                    "environment": merged_environment,
                },
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
        
        if status == 'CANCELLED':
            data.set_outputs('_hb_timeout_miss_count', 0)
            data.set_outputs('_hb_last_seen_heartbeat', '')
            return True
        
        if status in ['COMPLETED', 'FAILED', 'TIMEOUT']:
            normalized_result = task.result if isinstance(task.result, dict) else {}
            data.set_outputs('exit_code', task.exit_code if task.exit_code is not None else -1)
            data.set_outputs('stdout', task.stdout)
            data.set_outputs('stderr', task.stderr)
            data.set_outputs('result', normalized_result)
            self._apply_output_bindings(
                data,
                data.get_one_of_outputs('_plugin_output_bindings', []),
                task_id=task_id,
                exit_code=task.exit_code if task.exit_code is not None else -1,
                stdout=task.stdout,
                stderr=task.stderr,
                result=normalized_result,
            )
            data.outputs.ex_data = task.error_message
            data.set_outputs('_hb_timeout_miss_count', 0)
            data.set_outputs('_hb_last_seen_heartbeat', '')
            self.finish_schedule()
            return status == 'COMPLETED'
        
        if status in ['PENDING', 'DISPATCHED', 'RUNNING']:
            # Check heartbeat timeout for running tasks
            if status == 'RUNNING':
                miss_count = int(data.get_one_of_outputs('_hb_timeout_miss_count', 0) or 0)
                last_seen_heartbeat = data.get_one_of_outputs('_hb_last_seen_heartbeat', '') or ''
                current_heartbeat = task.last_heartbeat
                current_heartbeat_iso = current_heartbeat.isoformat() if current_heartbeat else ''

                # 心跳前进说明 agent 仍存活，先清零计数
                if current_heartbeat_iso and current_heartbeat_iso != last_seen_heartbeat:
                    miss_count = 0
                    data.set_outputs('_hb_last_seen_heartbeat', current_heartbeat_iso)

                activity_reference = current_heartbeat or getattr(task, 'started_at', None)

                if activity_reference:
                    activity_elapsed = (timezone.now() - activity_reference).total_seconds()
                    if activity_elapsed > HEARTBEAT_TIMEOUT_SECONDS:
                        miss_count += 1
                        data.set_outputs('_hb_timeout_miss_count', miss_count)
                        if miss_count >= HEARTBEAT_TIMEOUT_RETRY:
                            AgentTask.objects.filter(id=task_id).update(
                                status='FAILED',
                                error_message='Task heartbeat timeout',
                                finished_at=timezone.now()
                            )
                            data.outputs.ex_data = 'Task heartbeat timeout'
                            data.set_outputs('_hb_timeout_miss_count', 0)
                            data.set_outputs('_hb_last_seen_heartbeat', '')
                            self.finish_schedule()
                            return False
                    else:
                        # 未超时则立即恢复
                        if miss_count != 0:
                            miss_count = 0
                        data.set_outputs('_hb_timeout_miss_count', miss_count)
                else:
                    # started_at 也不存在时，回退到原来的等待逻辑
                    if miss_count != 0:
                        miss_count = 0
                    data.set_outputs('_hb_timeout_miss_count', miss_count)
            
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
                        data.set_outputs('_hb_timeout_miss_count', 0)
                        data.set_outputs('_hb_last_seen_heartbeat', '')
                        self.finish_schedule()
                        return False
                except (ValueError, TypeError):
                    pass
        
        return True
    
    def inputs_format(self):
        return [
            self.InputItem(
                name='Execution Mode',
                key='execution_mode',
                type='string',
                required=False,
                schema=ExtendedStringSchema(
                    description='Task execution mode',
                    param_type='select',
                    enum=[EXECUTION_MODE_COMMAND, EXECUTION_MODE_CODE],
                ),
            ),
            self.InputItem(
                name='Command',
                key='command',
                type='string',
                required=False,
                schema=ExtendedStringSchema(
                    description='Script path or command to execute',
                    visible_when={'execution_mode': EXECUTION_MODE_COMMAND},
                ),
            ),
            self.InputItem(
                name='Code',
                key='code',
                type='object',
                required=False,
                schema=ExtendedObjectSchema(
                    property_schemas={
                        'language': StringItemSchema(
                            description='Code language. One of shell/python',
                            enum=['shell', 'python'],
                        ),
                        'content': StringItemSchema(description='Code content to execute'),
                    },
                    description='Inline code execution payload',
                    param_type='code_editor',
                    visible_when={'execution_mode': EXECUTION_MODE_CODE},
                ),
            ),
            self.InputItem(name='Timeout (s)', key='timeout', type='int', required=False),
            self.InputItem(
                name='Parameters',
                key='parameters',
                type='list',
                required=False,
                schema=ExtendedArraySchema(
                    item_schema=ObjectItemSchema(
                        property_schemas={
                            'key': StringItemSchema(description='Environment variable key'),
                            'value': StringItemSchema(description='Environment variable value'),
                        },
                        description='Environment variable pair',
                    ),
                    description='Additional environment variables for this task',
                    param_type='key_values',
                ),
            ),
        ]

    def outputs_format(self):
        return [
            self.OutputItem(name='Task ID', key='task_id', type='string'),
            self.OutputItem(name='Exit Code', key='exit_code', type='int'),
            self.OutputItem(name='Standard Output', key='stdout', type='string'),
            self.OutputItem(name='Standard Error', key='stderr', type='string'),
            self.OutputItem(
                name='Result',
                key='result',
                type='object',
                schema=ObjectItemSchema(
                    property_schemas={},
                    description='Structured task result for splice access, e.g. ${result["file_name"]}',
                ),
            ),
        ]

class ClientAgentComponent(Component):
    name = '客户端代理'
    code = 'client_agent'
    bound_service = ClientAgentService
    version = '1.3'
    category = 'ClientAgent'
    icon = "faComputer"
    description = '将脚本分发给客户端代理执行'
