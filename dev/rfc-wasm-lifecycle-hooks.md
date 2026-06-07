# RFC: WASM plugin lifecycle hooks (HookRunner bridge)

**Status:** Filed — [zeroclaw-labs/zeroclaw#7338](https://github.com/zeroclaw-labs/zeroclaw/issues/7338)  
**Tracking:** [zeroclaw-labs/zeroclaw#7339](https://github.com/zeroclaw-labs/zeroclaw/issues/7339)  
**Sponsor:** _(human maintainer — assign on filing)_  
**Evidence:** Feasibility spike on [dakaii/zeroclaw#1](https://github.com/dakaii/zeroclaw/pull/1) (draft; not proposed for direct merge)  
**Parallel:** [PR #6667](https://github.com/zeroclaw-labs/zeroclaw/pull/6667) proceeds for [#4619](https://github.com/zeroclaw-labs/zeroclaw/pull/4619) unless maintainers say otherwise

---

## Problem

ZeroClaw already defines an in-process [`HookRunner`](https://github.com/zeroclaw-labs/zeroclaw/blob/master/crates/zeroclaw-runtime/src/hooks/) with ~15 lifecycle events, but:

1. **Most hooks are declared but unwired** — only a subset (`before_tool_call`, `fire_llm_input`, `on_message_received`, etc.) fire in production today.
2. **WASM plugins cannot subscribe to lifecycle events** — `zeroclaw-plugins` supports `tool`, `channel`, `skill`, etc., but there is no `Hook` capability or `on_hook` bridge to `HookRunner`.
3. **Hermes-style post-turn learning is landing in core Rust** — [PR #6667](https://github.com/zeroclaw-labs/zeroclaw/pull/6667) adds a background skill-review fork in `zeroclaw-runtime` to close [#4619](https://github.com/zeroclaw-labs/zeroclaw/issues/4619). That fixes an immediate gap, but embeds a specific learning/review policy inside the runtime crate rather than behind an extension boundary.

Operators who want post-turn review, prompt mutation, audit hooks, or custom learning loops today must either:

- Patch core Rust (high maintenance, review friction), or
- Use unwired builtin hooks only (no WASM ecosystem path).

This conflicts with the microkernel direction ([RFC #5574](https://github.com/zeroclaw-labs/zeroclaw/issues/5574)): **the runtime should orchestrate; extensions should implement policy.**

---

## Proposal

Add a **`Hook` plugin capability** that bridges WASM `on_hook` exports into the existing `HookHandler` / `HookRunner` surface.

### Phase 0 — Protocol + discovery (small PR)

- `PluginCapability::Hook` in manifest
- `hooks = ["on_turn_complete", ...]` subscription list (validated allowlist)
- `on_hook(event, payload) -> {action, payload?, reason?}` JSON envelope (Extism first; WIT alignment deferred to [#7060](https://github.com/zeroclaw-labs/zeroclaw/pull/7060))
- `PluginPermission::prompt_modify` for modifying hooks that touch prompts
- Document in `docs/book/src/developing/plugin-protocol.md`

### Phase 1 — Minimal bridge (MVP)

- New void hook: **`on_turn_complete`** with `TurnCompleteSummary` payload (no raw tool args — secrets risk)
- `WasmHookHandler` adapter in `zeroclaw-runtime` (implements `HookHandler`, calls `zeroclaw-plugins::runtime::call_on_hook` via `spawn_blocking`)
- **`build_hook_runner()`** — single factory registering builtins + hook plugins (replace duplicated inline construction in agent / orchestrator / gateway)
- Wire **`on_turn_complete`** at turn exit in `run_tool_call_loop`
- Task-local **`TURN_HOOK_CONTEXT`** for per-turn metadata (avoids threading 50+ parameters)
- Opt-in: `[hooks] enabled = true` **and** `[plugins] enabled = true`

### Phase 1.5 — Wire dormant modifying hooks (separate PRs)

- `before_prompt_build` at system-prompt assembly (orchestrator + CLI `run()`)
- `before_llm_call` before provider `chat()` (modifying messages/model)
- `fire_session_start` / `fire_session_end` at session boundaries
- Do **not** block Phase 1 on full hook coverage

### Phase 2 — Performance + ops

- Thread-local plugin instance pooling (spike: `call_on_hook_pooled`)
- CI fixture: `plugins/hook-test` wasm build in `dev/ci.sh` or xtask
- Optional: global pool / instance TTL (follow-up)

### Phase 3 — Ecosystem

- Example hook plugin: Hermes-style skill reviewer as WASM (replaces or complements core fork)
- WIT surface for hooks when #7060 stabilizes

---

## Why a hook plugin may be cleaner than core Rust (#6667)

This is a design trade-off, not a rejection of #6667.

| Dimension | Core review fork (#6667) | Hook plugin (this RFC) |
|-----------|--------------------------|-------------------------|
| **Time to ship** | Faster — fills #4619 now | Slower — needs bridge + manifest + docs |
| **Microkernel fit** | Adds Hermes-specific fork logic to runtime | Runtime fires events; plugin owns policy |
| **Operator choice** | One built-in review policy | Install/replace reviewers without recompiling |
| **SSoT / drift** | Review config + tool surface in core | Hook subscribes via manifest; no duplicate state in handles |
| **Security boundary** | Runs with full agent tools (restricted fork) | WASM sandbox + declared permissions |
| **Ecosystem** | Doesn't generalize to other lifecycle hooks | Same bridge serves audit, prompt, turn-review, etc. |

**Recommendation:** Land #6667 (or equivalent) as the **short-term gap fix** for #4619. Pursue this RFC as the **general extension path** so the next custom lifecycle feature does not require another core fork.

### Feasibility evidence (spike, Jun 2026)

A draft spike (not proposed for direct merge) demonstrated:

- `plugins/hook-test` WASM fixture: `on_turn_complete` counter + `before_prompt_build` appends `[hook-test]`
- E2E: full `agent::run` → mock LLM captures system prompt containing `[hook-test]` (`dev/experiments/run-hook-plugin-smoke.sh`)
- Unit tests: manifest validation, `fire_turn_complete` context scoping, `WasmHookHandler` integration
- **Gotcha documented:** prompt-mutating hooks require `permissions = ["prompt_modify"]` in manifest

---

## Design sketch

### Manifest

```toml
name = "turn-reviewer"
version = "0.1.0"
wasm_path = "reviewer.wasm"
capabilities = ["hook"]
hooks = ["on_turn_complete"]
permissions = ["prompt_modify"]  # only if subscribing to before_prompt_build
hook_priority = -100             # modifying hooks: lower runs later
```

### `on_hook` request/response

```json
// Request
{"event": "on_turn_complete", "payload": { "agent_alias": "...", "success": true, ... }}

// Response (modifying hooks)
{"action": "continue", "payload": {"prompt": "..."}}
{"action": "cancel", "reason": "policy block"}
```

Void hooks (`on_turn_complete`, `on_after_tool_call`) may omit response or return `continue`.

### `TurnCompleteSummary` (void hook payload)

- `agent_alias`, `user_message`, `final_response`, `success`, `turn_duration_ms`
- `tool_calls: [{name, success, duration_ms}]` — **no raw arguments**
- `channel`, optional `session_id`

### `build_hook_runner(hooks, plugins, data_dir)`

Single registration path:

1. Builtin hooks (`command_logger`, `webhook_audit`) when configured
2. WASM hook plugins when `plugins.enabled` and `plugins-wasm` feature

### Single source of truth

- Hook subscriptions: `manifest.toml` `hooks = [...]` only
- Permissions: `manifest.toml` `permissions = [...]` only
- No cached hook lists on channel handles or agent structs (AGENTS.md SSoT rule)

---

## Alternatives considered

### A. Core Rust only ([#6667](https://github.com/zeroclaw-labs/zeroclaw/pull/6667))

**Pros:** Ships now; closes #4619; familiar config (`skills.skill_improvement.enabled`).  
**Cons:** Hermes-specific logic in runtime; each new lifecycle feature repeats the pattern; no third-party hook marketplace.

**Verdict:** Good near-term fix; not a substitute for a general hook bridge.

### B. Builtin hooks only (no WASM)

**Pros:** Simpler; no Extism overhead.  
**Cons:** Operators cannot install custom reviewers without core patches; contradicts plugin ecosystem goal (v1.0).

### C. WIT-first ([#7060](https://github.com/zeroclaw-labs/zeroclaw/pull/7060))

**Pros:** Typed interfaces long-term.  
**Cons:** Hooks not in WIT yet; blocks MVP on larger plugin migration.

**Verdict:** Extism JSON envelope for Phase 1; WIT hook interface in Phase 3 aligned with #7060.

### D. Cron/agent subagent for review

**Pros:** No new protocol.  
**Cons:** Not tied to turn lifecycle; harder to enforce prompt/tool boundaries; more operator setup.

---

## Non-goals

- Replacing #6667 in the first iteration
- Wiring every dormant hook event in Phase 1
- Memory backend plugins via hooks (use `storage.*` config + `PluginCapability::Memory` separately)
- Public hook plugin marketplace / registry client
- Cross-process hook handlers (in-process only for v1)
- Translating hook log messages (English `error_key` per RFC #5653)

---

## Risks and mitigations

| Risk | Mitigation |
|------|------------|
| WASM load latency per hook | Thread-local pooling (Phase 2); void hooks are fire-and-forget |
| Prompt injection via `before_prompt_build` | Require `prompt_modify` permission; modifying hooks sequential by priority |
| Secret leakage in hook payloads | Omit tool args from `TurnCompleteSummary`; document payload contract |
| Hook + #6667 double-review | Document mutual exclusion; config guidance |
| Unwired hooks confuse authors | RFC tracking comment lists wired vs declared events |
| `plugins-wasm` feature gate | Document build flag; default-off `plugins.enabled` |

**Rollback:** `[hooks] enabled = false` and/or `plugins.enabled = false`. Revert `build_hook_runner` registration.

---

## Rollout

1. **RFC discussion** (7 days default)
2. **Phase 0 PR** — manifest + validation + docs (no runtime wiring)
3. **Phase 1 PR** — `on_turn_complete` + bridge + orchestrator wiring + tests
4. **Phase 1.5 PRs** — `before_prompt_build`, `before_llm_call`, session hooks
5. **Phase 2** — pooling + CI wasm fixture
6. **Phase 3** — example Hermes reviewer plugin; WIT alignment

All phases: opt-in config, no breaking changes to existing installs.

**Breaking change?** No.

---

## Acceptance criteria (Phase 1)

- [ ] Hook plugin with `hooks = ["on_turn_complete"]` is discovered and registered when hooks + plugins enabled
- [ ] Successful/failed turn exit fires `on_turn_complete` with `TurnCompleteSummary`
- [ ] `build_hook_runner()` used by agent, orchestrator, gateway (no duplicated inline registration)
- [ ] E2E test with `plugins/hook-test` fixture proves WASM `on_hook` invoked on real agent path
- [ ] `plugin-protocol.md` documents Hook capability and permissions
- [ ] Spike gotchas captured: `prompt_modify` required for prompt mutation

---

## Related work

- [#4619](https://github.com/zeroclaw-labs/zeroclaw/issues/4619) — SkillImprover unwired (motivates post-turn review)
- [PR #6667](https://github.com/zeroclaw-labs/zeroclaw/pull/6667) — Core skill-review fork (parallel near-term path)
- [PR #7060](https://github.com/zeroclaw-labs/zeroclaw/pull/7060) — WIT Tool/Channel/Memory (hooks WIT later)
- [#6254](https://github.com/zeroclaw-labs/zeroclaw/issues/6254) — Plugin install path mismatch (fix before operator docs)
- Feasibility spike: [dakaii/zeroclaw#1](https://github.com/dakaii/zeroclaw/pull/1)

---

## Data hygiene

- [X] No personal/sensitive data in examples
- [X] Neutral project-scoped placeholders

_Drafted with AI assistance; sponsoring human accountable for accuracy and review responses (RFC #5615)._
