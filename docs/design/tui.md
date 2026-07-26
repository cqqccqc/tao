# tao-tui(终端界面)

## 1. 渲染模式:inline viewport

**不用 alternate screen**(聊天场景的错误抽象——退出后历史全无)。采用 codex 验证过的 inline viewport:

```
终端 scrollback(系统原生,可滚动/搜索/复制)
┌─────────────────────────────────────┐
│ 已完成的 HistoryCell 逐行 append     │ ← 一旦完成即"提交"给终端,不再重绘
│ …                                   │
├─────────────────────────────────────┤
│ live 区域(ratatui 管理的视口)      │ ← 唯一重绘区:进行中的 cell + composer
│ ▸ 当前助手消息(流式)               │
│ ▸ 状态行 / composer / 审批弹窗       │
└─────────────────────────────────────┘
```

- cell 完成(收到 End/Complete)→ 渲染为静态行,emit 到 scrollback,live 区域收缩。
- 收益:崩溃不丢可见历史;tmux/ssh 友好;终端原生选择复制;实现上只需管理一个小视口。

## 2. 应用结构

```
App (event loop)
 ├─► crossterm 输入流 ──┐
 ├─► core Event 流 ──────┤ select! 汇合,单线程更新状态
 ├─► 后台任务(文件监听)─┘
 └─► 帧定时器(默认 30fps 空闲 / 流式时 15fps)
     │
     ▼
ChatWidget
 ├─ history: Vec<HistoryCell>      # append-only;只有 live cell 可变
 ├─ live: Option<LiveCell>         # 当前流式消息/执行中工具
 ├─ bottom_pane: Composer/Approval/StatusLine
 └─ overlays: 命令面板/会话选择/帮助/diff 视图
```

原则:**UI 是事件流的投影**,不持有会话真相。任何状态下 core 崩了,界面可从日志重放重建(见 testing.md 的崩溃恢复测试)。

## 3. 流式渲染管线

```
AgentMessageDelta ─► MarkdownStream(增量解析)
                     ├─ 缓冲到行边界/块边界才解析(pulldown-cmark)
                     ├─ 代码块:syntect 高亮(按当前主题)
                     ├─ 帧节流:delta 只标脏,渲染在 frame tick(codex 教训:逐 delta 重渲染 CPU 爆炸)
                     └─ 输出 ratatui Text<'static>(行缓存,宽度变化才整体重排)
```

- 宽字符:CJK/emoji 用 `unicode-width` 计算;换行用 `textwrap` 的 display width 模式。
- 图片:附件在 cell 中显示占位卡片(名/大小/缩略 ASCII);`ratatui-image` 真渲染留 M6 可选。
- 推理流(Thinking)默认折叠为一行状态,`ctrl+r` 展开。

## 4. 组件清单

| 组件 | 说明 |
|---|---|
| Composer | tui-textarea 多行输入;`@`文件补全(fuzzy,fd 遍历);`#`快捷指令;历史搜索(ctrl+r);外部编辑器(`/editor` 或 ctrl+x) |
| 审批弹窗 | 命令全文/diff 预览;`y`批准 `n`拒绝 `s`会话允许 `a`中止;显示命中规则与建议 pattern |
| 状态行 | 模型/模式/上下文用量条/token 速率/git 分支/MCP 健康点 |
| 命令面板 | `/`触发,fuzzy 过滤内置+自定义命令,带 argument_hint |
| 会话浏览器 | `/sessions`:树形(fork 关系)、预览、搜索 |
| diff 视图 | `/diff`:本会话全部改动的汇总 diff(similar 生成,syntect 着色) |
| 通知 toast | 后台事件/hook 警告,右下角短暂浮现 |

## 5. 键位(默认集,可配置后置)

| 键 | 行为 |
|---|---|
| Enter / Shift+Enter | 发送 / 换行(或 `\` 续行,双保险) |
| Esc | 中断当前 turn;空输入时清输入 |
| Esc Esc | rewind 到上一 turn 边界(fork 对话) |
| Tab | 补全(@文件、/命令) |
| Shift+Tab | 权限模式循环(default→plan→accept-edits) |
| Ctrl+R / Ctrl+L | 历史搜索 / 展开推理 |
| Ctrl+C(空输入) | 退出(有内容时第一次清空) |
| PgUp/PgDn | 滚动 scrollback 视图 |

## 6. 主题与可访问性

- 主题 = TOML 文件(`~/.tao/themes/*.toml`):语义色槽位(assistant/user/tool/diff-add/diff-del/status…),内置 dark/light/high-contrast。
- syntect 主题与 TUI 主题联动;syntect 只负责代码块。
- `--no-color` / NO_COLOR 尊重;24-bit 不支持时降级 256。
- 屏幕阅读器/精简模式:`--plain` 关闭装饰字符,输出线性文本。

## 7. 性能预算

- 冷启动到可输入 < 80ms(不连 MCP 的情况下;MCP 惰性)。
- 流式渲染:P95 帧耗时 < 16ms(1 万字消息、200 行代码块);行缓存命中时 < 4ms。
- 内存:10k 行历史 < 150MB(cell 内文本共享 Arc,渲染产物可逐出)。

## 8. 与其他前端的关系

- tao-tui **只依赖** `tao-core` + `tao-protocol`;不 import 任何 core 内部模块。
- web/gui 复用同一事件流;TUI 的 cell 渲染逻辑(markdown→行)抽在 `tao-tui/render`,web 端重做 HTML 版——渲染层不共享,事件语义共享。
