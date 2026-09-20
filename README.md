# Probe

Skill runtime / remote actuator of the stateless agent architecture. Design: [stateless-agent-architecture.md](../../.hermes/wiki/stateless-agent-architecture.md) (wiki), Gravity (turn executor), Krystallizer (skill graph).

**Upstream-agnostic by iron rule**: probe never depends on Aura crates — all
coupling with Aura is the frame protocol (serde types in `probe-protocol`).
Any service that speaks the frames can drive it; the Aura Actor deployment
form lives entirely on the Aura side (connection-plane adapter).

Control-plane integration (connect, handshake, every frame, and the obligations
each one carries): [docs/USAGE.md](docs/USAGE.md) · [中文](docs/USAGE.zh-CN.md).

## Model

Execution only — provides a runtime environment, never lives in Krystallizer. Container-isolated; skill code is untrusted.

Two deployment forms, both supported:

- **Aura embedded**: execution base component (heavy-isolation end of the Wasmtime sandbox lineage).
- **Remote actuator**: GitHub Actions runner mode — outbound registration + task pull, no inbound ports. Deploy it on your laptop or a target server and it operates that machine.

Skill resolution happens on the Gravity side (Krystallizer → Gravity, live on every tool call, zero cache — emergent skills must have zero staleness window). The Probe never sees a skill: it receives an operation and its arguments, executes, and returns a tool result.

Runtimes: steel / python (PyO3) / wasmtime (Aura polyglot ruling) + nushell (subprocess, CGI-shaped — JSON via stdin, structured result out), feature-gated.
