# tty7 CLI · 终态设计

不考虑历史包袱的目标形态。每一条都对照过当前实现(control 协议、pane 协议、路由、安装机制),
"能落地"的判断以代码为准,不是愿景。

## 一句话

一个名字 `tty7`,三个 artifact:CLI、GUI、server。GUI、CLI、coding agent 是 server 的三个平等客户端,
谁不在场都不影响另外两个。

## 形体

```
tty7          薄 CLI。console 二进制,用户 PATH 里的就是它。control/pane 协议的客户端。
tty7(GUI)   app bundle / GUI 子系统 exe。展示名 tty7,文件名是实现细节(tty7-app)。
tty7-server   每台机器上的 server。本地、WSL、SSH 远端一视同仁,本地就是 0 号机器。
```

为什么名字同一、文件必须分开(与历史无关的三个硬理由):

1. **Windows 子系统二选一**:GUI 子系统 exe 在 PowerShell 里没有 stdout;console exe 从开始菜单启动闪黑窗。
2. **升级文件锁**:GUI/server 常驻时 exe 被锁;CLI 与它们同文件就无法覆盖升级。
   server 用方言命名副本(`tty7-server-c{control}p{protocol}`)并存,新旧共存到用户确认重启——本地远端同一套机制。
3. **重量**:CLI 要毫秒级冷启动;GUI 链着 gpui/字体/渲染;server 是 6MB 静态二进制。

落盘:macOS 是 tty7.app,`/usr/local/bin/tty7` 软链进 bundle(zed 方式);Windows 安装目录放 GUI exe,
PATH 上放 `tty7.exe`;Linux 同理,.desktop 的 Name=tty7。server 由 GUI/CLI 按方言装到
`~/.local/share/tty7/bin/`,本地与远端走**同一条 install 路径**。

GUI 不再兼任 server:没有 `--daemon` 角色,启动时和连远端一样走 install→launch→connect。
收益:install/launch/supervise/版本握手只剩一份实现;server 崩溃域与 GUI 隔离;
Windows 上"daemon 锁住 GUI exe 无法升级"的问题结构性消失。

`agent-hook` 需要稳定入口(hook 命令行持久化在用户的 agent 配置里,而 server 路径带方言号):
装一个不带版本号的垫片 `~/.local/share/tty7/bin/tty7-agent-hook`,永远指向当前方言的 server。

## 语法

主语法 `tty7 <名词> <动词>`,热路径给顶层别名。地址采用 tmux 惯例:`%42` = pane,`@7` = tab,
workspace 用名字或 id。

```
# 热路径(顶层别名)
tty7 ls                          # = tty7 ws ls,一屏看完这台机器
tty7 attach %42                  # 接管一个 pane,raw mode
tty7 run -- cargo test           # 起一个 pane 跑命令,流式输出,透传 exit code
tty7 new [path]                  # 建 workspace + 第一个 tab,打印 id

# 名词
tty7 ws      ls | tree <ws> | new | rename <ws> <name> | stop <ws> | rm <ws> | attach <ws> | detach <ws>
tty7 tab     ls <ws> | new <ws> [--cwd] | close @7 | rename @7 <name> | move @7 <idx>
tty7 pane    ls [<ws>] | split %42 --h|--v [--ratio 0.5] | close %42
tty7 send %42 'make -j8' [--enter]
tty7 capture %42 [--scrollback]          # 范围 = server 端 ring buffer
tty7 procs %42
tty7 machine ls | connect <profile> | disconnect <machine>
tty7 server  start | stop | restart | status | logs
tty7 agents                      # 全机 agent 面板:哪个在跑、哪个在等回话
tty7 events [--json]             # 事件逐行流出(NDJSON)
tty7 status                      # = tty7 server status
tty7 doctor

# 全局
tty7 -m <machine> <anything>     # 路由到那台机器,由本地 server 的既有 route 转发
--json    每个动词可机器读     -q    安静模式
```

`tty7` 裸敲 / `tty7 <path>`:唤起或激活 GUI。其余一切不需要 GUI 在场。

**隐式上下文**:server 给每个 shell 注入 `TTY7_SOCKET` / `TTY7_WS` / `TTY7_PANE`
(今天已注入 `TTY7`=version,注入点现成)。在 tty7 的 shell 里热路径免写地址:
`tty7 split --v` 分割当前 pane,`tty7 run` 在当前 workspace 开 pane。

`tty7 doctor` 检查单:socket 可达 + 方言握手;config 解析(含 UTF-8 BOM 检测);
PATH 上的 CLI 与 GUI 版本一致性;agent hooks 安装状态;每条 remote link 状态;`$TTY7_*` 上下文在不在。

## 实现映射(对照过代码,三档)

### 免费 —— 协议已有,CLI 只是发请求

| CLI | 已有机制 |
|---|---|
| ws/tab/pane 全部结构动词 | `ControlRequest`:MachineGet、WorkspaceTree/Create/Rename/Remove/Attach/Detach、TabCreate/Close/Rename/Move、PaneSplit/Close/Move/Replace(control.rs,全齐) |
| ws 名字、离开 GUI 可用 | 树和名字权威在 server(machine.json),GUI 只是 mirror |
| `events` | ControlEvent{PaneExited, AgentStatus, Preempted, Layout, …} 已经推给 control 订阅者(tree_sync 就靠它活着);CLI = 保持连接、逐帧打印 |
| `procs` | `DaemonMsg::Procs(PaneProcs)` 已有 |
| `run` 的 exit code | `DaemonMsg::Exited{code}` + Prompt.last_exit 已有 |
| `-m` 远端路由 | ROUTE_KIND 帧 + `SshManager::global()` 在 **server 进程**里(server.rs),RouteTarget::Ssh/Wsl/LocalStdio 齐;SSH 连接本来就不归 GUI 持有 |
| 方言安装、版本握手 | install/(cXpY 命名、下载/上传/校验/握手)照搬到本地 |
| `send` | pane socket `ClientMsg::Input` |

### 组合 —— 原语齐,CLI 侧拼装

- `run` = Spawn(带 cwd/ws)→ 订阅 Output 流 → Exited{code} 透传;`--keep` 保留 pane,默认退出即收。
- `attach` = pane socket Attach + Snapshot 回放 + 双向 raw mode(termios/ConPTY),detach 键约定 `C-\ C-\`。
  **决定:暂不实现。** CLI 的主用户是 coding agent(run/send/capture/events 已覆盖);attach 只服务
  "无 GUI 的机器上人要接管 shell"这一个场景,底层(PaneSession 双半泵、Observe、Snapshot 回放)已就绪,
  要用时补一个 raw-mode 控制台模块即可。现状:`tty7 attach` 明确报未实现。
- CLI 的 control/pane 客户端代码在 tty7-core 里已有(验收测试的 control-client 即此),抽成 `tty7_core::client` 共享。

### 新增 —— 真缺口,都是小改

1. **pane 多订阅**。现状 `subscriber: Option<Sender<_>>`(pane.rs)——单订阅,新 attach 顶掉旧的;
   CLI `capture`/`attach` 会把 GUI 踢下线。改成 1 个 controlling subscriber(可写入)+ N 个只读 observer;
   `capture` = observer 进、收 Snapshot、走人。这是列表里唯一动到并发结构的改动。
2. **上下文注入**。`TTY7_PANE`(spawn 时已知)、`TTY7_WS`(Spawn 请求补 workspace 字段,fork 时注入)、`TTY7_SOCKET`。
3. **聚合查询**,各一个小请求:`ControlRequest::AgentStates`(server 已持有各 pane 状态,只差查询口)、
   `Routes`(活跃 link + config profile,供 machine ls)、`Status`(uptime/pane 数;Version 已有)。
4. **`tty7 run` 的 Spawn 带 workspace/tab 归属**,让树和 pane 一次落位(今天 owner 字段已预留位置)。

## 词汇与代码命名(终态)

| 层 | 词 |
|---|---|
| 二进制 | `tty7`(CLI)/ `tty7`(GUI,展示名)/ `tty7-server` |
| 代码模块 | `tty7_core::server`(进程侧,原 `daemon` 模块整体改名)、`core`(领域)、`client`(CLI/GUI 共用客户端) |
| UI | Machine / Server / Workspace / Shell / Connection |
| CLI 动词 | 与 UI 动词一一对应:`ws stop` = Stop Workspace(结束 shell、留布局),`ws rm` = Delete Workspace,`ws detach` = 关窗不打断,`machine disconnect` = Disconnect |

`daemon` 一词从全部四层(二进制、模块、文案、日志)消失。

## Crate 布局

```
crates/tty7-core      协议、server 实现、client 库(现状 + daemon→server 改名)
crates/tty7-server    headless 入口(现状,CLI 化:run|stdio|agent-hook|protocol)
crates/tty7           CLI(新,薄,依赖 tty7-core::client + clap)
crates/tty7-gui       GUI(现 src/,bin 名 tty7-app,bundle 内置 tty7-server 与 tty7)
```

## 落地顺序

1. `tty7_core::client` 抽出(control+pane 客户端,现有测试驱动代码升格)。
2. CLI crate:先做免费档(ls/tree/new/rename/rm/split/close/send/procs/events),立刻可用、立刻给 agent 用。
3. 新增档四件小事(多订阅、env 注入、聚合查询、Spawn 归属)→ attach/capture/run/agents/machine 补齐。
4. GUI 去 server 化:本地走 install 路径,bundle 内置 server;spawn.rs 与 install/ 合流。
5. 模块改名 daemon→server、二进制布局落位、agent-hook 垫片。

1–3 不动 GUI 一行,4–5 才是结构手术;两段可独立发布。
