# #3111: Convoy concern map (readers and writers per bundled concept, seams, cutting cost)

Research for map #3108. Read-only, against `main @ dba09029` (2026-10-12).
It builds on the inventories in `tmp/sm/{convoy,crew,vessel,history}.md`, which cite `3a5ab934`.
Every claim used below was re-checked at `dba09029`.

**Line-number drift since `3a5ab934`** (checked with `git diff --stat 3a5ab934 dba09029`):
- `crates/flotilla-resources/src/convoy.rs`: inventory lines 870–1434 shift **+11**, and lines after 1445 shift **+12**. For example, `apply` moved from 1887 to **1899**, and `enter_landing_if_completion_claims_settled` from 2744 to **2756**.
- `in_process.rs`: lines after 1875 shift **+1**.
- These files are unchanged, so inventory citations into them are still exact: `reconcile.rs`, `store/convoy.rs`, `crew_ops.rs`, `convoy_admission.rs`, `vessel.rs` and `stalls.rs` (one-line edit at 216).

All citations below are at `dba09029`.

Abbreviations:
- `C` = `crates/flotilla-resources/src/convoy.rs`
- `R` = `crates/flotilla-resources/src/convoy/reconcile.rs`
- `P` = `crates/flotilla-resources/src/convoy/promises.rs`
- `SC` = `crates/flotilla-store/src/convoy.rs` (the ConvoyReconciler)
- `CO` = `crates/flotilla-core/src/in_process/crew_ops.rs`
- `CA` = `crates/flotilla-core/src/in_process/convoy_admission.rs`
- `V` = `crates/flotilla-controllers/src/reconcilers/vessel.rs`
- `VP` = `crates/flotilla-controllers/src/reconcilers/vessel_placement.rs`
- `CE` = `crates/flotilla-controllers/src/reconcilers/convoy_ensure.rs`
- `AG` = `crates/flotilla-aggregator/src/aggregator.rs`
- `RP` = `crates/flotilla-core/src/in_process/read_projections.rs`
- `LE/*` = `crates/flotilla-core/src/leaf_engine/*`

Concept letters, following #3108's (a)–(f):

| Letter | Concept | One-line meaning in today's code |
|---|---|---|
| **H** (a) | vessel home | the convoy as the owner, namer and placement anchor of its vessels and checkouts |
| **W** (b) | workflow instance | the pinned workflow, its vessel DAG, and execution progress (`work[v]`, Pending/Active/Interrupted) |
| **D** (c) | deliverables checklist | subjects, promises and the decision-ledger claim evidence |
| **L** (d) | landing / settlement authority | Landing entry, settlement, Landed/Failed/Abandoned, completion authority, teardown sanction |
| **G** (e) | generation identity | `{project, role, generation}`, ensured-from, the frozen ensure config |
| **F** (f) | crew-work state | per-role phase plus all the per-crew overlays (stall, nudge, delivery, attention, follow-up) |
| X | plumbing / audit | message, lifecycle_mutations, name |

---

## 1. Field → concept table

### 1.1 `ConvoySpec` (`C:66-100`; stored via `ConvoySpecRecord` `C:233-268`)

Written once by admission (`CA:1195` create; ensure path `CA:1303-1351`). Nothing enforces immutability; `reap --force` writes metadata only.

| Field | Concept | Notes |
|---|---|---|
| `workflow_ref` | W | `R:836-855` fails the convoy if it changes after init (I16) |
| `inputs` | W | template interpolation only (`R:1099-1113`) |
| `role` | **G** | Also the display name (`AG:2204`) and the identity key for the generation allocator (`CA:3697-3724`). For dispatched convoys it is the human slug, `intent.name` (`CA:3045-3050`), so G applies to *every* convoy, not only standing ones |
| `generation` | **G** | allocated as `max(label, spec)+1` under the admission lock (`CA:3702-3722`) |
| `project_ref` | **G** + H | identity key, credential and brief scope (`V:1254`, `V:1301`), surface scoping |
| `dispatching_principal_ref` | L / X | attribution; surfaced as `ConvoyRow.dispatching_principal_ref` |
| `placement_policy` | **H** | fallback for the placement snapshot (`C:804-806`) |
| `repositories[]` (`url`, `repo_ref`, `source_ref`, `target_ref`, `workspace_slug`, `subpaths`) | **H** + D + L | H: checkout naming and path (`V:522-555`, `V:797-809`). D: subject addressing (`C:118-165`). L: `target_ref` produces `target_mismatches` at Settle (`R:1236-1260`) |
| `ref` | H | branch override (`V:529`) |
| `continuation` | H + D | marks one repository as continuing a branch; makes a bound CR Produces rather than Adopts (`C:301-302`) |
| `adopted_checkout_refs` | **H** | counted into `expected_checkout_refs` (`C:378-393`) |
| `subjects[]`, `issues[]` (decode-only shim), `change_request` (decode-only shim) | **D** | `declared_subjects()` (`C:270-308`); `issues` and `change_request` are also brief inputs (`V:1129`, `V:1248-1249`) |
| `instruction` | W / F | brief text |
| `promises[]` (DispatchPromise) | **D** | seeded into `status.promises` at Bootstrap (`C:2012-2014`) |

### 1.2 Metadata

| Item | Concept | Notes |
|---|---|---|
| `name` | X (shared key) | **Used as a key by H**: the shared checkout name `checkout-<convoy>…` (`V:797-803`), the `convoy=` label on every child, and the TerminalSession identity (`V:1162-1170`) |
| `creation_timestamp` | **H** + G | part of the shared checkout identity hash (`V:1622-1635`), so a new incarnation never reuses an old checkout |
| labels `project` / `role` / `generation` | **G** | the generation allocator reads the label before the spec (`CA:3707-3708`) |
| ann. `workflow-snapshot` | W | `pinned_workflow_ref` (`C:800-802`) |
| ann. `placement-snapshot`, `vessel-placements`, `image-layers` | **H** | `pinned_placement_ref` (`C:804`), `vessel_placement_pin` (`C:61-64`) |
| ann. brief-artifacts | W / F | brief rendering (`V:1267`) |
| ann. `ensured-from` | **G**, but a behavioural switch on W/H/L | `is_ensured` (`R:29-31`) changes failure semantics: work Stalled instead of Failed, vessel recreate instead of fail (I1) |
| ann. `force-teardown` | L | checkout finalizer mode (`checkout.rs:636`) |
| finalizer `convoy-teardown` | L + H | `SC:565-599` deletes children and waits on federated checkouts |

### 1.3 `ConvoyStatus` (`C:955-1040`)

| Field | Concept | Embedded structure → concept |
|---|---|---|
| `naming` | G / X | admission provenance (#3063) |
| `landing_entry` | **L** | `LandingEntry{entered_at, claims[], event_emitted}`. Each `LandingClaim{vessel, role, claimed_at, message, completed_while_crew_active, preceding_turn: LandingTurn{source, TurnDeliveryEpisode}}` is **an L record holding F data**: a frozen copy of crew claims and their delivery episode |
| `landing_settlement` | **L** | `SettlementEvaluation`, the evidence receipt |
| `environment_observations[v]` | **H** | per-vessel runtime evidence copied from Vessel status (`R:807-823`) |
| `ensure_admission` | **G** | an embedded frozen `ConvoyEnsureSpec`. ADR 0047 requires it to decode for a generation even after the ensure is deleted (`C:969-974`) |
| `phase: ConvoyPhase` | **W + L (fused)** + F | Pending/Active/Interrupted are W; Landing/Landed/Failed/Abandoned are L. Active⇄Landing is also flipped by F-level events (I2–I4) |
| `subjects[]`, `unlinked_subjects[]`, `branch_subject_scan_{at,error}` | **D** | `DiscoveredSubject{subject, relationship, sources[]}` |
| `stalled: Option<StalledCondition>` | **F**, but also **W/H** | One slot. Its `maker` is `Actor{vessel, role}` (F), `Supervisor{…}` (F), `Observed` (D), or `Controller{retry}` with `cause: Capacity`, which is a **placement wait (H/W)**. The aggregator reinterprets a Controller stall as `admission_blockers` (`AG:2291-2297`) |
| `nudge_obligations[]` | **F** | `NudgeObligation{maker, leaves, history, progress, quiet_since…}` |
| `provisioning` | W (H) | gates the capacity-wait check (`SC:178-245`) |
| `placement_decision` | **H** | write-once (`C:1993-1995`); the convoy-level target host, read by `select_convoy_children` (`C:484-513`) and by the vessel placement gate (`V:465-472`) |
| `workflow_snapshot: WorkflowSnapshot` (`C:1263-1276`) | **W**, but its embedded parts span four concepts | `vessels: Vec<VesselRequirement>`: `name`/`depends_on` are W; `repository_refs` and `credential_*` are H; `crew: Vec<CrewSpec>` is F (roster) and D (per-role template promises, `workflow_template.rs:647-648`). `exit: ExitDeclaration` is L. `turn_delivery`, `stall_nudges` and `supervision` are F policy. `cascade: ResolvedCascade` (roles, skills, charter) is F/W. The ADR 0047 shim for `VesselRequirement.stance` lives here (`workflow_template.rs:311-317`) |
| `work[v]: WorkState` (`C:1476-1496`) | **W**, plus three foreign fields | `phase` (W); `provisioning_retry` (**H**/G: ensured vessel recreate budget); `completion_authority` (**L**: HumanOverride versus CrewRollup, reset by 7+ F patches); `placement: PlacementStatus` (**H**: a free-form JSON map copied from Vessel status, `R:1648-1680`, holding `checkout_refs`, which `expected_checkout_refs` (`C:383-391`) then treats as the **teardown checklist**) |
| `crew_work[v][r]: CrewWorkState` (`C:1534-1590`) | **F**, with claim fields that are **D/L** | `phase`, `started_at`, `resumed_at`, `resume_brief_id`, `pending_follow_up` and `message` are F. `disposition`, `decision_ledger_ref`, `decision_ledger_digest`, `claim_evidence`, `completion_override`, `completed_while_crew_active`, `completion_turn`, `superseded_claims[]` and `completion_refusal{expectation, causes, consecutive_count}` are **the claim record (D evidence and L input)** |
| `promises[v][r][]: Promise` | **D** | Open/Submitted/Kept/Retracted. Keyed **by crew position**, so handoff moves them (`P:263-317`). Settlement reads them (`C:2103`, `R:1233-1235`) |
| `message`, `started_at`, `finished_at` | X / W / L | `finished_at` is rewritten by W roll-ups, F reopenings and L edges |
| `disposition`, `target_mismatches` | **L** | written by Settle |
| `observed_workflow_ref`, `observed_workflows` | W | written once by Bootstrap |
| `turn_deliveries[source]: TurnDeliveryStatus` | **F** | `hold`, `failure`, `episodes[]`, plus legacy `pending_brief`/`pending_supervisor_turn` (ADR 0047 shims, "remove after #2710 roll") |
| `attention: Option<ConvoyAttention>` | **F + L** | one slot. Its sources include F (delivery, hold, missing-hook, queued-turn) and L (settlement `observed-digest`) |
| `lifecycle_mutations[]` | X (audit) | ring buffer of 32 |

**Counts.** The spec has 16 fields; 4 are multi-concept. The status has 31 fields; 9 are multi-concept, or embed structures owned by another concept (`phase`, `stalled`, `attention`, `workflow_snapshot`, `work[v]`, `crew_work[v][r]`, `landing_entry`, `promises`, `finished_at`). Of the 44 patch variants (`C:1645-1897`), 19 are F-led and **about 20 write two or more concepts in one `apply`** (see §3; the W→F roll-ups `WorkRunning`, `WorkInterrupted` and `RollUpWork` count too).

---

## 2. Concept × code matrix

R = reads, W = writes. "Raw" means it reads the Convoy resource directly; "wire" means it reads `ConvoyRow`/`VesselRow`. Production code only.

### 2.1 Writers (status patches and metadata) by concept

| Writer (file:line) | H | W | D | L | G | F |
|---|---|---|---|---|---|---|
| Admission create + `SetPlacementDecision` / `RecordNaming` / `RecordEnsureAdmission` (`CA:1195-1201`, `CA:510`, `CA:1351`, `CA:1303-1351`) | W spec.repositories, placement annotations, `placement_decision` | W workflow_ref, inputs, workflow-snapshot annotation | W spec.subjects/issues/promises | — | W role/generation/labels, ensured-from, `ensure_admission`, naming | — |
| ConvoyReconciler pre-rules (`SC:177-400`, `SC:402-563`): R1/R2 capacity stall, R3 branch discovery, R4 ObservePromise, R23 settlement attention, R24 teardown wait | R1 reads host readiness | R1 gates on `provisioning` | W subjects (R3), promises (R4) | W attention (R23), message (R24); prepare evaluates settlement and `verify_reclaim` | — | W `stalled` (R1/R2, Controller maker) |
| Pure `reconcile_internal` (`R:781-908`), with Bootstrap `R:910-1019`, Backfill `R:1021-1058`, fail-fast `R:1115-1147`, ready `R:1149-1175`, crew roll-up `R:1177-1224`, phase roll-up/Settle `R:1226-1292`, vessel outcomes `R:1294-1483`, cleanup `R:1501-1547`, create/delete vessel `R:1556-1642` | W `environment_observations`, `work[v].placement`; actuates CreateVessel/DeleteVessel/DeleteCheckout; reads Vessel and Checkout | W phase (Pending/Active/Interrupted), `work[v].phase`, snapshot | W promises seed (Bootstrap) | W phase Landed/Failed, landing event, `completion_authority` (Backfill→HumanOverride) | R `is_ensured` switches branch (`R:1184-1197`, `R:1417-1476`, `R:1614-1642`) | W `crew_work` (Bootstrap/Backfill/WorkRunning/WorkInterrupted) |
| `crew complete` (`CO:1078-1468`) | — | W phase → Landing (via `enter_landing`), → Active (BeginFollowUp) | W subjects (Claim), promises seed/submit/observe | W `landing_entry`, phase Landing | — | W crew phase Done, refusal, follow-up, clears stall |
| `crew fail` / `crew stall` / `promise retract` (`CO:1470-1539`, `CO:1032-1057`) | — | — | R promises (retract → stall) | — | — | W crew phase, `stalled` (overwrites) |
| `crew supervise` (`CO:1541-1718`) | — | via resume | — | — | — | W `stalled` rung |
| `convoy abandon` (`CO:1996-2048`) | **effect**: git push archive of checkouts (`CO:1926-1994`) first | W non-terminal work → Abandoned | — | W phase Abandoned (even from Landed/Failed) | R ensure? (abandon is also driven by the ensure roll, `CE:430-437`) | — |
| `crew handoff` (`CO:2102-2435`) | effect: TerminalSession create | — | W promises move to target (`P:263-317`) | W `completion_authority` → CrewRollup | — | W crew phases (target Working, sender HandedBack); BYPASS rollback `CO:2429` |
| `convoy resume` / follow-up sweep / turn actuator (`CO:2437-2700`, `CO:3323-3401`, `CO:3035-3135`) | effect: credentials re-stage, TS MarkStarting | W phase → Active, work → Running | — | W `completion_authority`; leaves Landing | — | W crew → Working, clears `stalled`, deletes held turn sources; BYPASS `CO:3395`, `CO:3603`, `CO:3619` |
| `convoy work force-complete` (`crew_actions.rs:374-410`) | — | W work → Complete | — | W HumanOverride; may enter Landing without a promise check | — | — |
| Hold actuator (`CO:3445-3584`) | effect: TerminalSession holds | — | — | — | — | W hold + `attention` (overwrites) |
| Leaf engine: turn delivery (`LE/turn_delivery.rs:94-470`, BYPASS `:465`) | — | W phase → Active, work → Running | — | leaves Landing; clears **all** attention, including settlement | — | W crew → Working, episodes |
| Leaf engine: stalls judge (`LE/stalls.rs:222-1386`) and wake (`LE/wake.rs:140-170`) | — | R phase (holding = Active/Landing) | R promises/conditions (row arming, `LE/wake.rs:357-367`) | — | — | W `stalled`, `nudge_obligations`, `attention` (missing-hook, queued) |
| Subject discovery (`in_process.rs:2846`, `:3632-3656`, `:3720-3732`) | — | — | W subjects, scan, unlink | — | — | — |
| Finalizer / reap (`SC:565-612`; `CO:3625-3661`) | W deletes children (H) | — | — | W force annotation, `message` | — | — |

### 2.2 Writers outside Convoy status that are decided by Convoy fields

| Code | Convoy fields read | Concepts | Effect |
|---|---|---|---|
| Vessel `prepare` (`V:449-1392`) | `phase` terminal (`V:462`); placement pin and `placement_decision` (`V:465-472`); `workflow_snapshot.vessels` (`V:507`); `spec.repositories`/`ref`/`issues`/`change_request` (`V:522-555`, `V:929`, `V:1129`, `V:1248-1249`); `metadata.name` + `creation_timestamp` (shared checkout, `V:797-809`, `V:1622-1635`); `crew_work[v][r].phase` + `work[v].phase` (`V:1151-1158`, `V:1186-1196`); `spec.project_ref` (`V:1254`, `V:1301`) | **H W D L G F** (all six) | creates Checkout (owner = Convoy, `V:1694-1712`), Environment and TerminalSession (owner = Vessel, `V:1672-1685`); restarts sessions |
| Placement projector (`VP:94-235`) | convoy replica presence, pin / `placement_decision` | H | creates the actuator Vessel copy |
| Checkout finalizer / OwnerTerminal (`checkout.rs:606`, `:632-714`) | `convoy_sanctions_checkout_reclaim` (`C:454-457`: deleting or Landed/Abandoned), `force-teardown` annotation | L → H | deletes the shared checkout |
| TerminalSession reconciler (`terminal_session.rs` `TSR:288-289`, `:683-688`) | owner convoy terminal | L | MarkFailed |
| Owner GC (`crates/flotilla-store/src/owner_gc.rs`; `registry.rs:668-698`) | convoy absence (local and replica) | H | deletes children |

### 2.3 Control-plane readers (ensure, admission, credentials, dispatch, store, replication, validation)

| Reader (file:line) | H | W | D | L | G | F |
|---|---|---|---|---|---|---|
| Ensure controller `CE:188-197`, `:364-395` (`active_ensured_convoys`), `:551-584`, `:764-781`, `:848` | R `creation_timestamp` (backoff, `:637-646`) | — | — | R phase: **three terminal sets** — `is_terminal` (`:392`), {Failed, Abandoned} (`:781`), Failed-only streak (`:583`) | R ensured-from, `spec.generation` max, `ensure_admission` | — |
| Ensure roll `CE:400-458` (abandon then commit, **two durable writes**; comment at `:438-441`) | R `ensure_admission.repositories`/placement | R `ensure_admission.workflow_ref` | — | W Abandoned (via admission) | R/W generation, ensured-from (`CE:1148-1177`) | — |
| Ensure drift `CE:981-1029` | R | R | — | — | R `ensure_admission` vs ensure spec | — |
| Admission `CA:210-231` continuation hold | R spec.ref, repositories | — | R active CR subjects | R terminal, deletion | — | — |
| Admission `CA:610-649`, `:1179-1209`, `:1264-1355`, `:1384-1430`, `:3045-3108` | W repositories, placement annotations (`CA:701-736`, `:1145-1177`), `SetPlacementDecision` (`:1199`) | W workflow_ref, inputs, snapshot annotation | W ref, continuation, issues, change_request, promises | — | W **`spec.role = intent.name`** (`CA:3045-3050`), so **every** convoy carries G: a dispatched convoy's role is its human slug, and its generation is usually 1. W labels (`:1189-1193`), naming, `ensure_admission` (`:1351`) | — |
| Generation allocator `CA:3697-3724` | — | — | — | R phase (live = not terminal **or status absent**) | R role, project, generation label/spec; **includes replicas** | — |
| Ensure admission live check `CA:1280-1290` | — | — | — | R phase | R ensured-from, generation; **local only**. It disagrees with the allocator's replica-inclusive check, so a replica-live predecessor gets a different error from the allocator | — |
| `in_process.rs:511-532` CR credential refs; `:602-645` CR observation | — | R `workflow_snapshot.vessels[].credential_refs` | R subjects, change_request | R terminal | — | — |
| `in_process.rs:1295-1335`, `:3155-3238` (route a command to the convoy home), `:5527-5568` (message origin) | R placement pin, home host | R work Failed, snapshot | — | R terminal | R role/project labels | — |
| `in_process.rs:4061-4147` prepare ensured convoy; standing backing-dead check | R Environment `convoy=` | R provisioning; **refuses a workflow with `exit`** (`:4105`) | — | — | R ensure role | — |
| Credentials runtime `crates/flotilla-daemon/src/runtime/credentials.rs:181-207`, `:251-299`, `:584-615` | — | R `workflow_snapshot.vessels[].credential_{refs,scopes,permissions}`, crew roles | — | — | R `project_ref` (GitHub App scope `:471-508`), `dispatching_principal_ref` | — |
| `flotilla-credentials` | none (only `archive_environment_home(convoy_ref)` at `agent_material.rs:403`) | | | | | |
| Dispatch reconciler `crates/flotilla-daemon/src/dispatch_reconciler.rs:259-265`, `:397-428`, `:653-684`, `:725-746` | R placement_policy | R pinned workflow | R `spec.issues` (served / landed) | R Landed, terminal | R project_ref grouping | R **`crew_work` phases → capacity** (`:725-746`; empty crew_work counts as 1) |
| `runtime/state.rs:224-300` verify_backing_dead; `sleep_inhibitor.rs:339-350` | R placement_decision.target_host, Environment `convoy=` | R provisioning, snapshot vessels | — | R non-terminal | — | — |
| Store `message_retention.rs:57-68`, `role_routing.rs:202-224`, `:344-372`, `message_conditions.rs:21-35`, `:203-215`, `registry.rs:916-927` | — | R work[w] | — | R terminal | R project_ref (address book) | R `crew_work` keys (peer addresses), `pending_follow_up` (audit retention), turn_deliveries (legacy migration gate) |
| `owner_gc.rs:40-75`; `registry.rs:509-569` | R Convoy controller ownerRef on children; authorship collisions | | | | | |
| Leaf engine `LE/wake.rs:245-466` sync_rows, `LE/subscriptions.rs:201-206`, `:338-349` | R children | R snapshot crew conditions, work[v] | R subjects, snapshot crew promises | R phase ∈ {Active, Landing} ("holding"), exit | — | R stalled, crew_work, turn_deliveries |
| `agent_adapter.rs:157-160` standing classification; `:354-415` work context | R repositories, ref | R `exit` absent | R issues, change_request | — | R ensured-from; **`role == "governor"` literal** (`:369`) | — |
| Placement projector `VP:95-130`, `:243-252` | R **replica** Convoy pins, placement_decision | | | | | |
| Checkout reconciler `checkout.rs:211-263`, `:306-307`, `:370`, `:385-394`, `:633-637` | R `convoy=` label, expected_checkout_refs (federated) | — | R scan_at, continuation | R terminal, Abandoned, Landed, force-teardown | — | — |
| Pre-roll validation `src/resource_validate/frozen.rs:212-320`, `:468-489` | R placement pin/decision | R pinned workflow, snapshot, inputs, `phase != Pending` (`:292`) | — | R terminal → skip (`:216`) | R pre-roll re-admission annotation (`:26`) | — |

**Replication.**
- Convoy is `ReplicationClass::HomeBoundRuntime` (`C:33`).
- The replicator is class-agnostic. It streams the whole `ResourceObject` (metadata, spec **and status**) under `replica_writer(origin_root)`, with 60 s digest repair and multi-hop relay keeping the original origin (`crates/flotilla-replication/src/replicator.rs:273-286`, `:414-466`, `:592-628`, `:942-1055`).
- Replica reads go through `ReplicaReadResolver`, which has no mutation methods (`crates/flotilla-store/src/backend.rs:81-86`). Only the home writes.
- So **all six concepts share one authority (the convoy home) and one replication unit**. A change to any F field re-ships the whole record, snapshot included, to every peer.
- No Convoy-specific replication code exists. Splitting a concept into its own kind is a registration change for the replicator, not a protocol change. The cost is in homing and routing (which host authors the new record), plus the router-level scenario rows that the repository conventions require for targeting changes.

### 2.4 Surface readers (aggregator, projections, CLI/TUI, manifest)

Only two production functions read a raw Convoy and build surface rows: `Aggregator::summarize`/`summarize_vessel` (`AG:2204-2588`) and `ReadProjections::explain_convoy` (`RP:1207-1777`). A few narrow raw readers exist: `crew_stalls` (`RP:104-245`), crew list (`CO:819-899`), fleet address (`fleet.rs:174-183`), fleet inventory scope (`RP:1102-1112`) and manifest inbox holds (`flotilla-manifest/src/projection/subjects.rs:394-399`). Everything else reads the wire rows.

| Reader | H | W | D | L | G | F |
|---|---|---|---|---|---|---|
| `AG` change-request refresh (`AG:1112-1308`) | R repositories, ref | R phase Active | R subjects, change_request | R phase terminal | — | — |
| `AG::summarize` (`AG:2204-2365`) | R placement_decision, repositories | R workflow_snapshot, work, phase, observed_workflow_ref | R subjects, promises (counts), issues | R phase, landing_entry (`AG:2308-2311`), disposition | R role, generation, ensured-from, project_ref (`AG:2300-2306`) | R stalled, attention, holds, pending turns, crew_work |
| `AG::summarize_vessel` + `readiness.rs:20-199` | R work[v].placement (`AG:2405-2463`), child labels | R work[v].phase | — | R `complete_work` | — | R stalled (Actor/Work match), crew_work Pending, session attention |
| `RP::crew_stalls` (`RP:104-245`) | — | R phase ∈ {Pending, Active, Interrupted, Landing} | — | R phase non-terminal | R project, role, generation (display "role #gen") | R stalled, crew_work Stalled |
| `RP::explain_convoy` (`RP:1207-1777`) | R children via `select_convoy_children`, expected checkouts, environment_observations, placement, pins | R pinned workflow, snapshot cascade/skills, phase | R subjects, promises, decision ledgers (`RP:1834-1910`) | R **live** `evaluate_landing_settlement` (`RP:1469-1494`), landing_entry/settlement | R naming, role/project address | R holds, queued turns, unclaimed work (`RP:1793-1832`), lifecycle_mutations, stalled. **No `attention` field** |
| `fleet.rs` (`fleet.rs:140-472`) | R ConvoyRow.placement_decision | R phase | R subjects, promise_counts | R landing_reason | R raw project_ref/role (address) | R surface_state |
| `awareness_projection.rs:69-447` | R repo | R phase, initializing, vessels | R issues | R phase | R project_ref | — |
| TUI `convoy_model.rs:199-271`, `table_view.rs:612-1510`, `status_bar_widget.rs:256`, `palette.rs` | R placement, host | R phase, vessels, workflow_ref, readiness | R subjects, issues | R phase, disposition | R generation, project, address | R surface_state |
| CLI `cli.rs:746-1417` (fleet ls, crew list, explain, stalls) | R | R | R | R | R | R |
| Manifest projection (`flotilla-manifest/src/projection.rs:485-1108`, `subjects.rs:122-399`) | R vessels, placement | R phase, initializing, readiness | R subjects | R phase terminal, superseded | R ensured_from, generation (`projection.rs:485-492`, `:574-591`) | R surface_state, raw holds |

**Wire carriers.**
- `ConvoyRow` (`flotilla-protocol/src/result_set.rs:1037-1101`) carries **all six concepts in one flat row**:
  - H: `placement_decision`, `repo`
  - W: `workflow_ref`, `phase`, `initializing`, `observed_workflow_ref`
  - D: `promise_counts`, `issues`, `subjects`
  - L: `landing_reason`, `disposition`
  - G: `address_role`, `generation`, `project_ref`, `ensured_from`, `dispatching_principal_ref`
  - F: `admission_blockers`, `message`, `surface_state`
- `VesselRow` (`result_set.rs:1226`) carries H, W, the L field `complete_work`, and F.
- `ConvoyExplanation` (`flotilla-protocol/src/commands.rs:425-481`) carries all six.
- Fields that cross no surface wire except as opaque explain JSON: `placement_policy`, `adopted_checkout_refs`, `inputs`, `provisioning`, `observed_workflows`, `unlinked_subjects`, `branch_subject_scan_*`, `target_mismatches`, `ensure_admission`, `nudge_obligations`.

---

## 3. Coupling hot spots (one transaction or decision spanning two or more concepts)

Ranked by how hard each one is to cut.

1. **`ConvoyStatusPatch::apply` arms that cross concepts.** All of these sit in `C:1899-2645`, and each runs in one CAS write.

   | Arm | Lines | Concepts | What crosses |
   |---|---|---|---|
   | `ResumeCrewWork` | `C:2444-2489` | F→W→L→F | crew → Working, work → Running, convoy → **Active** (leaves Landing), `completion_authority` → CrewRollup, `stalled` cleared, held delivery sources deleted, matching attention cleared |
   | `RecordTurnDelivery` | `C:2523-2546` | F→W→L | the same, and **clears all attention** (`C:2532`), including L's settlement attention |
   | `MarkCrewCompleted` | `C:2282-2334` | F→D→L | refuses on open promises (D), sets crew Done (F), clears the stall, then `enter_landing_if_completion_claims_settled` (`C:2756-2811`) writes `landing_entry` and phase **Landing** (L) |
   | `ForceWorkCompleted` | `C:2230-2241` | W→L | work Complete + HumanOverride, then may enter Landing |
   | `HandoffCrewWork` | `C:2414-2443` | F→D→L | crew phases, **promises move with the crew** (`P:263-317`), `completion_authority` reset; convoy phase untouched (I4) |
   | `Bootstrap` | `C:1996-2019` | W→F→D | snapshot, work, crew_work, promise seeding, phase |
   | `BackfillCrewWork` | `C:2020-2032` | F→L | inserts crew and stamps **HumanOverride** on Complete work |
   | `Settle` | `C:2102-2119` | D→L→F | promise gate, then Landed, then **clears attention** |
   | `FailConvoy` | `C:2055-2072` | L→W→F | Failed, cancels work, clears the operator brief |
   | `MarkCrewFailed` / `MarkCrewStalled` / `RefuseCrewCompletion` | `C:2364-2413`, `C:2335-2363` | F→L, F→F | reset `completion_authority`; overwrite or clear the single `stalled` slot |
   | `RestoreTurnActivation` | `C:1925-1952` | F→W→L | compare-and-restore across crew, work, phase and stall |
   | `HoldTurnDelivery` | `C:1920-1924` | F→L | the hold **overwrites** the shared `attention` slot |

2. **`work[v]` is a W record that hosts H and L.** `work[v].placement.checkout_refs` (H) is the **teardown checklist** that `expected_checkout_refs` (`C:378-393`) feeds to the settlement evaluator (`R:711`) and to `verify_reclaim` (`SC:356-386`). `work[v].completion_authority` (L) is reset by seven F patches. Separating W from H or L means moving these fields.

3. **The ensure switch (`is_ensured`, `R:29-31`) reaches G into W/H/L decisions.** In `R:1184-1197`, a crew failure becomes Stalled instead of Failed. In `R:1417-1476` and `R:1614-1642`, a failed or missing vessel is deleted and recreated with `work[v].provisioning_retry`. So G decides W's failure semantics and H's vessel replacement, and nothing in the record says so except an annotation (I1, ADR 0032 §1 divergence).

4. **Settlement reads D, H and F to make an L decision.** `roll_up_phase_outcome` (`R:1226-1292`) reads all work Complete (W), all promises terminal (D), the exit disposition from `instantiate_exit` (snapshot L plus subjects D plus expected checkouts H, `C:716-756`), and `repositories.target_ref` (H). `SC:330-355` evaluates settlement only in Landing, against replica change requests. `crew complete` evaluates a different predicate, `evaluate_crew_completion` (`R:162`), with a forced forge refresh (`CO:1245-1288`).

5. **The single `stalled` and `attention` slots are shared by four concepts.**
   - `stalled` writers: F (crew stall, judge), H/W (R1 capacity wait with Controller maker, `SC:408-446`), and D (Observed maker).
   - `attention` writers: F (holds, deliveries, missing hook) and L (settlement digest, `SC:506-524`).
   - Readers then disambiguate by `maker` or `source` string: `AG:2259-2297`, `C:875-881` (`is_turn_delivery_attention`, #3086).
   - R1 versus the L7 judge is a likely writer fight (`LE/stalls.rs:1352-1384`).

6. **The Vessel reconciler is a six-concept consumer** (§2.2). Its interrupt/restart handshake reads `crew_work` (F) and `work` (W) in the same decision (`V:1186-1196`). Its checkout identity reads `metadata.name` plus `creation_timestamp` (H tied to the incarnation, G).

7. **`crew complete` (`CO:1078-1468`) runs about six sequential CAS writes** across D (discover subjects, seed and submit promises, observe), F (refuse, follow-up, complete) and L (Landing), with forge I/O in between. Partial failure leaves D written and F/L not.

8. **`convoy abandon` (`CO:1996-2048`)** runs an H effect (git push of the shared checkouts, `CO:1926-1994`), then an L write (Abandoned), then a W write (work Abandoned). `CE:430-437` does the same for G-driven rolls.

9. **Admission (`CA:1195-1351`, `CA:3697-3724`)** writes H, W, D and G in one create. Then **three non-atomic follow-up writes** (placement, naming, ensure admission) land.
   - The one-live-generation check (G) reads replicas and treats "status absent" as live (`CA:3711`).
   - The ensure-admission check right before it is local-only (`CA:1280-1290`).
   - The ensure roll (`CE:400-458`) is G+L+H+W in two durable writes: abandon, then commit.
   - "Terminal" means three different things inside CE (`:392`, `:583`, `:781`). Under `:781` a Landed ensured convoy counts as running. That is latent today, because ensured workflows may not declare `exit` (`in_process.rs:4105`), so they can never reach Landed.

10. **Surfaces re-derive cross-concept state.**
    - Convoy `SurfaceState` (`AG:2268-2282`) mixes D (subject conflicts), F (attention, holds, pending turns, stalled) and H (vessel surfaces), and ignores W/L phase except a terminal filter for delivery attention.
    - `ConvoyRow::readiness()` (`result_set.rs:1197-1215`) mixes L (terminal), W (initializing), F→W (Controller stall = admission blocker) and H (vessel readiness).
    - The manifest standing badge (`flotilla-manifest/src/projection.rs:494-528`) mixes G, L, W/H and F.
    - These are where "derived state disagrees" (`convoy.md` §4.2).

11. **Replication and routing couple F to H's home.** Convoy is `HomeBoundRuntime`: every write happens at the convoy's home, and replicas are read-only. A crew verb issued from the vessel's host must therefore route to the convoy home. When the home is unreachable, `crew complete` parks as TerminalSession `completion_pending` with background retry (`crates/flotilla-daemon/src/server/remote_commands.rs:1175-1269`). The crew's own state lives at the convoy home, not with the crew. See §2.3 for replication details.

---

## 4. History findings

### 4.1 Free-floating vessels

- **Decided**:
  - ADR 0009 (PR #649, Accepted) set PersistentAgent residency as "a **free-floating (convoyless) Vessel** running a harness" (`docs/adr/0009-persistent-agents.md:15-16`).
  - CONTEXT.md still says a Vessel "may also be free-floating — attached to no Convoy at all" (`CONTEXT.md:219-220`) and repeats it under PersistentAgent (`CONTEXT.md:741`).
  - ADR 0014 (#683/#698) named the free-floating sessions query `independents`.
- **Superseded in practice**: #650 ("PersistentAgent kind + Governor v1") closed 2026-08-15 with the comment "Superseded: ADR 0030/#1440 materialize the governor as an ensure-declared standing convoy, not a PersistentAgent kind". ADR 0030 (PR #1468, 2026-08-13) chose a standing convoy. **The PersistentAgent kind was never built**: the string appears in production code only in an issue-tracker replay fixture (`crates/flotilla-core/src/providers/issue_tracker/fixtures/github_dispatch_board.yaml`). Nobody amended ADR 0009 or CONTEXT.md.
- **Code**: a free-floating vessel is unrepresentable.
  - `VesselSpec.convoy_ref: String` is required (`crates/flotilla-resources/src/vessel.rs:26`).
  - `V:454-456` marks a vessel whose convoy is NotFound **Failed** (absorbing).
  - Vessels are created only by the convoy reconciler (`R:1556-1598`) and the placement projector.
  - `independents` covers sessions with no `convoy=` label (`AG:2726`), not vessels.
- **Consequence for #3108**: standing-ness had to go *into* Convoy, which is how generation identity (G) and the `ensured-from` behavioural switch got bundled. Timeline: #1580 `spec.generation` (08-18), #2418 `ENSURED_FROM` (10-02), #2452 `ensure_admission` (10-02), #2882 `provisioning_retry` (10-07).

### 4.2 Succession (convoy record changes home)

- **Decided**:
  - ADR 0016 (PR #741, Accepted) says: "a new home authors a successor convoy record (`succeeded_from` ref) and each vessel's host … repoints its convoy ref". A home that learns of a successor stands down, and live handoff is cooperative succession (`docs/adr/0016-overlay-replication-for-cross-root-state.md:183-226`).
  - CONTEXT.md:575-580 records this as **Succession**.
  - ADR 0033 (PR #1611) amends 0016's homing but keeps the convoy home fixed at admission. #2276 (09-30) kept placed-convoy authoring on the origin.
  - #2304 (open) proposes renaming "origin-root" to "home" partly *because* a home can change by Succession.
- **Code**: absent.
  - `succeeded_from` appears only in ADR 0016 (`git log -S` finds no code commit).
  - There is no repoint path: `VesselSpec.convoy_ref` is written once at creation.
  - There is no `host declare-lost` verb (`rg declare.lost` in crates finds nothing).
- **Consequence**: ADR 0016's premise is that "the convoy record is coordination only — spec, progression, vessel refs". Today's record is the opposite. It holds the crews' entire work state (F), the deliverables (D) and the teardown checklist (H via `work[v].placement`). A successor record would have to copy all of them, so the record shape works against Succession.

### 4.3 Warm-kept vessels

- **Decided**:
  - ADR 0021 (#1136/#1159): "the vessel stays warm" during Landing.
  - Its follow-on note asks for **per-vessel warmth by DAG reachability**: "a `Landing` convoy [may] hold one warm vessel having released three … Vessel teardown must therefore be independently reachable" (`docs/adr/0021-convoy-lifecycle-landing-landed-anchored.md:123-131`).
  - ADR 0028 (#1344) re-reads warmth as the shallowest **park depth** ("warm process → suspended vessel → no vessel") (`docs/adr/0028-world-terminal-exits-and-the-vessel-as-cache.md:89-93`, `:182`).
  - ADR 0059 gives AgentSession its own park depth.
  - CONTEXT.md:216-218 says vessels are "kept warm between Legs, or revisited by later Legs".
- **Code**: no mechanism.
  - Vessel reclaim is all-or-nothing at convoy terminal plus `reclaim_eligible` (`R:1501-1547`, `SC:356-386`).
  - There is no per-vessel release, no suspended depth, and no "revisit by a later Leg". Legs don't exist; work is per vessel per convoy.
  - #2165 (open, dehydrated crews) and #2872 (open, host-loss rehydration) are the live threads.
- **Consequence**: vessel lifetime (H) is a function of convoy phase (L), so independently reachable teardown is impossible without separating H from L.

### 4.4 Vessel/convoy ownership references

| Child | Owner ref | Labels | Created by | Origin |
|---|---|---|---|---|
| Vessel | Convoy, controller (`R:1582-1587`) | `convoy`, `vessel`, `authority=managed` | convoy reconciler; projector copy on the placed host | #581 (04-14) |
| Checkout (managed) | **Convoy** (`V:1694-1712` `convoy_owned_child_meta`) | `convoy` | **Vessel** reconciler | #761 (07-20); rationale written to CONTEXT.md only on 10-08 (#2913): "survive vessel replacement" (`CONTEXT.md:92-93`) |
| Environment, TerminalSession | Vessel (`V:1672-1685` `owned_child_meta`) | `vessel-ref` | Vessel reconciler; handoff creates a TS directly (`CO:2397-2421`) | #582 |
| Vessel → Convoy (forward) | `spec.convoy_ref: String` (name only, no uid or generation) | — | — | #582/#581 |

Two things to note:
- The forward ref is a **name**, not a `{uid | generation}`.
- Name collisions across incarnations are avoided only by generated names (ADR 0032 §2) and, for checkouts, by hashing `creation_timestamp` (§4.5).

### 4.5 Per-convoy-per-host shared checkout

Code: `V:797-809` and `V:1616-1635`.
- **HostDirect** and **DockerWorktreeOnHostAndMount** name the checkout `checkout-<convoy-name>[-<slug>]-<scope>`, where `scope = sha256("managed-checkout-placement-v1", host_ref, convoy.creation_timestamp, actuator-source-root)[..12]`.
- **DockerFreshCloneInContainer** uses the **vessel** name in place of the convoy name.

**History:**
1. #582 (2026-04-15): checkout per TaskWorkspace (per vessel), `checkout-<vessel>`.
2. #761 "Start convoys from project issues" (2026-07-20) switched host-direct and worktree-mount to the **convoy** name. The PR body's only rationale is one bullet: "Reuse convoy-owned branch worktrees across vessels". There was no ADR and no issue discussion. The underlying constraint is that the **branch is per convoy per repository** (`ConvoyRepositorySpec.source_ref`), and git refuses to check out one branch in two worktrees of the same clone. Given a branch per convoy and a shared host clone, one worktree per host is forced.
3. #1407 (2026-08-05, closes #1402) added the placement-scope hash so that "redispatches cannot collide with stale same-named records". This **made the per-host split explicit**: the same convoy on two hosts gets two checkouts.
4. Live follow-ups: #3090 (agreed design) and #3102 (open PR, `LocalCloneOnHostAndMount`) give Docker vessels **private** host clones. That adds a fourth sharing behaviour: a private clone per vessel on the same convoy branch. Whether vessels see each other's work then depends on the strategy *and* the host.

**Status**: never decided as semantics. It is a by-product of the branch-per-convoy rule combined with the worktree strategy, and #3108's "unit of shared working state" question is open.

### 4.6 Field accretion timeline (`git log -S` on `crates/flotilla-resources/src`)

There were 110 commits to `convoy.rs` since 2026-04-01.

| Date | PR | Field | Concept |
|---|---|---|---|
| 04-14 | #581 | `workflow_snapshot`, `work[v].placement` | W, H |
| 07-13 | #695 | `crew_work`, `completion_authority` | F, L |
| 07-28 | #1217 | `placement_decision` | H |
| 07-28 | #1182 | `ConvoyPhase::Interrupted` | W |
| 08-08 | #1429 | `turn_deliveries`, `attention` | F |
| 08-18 | #1580 | `spec.generation` | G |
| 08-24 | #1749 | `provisioning` | W |
| 09-13 | #1888 | `lifecycle_mutations` | X |
| 09-27 | #2130 | `stalled` | F |
| 09-29 | #2200 | `subjects` | D |
| 10-02 | #2418 | `ensured-from` annotation | G |
| 10-02 | #2452 | `ensure_admission` | G |
| 10-02 | #2442 | `nudge_obligations` | F |
| 10-05 | #2662 | `environment_observations` | H |
| 10-07 | #2882 | `provisioning_retry` | H/G |
| 10-10 | #2983 | `landing_entry` | L |
| 10-10 | #2999 | `promises` | D |
| 10-12 | #3063 | `naming` | G |

12 of the 18 landed in the last 50 days, each as a field on Convoy and none as a separate resource.

---

## 5. Candidate seams, with cost

**Cost axes:**
- **Churn**: code that moves or changes.
- **Store (ADR 0047)**: the stored-shape shim needed. Golden corpus: `crates/flotilla-store-testkit/tests/fixtures/stored-records/r355/Convoy.json`, 63 records, which **must not be regenerated**. Pre-roll validation is in `src/resource_validate/frozen.rs:214-292`. The CRD schema is in `crates/flotilla-resources/src/crds/convoy.crd.yaml` (404 lines).
- **Wire**: replication and protocol impact. Wire formats change freely, because the fleet rolls as a unit.

A general ADR 0047 rule applies to every split. A field that leaves Convoy status must still **decode** from stored N records for one roll: keep a `#[serde(default)]` read-only field, or use a deserialize-only record type as `VesselRequirement` does. The new resource also needs a one-time **projection on read**, or a controller-driven backfill, so that live N-generation convoys get their new sibling records after the roll. There is no migration framework, so the backfill has to be an idempotent reconciler step: "if the status has legacy field X and no sibling Y, create Y". That is the main recurring cost of every split below.

### S1. CrewWork as its own resource (F out of Convoy)

**Shape**: a `CrewWork` (or AgentSession-bound) resource per `{convoy, vessel, role}`. It carries:
- phase;
- `stalled` per crew, which fixes the one-slot problem;
- `nudge_obligation`;
- `turn_deliveries` per crew source;
- `pending_follow_up`;
- the resume grace;
- the claim record (`disposition`, `decision_ledger_*`, `claim_evidence`, `completion_refusal`, `superseded_claims`).

It is homed with the crew (the vessel's host) or at the convoy home. Convoy keeps an aggregate read.

| Axis | Cost |
|---|---|
| Churn | **Highest.** 19 of 44 patch variants are F-led. Rewrite the `crew_ops` verbs (`CO`, 3,759 lines, 36 `crew_work` references), `LE/stalls.rs` (14×) and `LE/turn_delivery.rs`. The convoy roll-up `R:1177-1224` becomes a cross-resource read. Every F→W→L cross-arm in §3.1 becomes a two-record transaction, or a choreography of crew write → convoy reconciler roll-up (preferable, as it removes I3/I4 by construction). Aggregator `summarize_vessel` and `crew_stalls` re-key. `dispatch_reconciler` capacity counts would read CrewWork. Hegel/router tests: new kind registration |
| Store | Decode-only shims for `crew_work`, `stalled`, `nudge_obligations`, `turn_deliveries` and `attention`'s F sources, plus a backfill that creates CrewWork from live N convoys. `turn_deliveries` already carries two un-retired #2710 shims, so retire those first. `LandingClaim.preceding_turn` embeds `TurnDeliveryEpisode`; keep a copy type |
| Wire | New kind in replication. If homed at the vessel host, crew verbs **stop routing to the convoy home**, which removes `completion_pending` parking (§3 item 11). That is a routing-scenario change (router harness row). `ConvoyRow.surface_state` would be computed from N+1 records |
| Pays off | Per-crew conditions (ADR 0045 §6), crew-outside-vessel (#2912), Succession (the convoy record shrinks toward "coordination only") |

### S2. Settlement / Landing authority (L) as a separate record, or at least separate fields with one writer

**Shape**: a `Landing` (or `Settlement`) record, or a status sub-struct with declared single ownership (ADR 0024 §2 status ownership). It holds `landing_entry`, `landing_settlement`, `disposition`, `target_mismatches`, the settlement `attention` source, `completion_authority` per work, and the terminal outcome (Landed/Failed/Abandoned plus ADR 0017 `failure_source`). Phase splits into W progress (Pending/Active/Interrupted) and L outcome (open/Landing/Landed/Failed/Abandoned).

| Axis | Cost |
|---|---|
| Churn | Medium. The writers are `enter_landing` (`C:2756`), Settle (`C:2102`), FailConvoy/FailInit, MarkConvoyAbandoned, ForceWorkCompleted, R22 and R23. Readers that switch on phase include AG (6 sites), RP explain/stalls, `fleet`, `awareness_projection`, the TUI rank and cells, the manifest, `convoy_sanctions_checkout_reclaim` (checkout and TS reclaim), the CE terminal sets (`CE:392`, `:583`, `:781`), `CA:3711` liveness, `V:462` and `frozen.rs:292`. Removing the forced Landing→Active flips (I3) is a **semantic** change that every F-reopen path must re-decide (#3094) |
| Store | Splitting `phase` is the expensive part. Keep `phase` decodable with today's 7 values for one roll, deriving the new pair on read (`Landing` → `{progress: Active, outcome: Landing}`; `Interrupted` → `{progress: Interrupted, outcome: open}`), and write only the new pair. The `Anchored`/`Cancelled` aliases from #2917 are still within their window, so wait for that roll. `status.phase` is a **queryable field** (`C:36`); the SQLite index and query clients change |
| Wire | `ConvoyRow.phase` splits into two; the TUI and manifest change. Replication is unaffected if L stays inside Convoy as fields; a separate record is a new kind |
| Pays off | Removes I2/I3/I4/I6/I7. Gives one place for the "terminal" definition (four today). The reclaim sanction can include Failed (D22) |

### S3. Deliverables checklist (D) as its own record

**Shape**: a `Deliverables` record per convoy with `subjects`, `unlinked_subjects`, scan state and `promises`. Promises are keyed by deliverable, not by crew position, with the crew as a reference. That makes handoff a reassignment, not a move.

| Axis | Cost |
|---|---|
| Churn | Medium-low. The writers are `P` (1,301 lines, already a module), subject discovery (`in_process.rs`), R3/R4, Bootstrap seeding and `crew complete` steps (a)–(i). The readers are settlement (`R:1233`, `C:2103`), the turn-delivery rejection rows (`C:645-671`), `AG` promise counts and subjects, explain, and `LE/wake.rs` row arming (`:357-367`). The D→F gate in `MarkCrewCompleted` (`C:2293`) becomes a cross-record precondition, checked by the caller against a version, or by making the claim a D-record write |
| Store | Decode-only `promises`/`subjects`/scan fields on ConvoyStatus plus a backfill. `spec.subjects`/`issues`/`change_request` stay on the spec: they are admission inputs, and `issues`/`change_request` are already decode-only shims (#2873 window) |
| Wire | `status.subjects.*.subject` is a queryable field (`C:44`), used for "which convoy produces CR X" lookups. The query would move to the new kind. The relay/subject routing (`flotilla-manifest/src/projection/subjects.rs`, `AG:1112-1308`) re-keys. Replication: new kind |
| Pays off | #3094 (who may retract a promise) becomes a D-record ownership question. Crew handoff stops mutating deliverables |

### S4. Generation identity (G) out of Convoy: the standing role as its own record

**Shape**: today ConvoyEnsure plus labels plus `ensure_admission` plus the `ensured-from` annotation. Make a `StandingRole`/`Incarnation` parent that owns generations: the one-live invariant, the retry budget, the frozen ensure config, and "ensured failure semantics" as a declared policy field instead of an annotation switch.

| Axis | Cost |
|---|---|
| Churn | Medium. Move `is_ensured` branches (`R:1184-1197`, `R:1417-1476`, `R:1614-1642`) to a policy read. Touches `CE` (`ensure_admission` 29×), `CA:3697-3724` allocation, the manifest standing projection, and `RP` stalls display. `provisioning_retry` moves to the parent, or stays as an H field on the vessel-home split (S5) |
| Store | `ensure_admission` already lives in convoy status with a "decode even after the ensure is deleted" obligation (`C:969-974`), which a move must preserve as a decode-only field. The labels stay. ADR 0047 cost is low because the G data is small, but `ensured-from` is a **queryable annotation** (`C:46`) |
| Wire | `ConvoyRow.ensured_from`/`generation` remain as projections. No routing change |
| Pays off | Restores ADR 0032 §1 (one record = one incarnation) by making in-generation vessel replacement an explicit policy. Fixes the forked "terminal" definitions in CE |

### S5. Vessel home (H) as a `Workspace`/`Berth` record (unit of shared working state), with vessels referencing it

**Shape**: a record per shared working state, which is today `(convoy, host)` for worktree strategies and `(vessel)` for fresh clone. It owns the managed checkouts (moving them from Convoy-owner to Workspace-owner), the branch, the placement decision and pins, `environment_observations`, and the teardown checklist (today `work[v].placement.checkout_refs`). Vessels reference it, and Convoy references it.

| Axis | Cost |
|---|---|
| Churn | Medium-high. Touches V (checkout naming `V:797-809`/`V:1616-1635`, owner meta `V:1694-1712`), the checkout finalizer and OwnerTerminal (`checkout.rs:606-714`), `convoy_sanctions_checkout_reclaim`, `expected_checkout_refs` and every consumer (settlement `R:711`, `SC:356-386`, explain), the convoy finalizer (`SC:565-599`), abandon's archive push (`CO:1926-1994`), reap, owner GC and `select_convoy_children` (D12 pin bug). The aggregator `summarize_vessel` placement fields |
| Store | Checkout identity is the cost. Live checkout names embed the convoy name plus the placement hash, so new Workspace records must **adopt existing checkout names**, not recompute them, or every in-flight convoy re-clones. Also, `work[v].placement` is free-form JSON (`PlacementStatus{flatten}`), so a decode shim is cheap but the backfill must parse `checkout_refs` (`C:387-389`, already fallible). Checkout ownerRefs change from Convoy to Workspace on live records, which is an owner-GC hazard: GC deletes a child whose owner is absent locally and in replicas, so the backfill must write the new owner before removing the old |
| Wire | New kind replicated HomeBound at the placement host (matching ADR 0016 "vessels home at placement"). It resolves the two-Vessel-copies shape (D12/D13) only if the projector is rethought. TUI/VesselRow unaffected |
| Pays off | Per-vessel warmth and independently reachable teardown (ADR 0021 follow-on). Free-floating vessels (a Workspace with no Convoy). Succession (convoy can be replaced; workspaces and vessels stay). #3090/#3102 become Workspace strategies instead of a fourth implicit sharing rule. Makes "who sees whose work" an explicit field |

### S6. Workflow instance (W) stays the Convoy core, slimmed

After S1–S5, Convoy is ADR 0016's "coordination only" record:
- `workflow_ref` and snapshot;
- `work[v].phase` (the DAG progress);
- `provisioning`;
- refs to Deliverables, Landing, Workspace(s) and CrewWork rows.

The W-side cost is the snapshot's embedded mix: `VesselRequirement.crew` (F roster), template promises (D), `exit` (L), and `turn_delivery`/`stall_nudges`/`supervision` (F policy). The snapshot should **stay frozen whole** on Convoy, because it is the admission pin. The other records read their slice of it. That avoids an ADR 0047 shim for the embedded snapshot, which already carries the `stance` and `cascade` shims.

### S0 (precondition, cheapest): declared status ownership without moving data

Before any record split, enrol Convoy status in `FieldOwnership`, extending it from spec paths to status paths (`crates/flotilla-resources/src/field_ownership.rs:62-65`, which says status "joins when the status write path enrolls"). Use the concept letters above as owners, run in Observe mode, and route the six BYPASS writes (`CO:2429`, `CO:3311`, `CO:3395`, `CO:3603`, `CO:3619`, `LE/turn_delivery.rs:465`) through `apply`.

| Axis | Cost |
|---|---|
| Churn | Low to medium. A per-variant owner table over 44 variants, plus routing the bypass writes |
| Store | **Zero** |
| Wire | **Zero** |
| Pays off | Measures, in production telemetry, exactly which §3 cross-arms fire. That gives empirical input to the #3108 decomposition grill before paying any S1–S5 store cost |

### Cost summary

| Seam | Churn | ADR 0047 store | Wire / replication | Main risk |
|---|---|---|---|---|
| S0 ownership table | L–M | none | none | none; observe-only |
| S2 L split (phase pair) | M | **H** (`phase` is queryable; 7-value decode-and-derive) | ConvoyRow field split | re-deciding every F-reopen edge (#3094) |
| S3 Deliverables | M–L | M (backfill; queryable `status.subjects`) | new kind; subject query re-key | claim gate becomes cross-record |
| S4 Standing parent | M | L | none | CE terminal-set consolidation |
| S5 Workspace | M–H | **H** (adopt live checkout names; ownerRef swap vs owner GC) | new kind at placement host | owner-GC deletion during backfill |
| S1 CrewWork | **H** | **H** (5 fields + 2 open #2710 shims; LandingClaim copy) | new kind; **routing change** (router scenario rows) | two-record transactions in every reopen path |

**Suggested order** (for the grill, not a recommendation to defer):
1. S0, to measure.
2. S2's phase split, since it fixes the most invented semantics per line.
3. S3 and S4, which have the smallest blast radius.
4. S5 and S1, which carry the routing and identity cost and depend on the #2912 AgentSession model.

Retire the open #2710, #2917 and #2873 shims before any of S1–S3, so that the one-roll window doesn't stack two generations of decode-only fields on `turn_deliveries`, `phase` and the spec.

ADR carry: none yet. This is research input to the #3108 decomposition grill. A resolution would amend ADR 0024 §1/§2 (status ownership and tables), ADR 0016 (the convoy record is not yet "coordination only"), ADR 0009 and CONTEXT.md (free-floating vessel and PersistentAgent superseded by ADR 0030 but never amended), and ADR 0021's follow-on (per-vessel warmth).
