<p align="right">

[English](README.md) | [日本語](README.ja.md) | **简体中文** | [Français](README.fr.md)

</p>

<p align="center">
  <img src="assets/app-icon.svg" alt="Loop X logo" width="256" height="256">
</p>

# Loop X Engineering

![CI](https://github.com/encoreshao/loop-engineering/actions/workflows/rust.yml/badge.svg)
![License](https://img.shields.io/github/license/encoreshao/loop-engineering)
![Platform](https://img.shields.io/badge/platform-macOS-lightgrey)
![Shell](https://img.shields.io/badge/shell-bash-4EAA25)

Loop X Engineering 的使命是把被 issue 分诊占用的时间还给你：它是一位常驻、
无需值守的队友，每个工作日处理你的 GitLab 队列，确保分配给你的事项不会无人问津——
提交修复、回答问题，或标记出真正需要你判断的事项——让你的注意力只花在真正重要的地方。
本地 Web 仪表盘让你可以观察它的工作、回顾它做过的一切，并通过界面手动完成所有配置，
无需编辑 JSON。

它的设计目标是可以放心地无人值守运行：它从不合并自己的 merge request，
从不给自己分配新 issue，并且只会触碰你明确告诉过它的项目。

## 目录

- [工作原理](#工作原理)
- [循环](#循环)
- [环境要求](#环境要求)
- [快速开始](#快速开始)
- [目录结构](#目录结构)
- [配置](#配置)
- [运行](#运行)
- [仪表盘](#仪表盘)
- [连接器](#连接器)
- [脚本参考](#脚本参考)
- [安全边界](#安全边界)
- [测试](#测试)
- [项目文档](#项目文档)
- [许可证](#许可证)



## 工作原理

每次定时运行（`loopx run gitlab-loop`）：

1. 列出配置中每个项目别名下、分配给你所配置用户名的所有未关闭 GitLab issue。
2. **逐个处理，绝不并行**，遵循 [`instructions/loopx/gitlab-issue.md`](https://github.com/encoreshao/loop-engineering/blob/main/instructions/loopx/gitlab-issue.md) 中的分步决策流程。
3. 对每个 issue 只执行以下其中一项：
  - **修复**——在隔离的 git worktree 中、在 `loop/issue-<iid>` 分支上进行，只有在项目自身的 lint/test 命令通过后才会创建 merge request。
  - **回答**——当请求无需改代码时（提问、状态查询），发布一条 GitLab 评论。
  - **升级**——当请求含义不明确或验证失败时，发布一条 GitLab 评论请求澄清。
4. 每个 issue 发送一条 Slack 消息，并在运行结束时发送一份汇总（每次运行都会发送，即使当天早上没有任何分配给你的 issue）。
5. 更新 [`PROGRESS.md`](https://github.com/encoreshao/loop-engineering/blob/main/PROGRESS.md) 和 `outputs/daily-review.md`，让下一次运行——以及你——了解发生了什么。

可复用的跨运行经验（修复模式、坑点）会通过 `loopx memory-store` 按 issue 记录为 markdown 任务记忆文件（在此格式出现之前记录的条目仍通过 `loopx memory` 读取），因此后续运行会比上一次更聪明。

第二个独立的循环（`loopx run topic-loop`）不监控 GitLab，而是监控更广泛网络上的任意主题——参见 [`docs/tasks/topic-monitor-loop.md`](https://github.com/encoreshao/loop-engineering/blob/main/docs/tasks/topic-monitor-loop.md)。

第三个循环（`loopx run inbox-triage-loop`）对 Gmail 和 Outlook 收件箱进行分诊：它将每封新的未读邮件归类到一个 `Loop/*` 标签，为紧急邮件起草（绝不发送）一封串联回复，并通过 Slack 汇总和仪表盘的 **Loops → Inbox Triage** 页面进行报告——参见 [`docs/tasks/inbox-triage-loop.md`](https://github.com/encoreshao/loop-engineering/blob/main/docs/tasks/inbox-triage-loop.md)。

## 循环

`config/loops.json.template` 内置 7 个循环；每个在仪表盘的 **Loops** 下都有独立页面，并在 `docs/tasks/` 下有各自的规格说明。调度可在该页面修改，下表为模板中的默认值。

| 循环 | 作用 | 所需条件 | 默认调度 | 对 Loop X 之外的写入 |
| --- | --- | --- | --- | --- |
| GitLab issues | 处理分配给你的 issue：修复、答复或上报 | GitLab 配置（`~/.gitlab/config.json`） | 工作日 10:00 | 分支与合并请求（绝不合并）、issue 评论、Slack |
| Topic monitor | 在网上调研你的主题并发送每日简报 | 主题（`topics.json`） | 每天 10:00 | Slack 摘要 |
| Inbox triage | 为新邮件打标签，并为紧急邮件起草回复（默认禁用） | 邮箱（`mail` 能力） | 工作日 09:00 | 邮件标签与草稿（绝不发送） |
| Daily Digest | 一份晨间简报：待办、分配给你的 issue、等你评审的 MR、今天的会议、Loop X 昨天做了什么（默认禁用） | `issues` 连接器（日历可选） | 工作日 09:30 | 仅通知 |
| MR Review | 预先评审你担任评审人的合并请求（默认禁用） | `merge_requests` 连接器（GitLab） | 每 2 小时 | 仅 GitLab 草稿备注；从不发布、批准或发表普通备注 |
| Pipeline Doctor | 诊断所跟踪项目和你名下 MR 中失败的 CI 流水线，并标出反复出现的失败（默认禁用） | `pipelines` 连接器（GitLab） | 每小时 | 仅通知 |
| RSS Watch | 按你的兴趣为订阅源新条目排序并发送简短摘要（默认禁用） | `feed` 连接器（RSS） | 每天 08:00 | 仅通知 |
| Meeting Prep | 每次会议前发送准备摘要：议程、关联的 GitLab 工作、与参会者的近期邮件、上次的跟进事项（默认禁用） | `calendar` 连接器（Google 日历），可选 GitLab 和邮箱 | 每 15 分钟 | 仅通知 |
| Release Notes | 跟踪的项目有新标签时，根据上一个标签以来合并的 MR 编写发布说明（默认禁用） | `merge_requests` 连接器（GitLab） | 每小时 | 本地 Markdown 文件 + 通知 |
| Stale Work Sweeper | 每周一列出停滞的 GitLab 议题和 MR 以及等待你审查的 MR；不调用模型（默认禁用） | `issues` 连接器（GitLab） | 每周一 09:00 | 仅通知 |

后四个是 LoopKit 插件（`crates/core/src/loopkit.rs`、`crates/core/src/loop_plugins/`）：模型在密封环境中运行（无工具、无 MCP 服务器），每个条目相互隔离，单个失败不会中断整次运行，并通过该循环的 **Notify via** 连接器发送通知。详见 [`docs/architecture.md`](https://github.com/encoreshao/loop-engineering/blob/main/docs/architecture.md#loopkit)。

## 环境要求

- macOS（定时任务和仪表盘都以 `launchd` 代理方式运行）
- `git` 2.42+（worktree、push-options）
- 一个 GitLab 账号，以及对需要跟踪的项目有权限的个人访问令牌
- （可选）一个 Slack incoming webhook，用于运行通知
- `[encore-skills](https://github.com/encoreshao/encore-skills)` 中的 `[gitlab-config](https://github.com/encoreshao/encore-skills/tree/main/skills/gitlab-config)` skill——本循环唯一的外部依赖，由 `setup.sh` 部署到 `~/.encore-skills`。随时可在仪表盘的 **Settings → Skills** 页面确认它是否确实已安装。
- （仅从源码构建时需要）`rust-toolchain.toml` 中固定的 Rust 工具链



## 快速开始

**新：安装 `loopx` 二进制（无需克隆）。** 从最新的 GitHub Release 下载适合你 Mac 的预编译 `loopx`，校验 SHA-256，安装到 `~/.loopx/bin/loopx`，并用模板生成 `projects.json`/`topics.json`/`loops.json`：

```bash
curl -fsSL https://raw.githubusercontent.com/encoreshao/loop-engineering/main/bin/scripts/install-loopx.sh | bash
curl -fsSL https://raw.githubusercontent.com/encoreshao/loop-engineering/main/bin/scripts/install-loopx.sh | bash -s -- --upgrade   # later
```

之后加 `--upgrade` 升级：保留你的配置，只替换程序，并重启已加载的 launchd 代理。全新安装时加 `--load-agents` 可同时启动仪表盘代理。也可用 Homebrew：`brew install encoreshao/tap/loopx`（再 `brew services start loopx`），或从 Release 的 DMG 安装已签名的 `LoopX.app`。首次启动检查（AI CLI、凭据、配置）：`loopx doctor`（应用用 `--json`）。卸载（两种模式）：`bin/scripts/uninstall-loopx.sh --project-dir ~/.loopx`（`--keep-config`、`--remove-app`）。发布流程见 [`docs/RELEASING.md`](docs/RELEASING.md)。

**已经在使用 Claude Code？** 粘贴以下内容，而不必自己运行命令：

> 请帮我设置 [https://github.com/encoreshao/loop-engineering](https://github.com/encoreshao/loop-engineering)：运行它的在线安装程序
> （`curl -fsSL https://raw.githubusercontent.com/encoreshao/loop-engineering/main/bin/scripts/install-loopx.sh | bash`），
> 然后帮我在 `~/.loopx/projects.json` 中填入我自己的 GitLab 项目，并在 `~/.gitlab/config.json` 中填入我的 GitLab 令牌。

`gitlab-config` skill（见“环境要求”）可在仪表盘的 **Settings → Skills** 页面安装，该页面会运行 `bin/scripts/setup.sh`；你也可以手动运行这个脚本。

### 从源码构建

```bash
cargo build --release -p loopx                        # -> target/release/loopx（工具链固定在 rust-toolchain.toml）
bin/scripts/build_loopx_app.sh target/release/loopx   # -> dist/LoopX.app，包裹该二进制的 Swift 应用（--install 会将其复制到 /Applications）
```

`LoopX.app`（源码在 [`macos/LoopX`](https://github.com/encoreshao/loop-engineering/tree/main/macos/LoopX)）内嵌 `loopx` 二进制，并将其注册为后台代理（`com.hermes.loopx.backend`），使后端在应用退出后继续运行；如果已有 `loopx serve` 在运行（见 `loopx.lock`），应用会改为连接它。构建详情：[`docs/building.md`](https://github.com/encoreshao/loop-engineering/blob/main/docs/building.md)。

**更新。** 应用通过 Sparkle 自行更新（仅限提供了 Sparkle feed URL 和公钥的构建）；二进制通过 `brew upgrade loopx` 或 `install-loopx.sh --upgrade` 更新。当此检出目录落后于 `origin` 时，仪表盘仍会显示“Update available”提示，但不再自行升级——请手动拉取并重新构建。

### 卸载

```bash
bin/scripts/uninstall-loopx.sh --keep-config   # 去掉 --keep-config 则全部删除
```

删除此安装的 `launchd` 代理，停止正在运行的后端（`loopx.lock`），并删除 `loop-engineering.*` 钥匙串条目和安装目录（默认 `~/.loopx`；传入 `--project-dir` 可指定其他位置）。`--keep-config` 保留 `projects.json`、`topics.json`、`loops.json` 和 `ai_cli.json`；`--remove-app` 同时删除 `LoopX.app`；`--keep-keychain` 不动钥匙串。可安全地重复运行；通过 Homebrew 安装的，最后再运行 `brew uninstall loopx`。


## 目录结构

使用默认安装路径时，所有内容都位于同一个文件夹下：

```
~/.loopx/            # 安装目录（loopx 二进制、配置、运行状态）
├── bin/loopx                   # loopx 二进制（install-loopx.sh）
├── projects.json                # your config: GitLab projects to track  ┐
├── topics.json                  # your config: topics to monitor         │
├── loops.json                   # your config: scheduled-loop registry   ├─ 归你所有
├── instructions.md              # your free-text instructions            │
├── connectors.json              # your config: connector accounts        │
├── ai_cli.json                  # your config: Claude Code vs Codex CLI   ┘
├── loop_scheduler_state.json    # managed automatically, not hand-edited
├── PROGRESS.md                  # live run state, updated every run
├── outputs/                     # ← generated docs & run history live here (gitignored)
│   ├── daily-review.md          #   latest GitLab-issue-loop report
│   ├── connectors/test-results.json  #   last Test result per connector account
│   ├── messages.json             #   Dashboard → Activity message thread
│   ├── status.json               #   GitLab loop's current/last run status
│   ├── status/<loop_name>.json   #   every other registered loop's current/last run status
│   └── history/<date>.{md,log}   #   every past run's report + log
└── worktrees/                    # ← per-issue git worktrees for tracked projects (gitignored)
    └── <project>-issue-<iid>/    #   that project's own checkout, on branch loop/issue-<iid>
```

无论本仓库检出在哪里，`projects.json`、`topics.json`、`loops.json`、`instructions.md` 和 `ai_cli.json` 始终解析到 `~/.loopx/…`。`projects.json` 生成时的 `worktree_root` 默认也是 `~/.loopx/worktrees`。

另有两个配置文件完全位于此目录树之外，可在仪表盘的 **Loops → GitLab Issues → Projects** 和 **Settings → Notifications** 页面编辑，而无需手动修改：`~/.gitlab/config.json` 和 `~/.slack/config.json`。

## 配置


| 文件                                  | 内容                                                                                                                                                                                                                   | 管理方式                                                                                                                                                                   |
| ------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `~/.loopx/projects.json`   | 要跟踪哪些项目、它们的本地检出路径、目标分支、install/lint/test 命令、你的 GitLab 用户名，以及 worktree 临时目录（`worktree_root`，默认为 `~/.loopx/worktrees`） | 仪表盘 **Loops → GitLab Issues → Projects** 页面的 “Tracked Projects” 区域，或手动复制 [`config/projects.json.template`](https://github.com/encoreshao/loop-engineering/blob/main/config/projects.json.template)，或交给 `install-loopx.sh` 完成 |
| ↳ 每个项目的 `instance`（可选）   | 为单个项目覆盖顶层的 `gitlab_instance`——当你的项目分布在多个 GitLab 实例上时设置。省略时回退到 `gitlab_instance`。                                               | 同一文件中的各项目条目——参见模板中的 `harbor` 示例                                                                                                            |
| `~/.loopx/topics.json`     | 要监控哪些主题，以及每个主题中什么算值得关注（仅主题监控循环）                                                                                                                               | 手动复制 [`config/topics.json.template`](https://github.com/encoreshao/loop-engineering/blob/main/config/topics.json.template)，或交给 `install-loopx.sh` 完成                                                                |
| `~/.loopx/inboxes.json`    | 要分诊哪些邮箱（服务商、账号、分类、VIP/排除的发件人、Slack bundle）以及共享的默认分类集（仅 Inbox Triage 循环）                                                              | 仪表盘的 **Loops → Inbox Triage → Setup** 页面（`/inbox/setup`），或由 `install-loopx.sh` 从 [`config/inboxes.json.template`](https://github.com/encoreshao/loop-engineering/blob/main/config/inboxes.json.template) 生成骨架               |
| `~/.loopx/mail_oauth.json` | Gmail/Outlook OAuth 应用自身的 client ID（Google 还包括 client secret）——这是一次性的应用注册步骤，而不是按邮箱的凭据                                                                           | 仪表盘的 **Loops → Inbox Triage → Setup** 页面                                                                                                                                                     |
| `~/.loopx/loops.json`      | 定时循环的注册表：每个条目的名称、计划（工作日/小时/分钟）、入口模块、超时以及各循环的参数——由 `loopx loops` 读取，由 `loopx scheduler` 轮询                 | 手动复制 [`config/loops.json.template`](https://github.com/encoreshao/loop-engineering/blob/main/config/loops.json.template)，或交给 `install-loopx.sh` 完成                                                                  |
| `~/.loopx/loop_scheduler_state.json` | 每个循环上次尝试运行的日期，确保调度器不会在同一天重复运行同一循环——不需要手动编辑                                                                                            | 由 `loopx scheduler` 自动写入；`install-loopx.sh` 会为每个已注册循环预填当天日期，因此启用调度器时不会立即触发一次运行 |
| `~/.loopx/instructions.md` | 你自己的自由文本指令，循环在每次运行开始时读取                                                                                                                                             | 仪表盘 **Settings** 页面的 Instructions 标签页                                                                                                                               |
| `~/.loopx/ai_cli.json`     | `loopx run` 为每个已注册循环调用哪个 AI CLI（Claude Code 或 Codex CLI）；默认为 `claude`                                                                                                        | 仪表盘 **Settings** 页面的 AI CLI 标签页，或交给 `install-loopx.sh` 完成                                                                                                                |
| `~/.gitlab/config.json`               | GitLab 实例 URL、令牌，以及项目别名 → 项目 ID 的映射（由 `gitlab-config` skill 读取）                                                                                                               | 仪表盘 **Loops → GitLab Issues → Projects** 页面                                                                                                                                                     |
| `~/.slack/config.json`                | 你的 Slack incoming webhook URL（以及各 bundle 的覆盖设置）                                                                                                                                                          | 仪表盘 **Settings** 页面的 Notifications 标签页（默认 webhook）/ **Loops → GitLab Issues → Projects** 页面的 Access bundles 区域（各 bundle 覆盖设置）                                                              |


`loopx config` 是唯一读取 `projects.json` 的代码——可以在终端用它来检查你的配置是否正确：

```bash
loopx config aliases                # 每个已配置的项目别名
loopx config project <alias>        # 该别名的完整配置，含解析后的 GitLab 实例
loopx config assignee               # 正在跟踪的 GitLab 用户名
loopx config worktree-root          # 每个 issue 的 worktree 创建位置
```

如果 `~/.loopx/projects.json` 尚不存在，每个需要它的命令都会立即失败并给出提示——不会悄悄猜测路径。可以在仪表盘的 **Loops → GitLab Issues → Projects** 页面保存项目，或复制 `config/projects.json.template`。

注意：该目录以前是 `~/.loop-engineering`。首次运行 `loopx`（或 `install-loopx.sh`）会把已有目录移动到 `~/.loopx`，并在旧名称处留下符号链接，因此写入配置和 launchd plist 的旧路径仍然有效。设置 `LOOP_ENGINEERING_HOME` 可让 loopx 使用其他目录。

**Access bundles（访问包）**——按项目覆盖令牌/webhook

大多数项目直接使用其 GitLab 实例的默认令牌。**access bundle** 是一个具名的覆盖设置——拥有自己的 `{instance, token}` 组合，外加一个可选的 Slack webhook——用于那些实例默认令牌权限不足的少数项目。

在仪表盘 **Loops → GitLab Issues → Projects** 页面的独立 “Access bundles” 区域中管理 bundle：

- **添加 bundle**：为其命名，选择它要认证的 GitLab 实例，粘贴其令牌，并可选填写 Slack webhook URL。
- **将 bundle 分配给项目**：编辑项目别名所在行，从 **Bundle** 下拉框中选择 bundle——默认为 “(use instance default)”。
- 只要仍有项目别名指向某个 bundle，该 bundle 就不能删除，其实例也不能更改。
- 删除 bundle 时也会一并清除其 Slack webhook 覆盖设置（如果有）。

Bundle 保存在 `~/.gitlab/config.json` 的 `bundles` 键中；如果设置了 webhook 覆盖，则还保存在 `~/.slack/config.json` 的 `bundle_webhooks` 键中——两者仅通过 bundle 名称关联。

## 运行

**手动**运行一次，在把它交给定时任务之前先看看它如何工作：

```bash
loopx run gitlab-loop   # the daily GitLab issue loop
loopx run topic-loop    # the topic monitor loop
```

两者都会把日志写入 `outputs/history/`，并且都会把每次 `claude` CLI 调用的输出追加到 `logs/loop-engineering.log`（可在仪表盘的 **Runs → Logs** 页面查看）；你也可以通过仪表盘的 **Run now** 按钮（Dashboard → Overview）触发 GitLab 循环，无需终端。

**定时运行**有三种方式：

- LoopX 应用内置的调度器：应用打开且 **Settings → Run scheduled loops while LoopX is open** 开启时，每 15 分钟运行一次 `loopx scheduler`；
- launchd 代理：`install-loopx.sh` 会把 [`launchd/loopx/`](https://github.com/encoreshao/loop-engineering/tree/main/launchd/loopx) 下的模板一次性渲染到 `<安装目录>/launchd/`，你可以在仪表盘的 **Settings → Daemons** 页面逐个点击加载（该页面还会显示每个代理当前是否已加载及其 PID），或手动执行 `launchctl load -w`；
- 或者在你自己的调度器中运行 `loopx scheduler`。

| 代理                                    | 运行内容                                                                                                                                         |
| ---------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------|
| `com.hermes.loop-engineering`           | `loopx scheduler`，每 15 分钟一次（`StartInterval`）——运行 `~/.loopx/loops.json` 中已注册且到期的循环 |
| `com.hermes.loop-engineering-dashboard` | Web 仪表盘（`loopx serve`），常驻运行（`RunAtLoad` + `KeepAlive`）                                                                                |
| `com.hermes.loopx.backend`              | 随 `LoopX.app` 打包的后端，由应用自行注册（不是本仓库中的模板）                                                                                  |

运行哪些循环、按什么计划运行是配置而非代码——编辑 `~/.loopx/loops.json`（参见 [`config/loops.json.template`](https://github.com/encoreshao/loop-engineering/blob/main/config/loops.json.template)）即可添加循环或更改其到期时间；添加循环需要在 `loops.json` 中新增条目，而不是新增 plist。**Settings → Daemons** 页面中按代理的计划编辑器只作用于 plist 自身的 `StartCalendarInterval`，而 `com.hermes.loop-engineering` 没有它（它以固定的 `StartInterval` 每 15 分钟轮询，实际哪个循环到期由 `loops.json` 决定）——循环自身的计划请在 **Loops** 页面中修改，或手动编辑 `loops.json`。


## 仪表盘

一个仅限 localhost 的 Web UI（服务端渲染页面，无 JS 框架），由 `loopx serve` 提供服务——单个 Rust 二进制，同时也是 Mac 应用使用的 API。`loopx serve --port N` 指定端口（默认 `8420`；`--port 0` 选取空闲端口）。`install-loopx.sh` 在首次渲染常驻 `launchd` 代理时，会在 `48420`-`48620` 中随机选取一个端口（可用 `--port` 覆盖，之后的 `--upgrade` 不会重新选取）——已有安装实际使用的端口请查看安装目录中的 `launchd/com.hermes.loop-engineering-dashboard.plist`。


| 侧边栏条目 | 显示内容 |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Dashboard** (`/`) | 视图：Overview — 当前/上次运行状态、实时进度指示器，以及 Run now 按钮; Activity — 与循环的消息会话，并带有独立的实时进度指示器。在此粘贴 GitLab issue 链接即可让循环立即处理该 issue，无论它分配给了谁。 |
| **Loops** (`/loops`) | 目录页，把已启用的循环与可用的循环分开列出；每个可见的循环也会作为子链接出现在侧边栏 **Loops** 之下（Inbox Triage 在被禁用且从未运行过时不会显示在那里） |
| — **GitLab Issues** (`/loops/gitlab-loop`) | 视图：Live — 实时获取的当前分配给你的 issue 和未关闭的 MR; Projects — 管理 `~/.gitlab/config.json`（实例、项目别名、access bundles）和 `~/.loopx/projects.json`（跟踪的项目、循环设置），无需手动编辑 JSON |
| — **Topic Monitor** (`/loops/topic-loop`) | 视图：Live — 每个已配置主题的状态和已保存的简报; Topics — 添加、编辑和删除监控主题——同一页面上的独立视图，避免配置项干扰 Live 状态视图 |
| — **Inbox Triage** (`/loops/inbox-triage-loop`) | 视图：Inbox Triage 自身的实时状态（**Live**），以及邮箱连接与分类（**Setup**：连接 Gmail/Outlook、分类、VIP/排除的发件人、Slack bundle） |
| **Runs** (`/runs`) | 视图：Loop Runs — `outputs/loop-runs/` 下记录的每次运行（每个处理过的 issue 或主题一条），按时间倒序——只读；概览栏显示总运行次数、成功/升级比例、平均成本，以及实验性的 Loop Efficiency Score; History — 每次历史运行的回顾报告，按时间倒序; Logs — `logs/loop-engineering.log` 的末尾部分——包括 GitLab 循环、主题监控循环以及本仪表盘自身聊天助手的每次 `claude` CLI 调用输出 |
| **Insights** (`/insights`) | 视图：Analytics — 在可选的天数窗口内循环的表现：Loop Health 评分、结果、质量、风险与分类、失败细分以及学习趋势; Cost — AI 使用成本——GitLab issue 循环自身在窗口期内的成本，以及 `outputs/loop-runs/` 下所有运行的总成本; Budget — 每次已记录运行的最新预算状态，以及按循环定义和按日/周/月的汇总; Memory — 按项目记录的跨运行经验，每个 GitLab issue 一个 markdown 文件，以及此格式出现之前记录的内容（显示在 “Legacy learnings” 下） |
| **Harness** (`/harness`) | 视图：Audit — 每个循环定义的评分及通过/未通过检查项 |
| **Connectors** (`/connectors`) | 视图：Accounts — 按类型分组的所有连接器账号，带能力标签、显示上次结果的 Test 按钮（通知目标为 **Send test message**），外部账号还有指向其所属页面的「Managed on …」徽标；Add — 选择类型并填写表单。密钥填入密码框，保存后不会再次显示（编辑时留空即保留已存的值） |
| **Settings** (`/settings`) | 视图：General — Notifications（管理 `~/.slack/config.json` 的默认 webhook）、AI CLI（选择 Claude Code 或 Codex CLI，并实时检查各自是否已安装）、Appearance（颜色模式、强调色主题、自动刷新间隔——保存在当前浏览器的 `localStorage` 中）以及 Instructions（你自己的自由文本指令，循环在每次运行开始时读取）——以标签页形式集中在同一页面 (**General** 视图拆分为 Notifications / AI CLI / Appearance / Instructions 标签页（`?tab=`）); Daemons — 每个 `launchd` 代理的加载状态、可编辑的计划及启用/禁用操作，外加 Registered Loops 分栏，列出统一调度器运行的每个循环（其自身计划和上次运行状态，读取自 `loops.json`）; Skills — 本循环依赖的每个外部 skill，以及它是否确实已安装 |
| **README** (`/readme`) | 已移至顶栏的帮助图标（`/readme`）：本文件，在应用内渲染，并带有跳转到章节的快速导航 |

所有旧 URL（`/activity`、`/gitlab`、`/topic-monitor`、`/inbox`、`/loop-runs`、`/history`、`/logs`、`/analytics`、`/cost`、`/budget`、`/memory`、`/audit`、`/settings/general`、`/daemons`、`/skills` 等）都会保留查询字符串并永久重定向（301）到新位置，因此书签依然有效。


## 连接器

连接器是循环可以接入的账号：GitLab 或 GitHub 实例、Slack、Telegram 或聊天 Webhook、Notion 工作区、RSS 订阅源列表、Jira 或 Linear 工作区、邮箱、Google 日历。在仪表盘的 **System → Connectors** 页面（`/connectors`）中管理。每种类型都声明了*能力*（`issues`、`merge_requests`、`pipelines`、`notify`、`feed`、`mail`、`docs`、`calendar`），循环可以要求某种能力，而不是某个具体产品。

| 类型 | 能力 | 需要填写 | 密钥 |
| --- | --- | --- | --- |
| GitLab | `issues`, `merge_requests`, `pipelines` | URL | 个人访问令牌 |
| GitHub | `issues`, `merge_requests`, `pipelines` | API URL（默认 `https://api.github.com`）、用户名 | 令牌 |
| Slack webhook | `notify` | — | Webhook URL |
| Chat webhook | `notify` | 选择预设：Feishu、DingTalk、WeCom 企业微信（群机器人；个人微信没有机器人 API）、Microsoft Teams、Discord、Google Chat 或 Generic webhook | Webhook URL |
| Telegram 机器人 | `notify` | 聊天 ID | 机器人令牌 |
| RSS / Atom feeds | `feed` | 订阅源 URL，每行一个 | — |
| Notion | `docs`（显示为 Documents） | — | 集成令牌 |
| Jira Cloud | `issues` | 站点 URL、邮箱 | API 令牌 |
| Linear | `issues` | — | API 密钥 |
| Mailbox | `mail` | 外部账号——在 Inbox Triage 设置中管理 | — |
| Google Calendar | `calendar`（显示为 Calendar） | 日历 ID（默认 `primary`） | Google 登录（只读，`calendar.readonly`）；刷新令牌保存在钥匙串中 |

**图库与表单。**

- **Add** 会打开连接器类型图库，按 Code hosting、Chat & notifications、Work tracking、Knowledge、Feeds、Mail 分组（Outlook 在 Mail 中），最前面是 Google 分组，依次为 Gmail、Google Calendar 和 Google Chat，并带搜索框用于筛选。卡片会跟随 **Settings → Appearance** 中选择的强调色以及各服务自身的品牌色，聊天 Webhook 的每个预设都有各自的说明。
- 每个卡片和每个账号行都带有对应服务的品牌标志（内联的 Simple Icons 图标；没有图标的服务——Feishu、DingTalk、通用 Webhook——使用字母标记）。
- 聊天 Webhook 卡片会展开为上述预设，每个预设带一行提示（例如 Teams 的 Workflows Webhook 可能需要 Adaptive Cards）以及指向该服务官方文档的 **Where do I get this?** 链接。
- 添加/编辑表单包含 **Account** 部分（**Label** 和 **Connector id**；id 会根据 label 自动建议，直到你手动修改）、**Connection** 部分（该类型的设置）和 **Credentials** 部分（密钥）。
- **Google Calendar** 没有需要粘贴的密钥：点击 **Connect with Google**（已连接时为 **Reconnect**）登录即可。它复用你已在 Inbox Triage 设置页（Gmail 标签页）为 Gmail 配置的 Google OAuth 客户端（缺少时表单会提供指向该页面的链接），因此 Google Cloud 中需要相同的重定向 URI：`http://127.0.0.1:<port>/oauth/google/callback`。账号行会显示 **Connected** / **Not connected** 标记，**Test** 会读取该日历。只请求只读的 `calendar.readonly` 权限范围。
- 必填项标有 `*`，其余标注「(optional)」，并带有示例占位文字。密钥框带有 **Show**/**Hide** 切换。
- 按钮有 **Save**、**Save and test**（先保存再探测）和 **Cancel**。保存失败时，表单会重新显示，并保留已填写的非机密值。

**数据存放位置。** 在页面上添加的账号是*原生*账号：非机密设置保存在 `~/.loopx/connectors.json`，密钥保存在 macOS 钥匙串中，服务名为 `loop-engineering.connectors`（设置了 `LOOP_ENGINEERING_HOME` 时会加上 `.sandbox-<hash>` 后缀，因此沙盒运行永远不会碰到真实密钥）。密钥不会写入 `connectors.json`，保存后也不会再次显示。

**外部账号**直接从原本管理它们的文件中读取，不做任何迁移：GitLab 实例来自 `~/.gitlab/config.json`（id 为实例别名），Slack Webhook 来自 `~/.slack/config.json`（`slack-default`，以及每个 bundle Webhook 对应的 `slack-<bundle>`），邮箱来自 `inboxes.json`（id 为收件箱名称）。它们会显示带链接的「Managed on …」徽标，指向可编辑它们的页面；你仍可在这里对其执行 Test。

**Test 按钮。** 每个账号都有 **Test** 按钮（Slack、Telegram 和聊天 Webhook 为 **Send test message**）；上次结果保存在 `outputs/connectors/test-results.json`。

**循环与通知。** 在 **Loops** 页面，需要某种能力的循环会显示「Needs: …」标签，在存在具备该能力的连接器之前无法启用（UI 和服务端都会拦截）。在 `loops.json` 条目中声明了 `"routes_notifications": true` 的循环（其运行器通过 `loopx notify-loop` 发送）还会有 **Notify via** 选项，以 `notify: [连接器 id]` 的形式保存。`loopx notify-loop` 会把这类循环的通知路由到这些连接器；未设置 `notify` 时，仍和以前一样发送到默认的 Slack Webhook。内置的 GitLab、Topic 和 Inbox 循环尚未声明该字段，仍直接发送到 Slack Webhook；如果它们已有 `notify` 列表，Loops 页面会以只读方式显示，并提供 **Clear** 按钮清除。可以通过命令行试用：`loopx notify-loop <loop> "<text>"`。仪表盘的 AI 面板也能列出连接器（聊天工具 `connector-list`）。

## 脚本参考

展开查看完整列表


| 命令 / 脚本 | 用途                                                                                                                                                                                                                              |
| ----------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `loopx run <loop>` | 立即运行一个已注册的循环（从 `~/.loopx/loops.json` 查找）——日志写入 `outputs/history/`，记录事件，失败时通知 Slack；`loopx run gitlab-loop <alias> <issue_iid>` 只处理单个 issue。由 `loopx scheduler`（按计划）或仪表盘（按需）调用 |
| `loopx scheduler` | launchd（或应用）每 15 分钟运行一次的轮询循环：读取 `~/.loopx/loops.json`，启动已注册且到期的循环 |
| `loopx loops`, `loopx ai-cli` | 循环注册表（`loops.json`：名称、计划、入口），以及每个循环调用哪个 AI CLI（`claude` 或 `codex`，`ai_cli.json`） |
| `loopx config` | 读取 `~/.loopx/projects.json`（别名、负责人、worktree 根目录、各项目配置） |
| `loopx issues list`, `loopx issues comments` | 列出已配置项目中分配给所配置用户的未关闭 GitLab issue；检测缓存 issue 上自循环上次查看以来有哪些新 note |
| `loopx memory`, `loopx memory-store` | 读取（旧版）保存在 GitLab 缓存中的按项目经验；以 markdown 文件形式读取/记录按 issue 的任务记忆（每个 issue 一个文件，外加每个项目一个 MEMORY.md 索引） |
| `loopx topic` | 主题注册表（`topics.json`）、每个主题滚动 7 天的去重窗口，以及主题监控运行（`loopx topic run`） |
| `loopx inbox run`, `loopx loopkit` | Inbox Triage 运行器；运行 LoopKit 插件 |
| `loopx notify`, `loopx notify-loop` | 向配置的 Slack incoming webhook 发送消息；将循环的通知路由到它的连接器 |
| `loopx worktree new`, `loopx mr open` | 在 `loop/issue-<iid>` 分支上创建（或复用）隔离的 git worktree；推送 issue 分支并开 MR——拒绝任何不以 `loop/issue-*` 命名的分支 |
| `loopx gitlab`, `loopx secrets`, `loopx auth` | GitLab REST 辅助工具与缓存；钥匙串中的连接器机密；Google/LinkedIn OAuth 登录 |
| `loopx serve`, `loopx chat-tool` | 仪表盘后端和 Mac 应用的 API；仪表盘聊天助手唯一可以运行的命令 |
| `loopx loop`, `loopx events`, `loopx ledger`, `loopx metrics`, `loopx cost`, `loopx risk`, `loopx learning` | 循环定义（init、validate、audit、eval 等）、事件台账以及基于它生成的报告 |
| `loopx doctor`, `loopx update`, `loopx status`, `loopx messages`, `loopx approvals` | 首次启动检查；更新检查；运行状态文件；仪表盘聊天收件箱；待处理的审批 |
| `bin/scripts/install-loopx.sh` | 无需克隆的 `loopx` 二进制安装程序：下载适用于这台 Mac 的发布构建，用 `SHA256SUMS` 校验，生成配置，渲染 `launchd/loopx/*.plist.template`；`--upgrade` 保留配置并只替换程序；`--load-agents`、`--skip-service`、`--release-base` |
| `bin/scripts/uninstall-loopx.sh` | 删除任一模式的 loopx 安装（LaunchAgents、`loopx.lock`、钥匙串条目、`--remove-app`、`--keep-config`） |
| `bin/scripts/build_loopx_app.sh` | 围绕指定的 `loopx` 二进制构建通用的 `LoopX.app`（Swift，`macos/LoopX`） |
| `install-loopx.sh` | 安装 `gitlab-config` skill（Settings → Skills 页面的安装按钮会运行它） |
| `bin/scripts/release_notes.sh` | 输出 `CHANGELOG.md` 中某个版本的章节，用作发布说明 |




## 安全边界

这些边界是固定的，不会随时间或多次成功而放宽（参见 [`docs/tasks/gitlab-issue-loop.md`](https://github.com/encoreshao/loop-engineering/blob/main/docs/tasks/gitlab-issue-loop.md)）：

- **从不合并 merge request。** 循环的工作止于“MR 已创建、验证通过”——合并始终是人工手动步骤。
- 每次代码变更都在独立的 git worktree 中、在 `loop/issue-<iid>` 分支上进行，绝不直接在目标分支上修改。
- 只有当项目自身配置的 `test_cmd`/`lint_cmd` 通过，且 diff 只涉及与该 issue 相关的文件时，才会创建 MR。
- 不允许任意 shell 命令、不升级依赖、不读取 `.env`/凭据/SSH 密钥——只允许 `instructions/loopx/gitlab-issue.md` 中的命令白名单。
- issue 逐个按顺序处理，绝不并行。
- 同一 issue 的验证失败在一次运行内绝不重试——而是通过 GitLab 评论升级处理。（启用 `verification.mode: gate` 后，循环会自行重新运行项目的检查，并允许带着失败输出作为反馈进行一次有限重试；测试/lint 失败的 MR 绝不会被创建，而是以 `loop:needs-human` 标签升级处理。）

Inbox Triage 循环有其自己的固定安全边界（参见 [`docs/tasks/inbox-triage-loop.md`](https://github.com/encoreshao/loop-engineering/blob/main/docs/tasks/inbox-triage-loop.md)）：

- **从不发送邮件。** 两个邮件服务商模块都不包含发送函数，且获取的 Outlook 令牌权限范围不含 `Mail.Send`——在令牌层面就无法发送，而不仅仅是代码层面。
- **从不归档、删除、移动邮件或更改已读状态。** 对邮箱的写操作仅限于创建 `Loop/*` 标签/分类、应用这些标签，以及在邮箱自身的草稿箱中创建回复草稿。
- **只应用 `Loop/*` 标签。** 每个分类标签（无论默认还是自定义）都必须以 `Loop/` 开头——否则 `inboxes.json` 在加载时会被拒绝，因此手动编辑的系统标签（如 `TRASH` 或 `UNREAD`）永远不会被应用到真实邮件上。
- **邮件正文从不持久化。** 它们只存在于内存中，以及每个收件箱的 `claude -p` 调用提示词中（外加最多一次重试），该调用不保留会话记录——绝不写入 `outputs/`、日志、状态或 Slack 汇总，这些地方只会包含发件人、主题、分类、AI 的简短理由和草稿链接。AI 撰写的回复草稿只保存在邮箱自身的草稿箱中。
- **Inbox Triage 需要 Claude CLI。** Codex 总是会给模型一个 shell，并将提示词记录到 `~/.codex/sessions/` 下，因此选择 Codex 时，每个收件箱都会在读取任何邮件之前直接失败——直到在 **Settings** 中将 AI CLI 切回 Claude。
- **刷新令牌只存储在 macOS 钥匙串中**，通过 `security -i` 并经 stdin 传入令牌写入，绝不以明文形式存储在磁盘上或出现在进程的 argv 中。



## 测试

```bash
cargo test --workspace
cargo build -p loopx && LOOPX_BIN=target/debug/loopx python3 -m pytest tests/
```

Rust 单元测试与代码放在一起。`pytest` 套件只把构建好的 `loopx` 二进制当作黑盒来驱动（CLI 契约、端到端运行、安装程序、打包），并尽可能针对真实子进程/临时目录而非 mock 进行测试；`tests/golden_ui/` 保存了每个仪表盘页面的四种语言渲染 HTML，`cargo test` 会逐字节比较。

`loop eval` 运行脚本化评测用例（免费），并写入 `outputs/evals/last.json`。`loop eval --golden [--budget-usd N] [--case NAME]` 在合成的夹具仓库上运行真实智能体（付费：预算默认为 10 美元，花完后不再启动新用例），并写入 `outputs/evals/golden-last.json`。两者的结果都显示在 Harness → Evals。`loop ledger backfill` 会根据旧的 `result.json` 重建账本记录。

## 项目文档


| 文档                                                                    | 用途                                                                                          |
| ---------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| [`docs/architecture.md`](https://github.com/encoreshao/loop-engineering/blob/main/docs/architecture.md)                         | V2 运行时架构：`LoopDefinition`/`LoopState`/`LoopRuntime`、验证/预算/策略、可观测性以及 CLI——这是全局地图，而非任一循环自身的规格说明 |
| [`TASK.md`](https://github.com/encoreshao/loop-engineering/blob/main/TASK.md)                                                   | 本仓库运行的所有定时任务的索引，每项都指向 `docs/tasks/` 下各自的规格说明        |
| [`docs/tasks/gitlab-issue-loop.md`](https://github.com/encoreshao/loop-engineering/blob/main/docs/tasks/gitlab-issue-loop.md)   | GitLab issue 循环面向人的规格说明：目标、范围、安全边界                              |
| [`docs/tasks/topic-monitor-loop.md`](https://github.com/encoreshao/loop-engineering/blob/main/docs/tasks/topic-monitor-loop.md) | 主题监控循环面向人的规格说明：目标、范围、安全边界                             |
| [`instructions/loopx/gitlab-issue.md`](https://github.com/encoreshao/loop-engineering/blob/main/instructions/loopx/gitlab-issue.md)                         | GitLab issue 循环每次运行时自身遵循的分步流程                               |
| [`instructions/loopx/topic-monitor.md`](https://github.com/encoreshao/loop-engineering/blob/main/instructions/loopx/topic-monitor.md)       | 主题监控循环每次运行时自身遵循的分步流程                              |
| [`PROGRESS.md`](https://github.com/encoreshao/loop-engineering/blob/main/PROGRESS.md)                                           | 循环每次运行都会读取和更新的实时状态——上次运行摘要、未解决的升级事项、已做出的决策 |
| [`docs/troubleshooting/crash-looping-launchd-agent.md`](https://github.com/encoreshao/loop-engineering/blob/main/docs/troubleshooting/crash-looping-launchd-agent.md) | 诊断并修复陷入崩溃循环、日志被刷屏的 `com.hermes.loop-engineering*` launchd 代理 |




## 许可证

[MIT](https://github.com/encoreshao/loop-engineering/blob/main/LICENSE)——详见 [`LICENSE`](https://github.com/encoreshao/loop-engineering/blob/main/LICENSE) 文件。
