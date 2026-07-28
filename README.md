<div align="center">

# tao

**One core, one protocol, many surfaces — a Rust coding agent.**

A Rust coding agent built around a universal harness core and one wire-stable protocol.

[English](README.md) | [简体中文](README_cn.md)

[![CI](https://github.com/chenqi44/tao/actions/workflows/ci.yml/badge.svg)](https://github.com/chenqi44/tao/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable%20·%20edition%202024-orange.svg)](https://www.rust-lang.org/)
[![Stage](https://img.shields.io/badge/stage-alpha%20·%20dogfood-yellow.svg)](#roadmap)
[![Platform](https://img.shields.io/badge/platform-macOS%20·%20Linux-lightgrey.svg)](#installation)

</div>

tao is a coding agent written in Rust. Its core is not a particular terminal UI but a **UI-agnostic agent harness** (`tao-core`): sessions, models, tools, permissions, persistence, and extensibility are all exposed through a single `Op`/`Event` protocol. The TUI, headless execution, stdio protocol, persistent server, ACP editor embedding, and MCP server are all just different consumers of that one protocol.

It is designed alongside [Claude Code](https://github.com/anthropics/claude-code), [OpenAI codex CLI](https://github.com/openai/codex), [gemini-cli](https://github.com/google-gemini/gemini-cli), and [opencode](https://github.com/sst/opencode): deliver the classic coding-agent experience, but push harder on four axes — **multi-surface reuse, sessions as assets, model-agnostic providers, and programmable extension**.

> **Status: alpha / dogfood.** M0–M4 are substantially complete; the author already uses tao for day-to-day development. M5 (hardening) and M6 (web frontend) are in progress. See the [roadmap](#roadmap).

---

## ✨ Features

**One core, many surfaces**
- A single `Op`/`Event` protocol drives both the in-process TUI and JSONL-over-stdio / socket — adding a new frontend costs roughly nothing.
- The same `tao` binary is a terminal agent, a headless scripting tool, an [ACP](https://agentclientprotocol.com) process spawned by editors like Zed, and an MCP server exposing itself to other agents.

**Multi-provider, no capability erasure**
- Three wire-protocol codecs: Anthropic Messages · OpenAI Responses · OpenAI Chat Completions.
- core maintains a **canonical model format** as the only intermediate layer; the agent loop sees only it. Reasoning / thinking / prompt caching are first-class fields, never flattened to a lowest common denominator.
- `base_url` is configurable, so DeepSeek / Qwen / Kimi / OpenRouter / Ollama and the whole OpenAI-compatible ecosystem drop in directly.

**Sessions as assets**
- An append-only JSONL event log is the single source of truth; in-memory state is fully rebuildable by replaying it.
- `fork` to explore branches, `rewind` to step back a conversation, shadow-git `checkpoint` to roll back file changes, `share` to export a sanitized transcript, and a full permission audit trail — all built on event sourcing.

**Programmable platform, no reinvention**
- Hooks (8 event points) · markdown subagents · slash commands · MCP · skills (progressive disclosure). **Processes + filesystem first** — no binary plugin ABI.
- This covers the bulk of the Claude Code ecosystem's value, at the cost of spawning a process and parsing markdown.

**Permissions live in the core, UI only renders**
- Permission modes (`default` / `plan` / `accept-edits` / `bypass`) + a rule engine + approvals are first-class protocol citizens. The UI has no authority to decide whether an action is safe; `plan` mode is itself just a permission profile.

**A single Rust binary**
- Fast, memory-safe, zero runtime dependencies. Crashes still flush the log, and `resume` success rate is a hard target.

### Built-in tools

| Tool | Description |
|---|---|
| `Bash` | Streaming output, timeouts, cancel-on-drop, output truncation |
| `Read` / `Write` / `Edit` | Precise edits (must Read first; replaces a unique match), diff generated before write |
| `Patch` | apply-patch DSL — transactional multi-file add/update/delete + move, with syntax/semantic-separated addressing |
| `Glob` / `Grep` | Filename pattern search; ripgrep wrapper (with built-in fallback) |
| `WebFetch` / `WebSearch` | Fetch-to-markdown / provider-native search |
| `Task` | Subagent with its own session and read-only permissions |
| `Plan` | A checklist maintained by the model |

---

## 📦 Installation

> tao is not yet published to crates.io / Homebrew (planned — see the [roadmap](#roadmap)). For now, build from source.

**Prerequisite**: [Rust stable](https://www.rust-lang.org/tools/install) (the toolchain is pinned in `rust-toolchain.toml`).

```bash
git clone https://github.com/chenqi44/tao.git
cd tao
cargo build --release        # binary: target/release/tao
# optional: install to PATH
cargo install --path crates/tao-cli
```

Supported platforms: macOS, Linux. Full Windows support (PTY / process groups / sandbox) is a M5 non-goal and not on the main line yet.

---

## 🚀 Quick start

tao is not tied to any single model provider. The fastest path is a provider API key:

```bash
export ANTHROPIC_API_KEY="sk-..."     # or OPENAI_API_KEY / DEEPSEEK_API_KEY / ...
tao                                     # launch the TUI (default subcommand)
```

Type a task right into the TUI:

```
> switch the render loop in src/render.rs to frame-throttling and add a test
```

tao reads files, edits, and runs tests, prompting for approval at each step. Press `?` for keybindings, `/help` for commands.

**One-shot headless run** (for scripts / CI):

```bash
tao exec "run cargo clippy and fix every warning" --json
```

**Point at any OpenAI-compatible endpoint** (e.g. local Ollama):

```bash
tao -c model_providers.local.base_url=http://localhost:11434/v1 \
    -c model_providers.local.wire_api=openai-chat \
    -c model=local/qwen2.5-coder
```

---

## 🧑‍💻 Commands

`tao` is a thin-shell binary that dispatches subcommands to different frontend crates:

| Command | Description | Status |
|---|---|---|
| `tao` / `tao tui` | Terminal UI (ratatui inline viewport, streaming markdown + diff coloring) | ✅ |
| `tao exec "<prompt>"` | Headless one-shot; `--json` emits the event stream, `--on-ask deny\|approve` controls approvals | ✅ |
| `tao proto` | Protocol mode: JSONL `Op`/`Event` over stdin/stdout for any-language frontends/scripts | ✅ |
| `tao acp` | ACP mode: spawned over stdio by editors like Zed for embedding | ✅ |
| `tao sessions ls\|show\|share\|audit\|gc` | Session management: fork tree / preview / sanitized export / permission audit / cleanup | ✅ |
| `tao serve --port N` | Persistent server: TCP/WS, multiple clients attach to one session | 🚧 planned |
| `tao mcp-serve` | Expose tao itself as an MCP server (list/send/read session) | 🚧 planned |
| `tao login\|logout\|auth` | OAuth / API-key interactive login | 🚧 planned (env vars for now) |

Common global flags: `-c key=value` (config override, dot-paths supported), `--profile <name>`, `--model provider/id`, `--resume <id>`, `--fork`, `--dangerously-bypass-permissions`.

### Sessions as assets, in practice

```bash
tao sessions ls                      # fork tree + titles
tao sessions show <id>               # message-summary preview
tao sessions share <id> > out.md     # export a sanitized transcript (secrets auto-redacted)
tao sessions audit <id>              # every permission decision, with its source
tao --resume <id> --fork             # branch a new session off a past one and continue
```

---

## ⚙️ Configuration

Layered loading, each layer overriding the previous:

```
built-in defaults < ~/.tao/config.toml (user) < <repo>/.tao/config.toml (project)
                 < TAO_* env vars < CLI flags (-c / --model) < --profile overrides
```

`~/.tao/config.toml` example:

```toml
model = "anthropic/claude-sonnet-4-6"
model_reasoning_effort = "medium"
permission_mode = "default"            # default | plan | accept-edits | bypass

[model_providers.anthropic]
base_url = "https://api.anthropic.com"
wire_api = "anthropic"                 # anthropic | openai-responses | openai-chat
env_key = "ANTHROPIC_API_KEY"

[model_providers.deepseek]             # any OpenAI-compatible endpoint
base_url = "https://api.deepseek.com"
wire_api = "openai-chat"
env_key = "DEEPSEEK_API_KEY"

# Permission rules: command-prefix / path glob, allow | deny | ask
[[permissions.rules]]
tool = "Bash"
pattern = "cargo *"
action = "allow"

# MCP server
[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/data"]
```

**Auth**: today the preferred path is environment variables (`env_key`, CI-friendly). OS keychain and interactive OAuth login belong to the `tao login` subcommand, which is still in progress.

**Instruction file `AGENTS.md`** (compatible with `CLAUDE.md` / `TAO.md`): discovered hierarchically — `~/.tao/AGENTS.md` → `<repo>/AGENTS.md` → subdirectories — and merged into the cached prefix of the system prompt; `@path/to/file` references are expanded inline.

Full reference in [docs/design/config.md](docs/design/config.md).

---

## 🧩 Extensibility

Five mechanisms, each with its own niche, all process + filesystem:

| Mechanism | Form | Good for |
|---|---|---|
| **hooks** | process + JSON stdin/stdout | policy gates, formatting, notifications |
| **subagents** | markdown definition + `Task` tool | context-isolated exploration / review / parallelism |
| **slash commands** | markdown templates | prompt reuse, workflow wrapping |
| **MCP** | standalone server process (stdio / HTTP) | external systems and data sources |
| **skills** | `SKILL.md` + resource pack, progressive disclosure | domain knowledge / process methodology |

```markdown
<!-- ~/.tao/agents/explorer.md — a subagent definition -->
---
name: explorer
description: Read-only code exploration and location; use when you need to understand an unfamiliar area of the codebase
tools: [Read, Grep, Glob, Bash]
---
You are a code-exploration expert. Answer "where is X / how does X work" with minimal reading, citing file:line.
```

Built-in slash commands: `/help /clear /compact /plan /mode /model /sessions /rewind /rollback /diff /cost /hooks /mcp /agent /init`.

See [docs/design/extensibility.md](docs/design/extensibility.md).

---

## 🏗️ Architecture

A Cargo workspace with a deliberately thin binary; all capability lives in libraries:

```
                    ┌─► tao-tui      (ratatui terminal UI)
                    ├─► tao-exec     (headless one-shot)
                    ├─► tao-server   (stdio proto / socket serve)
 tao-cli ───────────┼─► tao-acp      (ACP adapter, spawned by Zed)
 (thin dispatcher)  ├─► tao-mcp      (MCP client + tao itself as MCP server)
                    └─► tao-core ──► tao-protocol (Op/Event + LogEvent types)
                            │
                            └─► tao-apply-patch (patch DSL parsing & execution)
```

Three load-bearing decisions:

1. **One protocol, two transports** — `Op`/`Event` is used for both the in-process channel and JSONL over the wire. This is the key decision learned and confirmed from codex: the marginal cost of a second frontend ≈ 0.
2. **Event sourcing** — sessions persist as append-only JSONL; fork = replay a prefix under a new `session_id`; resume / rewind / checkpoint / share all build on the log.
3. **A model-agnostic canonical model** — each wire protocol is a bidirectional codec, the agent loop sees only the canonical format, and provider features are not flattened away.

The full design docs (architecture / protocol / agent loop / providers / tools / permissions / sessions / config / extensibility / ACP / TUI / testing) live in **[docs/design/](docs/design/README.md)**.

---

## 🗺️ Roadmap

| Milestone | Goal | Status |
|---|---|---|
| **M0 skeleton** | workspace + protocol types first + CI | ✅ |
| **M1 talking loop** | end-to-end single turn: 3 wire codecs + tools + turn loop + minimal TUI | ✅ |
| **M2 real coder** | permissions+approvals, Edit/Patch/Grep/Glob, AGENTS.md, event log+resume/fork, compaction, markdown rendering | ✅ |
| **M3 extensible platform** | slash commands, hooks, subagents, MCP (OAuth deferred) | ✅ 4/5 |
| **M4 sessions & collaboration** | skills, ACP, shadow-git checkpoint+rollback, session browser, share export (serve / mcp-serve deferred) | ✅ 5/7 |
| **M5 hardening** | OS sandbox (seatbelt/landlock), stronger escape analysis, performance & crash-recovery audit, Windows evaluation | 🚧 |
| **M6 tao-web-ui** | validate the protocol with a second frontend: web consuming the serve protocol, multi-client attach, proving core has no UI assumptions | ⬜ |

Every milestone is releasable on completion. From M2 on, tao is dogfooded — tao is developed using tao.

Full roadmap, risk register, and non-goals in [docs/design/roadmap.md](docs/design/roadmap.md).

---

## 🤝 Contributing

Issues and PRs are welcome. Local development:

```bash
cargo fmt --all --check        # enforced in CI
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI (`RUSTFLAGS="-D warnings"`) runs fmt / clippy / test green on macOS + Linux as the merge gate. xtask provides dev scripts for snapshot updates and fixture generation.

Design decisions and trade-offs are all recorded in [docs/design/](docs/design/README.md); please read the architecture and protocol docs before diving in.

---

## 📄 License

[MIT](LICENSE).

## Acknowledgements

tao's design draws heavily on the ideas and lessons of [Claude Code](https://github.com/anthropics/claude-code), [OpenAI codex CLI](https://github.com/openai/codex), [gemini-cli](https://github.com/google-gemini/gemini-cli), and [opencode](https://github.com/sst/opencode) — "steal, don't reinvent," self-building only the load-bearing abstractions (the model client, the patch engine). Thanks to these projects for making the coding agent a reusable form.
