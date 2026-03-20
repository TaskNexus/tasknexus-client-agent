import logging
import re
from uuid import uuid4

from django.db import transaction
from django.utils import timezone
from pipeline.component_framework.component import Component
from pipeline.core.flow.activity import Service, StaticIntervalGenerator

from client_agents.dispatch_stream import publish_dispatch_event
from tasks.workspace_runtime import upsert_workspace_scope
from workflows.workspace import (
    WORKSPACE_MODE_NONE,
    WORKSPACE_MODE_SANDBOX,
    WORKSPACE_MODE_WORKSPACE,
    WORKSPACE_STRATEGY_REUSE_PARENT,
)


logger = logging.getLogger("django")

MAX_WAIT_FOR_WORKSPACE = 600
MAX_SANDBOX_WORKSPACE_PREFIX_LENGTH = 48


class WorkspaceAcquireService(Service):
    __need_schedule__ = True
    interval = StaticIntervalGenerator(2)

    def _record_scope(self, **patch):
        upsert_workspace_scope(
            self.root_pipeline_id,
            self.top_pipeline_id,
            {
                "node_id": self.id,
                **patch,
            },
        )

    def _get_project_repo_config(self, project_id):
        from projects.models import Project

        client_repo_url = ""
        client_repo_ref = "main"
        client_repo_token = ""

        if not project_id:
            return client_repo_url, client_repo_ref, client_repo_token

        try:
            project = Project.objects.get(id=project_id)
        except Project.DoesNotExist:
            logger.warning("Project %s not found, skipping repo config", project_id)
            return client_repo_url, client_repo_ref, client_repo_token

        extra_config = project.extra_config or {}
        client_repo_url = extra_config.get("agent_repo_url", "")
        client_repo_ref = extra_config.get("agent_repo_ref", "main") or "main"
        client_repo_token = extra_config.get("agent_repo_token", "")
        return client_repo_url, client_repo_ref, client_repo_token

    def _set_system_outputs(
        self,
        data,
        *,
        workspace_id="",
        workspace_name="",
        agent_name="",
        workspace_mode="",
        workspace_label="",
        workspace_agent_name="",
    ):
        data.set_outputs("__workspace_id", workspace_id)
        data.set_outputs("__workspace_name", workspace_name)
        data.set_outputs("__agent_name", agent_name)
        data.set_outputs("workspace_mode", workspace_mode)
        data.set_outputs("workspace_label", workspace_label)
        data.set_outputs("workspace_agent_name", workspace_agent_name)

    def _try_acquire_workspace(self, workspace_label, pipeline_id="", workspace_agent_name=""):
        from client_agents.models import AgentWorkspace

        base_qs = AgentWorkspace.objects.filter(
            status="IDLE",
            agent__status="ONLINE",
            occupied_by__isnull=True,
        )
        if workspace_agent_name:
            base_qs = base_qs.filter(agent__name=workspace_agent_name)

        if workspace_label:
            workspace = base_qs.filter(labels__contains=[workspace_label]).order_by("?").first()
        else:
            workspace = base_qs.order_by("?").first()

        if not workspace:
            return None

        with transaction.atomic():
            locked = (
                AgentWorkspace.objects.select_for_update(nowait=True)
                .filter(id=workspace.id, status="IDLE", occupied_by__isnull=True)
                .first()
            )
            if not locked:
                return None

            locked.status = "RUNNING"
            locked.pipeline_id = pipeline_id
            locked.save(update_fields=["status", "pipeline_id"])
            return locked

    def _dispatch_workspace_prepare_task(self, data, workspace):
        from client_agents.models import AgentTask

        agent = workspace.agent
        client_repo_url = data.get_one_of_outputs("_client_repo_url", "")
        client_repo_ref = data.get_one_of_outputs("_client_repo_ref", "main")
        client_repo_token = data.get_one_of_outputs("_client_repo_token", "")
        pipeline_id = data.get_one_of_outputs("_pipeline_id", "")

        try:
            task = AgentTask.objects.create(
                agent=agent,
                workspace=workspace,
                pipeline_id=pipeline_id,
                client_repo_url=client_repo_url,
                client_repo_ref=client_repo_ref,
                command="[workspace_prepare]",
                timeout=300,
                status="PENDING",
            )
        except Exception as exc:
            data.outputs.ex_data = f"Failed to create workspace prepare task: {exc}"
            return False

        data.set_outputs("_prepare_task_id", task.id)
        try:
            publish_dispatch_event(
                task_id=task.id,
                agent_id=agent.id,
                payload={
                    "type": "task_dispatch",
                    "task_id": task.id,
                    "workspace_name": workspace.name,
                    "client_repo_url": client_repo_url,
                    "client_repo_ref": client_repo_ref,
                    "client_repo_token": client_repo_token,
                    "execution_mode": "command",
                    "command": "",
                    "timeout": 300,
                    "environment": agent.environment,
                    "prepare_repo_before_execute": True,
                    "cleanup_workspace_on_success": False,
                },
            )
            return True
        except Exception as exc:
            AgentTask.objects.filter(id=task.id).update(
                status="FAILED",
                error_message=str(exc),
                finished_at=timezone.now(),
            )
            data.outputs.ex_data = f"Failed to dispatch workspace prepare task: {exc}"
            return False

    def _select_online_agent(self, requested_agent_name=""):
        from client_agents.models import ClientAgent

        qs = ClientAgent.objects.filter(status="ONLINE")
        if requested_agent_name:
            qs = qs.filter(name=requested_agent_name)
        return qs.order_by("name").first()

    def _build_sandbox_workspace_name(self, workspace_label):
        normalized = re.sub(r"[^A-Za-z0-9_-]+", "_", str(workspace_label or "").strip()).strip("_")
        normalized = normalized or "sandbox"
        normalized = normalized[:MAX_SANDBOX_WORKSPACE_PREFIX_LENGTH].rstrip("_") or "sandbox"
        return f"{normalized}_{uuid4().hex}"

    def _dispatch_sandbox_prepare_task(self, data, agent, workspace_name):
        from client_agents.models import AgentTask

        pipeline_id = data.get_one_of_outputs("_pipeline_id", "")
        client_repo_url = data.get_one_of_outputs("_client_repo_url", "")
        client_repo_ref = data.get_one_of_outputs("_client_repo_ref", "main")
        client_repo_token = data.get_one_of_outputs("_client_repo_token", "")

        try:
            task = AgentTask.objects.create(
                agent=agent,
                workspace=None,
                pipeline_id=pipeline_id,
                client_repo_url=client_repo_url,
                client_repo_ref=client_repo_ref,
                command="[sandbox_prepare]",
                timeout=300,
                status="PENDING",
            )
        except Exception as exc:
            data.outputs.ex_data = f"Failed to create sandbox prepare task: {exc}"
            return False

        data.set_outputs("_prepare_task_id", task.id)
        try:
            publish_dispatch_event(
                task_id=task.id,
                agent_id=agent.id,
                payload={
                    "type": "task_dispatch",
                    "task_id": task.id,
                    "workspace_name": workspace_name,
                    "client_repo_url": client_repo_url,
                    "client_repo_ref": client_repo_ref,
                    "client_repo_token": client_repo_token,
                    "execution_mode": "command",
                    "command": "",
                    "timeout": 300,
                    "environment": agent.environment,
                    "prepare_repo_before_execute": True,
                    "cleanup_workspace_on_success": False,
                },
            )
            return True
        except Exception as exc:
            AgentTask.objects.filter(id=task.id).update(
                status="FAILED",
                error_message=str(exc),
                finished_at=timezone.now(),
            )
            data.outputs.ex_data = f"Failed to dispatch sandbox prepare task: {exc}"
            return False

    def _mark_ready(
        self,
        data,
        *,
        owner,
        resource_type,
        workspace_id="",
        workspace_name="",
        agent_name="",
        workspace_mode="",
        workspace_label="",
        workspace_agent_name="",
        borrowed_from="",
    ):
        self._set_system_outputs(
            data,
            workspace_id=workspace_id,
            workspace_name=workspace_name,
            agent_name=agent_name,
            workspace_mode=workspace_mode,
            workspace_label=workspace_label,
            workspace_agent_name=workspace_agent_name,
        )
        self._record_scope(
            phase="ready",
            mode=workspace_mode,
            label=workspace_label,
            owner=owner,
            resource_type=resource_type,
            workspace_id=workspace_id or None,
            workspace_name=workspace_name,
            agent_name=agent_name,
            borrowed_from=borrowed_from,
        )
        data.set_outputs("_phase", "ready")
        self.finish_schedule()
        return True

    def execute(self, data, parent_data):
        workspace_strategy = str(data.get_one_of_inputs("workspace_strategy", "") or "").strip().upper()
        workspace_mode = str(data.get_one_of_inputs("workspace_mode", WORKSPACE_MODE_NONE) or WORKSPACE_MODE_NONE).strip().upper()
        workspace_label = str(data.get_one_of_inputs("workspace_label", "") or "").strip()
        workspace_agent_name = str(data.get_one_of_inputs("workspace_agent_name", "") or "").strip()
        inherited_workspace_id = data.get_one_of_inputs("__workspace_id", "")
        inherited_workspace_name = str(data.get_one_of_inputs("__workspace_name", "") or "").strip()
        inherited_agent_name = str(data.get_one_of_inputs("__agent_name", "") or "").strip()
        timeout = int(data.get_one_of_inputs("timeout", MAX_WAIT_FOR_WORKSPACE) or MAX_WAIT_FOR_WORKSPACE)
        pipeline_id = parent_data.get_one_of_inputs("pipeline_id", "")
        project_id = parent_data.get_one_of_inputs("project_id", "")

        client_repo_url, client_repo_ref, client_repo_token = self._get_project_repo_config(project_id)
        data.set_outputs("_pipeline_id", pipeline_id)
        data.set_outputs("_timeout", timeout)
        data.set_outputs("_wait_start_time", timezone.now())
        data.set_outputs("_client_repo_url", client_repo_url)
        data.set_outputs("_client_repo_ref", client_repo_ref)
        data.set_outputs("_client_repo_token", client_repo_token)
        data.set_outputs("_resource_type", "none")
        self._set_system_outputs(
            data,
            workspace_id=inherited_workspace_id,
            workspace_name=inherited_workspace_name,
            agent_name=inherited_agent_name,
            workspace_mode=workspace_mode,
            workspace_label=workspace_label,
            workspace_agent_name=workspace_agent_name,
        )

        if workspace_strategy == WORKSPACE_STRATEGY_REUSE_PARENT:
            resource_type = "none"
            if workspace_mode == WORKSPACE_MODE_SANDBOX:
                resource_type = "sandbox"
            elif inherited_workspace_id:
                resource_type = "workspace"
            self._set_system_outputs(
                data,
                workspace_id=inherited_workspace_id,
                workspace_name=inherited_workspace_name,
                agent_name=inherited_agent_name,
                workspace_mode=workspace_mode,
                workspace_label=workspace_label,
                workspace_agent_name=workspace_agent_name,
            )
            self._record_scope(
                phase="reused_parent",
                mode=workspace_mode,
                label=workspace_label,
                owner=False,
                resource_type=resource_type,
                workspace_id=inherited_workspace_id or None,
                workspace_name=inherited_workspace_name,
                agent_name=inherited_agent_name,
                borrowed_from=pipeline_id,
            )
            data.set_outputs("_phase", "reused_parent")
            self.finish_schedule()
            return True

        if workspace_mode == WORKSPACE_MODE_NONE:
            self._set_system_outputs(
                data,
                workspace_id="",
                workspace_name="",
                agent_name="",
                workspace_mode=workspace_mode,
                workspace_label="",
                workspace_agent_name="",
            )
            self._record_scope(
                phase="ready",
                mode=workspace_mode,
                label="",
                owner=False,
                resource_type="none",
                workspace_id=None,
                workspace_name="",
                agent_name="",
                borrowed_from="",
            )
            data.set_outputs("_phase", "ready")
            self.finish_schedule()
            return True

        if workspace_mode == WORKSPACE_MODE_WORKSPACE:
            self._record_scope(
                phase="acquiring_workspace",
                mode=workspace_mode,
                label=workspace_label,
                owner=True,
                resource_type="workspace",
                workspace_id=None,
                workspace_name="",
                agent_name="",
                borrowed_from="",
            )
            data.set_outputs("_phase", "acquiring_workspace")
            data.set_outputs("_resource_type", "workspace")
            workspace = self._try_acquire_workspace(
                workspace_label,
                pipeline_id,
                workspace_agent_name,
            )
            if not workspace:
                self._record_scope(phase="waiting")
                data.set_outputs("_phase", "waiting")
                return True

            data.set_outputs("_workspace_id_cached", workspace.id)
            data.set_outputs("_workspace_name_cached", workspace.name)
            data.set_outputs("_agent_name_cached", workspace.agent.name)
            if client_repo_url:
                self._set_system_outputs(
                    data,
                    workspace_id=workspace.id,
                    workspace_name=workspace.name,
                    agent_name=workspace.agent.name,
                    workspace_mode=workspace_mode,
                    workspace_label=workspace_label,
                    workspace_agent_name=workspace_agent_name,
                )
                self._record_scope(
                    phase="preparing_repo",
                    mode=workspace_mode,
                    label=workspace_label,
                    owner=True,
                    resource_type="workspace",
                    workspace_id=workspace.id,
                    workspace_name=workspace.name,
                    agent_name=workspace.agent.name,
                    borrowed_from="",
                )
                data.set_outputs("_phase", "preparing_repo")
                return self._dispatch_workspace_prepare_task(data, workspace)

            return self._mark_ready(
                data,
                owner=True,
                resource_type="workspace",
                workspace_id=workspace.id,
                workspace_name=workspace.name,
                agent_name=workspace.agent.name,
                workspace_mode=workspace_mode,
                workspace_label=workspace_label,
                workspace_agent_name=workspace_agent_name,
            )

        if workspace_mode == WORKSPACE_MODE_SANDBOX:
            self._record_scope(
                phase="selecting_agent",
                mode=workspace_mode,
                label=workspace_label,
                owner=True,
                resource_type="sandbox",
                workspace_id=None,
                workspace_name="",
                agent_name="",
                borrowed_from="",
            )
            data.set_outputs("_phase", "selecting_agent")
            data.set_outputs("_resource_type", "sandbox")
            agent = self._select_online_agent(workspace_agent_name)
            if not agent:
                if workspace_agent_name:
                    data.outputs.ex_data = (
                        f'Requested agent "{workspace_agent_name}" is not available online for sandbox mode'
                    )
                else:
                    data.outputs.ex_data = "No online agent available for sandbox mode"
                return False

            workspace_name = self._build_sandbox_workspace_name(workspace_label)
            data.set_outputs("_workspace_name_cached", workspace_name)
            data.set_outputs("_agent_name_cached", agent.name)
            self._set_system_outputs(
                data,
                workspace_id="",
                workspace_name=workspace_name,
                agent_name=agent.name,
                workspace_mode=workspace_mode,
                workspace_label=workspace_label,
                workspace_agent_name=workspace_agent_name,
            )
            self._record_scope(
                phase="preparing_repo",
                mode=workspace_mode,
                label=workspace_label,
                owner=True,
                resource_type="sandbox",
                workspace_id=None,
                workspace_name=workspace_name,
                agent_name=agent.name,
                borrowed_from="",
            )
            data.set_outputs("_phase", "preparing_repo")
            return self._dispatch_sandbox_prepare_task(data, agent, workspace_name)

        data.outputs.ex_data = f"Unsupported workspace_mode: {workspace_mode}"
        return False

    def schedule(self, data, parent_data, callback_data=None):
        from client_agents.models import AgentTask

        phase = data.get_one_of_outputs("_phase", "waiting")
        workspace_mode = str(data.get_one_of_outputs("workspace_mode", "") or "").strip().upper()
        workspace_label = str(data.get_one_of_outputs("workspace_label", "") or "").strip()
        workspace_agent_name = str(data.get_one_of_outputs("workspace_agent_name", "") or "").strip()

        if phase in {"ready", "reused_parent"}:
            self.finish_schedule()
            return True

        if phase in {"waiting", "acquiring_workspace"}:
            wait_start = data.get_one_of_outputs("_wait_start_time")
            timeout = int(data.get_one_of_outputs("_timeout", MAX_WAIT_FOR_WORKSPACE) or MAX_WAIT_FOR_WORKSPACE)
            if wait_start:
                elapsed = (timezone.now() - wait_start).total_seconds()
                if elapsed > timeout:
                    data.outputs.ex_data = f"Timed out waiting for workspace after {timeout} seconds"
                    self.finish_schedule()
                    return False

            pipeline_id = data.get_one_of_outputs("_pipeline_id", "")
            workspace = self._try_acquire_workspace(
                workspace_label,
                pipeline_id,
                workspace_agent_name,
            )
            if not workspace:
                self._record_scope(phase="waiting")
                data.set_outputs("_phase", "waiting")
                return True

            client_repo_url = data.get_one_of_outputs("_client_repo_url", "")
            if client_repo_url:
                self._set_system_outputs(
                    data,
                    workspace_id=workspace.id,
                    workspace_name=workspace.name,
                    agent_name=workspace.agent.name,
                    workspace_mode=workspace_mode,
                    workspace_label=workspace_label,
                    workspace_agent_name=workspace_agent_name,
                )
                self._record_scope(
                    phase="preparing_repo",
                    mode=workspace_mode,
                    label=workspace_label,
                    owner=True,
                    resource_type="workspace",
                    workspace_id=workspace.id,
                    workspace_name=workspace.name,
                    agent_name=workspace.agent.name,
                    borrowed_from="",
                )
                data.set_outputs("_phase", "preparing_repo")
                return self._dispatch_workspace_prepare_task(data, workspace)

            return self._mark_ready(
                data,
                owner=True,
                resource_type="workspace",
                workspace_id=workspace.id,
                workspace_name=workspace.name,
                agent_name=workspace.agent.name,
                workspace_mode=workspace_mode,
                workspace_label=workspace_label,
                workspace_agent_name=workspace_agent_name,
            )

        if phase == "preparing_repo":
            prepare_task_id = data.get_one_of_outputs("_prepare_task_id")
            if not prepare_task_id:
                data.outputs.ex_data = "Prepare task ID not found"
                self.finish_schedule()
                return False

            try:
                task = AgentTask.objects.get(id=prepare_task_id)
            except AgentTask.DoesNotExist:
                data.outputs.ex_data = "Prepare task not found in database"
                self.finish_schedule()
                return False

            if task.status == "COMPLETED":
                return self._mark_ready(
                    data,
                    owner=True,
                    resource_type=str(data.get_one_of_outputs("_resource_type", "none") or "none"),
                    workspace_id=data.get_one_of_outputs("__workspace_id", ""),
                    workspace_name=str(data.get_one_of_outputs("__workspace_name", "") or ""),
                    agent_name=str(data.get_one_of_outputs("__agent_name", "") or ""),
                    workspace_mode=workspace_mode,
                    workspace_label=workspace_label,
                    workspace_agent_name=workspace_agent_name,
                )

            if task.status in {"FAILED", "TIMEOUT", "CANCELLED"}:
                data.outputs.ex_data = task.error_message or task.stderr or task.status
                self.finish_schedule()
                return False

            return True

        self.finish_schedule()
        return True

    def inputs_format(self):
        return [
            self.InputItem(name="Workspace Strategy", key="workspace_strategy", type="string", required=False),
            self.InputItem(name="Workspace Mode", key="workspace_mode", type="string", required=False),
            self.InputItem(name="Workspace Label", key="workspace_label", type="string", required=False),
            self.InputItem(name="Workspace Agent Name", key="workspace_agent_name", type="string", required=False),
            self.InputItem(name="Inherited Workspace ID", key="__workspace_id", type="string", required=False),
            self.InputItem(name="Inherited Workspace Name", key="__workspace_name", type="string", required=False),
            self.InputItem(name="Inherited Agent Name", key="__agent_name", type="string", required=False),
            self.InputItem(name="Timeout (s)", key="timeout", type="int", required=False),
        ]

    def outputs_format(self):
        return [
            self.OutputItem(name="Workspace ID", key="__workspace_id", type="string"),
            self.OutputItem(name="Workspace Name", key="__workspace_name", type="string"),
            self.OutputItem(name="Agent Name", key="__agent_name", type="string"),
            self.OutputItem(name="Workspace Mode", key="workspace_mode", type="string"),
            self.OutputItem(name="Workspace Label", key="workspace_label", type="string"),
            self.OutputItem(name="Workspace Agent Name", key="workspace_agent_name", type="string"),
        ]


class WorkspaceAcquireComponent(Component):
    name = "获取工作空间"
    code = "workspace_acquire"
    bound_service = WorkspaceAcquireService
    version = "1.0"
    category = "ClientAgent"
    icon = "faComputer"
    description = "获取并锁定工作空间，并克隆/更新代码仓库"
