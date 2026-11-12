# agent-cli-x 开发约定

agent-cli-x 的主要使用者是 AI agent，它的每一行输出都会进入模型上下文，按 token 计费。
**节省 AI token 是本项目的核心设计原则**：新增或修改任何命令、输出、提示语、技能文档时都要遵守下面的规则。

## 输出必须有上限

- 任何可能随数据量增长的输出都要有默认上限，并同时按**行数和字节数**限制；单行也要截断（当前 2000 字符）。
- 被截断时必须：说明省略了多少、怎么拿到其余部分（下一段 `--lines` 范围、`--max`、本地完整日志路径）。不要让 AI 靠猜去重试。
- `--json` 和普通输出使用**同一套上限**；只有显式的 `--full` / 明确的范围参数才能放开。
- 当前默认值（改动前先想清楚 token 成本，并同步 README 的 "Output limits" 与 SKILL.md）：

  | 输出 | 默认上限 | 代码位置 |
  |---|---|---|
  | `exec` / `job logs` | 头尾各 100 行、各 16KB | `src/client/output.rs` `SINGLE` |
  | 多对端 `exec` 的每个对端 | 头尾各 50 行、各 8KB | `src/client/output.rs` `PER_PEER` |
  | `read`（未给结束行） | 2000 行 / 64KB、单行 2000 字符 | `src/cli.rs` `READ_WINDOW_*` |
  | `ls` | 500 条 / 60KB 条目 JSON（`--max` / `--offset`） | `src/cli.rs` / `src/server/fs.rs` |
  | `grep` | 200 条 / 60KB 条目 JSON、单行 300 字符（`--max` / `--offset`） | `src/server/fs.rs` |

- 截断逻辑统一复用 `client::output::HeadTail` / `clip_text`，不要再各自实现。

## 减少调用轮次

一次多余的往返要重发整个上下文，往往比多几行输出更贵。

- 退出码要稳定、语义明确（见 SKILL.md 的退出码表），让 AI 不读错误文本也能判断下一步。
- 错误和提示里直接给出可执行的下一条命令（例如 `继续读取用 --lines 2001:4000`、`请先执行 agent-cli-x peer add`）。
- 能缓存的就缓存（如 `peer list` 缓存 OS/shell），能一次做完的不要拆成多次调用（多对端并行、`cp` 直传、`exec --script`）。

## 输出要紧凑

- 普通输出不加装饰性内容（横幅、表情、重复的标题、成功时的冗长说明）。成功时尽量一行，失败时给原因加下一步。
- 状态与提示走 stderr，数据走 stdout，方便 AI 和脚本分别处理。
- `--json` 只放必要字段，不重复内容（同一份文本不要既在 `stdout` 又在别的字段里）。

## 技能文档（`skills/agent-cli-x/SKILL.md`）

- 触发后整篇都会进入上下文，目标保持在约 1k token（当前约 4KB）以内。
- 只写 AI 高频需要的命令、规则和安全约束；低频内容（开机自启、升级细节、配置项说明）放 README，在 SKILL.md 里一句话指过去。
- 新增功能时先问：AI 不知道它会不会犯错或多走几轮？不会就不写进 SKILL.md。
- SKILL.md 通过 `include_str!` 编进二进制（`agent-cli-x skill`），修改后要重新构建。

## 测试

- 改动输出上限或截断逻辑时，同步更新 `src/client/output.rs` 的单测和 `tests/e2e.rs` 的 `output_limits_save_context`。
- 提交前：`cargo fmt --check`、`cargo clippy --all-targets`、`cargo test`。
