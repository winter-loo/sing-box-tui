# TUI Interaction Specification

状态：待确认的目标交互规格
核对日期：2026-09-17
范围：用户进入 `sing-box-tui run` 后能够看到和触发的全部 TUI 行为

## 1. 这份文档是什么

这类文档通常叫 **UI Interaction Specification**、**UI Behavior Specification** 或 **Use Case Specification**。

- OMG 把 use case 定义为系统执行的一组动作，并要求产生用户可观察的结果；它可以包含基本行为、异常行为和错误处理。[OMG UML](https://www.omg.org/spec/UML/ISO/19505-2/PDF)
- IBM 将文字形式称为 **use case specification**，用于描述用户与系统交互的动作序列。[IBM Use Case Specification](https://www.ibm.com/docs/en/engineering-lifecycle-management-suite/doors-next/7.1.0?topic=requirements-defining-use-cases)
- Cucumber 用 `Given / When / Then` 表达初始状态、用户动作和可观察结果。[Cucumber Gherkin Reference](https://cucumber.io/docs/gherkin/reference/)
- W3C 的组件规范也按控件状态和每个按键的响应逐项定义 keyboard interaction。[W3C ARIA APG](https://www.w3.org/WAI/ARIA/apg/)

本项目采用 **TUI Interaction Specification（TUI 交互行为规格）**。每个场景只回答五件事：

1. 用户操作前在哪个界面、什么状态；
2. 用户按了什么键；
3. 程序立即显示什么；
4. 用户继续操作后界面如何变化；
5. 哪些运行状态被改变，哪些绝不能被改变。

## 2. 界面名称

| 名称 | 含义 |
|---|---|
| Workspace Chooser | 首次启动时选择 Internet Proxy 或 Private Access。 |
| Node List | Internet Proxy 的节点选择列表，是 Internet 默认主页。 |
| Provider Select | 从 Node List 打开的 Provider 选择框。 |
| Private Access | 内网 profile 和连接状态页面。 |
| Profile Select | 从 Private Access 打开的 profile 选择框。 |
| Node Dashboard | current route node 的监控页面，不是节点选择页。 |
| Help | 快捷键帮助弹窗。 |
| Node Quality | 节点质量详情弹窗。 |
| Active Connections | 当前活动连接弹窗。 |
| Settings | 设置弹窗。 |
| Command Palette | `Ctrl+K` 打开的搜索和导航弹窗。 |
| Authentication | Private Access 认证弹窗。 |
| Progress | Private Access 连接或断开进度弹窗。 |

本文中的按键区分大小写：`p` 打开 Provider/Profile Select，`P` 控制 background probe schedule；`u` 刷新订阅，`U` 运行 usability probe；`t` 与 `T`、`v` 与 `V`、`b` 与 `B` 也分别表示不同操作。

## 3. 所有界面共同遵守的行为

### G-01 弹窗接管输入

**前置状态**：任意页面上已经打开一个弹窗。

**用户操作**：按下任意键。

**程序响应**：

- 按键只交给最上层弹窗处理。
- 弹窗下面的页面不能同时响应这个按键。
- 关闭弹窗的 `Esc`、`Enter` 或同名快捷键不能继续传递给下面的页面。

**结束状态**：关闭弹窗后回到打开它的页面，原页面的 workspace、列表焦点、tab、滚动位置和展开状态不变。

### G-02 当前焦点与已经应用的对象

**前置状态**：Node List 或任何选择框中存在当前焦点。

**用户操作**：使用方向键移动焦点。

**程序响应**：

- `>` 表示键盘当前焦点。
- `*` 表示已经应用的 provider、profile 或 route node。
- 移动 `>` 不得移动 `*`。
- 只有场景明确写出“应用”或“连接”时，运行状态才改变。

### G-03 页面底部

任意普通页面都必须这样显示：

- 当前页面可以使用的快捷键显示在左下角。
- system status、GLOBAL NET、实时上传和下载显示在右下角。
- `Switch Workspace`、`Actions`、`Ctrl+K` 和 system status 不显示在顶部。
- 左右两组内容在窄窗口中可以减少次要文字，但不能重叠。

### G-04 小窗口

**前置状态**：终端小于 `80×24`。

**程序响应**：显示 resize 提示，不绘制被压坏的完整页面；仍显示 `q` 和 `?`。

**用户操作**：

- `q`：退出。
- `?`：打开适合当前尺寸的 Help。
- 终端恢复到至少 `80×24`：立即重新绘制正常页面，不需要重启。

### G-05 鼠标和复制

TUI 不启用 mouse capture。用户用鼠标拖选时由终端执行原生文本选择和复制，程序焦点和运行状态不改变。

## 4. 启动流程

### ST-01 同时配置了两个 workspace 的首次启动

**前置状态**：Internet Proxy 和 Private Access 都已配置，且没有保存过 workspace 选择。

**程序响应**：显示 Workspace Chooser，包含 `Internet Proxy` 和 `Private Access` 两项。

**用户操作**：

- `Up/Down` 或 `j/k`：移动焦点。
- `Enter`：确认当前项。
- `Esc`：退出程序，不连接任何服务。

**确认 Internet Proxy 后**：关闭 chooser，进入 Node List，保存 workspace 选择；不得切换 route node。

**确认 Private Access 后**：关闭 chooser，进入 Private Access；不得自动连接 profile。

### ST-02 只配置了一个 workspace

**前置状态**：只配置 Internet Proxy 或只配置 Private Access。

**程序响应**：不显示 Workspace Chooser，直接进入唯一可用的 workspace。

**状态限制**：进入 Private Access 不等于连接；进入 Internet 不等于切换节点。

### ST-03 恢复上次 workspace

**前置状态**：已经保存过最后使用的 workspace。

**程序响应**：

- 该 workspace 仍存在：直接恢复它。
- 该 workspace 已删除、只剩一个：进入剩余 workspace。
- 两个都不存在：进入 Subscription Setup。

恢复过程不得连接 Private Access，也不得重新应用 Internet 节点。

### ST-04 Subscription Setup

**前置状态**：没有可用 Internet 配置，需要输入订阅 URL。

**程序响应**：显示 URL 输入框和操作提示。

**用户操作**：

- 输入字符、`Backspace`：编辑 URL。
- `Enter`：校验并保存。
- `s`：标记为跳过，以后可在 Settings 配置。
- `Esc`：仅推迟本次 setup，下次仍可出现。

**成功结果**：URL 必须以 `http://` 或 `https://` 开头；保存到 `.suburl` 后提示用户执行 subscription refresh。保存 URL 不代表 running core 已经加载新节点。

**失败结果**：输入框保持打开，在原位置显示错误，用户输入不丢失。

## 5. Node List

### NL-01 页面初始状态

**前置状态**：用户进入 Internet Proxy。

**程序响应**：

- 显示 Node List，而不是 Node Dashboard。
- 顶部 breadcrumb 显示 `INTERNET / <applied provider> / <current route node>`。
- node-view tabs 显示在节点列表上方。
- 当前 route node 使用 `*` 标记；当前 browsed node 使用 focused row 标记。
- 当前 selector 已应用的节点显示在列表第一行；该行固定，不随下面的候选节点滚动。
- browsed node 与 current route node 可以不是同一个节点。

### NL-02 打开并确认 Provider Select

**前置状态**：用户位于 Node List，没有其他弹窗。

**用户操作**：按 `p`。

**立即响应**：

- Node List 留在背景。
- 居中显示 `Select Provider` / `Provider Select` 弹窗。
- 弹窗列出可用 provider。
- 已应用 provider 使用 `*`；键盘焦点使用 focused row。

**弹窗内操作**：

- `Up/Down` 或 `j/k`：移动 provider 焦点，不改变 route。
- `Enter`：确认 focused provider。
- `Esc` 或 `p`：不做修改并关闭。
- `q`：退出程序。

**按 Enter 后**：

1. 关闭 Provider Select。
2. 返回 Node List。
3. Node List 改为显示所选 provider 的节点。
4. browsed node 对齐到该 provider 当前已经应用的节点；如果没有，则对齐到第一项。
5. node-view tab 保持在当前有效 tab；若该 tab 在新 provider 不可用，则回到 `Current selector`。
6. breadcrumb 仍显示真实的 applied provider 和 current route node。
7. 不调用 sing-box route switch；只有用户之后按 `Space` 应用节点时才改变路由。

**没有 provider 时**：不打开空弹窗；在 status 区显示 `No proxy providers configured`。

### NL-03 浏览节点

**前置状态**：Node List 已显示节点。

**用户操作和响应**：

| 按键 | 响应 |
|---|---|
| `Up` / `k` | focused row 上移一行。 |
| `Down` / `j` | focused row 下移一行。 |
| `g` | focused row 移到第一项。 |
| `G` | focused row 移到最后一项。 |
| `Tab`、`h`、`l` | Node List 没有第二个永久 pane，不执行 workspace 切换，也不改变焦点。 |
| `Enter` | 不应用节点，不改变页面。 |

移动过程中 breadcrumb、`*` 和 current route node 不变。
当前 route node 的首行保持固定；其余候选节点在首行下方滚动。

### NL-04 应用 browsed node

**前置状态**：Node List 中 focused row 位于一个可应用节点。

**用户操作**：按 `Space`。

**程序响应**：

1. 向对应 selector 应用该节点。
2. 如果配置为 parent selector → provider selector → node，同时把 parent selector 指向该 provider。
3. 成功后把 `*` 移到新节点。
4. 把新节点移动到列表第一行并固定；其余节点从第二行开始滚动。
5. breadcrumb 更新为新的 applied provider 和 current route node。
6. 保存该 selector 的选择。
7. status 区显示成功信息。

**状态限制**：已有连接不被强制中断；新连接和重试使用新节点。

**失败结果**：保持原 `*` 和 breadcrumb，在 status 区显示错误；focused row 不丢失。

### NL-05 切换 node-view tab

**前置状态**：节点列表区域拥有焦点。

**用户操作**：按 `Left/Right`。

**程序响应**：在 `Current selector`、`Streaming` 和可见 custom criteria 之间切换。

**结束状态**：

- 节点列表只显示该 tab 接受的节点。
- 切 tab 不运行 probe，不修改 selector，不移动 `*`。
- 原 browsed node 仍在新 tab 时保持焦点；否则焦点移到第一项。
- tab 没有结果时显示明确空状态。

### NL-06 打开 Help

**前置状态**：用户位于 Node List。

**用户操作**：按 `?`。

**立即响应**：在 Node List 上方打开 Help 弹窗，显示按类别排列的实际快捷键。

**Help 内操作**：

- `Up/Down` 或 `j/k`：移动帮助条目。
- `g/G`：第一项/最后一项。
- `?`、`Esc` 或 `Enter`：关闭 Help。
- `q`：退出程序。

**关闭结果**：回到同一个 Node List；provider、tab、focused row、`*` 和滚动位置不变。

### NL-07 打开 Node Quality

**前置状态**：Node List 中有 browsed node。

**用户操作**：按 `i`。

**立即响应**：打开 Node Quality 弹窗，标题和内容对应 browsed node，而不是 current route node。

弹窗显示 reachability attempts、assessment、warm/cold 数据、sustained throughput、usability results、历史和最近一次 auto-pick explanation。没有数据时显示 missing，不伪造 `0`。

**弹窗内操作**：

- `Up/Down` 或 `j/k`：滚动。
- `i`、`Esc` 或 `Enter`：关闭。
- `q`：退出程序。

**关闭结果**：回到原 Node List 和原 browsed node。

**没有 browsed node 时**：不打开弹窗，在 status 区说明没有可查看节点。

### NL-08 打开 Active Connections

**前置状态**：用户位于 Node List。

**用户操作**：按 `c`。

**立即响应**：打开 Active Connections 弹窗，显示当前活动连接的 destination、inbound/source、outbound chain、rule、upload/download 和 age。

**弹窗内操作**：

- `r`：立即刷新连接数据。
- `c`、`Esc` 或 `Enter`：关闭。
- `q`：退出程序。

**关闭结果**：回到原 Node List。连接弹窗是实时快照，不显示已经结束的连接历史。

### NL-09 打开 Command Palette

**前置状态**：用户位于 Node List。

**用户操作**：按 `Ctrl+K`。

**立即响应**：

- 在 Node List 上方打开居中的 Command Palette。
- 顶部是一行搜索输入 `> query█`。
- 下方是过滤后的命令列表，每行显示 category、命令名称和直接快捷键。
- 第一条匹配命令获得焦点。
- 保存来源页面为 Node List。

**Palette 内操作**：

- 输入字符：按名称、category 或 shortcut 过滤；无 substring 匹配时使用 fuzzy subsequence。
- `Backspace`：删除一个字符。
- `Up/Down`：移动命令焦点。
- `Enter`：执行 focused command 并关闭 Palette。
- `Esc` 或再次 `Ctrl+K`：不执行命令，关闭并精确返回来源页面。

**命令执行后的页面变化**：

| 命令 | 执行结果 |
|---|---|
| Switch to Internet | 关闭 Palette，进入 Internet Node List。 |
| Switch to Private Access | 关闭 Palette，进入 Private Access，不自动连接。 |
| Switch Provider | 进入 Internet Node List，然后打开 Provider Select。 |
| Switch Profile | 进入 Private Access；多个 profile 时打开 Profile Select，单个时直接显示。 |
| Toggle Internet TUN | 关闭 Palette，留在来源页面，启动 TUN transition。 |
| Toggle System Proxy | 关闭 Palette，留在来源页面，启动 system proxy transition。 |
| Trigger Usability Probe | 关闭 Palette，留在来源页面，对 active usability tab 启动 probe。 |
| Refresh Subscriptions | 关闭 Palette，留在来源页面，启动 subscription refresh。 |
| Refresh Connections | 关闭 Palette，留在来源页面，立即刷新连接快照。 |
| View Connections | 关闭 Palette，在来源页面上打开 Active Connections。 |
| View Node Quality | 关闭 Palette；Node List 使用 browsed node，Node Dashboard 使用 current route node。 |
| Settings | 关闭 Palette，在来源页面上打开 Settings。 |
| Help | 关闭 Palette，在来源页面上打开 Help。 |
| Enter Node Dashboard | 关闭 Palette，进入 Node Dashboard。 |
| Quit | 关闭程序。 |

不可执行的命令必须显示 disabled 原因或执行后显示明确 status，不能静默无响应。

Palette 中 `Trigger Usability Probe` 的 shortcut badge 必须是 `[U]`，`Refresh Subscriptions` 必须是 `[u]`。workspace 命令不显示 `[Tab]`，因为 `Tab` 不负责切换 workspace。

### NL-10 打开 Settings

**前置状态**：用户位于 Node List。

**用户操作**：按 `o` 或 `s`。

**立即响应**：打开 Settings。

Settings 分为：

- Probes：quick target、sustained target、timeouts、concurrency、verification targets、auto-pick interval。
- Network：System Proxy server、China IP routing。
- Tailscale：enabled、tailnet domain、hostname。
- Private Access：profile、manifest、mode、server、port、username、password/password env、bridge、SonicWall proxy、TLS verify。

**Settings 内操作**：

- `Up/Down` 或 `j/k`：移动字段焦点。
- `Enter`：进入当前字段编辑。
- 编辑时字符键和 `Backspace`：修改值。
- 编辑时 `Enter`：校验并保存。
- 编辑时 `Esc`：放弃当前字段修改，仍停留在 Settings。
- 非编辑时 `Esc`、`o` 或 `s`：关闭 Settings。
- `q`：退出程序。

**保存成功**：更新显示值和 status；需要 restart 的设置只重启本应用拥有的 sing-box。

**保存失败**：保持编辑状态和输入内容，在字段附近显示错误。

**安全要求**：password 在字段列表和编辑状态中都显示掩码。活动 Private Access session 锁定会改变其 transport/profile contract 的字段，并显示锁定原因。

### NL-11 编辑 node-name filter

**用户操作**：按 `/`。

**立即响应**：打开 node-name filter 输入框，预填当前 filter。

**输入规则**：逗号分隔；普通词为 include；`!` 或 `-` 前缀为 exclude。

**操作**：

- 字符键、`Backspace`：编辑。
- `Enter`：应用并关闭。
- `Esc` 或 `Space`：放弃修改并关闭。

**结果**：重新计算当前列表和 auto-pick 候选；不修改 selector 成员，不切换 current route node。

### NL-12 节点 probe

| 用户操作 | 程序响应 |
|---|---|
| `T` | 对当前 selector/tab 范围的候选执行最多三次 quick reachability attempts。 |
| `t` | 对 browsed node 执行 quick assessment，再执行一次 bounded sustained probe。 |
| `U` | 执行 active usability tab 的 criterion；在 `Current selector` 时提示没有 criterion。 |
| `P` | 为 active criterion + selector 开启或关闭 background schedule；manifest 不允许时显示原因。 |

**运行期间**：列表显示进度，UI 保持可操作；probe 不改变 live selector。

**按 Esc**：先暂停/取消正在运行的 probe，不退出程序。

**失败结果**：controller/runtime/cancelled 产生 incomplete，不得把未测节点标记为 unreachable，也不得覆盖仍有效的完整旧结果。

### NL-13 auto-pick

**用户操作**：按 `a`。

**关闭 → 开启**：保存当前 selector、active node-view tab 和 filter，启动或连接 background worker，status 显示开启。

**开启 → 关闭**：停止该 selector/tab 的自动切换并保存状态，status 显示关闭。

**自动切换条件**：候选必须连续赢得两轮、同 tier 至少有 20% 实质改善，并且 current route node 最近没有 active transfer。条件不足时只更新 explanation，不切换。

### NL-14 其他 Node List 快捷键

| 按键 | 程序响应 |
|---|---|
| `r` | 从 controller 刷新 groups、mode 和 connection state，保持可恢复的焦点。 |
| `u` | 强制启动一次 subscription refresh；已有任务时只提示进行中。 |
| `m` | 循环 Clash mode，成功后刷新显示。 |
| `v` | 后台执行配置的 network verification targets。 |
| `x` | 异步开关 OS System Proxy。 |
| `\` | 异步开关 Internet TUN；需要权限时进入平台授权流程。 |
| `b` | 打开 Bypass Editor。 |
| `q` | 正常退出并停止本次前台会话拥有的 managed runtime/worker。 |
| `B` | 退出 TUI，但保留 managed sing-box、auto-pick 和活动 Private Access sessions。 |

TUN 或 System Proxy transition 进行中时，重复操作和 `q/B` 被拒绝，status 说明必须等待完成。

## 6. Node Dashboard

### ND-01 页面内容

**前置状态**：用户通过 Command Palette 主动进入。

**程序响应**：

- 页面主体始终是 current route node，不是进入前的 browsed node。
- 120×30 显示 current-route summary、current-node quality、Active connections、aggregate latency 和 Core traffic。
- 空间不足时先隐藏 Active connections，再隐藏 current-node quality；`c` 和 `i` 仍可打开详情。
- 80×24 只保留 route summary 和 global charts。
- 页面填满 footer 以上的剩余空间。

### ND-02 返回 Node List

**用户操作**：按 `Enter`。

**程序响应**：立即返回进入 Dashboard 前对应 workspace 的 Node List；恢复 provider、tab 和 browsed node。

### ND-03 在 Dashboard 打开弹窗

| 按键 | 响应 | 关闭后的页面 |
|---|---|---|
| `?` | 打开 Help。 | Node Dashboard |
| `c` | 打开 Active Connections。 | Node Dashboard |
| `i` | 打开 current route node 的 Node Quality。 | Node Dashboard |
| `o` / `s` | 打开 Settings。 | Node Dashboard |
| `Ctrl+K` | 打开 Command Palette，来源记录为 Node Dashboard。 | 取消时回 Node Dashboard |
| `q` | 退出程序。 | 无 |

**其他未绑定键**：不返回 Node List，不被当作“唤醒键”，不产生状态变化。

### ND-04 Dashboard 监控数据变化

- route node 改变时，summary 立即更新，并开始新的 route interval。
- A→B→A 产生三个 interval，不合并前后两个 A。
- Core traffic 显示 sing-box core 实际提供的 upload/download；不称为 whole-machine、proxy-only、per-node 或 subscription quota。
- 未采样时段保留 gap，不补零，不跨 gap 连线。
- Dashboard 不为了画图启动 sustained download probe。

## 7. Private Access

### PA-01 进入 Private Access

**只有一个 profile**：直接显示该 profile 的页面，不显示 Profile Select，不自动连接。

**多个 profile**：进入上次 focused profile；用户可按 `p` 打开 Profile Select。

**没有 profile**：显示未配置状态和进入 Settings 的提示；`V` 不启动连接。

### PA-02 打开并确认 Profile Select

**用户操作**：按 `p`。

**立即响应**：

- 多个 profile：打开 Profile Select；当前 focused profile 获得初始焦点，已连接状态独立显示。
- 一个 profile：不打开无意义弹窗，status 说明当前已是唯一 profile。
- 零个 profile：不打开弹窗，status 说明未配置。

**弹窗内操作**：

- `Up/Down` 或 `j/k`：移动焦点。
- `Enter`：确认 focused profile。
- `Esc` 或 `p`：取消。
- `q`：退出程序。

**按 Enter 后**：关闭 Profile Select，返回 Private Access，显示所选 profile 的状态和资源；不连接它，也不切断其他 profile。

### PA-03 按 V 连接或断开

**用户操作**：按 `V`。

| focused profile 当前状态 | 程序响应 |
|---|---|
| Disabled / Disconnected / Error | 打开 Progress，显示正在连接，启动 connect。 |
| Connecting | 打开 Progress，显示正在断开，启动 disconnect。 |
| Connected | 打开 Progress，显示正在断开，启动 disconnect。 |
| Disconnecting | 打开/更新 Progress，显示正在等待，不启动第二个任务。 |

操作只作用于 focused profile。程序不得为了连接它而自动断开其他 profile。

### PA-04 连接过程不需要认证

**前置状态**：Progress 正在显示连接事件。

**后端返回成功**：

1. profile 状态变为 Connected。
2. Progress 显示成功事件和已取得的真实 session/capabilities。
3. 用户按 `Enter` 或 `Esc` 关闭 Progress。
4. 返回 Private Access connected overview。

**后端返回失败**：

1. profile 状态变为 Error。
2. Progress 显示具体失败原因。
3. 关闭后回到 error 页面。
4. 页面不得显示旧的 active DNS 或 Routes。

### PA-05 连接过程要求认证

**前置状态**：后端发出 authentication challenge。

**程序响应**：在 Progress 上方或替换 Progress 显示 Authentication；字段来自真实 challenge，不使用 Figma demo 字段作为固定生产字段。

**Authentication 内操作**：

- `Tab/Down`：下一字段。
- `Shift+Tab/Up`：上一字段。
- `Left/Right`：切换当前 option 字段的选项。
- 字符键、`Backspace`：编辑当前文本字段。
- `Enter`：进入下一字段；最后一项上提交。
- `Esc`：取消认证并请求结束这次连接。

**显示要求**：password/sensitive 字段始终掩码；普通字段正常显示；caret 与字段值处于同一行。

**必填项为空时提交**：不关闭 Authentication；焦点移到第一个缺失字段，在其附近显示 `<field> is required`。

**提交成功**：关闭 Authentication，回到 Progress 显示后续连接事件；最终成功后进入 connected overview。

**提交失败**：Authentication 保持打开或按后端事件进入 Error；用户已经输入的非敏感值按 challenge 生命周期保留，secret 不写入普通日志。

### PA-06 Connected overview 的 DNS 和 Routes

**前置状态**：focused profile 为 Connected。

**页面显示**：session summary、capabilities、DNS summary、Routes summary；只显示当前 session 实际提供的数据。

**用户操作**：

- `Tab`：在 DNS 和 Routes 两个 section 之间移动焦点。
- `Enter`：展开或收起 focused section。
- `Up/Down` 或 `j/k`：滚动当前 details pane。

**展开后**：summary 区域变窄，右侧出现对应 details pane。收起后 summary 恢复全宽。展开/收起不改变连接状态。

**DNS 和 Routes 都展开**：两个 section 保留各自状态，当前 focused section 有唯一焦点样式；页面不得出现永久 profile sidebar。

### PA-07 Error 页面

**前置状态**：连接或活动 session 失败。

**程序响应**：在 session/capabilities 区域下方显示 error summary 和可用 detail；DNS/Routes 标为 unavailable，不显示旧 session 的有效样式。

**用户操作**：

- `V`：重新连接。
- `p`：选择其他 profile。
- `o/s`：打开 Settings。
- `?`：打开 Help。

### PA-08 在 Private Access 按 i

**用户操作**：按 `i`。

**程序响应**：不打开 Private Access 的伪 Node Quality；status 显示 `Node quality is available for Internet Proxy nodes only`。用户可通过 Command Palette 回到 Internet 后查看。

### PA-09 活动 session 下打开 Settings

**用户操作**：按 `o` 或 `s`。

**程序响应**：打开 Settings；当前 profile 的 identity、manifest、mode 和其他会改变活动 session contract 的字段显示 locked。

**用户选择 locked 字段并按 Enter**：不进入编辑，显示 `Disconnect this profile before changing ...`。程序不得静默断开 session。

## 8. Active Connections

### AC-01 正常数据

打开后显示当前快照。连接数或速率变化时更新对应行；已有焦点和滚动位置不因刷新跳回顶部。

### AC-02 空状态

controller 正常但没有连接时显示 `No active connections`，不把它显示成错误。

### AC-03 controller 错误

保留弹窗，显示刷新错误和最后成功刷新时间；不得把旧连接冒充为当前数据。

### AC-04 关闭

`c`、`Esc` 或 `Enter` 关闭并返回来源页面；`q` 退出整个程序；`r` 只刷新，不关闭。

## 9. Node Quality

### NQ-01 来源决定查看对象

| 来源 | `i` 查看对象 |
|---|---|
| Node List | browsed node |
| Node Dashboard | current route node |
| Private Access | 不打开，显示 Internet-only status |

打开时冻结 selector/node identity；后台刷新只更新这个节点的数据，不因背景页面焦点变化而换节点。

### NQ-02 滚动和关闭

`Up/Down` 或 `j/k` 滚动；`i`、`Esc` 或 `Enter` 关闭；关闭后精确返回来源页面。

### NQ-03 数据缺失

没有某项测量时显示 `—`、`not measured` 或 gap；必须保留单位和测量时间，不能用单个 latency 代表整体 Node Quality。

## 10. Help

### HP-01 内容

Help 必须显示实际 handler 使用的快捷键，并按当前上下文标明是否可用。footer、Help、Command Palette shortcut badge 和 handler 必须来自同一份 command 定义。

### HP-02 操作

`Up/Down`、`j/k` 移动；`g/G` 首尾；`?`、`Esc`、`Enter` 关闭；`q` 退出程序。

### HP-03 Diagnostics

存在无效 usability manifests 时，在独立 Diagnostics 区显示文件路径和错误；没有 diagnostics 时不显示空警告区。

## 11. Settings

### SE-01 选择字段

打开后 focused row 位于上次选择的仍可见字段；`Up/Down` 或 `j/k` 移动；列表自动滚动保证 focused row 可见。

### SE-02 编辑字段

`Enter` 进入编辑；输入和 `Backspace` 修改；`Enter` 校验保存；`Esc` 取消本字段修改。

保存失败时输入不丢失。保存成功后退出编辑态，但 Settings 保持打开。

### SE-03 关闭

非编辑状态下 `Esc`、`o` 或 `s` 关闭并返回来源页面；`q` 退出程序。

### SE-04 Password

Private Access password 的列表值和编辑值都显示掩码。保存时可以使用真实输入，渲染、status、error 和日志不能回显 secret。

### SE-05 条件字段

- Private Access 未配置：隐藏 Private Access 字段。
- 非 SonicWall profile：隐藏 `SonicWall use Internet proxy`。
- 活动 session：锁定会改变该 session contract 的字段。
- 启用 China IP routing：先下载/验证 rulesets，失败时保留旧值。
- 启用 Tailscale：缺少所需 domain 时拒绝保存并停留在编辑态。

## 12. Command Palette

### CP-01 搜索

每次输入后立即过滤；结果为空时显示 `No matching commands`。过滤后 focused row 总是位于有效结果内。

### CP-02 取消

`Esc` 或再次 `Ctrl+K` 关闭；来源页面及其所有 view state 不变。

### CP-03 执行

`Enter` 执行 focused command。执行失败时 Palette 关闭或保持的选择必须统一：本规格要求关闭 Palette、回到来源页面，并在 status 显示失败原因；需要用户继续选择的 Provider/Profile 命令除外，它们打开相应 Select 弹窗。

## 13. Bypass Editor

### BY-01 打开和编辑

Node List 按 `b` 后打开输入框，预填现有 domains、IPs 和 CIDRs。字符键和 `Backspace` 编辑。

### BY-02 保存

`Enter` 校验并保存 runtime state 和 local rule-set。成功后关闭并返回来源页面；新连接使用新规则，现有连接不宣称已经改道。

### BY-03 取消或失败

`Esc` 或 `Space` 放弃修改。非法条目保存失败时输入框保持打开并显示错误。

## 14. Subscription Refresh

### SU-01 自动刷新

存在 `.suburl` 时，启动时检查一次，此后按日检查。任务在后台运行，不能冻结 TUI。

### SU-02 手动刷新

按 `u` 或从 Command Palette 执行 Refresh Subscriptions。没有任务时启动；已有任务时不创建第二个，只显示进行中。

### SU-03 完成

- 成功写入配置前保存一个固定 backup。
- 配置写入成功后说明 running core 是否仍需 reload/restart。
- 节点新增、删除或 fingerprint 改变时，暂停把旧 runtime 的质量结果归给新配置，直到 reload/restart。
- 失败时保留原配置和原质量事实，并显示失败原因。

## 15. System Proxy、TUN 和 Verification

### NW-01 System Proxy

按 `x` 后立即显示 transition 状态，后台应用 OS proxy。成功后右下 system status 更新；失败后保持原状态并显示错误。

### NW-02 Internet TUN

按 `\` 后显示 transition 状态；需要管理员权限时使用平台授权流程。成功后只重启当前 `ManagedSingBox` 拥有的进程并更新状态；失败时恢复到可重试状态。

### NW-03 Network verification

按 `v` 后后台检查配置的 targets；页面保持可操作。完成后 status 给出成功/失败摘要，详细错误可在 Help/Diagnostics 或日志中定位。

### NW-04 Clash mode

按 `m` 循环到下一个可用 mode；controller 成功确认后才更新页面。失败时保持原 mode。

## 16. 退出

### EX-01 正常退出

无弹窗时按 `q` 或 `Esc`：停止本次前台会话拥有的 managed sing-box 和 worker，然后恢复终端并退出。

如果 probe 正在运行，第一次 `Esc` 只暂停 probe；再次 `Esc` 才退出。

### EX-02 保留后台退出

按 `B`：保存必要状态，保留 managed sing-box、auto-pick worker 和活动 Private Access sessions，恢复终端并退出。下次启动只重连有 ownership 证据的后台进程。

### EX-03 transition 期间

TUN/System Proxy transition 期间按 `q`、`Esc` 或 `B`：不退出，status 显示必须等待 transition 完成。

## 17. 当前实现与本规格不一致的地方

这些是后续实现任务，不是允许存在的另一套行为：

1. 当前代码中 `Tab` 循环 workspace；本规格要求 workspace 只通过 `Ctrl+K` 切换。Node List 的 `Tab` 无操作，Private Access Connected 的 `Tab` 在 DNS/Routes 之间切换焦点。
2. 当前 Help/README 把 `p` 写成 System Proxy；实际和本规格为 `p = Provider/Profile Select`、`x = System Proxy`。
3. 当前 Command Palette 把 usability probe 标成 `[u]`、subscription refresh 标成 `[U]`，两者写反。
4. 当前 Settings 和 Authentication 会显示 password 原文；本规格要求始终掩码。
5. 当前 first-run 只有 Subscription Setup，没有按 workspace 配置数量执行 Workspace Chooser。
6. 当前 Private Access 用滚动位置和“超过 10 项”决定 DNS/Routes 折叠；本规格要求 `Tab` 选择 section、`Enter` 展开 details pane。
7. 当前从 Node Dashboard 按 `i` 可能沿用 Node List 的 browsed node；本规格要求查看 current route node。
8. 当前 Dashboard 只处理一部分全局快捷键；必须让实际 handler 与 Dashboard footer 完全一致。
9. 当前 Help、footer、Command Palette 和 README 分别维护快捷键文本，已经发生漂移；必须改为同源定义。

## 18. Figma 对照

| 界面 | Figma frame |
|---|---|
| Implementation Handoff | `1014:2` |
| Node List | `8:2`, `73:277` |
| Provider Select | `796:7` |
| Workspace Chooser | `953:3`, `953:25` |
| Private Access states | `595:2`, `596:2`, `596:226`, `596:348`, `596:110`, `73:2`, `596:168`, `133:2` |
| DNS / Routes expanded | `599:2`, `599:62`, `602:102` |
| Profile Select | `998:2` |
| Node Dashboard | `1025:2`, `1027:16` |
| Active Connections | `600:592` |
| Node Quality | `600:883` |
| Settings | `600:2` |
| Help | `600:288` |
| Command Palette | `635:2` |
