# PLAN

Design lives in the wiki (stateless-agent-architecture.md); this file only sequences phases.

## Milestone A — Container runtime

- [ ] Phase 0 — Workspace skeleton: `crates/{runtime,protocol,config}`; task contract (tool_call in → tool_result out); container isolation boundary definition (what a skill may touch: fs/network scope, no credentials).
- [ ] Phase 1 — Runtime carriers: koto / rune / steel / wasmer / wasmtime / wasmi, feature-gated (carried over from krystallizer's vm crate); per-skill language declaration; script-only skills end-to-end.
- [ ] Phase 2 — Container execution: OCI container per skill invocation (untrusted code); image contract (skill deps preinstalled vs mounted); timeout / resource limits; result streaming back.

## Milestone B — Actuator

- [ ] Phase 3 — Outbound registration: deploy on any machine and it operates that machine. Connection-direction/data-direction decoupling: on startup the probe actively connects to the control plane and registers (presence + capability list), keeps the connection open, tasks are pushed down the connection (WS frames) — no inbound ports, network ingress topology irrelevant; long-polling is the degraded implementation. Probe is an Aura Actor type (actor_type = Probe, partition_key = node_id): the control plane's calls are standard `ctx.invoke()` routing — registration credential = user credential, capability list written into the user's namespace registry; target resolution = user namespace + node alias + capability (`probe:home-pc:read_file`). Same-node seriality free via Actor mailbox semantics; timeout/error reuse `pending_calls` deadline scan.
- [ ] Phase 4 — Skill fetch through Gravity: every tool call fetches the skill spec live from Krystallizer via the pull-through path (never direct, zero cache); local disk holds nothing between calls.
- [ ] Phase 4.5 — Host-side storage service (ADR-0010): a `#[kv_storage]` executor instance deployed alongside the actuator — remote VirtualStorage backends (e.g. a Krystallizer hosted on this machine) send op frames over the existing outbound WS connection; the executor prepends its declared prefix and executes against the local engine. No dedicated listener, no second protocol: KV rides the same connection as tool calls.
- [ ] Phase 5 — Host capability surface: the skill's view of the host machine (fs scope, command exec, network) declared per-probe registration; capability check on the Gravity side before dispatch.
- [ ] Phase 6 — Data-path discipline: control plane carries instructions and result summaries only — Result stays message-sized; large artifacts (files, binaries) are handled by the skill on the Probe side (local fs, user-configured transfer tooling, transport skills). Transport-reachability requirements (direct connection / same VPN) are declared in skill metadata — the control plane neither knows nor governs the data path. AI-generated invoke arguments are instruction semantics (paths, options, fragments) and naturally bounded; a loose inbound message cap on the control plane guards anomalies only, it is not a data-plane design.

Deferred gates:

- Aura-embedded form (Wasmtime-heavy-isolation sibling): deferred until the remote actuator is stable; embedding changes nothing in the task contract.
- Skill packaging/install lifecycle: skills are emergent (Krystallizer graph), not installed artifacts — revisit only if cold-start cost of spec fetch becomes measurable.