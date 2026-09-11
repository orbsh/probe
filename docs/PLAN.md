# PLAN

Design lives in the wiki (stateless-agent-architecture.md); this file only sequences phases.

## Milestone A — Container runtime

- [ ] Phase 0 — Workspace skeleton: `crates/{runtime,protocol,config}`; task contract (tool_call in → tool_result out); container isolation boundary definition (what a skill may touch: fs/network scope, no credentials).
- [ ] Phase 1 — Runtime carriers: koto / rune / steel / wasmer / wasmtime / wasmi, feature-gated (carried over from krystallizer's vm crate); per-skill language declaration; script-only skills end-to-end.
- [ ] Phase 2 — Container execution: OCI container per skill invocation (untrusted code); image contract (skill deps preinstalled vs mounted); timeout / resource limits; result streaming back.

## Milestone B — Actuator

- [ ] Phase 3 — Outbound registration: GitHub Actions runner mode — probe registers to Gravity/Prism via outbound WS, pulls tasks, reports results; no inbound ports; deploy on any machine and it operates that machine.
- [ ] Phase 4 — Skill fetch through Gravity: every tool call fetches the skill spec live from Krystallizer via the pull-through path (never direct, zero cache); local disk holds nothing between calls.
- [ ] Phase 5 — Host capability surface: the skill's view of the host machine (fs scope, command exec, network) declared per-probe registration; capability check on the Gravity side before dispatch.

Deferred gates:

- Aura-embedded form (Wasmtime-heavy-isolation sibling): deferred until the remote actuator is stable; embedding changes nothing in the task contract.
- Skill packaging/install lifecycle: skills are emergent (Krystallizer graph), not installed artifacts — revisit only if cold-start cost of spec fetch becomes measurable.
