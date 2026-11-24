# agent-cli-x

[English](README.md) · **简体中文**

在当前终端调用另一台机器编译或跑测试，查看日志，也可以读写已登记项目中的文件。调用端和远端使用同一个二进制，命令输出与文件读取都有默认上限，方便 AI 编程助手直接处理。

[下载 Windows / Linux 版](https://github.com/Fracizz/agent-cli-x/releases/latest) · [快速开始](#快速开始) · [Agent Skill](skills/agent-cli-x/SKILL.md) · [安全与权限](#安全与权限)

## 安装

在调用端和目标主机上安装。发布页提供 Windows x64 和 Linux x64 的预编译二进制。解压后把可执行文件加入 PATH，例如 Windows 的用户 `%USERPROFILE%\bin` 或 Linux 的 `~/.local/bin`。

源码安装需要 Rust 1.88 或更新版本。

```bash
cargo install --git https://github.com/Fracizz/agent-cli-x
```

在已克隆的仓库中，也可执行 `cargo build --locked --release`，再将 `target/release/` 里的程序加入 PATH。

## 快速开始

在项目所在主机上，用负责跑构建的用户执行。将工作区路径换成项目的真实路径，并确保调用端可在可信网络上访问监听端口。Windows 可使用 `--workspace proj=D:/code/proj`。

```bash
agent-cli-x init --name build-01 --workspace proj=/home/me/proj
agent-cli-x serve
agent-cli-x token add dev-workstation
```

不加 `--workspace` 时，主机的用户主目录会作为工作区 `home`（可读写）提供，授权后即可直接操作。要加目录，在主机上执行 `agent-cli-x workspace add proj /home/me/proj --default`，或由已授权的调用端执行 `agent-cli-x workspace add build-01 proj /home/me/proj --default`。它会保留注释地修改 `config.toml`，按 `serve` 的规则校验并重启 `serve`，不用手改配置；配置了任意工作区后，自动的 `home` 工作区不再提供。工作区名至少 2 个字符；工作区在 Windows 的 `C:` 盘上时加 `--allow rcud` 放开写权限。

在项目主机上运行 `agent-cli-x fingerprint`，核对配对命令里的指纹。在调用端执行 `token add` 打印的配对命令，填写可达地址并使用已核对的指纹。然后检查连接，在登记的工作区运行测试。

```bash
agent-cli-x info build-01
agent-cli-x exec build-01 --cwd proj: cargo test
```

临时授权可改用授权码：`agent-cli-x token add guest --code` 签发全权限临时令牌，默认 4 小时有效，`--ttl 8h` 延长到 8 小时；到期后调用返回 101，用 `token rotate guest` 续期；所有授权码都到期且tokens.toml 中没有长期令牌（默认令牌不计）时，`serve` 自动停止（退出码 0）。一次性使用时可跳过 `init`：本机未初始化时 `token add` 会先按默认值自动初始化；本机 `serve` 未运行时会提示先执行 `agent-cli-x serve`。

如果配对时用了其他名字，把 `build-01` 换成登记的名字。需要再次查看默认令牌的配对命令时，在项目主机运行 `agent-cli-x whoami --pair`。

`serve` 默认在后台运行，监听就绪后返回 PID，关掉终端仍可继续运行。启动时会检查一次入站防火墙：缺规则时，以管理员 / root 运行会自动放行（Windows 按程序放行，长期有效，改端口、升级都无需再改），否则在终端提示一次，按提示执行 `agent-cli-x firewall open` 即可。日志在 `~/.agent-cli-x/serve-logs/`。排查问题或交给服务管理器时用 `serve --foreground`。在本机执行 `agent-cli-x stop` 可停止服务（只能在本机执行，后台任务继续运行；配置了开机自启时下次开机仍会启动）。配置重启后自动启动，可参考 [Linux systemd 与 Windows 任务计划程序示例](README.md#start-at-boot-recommended)，再运行 `agent-cli-x doctor` 验证。

给 Codex、Claude Code 等助手使用时，可安装 [Agent Skill](skills/agent-cli-x/SKILL.md)，或运行 `agent-cli-x skill` 查看。

## 常用操作

| 操作 | 命令 |
| --- | --- |
| 查在线状态、系统、Shell 和工作区 | `agent-cli-x info build-01` |
| 在登记的项目目录跑命令 | `agent-cli-x exec build-01 --cwd proj: cargo test` |
| 读取文件的指定行 | `agent-cli-x read build-01 proj:Cargo.toml --lines 1:40` |
| 按文件名查找 | `agent-cli-x find build-01 '*.rs' proj:` |
| 将远端文件下载到本机 | `agent-cli-x get build-01 proj:Cargo.toml ./remote-Cargo.toml` |
| 诊断本机服务与已登记的对端 | `agent-cli-x doctor` |

路径用 `工作区名:相对路径`。使用 `exec --detach` 可运行后台任务，再通过 `job list`、`job logs` 或 `job kill` 管理。可用 `rollback` 撤销支持回滚的写入、编辑、上传与删除操作。完整语法见[命令参考](README.md#commands)。

## 输出上限

普通输出和 JSON 使用同样的数据预算。输出被截断时，命令会给出日志路径或继续读取的参数。

| 操作 | 默认上限与续读方法 |
| --- | --- |
| `exec` / `job logs` | 头尾各 100 行、各 16 KB，单行 2000 字符。截断时保留完整本机日志 |
| 多主机 `exec` | 每台主机头尾各 50 行、各 8 KB，分别保存完整日志 |
| `read` 未指定结束行 | 2000 行 / 64 KB，单行 2000 字符，多文件共用同一窗口。用 `--lines N:` 或 `path#N:` 继续 |
| `ls` | 500 条 / 60 KB 条目 JSON。用 `--offset` 分页，用 `--max` 调整条数 |
| `find` | 按修改时间倒序，默认 200 条。缩小范围或调整 `--max` |
| `grep` | 200 条匹配或文件 / 60 KB 条目 JSON（含上下文），单行 300 字符。可分页或用 `--glob` 缩小范围 |

Windows 桌面查看功能默认关闭。目标主机启用后，可只读列出窗口或捕获屏幕可见区域，常规截图预览最长边为 1600 像素、PNG 最多 1 MB。会话、权限和其他输出上限见[桌面查看](README.md#native-windows-desktop-observation)与[输出参考](README.md#output-limits)。

## 多台机器

可在多台机器上分别运行 `serve` 并登记所需的对端。连接关系按方向配置，由被访问的一方为调用者签发令牌。可为每个调用者单独撤销或轮换令牌。用 `exec A,B,C` 或 `exec @all` 并行运行命令，用 `info @all` 查看各端的在线状态和版本。

后台任务由独立进程管理，通常可跨服务重启继续运行。systemd 需按自启示例配置 `KillMode=process`。对端升级、重启、迁移端口和单向权限见[多主机参考](README.md#multiple-machines)。

## 安全与权限

- 适用于你管理的机器和可信网络。连接使用 TLS 并固定服务端指纹，调用需要令牌。
- 文件接口经过工作区、拒绝列表以及按路径配置的读写权限检查。工作区与访问规则保存在 `~/.agent-cli-x/config.toml`。可用 `AGENT_CLI_X_HOME` 指定配置目录。
- `exec` 以运行 `serve` 的系统用户权限执行。工作区规则只检查文件接口和工作目录，Shell 命令的隔离由系统权限负责。需要强隔离时，使用独立的低权限系统用户，并配置文件系统权限。
- 写入、编辑和上传会保留备份，文件删除默认使用系统回收站。请求写入 `~/.agent-cli-x/audit/`，记录来源 IP 和调用令牌名称。

[配置字段](README.md#configuration) · [安全模型](README.md#security-model) · [退出码](README.md#exit-codes)

## 许可证

[MIT](LICENSE)
