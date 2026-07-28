# 配置、认证与指令文件

## 1. 分层配置(figment)

优先级从低到高(后者覆盖前者):

```
内置默认值 < ~/.tao/config.toml(用户级) < <repo>/.tao/config.toml(项目级)
          < 环境变量(TAO_*) < CLI flags(-c key=value / --model 等) < profile 覆盖
```

- 项目级文件**只承诺稳定子集**(permissions、mcp_servers、instructions、hooks 白名单),敏感字段(api_key)只允许用户级。
- `-c key=value` 支持点路径(`-c model_providers.x.base_url=...`),与 codex 习惯一致。
- `profiles.<name>`:任意顶层字段的覆盖包,`--profile work` 一键切换。
- 未知键:warning 不报错(向前兼容);类型错误:fail-fast 并指出文件:行。

## 2. config.toml 参考

```toml
# ---- 模型 ----
model = "anthropic/claude-sonnet-4-6"
model_provider = "anthropic"            # model 带前缀时可省略
model_reasoning_effort = "medium"       # minimal|low|medium|high|max
small_model = "anthropic/claude-haiku-4-5"  # 压缩/标题生成等辅助任务

[model_providers.anthropic]
name = "Anthropic"
base_url = "https://api.anthropic.com"
wire_api = "anthropic"                  # anthropic | openai-responses | openai-chat
env_key = "ANTHROPIC_API_KEY"

[model_providers.openai]
name = "OpenAI"
base_url = "https://api.openai.com"
wire_api = "openai-responses"
env_key = "OPENAI_API_KEY"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com"
wire_api = "openai-chat"                # 兼容生态统一走 openai-chat
env_key = "DEEPSEEK_API_KEY"

# ---- 行为 ----
permission_mode = "default"             # default|plan|accept-edits|bypass
auto_compact_at = 0.92                  # 上下文窗口占比
max_turn_steps = 100
exec_timeout_ms = 120_000
editor = "vim"                          # /editor 外部编辑器

# ---- 网络/重试 ----
request_max_retries = 4
stream_max_retries = 3
stream_idle_timeout_ms = 60_000

# ---- MCP ----
[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/data"]
startup_timeout_ms = 10_000

[mcp_servers.remote]
url = "https://example.com/mcp"         # streamable HTTP
headers = { Authorization = "Bearer ..." }

# ---- 钩子 ----
[hooks]
PreToolUse = [{ matcher = "Bash", command = "./scripts/check.sh" }]
PostToolUse = [{ matcher = "Edit|Patch", command = "cargo fmt --check || true" }]
Stop = [{ command = "./scripts/on-stop.sh" }]

# ---- 会话 ----
[sessions]
keep_days = 30
max_session_mb = 50

# ---- 档案 ----
[profiles.work]
model = "openai/gpt-5.1"
permission_mode = "accept-edits"
```

## 3. 认证(auth)

凭证解析顺序:`env_key 环境变量` → OS keychain(`keyring`)→ `~/.tao/auth.json`(0600)→ OAuth 交互登录。

```bash
tao login anthropic            # OAuth PKCE + loopback 浏览器流程
tao login --api-key openai     # 交互输入,存 keychain
tao logout anthropic
tao auth status                # 每个 provider 的凭证来源与有效期
```

- OAuth:loopback 端口随机,state/PKCE 校验;refresh token 存 keychain,access token 内存缓存 + 401 惰性刷新重试一次。
- `auth.json` 结构按 provider 分节;权限 0600;被外部修改时按 mtime 热加载。
- headless/CI:只认 `env_key`,不做任何交互。

## 4. AGENTS.md(指令文件)

层级发现,全部并入 system 的"指令区"(缓存前缀内,位置固定):

```
~/.tao/AGENTS.md              # 个人全局偏好
<repo 根>/AGENTS.md           # 项目约定(社区通用名;兼容 CLAUDE.md/TAO.md)
<repo>/子目录/AGENTS.md       # 目录级,访问该目录文件时生效(惰性追加)
```

- 兼容:repo 根若只有 `CLAUDE.md`/`TAO.md` 而无 `AGENTS.md`,读取并在 UI 提示一次(降低迁移成本)。`AGENTS.md` 优先。
- `@path/to/file` 引用语法:展开为文件内容(带路径标注),递归深度限 3。
- 合并顺序:全局 → 根 → 目录;冲突时更具体的靠后(后写优先,模型天然更重视靠后指令)。
- `SessionMeta.config_fingerprint` 记录指令文件 hash 集,resume 时指令变了会提示。

## 5. 项目级目录

```
<repo>/.tao/
├── config.toml       # 项目配置子集
├── agents/*.md       # 项目子 agent 定义
├── commands/*.md     # 项目 slash 命令
├── skills/*/SKILL.md # 项目技能
└── hooks/            # (可选)hook 脚本,相对路径引用
```

项目级可执行内容( hooks、命令里的 bash 占位)首次加载时需要用户在 TUI 一次性确认"信任此项目"(hash 记入用户级 trust db;内容变化重新询问)——防恶意仓库投毒。
