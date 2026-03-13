import logging
import json
from uuid import uuid4

from django.utils import timezone
from pipeline.component_framework.component import Component
from pipeline.core.flow.activity import Service, StaticIntervalGenerator
from pipeline.core.flow.io import StringItemSchema, ObjectItemSchema

from components.schemas import ExtendedArraySchema, ExtendedStringSchema, ExtendedObjectSchema
from client_agents.dispatch_stream import publish_dispatch_event


logger = logging.getLogger('django')

MAX_WAIT_FOR_AGENT = 600  # 10 minutes
HEARTBEAT_TIMEOUT_SECONDS = 120
HEARTBEAT_TIMEOUT_RETRY = 3
EXECUTION_MODE_COMMAND = 'command'
EXECUTION_MODE_CODE = 'code'
ALLOWED_CODE_LANGUAGES = {'shell', 'python'}


class ClientSandboxService(Service):
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
                normalized[key] = '' if value is None else str(value)
            return normalized

        if isinstance(raw_value, list):
            for item in raw_value:
                if not isinstance(item, dict):
                    continue
                key = str(item.get('key', '')).strip()
                if not key:
                    continue
                value = item.get('value', '')
                normalized[key] = '' if value is None else str(value)

        return normalized

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
    def _build_temp_workspace_name():
        return f"sandbox_{uuid4().hex}"

    @staticmethod
    def _get_registered_client_agents():
        from client_agents.models import ClientAgent

        try:
            return list(
                ClientAgent.objects.order_by('name').values_list('name', flat=True)
            )
        except Exception:
            logger.exception('Failed to load registered client agents for component schema')
            return []

    def execute(self, data, parent_data):
        from client_agents.models import ClientAgent
        from projects.models import Project

        client_agent_name = str(data.get_one_of_inputs('client_agent', '') or '').strip()
        execution_mode = self._normalize_execution_mode(
            data.get_one_of_inputs('execution_mode', EXECUTION_MODE_COMMAND)
        )
        command = str(data.get_one_of_inputs('command', '') or '').strip()
        code = self._normalize_code_input(data.get_one_of_inputs('code', {}))
        timeout = data.get_one_of_inputs('timeout', 3600)
        parameters = data.get_one_of_inputs('parameters', [])
        pipeline_id = parent_data.get_one_of_inputs('pipeline_id', '')
        project_id = parent_data.get_one_of_inputs('project_id', '')

        if not client_agent_name:
            data.outputs.ex_data = 'Client agent is required'
            return False

        if execution_mode == EXECUTION_MODE_COMMAND and not command:
            data.outputs.ex_data = 'Command is required when execution mode is command'
            return False

        if execution_mode == EXECUTION_MODE_CODE and not code.get('content', '').strip():
            data.outputs.ex_data = 'Code content is required when execution mode is code'
            return False

        if execution_mode == EXECUTION_MODE_CODE and code.get('language') not in ALLOWED_CODE_LANGUAGES:
            data.outputs.ex_data = f'Unsupported code language: {code.get("language")}'
            return False

        client_repo_url = ''
        client_repo_ref = 'main'
        client_repo_token = ''
        if project_id:
            try:
                project = Project.objects.get(id=project_id)
                extra_config = project.extra_config or {}
                client_repo_url = extra_config.get('agent_repo_url', '')
                client_repo_ref = extra_config.get('agent_repo_ref', 'main')
                client_repo_token = extra_config.get('agent_repo_token', '')
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
        data.set_outputs('_parameters', self._normalize_env_parameters(parameters))
        data.set_outputs('_wait_start_time', timezone.now().isoformat())
        data.set_outputs('_hb_timeout_miss_count', 0)
        data.set_outputs('_hb_last_seen_heartbeat', '')

        try:
            agent = ClientAgent.objects.get(name=client_agent_name)
        except ClientAgent.DoesNotExist:
            data.outputs.ex_data = f'Client agent "{client_agent_name}" not found'
            return False

        if agent.status != 'ONLINE':
            data.outputs.ex_data = f'Client agent "{client_agent_name}" is offline'
            return False

        workspace_name = self._build_temp_workspace_name()
        data.set_outputs('_workspace_name', workspace_name)
        return self._dispatch_task(data, agent, workspace_name)

    def _dispatch_task(self, data, agent, workspace_name):
        from client_agents.models import AgentTask

        execution_mode = self._normalize_execution_mode(
            data.get_one_of_outputs('_execution_mode', EXECUTION_MODE_COMMAND)
        )
        command = data.get_one_of_outputs('_command')
        display_command = str(data.get_one_of_outputs('_display_command', command) or '')
        code = self._normalize_code_input(data.get_one_of_outputs('_code', {}))
        timeout = data.get_one_of_outputs('_timeout', 3600)
        pipeline_id = data.get_one_of_outputs('_pipeline_id', '')
        client_repo_url = data.get_one_of_outputs('_client_repo_url', '')
        client_repo_ref = data.get_one_of_outputs('_client_repo_ref', 'main')
        client_repo_token = data.get_one_of_outputs('_client_repo_token', '')
        task_parameters = self._normalize_env_parameters(data.get_one_of_outputs('_parameters', {}))
        base_environment = self._normalize_env_parameters(agent.environment)
        merged_environment = {**base_environment, **task_parameters}

        try:
            agent_task = AgentTask.objects.create(
                agent=agent,
                workspace=None,
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
                    "prepare_repo_before_execute": True,
                    "cleanup_workspace_on_success": True,
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
        from client_agents.models import AgentTask
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
            data.set_outputs('exit_code', task.exit_code if task.exit_code is not None else -1)
            data.set_outputs('stdout', task.stdout)
            data.set_outputs('stderr', task.stderr)
            data.set_outputs('result', task.result if isinstance(task.result, dict) else {})
            data.outputs.ex_data = task.error_message
            data.set_outputs('_hb_timeout_miss_count', 0)
            data.set_outputs('_hb_last_seen_heartbeat', '')
            self.finish_schedule()
            return status == 'COMPLETED'

        if status in ['PENDING', 'DISPATCHED', 'RUNNING']:
            if status == 'RUNNING':
                miss_count = int(data.get_one_of_outputs('_hb_timeout_miss_count', 0) or 0)
                last_seen_heartbeat = data.get_one_of_outputs('_hb_last_seen_heartbeat', '') or ''
                current_heartbeat = task.last_heartbeat
                current_heartbeat_iso = current_heartbeat.isoformat() if current_heartbeat else ''

                if current_heartbeat_iso and current_heartbeat_iso != last_seen_heartbeat:
                    miss_count = 0
                    data.set_outputs('_hb_last_seen_heartbeat', current_heartbeat_iso)

                if current_heartbeat:
                    heartbeat_elapsed = (timezone.now() - current_heartbeat).total_seconds()
                    if heartbeat_elapsed > HEARTBEAT_TIMEOUT_SECONDS:
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
                        if miss_count != 0:
                            miss_count = 0
                        data.set_outputs('_hb_timeout_miss_count', miss_count)
                else:
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
                name='Client Agent',
                key='client_agent',
                type='string',
                required=True,
                schema=ExtendedStringSchema(
                    description='Type or select a registered client agent',
                    param_type='editable_select',
                    enum=self._get_registered_client_agents(),
                ),
            ),
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


class ClientSandboxComponent(Component):
    name = '客户端代理（沙箱）'
    code = 'client_sandbox'
    bound_service = ClientSandboxService
    version = '1.0'
    category = 'ClientAgent'
    icon = "Computer"
    description = '指定客户端代理并使用临时工作空间执行任务'
