# Hiddify 流量统计源码研究

**2026-09-13 scope update:** Use existing core-wide traffic rates and totals, with no direct exclusion, per-node byte attribution, or separate probe accounting. Earlier proxy-only completeness gates below are historical findings and no longer block the refactor. See ADR 0003. Current editable Figma frames: 1025:2 (120×30), 1027:16 (80×24). No core changes.

核查日期：2026-09-13。仅阅读 GitHub 一手源码和构建元数据；除本研究记录外未修改本项目，没有运行 Hiddify、测速或修改运行中的 sing-box 配置/二进制。下文将源码结论与未验证的运行条件分开。

## 先锁定实际版本

本次 App main 固定为 `276a7effb0046a039220a745022563740968c0b8`。生产构建按 `dependencies.properties` 的 `core.version=4.1.0` 下载 release，源码构建还存在另一条 gitlink 路径，因此不能把当前 hiddify-core main 当成 App 已发布实现。[App dependencies](https://github.com/hiddify/hiddify-app/blob/276a7effb0046a039220a745022563740968c0b8/dependencies.properties#L1-L10)、[Makefile 生产下载分支](https://github.com/hiddify/hiddify-app/blob/276a7effb0046a039220a745022563740968c0b8/Makefile#L1-L55)、[源码构建 targets](https://github.com/hiddify/hiddify-app/blob/276a7effb0046a039220a745022563740968c0b8/Makefile#L515-L570)。

| 路径 | hiddify-core commit | hiddify-sing-box commit |
| --- | --- | --- |
| App 默认生产依赖 v4.1.0 的 tag 源码 | `c9d6f0f00b2eda34e4fb71863e4e0a62b3e931a0` | `0a02b7729f6a211436bb8bdcd8696c283eb27767` |
| App 固定树的源码 gitlink | `f2034de743b1ad775dba026f4e6e3c44cf7d9790` | `3a1c923e306d5b39cde8f2c02ec9dd556ea112d3` |
| 研究时 core 最新 main，不能冒充 App pin | `db74dfc257d5becb4b4e9dbc7257a3dcdde20692` | `170d8315cab7a8695fd80469073ed2f1d07d63af` |

来源：[v4.1.0 core 固定树](https://github.com/hiddify/hiddify-core/tree/c9d6f0f00b2eda34e4fb71863e4e0a62b3e931a0)、[App 固定树 gitlink](https://github.com/hiddify/hiddify-app/tree/276a7effb0046a039220a745022563740968c0b8)、[gitlink core 固定树](https://github.com/hiddify/hiddify-core/tree/f2034de743b1ad775dba026f4e6e3c44cf7d9790)、[最新 core 固定树](https://github.com/hiddify/hiddify-core/tree/db74dfc257d5becb4b4e9dbc7257a3dcdde20692)。gitlink 为树条目，没有源码行号。release 二进制本身没有下载做可复现构建校验；这里把 tag 源码作为生产版本的来源证据，不声称已证明每个发布资产逐字节对应该树。

go.mod 的 `require github.com/sagernet/sing-box v1.13.0` 不是最终依赖：实际 `replace ... => ./hiddify-sing-box` 使用上述本地子模块 fork。也不是通过独立标准 sing-box 的 V2Ray API 获取这些流量字段。[gitlink core go.mod](https://github.com/hiddify/hiddify-core/blob/f2034de743b1ad775dba026f4e6e3c44cf7d9790/go.mod#L276-L304)、[v4.1.0 go.mod](https://github.com/hiddify/hiddify-core/blob/c9d6f0f00b2eda34e4fb71863e4e0a62b3e931a0/go.mod)。

## 实时速度与累计量从哪里来

App 调用链是 `SideBarStatsOverview` → `StatsNotifier` → `StatsRepository.watchStats()` → `HiddifyCoreService.watchStats()` → `bgClient.getSystemInfoStream(Empty())`。这条 gRPC stream 返回 `SystemInfo`；Dart UI 直接读取 uplink/downlink 和 uplinkTotal/downlinkTotal，没有在显示层遍历活动连接来计算总量。[core service:321-329](https://github.com/hiddify/hiddify-app/blob/276a7effb0046a039220a745022563740968c0b8/lib/hiddifycore/hiddify_core_service.dart#L321-L329)、[repository](https://github.com/hiddify/hiddify-app/blob/276a7effb0046a039220a745022563740968c0b8/lib/features/stats/data/stats_repository.dart)、[UI:53-101](https://github.com/hiddify/hiddify-app/blob/276a7effb0046a039220a745022563740968c0b8/lib/features/stats/widget/side_bar_stats_overview.dart#L53-L101)。

Notifier 只在 serviceRunning 时订阅，停机时返回空 SystemInfo，流中的失败也映射为空 SystemInfo；此层不是跨重启历史账本。速度格式器把输入解释为每秒字节。展开卡片分别显示双向速度和双向累计，折叠卡片的 totalTransferred 实际只使用 downlinkTotal，不能将这个标签推断成上下行之和。[Notifier](https://github.com/hiddify/hiddify-app/blob/276a7effb0046a039220a745022563740968c0b8/lib/features/stats/notifier/stats_notifier.dart)、[速度格式器:14-16](https://github.com/hiddify/hiddify-app/blob/276a7effb0046a039220a745022563740968c0b8/lib/utils/number_formatters.dart#L14-L16)、[折叠卡片:53-66](https://github.com/hiddify/hiddify-app/blob/276a7effb0046a039220a745022563740968c0b8/lib/features/stats/widget/side_bar_stats_overview.dart#L53-L66)。

核心调用链为：`GetSystemInfoStream` → 每秒 `readStatus(previous)` → `StartedService.ReadStatus()` → Clash `TrafficManager.Total()`。累计量直接读取 manager 的 atomic 计数；`uplink/downlink` 是本次累计量减上次累计量。不是轮询 active connection 列表后求和。首条状态没有 previous，因此速度字段为零；后续按 1 秒 ticker 差分，源码没有按实际 elapsed 再归一化，也没有在这段差分代码里处理计数器回退。[v4.1.0 commands.go:22-38](https://github.com/hiddify/hiddify-core/blob/c9d6f0f00b2eda34e4fb71863e4e0a62b3e931a0/v2/hcore/commands.go#L22-L38)、[stream:96-130](https://github.com/hiddify/hiddify-core/blob/c9d6f0f00b2eda34e4fb71863e4e0a62b3e931a0/v2/hcore/commands.go#L96-L130)、[ReadStatus:420-440](https://github.com/hiddify/hiddify-sing-box/blob/0a02b7729f6a211436bb8bdcd8696c283eb27767/daemon/started_service.go#L420-L440)、[Total:95-118](https://github.com/hiddify/hiddify-sing-box/blob/0a02b7729f6a211436bb8bdcd8696c283eb27767/experimental/clashapi/trafficontrol/manager.go#L95-L118)。

源码 gitlink 的同一路径一致：读取 service 的 total，再 1 秒差分。[gitlink commands.go](https://github.com/hiddify/hiddify-core/blob/f2034de743b1ad775dba026f4e6e3c44cf7d9790/v2/hcore/commands.go#L22-L130)。

## 它统计的是哪些流量

**全局 total 包含经过该 sing-box routed tracker 的 direct 流量。** `RoutedConnection` 无 direct 过滤地创建 TCP tracker；tracker 确定 outbound 后，读写回调都调用 manager 的全局累加。`Total()` 也没有减去 direct。它不是 Internet Proxy 白名单统计，更不是全机器网卡统计；绕过该进程的流量不在这条数据源内。[Server tracker 接入](https://github.com/hiddify/hiddify-sing-box/blob/0a02b7729f6a211436bb8bdcd8696c283eb27767/experimental/clashapi/server.go#L242-L252)、[TCP tracker](https://github.com/hiddify/hiddify-sing-box/blob/0a02b7729f6a211436bb8bdcd8696c283eb27767/experimental/clashapi/trafficontrol/tracker.go#L127-L181)、[全局累加](https://github.com/hiddify/hiddify-sing-box/blob/0a02b7729f6a211436bb8bdcd8696c283eb27767/experimental/clashapi/trafficontrol/manager.go#L95-L109)。

**短连接不会仅因 UI 一秒轮询而丢失全局已计字节。** 数据在连接读写回调时就加入 manager 的累计值；关闭连接只从 active map 移除并放入最多 1000 条的已关闭列表，不回减 total。TCP 与 UDP 都有累计回调。因此“只在两次快照间存在”的 routed 连接不依赖被 UI 看见。但这不等于崩溃后持久化、链路协议开销或任意异常读写路径已经验证。[TCP/UDP 回调](https://github.com/hiddify/hiddify-sing-box/blob/0a02b7729f6a211436bb8bdcd8696c283eb27767/experimental/clashapi/trafficontrol/tracker.go#L155-L181)、[UDP 回调](https://github.com/hiddify/hiddify-sing-box/blob/0a02b7729f6a211436bb8bdcd8696c283eb27767/experimental/clashapi/trafficontrol/tracker.go#L235-L263)、[Leave 与保留上限](https://github.com/hiddify/hiddify-sing-box/blob/0a02b7729f6a211436bb8bdcd8696c283eb27767/experimental/clashapi/trafficontrol/manager.go#L36-L92)。

累计值属于 manager 实例：构造是空 manager，`ResetStatistic` 清零；这里没有 30 分钟序列存储或跨进程恢复契约。不能把它的“累计量”描述成跨重启用量账本。[构造与字段](https://github.com/hiddify/hiddify-sing-box/blob/0a02b7729f6a211436bb8bdcd8696c283eb27767/experimental/clashapi/trafficontrol/manager.go#L38-L54)、[ResetStatistic](https://github.com/hiddify/hiddify-sing-box/blob/0a02b7729f6a211436bb8bdcd8696c283eb27767/experimental/clashapi/trafficontrol/manager.go#L177-L188)。

## 按节点能力和一个重要版本问题

这个 fork 确实增加了按 outbound tag 的累计 map 和 `OutboundUsage(tag)`。TCP tracker 在建立时沿 `OutboundGroup.Now()` 解析链并把结果 tag 捕获进读写回调；当前选择之后改变，不会把这个已有 tracker 的计数键改为新节点。这里的身份只是 tag，不包含配置 fingerprint 或 runtime epoch；也不能把普通 group 链解析外推成所有 detour/balancer 路径都有精确归属。[tracker 解析与捕获](https://github.com/hiddify/hiddify-sing-box/blob/0a02b7729f6a211436bb8bdcd8696c283eb27767/experimental/clashapi/trafficontrol/tracker.go#L127-L178)、[OutboundUsage](https://github.com/hiddify/hiddify-sing-box/blob/0a02b7729f6a211436bb8bdcd8696c283eb27767/experimental/clashapi/trafficontrol/manager.go#L95-L118)。core 把这份数据赋给每个 `OutboundInfo.Upload/Download`。[core GetProxyInfo](https://github.com/hiddify/hiddify-core/blob/f2034de743b1ad775dba026f4e6e3c44cf7d9790/v2/hcore/proxy_info.go#L20-L57)。

**v4.1.0 与 App gitlink 的节点下载计数有明确源码缺陷：`PushDownloaded(outbound,size)` 全局累加 `size`，但是 outbound map 累加常数 `100`。** 所以全局下载字段和每节点下载字段不能混为一谈；例如一次回调 `size=4096`，该路径全局增加 4096、节点增加 100。该数值例子是代码推导，不是 Hiddify 实测。[v4.1.0 固定代码](https://github.com/hiddify/hiddify-sing-box/blob/0a02b7729f6a211436bb8bdcd8696c283eb27767/experimental/clashapi/trafficontrol/manager.go#L95-L118)、[App gitlink 固定代码](https://github.com/hiddify/hiddify-sing-box/blob/3a1c923e306d5b39cde8f2c02ec9dd556ea112d3/experimental/clashapi/trafficontrol/manager.go#L95-L118)。

不要把这个缺陷泛化到最新 core：研究时最新 core 依赖的 `170d831...` 已把全局与 outbound 更新拆为独立函数，`PushOutboundDownloaded` 使用 `Add(size)`。这只说明该常数问题在那个新源码快照不再存在，不代表 App 生产包已经使用它，也不代表新路径整体完成验证。[较新源码](https://github.com/hiddify/hiddify-sing-box/blob/170d8315cab7a8695fd80469073ed2f1d07d63af/experimental/clashapi/trafficontrol/manager.go#L95-L111)。

## Delay 探测是否计入

对研究的 v4.1.0 / App gitlink 路径，**没有发现把 URLTest 请求送入上述 routed traffic tracker 的计数 hook**。`OutboundMonitoring.tester` 直接把 outbound 传入 `urltest.URLTest`；后者直接 `detour.DialContext` 再发送 HEAD。这与 inbound route 上的 `RoutedConnection` 包装不同。源码支持的判断是：这条标准 URLTest 路径绕过上述 manager 的 routed 累计机制，不能声称 Hiddify 已把所有 Delay 流量纳入总量。[v4.1.0 tester](https://github.com/hiddify/hiddify-sing-box/blob/0a02b7729f6a211436bb8bdcd8696c283eb27767/common/monitoring/outbound_monitoring.go#L576-L590)、[URLTest 直接拨号](https://github.com/hiddify/hiddify-sing-box/blob/0a02b7729f6a211436bb8bdcd8696c283eb27767/common/urltest/urltest.go#L101-L159)、[gitlink 路由 tracker](https://github.com/hiddify/hiddify-sing-box/blob/3a1c923e306d5b39cde8f2c02ec9dd556ea112d3/route/route.go#L150-L159)。

已读的 manager 接口只有 outbound/size，没有 probe-purpose 子集标记。不能从“有全局吞吐显示”推导其满足“探测计入总量、并且单独识别、不重加”。特殊 outbound 自己内部的额外流量、多跳连接、抓包级网络字节均未运行验证。[manager 接口](https://github.com/hiddify/hiddify-sing-box/blob/0a02b7729f6a211436bb8bdcd8696c283eb27767/experimental/clashapi/trafficontrol/manager.go#L95-L118)。

## 套餐用量是另一条来源

订阅套餐的 upload/download/total/expire 来自订阅提供的 `subscription-userinfo`。ProfileParser 解析这些字段并保存为 profile 的 SubscriptionInfo；不是将本地实时流量累加到套餐数据中。模型中 consumption=upload+download，remainingBW=total-consumption。[解析:289-301](https://github.com/hiddify/hiddify-app/blob/276a7effb0046a039220a745022563740968c0b8/lib/features/profile/data/profile_parser.dart#L289-L301)、[读取并附到 profile:359-375](https://github.com/hiddify/hiddify-app/blob/276a7effb0046a039220a745022563740968c0b8/lib/features/profile/data/profile_parser.dart#L359-L375)、[模型公式:48-65](https://github.com/hiddify/hiddify-app/blob/276a7effb0046a039220a745022563740968c0b8/lib/features/profile/model/profile_entity.dart#L48-L65)。

因此必须区分：实时速率、当前核心实例的累计量、订阅服务提供的套餐已用/剩余。套餐数字的更新取决于订阅更新和服务端上报；上述源码不能证明它每秒与本机流量同步。

## 对 sing-box-tui 的可借鉴边界

可借鉴的是“读核心累计计数 → 按时间差取速率 → UI 订阅显示”，而不是对 active 列表求和。本项目已有 `/traffic` 和 `/connections` 的 global totals：`src/controller.rs:230-264,784-799`，因此同口径全局图表并不以改核心为先决条件。但这份 Hiddify 实现没有证明不改当前核心就能获得 ADR 0003 的 Internet Proxy 专属、包含并单独识别所有探测的完整账本。不能拿包含 direct 的全局总量或存在版本缺陷的按节点计数静默替代已接受范围。

研究到此为止：未改 sing-box；未提议把 Hiddify fork、build tag 或生产核心替换作为已经获准的下一步。
