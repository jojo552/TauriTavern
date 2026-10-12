# Browser Host 当前状态

本文记录 Rust HTTP 宿主的当前实现与边界，不是部署记录或全功能验收报告。前端入口与稳定契约见 [FrontendGuide](../FrontendGuide.md) 和 [FrontendHostContract](../FrontendHostContract.md)，后端分层见 [BackendStructure](../BackendStructure.md)。

## 1. 一个应用，两种宿主边界

`tt-server` 不启动 Tauri、WebView 或显示服务器。它通过 `tt-runtime` 装配共享 `CoreServices`；HTTP command、资源与下载入口只是宿主边界，业务继续由共享 application service 和 adapter 执行，不维护另一套浏览器业务后端。

HTTP 宿主在模块执行前注入并冻结 `window.__TAURITAVERN_HOST__ = { platform: 'browser', kind: 'web' }`。这描述的是宿主，不是浏览器所在的操作系统；手机浏览器也不是原生 `mobile` 宿主。它不伪造 `__TAURI__` / `__TAURI_INTERNALS__`，`init.js` 不为 web 写入 `__TAURI_RUNNING__`。

- 第一方身份识别统一由 [host-identity.js](../../src/scripts/util/host-identity.js) 提供。`isWebHost()` 只匹配完整 `browser/web` 身份；非项目环境缺少身份时返回 `false`，其他身份读取器缺少身份时抛错。
- [bridge](../../src/tauri-bridge.js) 重导出 `isWebHost()`，按身份选择原生 invoke/listen/Channel 或浏览器 transport。
- 两种宿主复用 `init.js → tauri-main.js → bootstrapTauriMain()`、前端兼容路由、同源窗口请求 patch、扩展运行时与 `__TAURITAVERN__` API 装配。真实平台差异留在宿主能力边界，不复制路由业务。
- 缩略图、用户文件、扩展资源与用户 CSS 共用 `HostResourceService`，保持真实子资源端点、404、条件缓存与媒体 Range 语义，不通过 DOM patch 模拟资源。

启动快照的任一域读取失败会使请求失败并由前端统一显示阻断错误与重载入口；早期模块/session 初始化失败也使用这个独立于主应用的面板。不返回无区分的空数据，也不依赖服务器日志代替用户反馈。

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

文本 commit（设置、世界书、聊天、扩展存储 JSON）的字节拥有者通过 `compressible` 提供内容提示；共享 IPC 编码不做 HTTP 压缩，浏览器 [transport](../../src/web/transport.js) 在 invoke 生命周期内将 32 KiB 以上的帧经 `CompressionStream('gzip')` 压缩，以 HTTP `Content-Encoding: gzip` 发送，`/__tt/invoke` 在命令读取 body 前解压。offset、`expectedSize` 与块大小一律按解压后的字节计；64 MiB 上限同样作用于解压后的大小，超限返回 413。只接受 gzip，其他 `Content-Encoding` 返回 415，不透传。staging 上传与扩展存储 Blob 是不透明内容，传输层不猜测其压缩性，默认保持原始字节。实测（Chromium，约 2.9 MiB 设置）：gzip 后约 0.75 MiB，压缩耗时约 90 ms，4 倍 CPU 降速下约 340 ms；在 10 Mbps 上行时省下的传输时间远大于此。请求准备期间发生页面关闭、取消或 session reset 时立即拒绝该 invoke；压缩完成后的迟到帧不发送、不重放。m2 三轮回测中上传耗时明显降低，但手机模拟条件下整体 ready 中位数没有明显改善，不能把带宽收益等同启动速度收益。

原生 WebView 的 IPC 在进程内传字节，浏览器宿主则经网络传输。设置聚合（含全部预设与世界书名，实测约 90 MiB）、聊天字节与脚本都是文本，因此 [host.rs](../../src-tauri/crates/tt-server/src/host.rs) 在静态资源、Host Resource 与 invoke 响应上统一启用响应压缩：按浏览器 `Accept-Encoding` 选择 zstd 或 gzip，等级 3（在单核宿主上实测：约 110 MiB 预设聚合经 zstd 压到约 11 MiB，CPU 不高于等级 1）；不启用 Brotli（默认等级过慢）。图片、音视频、字体、ZIP、SSE 与 Range 响应不压缩；一次性归档下载路由在压缩层之外。压缩不改变响应语义，ETag/304 等条件请求照旧由 Host Resource Service 判定。

[PageBus](../../src-tauri/crates/tt-server/src/events.rs) 承载页面事件与 Channel：

- Channel 参数序列化为 `{ "$channel": "<id>" }`；ID 只属于创建它的页面，且每个 ID 只能打开一次。
- `channel` 帧带从 0 开始、严格递增的 `sequence`；序号分配、入队和结束共用 Channel 的串行边界，支持并发任务共享 sink。`channel-end` 结束该 Channel；前端拒绝未知或乱序帧。
- Channel 生命周期由后端持有它的任务/注册项决定，不在每次发起它的 invoke 返回时一律删除；实时消费者也不能因独立 HTTP 请求成功就丢弃稍后到达的帧。订阅主动取消或 Channel 结束才停止交付，同 session 短暂断线保留 channels、listeners 与资源。
- 数据帧还有页面级 SSE `id`；ready/reset 控制帧无 id，不能推进数据 cursor。PageBus 保存最多 4096 帧、总序列化 JSON 16 MiB 的滚动回放窗口，淘汰最旧帧不是永久毒化在线页面。cursor 落在窗口外、在线消费者落后或超大单帧时明确 reset；字节上限不是上游 Value 构造的瞬时内存上限。
- SSE 断线后，transport 关闭旧 EventSource，以 1 秒到最多 10 秒退避显式重开同页 SSE，从最后已派发 `id` 继续，不重新执行已经派发的异步回调。visibility 恢复、online、BFCache pageshow 与真实 HTTP 网络失联共用这一恢复路径，不无条件拆除健康连接；单个定时器负责退避或 90 秒静默检查，旧 source/epoch/定时器回调都有 fence。服务端约每 20 秒发无 id 的 `{type:'heartbeat',sessionId}`，不入 PageBus、不推进数据 cursor；浏览器挂起时不能保证按墙钟准时检测。初始化和新 session GET 的明确网络失败也沿此退避等待，认证、有效 CommandError 和协议错误仍终止并传播。可恢复断线和自动 session 重建静默进行，不弹通用连接警告；只有 transport 进入 failed、自动恢复已停止时，持久提示“无法自动恢复连接，请手动刷新页面重试”。具体操作的保存结果未知、资源失效等错误仍由调用方独立报告，不因静默重连而被吞掉。
- SSE 本身断开不强制中止仍可得到成功响应的 HTTP 保存。实际 HTTP 中断只报告结果未知，绝不自动重放 invoke；一次性 commit 的 offset 与最终确认规则保持不变。
- 合法 owner 的 session 消失（服务器重启/过期）或回放 gap，服务器返回无 id `{type:'session-reset', reason:'expired'|'gap'}`，结束旧页。前端创建新 session/CSRF，失败的旧请求不重放；后续独立请求可继续。错误 origin/cookie 仍拒绝，不能伪装为 reset。

页面到期或确定不能恢复时，清理页面所属的临时 AI 请求、extension tools、reader 与未完成 commit，已发布数据不撤销；可恢复的浏览器 completion job 仅撤销旧页附着，不取消模型任务（见下节）。两个宿主共用 `tt-adapter-extension::tools::AgentExtensionTools` 的页面工具 registry / receipt runtime；浏览器 `PageChannel` 与原生 WebView Channel 各自实现 `ExtensionToolEventSink`，宿主只提供 transport 与触发页面清理，`tt-runtime` 只装配注入的 `ExtensionTools` port。同一工具 ID 可由多个打开的页面各自注册（同一页面重复注册仍报冲突），调用交给最近注册的存活页面，页面关闭后由次新的页面接替；Agent run 不属于某个页面，因此不按发起页路由。新 session 使旧文件资源失效：浏览器 numeric handles 单调分配并映射 server ids，旧句柄不能读/关新页同号资源；迟到旧响应也不得登记到新页。冷滑动 projection 不改存储内容，引用按 epoch 校验，缺失滑动记录必须重新打开聊天，不能删 marker 后按完整 payload 保存。Agent 工具仅重建已经确认的声明式注册/启用状态，旧 pending callback 取消；不自动恢复旧 generation、live subscription 或 commit。

### 3.1 普通生成的恢复边界

页面 SSE 的恢复与模型任务恢复不是同一概念。[BrowserAiJobs](../../src-tauri/crates/tt-server/src/ai_jobs.rs) 只拥有 Cookie 归属、页面附着和保留期；模型执行复用共享 ChatCompletionService，不复制生成或聊天保存业务。持久聊天的 Chat Completion `normal`/`regenerate` 请求先确认 provider-barrier 基线，预分配 jobId 后启动一次；流/非流任务均独立于 HTTP 和页面 lifetime。start 响应丢失仅查询同 ID，绝不重发模型请求。临时聊天、quiet、JSON-schema 等仍沿原管线；临时流可短断网同 cursor 读取，但不承诺关闭页后恢复。原生 stream 默认 ACK 淘汰规则不变。

- 宿主最多保留 16 个未确认 job，创建起固定 2 小时到期，由后台每分钟回收并取消尚在运行的任务。流保留完整序列化回放，最多 16 MiB/8192 帧，超限显式失败，不滚动丢失历史；非流结果最多 16 MiB，presentation/error 最多 32 KiB。这些是保留数据预算，不是进程瞬时总内存保证。
- 每个任务同一时刻仅附着一个页面，重复同页 attach 幂等；page bus 关闭立即撤销旧 reader lease，迟到 read/ACK 不得命中新附着。页面重建可声明 attach 后同 cursor 继续安全查询，命令和写请求本身不自动重放。
- 重新打开聊天时，以目标、原始全消息内容摘要（克隆后水合冷 swipe，包含隐藏历史）和追加位置核对基线，再把旧响应送入同一 provider decoder 与 StreamingProcessor 完成/保存管线，不重入 Generate 的命令、删消息、提示词构建或模型 start。原 speaker/provider/model/reasoning/signature 保留。聊天被修改或已有无法证明来源的已保存 partial 时停止自动恢复，保留结果并提示；提示可经用户确认取消旧 job，不猜测覆盖。内容摘要不是跨页面并发写入的存储 CAS。
- 恢复不重发历史 STREAM_TOKEN_RECEIVED，不自动执行工具、auto-swipe 或 auto-continue。正常首次生成原行为保留；工具轮已保存的 transcript 确认后才释放上一 job。恢复遇工具结果保留任务并要求用户处理，不把断链恢复当成副作用续跑。
- 最终消息携带 `extra.tauritavern.browserGeneration={jobId,completed:true}`，沿既有 final events → chat save → job ACK 完成。再次发现完成标记会通过既有聊天 history API 核对实际落盘标记后才确认释放，不重复应用回复或事件；CHAT_CHANGED 可能只是同页内容刷新，既不证明保存成功，也不无条件中止同聊天的附着。事件到保存之间崩溃仍可能使扩展完成事件重复，不能声称跨崩溃 exactly-once；聊天保存失败/拒绝覆盖不得 ACK。任务不会独立自动写聊天，关闭页后的结果等重开、核对并保存后才发布。
- 浏览器仅持久记录 jobId/chatRef 发现索引，不记录提示词、聊天全文或凭据。服务器进程重启销毁内存任务，旧索引让失效可见；不自动重新生成。Agent run/checkpoint/live UI 的生命周期与此独立，不能由普通生成恢复推出 Agent 自动续跑。

## 4. 文件传输与隔离边界

### 4.1 浏览器选择与 staging

[file-transfer.js](../../src/web/file-transfer.js) 在用户激活内打开 DOM 文件输入框，返回标准 `File[]`；取消返回 `null`，失败或中止拒绝 Promise。角色卡、skill 与数据归档选择复用该入口；`api.characterCards.isNativePickerAvailable()` 的旧名称因兼容保留，在浏览器为 `true` 不意味着存在原生 picker。

浏览器上传不将用户本机磁盘路径直接透传给 Rust。共享上传服务将 `File` / `Blob` 交给 staging：

- `stage_file_begin` 分配 UUID 文件并返回 `file_path` 与块大小；头像、用户头像、世界书导入为 512 KiB，其他 kind 当前为 8 MiB。
- `stage_file_chunk` 写入原始 bytes，offset 必须等于当前文件长度；重复或乱序块失败，不能盲目重放。
- `stage_file_finish` 校验最终长度；两宿主的 append/finish/discard 复用 `tt-adapter-storage-core::staged_file`，同一 canonical 文件的 offset 检查与追加、最终长度检查、删除在同一串行边界中执行，不重复维护宿主 IO。消费后的暂存文件由调用链负责清理，失败路径尝试 discard。

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

设置页与扩展快捷菜单共同消费宿主能力投影；浏览器不会呈现可点击的原生同步或 debug bundle 入口，已有保存的快捷入口也按当前能力过滤。ABI 对不可用命令仍显式返回错误，不伪装功能已经实现。

| 能力 | 浏览器宿主当前状态 |
| --- | --- |
| SillyTavern 纯前端扩展、共享 API 与资源端点 | 复用现有前端/共享服务；不是任意第三方扩展全部兼容的保证。Node 后端插件不支持。 |
| TT LAN / TT-Sync / 同步自动化 | 当前 HTTP command dispatch 未接 `lan_sync_*`、`tt_sync_*`、`sync_*` 命令；原生面板仍依赖 Tauri invoke/event，不能因共享库存在就宣称浏览器可用。 |
| 前端日志 | 本地 `api.dev.frontendLogs` 与捕获设置可用；`devlog_append_frontend_logs` 将页面日志镜像到服务端 tracing。 |
| 后端日志与 LLM API 日志 | 与原生共用 `tt-adapter-observability` 的日志存储：后端 tracing 环形缓冲、LLM 请求记录装饰器与磁盘日志（位于 app root 下私有 `logs` 目录，与 `security` 同样拒绝与静态/数据/资源目录重叠，不被提供或归档）。tail/index/preview/raw 经共同 invoke 读取；实时流由页面声明启用，服务端为该页订阅共享广播并经其 SSE 推送，关闭面板或页面即取消，不设全局开关。订阅落后于广播容量时沿用 SSE 的 gap 协议使页面会话重置，不静默跳过条目；重置后前端重新声明仍在使用的订阅，断开期间的条目以 tail/index 补看。debug bundle 导出未接 HTTP host。 |
| 后端用户错误主动提示 | 共用原生 `ERROR + tauritavern::user_error` 的显式选择和既有 backend-error bridge/toastr；不依赖日志面板开关。密码保护的单用户宿主向已注册认证页面投递原始 message，不附带 structured fields，不声称多租户隔离或脱敏。普通 error/warn 与 HTTP invoke 返回失败不自动升级为 toast。队列和错误风暴边界见下文。 |
| 内容视口布局 | `api.layout.snapshot()/subscribe()` 照常安装，基于 `visualViewport` / `innerWidth/innerHeight`；不安装原生窗口快照、窗口背景发布或移动端原生 IME glue。 |
| 剪贴板与外链 | 使用 DOM Clipboard API 与浏览器窗口/导航规则，受安全上下文、权限、用户激活和弹窗阻止约束；不调用原生 clipboard/opener。 |
| Tauri plugin / shell 能力 | 不模拟 Tauri runtime，不支持 `plugin:*` invoke；原生数据目录选择、系统分享、托盘与原生窗口控制不能视为浏览器能力。 |

后端错误通知由 [backend_errors.rs](../../src-tauri/crates/tt-server/src/backend_errors.rs) 拥有页面 pending/ready；[host.rs](../../src-tauri/crates/tt-server/src/host.rs) 在认证 session 创建时注册弱 PageBus 引用、在关闭/到期/gap 时注销。共同前端先 listen 再 ready；浏览器 ready 把首批错误入页面 SSE 并返回空数组，重复 ready 幂等，新 session 再声明，短断线只回放。首次页面 ready 前有最多 50 条 startup backlog，首个 ready 后清空；后续新页面不补播历史，没有页面时不积累离线通知。

每页 pending 最多 50 条，重复合并、溢出淘汰最旧消息并报告至少省略数量；摘要占一席。原文最多 2048 UTF-8 字节，超出时安全截断并提示查日志。首次 ready 最多交付 4 帧，之后每秒最多一批 4 帧，错过周期不追赶；不调用 log pump 的 `fail_gap`，避免仅因错误广播溢出就失效 AI/文件句柄。其他 SSE 数据仍适用共同回放窗口/gap 契约。前端消费者未就绪时仍有原有 50 条队列，真实 UI 用纯文本 toastr 和约 1.2 秒相同消息去重。通知不是历史存储：新 session/关闭页会丢弃尚未进 SSE 的旧 pending，已入 SSE 的短断线通知按 cursor 回放；日志面板可查看完整详情。这不是系统通知或 Web Push。

聊天区尺寸变化共用 [RossAscends-mods.js](../../src/scripts/RossAscends-mods.js) 的输入框自适应与 resize 处理：消息视图保留既有底部距离补偿，欢迎页则保持阅读位置，不因手机视口变矮被主动推下。依据当前 direct-child `.welcomePanel` 视图判断，不用持久聊天 ID 排除合法的临时聊天；resize 高度基线仍在每次回调更新。此规则同时适用于原生与浏览器宿主。

不可用或拼错的 HTTP command 明确返回错误，不静默降级为成功。后续接入能力应复用共享服务，并保留真实宿主差异；测试结果应注明验证范围，不能由 command 已接线推导全平台通过。

用户文件 upload/delete/verify 由共享 `UserMediaService` 与文件 adapter 执行，业务验证规则不在两个 host command 各维护一份；写入以同步暂存文件后原子发布为边界，不直接截断旧文件。Agent 持久状态 prune、角色重命名/删除的关联 Skill 迁移，以及预设删除的关联清理同样由已有应用用例负责。

启动时 canonicalize app/data/web/resource 根并验证实际 `security`、`staging`、`.data-archive` 私有目录不会被静态提供或作为用户数据归档；相对 app 根会规范化为绝对路径，`app=/state,data=/state/data` 的安全嵌套仍合法。该校验是宿主配置边界，不是对本机管理员动态替换目录的 sandbox。

## 6. 构建与发布产物

浏览器宿主只在 GitHub 托管 runner 上构建：[web-server-build.yml](../../.github/workflows/web-server-build.yml) 并行运行完整 `pnpm run check` 与 Debian bookworm 容器内（与 glibc 2.36 的部署机一致）的构建，后者执行 `pnpm run web:build` 与 `cargo build --locked -p tt-server --profile server`，由 [package-web-server.mjs](../../scripts/package-web-server.mjs) 组装发布目录。打包脚本从 rspack 配置推导全部前端产物，连同二进制在删除旧输出前逐一检查，缺任何一项即失败。

构建任务的输出带 `unverified-` 前缀、只保留 1 天；检查与构建都成功后，发布任务校验并重新上传为 `tt-server-linux-x64-<sha12>.tar.gz` 及其 SHA-256，只有这个名字的产物可以部署。仅改动文档或 Markdown 的推送不触发构建。包内含 `SOURCE_COMMIT` 与二进制校验和。打包前在 runner 上启动实际二进制，只使用生成的临时数据；验证二进制上报 revision/branch 与包身份一致、认证/SSE ready/gzip frame 协议，以及连续超过八次错误 finish 仍可 abort 回收提交。该验收不连接生产、不上传私有数据；格式检查只读，CI 不会改写 checkout 再掩盖失败。部署机只用 `gh run download` 取得产物、校验后原子替换发布目录并保留上一版本，不安装编译工具链，也不接触 CI。工作流只有 `contents: read` 权限，不持有部署凭据或用户数据。
