# Research #3110: how comparable systems separate workspace home, workflow instance and deliverables, and where the agent loop runs

Part of map #3108. Researched 2026-10-12.

**Method.** Facts come from primary docs and API references fetched on 2026-10-12, with URLs given inline. **[prior]** marks a claim I could not confirm in fetched text. **[uncertain]** marks an inference, or a claim from a secondary source such as a GitHub issue or search snippet.

**Flotilla context.** Read against CONTEXT.md; ADRs 0017, 0021, 0028, 0032, 0049, 0050 and 0061; and the de facto Convoy machine survey (`tmp/sm/convoy.md`).

**Purpose.** This report describes patterns. It deliberately does not choose flotilla's model.

The six concerns the Convoy bundles today, as named in #3108, are used as the comparison axes throughout:

| Axis | Flotilla today (where it lives) |
|---|---|
| **A. Vessel home**: owner of execution environments | The Convoy owns its Vessels through a creation cascade and a teardown finalizer (`flotilla.work/convoy-teardown`). A Vessel's home is its placement host (ADR 0016/0033). Free-floating Vessels, warm-kept Vessels and Succession exist in CONTEXT.md but not in code. |
| **B. Workflow instance**: orchestration of one run | Convoy spec `workflow_ref`, frozen to a `workflow-snapshot-<hash>` template. The `work[vessel].phase` DAG lives in Convoy status. |
| **C. Deliverables checklist** | `status.promises[vessel][role][]`, with states Open, Submitted, Kept and Retracted (ADR 0061), and `subjects` (ADR 0049). All of it sits inside Convoy status. |
| **D. Landing authority** | Exit Table plus the Settle edge (R19) in the Convoy reconciler. The Active→Landing edge is written by claims (ADR 0021/0028). |
| **E. Generation identity** | One Convoy record per generation of a standing ensure. The label triple is `{project, role, generation}`, with at most one live generation (ADR 0032). |
| **F. Crew-work state** | `status.crew_work[vessel][role]` inside the Convoy. Crew work is not a resource. |

---

## 1. Kubernetes

**Ownership mechanism.**
- `metadata.ownerReferences[]` holds `{apiVersion, kind, name, uid, controller, blockOwnerDeletion}`. At most one entry has `controller: true` (https://kubernetes.io/docs/concepts/overview/working-with-objects/owners-dependents/).
- The garbage collector resolves owners **by uid, not by name**. A recreated owner with the same name does not adopt the old dependents.
- Owner references never cross namespaces. An out-of-namespace owner reference counts as absent, and the dependent becomes collectable (https://kubernetes.io/docs/concepts/architecture/garbage-collection/).
- Cascading deletion has three modes:
  - **Foreground**: the API server sets `deletionTimestamp` and the `foregroundDeletion` finalizer. Dependents with `blockOwnerDeletion=true` go first; the owner stays visible until they are gone.
  - **Background** (the default): the owner goes immediately and the garbage collector cleans up afterwards.
  - **Orphan**: dependents are left behind.

**Generation and observation.**
- `metadata.generation` increments monotonically on spec changes.
- Every condition carries `observedGeneration`: the generation it was computed from.
- Conditions are `{type, status: True|False|Unknown, reason, message, lastTransitionTime}` (https://raw.githubusercontent.com/kubernetes/community/master/contributors/devel/sig-architecture/api-conventions.md).
- `uid` tells apart two objects that share a name.

**Pods are never moved.**
- "A given Pod (as defined by a UID) is never 'rescheduled' to a different node."
- On node loss, the Pods on that node are set to Failed, and recovery is always a new Pod with a new uid.
- `restartPolicy` restarts containers in place, on the same node and in the same Pod.
- The `DisruptionTarget` condition marks a Pod being preempted or evicted (https://raw.githubusercontent.com/kubernetes/website/main/content/en/docs/concepts/workloads/pods/pod-lifecycle.md).

**Job (batch/v1)** (https://kubernetes.io/docs/reference/kubernetes-api/workload-resources/job-v1/; job.md in kubernetes/website)

Spec:
- `completions`, `parallelism`, `completionMode` (NonIndexed or Indexed). Indexed Pods get `JOB_COMPLETION_INDEX`.
- `backoffLimit` (default 6), `backoffLimitPerIndex`, `maxFailedIndexes`.
- `podFailurePolicy.rules[]`, with actions `FailJob`, `Ignore`, `Count` and `FailIndex`, matched by `onExitCodes` or `onPodConditions`. The canonical use is `Ignore` on `DisruptionTarget`, so that infrastructure disruption does not consume the failure budget.
- `successPolicy`, `podReplacementPolicy`, `suspend`, `activeDeadlineSeconds`, `ttlSecondsAfterFinished`.
- `managedBy`: hands the object to an external controller.

Status:
- `active`, `ready`, `terminating`, `succeeded`, `failed`, `completedIndexes`, `failedIndexes`, `uncountedTerminatedPods`, `conditions`.

Termination is two-phase:
- The controller first writes `FailureTarget` or `SuccessCriteriaMet`, which is the **decision**.
- From v1.31 it writes the terminal `Failed` or `Complete` only once all Pods have actually terminated, which is the **confirmation**.

Pod accounting:
- Pods carry the `batch.kubernetes.io/job-tracking` finalizer.
- The controller stages Pod uids in `uncountedTerminatedPods` before folding them into its counters, so a crash between steps neither loses nor double-counts a Pod. **[prior]** for the exact three-step order.

Unmanaged deletion:
- An unmanaged Job deletes with `orphanDependents` by default and leaves its Pods behind.

**CronJob as a generator.**
- The CronJob owns its Jobs, and each Job owns its Pods.
- `concurrencyPolicy` is `Allow`, `Forbid` or `Replace`.
- `successfulJobsHistoryLimit` defaults to 3 and `failedJobsHistoryLimit` to 1.
- The Job name is the CronJob name plus an 11-character suffix derived from the scheduled time, which makes creation idempotent per schedule slot. **[prior]** for the derivation.
- Runs are created "approximately once" per slot (https://kubernetes.io/docs/concepts/workloads/controllers/cron-jobs/).

**Revisions as generations.**
- Deployment hashes the Pod template into the `pod-template-hash` label on each ReplicaSet. `deployment.kubernetes.io/revision` holds the revision number, and `revisionHistoryLimit` defaults to 10.
- StatefulSet stores each revision as a **ControllerRevision** object. Pods carry the `controller-revision-hash` label. Status has `currentRevision`, `updateRevision`, `currentReplicas`, `updatedReplicas` and `collisionCount` (https://kubernetes.io/docs/reference/kubernetes-api/workload-resources/stateful-set-v1/).
- The pattern: a content hash is the identity, and a monotonic number is the human alias.

**Storage outlives executors.**
- A PVC binds 1:1 to a PV through `claimRef`.
- PV phases are `Available`, `Bound`, `Released` and `Failed`. `persistentVolumeReclaimPolicy` is `Retain` or `Delete`.
- The `pvc-protection` finalizer blocks deleting a PVC while a Pod uses it (https://kubernetes.io/docs/concepts/storage/persistent-volumes/).
- StatefulSet `volumeClaimTemplates` produce stable claims named `<template>-<pod>`.
- `persistentVolumeClaimRetentionPolicy.{whenDeleted, whenScaled}` each default to `Retain`.
  - Choosing `Delete` adds an owner reference: to the StatefulSet for `whenDeleted`, or to the condemned Pod for `whenScaled`.
  - A node failure keeps the claim and reattaches it to the replacement Pod (statefulset.md).

**Leases.**
- `coordination.k8s.io/Lease` has `holderIdentity`, `leaseDurationSeconds` and `renewTime`, plus `acquireTime` and `leaseTransitions` **[prior]**.
- Kubelet heartbeats and leader election use Leases (https://kubernetes.io/docs/concepts/architecture/leases/).

| Entity | Lifecycle owner | References | Executor death | Floats free? |
|---|---|---|---|---|
| CronJob | user | — | — | — |
| Job | CronJob (ownerRef) or user | `ownerReferences` to the CronJob; Pods are selected by label plus ownerRef | Pod failure counts toward `backoffLimit` unless `podFailurePolicy` reclassifies it; a new Pod is created | no |
| Pod | Job, ReplicaSet or StatefulSet controller (ownerRef) | `spec.nodeName`; `volumes[].persistentVolumeClaim.claimName` | never moved; the replacement is a new uid | no |
| PVC | user; StatefulSet template; ownerRef only under the Delete retention policy | `spec.volumeName` ↔ PV `claimRef` | survives Pod and node loss; reattached to the replacement | **yes, by default** |
| ControllerRevision | StatefulSet | `controller-revision-hash` label on the Pods | — | history, bounded |
| Lease | the holder renews it | `holderIdentity` | expiry hands it over | — |

---

## 2. Argo Workflows

**Kinds and ownership.**
- Workflow is the live instance. WorkflowTemplate and ClusterWorkflowTemplate are definitions. CronWorkflow is a generator.
- **Templates do not own Workflows.** The controller copies resolved templates into `status.storedTemplates`. Maintainers declined to add an owner reference: the content is copied, so "there really is no 'dependency'" (https://github.com/argoproj/argo-workflows/issues/7192).
- **CronWorkflow does own its Workflows.** Its fields include `concurrencyPolicy`, `successfulJobsHistoryLimit`, `failedJobsHistoryLimit`, `when` and `stopStrategy`. Generated Workflows are named `<cron>-<random>` (https://argo-workflows.readthedocs.io/en/latest/cron-workflows/).
- Each step becomes a Pod owned by the Workflow, with `init`, `main` and `wait` containers (https://argo-workflows.readthedocs.io/en/latest/architecture/).
- **WorkflowTaskResult** carries outputs from the executor back to the controller. It is owned by the Workflow, not the Pod.
  - A Pod killed between the placeholder write and `FinalizeOutput` used to leave the Workflow Running forever. The fix marks the node failed when its Pod disappears (https://github.com/argoproj/argo-workflows/issues/12993).

**Status: the whole node tree is embedded in the Workflow.**
- `status.phase`, `status.nodes` (map from node ID to NodeStatus), `storedTemplates`, `persistentVolumeClaims`, `synchronization`, `conditions`.
- NodeStatus has `{id, type, phase, boundaryID, children, outboundNodes, templateRef, hostNodeName, outputs, memoizationStatus, synchronizationStatus, message, startedAt, finishedAt, progress}` (https://argo-workflows.readthedocs.io/en/latest/fields/).
- Node types: Pod, Container, Steps, StepGroup, DAG, TaskGroup, Retry, Skipped, Suspend, HTTP and Plugin.
- Node phases: Pending, Running, Succeeded, Skipped, Failed, Error and Omitted (workflow_types.go).
- **This embedding hit etcd's roughly 1 MB limit.** Argo first compressed the tree into `status.compressedNodes`, then offloaded it to SQL (`nodeStatusOffLoad`, with `offloadNodeStatusVersion` as a pointer) (https://argo-workflows.readthedocs.io/en/latest/offloading-large-workflows/).
- Completed Workflows can be archived to SQL (`archiveTTL`). Pod logs are not archived (https://argo-workflows.readthedocs.io/en/latest/workflow-archive/).

**Executors and volumes.**
- `podGC.strategy` is `OnPodCompletion`, `OnPodSuccess`, `OnWorkflowCompletion` or `OnWorkflowSuccess`. By default Pods are not deleted.
- `spec.volumeClaimTemplates` creates PVCs scoped to the Workflow.
- `volumeClaimGC.strategy` is `OnWorkflowCompletion` or `OnWorkflowSuccess`. **By default a claim is deleted only on success**, so a failed run keeps its workspace for inspection or resubmission (https://argo-workflows.readthedocs.io/en/latest/cost-optimisation/).
- An existing `persistentVolumeClaim.claimName` floats free of any Workflow (https://argo-workflows.readthedocs.io/en/latest/walk-through/volumes/).

**Executor death.**
- `retryStrategy` has `{limit, retryPolicy, backoff, affinity.nodeAntiAffinity, expression}`.
- `retryPolicy` values:
  - `OnFailure` (the default): the main container failed.
  - `OnError`: controller errors, or init or wait container failures.
  - `OnTransientError`.
  - `Always`.
- `expression` can read `lastRetry.exitCode`, `.status`, `.duration` and `.message` (https://argo-workflows.readthedocs.io/en/latest/retries/).
- A retry produces a `Retry` node with one child Pod node per attempt **[prior]**.
- The failure classes split the workload's own fault (OnFailure) from the infrastructure's (OnError).

**Control flow.**
- `suspend` templates and whole-workflow suspend/resume.
- `onExit` always runs; `{{workflow.status}}` is available to it.
- `synchronization` provides mutexes and ConfigMap-backed semaphores, at workflow or template level. Waiters queue by priority, and the locks can be database-backed across controllers (https://argo-workflows.readthedocs.io/en/latest/synchronization/).

**Deliverables.**
- `outputs.artifacts` are passed between steps by reference.
- `artifactGC.strategy` (`OnWorkflowCompletion`, `OnWorkflowDeletion` or `Never`) is guarded by the `workflows.argoproj.io/artifact-gc` finalizer (https://argo-workflows.readthedocs.io/en/latest/walk-through/artifacts/).
- `memoize{key, maxAge, cache.configMap}` caches template outputs across instances (https://argo-workflows.readthedocs.io/en/latest/memoization/).

| Entity | Lifecycle owner | References | Executor death | Floats free? |
|---|---|---|---|---|
| WorkflowTemplate | user | none from instances (copied into `storedTemplates`) | — | — |
| CronWorkflow | user | owns Workflows (ownerRef) | — | — |
| Workflow | user or CronWorkflow | node tree embedded in status | — | — |
| Pod (node) | Workflow | `status.nodes[id]` ↔ Pod name; `hostNodeName` | Error or Failed node → Retry node child | no |
| WorkflowTaskResult | Workflow | label `report-outputs-completed` | missing → node failed | no |
| PVC (volumeClaimTemplate) | Workflow | `status.persistentVolumeClaims` | survives Pod death; GC on success by default | outlives a failed instance |
| Artifact (repository) | artifact GC policy | `outputs.artifacts` key | survives | yes, by policy |

---

## 3. Tekton Pipelines

**Kinds and ownership.**
- Task and Pipeline are definitions. TaskRun, PipelineRun and CustomRun are instances.
- A PipelineRun owns its TaskRuns and CustomRuns through ownerReferences **and** the label `tekton.dev/pipelineRun`. A TaskRun owns its Pod (https://raw.githubusercontent.com/tektoncd/pipeline/main/docs/labels.md).
- **The parent rebuilds its state from its children.** When history pruning deleted the completed TaskRuns of a still-running PipelineRun, the controller treated those tasks as unscheduled and **ran them again** (https://github.com/tektoncd/pruner/pull/424).

**From embedded status to references (TEP-0100).**
- PipelineRun status used to embed full child statuses in `status.taskRuns` and `status.runs`.
- That rewrote the parent on every child change and more than doubled storage. Matrix fan-out would have made it worse.
- It was replaced by `status.childReferences[]` entries of `{apiVersion, kind, name, pipelineTaskName, whenExpressions}`.
- The migration ran through a feature flag (`embedded-status`: `full`, `both` or `minimal`). `minimal` became the default after about 9 months and the old fields were removed about 3 months later. The v1 API has only references (https://raw.githubusercontent.com/tektoncd/community/main/teps/0100-embedded-taskruns-and-runs-status-in-pipelineruns.md).

**PipelineRun** (https://tekton.dev/docs/pipelines/pipelineruns/)
- The `Succeeded` condition carries one of the reasons Started, Running, Succeeded, Completed, Failed, Cancelled or PipelineRunTimeout.
- `status.pipelineSpec` is a **snapshot of the exact spec** the run used.
- `timeouts.{pipeline, tasks, finally}`.
- `spec.status` is one of:
  - `Cancelled`
  - `CancelledRunFinally`
  - `StoppedRunFinally`: running tasks are allowed to finish before `finally` runs.
  - `PipelineRunPending`: hold before start.
- `finally` tasks always run after the main tasks and can read `$(tasks.status)`.
- `when` expressions skip a task and the tasks that consume its results (https://raw.githubusercontent.com/tektoncd/pipeline/main/docs/pipelines.md).

**TaskRun** (https://tekton.dev/docs/pipelines/taskruns/)
- Status has `podName`, `conditions` (reasons include `PodEvicted`, `TaskRunTimeout` and `TaskRunCancelled`), `results`, `retriesStatus` and `provenance`.
- On retry, the previous status is archived into `retriesStatus` and a new Pod is created. **Attempt history stays inside the one object.**
- Results go through the Pod termination message, capped at 4096 bytes in total, or through sidecar logs (https://raw.githubusercontent.com/tektoncd/pipeline/main/docs/tasks.md).

**Workspaces** (https://raw.githubusercontent.com/tektoncd/pipeline/main/docs/workspaces.md, https://raw.githubusercontent.com/tektoncd/pipeline/main/docs/affinityassistants.md)
- A `volumeClaimTemplate` creates one PVC per run. It is deleted when the run *object* is deleted, through the owner reference, not when the run completes. Since v1.11, `tekton.dev/auto-cleanup-pvc` deletes it on completion instead.
- A `persistentVolumeClaim` workspace binds an existing shared claim that floats free of runs.
- The **affinity assistant** is a placeholder StatefulSet Pod. It pins every TaskRun Pod that shares a ReadWriteOnce PVC to one node, either per workspace or per PipelineRun.
  - It is deleted when the run completes.
  - If its node is cordoned, a new placeholder is created elsewhere while running Pods finish where they are.
  - In effect, it is an explicit "home" object for co-located work.

**Deliverables and long-term records.**
- **Tekton Results** stores Results and Records outside the cluster, then annotates the source objects (`results.tekton.dev/record`) so they can be pruned (https://tekton.dev/docs/results/).
- **Tekton Chains** signs SLSA or in-toto provenance for completed runs. It finds artifacts through type-hinted results (`IMAGE_URL`, `IMAGE_DIGEST`, `ARTIFACT_OUTPUTS`) (https://tekton.dev/docs/chains/).
- Both make the record of what a run produced outlive the run.

| Entity | Lifecycle owner | References | Executor death | Floats free? |
|---|---|---|---|---|
| Pipeline / Task | user | snapshotted into run status | — | — |
| PipelineRun | user or trigger | `childReferences[]` (names only) | — | — |
| TaskRun / CustomRun | PipelineRun (ownerRef + label) | `podName` | `retriesStatus` archive, new Pod | no |
| Pod | TaskRun | — | `PodEvicted` reason | no |
| Workspace PVC (template) | run object (ownerRef) | workspace binding | survives Pod death | until the run is deleted or completes (by mode) |
| Affinity assistant | PipelineRun | node affinity | recreated on cordon | no; a home marker |
| Result Record / Chains attestation | Results / Chains | annotations on the run | — | **yes, outlives the run** |

---

## 4. Temporal

**Identity.**
- An execution is identified by Namespace + **Workflow Id** (a reusable business key) + **Run Id** (system-generated, one per run).
- Within a namespace, at most one execution with a given Workflow Id is open at a time (https://docs.temporal.io/workflow-execution/workflowid-runid).
- Docs: do not "rely on storing the current Run Id … for any logical choices".

**Statuses.**
- Open: Running, Paused.
- Closed: Completed, Failed, Cancelled, Terminated, Timed Out, Continued-As-New.
- Retries, Continue-As-New and Cron link runs into a **Workflow Execution Chain** under one Workflow Id (https://docs.temporal.io/workflow-execution).

**Id policies.**
- **Reuse Policy** applies when a *closed* run already has the Id: `AllowDuplicate`, `AllowDuplicateFailedOnly`, `RejectDuplicate` or `TerminateIfRunning`.
- **Conflict Policy** applies when an *open* run already has the Id: `Fail`, `UseExisting` (returns the existing Run Id, so the start is an idempotent "ensure") or `TerminateExisting`.

**History as state; Continue-As-New as generations.**
- The Event History is the state of record.
- Above 51,200 events, 10,000 Signals or 2,000 Updates, the server terminates the execution (https://docs.temporal.io/workflow-execution/event).
- **Continue-As-New** closes the current run and starts a new run with the **same Workflow Id**, a **new Run Id** and a fresh history. State is carried across explicitly as input arguments.
- Continue-As-New is also how a long-lived workflow picks up new code. The SDK raises "Continue-As-New Suggested" when history grows (https://docs.temporal.io/workflow-execution/continue-as-new).

**Activities and executor death** (https://docs.temporal.io/encyclopedia/detecting-activity-failures)
- Activity timeouts:
  - `Schedule-To-Start`: not retryable; detects a dead or undersized fleet.
  - `Start-To-Close`: one attempt.
  - `Schedule-To-Close`: all attempts together.
  - `Heartbeat`: the maximum gap between heartbeats.
- "The Temporal Server doesn't detect failures when a Worker … crashes." Timeouts drive the retry.
- Heartbeat **details** checkpoint progress, and the next attempt can read them.
- Activities retry by default; Workflows do not (https://docs.temporal.io/retry-policies).

**Workers are stateless pools** (https://docs.temporal.io/workers, https://docs.temporal.io/task-queue, https://docs.temporal.io/sticky-execution)
- Workers long-poll task queues and pull work when they have capacity.
- Any Workflow can be evicted from a worker and replayed elsewhere.
- **Sticky execution** is purely a cache. After 5 s the server falls back to the shared queue and any worker can replay.
- **Host or file affinity is explicit**, and comes in two forms:
  - A worker-specific task queue, returned by the first activity (https://docs.temporal.io/task-routing).
  - **Worker Sessions** (Go SDK; https://docs.temporal.io/develop/go/workers/sessions). `CreateSession` pins later activities to one worker, recorded as a `SessionID` and `HostName`.
- **If the session's host dies, new activities fail with `ErrSessionFailed`, and the workflow must call `RecreateSession`.** Affinity loss is surfaced to the orchestrator; it is never silently healed.

**Versioning** (https://docs.temporal.io/worker-versioning)
- A Worker Deployment Version is identified by deployment name + Build ID.
- A workflow type is either **Pinned** (completes on one version) or **Auto-Upgrade**.
- Version states: Inactive → Active (Current or Ramping) → Draining (pinned runs still open) → Drained.
- The old executor fleet must outlive its pinned runs.

**Parent/child and generators.**
- `ParentClosePolicy` is `Terminate` (default), `RequestCancel` or `Abandon`, chosen per child at start (https://docs.temporal.io/parent-close-policy).
- **Schedules** generate executions. The Workflow Id gets the action timestamp appended. The overlap policy is `Skip`, `BufferOne`, `BufferAll`, `CancelOther`, `TerminateOther` or `AllowAll` (https://docs.temporal.io/schedule).
- Messages (https://docs.temporal.io/sending-messages):
  - Signals: asynchronous.
  - Updates: synchronous, with validators. Validators are an application-level landing gate.
  - Queries: read-only, and work on closed runs too.

| Entity | Lifecycle owner | References | Executor death | Floats free? |
|---|---|---|---|---|
| Workflow Execution (Id) | server | Workflow Id across runs | replayed on any worker | — |
| Run | server | Run Id; chain link | — | — |
| Activity task | server (timeouts, retries) | scheduled on a task queue | retried after Start-To-Close or Heartbeat timeout, resuming from heartbeat details | — |
| Worker | operator | polls task queues; Build ID | nothing durable lost | **yes, always** |
| Session | workflow | `SessionID`, `HostName` | `ErrSessionFailed`, explicit `RecreateSession` | no; bound to the workflow |
| Schedule | user | generates runs | — | — |
| Child workflow | parent | `ParentClosePolicy` | — | `Abandon` lets it outlive the parent |

---

## 5. GitHub Actions, Buildkite and Nix

### GitHub Actions
- **Workflow run** fields: `id`, `run_number`, `run_attempt`, `head_sha`, `status`, `conclusion`, `previous_attempt_url`.
  - **A re-run keeps the run `id` and increments `run_attempt`.** It is an attempt counter on one entity (https://docs.github.com/en/rest/actions/workflow-runs).
- **Job** fields: `{id, run_id, run_attempt, status, conclusion, steps[], runner_id, runner_name, runner_group_id, labels}`. `runner_id` stays null until a runner is assigned (https://docs.github.com/en/rest/actions/workflow-jobs).
- **Runners** match on all of a job's `runs-on` labels.
  - `--ephemeral` and JIT runners take one job, then deregister.
  - Persistent runners float free of runs (https://docs.github.com/en/actions/hosting-your-own-runners/managing-self-hosted-runners/autoscaling-with-self-hosted-runners).
  - With ARC runner scale sets, a listener Pod watches for jobs and ephemeral runner Pods take one job each. **`minRunners` is the warm idle pool**, added on top of the assigned jobs (ARC deployment docs).
- **Runner death.**
  - The job fails with "The self-hosted runner lost communication with the server".
  - There is no automatic retry. Recovery is a re-run, which becomes a new attempt.
  - **[uncertain]** These semantics come from issue threads (actions/runner#3862, ARC#3519), not docs.
- **Landing authority** is layered outside the run:
  - **Environments** with deployment protection rules: required reviewers, wait timers, branch rules and custom app rules. Environment secrets are withheld until the rules pass (https://docs.github.com/en/actions/managing-workflow-runs-and-deployments/managing-deployments/managing-environments-for-deployment).
  - The **merge queue** requires workflows to trigger on `merge_group`. It builds `gh-readonly-queue/{base}` branches that include every queued PR ahead. A PR that fails is removed and the queue behind it is regrouped (https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/configuring-pull-request-merges/managing-a-merge-queue).
- **Concurrency groups** allow at most one running and one pending member per group, with optional `cancel-in-progress` (https://docs.github.com/en/actions/writing-workflows/choosing-what-your-workflow-does/control-the-concurrency-of-workflows-and-jobs).
- **Caches** are immutable by key, with 7-day idle eviction.
- Only caches and artifacts carry state across attempts.

### Buildkite
- **Build** fields: `{id, number, state, jobs[], meta_data, rebuilt_from}`. **A rebuild is a new build that links back through `rebuilt_from`** (https://buildkite.com/docs/apis/rest-api/builds).
- **Job states** (https://buildkite.com/docs/pipelines/configure/defining-steps):
  - Gating: `pending`, `waiting`, `blocked`, `unblocked`, `limiting`, `limited`.
  - Dispatch: `scheduled` → `assigned` → `accepted` → `running`.
  - Terminal: `finished`, `canceled`, `timed_out`, `expired`, `skipped`, `broken`.
- **Agents** are pull-based and heartbeat (https://buildkite.com/docs/agent/lifecycle).
  - After 3 minutes without a heartbeat, an agent is marked lost and its job gets exit status `-1`.
  - `SIGTERM` drains the current job; `SIGQUIT` cancels it.
- **Automatic retry** is declared on the step and matches on `exit_status`, including `-1`, and on `signal_reason`: `agent_stop`, `agent_refused`, `cancel` and others.
  - A retry creates a **new job**; the old one is linked through `retried_in_job_id` (https://buildkite.com/docs/pipelines/configure/retry).
  - Infrastructure loss is a first-class retry class.
- **Pools.**
  - Clusters contain queues, and a queue can be paused.
  - The Elastic CI Stack `MinSize` sets the warm floor.
  - `--disconnect-after-job` gives ephemeral agents. `--acquire-job <id>` lets an external scheduler bind one job to one agent (https://buildkite.com/docs/agent/v3/cli-start).
- **Block step** is the manual landing gate, with `allowed_teams` and `fields`. The values go to build meta-data, which is last-write-wins (https://buildkite.com/docs/pipelines/configure/step-types/block-step).
- **`pipeline upload`** lets a running job append steps, which makes the DAG dynamic and agent-authored (https://buildkite.com/docs/pipelines/configure/dynamic-pipelines).

### Nix and Hydra
- A **derivation** is a canonical, serialised build spec. Its store path `/nix/store/<digest>-<name>` is "an opaque, unique identifier" (https://nix.dev/manual/nix/latest/store/store-path).
- Same inputs give the same path, so rebuilding is idempotent and caching is free.
- Floating content-addressed outputs need a **realisation** record that maps `drv^out` to the output path (https://nix.dev/manual/nix/latest/store/derivation/outputs/).
- **Remote builders** are an interchangeable pool matched by platform and features, and hold no per-build state (https://nix.dev/manual/nix/latest/advanced-topics/distributed-builds).
- **Profiles** are numbered generation links (`default-42-link`) to store paths. Switching is an atomic symlink flip, which makes rollback trivial (https://nix.dev/manual/nix/latest/package-management/profiles).
  - This is the cleanest example of the two-layer identity pattern: a monotonic human generation number over a content-hash identity.
- **Hydra** chains Project → Jobset → Evaluation → Build. **[uncertain]** These definitions come from the community wiki, because the manual pages returned 503 (https://wiki.nixos.org/wiki/Hydra).

| System | Instance identity vs attempt | Executor death | Executors vs instance | Landing |
|---|---|---|---|---|
| GHA | run `id` + `run_attempt` | job fails; manual re-run = new attempt | pool, or ephemeral per job; ARC `minRunners` warm | environments, merge queue, required checks |
| Buildkite | build `number`; `rebuilt_from`; job `retried_in_job_id` | exit `-1`; declarative auto-retry = new job | pool per queue tag; ephemeral; `--acquire-job` | block step |
| Nix | `.drv` hash / output path; profile gen N | rebuild is idempotent | interchangeable builders | profile symlink flip |

---

## 6. Agent platforms and sandbox providers

### 6.1 Claude Managed Agents (beta `managed-agents-2026-04-01`)

The design write-up is Anthropic's "Scaling Managed Agents: Decoupling the brain from the hands" (https://www.anthropic.com/engineering/managed-agents). It splits the system into three parts:

- **Brain** = Claude plus the harness. The harness is **stateless**. After a crash a new one is started with `wake(sessionId)`, reads the log with `getSession(id)`, and resumes from the last event. All progress goes through `emitEvent`.
- **Hands** = sandboxes and tools, reached through `execute(name, input) → string`. **A container death surfaces as a tool-call error.** Claude decides whether to retry, and a new container can be initialised.
- **Session** = an append-only event log held outside both.

Other points from the post:
- Containers are provisioned lazily, "only if they are needed". This cut p50 time-to-first-token by about 60%.
- Credentials stay out of the sandbox. Git tokens are bundled at sandbox initialisation, and MCP OAuth tokens live in a vault behind a session-scoped proxy.
- "Brains can pass hands to one another." A brain may reason over several environments.

Resources (https://platform.claude.com/docs/en/managed-agents/overview, …/sessions, …/environments, …/session-operations):

| Entity | Created by | Destroyed by | States | References |
|---|---|---|---|---|
| Agent | `POST /v1/agents` | archive or delete | **versioned**; a session pins `{id, version}` or takes the latest | — |
| Environment (`cloud` or `self_hosted`) | `POST /v1/environments` | archive (existing sessions keep running) or delete (only when unreferenced) | **not versioned** | — |
| Session `sesn_…` | `POST /v1/sessions {agent, environment_id, resources[], vault_ids[], initial_events[], budget, metadata}` | archive (history kept) or delete (removes events **and the sandbox**) | `idle`, `running`, `rescheduling` (transient error, retrying), `terminated`. Finishing work goes to `idle`, not `terminated` | agent (and version), environment, resources, vaults |
| Session resource `sesrsc_…` (`github_repository`, `file`, `memory_store`) | session create | lives with the session | "to change which repositories are mounted, create a new session"; the token can be rotated | session |
| Sandbox | platform, one per session | platform, on session delete | not exposed in the API; "sessions do not share filesystem state" | implicit |

Sandbox provisioning and warmth:
- The docs say provisioning starts when the session is created. The engineering post says it is lazy. **[uncertain]** which describes the current behaviour.
- `packages` and repo clones are **cached per environment** across sessions. This is the only warm path documented; there is no explicit pool.

**Self-hosted environments** are the purest loop-outside design (https://platform.claude.com/docs/en/managed-agents/self-hosted-sandboxes, …-workers).
- The environment acts as a **work queue**.
- A customer-run worker dials out over HTTPS, claims a work item, and runs each tool call locally. "Claude runs on Anthropic's side."
- With no worker connected, the session stays queued rather than failing.
- Each item carries a per-session secret.
- A `reclaim_older_than_ms` option suggests claims are leased. **[uncertain]** what exactly happens when a worker dies.
- Workers can spawn one sandbox per session (`--on-work ./spawn.sh`).
- Repositories are staged by the customer: self-hosted environments reject `github_repository` resources.

**Claude Code on the web** is the contrast case (https://code.claude.com/docs/en/claude-code-on-the-web).
- The CLI harness runs **inside** the VM, so the loop is inside.
- Credentials still stay outside. GitHub credentials "never enter a session's VM" and go through a proxy.
- An idle VM is reclaimed. Reopening restores the conversation onto a fresh VM, but not running processes.
- The deliverable is a pushed branch and a PR, with auto-fix watching CI and reviews. `--teleport` pulls the branch and conversation to a local machine.

### 6.2 OpenHands

**V0** (https://docs.openhands.dev/usage/architecture/runtime)
- The agent controller runs **outside** a Docker sandbox. Inside, an ActionExecutor server receives Actions over the EventStream and returns Observations.
- Runtime images use three tags: versioned, lock (dependency hash) and source (source-tree hash).

**The OpenHands SDK paper** (https://arxiv.org/html/2511.03690v1) records why they moved away from that split: the sandbox "might crash while the agent continued (or vice versa), leading to corrupted sessions."

**V1** makes **ConversationState** the only stateful component.
- It is event-sourced: `base_state.json` plus one JSON file per event, and resume replays the events.
- Workspace types are `LocalWorkspace`, `RemoteWorkspace`, `DockerWorkspace` and `APIRemoteWorkspace` (https://docs.openhands.dev/sdk/arch/workspace).
- With a remote workspace, the **whole loop runs inside the container's agent-server** (https://github.com/OpenHands/software-agent-sdk/issues/4187).
  - That issue proposed server-owned conversations with execution-only workspaces, because LLM keys were exposed inside the sandbox.
  - It was auto-closed for inactivity.

**OpenHands Cloud** (https://docs.openhands.dev/enterprise/conversations-and-sandboxes)
- **Conversation and Sandbox are separate entities**, and several conversations may share one sandbox.
- A "Sandbox Grouping Strategy" setting chooses where new conversations go: none, newest, LRU, fewest conversations, or any.
- Lifecycle: idle → paused (resumes automatically) → deleted after a configured paused time. A running session is capped at 12 h.
- Transcripts persist. An archive of workspace contents is kept.
- **Warm pools** are explicit: `POST /api/v1/sandboxes`, wait for RUNNING, then attach conversations. `/pause` and `/resume` are also available.

**A failure mode worth noting** (https://github.com/OpenHands/software-agent-sdk/issues/4893)
- After a pause, the conversation stays at `execution_status=RUNNING` and `run()` returns 409.
- The issue's explanation: "a paused VM freezes its own clock, so the lease never expires … the dead owner is never fenced."
- **A lease kept inside the sandbox cannot fence the sandbox.**

### 6.3 OpenAI

**Codex cloud** (https://learn.chatgpt.com/docs/environments/cloud-environment; the page is now labelled "Legacy")
- A task creates a container, checks out the repo, runs the setup script, applies internet settings, then runs the agent loop.
- **Container state is cached for up to 12 h.** A resumed container runs the maintenance script. The cache is invalidated when scripts or secrets change.
- **Secrets exist only during setup** and are removed before the agent phase.
- Best-of-N runs through `--attempts 1..4`.
- The deliverable is a diff, which the user turns into a PR or commit.
- **[uncertain]** where the loop runs relative to the container.

**Agents SDK Sandbox Agents** (https://openai.github.io/openai-agents-python/sandbox/guide/, https://developers.openai.com/api/docs/guides/agents/sandboxes)
- The harness owns "the agent loop, model calls, tool routing, handoffs, approvals, tracing, recovery, and run state", which keeps control-plane work in trusted infrastructure.
- Main types: `SandboxAgent`, `Manifest` (the contract for a fresh workspace), `SandboxRunConfig` and `RunState`.
- "The provider is part of the run configuration, not the agent definition." Clients include Docker, Daytona, E2B, Modal, Cloudflare, Vercel, Runloop and Blaxel.
- **Session-source precedence**: live session → `RunState` sandbox state → explicit `session_state` (reconnect) → fresh session from a snapshot or manifest.
- **Ownership is explicit:**
  - SDK-owned: the runner creates, snapshots and cleans up.
  - Developer-owned: you pass `session=`, which allows reuse across runs.
- Serialized state strips mount credentials, and "serialized state never grants authority by itself."

**Responses API containers** (https://developers.openai.com/api/reference/resources/containers/methods/create)
- Fields: `status`, `expires_after{anchor: last_active_at, minutes}`, `memory_limit`, `network_policy`.
- **[uncertain]** 20-minute idle expiry.

### 6.4 Sandbox providers

| Provider | States | Reattach | Persistence / warm | Limits |
|---|---|---|---|---|
| **E2B** (https://docs.e2b.dev/sandbox, …/persistence, …/snapshots) | Running ⇄ Paused → Killed; transient Snapshotting | `connect(id)` auto-resumes, so the orchestrator must keep the id | `pause()` keeps filesystem **and memory**; paused sandboxes are kept indefinitely; `lifecycle.onTimeout: 'pause'`; `createSnapshot()` → many sandboxes; templates snapshot a running start command | 24 h continuous (Pro) |
| **Modal** (https://modal.com/docs/guide/sandbox, …/sandbox-snapshots) | Created → Scheduled → Started → Ready → Finished | `Sandbox.from_id`, `from_name` (one running per name), tags plus `list(tags=)`; `detach()` drops the handle and leaves the sandbox running | filesystem snapshot → Image (30-day TTL, forkable); directory snapshots; experimental memory snapshots (7-day) | `timeout` default 5 min, max 24 h; `idle_timeout` |
| **Daytona** (https://www.daytona.io/docs/en/sandboxes) | running, paused, stopped, archived, deleted (plus recover) | by id; labels | auto-stop after 15 min idle; auto-archive after 7 days stopped; auto-delete off by default; `ephemeral` | **[uncertain]** defaults came from SDK snippets |

### 6.5 Devin, Cursor and Copilot

**Devin.**
- **Sessions API v3** (https://docs.devin.ai/api-reference/v3/sessions/post-organizations-sessions):
  - `status`: new, claimed, running, exit, error, suspended, resuming.
  - `status_detail`: working, waiting_for_user, waiting_for_approval, finished; when suspended, inactivity, user_request, out_of_credits.
  - `resumable` (default true) preserves VM state.
  - The response carries `pull_requests[{pr_url, pr_state}]`.
- **Machine snapshots** are built from a blueprint, and every session boots from one. "Session changes don't persist back to the snapshot" (https://docs.devin.ai/onboard-devin/environment).
- **Outposts** run the brain in Devin's cloud while the customer's orchestrator claims sessions atomically and provisions a VM, where `devin worker start` dials out (https://docs.devin.ai/cloud/outposts/orchestration).
  - "If a worker dies after claiming, its claim expires at the claim deadline and the session returns to the queue."
  - Resumable outposts snapshot the machine before termination.

**Cursor Cloud Agents** (https://cursor.com/docs/cloud-agent/api/endpoints, …/self-hosted)
- **Agent** (ACTIVE, IDLE, ARCHIVED) and **Run** (CREATING, RUNNING, FINISHED, ERROR, CANCELLED, EXPIRED) are separate.
- A follow-up is a new Run on the same Agent.
- Self-hosted: "Cursor runs the agent loop … your worker performs file edits and terminal commands." Workers connect outbound and run one agent per machine.

**GitHub Copilot cloud agent** (https://docs.github.com/en/copilot/concepts/agents/cloud-agent/risks-and-mitigations)
- Runs in an ephemeral environment powered by GitHub Actions. Sessions are capped at 59 minutes.
- Each task gets one branch (`copilot/…`) and one PR.
- **Landing has separation of duties**: the requester cannot approve, and workflows need "Approve and run workflows".

---

## 7. Cross-system comparison matrix

Rows are the six concerns flotilla bundles. Cells say which entity carries that concern and how it relates to the instance.

| Concern | Kubernetes | Argo | Tekton | Temporal | GHA / Buildkite | Managed Agents | OpenHands Cloud | Devin / Cursor | **Flotilla today** |
|---|---|---|---|---|---|---|---|---|---|
| **A. Vessel home** (environment owner) | PVC / StatefulSet claims are separate objects; ownerRef optional (Retain by default) | Workflow-scoped PVC (deleted on success only) or an external PVC | per-run PVC via ownerRef, or a shared PVC; affinity assistant as the "home" Pod | none in the server; a Worker Session is workflow-held affinity that fails loudly | runner pool / ephemeral runner; no workspace survives the job except caches and artifacts | Environment (config, not versioned) + implicit per-session sandbox, deleted with the session | **Sandbox is its own entity; N conversations per sandbox; grouping strategy** | snapshot (image) + per-session VM; claimed by an external orchestrator | Convoy owns Vessels (cascade + finalizer) |
| **B. Workflow instance** | Job (or Deployment rollout) | Workflow + embedded node tree; template copied into `storedTemplates` | PipelineRun + `pipelineSpec` snapshot; children by reference | Workflow Execution (Id) / Run (Run Id) | run / build | Session (event log); Agent version pinned | Conversation (event-sourced) | Session / Run | Convoy + frozen `workflow-snapshot` |
| **C. Deliverables checklist** | none (completions counter, `completedIndexes`) | `outputs.artifacts`, memoization cache | `results`; Results/Chains records outlive the run | workflow return value / application state | artifacts, deployments (statuses), check runs | session files; PR via MCP | workspace archive; PR | `pull_requests[]` on the session; `repos[].prUrl` | `status.promises` + `subjects` inside Convoy |
| **D. Landing authority** | `successPolicy` / `podFailurePolicy` (controller); decision vs confirmed terminal split | controller + `onExit`; suspend for humans | `finally`, `when`; `StoppedRunFinally` | application code; Update validators | environments + reviewers, merge queue, required checks, block step: **outside the run** | customer merges the PR | human merges | human merges; Copilot: separation of duties | Exit Table + Settle edge in Convoy reconciler |
| **E. Generation identity** | `generation` / `observedGeneration`; ControllerRevision hash + number; CronJob time-suffixed Job names | CronWorkflow → `<cron>-<random>` | — | **Workflow Id stable, Run Id per run; Continue-As-New; Id reuse/conflict policies** | `run_attempt` (same entity) vs `rebuilt_from` (new entity) | Agent version; new session per run | — | Cursor: Agent + Runs | one Convoy record per generation; labels `{project, role, generation}` |
| **F. Crew-work state** | per-Pod status; Job counters only | `status.nodes` embedded (hit the 1 MB limit → compress → offload) | **`childReferences` only (moved off embedding, TEP-0100)** | activity state in history; heartbeat details | per-job status | session event log; `status` per session | ConversationState per conversation | session `status_detail` | `crew_work` map inside Convoy status |

What stands out:
- **No surveyed system keeps the deliverables checklist or landing authority inside the same record as executor ownership and per-worker state.**
  - Landing is either outside the run (GHA environments and merge queue, Buildkite block steps, a human merging the PR) or a separately named controller decision with its own condition (Kubernetes `SuccessCriteriaMet`).
- **Both Kubernetes-style systems that embedded child status in the parent paid for it.** Argo had to compress and offload; Tekton migrated to references over about a year.
- **Generation identity is almost always two-layered**: a stable logical key (Workflow Id, Role Address) plus a per-incarnation id (Run Id, uid, Nix store hash).
  - The stable key never stores the current per-incarnation id as truth (Temporal's explicit warning).

---

## 8. Agent loop outside the sandbox

### 8.1 Placement

| Loop outside the sandbox | Loop inside the sandbox |
|---|---|
| Claude Managed Agents (cloud and self-hosted workers) | OpenHands V1 remote (agent-server in the container) |
| OpenAI Agents SDK Sandbox Agents | Claude Code on the web (CLI in the VM; credentials proxied outside) |
| Devin Outposts | Copilot cloud agent, Codex cloud **[uncertain]** |
| Cursor self-hosted workers | |
| OpenHands V0 (controller outside) | |

### 8.2 What each side holds

| Item | Loop-outside designs (Managed Agents, OpenAI SDK, Outposts, Cursor self-hosted) | Loop-inside designs |
|---|---|---|
| Conversation / event log | control plane (session log, `RunState`) | inside, then persisted out (OpenHands V1 `base_state.json`, CC-web conversation restore) |
| Model credentials | control plane only | inside the sandbox (OpenHands V1; flagged as a risk in #4187) |
| Forge / MCP credentials | vault + proxy; never in the sandbox (Managed Agents, CC-web), or present only during setup (Codex) | proxied (CC-web) or present |
| Files, git working tree, processes | sandbox | sandbox |
| Budget, approvals, tracing | control plane | mixed |
| Lease / claim state | control plane (claim deadline) | **inside: fails to fence a paused VM** (OpenHands #4893) |

### 8.3 Attach, detach and death

- **Attach.**
  - Lazy, at the first tool call (the Managed Agents post).
  - Eager, at session create (Managed Agents docs).
  - Reconnect by id: E2B `connect(id)`, Modal `from_id`/`from_name`, OpenAI `session_state`.
  - Resolved by a precedence chain: OpenAI SDK, live → saved state → reconnect → fresh from snapshot or manifest.
- **Detach.**
  - Modal `detach()` drops the client handle and leaves the sandbox running.
  - OpenHands workspace context exit "disconnects and optionally stops".
  - OpenAI's developer-owned sessions survive the run.
- **Self-hosted claim protocol.** Anthropic, Devin and Cursor converge on the same shape:
  - A session is queued.
  - A worker dials out and claims it, with a lease or deadline.
  - The worker provisions one sandbox per session.
  - The control plane pushes tool calls and receives results.
  - A dead worker's claim expires and the session is re-queued (Devin says this explicitly; Anthropic's `reclaim_older_than_ms` suggests the same).
- **Sandbox death mid-task.**
  - Managed Agents surfaces it to the *model* as a tool error, and the model decides whether to retry. The session itself goes to `rescheduling` only for platform transients.
  - CC-web restores the conversation onto a fresh VM but loses running processes.
  - Temporal's analogue is `ErrSessionFailed`, which forces `RecreateSession`.
  - In every case, **affinity loss is surfaced, not silently healed**.

### 8.4 Pooling, warmth and floating sandboxes

- **Explicit pre-created pool, attached later**: OpenHands `POST /api/v1/sandboxes`, GHA ARC `minRunners`, Buildkite Elastic `MinSize`.
- **Warm by image or snapshot rather than by a live process**:
  - Devin machine snapshots, where session changes never flow back.
  - E2B templates that snapshot a running start command.
  - Modal and Daytona snapshots.
  - The Managed Agents per-environment package and repo cache.
  - Codex's 12 h container cache.
- **Paused, not destroyed**: E2B pause keeps memory and is retained indefinitely. Daytona has stopped → archived tiers. OpenHands Cloud pauses idle sandboxes, then deletes them.
  - This is the same depth spectrum ADR 0028 describes (warm → suspended → none).
- **Shared sandbox for several conversations**: only OpenHands Cloud, through its grouping strategy. The cost is a shared filesystem, shared credentials and a shared failure domain.
- **Free-floating sandboxes** with no owning run: Modal named sandboxes, E2B paused sandboxes, OpenAI developer-owned sessions, and K8s/Tekton/Argo shared PVCs.
  - In each case **something other than the run must own deletion**: TTL, idle policy or explicit kill.

---

## 9. Decomposition patterns and trade-offs

**P1. Definition / instance / attempt, three levels.**
- Shapes: Template → Workflow → Pod (Argo); Pipeline → PipelineRun → TaskRun + `retriesStatus` (Tekton); Agent version → Session (Managed Agents); Agent → Run (Cursor).
- Definitions never own instances (Argo #7192) and are snapshotted into the instance (`storedTemplates`, `pipelineSpec`).
- For: definition edits cannot change a live run.
- Against: two ways to model attempts.
  - Attempts inside one object: Tekton `retriesStatus`, GHA `run_attempt`.
  - Attempts as sibling objects linked back: Buildkite `retried_in_job_id` and `rebuilt_from`, Argo Retry node children.
  - Inside is simpler to query. Siblings keep each attempt's own lifecycle checkable.

**P2. Generator owns instances; instance identity is two-layered.**
- Shapes: CronJob → Job (time-suffixed names, `concurrencyPolicy`, history limits); CronWorkflow; Temporal Schedule → executions (overlap policy); Workflow Id + Run Id, with Continue-As-New.
- For:
  - "At most one live" becomes a declared policy: `Forbid`/`Replace`, or Temporal's `UseExisting`/`TerminateExisting`/`Fail`.
  - History retention is a generator setting.
- Against:
  - Carrying state between generations must be explicit (Continue-As-New arguments).
  - Implicit carry-over is the source of resurrection bugs.
- Temporal's `UseExisting` conflict policy is the idempotent-ensure primitive. `AllowDuplicateFailedOnly` encodes "a new generation only after failure".

**P3. Executor ownership separate from the workspace that outlives it.**
- Shapes:
  - Pod ≠ PVC. StatefulSet retention defaults to Retain.
  - Argo deletes volumes on success only.
  - Tekton per-run PVCs, or shared PVCs.
  - OpenHands Cloud Sandbox ≠ Conversation.
  - Managed Agents Environment (config) ≠ per-session sandbox.
- For:
  - Executor death does not lose work.
  - Workspaces can be reused or inspected after failure.
- Against:
  - Something must own deletion of the free workspace: TTL, idle, retention policy or a finalizer.
  - Leaks are the usual failure: flotilla's own #1894 and ADR 0021's disk exhaustion.
  - Sharing a workspace across instances couples their failure domains.

**P4. Explicit affinity that fails loudly.**
- Shapes: Temporal Worker Sessions (`ErrSessionFailed` → `RecreateSession`); Tekton affinity assistant; Kubernetes "Pods are never rescheduled".
- For: no silent migration of work that depends on local state.
- Against: the orchestrator needs a recreate path and a source of durable inputs. Compare flotilla's Succession rule: re-provision from the pushed branch plus the Brief.

**P5. Child status by reference, not embedded.**
- Shapes: Tekton TEP-0100 `childReferences`; Argo's forced offload; Kubernetes Job counters.
- For:
  - The parent is not rewritten on every child change.
  - Each child carries its own lifecycle.
  - The size limit does not hit as the number of children grows.
- Against:
  - Readers must join.
  - **A parent that rebuilds state from children redoes work when children are pruned** (Tekton pruner #424). History retention then becomes a correctness concern.
  - Tekton needed a three-stage flag migration (`full` → `both` → `minimal`) over about a year.

**P6. Decision separate from confirmed terminal.**
- Shapes: Kubernetes Job `SuccessCriteriaMet` or `FailureTarget`, then `Complete` or `Failed` after Pods terminate; Tekton `StoppedRunFinally`; Buildkite `canceling`/`timing_out`.
- For:
  - Teardown hangs off the confirmed state.
  - Consumers can tell "decided" from "done".
- Against: one more state per terminal edge.

**P7. Failure classification before retry budget.**
- Shapes: `podFailurePolicy` Ignore on `DisruptionTarget`; Argo `OnError` vs `OnFailure`; Buildkite exit `-1` and `signal_reason: agent_stop`; Temporal non-retryable `Schedule-To-Start`.
- For: infrastructure loss does not consume the workload's failure budget, and does not turn into a workload "Failed".
- Compare ADR 0017's `failure_source: Bootstrap | Crew | Steward`, which the survey finds absent in code.

**P8. Landing authority as a separate gate.**
- Shapes: GHA environments with required reviewers and withheld secrets; the merge queue (`merge_group`); Buildkite block steps with `allowed_teams`; Copilot separation of duties; Temporal Update validators.
- For:
  - Landing policy can change without touching run state.
  - Several runs can contend for one landing queue (merge queue).
  - Authority is auditable apart from execution.
- Against: the run must observe the gate's outcome, which adds a cross-entity wait.

**P9. Durable log in the control plane; disposable hands.**
- Shapes: the Managed Agents brain/hands/session split; OpenAI `RunState`; OpenHands V1 ConversationState; Temporal Event History.
- For:
  - Any executor can be replaced.
  - Credentials stay out of the sandbox.
  - One brain can drive several sandboxes, and hands can be passed between brains.
- Against:
  - Every tool call is a round trip.
  - The process state inside the sandbox is lost on death.
  - OpenHands V0 → V1 shows that **two independently stateful halves corrupt sessions**. The split works only when exactly one side is stateful.

**P10. Claim/lease queue for executor binding.**
- Shapes: Managed Agents self-hosted workers; Devin Outposts (claim deadline, re-queue); Buildkite `scheduled → assigned → accepted`; GHA JIT runners; Kubernetes Leases.
- For:
  - Executor death is detected by lease expiry in the control plane.
  - A worker can bind work wherever it runs.
- Against: fencing must use the control plane's clock (OpenHands #4893).

**P11. Content-hash identity with a human generation number on top.**
- Shapes: Nix profile generations over store paths; StatefulSet ControllerRevision hash + revision; Deployment `pod-template-hash` + revision.
- For:
  - Dedup and caching come for free.
  - Rollback is a pointer flip.
- Against: needs a separate mutable pointer (profile link, `currentRevision`) whose transitions must themselves be modelled.

---

## 10. Questions for the grill

1. **Which of the six concerns has its own record, and which is a projection?**
   - Every surveyed system separates at least executor ownership (A) from instance (B).
   - None stores landing (D) and per-worker state (F) in the executor owner's record.
   - Which separations does flotilla want, and in which direction does each reference point (ownerRef vs label vs name)?
2. **Does the vessel home own vessels, or merely reference them?**
   - If vessels can be free-floating, warm-kept or shared (as in OpenHands Cloud), what owns deletion: TTL, idle policy, retention policy like `whenDeleted`/`whenScaled`, or explicit reap?
   - What is the vessel's default retention on success versus failure? Argo keeps a failed run's workspace and deletes on success.
3. **Is affinity loss surfaced or healed?**
   - When a vessel dies, does the crew get the Temporal `ErrSessionFailed` analogue (orchestrator must recreate) or the Managed Agents analogue (tool error, model decides)?
   - Who may recreate it, and from which durable inputs?
4. **Generation identity: a stable key plus per-incarnation id, with which conflict policy?**
   - Should ensure semantics map onto the Temporal Id policies: `UseExisting` vs `TerminateExisting` vs `AllowDuplicateFailedOnly`?
   - What, if anything, is explicitly carried across generations (Continue-As-New arguments)?
   - Today's code replaces vessels within one generation (the I1 invented semantics). Is that an attempt within a generation (GHA `run_attempt`) or a new generation?
5. **Attempts inside or beside?**
   - When a vessel or crew session is retried, does the history live in one object (`retriesStatus`) or in sibling records linked back (`retried_in_job_id`)?
   - Which makes the machine table checkable?
6. **Embedded or referenced crew-work state?**
   - If crew work stays a map in the instance record, how does flotilla avoid Argo's size problem and Tekton's rewrite-the-parent-on-every-child-change cost?
   - If it becomes a resource, does the parent rebuild from children? If so, how is pruning prevented from causing re-execution (Tekton #424)?
7. **Is the deliverables checklist a property of the instance, of the crew member, or a standalone record that outlives both?**
   - Tekton Results and Chains outlive runs. Devin reports PRs on the session.
   - ADR 0061 already notes that "a separate Promise resource kind would be more normalised."
8. **Where does landing authority live, and is it a gate the instance waits on (merge queue, environment) or a terminal edge the instance's controller writes?**
   - Can two instances share one landing gate?
   - Is "decided" separate from "confirmed terminal and teardown-safe" (Kubernetes `SuccessCriteriaMet` → `Complete`)?
9. **Failure classification:** which failure classes (infrastructure loss, crew testimony, steward, bootstrap) consume which budget, and which may reprovision? Compare `podFailurePolicy` and Buildkite's `signal_reason`.
10. **Agent loop placement: which side is stateful?**
    - If a crew's loop runs outside its vessel(s), is the AgentSession log the single stateful component, as in OpenHands V1 and the Managed Agents session?
    - Which credentials stay with the loop and which are staged into the vessel?
    - How are hands passed between brains (Managed Agents) or between vessels (ADR 0050 carries)?
11. **Binding protocol:** is a vessel bound to work by placement (today) or by a claim/lease that a host's worker takes (Managed Agents self-hosted, Outposts)?
    - Whose clock fences a dead or paused holder?
12. **Warmth:** is warmth a live process (shallow), a paused VM, or a snapshot or image (Devin, E2B)?
    - Do session changes ever flow back into the warm artifact?
    - Does a warm pool exist before any instance asks, like `minRunners`?
13. **Unit of shared working state:** is a shared checkout tied to an instance per host (today), to a vessel home, or to a separately owned workspace that several instances or crews mount, like a shared PVC or OpenHands sandbox grouping?
    - What conflict rule applies when two writers share it (ADR 0050's linear chain)?
14. **Which interactions need model checking?**
    - Candidates the survey highlights: decision vs confirmed terminal; claim expiry vs late result; generation admission vs a terminating predecessor (CronJob `Replace`); parent rebuild vs child pruning.
