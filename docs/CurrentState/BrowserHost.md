# Browser Host 当前状态

本文记录 Rust HTTP 宿主的当前实现与边界，不是部署记录或全功能验收报告。前端入口与稳定契约见 [FrontendGuide](../FrontendGuide.md) 和 [FrontendHostContract](../FrontendHostContract.md)，后端分层见 [BackendStructure](../BackendStructure.md)。

## 1. 一个应用，两种宿主边界

`tt-server` 不启动 Tauri、WebView 或显示服务器。它通过 `tt-runtime` 装配共享 `CoreServices`；HTTP command、资源与下载入口只是宿主边界，业务继续由共享 application service 和 adapter 执行，不维护另一套浏览器业务后端。

HTTP 宿主在模块执行前注入并冻结 `window.__TAURITAVERN_HOST__ = { platform: 'browser', kind: 'web' }`。这描述的是宿主，不是浏览器所在的操作系统；手机浏览器也不是原生 `mobile` 宿主。它不伪造 `__TAURI__` / `__TAURI_INTERNALS__`，`init.js` 不为 web 写入 `__TAURI_RUNNING__`。

- 第一方身份识别统一由 [host-identity.js](../../src/scripts/util/host-identity.js) 提供。`isWebHost()` 只匹配完整 `browser/web` 身份；非项目环境缺少身份时返回 `false`，其他身份读取器缺少身份时抛错。
- [bridge](../../src/tauri-bridge.js) 重导出 `isWebHost()`，按身份选择原生 invoke/listen/Channel 或浏览器 transport。
- 两种宿主复用 `init.js → tauri-main.js → bootstrapTauriMain()`、前端兼容路由、同源窗口请求 patch、扩展运行时与 `__TAURITAVERN__` API 装配。真实平台差异留在宿主能力边界，不复制路由业务。
- 缩略图、用户文件、扩展资源与用户 CSS 共用 `HostResourceService`，保持真实子资源端点、404、条件缓存与媒体 Range 语义，不通过 DOM patch 模拟资源。

## 2. 认证、页面 session 与 CSRF

[host.rs](../../src-tauri/crates/tt-server/src/host.rs) 的全部 HTTP 路由，包括静态资源与下载，都经过 Basic 认证。[security.rs](../../src-tauri/crates/tt-server/src/security.rs) 校验配置的精确 Host；页面 session、invoke、SSE 与下载还会拒绝不匹配的 Origin / 跨站 Fetch Metadata，不信任 forwarded host/origin。

1. [transport](../../src/web/transport.js) 以同源凭据请求 `GET /__tt/session`，取得随机 `sessionId` 与 `csrfToken`；响应为 `no-store`。
2. 服务端用 `tt_browser` Cookie 绑定浏览器：`HttpOnly; SameSite=Strict; Path=/`，HTTPS origin 再加 `Secure`。同一 Cookie 下各页面仍分别创建 session。
3. `GET /__tt/events?sessionId=...` 校验 session 属于该 Cookie；同页新的 SSE 接入会替代旧流。transport 收到匹配 session 的首个 `ready` 帧后才允许 invoke 正常开始。断线重开带 `after` cursor；原生 `Last-Event-ID` 也进入同一校验入口，同时提供的 cursor 必须一致。
4. `POST /__tt/invoke` 的 JSON envelope 为 `{ sessionId, command, args }`，每次 invoke 都携带并校验 `x-tt-csrf`，不仅是写命令。共享 `/csrf-token` 路由返回这个真实 token；原生 Tauri 的兼容 token 不能用于 HTTP host。

当前 session 注册上限为 64；创建后 120 秒仍未建立 SSE 的 session 由到期清理处理。建立过 SSE 的页面在最后一条流结束后有 10 分钟恢复期，短暂切后台或网络变更不立即销毁资源；超过期限不能通过迟到重连复活。流认领与到期回收共享 Security 的原子生命周期入口。真正离开页面（非 BFCache 的 pagehide）发出带 CSRF 的 keepalive `POST /__tt/session/close`，立即释放该页的工具和资源；临时切后台没有 pagehide，不进入此清理。Cookie、页面 session 与资源句柄是不同生命周期，不应混为一个全局 session。

## 3. HTTP bytes、SSE 与 Channel

普通成功响应保留 JSON 值，`204` 转为 `null`；`application/octet-stream` 响应转为 `Uint8Array`。JSON 错误保留 Rust `CommandError` 及结构化详情，不转换成伪成功值。

二进制 invoke 使用原始字节 body，`Content-Type: application/octet-stream`，并由 `x-tt-command` / `x-tt-session` 指定命令和页面。transport 自己管理这些 header 及 `x-tt-csrf`，调用方不得覆盖。staging 与 commit chunk 的 offset 等参数仍走各自 header；浏览器明确拒绝 `chunk-encoding`，不使用 base64 字节回退。单次 invoke body 当前上限为 64 MiB，实际块大小由对应协议返回。

原生 WebView 的 IPC 在进程内传字节，浏览器宿主则经网络传输。设置聚合（含全部预设与世界书名，实测约 90 MiB）、聊天字节与脚本都是文本，因此 [host.rs](../../src-tauri/crates/tt-server/src/host.rs) 在静态资源、Host Resource 与 invoke 响应上统一启用响应压缩：按浏览器 `Accept-Encoding` 选择 zstd 或 gzip，等级 3（在单核宿主上实测：约 110 MiB 预设聚合经 zstd 压到约 11 MiB，CPU 不高于等级 1）；不启用 Brotli（默认等级过慢）。图片、音视频、字体、ZIP、SSE 与 Range 响应不压缩；一次性归档下载路由在压缩层之外。压缩不改变响应语义，ETag/304 等条件请求照旧由 Host Resource Service 判定。

[PageBus](../../src-tauri/crates/tt-server/src/events.rs) 承载页面事件与 Channel：

- Channel 参数序列化为 `{ "$channel": "<id>" }`；ID 只属于创建它的页面，且每个 ID 只能打开一次。
- `channel` 帧带从 0 开始、严格递增的 `sequence`，`channel-end` 结束该 Channel；前端拒绝未知或乱序帧。
- Channel 生命周期由后端持有它的任务/注册项决定，不在每次发起它的 invoke 返回时一律删除；同 session 短暂断线保留 channels、listeners 与资源。
- 数据帧还有页面级 SSE `id`；ready/reset 控制帧无 id，不能推进数据 cursor。PageBus 保存最多 4096 帧、总序列化 JSON 16 MiB 的滚动回放窗口，淘汰最旧帧不是永久毒化在线页面。cursor 落在窗口外、在线消费者落后或超大单帧时明确 reset；字节上限不是上游 Value 构造的瞬时内存上限。
- SSE 断线后，transport 关闭旧 EventSource，以 1 秒到最多 10 秒退避显式重开同页 SSE，从最后已派发 `id` 继续，不重新执行已经派发的异步回调。只有这一条前端恢复路径；旧 source 的迟到回调被 identity/epoch fence 拒绝。重连状态向用户可见。
- SSE 本身断开不强制中止仍可得到成功响应的 HTTP 保存。实际 HTTP 中断只报告结果未知，绝不自动重放 invoke；一次性 commit 的 offset 与最终确认规则保持不变。
- 合法 owner 的 session 消失（服务器重启/过期）或回放 gap，服务器返回无 id `{type:'session-reset', reason:'expired'|'gap'}`，结束旧页。前端创建新 session/CSRF，失败的旧请求不重放；后续独立请求可继续。错误 origin/cookie 仍拒绝，不能伪装为 reset。

页面到期或确定不能恢复时，清理 AI、extension tools、reader 与未完成 commit，已发布数据不撤销。新 session 使旧文件资源失效：浏览器 numeric handles 单调分配并映射 server ids，旧句柄不能读/关新页同号资源；迟到旧响应也不得登记到新页。冷滑动 projection 不改存储内容，引用按 epoch 校验，缺失滑动记录必须重新打开聊天，不能删 marker 后按完整 payload 保存。Agent 工具仅重建已经确认的声明式注册/启用状态，旧 pending callback 取消；不自动恢复旧 generation、live subscription 或 commit。

## 4. 文件传输与隔离边界

### 4.1 浏览器选择与 staging

[file-transfer.js](../../src/web/file-transfer.js) 在用户激活内打开 DOM 文件输入框，返回标准 `File[]`；取消返回 `null`，失败或中止拒绝 Promise。角色卡、skill 与数据归档选择复用该入口；`api.characterCards.isNativePickerAvailable()` 的旧名称因兼容保留，在浏览器为 `true` 不意味着存在原生 picker。

浏览器上传不将用户本机磁盘路径直接透传给 Rust。共享上传服务将 `File` / `Blob` 交给 staging：

- `stage_file_begin` 分配 UUID 文件并返回 `file_path` 与块大小；头像、用户头像、世界书导入为 512 KiB，其他 kind 当前为 8 MiB。
- `stage_file_chunk` 写入原始 bytes，offset 必须等于当前文件长度；重复或乱序块失败，不能盲目重放。
- `stage_file_finish` 校验最终长度；消费后的暂存文件由调用链负责清理，失败路径尝试 discard。

`file_path` 当前确实是服务端绝对暂存路径；前端应当把它当作宿主发放的暂存引用，不作为任意文件访问入口。[HostFiles](../../src-tauri/crates/tt-server/src/files.rs) 验证 kind/extension，并用 canonical 路径约束到宿主 staging 根。

**当前 staging 只有宿主根目录 confinement，没有页面所有权隔离。** `HostFiles` 是 host-global，没有按页面维护 staging owner 表；不能声称某页面无法消费同一根内由另一页面创建的路径，也不能声称 SSE 关闭会自动删除该页面所有 staging。与此不同，[SessionResources](../../src-tauri/crates/tt-server/src/resources.rs) 的 reader、cold swipe source 与 commit ID，以及 `PageBus` 的 Channel，确实按页面归属校验并随页面关闭清理。以上隔离不意味着多页面使用独立业务数据库；共享应用数据仍由共享服务持有。

[media commands](../../src-tauri/crates/tt-server/src/commands/media.rs) 的 `upload_sprite` 与 `upload_sprite_pack` 均先验证 staging 路径，再分别调用共享 `sprite_service.upload()` / `upload_pack()`。两项已在 nc 的真实 Chromium 测试副本通过单图、ZIP 导入、列表与资源字节读取、拒绝暂存根外路径及测试数据清理；此结果只覆盖这些场景，不代表整体扩展兼容或所有浏览器验收通过。

### 4.2 下载 handoff 与一次性归档

浏览器不安装原生 `download-bridge` 的链接/图片拦截，保留浏览器自己的下载与保存图片行为。前端 Blob 导出使用 `a[download]`，返回 `{ status: 'handed-off' }`：只确认已交给浏览器，无法观察用户保存、取消或磁盘写入成功。Blob URL 的延迟回收只是资源清理，不是完成检测。

数据归档导出和用户备份复用共享归档服务；[archive commands](../../src-tauri/crates/tt-server/src/commands/archives.rs) 将宿主产物注册为 `/__tt/download/<token>`，返回同源 URL 与文件名，不把下载磁盘路径交给浏览器。

- [download registry](../../src-tauri/crates/tt-server/src/downloads.rs) 的 token 绑定 `tt_browser` Cookie，**不是页面 session 绑定**；最多 32 个待下载产物，当前有效期为 10 分钟。
- 匹配浏览器首次取用即消费 token；token 不存在、Cookie 不匹配、过期或已用返回 404，登记文件已丢失返回 410。路由仍须通过 Basic 认证，未认证为 401。不能将一次性 URL 作为可重复读取或断点恢复入口。
- 响应使用 `Content-Disposition` 和 `no-store`；响应 body 结束、失败或中止后删除宿主产物，未取用产物到期后清理。服务端交付与清理不代表浏览器已经保存到本机磁盘。
- `downloadUrl()` 只接受无 URL 凭据的同源 HTTP(S) URL；浏览器远程导出也只接受同源内容。跨域导出必须另行由宿主生成下载产物，不能靠原生插件或任意服务端路径绕过。

原生暂存与系统交付链路保持原有语义，见 [FileTransfer](FileTransfer.md)；不要把原生 `{ delivered }` 语义套到浏览器 handoff。

## 5. 当前能力边界

| 能力 | 浏览器宿主当前状态 |
| --- | --- |
| SillyTavern 纯前端扩展、共享 API 与资源端点 | 复用现有前端/共享服务；不是任意第三方扩展全部兼容的保证。Node 后端插件不支持。 |
| TT LAN / TT-Sync / 同步自动化 | 当前 HTTP command dispatch 未接 `lan_sync_*`、`tt_sync_*`、`sync_*` 命令；原生面板仍依赖 Tauri invoke/event，不能因共享库存在就宣称浏览器可用。 |
| 前端日志 | 本地 `api.dev.frontendLogs` 与捕获设置可用；`devlog_append_frontend_logs` 将页面日志镜像到服务端 tracing。 |
| 原生 Dev 日志能力 | backend tail/stream、LLM API log 查询/stream 与 debug bundle 导出未接 HTTP host；`api.dev` 对象存在不代表这些调用可用。服务端后台错误当前留在日志，未接原生 user-error toast hub。 |
| 内容视口布局 | `api.layout.snapshot()/subscribe()` 照常安装，基于 `visualViewport` / `innerWidth/innerHeight`；不安装原生窗口快照、窗口背景发布或移动端原生 IME glue。 |
| 剪贴板与外链 | 使用 DOM Clipboard API 与浏览器窗口/导航规则，受安全上下文、权限、用户激活和弹窗阻止约束；不调用原生 clipboard/opener。 |
| Tauri plugin / shell 能力 | 不模拟 Tauri runtime，不支持 `plugin:*` invoke；原生数据目录选择、系统分享、托盘与原生窗口控制不能视为浏览器能力。 |

不可用或拼错的 HTTP command 明确返回错误，不静默降级为成功。后续接入能力应复用共享服务，并保留真实宿主差异；测试结果应注明验证范围，不能由 command 已接线推导全平台通过。
