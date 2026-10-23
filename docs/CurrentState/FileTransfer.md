# 文件导入与导出

宿主提供两个能力：**选取**用户文件，以及**交付**已经生成完毕的文件。功能代码提供用途、内容和建议文件名。原生宿主的系统面板、URI 授权与文件复制集中在 `platform/file_transfer`；浏览器的选取与下载集中在 `src/web/file-transfer.js`，服务器产物经一次性下载链接交接。两者共用调用契约，不伪装相同的保存保证。

## 1. 规则

- **谁产出文件，谁发起交付。** Rust 产出的文件由对应命令交付；原生前端交付先暂存内容（WebView 字节，或宿主下载的远程资源），再调用 `deliver_staged_file`。浏览器前端 Blob 直接交给浏览器下载，不为下载而上传回服务器。
- **交付消费宿主源文件。** 原生保存、取消或失败都会结束本次产物；服务器下载在响应结束或过期时删除产物。浏览器 Blob 的 object URL 延迟释放，不用定时器猜测保存完成。
- **交付结果只表达宿主可确认的事实。** 原生 `{ delivered }` 完成目标写入或系统交接后为 `true`，取消为 `false`，失败抛错；普通文件同步落盘后才算完成。浏览器返回 `{ status: 'handed-off' }`，不能观察用户最终保存或取消。统一反馈区分“已交给浏览器”与“导出完成”，不能把交接伪装成 `{ delivered: true }`。
- **选取按用途过滤。** 角色卡、Skill 归档、数据归档分别映射为各平台的过滤条件；文件内容由导入方校验。

## 2. 平台实现

| 平台 | 选取 | 交付 |
| --- | --- | --- |
| Windows / macOS / Linux | dialog 插件返回路径，复制进暂存；数据归档原地使用 | 保存面板；同卷时移动，跨卷时复制 |
| Android / OpenHarmony | dialog 插件返回带授权的 URI，通过 fs 插件打开 fd 后复制进暂存 | 保存面板授权目标 URI，通过 fd 写入 |
| iOS | Document Picker 生成副本，移动进暂存 | Share Sheet |
| Browser | 用户手势内打开 file input，返回原始 File；取消返回 null | Blob object URL 或同源宿主下载链接交给 `a[download]` |

Android 的 content URI 不带文件名，由 `ContentUriPlugin` 查询 `DISPLAY_NAME` 得到。

Android 与鸿蒙的保存面板只接收文件扩展名，类型由系统按自己的表转换；系统不认识的扩展名不声明具体类型。保存面板会按类型推导后缀，自行指定的类型可能与系统的表不一致，例如 ColorOS 会把 `application/octet-stream` 的 `.jsonl` 存成 `.bin`。

## 3. 暂存

| 暂存 | 位置 | 内容 |
| --- | --- | --- |
| 通用 | `app_cache/tauritavern-staging/<kind>/` | WebView 字节（上传与 Blob 导出）、宿主下载的远程资源、选取的角色卡与 Skill 归档、调试包 |
| 归档 | `archive_imports_root/incoming`、`archive_exports_root` | 数据归档、用户备份 |

- 上传字节经 `stage_file_*` 进入宿主：Android 用 base64 帧，其余平台用原始字节。浏览器使用同一暂存服务与二进制分块协议，供角色卡、立绘、Skill 和数据归档导入消费。
- 浏览器服务器通用暂存在 `app_root/staging/<kind>/`，只接受 canonical 路径位于暂存根内的文件；当前是 host-global 路径限制，尚无页面所有权隔离。不能将此保证混同于资源 reader、commit 或 channel 的页面隔离。
- 前端经宿主 `safeInvoke` 调用暂存与交付命令，失败时得到可读的错误消息；只有二进制分块走原始 invoke。
- 远程资源经 `stage_file_from_url` 进入宿主：由 `ContentService::download_external_file` 下载，与外部内容导入共用 URL 规则和 `Download` 配置（含用户的请求代理），不带页面的 Cookie 与 Referer。
- 写入失败的暂存文件由写入方删除。原生通用暂存在宿主启动时清空，暂存后未进入交付的文件随之回收；浏览器服务器目前只有调用链 discard，没有全局暂存到期清扫或页面断开清扫。归档暂存沿用独立过期清扫。
- 归档单独暂存：Android 的数据根与 cache 不在同一个卷，大型归档要放在数据卷上，才能直接移动进任务工作区。

## 4. 入口

| 产物或来源 | 入口 |
| --- | --- |
| `download()`、同源 `a[download]`、Android 图片长按（页面可读来源）、Skill 与 Agent 导出 | `deliverBlob()` |
| Android 图片长按（远程图片） | `deliverRemoteFile()`：`stage_file_from_url` 后调用 `deliver_staged_file` |
| 数据归档导出 | 任务完成后调用 `save_export_data_archive`：领取产物，交付，再标记为已处置 |
| 数据归档导入 | `import_data_archive_from_picker` |
| 用户备份 | `export_user_backup_archive` |
| 调试包 | `devlog_export_bundle` |
| 角色卡、Skill 归档选取 | `pick_import_files` |

上述入口表描述原生宿主。浏览器角色卡与 Skill 选取调用浏览器 picker，数据归档选取先上传暂存再启动共享导入任务；归档导出和用户备份返回绑定浏览器 cookie 的一次性 `/__tt/download/<token>` 链接，最多 32 个待取产物，10 分钟过期。链接被取用后即消费，响应结束（包括失败或中止）删除文件，不允许重放。未取的产物过期后清理；此生命周期不证明用户本机保存成功。

浏览器不安装原生 `downloadBridge`：原生浏览器的同源链接、Blob 下载与图片菜单保持原生语义，避免把自身下载重新拦截进交付函数。调试包导出、桌面目录选取等未接的原生能力不能视为已支持；详细边界见 [BrowserHost](BrowserHost.md)。

`<input type="file">`、拖放和原生桌面目录选择沿用各自入口。新增的导入导出功能接入宿主的共同交付契约。

## 5. 已知限制

- 移动端 dialog 插件会把面板失败也报告为取消。
- 写入中途失败时，目标文件可能不完整；Android 与鸿蒙的 fs 插件不能删除 URI 目标。
- 内容要在保存面板出现前完成暂存，期间不显示进度；远程下载没有总超时，也不能取消。
