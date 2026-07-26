# 模型提供方(Providers)

## 1. 设计原理:规范模型 + 线协议 codec

三家 provider 的请求/响应结构互不相似(Anthropic 的 content blocks、Responses 的 output items、Chat 的 delta 累积)。tao-core 内部定义**规范模型格式 `Model`**,agent loop、历史、日志全部使用它;每个 provider 模块是一个**双向 codec**:

```
                ┌─────────────────────────────────────────────┐
   agent loop ─►│ 规范 ModelRequest {messages, tools, ...}     │
                └───────┬──────────────────▲──────────────────┘
              encode(请求)│                  │decode(流事件→规范增量)
        ┌───────────────┼──────────────────┼───────────────┐
        ▼               ▼                  ▼               │
 anthropic.rs   openai_responses.rs   openai_chat.rs       │
 (Messages API) (Responses API)       (Chat Completions)   │
        └───────────────┴──────────────────┴───────────────┘
                        HTTPS + SSE
```

原则:codec 只做**结构翻译**,不做策略(重试/预算在 client 公共层);provider 特性(reasoning、cache、thinking)在规范格式里有**一等字段**,不被抹平。

## 2. 规范模型格式

```rust
pub struct ModelRequest {
    pub system: Vec<SystemBlock>,            // text + cache breakpoint 标记
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ToolSpec>,                // wire 无关:{name, desc, schema}
    pub model: ModelId,                      // "claude-sonnet-4-6" / "gpt-5.1" / ...
    pub reasoning: Option<ReasoningEffort>,  // minimal|low|medium|high|max
    pub max_output_tokens: u32,
    pub temperature: Option<f32>,
    pub metadata: RequestMeta,               // session_id 等,供 provider 侧遥测
}

pub enum ModelMessage {
    User { content: Vec<Content> },
    Assistant { content: Vec<Content>, stop_reason: Option<StopReason> },
    ToolResult { call_id: String, content: Vec<Content>, is_error: bool },
}

pub enum Content {
    Text(String),
    Thinking { text: String, signature: Option<String> }, // Anthropic thinking 需回传
    ToolUse { call_id: String, name: String, input: Value },
    Image { mime: String, data: Bytes },
}

pub enum ModelStreamEvent {                   // 规范增量,UI 直接消费
    TextDelta(String),
    ThinkingDelta(String),
    ToolUseBegin { call_id: String, name: String },
    ToolUseInputDelta { call_id: String, json_fragment: String },
    ToolUseEnd { call_id: String },
    MessageEnd { stop_reason: StopReason, usage: TokenUsage },
}
```

`ToolSpec` 的 schema 用 JSON Schema;Responses/Anthropic 原生接受,Chat 需要 `parameters` 包装(codec 负责)。

## 3. 三个线协议适配器

### 3.1 Anthropic Messages API(`wire_api = "anthropic"`)

- `POST {base}/v1/messages`,`anthropic-version` 头,`x-api-key` 或 OAuth bearer。
- encode:`system` 数组(带 `cache_control: ephemeral` 断点);`messages` 交替 user/assistant;`ToolResult` 放 user 消息的 `tool_result` block;`Thinking` block 回传时**必须携带原 signature**(缓存内思考块原样保留)。
- decode:SSE 事件 `content_block_start/delta/stop` → 规范增量;`input_json_delta` 的 partial_json 直接透传为 `ToolUseInputDelta`(累积由公共层做)。
- thinking:`thinking: {type:"enabled", budget_tokens}` 由 `reasoning` 映射;开启时 temperature 强制 1(codec 处理)。
- prompt caching:在静态前缀末尾打 breakpoint;TTL 默认 5m,长任务可配 1h。

### 3.2 OpenAI Responses API(`wire_api = "openai-responses"`,OpenAI 首选)

- `POST {base}/v1/responses`,`store: false`(core 自持历史,不用 server-side state)。
- encode:`input` 为 item 数组:`message` / `function_call` / `function_call_output` / `reasoning`;assistant 的思考映射为 `reasoning` item(带 `encrypted_content` 若提供,zdr 兼容)。
- decode:SSE `response.output_text.delta`、`response.reasoning_summary_text.delta`、`response.output_item.added/done`、`response.completed`(取 usage 与 stop 原因)。
- 内置工具(web_search 等)按 provider 配置透传;`include`/`reasoning.effort` 映射。

### 3.3 OpenAI Chat Completions(`wire_api = "openai-chat"`,兼容生态)

- `POST {base}/v1/chat/completions`,覆盖 DeepSeek / Qwen / Kimi / OpenRouter / Ollama 等 OpenAI 兼容端点。
- encode:规范消息 → `messages`;`ToolUse` → assistant `tool_calls[]`;`ToolResult` → `role: "tool"`(配对 `tool_call_id`);`Thinking` 块丢弃(该协议无法回传)。
- decode(SSE delta 累积,该协议**没有 item 边界**,需状态机):
  - `tool_calls[i].id` 先到、后续 chunk 只有 `arguments` 片段 → 按 index 累积;
  - 增量参数**不做**完整 JSON parse,等 `finish_reason` 后一次性 parse(失败则把原始串作为错误结果返回模型,让其自我修正);
  - `reasoning_content` 字段(DeepSeek 等)→ `ThinkingDelta`。
- 该适配器刻意保持最小(codex 的教训:两个 client 漂移);新增能力优先落在前两个协议。

### 3.4 自研 client 而非 SDK 的理由

承重抽象必须自控:tool-call 流式增量、usage 提取、provider 怪癖(缓存头、429 退避、空 SSE keep-alive)、断流恢复——通用 SDK(async-openai/genai/非官方 anthropic-sdk)都会在某个关键处挡住你。用 `reqwest`(rustls)+ `eventsource-stream` 自研,参考 codex/aichat 的做法;类型定义可借 `async-openai` 起步。

## 4. 公共层:重试、超时、取消、usage

`ModelClient::stream` 的公共包装(所有 provider 共享):

| 机制 | 行为 |
|---|---|
| 重试 | 429 / 5xx / 连接错误:指数退避(0.5s 起,×2,上限 30s),最多 `request_max_retries`(默认 4);尊重 `Retry-After` |
| 流中断 | 已收到 ≥1 个字节后断流:整个请求重建重试(`stream_max_retries` 默认 3);**不重放 partial**——重试拿到的是全新完整流,UI 端以"本条消息重置"事件处理 |
| 空闲超时 | 60s 无 SSE 事件 → 视为断流(provider 通常有 keep-alive,超时即异常) |
| 取消 | `CancellationToken` 挂在 reqwest future 上,drop 即断连 |
| usage | 归一化 `TokenUsage { input, cached_input, output, reasoning }` → `TokenCount` 事件 + 日志 |
| 观测 | 每次请求一个 `tracing` span(provider/model/ttft/total/retry 次数) |

## 5. 模型目录与选择

- 内置 catalog(内置在二进制,可被 config 覆盖/扩展):`{id, provider, display, context_window, max_output, supports_thinking, supports_images, supports_cache}`。
- `model` 配置支持 `provider/model-id` 显式指定;未带前缀时按 catalog 默认 provider 解析。
- `ListModels` Op 返回 catalog + 各 provider 在线探测结果(可选)。

## 6. 认证

| 方式 | 存储 | 适用 |
|---|---|---|
| `env_key` | 读环境变量(不落盘) | 所有 provider,CI 友好 |
| config `api_key`(不推荐) | config.toml | 本地便利 |
| OS keychain | `keyring` crate | `tao login --api-key` 交互存入 |
| OAuth PKCE + loopback | `~/.tao/auth.json`(0600)+ keychain 存 refresh token | Anthropic(Claude 订阅)/ OpenAI(ChatGPT)/ Gemini 后期 |

- 凭证抽象:`Credential::resolve(provider) -> HeaderValue`,请求时惰性解析,401 时刷新一次并重试。
- `secrecy::SecretString` 全程包裹,日志永不打印。

## 7. 测试策略(provider 相关)

- **契约测试**:对真实 API 的只读冒烟(夜间 CI,需 key);每个 wire 格式 3-5 条"能发能收"。
- **MockModel**:core 内测用 `ModelClient` 的脚本化实现(见 testing.md)——agent loop 测试不碰网络。
- **wiremock**:录制的 SSE fixture(chunk 级回放)测 codec 与重试。
- **性质测试**:partial JSON 累积器、stop_reason 映射的 round-trip。
