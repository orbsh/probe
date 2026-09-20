# Probe —— 控制端接入指南

> **语言 / Languages:** [English](USAGE.md) (primary) · [中文](USAGE.zh-CN.md)

控制端要驱动一个 Probe 需要实现的全部内容：连接怎么建立、交换哪些帧、每一帧附带哪些义务。权威来源是 `crates/protocol` 里的帧类型与 `crates/runtime/src/remote.rs` 里的封装行为；可工作的控制端实现是 Aura 的 probe gateway（`aura/crates/engine/src/probes.rs`），而 `crates/runtime/tests/` 下那些逐帧驱动的假控制平面就是验收测试。

## 1. 角色

Probe 是**客户端**：它主动拨出、注册，然后服务到达的帧。它不开端口，也不自发发起任何动作——它唯一会去取的 URL 是 `link` 载荷里声明的那个。

Probe 同时是**技能盲**的：它看不到 skill、看不到操作名、看不到 LLM 提示词。到达的是 `session`（驻留身份）、`entry`（代码里的入口点）、`language`（用哪个运行时）、`args`、`code`（可执行部分）。把 skill 解析成这一组是控制端的工作——而且其中没有任何字段携带能力语义：`session` 与 `entry` 在 Probe 眼里只是一个身份与一个入口名，别无其他。

Probe 拥有运行时、常驻会话与 sandbox。控制端拥有身份、调度、deadline、ctx 作用域与策略（某个操作是 AI 现场生成还是预先审计过的）。

## 2. 连接与启动

Probe 启动顺序：

1. 读配置文件——`argv[1]`，默认 `probe.json`。
2. 从 `credential_env` 指名的环境变量读凭据。变量缺失是**启动失败**：进程不会拨号。
3. 拨 `control_plane_url` 并注册。此后断线即重连（§6）。

```json
{
  "control_plane_url": "ws://127.0.0.1:8787",
  "sandbox": true,
  "credential_env": "PROBE_CREDENTIAL",
  "capabilities": {
    "node_alias": "home-pc",
    "carriers": ["steel", "python", "nushell"],
    "fs_scope": ["/home/user/work"],
    "command_exec": false,
    "network": "none"
  }
}
```

- `control_plane_url` —— 只认 WebSocket。按设计没有降级通道（long-polling 被否决过；WS-only 出网就是契约）。
- `sandbox` —— 默认 `true`，把会话进程包进 bubblewrap。远程部署必须保持开启（无人值守的外部代码）。
- `credential_env` —— 注册凭据是**用户**凭据，它从不写进配置文件。
- `capabilities.node_alias` —— **别名必须设置。** 默认值是空串，以 `""` 注册的 probe 无法按名字寻址（症状出现在控制端一侧："probe not connected"）。
- `capabilities.network` —— `"none"`、`{ "allow": ["host:port"] }` 或 `"open"`。

连接形态：一条 WS 消息恰好一个帧，JSON 文本，以 `"type"` 为内部 tag。Probe 的读端与写端是两个任务，所以连接是**全双工**的——一个调用执行期间帧仍可双向流动（§4.2 依赖这一点）。非文本 WS 消息被忽略。

## 3. 握手

Probe 发出的第一帧是 `register`；控制端必须回 `registered`。在这句应答之前，Probe 不服务任何东西。

```json
{ "type": "register", "node_alias": "home-pc", "credential": "tok-abc",
  "carriers": ["steel", "python", "nushell"] }
```

```json
{ "type": "registered" }
```

- `node_alias` 是控制端一侧连接的键位。重连即重注册：控制端必须替换该别名此前的连接（旧的那条已经死了）。
- `credential` 在这里校验，并映射到用户命名空间。能力清单就是控制端写进该用户命名空间注册表的内容。

## 4. 帧

| 方向 | `type` | 用途 |
|---|---|---|
| probe → CP | `register` | 在册与能力握手（§3） |
| CP → probe | `registered` | 握手被接受 |
| CP → probe | `call` | 一个操作 |
| probe → CP | `result` | 该操作的应答 |
| probe → CP | `host`（`kind: "call"`） | 运行中的脚本发起的 `ctx_*` 宿主调用 |
| CP → probe | `host`（`kind: "result"`） | 该宿主调用的应答 |

### 4.1 `call` / `result`

`session` 是调用方给的不透明**驻留身份**：Probe 用它给常驻运行时定键（`probe/<node_alias>/<session>`），所以共享同一 `session` 的调用共享运行时状态（VM / module 全局），不同值之间绝不共享。它必须跨调用稳定——上游 actor 的 `type/key` 是自然取值。每次调用唯一的 `call_id` 顶不了这个位置：拿它定键等于每次都冷启一个运行时。

`entry` 是所交付代码里的入口点名字。它**不是**查表键：Probe 没有注册表，也不拿它的值解析任何东西。代码没有绑定这个名字时，若代码定义了惯例入口 `execute` 就落到它，否则就是错误值。

`language` 必须是本节点真正携带的 carrier；未知或未编译进来的语言会体现在 `result` 的错误值里，而不是丢掉这次调用。

```json
{
  "type": "call",
  "call_id": "rp-7",
  "session": "notes/7",
  "entry": "read_file",
  "language": "python",
  "args": { "path": "/tmp/x" },
  "code": { "type": "inline", "bytes": [100, 101, 102, 32, 101, 120, 101, 99, 117, 116, 101, 40, 97, 114, 103, 115, 41, 58, 10, 32, 32, 32, 32, 114, 101, 116, 117, 114, 110, 32, 97, 114, 103, 115] }
}
```

```json
{
  "type": "call",
  "call_id": "rp-8",
  "session": "heavy/1",
  "entry": "main",
  "language": "wasmtime",
  "args": {},
  "code": { "type": "link", "url": "https://cdn.example/op.wasm",
            "version": "sha256:abc123", "expected_sha256": "abc123" }
}
```

`code` 只有两种形态：

- `inline` —— `bytes` 直接进帧（KB 量级脚本）。
- `link` —— Probe GET `url`（30 秒超时），在**执行之前**用 `expected_sha256` 校验 sha256，不匹配即错误值（绝不静默接受）。这个 URL 是 CDN 缓存键，不是 Probe 侧缓存：Probe 在调用之间不持有任何东西。

```json
{ "type": "result", "call_id": "rp-7", "outcome": { "Ok": { "ok": true } } }
{ "type": "result", "call_id": "rp-7", "outcome": { "Err": "unknown language 'koto'" } }
```

每个 `call` 恰好得到一条 `result`，无论成功还是失败。结果按纪律保持在消息量级：大产物不走控制平面——由操作在 Probe 一侧自行处理。

### 4.2 `host` —— ctx 桥

运行中的脚本可以回调控制端。**脚本要持久化任何东西，走的就是这条路**：控制端的存储是持久事实，而 probe 在调用之间什么都不持有（它的常驻会话是可丢弃的热缓存）。状态外置是立项时的形状，不是权宜通道——probe 不持有存储、也不持有自己的数据面凭证，正是因此。

脚本函数到线上 op 的对应关系：

| 脚本函数 | `op.op` | 载荷 |
|---|---|---|
| `ctx_state_get` | `state_get` | `field` |
| `ctx_state_set` | `state_set` | `field`、`value` |
| `ctx_state_delete` | `state_delete` | `field` |
| `ctx_invoke` | `invoke` | `target_type`、`target_key`、`handler`、`args` |

```json
{
  "type": "host", "kind": "call", "host_call_id": "h-1", "call_id": "rp-7",
  "op": { "op": "state_get", "field": "visits" }
}
```

```json
{
  "type": "host", "kind": "result", "host_call_id": "h-1",
  "outcome": { "Ok": { "present": true, "value": 1 } }
}
```

- `call_id` 是**外层** `call` 的 id —— 控制端就是靠它解析 ctx 作用域（正在读写哪个 actor 实例的字段）。Probe 不需要知道自己的实例键。
- `host_call_id` 由 Probe 铸造，每次宿主调用唯一。应答必须回显它；id 对不上的应答会被静默丢弃。
- **脚本的宿主调用会一直阻塞到应答到达。** 这一侧没有 deadline：没人应答的 `host` 调用会卡住这次脚本调用，直到连接断开。每个 `host` 调用都要答。
- 宿主调用会**在**外层 `call` 仍在执行时到达 —— "读一个 call、答一个、再读下一个"的半双工服务器承载不了这个。
- `state_get` 应答 `{ "present": bool, "value": … }`，脚本由此分支，不需要哨兵值。

## 5. 义务清单

1. 先以 `registered` 应答 `register`，再发别的。
2. 只发上表里的帧，且字段齐全。Probe 的读端遇到不认识的帧、或解析不过去的帧（字段缺失/类型不对）会**拆掉连接**然后重连——两者都当作协议错误，不要当作前向兼容。
3. 每个请求铸造唯一的 `call_id`，自己维护关联。
4. 每个 `host` 调用都要答——脚本阻塞在上面。
5. 调用 deadline 归控制端。Probe 对调用没有 deadline；结果只在活着的连接上送达，所以进行中的调用随连接一起死，控制端的超时是唯一能结束它的东西。
6. 把重连当作重注册：替换该别名的连接，并预期要重新驱动 Probe 不持有的那些状态（它在调用之间什么都不持有）。

## 6. 重连与生命周期

- 任何一次断开后，Probe 以指数退避重连：1s、2s、4s …… 封顶 60s。**退避从不重置**——即使已健康运行一小时，此后断线仍按当前（最高 60s）的延迟重连。
- 控制端**干净**关闭连接不会触发重连：Probe 视其为致命（`control plane closed the connection cleanly`）并退出进程。只有当你确实想让那台机器上的这个 probe 消失时才这么用。
- 常驻会话状态是进程内存；它跨重连存活，但不跨驱逐或进程退出。持久事实住在控制端的存储里。

## 7. 报文转录

一次完整会话，一帧一行，如实出现在线上（上文是格式化版本，这里是串联后的流程）：

```
probe → CP   {"type":"register","node_alias":"home-pc","credential":"tok-abc","carriers":["steel","python"]}
CP → probe   {"type":"registered"}
CP → probe   {"type":"call","call_id":"rp-7","session":"counter/k1","entry":"counter","language":"steel","args":{"n":4},"code":{"type":"inline","bytes":[40,100,101,102,105,110,101,32,40,101,120,101,99,117,116,101,32,97,114,103,115,41,32,40,104,97,115,104,32,34,110,34,32,52,41,41]}}
probe → CP   {"type":"host","kind":"call","host_call_id":"h-1","call_id":"rp-7","op":{"op":"state_set","field":"visits","value":1}}
CP → probe   {"type":"host","kind":"result","host_call_id":"h-1","outcome":{"Ok":null}}
probe → CP   {"type":"result","call_id":"rp-7","outcome":{"Ok":{"n":4}}}
```

（`bytes` 是字节数组：这里 probe 的代码载荷是 steel 源码 `(define (execute args) (hash "n" 4))`。这段拼接是示意性的——"先写 ctx 状态再返回"只是调用可以长的众多形状之一。）

## 8. 失败语义

| 输入 | Probe 的行为 |
|---|---|
| 未知 `language` / carrier 未编译进来 | 以 `Err` 结果的 `result` 应答 |
| `link` hash 不匹配、取件超时（30s）、代码非 UTF-8 | 以 `Err` 结果的 `result` 应答 |
| 控制端发来不认识的帧类型，或解析不过去的帧 | 拆掉连接，随后重连 |
| 非文本 WS 消息 | 忽略 |
| 启动时 `credential_env` 变量缺失 | 进程在拨号前退出 |
| `host` 结果的 `host_call_id` 不认识 | 丢弃（调用方已放弃） |

## 9. 演进

新的帧类型与字段以新增小节的方式追加到本文档；改变某个既有帧的含义属于 `docs/PLAN.md` 里落地它的那个 phase，以及 protocol crate 的文档注释——那是这份契约可被机器检查的那一半。