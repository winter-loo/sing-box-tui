# TUI 重构可行性核查

**2026-09-13 scope update:** Use existing core-wide traffic rates and totals, with no direct exclusion, per-node byte attribution, or separate probe accounting. Earlier proxy-only completeness gates below are historical findings and no longer block the refactor. See ADR 0003. Current editable Figma frames: 1025:2 (120×30), 1027:16 (80×24). No core changes.

2026-09-11。由两个 `gpt-6-astra`、`low reasoning` 子代理分别核查统计和布局，主代理补充导航接入并汇总。本文是技术核查记录与建议实施顺序，不代表业务代码已经实现，也不改变 ADR 0002/0003 已接受的产品口径。

## 结论

**历史中间方案（已被本文开头的 2026-09-13 决定取代）：用户选择不修改 sing-box，使用可获得的统计。** 因此以下全量统计门槛保留为历史技术结论，不再阻塞 TUI 重构。按 ADR 0003 修订版显示可归属的观测/估算数据，未支持的探测分项留空，不把含直连的核心总量冒充代理总量。用户同时接受现有空间布局，要求减少文字、图例与图形噪音。

120×30 的字符预算可以容纳完整监控页；当前渲染架构可增加独立的闲置页。2 秒采集、10 秒轻量探测和 30 分钟持久化也可以实现。主要前置缺口是**完整、可按 Internet Proxy 节点归属并单列探测流量的累计计数来源**：当前活动连接快照无法满足这一要求。

后续授权的隔离验证已经执行，详见 [实际统计实验](tui-telemetry-contract-results.md) 和 [布局渲染验证](tui-idle-layout-validation.md)。当前核心明确拒绝 V2Ray API 配置；Clash 总量包括直连，却漏掉产生实际字节的失败 Delay 探测。两个独立实验复现了这一反例。因此统计前置门槛未通过，不能继续把现有接口包装成满足 ADR 0003 的完整账本。正确的 Figma Remote 连接已完成实时复查，120×30 和 80×24 已产生实际 TestBackend 图片；这仍是独立 fixture，不是已接入产品的监控页。

本轮为只读代码与渲染库能力核查，未启动真实测速、改变网络配置或运行中的代理，未实现产品或执行新页面的渲染测试。子代理误用了另一套 Figma 连接：`mcp__codex_apps__figma_*` 的账号为 songli，文件请求报没有编辑权限。随后主代理使用本会话此前采用的 `mcp__figma__*` 核验：账号为 David Lu，`get_metadata` 成功读取 `1014:2`。因此不能概括为目标 Figma Remote MCP 不可访问；后续应明确使用 `mcp__figma__*`。布局预算仍基于此前完整读取的 [Implementation Handoff](https://www.figma.com/design/jGcpW9esdjzunUpuU3Aqu5?node-id=1014-2) 及其后用户确认的 ADR，本次元数据复查不等于新的视觉验证。

## 统计能力与缺口

| 要求 | 当前证据 | 实施约束 |
| --- | --- | --- |
| 跨页面采集 | `src/tui/app.rs:126` 为 2 秒周期，`:284` 主循环持续刷新连接 | `src/tui_connections.rs:46` 同步请求，`src/controller.rs:171` 使用 block_on；转入独立 worker，避免 UI 阻塞影响周期 |
| Internet Proxy 归属 | `src/tui_connections.rs:7` 只识别 direct/国内直连，其他连接归为 proxy | 必须按真实经过的 Internet Proxy 节点正向归属，不能使用“非 direct”的余集，也不能直接使用无节点维度的全局 totals |
| 短连接完整性 | `src/automatic_selection.rs:234` 对活动连接快照取差，消失的 ID 被移除 | 两次快照之间完成的连接及关闭前尾段会漏计；现有 tracker 适合自动切换保护，不能作为完整用量账本 |
| 切换后旧连接流量 | `src/config.rs:1691` 默认保留现存连接；现有 tracker 只过滤 current_node | 汇总所有纳入范围的节点，不能随当前节点切换而清空或更换聚合过滤器 |
| 隔离探测流量 | `src/node_runtime_manager.rs:1403` 有独立 controller；`:1254` 只暴露代理 URL，`:1280` 停止后清理 runtime | live controller 看不到其他进程的计数；需要每个受管 runtime 的采集接口及关闭前最终计数 |
| 探测分项 | `src/controller.rs:595` 的 Delay 返回只有延迟；`src/sustained_quality.rs:89` 的 bytes_read 是响应体字节；`src/usability_probe.rs:98` 无统一流量结果 | 响应体大小不是双向代理总字节。Delay 是否被统计、能否单列，必须实验；不可假设一定进入 /connections |
| 当前路由延迟 | 已有 named-outbound Delay API，但无独立 10 秒路由监控调度 | 解析实际 selector 链，捕获发起时节点身份和 route interval；迟到结果不得归入新节点 |
| 历史持久化 | `src/storage.rs:293` 有质量/探测表，无聚合流量和区段表；`:1880` 有节点指纹机制 | 新增独立历史生命周期；`:1871` 的节点 reconciliation 删除不应抹掉跨节点聚合历史 |

### 计数来源优先级

1. **先验证官方 V2Ray API 的累计 stats。** [官方文档](https://sing-box.sagernet.org/configuration/experimental/v2ray-api/) 提供 inbound/outbound/user 维度的 gRPC 流量统计，并明确该功能不在默认构建中。针对目标实际版本和构建验证支持性、叶节点与 selector 的归属、detour 去重、短连接尾段、Delay 覆盖和重启/重置行为。`scripts/install.sh:8` 的默认版本为 `v1.13.13-winterloo.2`，这不是本机正在运行的版本或 build 能力证明。
2. 若现有 stats 不满足已确认口径，再评估内核统一计数 hook 或受支持的事件接口，携带节点、runtime 和 probe purpose。是否更换或定制 sing-box 需在证据明确后单独决定。
3. Sidecar 只能补齐实际通过它的数据通道；无法自动覆盖绕过它的 live Delay，也不能天然补齐 live 全量统计。

**不能把快照估计值静默当作完整统计。** 另外，代理数据通道字节与包括封装、重传的链路字节不是同一口径；当前决策未承诺与 Provider 账单逐字节一致，正式命名和验收时要明确。

### 建议数据边界

- 每个 runtime 使用独立 epoch，避免 PID、连接 ID 和计数器在重启后复用造成重计。
- 以节点配置身份快照归属历史，保留当时标签；同名但配置变更不能混作同一个节点。
- 以实际 elapsed 时间把累计差转换成速率。重启首次读建立基线，不能把停机期间累计差压到当前 2 秒内。
- probe 是 aggregate total 的子集；多层 selector/detour 只选定一个计量层级，避免重复相加。
- 覆盖失败、runtime 崩溃未收尾和采集停顿都需要缺测状态；观测到零流量与没有观测必须区分。
- 当前路由延迟复用要求节点身份、目标 URL 和时效等价。发起和完成之间发生切换时，结果仍属于发起时区段。
- 聚合历史、路由区段和节点质量历史分别建模；render 只消费快照，不启动探测。

## 120×30 建议字符布局

坐标从 0 开始，尺寸包含边框。以下已核算字符预算，**尚未生成真实 Ratatui 截图或验证实际可读性**。

| 区域 | x | y | 宽 | 高 |
| --- | ---: | ---: | ---: | ---: |
| 当前路由摘要 | 0 | 0 | 120 | 4 |
| 当前节点质量历史 | 0 | 4 | 38 | 12 |
| 活动连接 | 0 | 16 | 38 | 12 |
| 全局两图共用外框 | 38 | 4 | 82 | 24 |
| 快捷键／状态，无边框 | 0 | 28 | 120 | 2 |

全局外框内部 80 列，固定 8 列作为纵轴标签和间隔，两张图统一绘图区 x=47..118（72 列）。延迟 y=8..13、吞吐 y=16..21，各 6 行；y=5..7 放区段图例、节点名和延迟单位，y=14..15 放吞吐单位及上下行图例；y=22..23 放公共时间轴；y=24..26 放探测子集、缺测和区段说明。

节点图内部 10 行可分配延迟和实测吞吐各 3 行、标题各 1 行及时间/说明 2 行。活动连接内部 10 行可放表头、8 条精简记录、更多提示；完整目标和链路留在 c 面板。

建议断点：120×30 完整；宽 96..119 且高至少 30 时先隐藏活动连接、缩小节点列；低于 96 列或 30 行再隐藏节点图。后续通过正确的 mcp__figma__* 实时读取 Terminal Contract（874:2），确认最低支持尺寸为 80×24；低于任一维度显示 resize 提示并保留 quit/help。此前 64×22 建议撤回。中间断点仍需渲染验证。

### 绘图实现约束

- 当前 `src/tui/view/dashboard.rs:129,203` 是操作候选页的 snapshot 和左右布局。新增独立 `IdleDashboardSnapshot` 与 render，避免复用名称而混淆当前路由和浏览节点。
- 本机 Ratatui 0.30 所使用的 `ratatui-widgets-0.3.0/src/chart.rs:158,320,732` 表明 GraphType 有 Scatter/Line/Bar，没有原生虚线。可用下载连线、上传散点；样式可读性需终端验证。
- Chart 会按 y 轴标签宽度调整绘图区。两个相同外框不保证时间轴对齐，应固定 gutter、自绘公共轴和标签，统一时间投影。
- 缺测和节点切换拆分 Dataset，不跨缺口连线。30 分钟投到 72 列约 25 秒/列，2 秒样本必须降采样，保留范围尖峰、缺测及边界信息，不伪造时间分辨率。
- 每个字符格只有一个前景色；同格多次切换、上下行重合无法全部同时精确表达。密集区段用短编号和完整详情补充，标签截断避碰，不承诺每个长节点名始终完整出现在图内。
- `src/tui/view/shared.rs:38` 已按显示宽度截断，但逐 char 截断会分开组合字符/ZWJ；需要 grapheme 边界处理及 Windows Terminal 实测。
- 图例占独立固定行，避免内置 legend 自动隐藏或遮盖曲线。节点图稀疏测速只显示真实测量，不能延展为连续带宽。

## 导航接入补充

- `src/tui/app.rs:298` 只把 key.code 交给 handler；Ctrl+K 必须保留修饰键并统一命令映射，避免与普通 k 混淆。
- `src/tui/app.rs:765` 按认证、进度、引导、设置、输入、弹窗顺序分派。应显式建模 base page、overlay 来源、工作区选择和输入活动时间，集中计算闲置资格，不让 render 或后台更新重置闲置计时。
- `src/tui_node_quality_detail.rs:11` 从 selected_member_name 打开质量详情；监控页 i 必须传入明确当前路由身份，操作页 i 则保留浏览节点语义。
- `src/tui_state.rs:55` 与 `src/tui_runtime_state.rs:186` 可增加默认兼容的持久工作区选择；不要持久化闲置页作为工作区，也不要借页面恢复执行 selector PUT。
- 现有启动过程另有 `src/tui/app.rs:691` 调用 `restore_persisted_selections`，该函数在 `src/tui_runtime_state.rs:259` 确实可能切换 selector。它与“恢复工作区页面”是不同机制，实施时保持边界，不因新增导航而重复调用或未经讨论删除旧行为。
- `src/tui/app.rs:269` 明确禁用鼠标报告以保留终端原生选字复制；本方案不因 Figma 热点默认引入鼠标交互。

## 实施顺序与验收门槛

1. **统计能力隔离契约实验。** 在可控本地端点和隔离 runtime 中检查目标构建、累计 counters、归属及探测覆盖，不操作用户当前代理。未满足时输出明确缺口，不能宣称完成 ADR 统计要求。
2. **独立采集与历史模型。** 建立 runtime epoch、节点身份、覆盖状态、probe 子集、route interval、2/10 秒调度和 30 分钟存储；与节点质量清理生命周期分开。
3. **工作区和闲置状态机。** 实现首次/恢复/回退、30 秒例外、直接快捷键、弹窗返回、当前路由详情绑定；保留现有 VPN 生命周期。
4. **纯布局和历史投影。** 独立闲置 snapshot、布局函数、分桶/分段、公共轴和图例；实现收起顺序并复用详情。
5. **定向验证。** 先做状态与计数实验，再做 TestBackend 和真实终端视觉检查；通过后运行仓库所需 Rust 检查。此次仅文档核查，无新业务代码需要 cargo 测试。

计数测试重点：小于 2 秒的短连接、关闭尾段、代理/direct/内网混合、切换后旧连接传输、两个 probe runtime、Delay 覆盖、失败/部分传输探测、runtime 重启/计数器重置、同名节点变配置、probe 子集不重加。

历史与交互测试重点：A→B→A、迟到 probe 归属、30 分钟窗口、重启间隙不连线、输入与认证期间不闲置切页、无效键不导航、c/i 关闭返回监控页、工作区删除回退且不新增 selector 写入。

布局测试沿用 `src/tui/view/dashboard_tests.rs:212` 的 TestBackend 模式：120×30、119/96/95 列、29 行、80×24、79×24、80×23、极小窗口；CJK/emoji/长标签；同列多个切换；双图边界同 x；所有绘制矩形不越界。真实 Windows Terminal 检查 Braille、上传散点与下载连线重合、颜色和缩放。

下一项应执行第 1 步统计契约实验；是否需要更换内核及字节计量口径，仅在实验给出可比较方案后再形成决策。
