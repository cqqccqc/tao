# 路线图

## 1. 里程碑总览

| 里程碑 | 目标 | 关键交付 | 规模(单人参考) |
|---|---|---|---|
| **M0 骨架** | workspace 立起来,协议类型先行 | 9 个 crate 骨架(tao-acp/tao-mcp 可空壳占位);Op/Event/LogEvent 全量定义;CI(fmt/clippy/test);`tao --help` 可用 | 1 周 |
| **M1 会说话的 loop** | 端到端单轮对话 | providers(3 个 wire codec)+ SSE + 重试;system 提示;Bash/Read/Write 三个工具;turn loop;TUI 最小可用(输入+流式文本) | 3–4 周 |
| **M2 真正的 coder** | 编码工作流闭环(✅ 完成) | ✅权限模式+规则+审批弹窗(M2-1);✅Edit/Patch/Grep/Glob(M2-2);✅AGENTS.md 指令文件(M2-3);✅事件日志+resume/fork(M2-4);✅compaction(M2-5);✅markdown 渲染(M2-6) | 4–6 周 |
| **M3 可扩展平台** | 生态机制(4/5 完成) | ✅slash(M3-1);✅hooks(M3-2);✅子agent(M3-3);✅MCP(M3-4);✅AGENTS.md(M2-3);⬜OAuth(跳过,留后续) | 3–4 周 |
| **M4 会话与协作** | 会话资产化(5/7 完成) | ✅技能(SKILL.md)(M4-1);✅tao acp(M4-2);✅shadow-git checkpoint+rollback(M4-3);✅会话浏览器(M4-5);✅share 导出(M4-6);⬜tao serve;⬜tao mcp-serve | 4–5 周 |
| **M5 硬化** | 安全与性能 | OS 沙箱(seatbelt/landlock);逃逸分析加强;性能达标(启动/渲染/内存);崩溃恢复审计;Windows 基础支持评估 | 3–4 周 |
| **M6 tao-web-ui** | 第二前端验证协议 | web 前端(消费 serve 协议);多客户端 attach;反向验证 core 无 UI 假设 | 视前端栈而定 |

每个里程碑结束 = 可发布(版本号 + CHANGELOG + 安装方式)。M2 结束即可日常自用 dogfood。

## 2. 依赖关系与并行空间

```
M0 ─► M1 ─► M2 ─► M3 ─► M4 ─► M5 ─► M6
        │      │      │
        │      │      └─ hooks 只依赖工具系统,可在 M2 后半并行
        │      └─ 日志/resume 依赖 turn loop 稳定;权限依赖工具落地
        └─ TUI 的 inline viewport 骨架可与 providers 并行
```

## 3. 风险登记册

| 风险 | 影响 | 缓解 |
|---|---|---|
| 三协议 codec 漂移(codex 的双 client 教训) | 行为不一致、维护翻倍 | 规范模型为唯一中间层;chat 适配器刻意最小;codec golden tests 门禁 |
| 上下文压缩质量(全行业痛点) | 长会话体验崩坏 | 结构性丢弃优先于摘要;摘要结构化(目标/决策/改动/待办);Compaction 事件可回放可评估 |
| patch 寻址错误改错位置 | 用户信任崩塌 | 语法/语义分离 + AST 锚定;事务性应用;失败即拒不猜测 |
| 逃逸分析误判(放行危险命令) | 安全事故 | 不可解析即升级 Ask;文档明确 v1 非安全边界;M5 OS 沙箱兜底 |
| TUI 流式渲染性能 | 卡顿感 | 帧节流 + 行缓存;性能预算进测试 |
| 范围膨胀(插件系统/server 提前) | 里程碑崩盘 | 非目标清单(§4);M4 前不碰 serve |
| 一人维护成本 | 进度不可持续 | 能抄不造;测试自动化;每个 M 都可发布可中断 |

## 4. 非目标(明确不做)

- v1 不做:Windows 完整支持(PTY/进程组/沙箱)、WASM/二进制插件、云端会话同步、团队协作服务、GUI、远程扩展市场。
- 永不(原则性):不经审批的默认全开放模式( bypass 必须显式);把用户仓库的 git 历史当作 agent 的草稿纸(aider 模式);telemetry 上传代码内容。

## 5. 发布与分发

- 包名冲突:`tao` 已被占 → crates 发布用 `tao-agent`(库)、二进制 `tao`(`cargo install tao-agent-cli` 提供);Homebrew tap `tao-agent/tao`。
- 发布产物:macOS(arm64/x86_64)、Linux(gnu/musl)预编译二进制 + 校验和;`install.sh` curl 脚本;版本号 semver,协议版本独立演进。
- dogfood:从 M2 起,tao 的开发本身用 tao 完成(每次里程碑用当版构建做一周日常开发)。

## 6. 成功度量(dogfood 期)

- 自用覆盖:一周日常编码中 ≥70% 的改动由 tao 完成。
- 审批打扰率:Ask 次数 / 工具调用数,目标随规则累积单调下降。
- 恢复率:kill -9 后 resume 成功率 = 100%(测试 + 实测)。
- 会话复用:fork/rewind 周使用次数 > 0(证明会话资产化有人用)。
