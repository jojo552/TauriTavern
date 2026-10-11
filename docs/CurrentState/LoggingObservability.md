# Logging / Dev Observability 当前状态

本文记录当前已经落地的日志与开发观测链路。`.cache/PlanDocs/RustCompileAcceleratingPlan/LoggingRefactorPlan/` 是历史分析与重构计划，不是当前实现的唯一事实来源。

## 1. 普通日志链路

Rust 普通日志统一使用 `tracing::{debug, info, warn, error}` 宏，不再经过 `infrastructure::logging::logger` facade。原生在 `src-tauri/crates/tauritavern/src/infrastructure/logging/tracing_runtime.rs`、浏览器在 `tt-server/src/main.rs` 安装全局 subscriber。两宿主复用 `tt-adapter-observability::user_errors::BackendUserErrorLayer` 的用户错误选择，不维护两套 target/visitor 规则：

- 全局 `EnvFilter` 控制所有 tracing event；
- stdout 与 rolling file 记录普通日志；
- backend log layer 写入 Dev 面板使用的 backend log store；
- user-error layer 只观察 `ERROR + target = tauritavern::user_error`。

`infrastructure::logging::logger.rs` 已删除。不要重新引入跨层 logger facade，也不要给 application service 注入 Logger trait；普通 instrumentation 直接用 `tracing`。

## 2. 用户可见错误

backend error toast 不是普通 error 日志的副作用，而是显式产品事件：

- command boundary 失败通过 `presentation/commands/helpers.rs::map_command_error()` 统一记录；
- `Cancelled` / `TooManyRequests` 等预期失败只记录 command warn，不触发 toast；
- 无法通过 `Result` 自然返回、但用户应立即感知的重要后台/宿主失败，显式使用 user-visible error target；host crate 使用 `target: crate::observability_targets::USER_VISIBLE_ERROR`，adapter crate 使用 `target: tt_contracts::observability::USER_VISIBLE_ERROR`；
- 普通诊断 `tracing::error!` 只进入 stdout/file/backend log，不自动弹窗。

应用遵循全局 `EnvFilter`。如果用户过滤掉 `tauritavern::user_error` target，对应 backend error toast 也会被过滤；这是用户对观测输出的合理控制。

## 3. 前端桥接

共同 user-error layer 只取非空 `message`，不附 tracing fields。事件名与 `{ message }` DTO 在 `tt-contracts::observability`，选择规则在共享 adapter，排队、就绪和投递在各宿主：

- 原生：`app/backend_errors.rs::BackendErrorHub` 保持原有最多 50 条 early backlog、ready command 返回并 drain 字符串、之后直接 emit 的行为。
- 浏览器：`tt-server/src/backend_errors.rs::BackendErrorHub` 面向密码保护的单用户服务，将同一显式消息交给各认证页面；原文可能含用户自己的数据名称或路径，这不是公开脱敏 DTO。Startup backlog 在首个页面 ready 前可用；之后新页面不补播旧历史，无页面时不积累离线通知。每页有独立 pending/ready，registry 仅持有弱 PageBus 引用，Security 仍拥有页面生命周期。详见 [BrowserHost](BrowserHost.md)。

共同前端 `src/tauri/main/bootstrap/backend-error-bridge.js` 先 listen `tauritavern-backend-error` 再 invoke `backend_error_bridge_ready`。浏览器 ready 在同一锁内将首批 pending 放入既有页面 SSE，返回空数组而非 HTTP backlog；剩余有界 pending 周期投递。重复 ready 幂等，避免 HTTP ready 响应丢失造成首批消息丢失。新 session 重新声明 ready，同页短断线不重新监听或 drain。

首次 ready 失败清理 listener/reset 注册并让初始化 fail-fast；新 session 的 ready 失败由 transport 现有错误报告显示，不自动重放未知 invoke。前端消费者未安装时仍使用既有最多 50 条队列；`src/script.js` 消费 `tauritavern:backend-error`，按约 1.2 秒窗口去重并以纯文本 toastr 显示，即使扩展改了全局 toastr HTML 选项也不执行错误文案中的 HTML。

浏览器 pending 最多 50 条（省略摘要也占其中一席），重复排队消息合并，溢出淘汰最旧项并显示计数摘要；单条原文最多 2048 UTF-8 字节，超出时安全截断并提示查日志。首次 ready 最多投递 4 帧，之后每秒最多一批 4 帧，错过周期不追赶，不照搬 Dev log 的 Lagged→gap/reset。正常同页的 pending/live 与 ready 在同一串行边界交接；通知不是持久记录，新 session/关闭页会丢弃尚未入 SSE 的旧 pending，已经入 SSE 的短断线消息沿现有 cursor 回放。完整细节仍保留在后端日志。

不把 HTTP invoke 的所有失败再映射一次为用户错误。共享 preset 清理失败原先同时显式 toast 与返回 Err，现只保留诊断与 Result，由调用者通知；其他显式部分结果/恢复警告仍遵循原生语义，后续主操作另有失败时可能同时得到请求错误，不承诺跨不同文案去重。

## 4. Dev Observability

后端日志缓冲（`devtools::BackendLogStore` 及其 tracing layer）、LLM API 日志存储（`llm_api_logs::LlmApiLogStore`）与请求记录装饰器（`LoggingChatCompletionRepository`）位于 Tauri-free 的 `tt-adapter-observability`，原生与浏览器宿主共用同一实现。store 只负责保存与广播新条目，不持有“是否推送”的全局开关；每个宿主决定交付方式：

- 原生：`app/dev_observability.rs::DevObservabilityHub` 暴露给 presentation commands，前端启用流时订阅广播并以 Tauri 事件 `tauritavern-backend-log` / `tauritavern-llm-api-log` 推送，落后于广播容量时记录 warn（面板仍可用 tail/index 补看）；`dev_logging_commands.rs` 和 `settings_commands.rs` 不直接接触 store。
- 浏览器：`tt-server` 在页面声明启用时为该页订阅广播，经页面 SSE 推送同名事件；页面关闭或停用即取消。见 [BrowserHost](BrowserHost.md)。

LLM API logs 由 `LoggingChatCompletionRepository` 记录 raw/readable 请求响应。JSON 请求及非流式 JSON 响应写入前会移除内部 provider state，并把 data URL、Claude base64 source、Gemini inline data 中的媒体内容替换为只含 MIME 与编码长度的占位符；脱敏只作用于日志副本，不修改出站 payload。持久化失败是诊断日志，不触发 backend error toast。

## 5. 持续开发守卫

`pnpm run check` 会运行 `pnpm run check:logging-boundaries`，防止以下旧边界回潮：

- `infrastructure::logging::logger` facade 引用；
- `logger::{debug,info,warn,error}` 调用；
- application/presentation 重新依赖 logging infrastructure；
- dev logging commands 直连 infrastructure；
- settings commands 直连 `LlmApiLogStore`。

涉及日志/可观测性改动时，至少运行：

```bash
pnpm run check:logging-boundaries
cargo test --manifest-path src-tauri/Cargo.toml --no-default-features
```
