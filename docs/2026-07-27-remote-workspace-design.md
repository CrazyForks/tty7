# tty7 远程开发：远程 Workspace 设计

> 状态：设计定稿，待实现
> 日期：2026-07-27

## 1. 一句话

选一台开发机，tty7 给你一个整体就是那台机器的窗口。合上笔记本、重启、断网、换台电脑——里面的东西一直在跑，回来还在原地。

主卖点是 **agent 不再因为合盖而中断**。丢个 shell 忍忍就过去了，丢一个跑了四十分钟的 agent 会话不行。

对老用户还有第二条：**远程终于不再是残的**。今天 SSH 上去，repo 分组、分支、diff、file tree、worktree 全部消失；远程 workspace 里它们全都在。

## 2. 用户模型：只有一条规则

**一个窗口 = 一台机器的一个 workspace。**

- 一个 remote host 可以跑多个 workspace，跟本地一样。
- 窗口里所有 tab 和 pane 都在那台机器上，不混。
- 同一台机器可以开好几个窗口；几台机器的窗口并排也行；本地窗口和远程窗口并排也行。

### 与 SSH pane 的区别

这是两个功能，不要混：

| | 连一下（SSH pane） | 在上面开发（远程 workspace） |
|---|---|---|
| 干嘛 | 看个日志、重启个服务 | 写代码 |
| 单位 | 一个 pane | 一个窗口 |
| 关掉之后 | 没了 | 还在跑 |
| 入口 | 命令面板 | 首页「连接主机」 |

远程 workspace 里不提供连别的机器的入口。用户自己在 shell 里敲 `ssh` 当然照样能用，但 tty7 不识别、不接管。

**机器只配一次**：远程 workspace 直接用已存的 SSH 配置（`core::ssh_profile` 的 profile、`~/.ssh/config` 的 alias），密码、密钥、跳板机全都现成。不新做一套主机配置 UI。

## 3. 目标与非目标

### v1 做

- Mac / Linux / Windows 都能当客户端
- 连 Linux 机器；Windows 用户连自己的 WSL 也算
- 服务自动装
- 一台机器多个窗口、多台机器并存、和本地窗口混着用
- 断线自动重连
- repo 分组、分支、diff、worktree、agent 全套在远程可用
- 文件浏览、端口转发

### v1 不做

- **两台电脑同时连同一个 workspace** —— 先后连没问题；撞上时是**接管**（§10），不是共享
- **拿 Windows 当被连的机器** —— 用 WSL
- **一个窗口里既有本地又有远程** —— 这个**永远不做**
- **自动同步文件、自动猜端口** —— 手动就够了
- 远程 workspace 里连别的机器的入口
- 远程读远程的 `config.json`（§13）
- 断线超出 replay ring 的输出的持久化补偿（§10）
- 客户端没运行时的推送通知

## 4. 现状盘点

### 已经成立的地基

| 能力 | 在哪 | 对远程的意义 |
|---|---|---|
| 持久 daemon：一条连接一个 pane，`Attach`/`Detach`，断连即 detach、pane 继续裸跑 | `daemon/server.rs`、`daemon/pane.rs` | "合上笔记本还在跑"在本地已经成立，远程要做的是把这个 daemon 挪到对面 |
| `ReplayRing`：断连期间的输出进环形缓冲，attach 时重放 | `daemon/pane.rs` | 重连补屏直接可用 |
| `PROTOCOL_VERSION` 握手 + 不兼容时询问用户 | `daemon/spawn.rs::ensure_running` | 远程版本 skew 照搬 |
| 传输抽象：`Stream = Read + Write + try_clone` | `daemon/transport.rs` | 多一种传输形态不破坏上层 |
| 原生 SSH 栈：连接复用（`ConnectionKey`）、`direct-tcpip`、session channel、SFTP、known_hosts、auth broker、jump / ProxyCommand / SOCKS5 | `daemon/ssh/*` | 远程 workspace 的传输层几乎白送 |
| Workspace 模型：`Workspace` = 一组 tab + 窗口几何 + `open` 标记 + 名字，home 页 picker | `core/session.rs` | 远程模型 1:1 照搬，不新造概念 |
| agent 状态：hook 发 OSC 777 → daemon 侧 sniffer → 客户端 | `core/agent_hooks.rs`、`daemon/pane.rs` | PTY 在哪 sniffer 就在哪，远程天然成立 |

### 反着的那块

右侧那套富功能全部直接读 **GUI 进程自己的**文件系统和 git：

- `ui/file_tree.rs:120` —— `std::fs::read_dir`，注释明写 "no daemon round-trips (the SFTP panel covers the remote case)"
- `ui/app.rs:3895`、`core/worktree.rs:65` —— `Command::new("git")`，跑在客户端
- gitignore 判定、`notify` 文件监听、repo 根上溯（`ui/file_tree.rs:628` 的 `.find(|p| p.join(".git").exists())`）也都是本地 fs

所以"全套在远程可用"不是把 daemon 挪过去就顺带有的，它是本设计里最大的一块（§8）。

## 5. 关键决策一览

| # | 决策 | 选了 | 否掉了 | 为什么 |
|---|---|---|---|---|
| D1 | 富功能远端化 | 抽 `Host` 抽象层，本地直调 / 远程 RPC | 窄推送 + 复用 SFTP 面板 | 窄方案下 file tree 没有 gitignore 和文件监听，diff overlay 和 code editor 在远程缺失或另写一套，长期两条代码路径并存，最后还得推倒 |
| D2 | 远程二进制 | 拆出 headless crate，远程只装 `tty7-server` | 远程跑完整 `tty7 --daemon`；同 crate 加 cargo feature | 完整二进制含 gpui/字体/资源且在无头 Linux 上可能因缺 libfontconfig 起不来，而无头机正是目标场景；cargo feature 方案会让 `#[cfg]` 撒遍 `ui/` 和 `core/` |
| D3 | 谁开 SSH 连接 | 本地 daemon 当转发中枢，GUI 传输层不动 | GUI 内嵌 russh 直连 | SSH 引擎、auth broker、known_hosts、jump 链、端口转发全在本地 daemon；GUI 直连意味着两套 SSH 引擎并存 |
| D4 | 通道形态 | 每条逻辑流一条 SSH channel，首选 `direct-streamlocal` | 自建多路复用层 | russh 客户端侧有 `channel_open_direct_streamlocal`（`client/mod.rs:854`），远程零 bridge 进程、零 mux 代码 |
| D5 | 二进制怎么上去 | 客户端下载 + SFTP 上传 | 远程 curl；两者都做并回退 | 内网 / 跳板机后面的机器上不了外网，而那是一大类目标用户；双路径的失败回退边界很难调对 |
| D6 | 断线时的窗口 | 只读降级 + 状态条 | 整窗遮罩；缓存输入重连后发 | 断线那一刻最想看的就是 agent 断之前输出了什么；缓存输入会在看不见的时候落到一个已经变样的屏幕上 |
| D7 | 启动时 | 即连，认证 sheet 排队一次弹一个 | 开窗不连等点击；按凭证类型分情况 | "回来还在原地"不该变成"回来再点一下"；按凭证分情况会让同一个动作在不同机器上行为不同 |
| D8 | 两个客户端撞上 | 后来者接管，先来的转 `Preempted` 只读 | 拒绝后来者；并存只读旁观 | 最常见的撞车是"旧机器忘了关"，拒绝等于把人锁在门外；并存只读实质就是在做多客户端 |
| D9 | WSL | 单独一条 stdio 传输 | 要求 WSL 里跑 sshd；v1 不做 WSL | 为一个本机上的发行版配 sshd 很荒谬，也拆了"服务自动装"的台；stdio 传输还顺带让端到端测试不需要 sshd（§17） |
| D10 | Linux 二进制链接方式 | musl 静态链接 | glibc 动态链接 | 一个二进制通吃所有发行版，不看目标机的 glibc 版本 |

## 6. 架构总览

```
┌─ 客户端 GUI（gpui） ─────────────────────────────┐
│  TerminalView×N    file_tree / git_diff /        │
│        │           worktree / code_editor        │
│        │                    │                    │
│        │              Host trait ◄── 新           │
│        ▼                    ▼                    │
│  RemoteTerminal（现有）  LocalHost │ RemoteHost   │
└────────┴────────────────────┴────────────────────┘
         │ 现有 transport：UDS / loopback TCP，不动
         ▼
┌─ 本地 daemon ────────────────────────────────────┐
│  本地 pane（PTY）· SSH pane · SFTP · 端口转发     │
│  SshManager / PromptBroker / known_hosts / jump  │
│  ── 全部现有，远程 workspace 直接复用 ──          │
│  RemoteRouter ◄── 新：纯字节转发，不解析          │
└────────┬─────────────────────────────────────────┘
         │ SSH：每条流一条 channel
         │   首选 direct-streamlocal → 远程 daemon.sock
         │   回退 session channel + exec tty7-server --stdio
         │ WSL：wsl.exe 子进程的 stdin/stdout
         ▼
┌─ 远程 tty7-server（headless，无 gpui） ──────────┐
│  pane registry（DaemonPane，现有代码原样搬）      │
│  workspace store ◄── 新：布局的权威副本存这里     │
│  Host 服务端 ◄── 新：fs / git / watch RPC        │
└──────────────────────────────────────────────────┘
```

**GUI 侧传输代码一行不改**。`transport::Stream` 仍然是那条本地流；`Spawn` / `Attach` / control 消息多带一个路由头，说明"去哪台机器"。本地 daemon 对远程流只做字节转发，不解析内容。

## 7. 传输层

### 7.1 SSH 主机

每条逻辑流一条 SSH channel，不自建多路复用：

| 流 | channel 数 | 说明 |
|---|---|---|
| 每个 pane | 1 | 对应现有"一条连接 = 一个 pane" |
| 每个远程 workspace 的控制流 | 1 | Host RPC + workspace store + 事件推送 |

**首选** `direct-streamlocal@openssh.com` 直接接到远程的 daemon socket。OpenSSH 的 `AllowStreamLocalForwarding` 默认为 `yes`。

**回退**：被管理员关掉时（channel open 失败），改用 session channel `exec tty7-server --stdio`——一个纯字节转发的小进程，把自己的 stdin/stdout 接到同一个 unix socket。回退是每连接一次性探测，结果缓存在 `SshConnection` 上，不逐 channel 重试。

同一台机器的多个 workspace 共用一条 `SshConnection`（现有 `ConnectionKey` 的复用逻辑直接生效）：一台机器只认证一次。

### 7.2 远程 socket 路径

`$XDG_RUNTIME_DIR/tty7/daemon.sock`，没有 `XDG_RUNTIME_DIR` 时退到 `~/.local/share/tty7/daemon.sock`。`sun_path` 长度限制的 fallback（短路径 + 配置目录哈希）沿用 `transport.rs` 现有实现。

一台机器**一个** `tty7-server`（per user），多个 workspace 在它内部；socket 权限 0600，目录 0700。

### 7.3 WSL

`wsl.exe -d <distro> -- tty7-server --stdio`，子进程的 stdin/stdout 就是 `Stream`。无 SSH、无认证、无网络。

## 8. 协议扩展

现有协议是"一条连接一个 pane"，控制类只有短连接 `List`。Host 层要的是长连接上的请求/响应，量大且并发。

新增一条 **control 连接**，`PROTOCOL_VERSION` bump 到 **3**。

### 帧格式

沿用外层 `[u32 LE payload_len][u8 kind][payload]`，control 连接的 kind 是新值：

| 形态 | payload 布局 | 用于 |
|---|---|---|
| 小请求 / 响应 | `[u64 req_id][JSON]` | `read_dir`、`stat`、`git`、`repo_root`、workspace 读写 |
| 大 payload | `[u64 req_id][raw bytes]` | `read_file` / `write_file` 的文件内容 |
| 事件推送 | `[u64 req_id = 0][JSON]` | 文件变更、pane 死亡、agent 状态、被接管通知 |

`req_id` 允许乱序匹配，所以一个慢的 `git` 调用不会堵住 file tree 的目录展开。`req_id == 0` 保留给无请求对应的服务端推送。

热路径（pane 的 `Input` / `Output` / `Snapshot`）不走 control 连接，保持现有的零序列化直传。

## 9. Host 抽象层

```rust
pub trait Host: Send + Sync {
    fn read_dir(&self, p: &Path) -> io::Result<Vec<Entry>>;   // Entry 带 ignored 标记
    fn stat(&self, p: &Path) -> io::Result<Meta>;             // 含 mtime
    fn read_file(&self, p: &Path) -> io::Result<Vec<u8>>;
    fn write_file(&self, p: &Path, b: &[u8]) -> io::Result<()>;
    fn create_dir(&self, p: &Path) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove(&self, p: &Path, recursive: bool) -> io::Result<()>;
    fn repo_root(&self, p: &Path) -> io::Result<Option<PathBuf>>;
    fn git(&self, cwd: &Path, args: &[&str]) -> io::Result<Output>;
    fn watch(&self, dirs: &[PathBuf]) -> WatchSub;
}
```

**同步阻塞签名是刻意的。** 这些调用点现在全部已经在 background executor 上跑（`file_tree.rs` 的注释：render 只读缓存，miss 变成排队加载），保持阻塞语义意味着调用点的结构一行不用动，只换实现来源。

`LocalHost` 直调 `std::fs` / `Command::new("git")`，零开销、零往返。`RemoteHost` 走 control 连接的 RPC。

### 三个为"每次往返都要钱"而变形的方法

| 方法 | 天真做法的问题 | 设计 |
|---|---|---|
| `read_dir` 的 `ignored` | 客户端自己解析 `.gitignore` 链，一次展开要往返读好几个 `.gitignore` | **服务端算好再返回**。gitignore 解析代码搬进 `tty7-core`，本地与远程共用同一份，一个目录一次往返 |
| `repo_root` | 现在是逐级 `p.join(".git").exists()` 上溯，远程等于逐级往返 | 提成一个方法，服务端一次走完 |
| `watch` | 递归 watch 一个大 repo，事件洪水跨网络 | 只 watch **已展开的目录集合**，非递归；服务端按 100ms 窗口合并后批量推送 |

### `git` 的约定

`GIT_OPTIONAL_LOCKS=0` 的只读约定（现在在 `git_status::git` helper 里）下沉到 `Host::git` 的两个实现里，两边一致。远程一次 git 探针 = 一次往返，跨洲可能 200ms+；这是可接受的，因为探针本来就是后台触发（cd / 命令结束 / agent 回合结束），UI 期间显示上一份快照。

### Host 从哪来

一个 workspace 一个 `Arc<dyn Host>`，pane 和面板从所属 workspace 拿。本地 workspace 拿 `LocalHost`。

### 要改的调用点

| 文件 | 内容 |
|---|---|
| `ui/file_tree.rs` | `read_dir`、gitignore 判定、`notify` watcher、新建 / 重命名 / 删除、`:628` 的 repo root 上溯 |
| `ui/code_editor.rs` | `:343` `:382` `:642` 的 stat、`:531` 的 write、`:691` 的 read；mtime 冲突检测照旧，走 `Host::stat` |
| `terminal/git_status.rs` | 分支 + `+N −M` 的 shell-out；`GitStatusCache` 的 key 从 `PathBuf` 变成 `(HostId, PathBuf)` |
| `terminal/git_diff.rs` | `git diff HEAD`。`ui/diff_overlay.rs` 只消费结果，本身不用改 |
| `core/worktree.rs` | `git worktree add` / `list`、`is_inside_repo`、`.tty7/.gitignore` 的写入。路径构造是纯字符串，留在客户端 |
| `ui/app.rs:3895` | 送给 agent 的 diff。这里现有一道显式挡板（`local_cwd()`，注释："远程 pane 的 cwd 不能用本地 git"）——改造后这道挡板拆掉，远程 pane 的 diff 真的能取到 |

## 10. 会话与 workspace 模型

### 存储分工

| 存在哪 | 内容 | 为什么在这边 |
|---|---|---|
| **远程** `~/.local/share/tty7/workspaces.json` | workspace 列表与名字、tab / pane 树、每个 pane 的 cwd / pane_id / agent 信息、`last_active` | 换台电脑连过来要看到同一份。这是机器的事实 |
| **客户端** `session.json` | 「我连过哪些 host 的哪些 workspace」、窗口几何、`open` 标记 | 这是**这台客户端**的视图状态。公司电脑上关掉窗口，不该让家里电脑看不见 |

`Workspace` 加一个字段：

```rust
pub struct Workspace {
    // ...现有字段不动
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<RemoteRef>,   // None = 本地，语义与今天完全一致
}
```

远程条目的 `session` 字段在客户端留空——布局的权威在远程，连上之后拉。旧的 `session.json` 没有 `host` 字段，反序列化后全是 `None`，即全部是本地 workspace，与今天行为逐字相同。

`RemoteRef` 指向一个已存的 SSH 配置（profile id 或 `~/.ssh/config` alias 或 `user@host:port`）加上远程侧的 `WorkspaceId`。

**`HostId`** 是客户端进程内对一个 `Arc<dyn Host>` 的稳定标识：本地是一个固定值，远程由 `RemoteRef` 里的连接部分派生（同一台机器的多个 workspace 共享同一个 `HostId`，与 §7.1 的连接复用同粒度）。它只在进程内有效，不持久化。

pane 标识在客户端侧是 `(HostId, pane_id)`；`pane_id` 只在单台远程 server 内唯一。

### 首页入口

「连接主机」→ 选一个已存的 SSH 配置 → 连上（首次触发安装，§12）→ 列出这台机器上已有的 workspace + 「新建」。新建的 workspace 落在 `~`，名字按现有 `Workspace::display_name` 的规则从 tab 的 repo / cwd 推导。

### 连接状态机

```
Disconnected ──connect──> Connecting ──✓──> Attached
                             │ ✗
                             ▼
                          Failed（状态条给 [重试]）

Attached ──网络断──> Reconnecting ──✓──> Attached
                     只读 + 状态条，指数退避 1/2/4/…/30s 封顶，无限重试

Attached ──别处 attach──> Preempted
                     只读 + [抢回]，不自动重连
```

**永不自动关窗**，任何失败态都停在窗口里等用户处置。

**只读降级的具体表现**：窗口照常显示，能滚历史、能选能复制、能 ⌘F 搜索；键盘输入不生效，底部一条"未连接 — 输入暂不生效"，顶部一条状态条写当前状态。输入**不缓存**（见 D6）。

**重连流程**：control 连接重建 → 拉 workspace 布局 → 对每个 pane 重开 channel + `Attach` + replay 补屏 → 以新客户端的尺寸 `Resize`。

**补屏的诚实边界**：断得太久、输出太多，`ReplayRing`（默认几 MB）会滚掉最早的部分，那时以 daemon 当前的 grid 快照为准，中间那段是真的丢了。这与今天本地 daemon 的行为一致，不额外承诺。

### 接管

远程 server 为每个 workspace 记录当前 attach 的客户端会话（一个随机 token + 客户端主机名）。新的 attach 到来时，向旧会话推送 `Preempted { by: <主机名> }` 然后关闭它的流。旧客户端转入 `Preempted` 状态，状态条写"已在 <主机名> 上打开"，给一个 [抢回] 按钮——点了就是反向再接管一次。

### 启动时

`open: true` 的远程 workspace 在启动时立即重开并连接。需要认证的窗口**一次只弹一个 sheet**，其余排队；不需要认证的（密钥、ssh-agent）并行连。

## 11. crate 拆分

`src/daemon/` 依赖的 core 模块只有 7 个：`agent_hooks` `cli_agent` `config` `osc` `proc` `shells` `threads`。其中真正沾 gpui 的**只有 `config` 一个文件的一行** `use gpui::{FontFeatures, Global}`（`cli_agent` 的两处 gpui 只是注释）。

| 搬进 `tty7-core` | 处理方式 |
|---|---|
| `daemon/*`（protocol、pane、server、ssh、transport、shell_integration…） | 原样搬，零改动 |
| `core/{osc, proc, shells, threads, agent_hooks, cli_agent}` | 原样搬 |
| `core/config` | `font_features` 在 core 里存 `HashMap<String, bool>`，GUI 侧转 `gpui::FontFeatures`；`impl Global` 留在 GUI |
| `core/session` 的数据部分（`SessionPane` / `SessionTab` / `Workspace` / `Workspaces`） | 纯 serde，搬。`WorkspaceStore`（gpui `Global` + `claim` / `focus` / `rename`）留在 GUI |
| `core/worktree`、gitignore 解析、`git_status` 的 shell-out helper | 搬——服务端要用同一份 |
| `core/crash` | 搬（远程 server 崩了也要写 crash.log） |
| 留在 GUI crate | `ui/*`、`terminal/*`、`core/{actions, window_state, update}` |

产物：

```
tty7-core     无 gpui，protocol / daemon / pty / ssh / Host 的两个实现 / 服务端 RPC
tty7          GUI bin，依赖 gpui + tty7-core
tty7-server   headless bin，只依赖 tty7-core
```

CI 增加 `x86_64-unknown-linux-musl` 和 `aarch64-unknown-linux-musl` 两个 target，产出 `tty7-server` 的静态二进制（D10）。

## 12. 安装、启动、版本

```
1. uname -sm            → Linux x86_64
2. SFTP stat ~/.local/share/tty7/bin/tty7-server-<客户端版本>
3. 不在 → 客户端 GET GitHub Release asset + sha256 校验
4. SFTP put → bin/.tty7-server-<ver>.tmp
5. chmod 0755 → rename（原子）
6. direct-streamlocal 试连远程 daemon socket
   连不上 → exec 一次 tty7-server --daemon（setsid 脱离）→ 重试
```

第 6 步就是远程版的 `spawn::ensure_running`。

**首次连一台新机器时，安装那一步给一次明确确认**（写哪个路径、多大、从哪来），之后同一台机器的升级静默。往别人机器上写二进制值得问一次。

**全程不用 sudo**，只碰 `$HOME`。

**版本不匹配**照搬 `spawn::ensure_running`：握手比 `PROTOCOL_VERSION`，兼容就继续用旧 server；不兼容就问用户"保留旧会话（继续用旧方言）还是重启服务（丢掉正在跑的 pane）"。二进制路径带版本号所以能并存，但 socket 只有一个——并存的是文件，不是运行中的服务。

**WSL 的安装**不走下载：直接把客户端自带的 Linux 二进制拷到 `\\wsl$\<distro>\home\<user>\.local\share\tty7\bin\`。这要求 Windows 客户端的安装包里带一份 `tty7-server` 的 Linux musl 二进制。

## 13. 配置归属

**客户端的 `config.json` 是唯一权威，远程机器上不需要 config.json。**

服务端需要知道的字段（`shell`、`shell_args`、`agent_commands`、`restore_agent_sessions`）随 `Spawn` / 控制消息下发。现有 `ShellSpec` 已经是这个做法，照着扩。

远程 workspace 窗口里的 Settings 页显示、修改的都是客户端配置，与本地窗口无差别。

## 14. agent 集成

链路在远程与本地同构，只换了位置：

```
远程 agent 进程
  └─ hook 调 tty7-server agent-hook <agent> <event>
       └─ 写 OSC 777 到控制终端
            └─ 远程 server 的 sniffer 收进 pane 状态
                 └─ control 连接推送到客户端
                      └─ tab 状态点 / 通知 / tray 图标
```

要改的两处：

- hook emitter 的命令从 `tty7 agent-hook` 变成远程的 `tty7-server agent-hook`（同一份代码，换 bin）。
- `TTY7` env marker 由远程 server 在 spawn 时注入（现有逻辑原样搬）。

Settings → Agents 的"安装 hooks"动作，在远程 workspace 下作用于**远程机器**（走 `Host::write_file`）。

## 15. 端口转发与文件传输

### 端口转发

远程 workspace 下，转发的归属从 **pane 变成 workspace**（现有 `SshForwardRegistry` 按 `pane_id` 键，要加一个 workspace 维度）。转发跑在该 workspace 所属的 `SshConnection` 上，现有 `daemon/ssh/forward.rs` 直接可用。

⌘/Ctrl-click 远程 pane 里的 `localhost:PORT`：在该 workspace 的连接上按需建一条 local forward，再用本地浏览器打开。这是**按需**，不是自动扫描——"自动猜端口"不做。

**WSL 例外**：WSL 与 Windows 共享 localhost，不需要任何转发，⌘-click 直接开浏览器。

### 文件传输

| 场景 | 走哪 |
|---|---|
| file tree 浏览、打开、保存、新建 / 重命名 / 删除 | `Host`（统一，走 control 连接） |
| 大文件上传 / 下载、拖到 Finder | 现有 SFTP 面板，同一条 SSH 连接 |
| WSL 的大文件传输 | 没有 SFTP；走 `Host::read_file` / `write_file`，或直接用 `\\wsl$` 路径 |

## 16. 安全

| 面 | 措施 |
|---|---|
| 二进制来源 | GitHub Release + sha256 校验（release 里带 checksums 文件），校验失败即中止，不装 |
| 权限范围 | 不用 sudo，只写 `$HOME`；目录 0700，socket 0600 |
| 通道信任边界 | `direct-streamlocal` 只有已认证的 SSH 会话能开，等价于 SSH 本身的信任边界。tty7 自身的通信**不开任何监听端口**（用户显式要求的端口转发是另一回事，见 §15） |
| 主机认证 | 沿用现有 known_hosts（新主机 / 变更主机的 GUI 确认 sheet） |
| 首次写入的知情 | 首次安装给一次明确确认（§12） |

## 17. 错误处理与降级

| 情况 | 行为 |
|---|---|
| 远程没装 git | `Host::git` 返回错误；分支 / diff / worktree 优雅缺省（跟本地非 repo 目录同路径），file tree 照常工作，`ignored` 全为 false |
| `AllowStreamLocalForwarding no` | 自动回退 stdio bridge（§7.1），用户无感 |
| 远程磁盘满 / 无写权限 | 安装报明确错误（路径 + 原因），不重试，不降级到别的路径 |
| 远程 server 崩了 | 客户端的 pane 流全断 → 走 `Reconnecting`；重连时 `ensure_running` 把它拉起来。**布局不丢**（远程的 `workspaces.json` 是持久化的），但 pane 进程没了，按现有"pane 不存在"的路径处理：依 `workspaces.json` 里的 cwd / agent 信息重新 spawn，agent 走现有的 `--resume` 恢复 |
| control 连接断但 pane 流还活着 | 不允许——control 连接是 workspace 的生命线，它断了就整个 workspace 转 `Reconnecting` |
| 单个 `Host` RPC 超时 | 该请求返回 `TimedOut`，调用点显示上一份缓存 / 加载态，不影响其它请求（`req_id` 乱序匹配） |
| sha256 不匹配 | 中止安装并明确报出来，不静默重试、不降级到无校验安装 |

## 18. 验证策略

最重要的一条：**stdio 传输让远程 workspace 能在 CI 里端到端测，不需要 sshd、不需要网络**——同机起一个 `tty7-server --stdio` 子进程，跑完整的"远程" workspace 流程。

| 层 | 怎么测 |
|---|---|
| `Host` trait | 一套 conformance 测试，`LocalHost` 和 `RemoteHost` 都跑，逐条比对结果 |
| 协议 | round-trip（照搬 `protocol.rs` 现有模式）+ 版本 skew 的握手分支 |
| 传输 | streamlocal 与 stdio 回退各一个集成测试 |
| 状态机 | 重连退避、接管、启动排队认证——纯单元测试，不碰网络 |
| 安装 | `uname` 解析、版本路径构造、原子替换、sha256 失败路径 |
| 端到端 | stdio 传输跑通"开 workspace → 开 pane → 断开 → 重连补屏 → 接管" |
| 回归护栏 | M1 / M2 是纯重构，现有全部测试必须逐条绿，不允许改测试来适配 |

## 19. 里程碑

前两步是**纯重构、零行为变化、CI 必须全绿**——这让这份大 spec 有一段安全的前半程。

| | 内容 | 完成标志 |
|---|---|---|
| M1 | crate 拆分（§11） | 本地功能一个不少，`tty7-server` 能在无头 Linux 上跑起来 |
| M2 | `Host` trait + `LocalHost`，改造全部调用点（§9） | 行为逐字不变 |
| M3 | control 连接 + Host 服务端 RPC（§8） | stdio 传输在本机端到端跑通 |
| M4 | SSH 传输 + 安装 + 版本协商（§7.1、§12） | 能连一台真的远程机器 |
| M5 | workspace 模型 + 首页入口 + 窗口绑定（§10） | 一台机器多窗口、多机器并存、和本地混开 |
| M6 | 状态机：重连 / 接管 / 启动即连（§10） | 拔网线再插回来 |
| M7 | 端口转发 + SFTP 在远程 workspace 下接线（§15） | 远程起的 dev server，⌘-click `localhost:3000` 能在本地浏览器打开；拖文件到 Finder 能下来 |
| M8 | WSL（§7.3、§12） | Windows 上「连接主机」能选到本机 WSL 发行版，全套功能与 SSH 主机一致 |
