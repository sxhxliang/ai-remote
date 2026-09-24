# AI Remote — Ollama over WebRTC

[![Build and release](https://github.com/sxhxliang/ai-remote/actions/workflows/build.yml/badge.svg)](https://github.com/sxhxliang/ai-remote/actions/workflows/build.yml)

公司电脑只需打开浏览器，通过 WebRTC DataChannel 访问家里 Agent，再由 Agent 请求本机 Ollama。聊天数据不经过信令服务；连接需要中继时，由 TURN 转发加密的 WebRTC 数据。Ollama 保持监听本机回环地址，不需要暴露 11434，也不需要 VPN。

## 一键安装

安装 [最新 Release](https://github.com/sxhxliang/ai-remote/releases/latest)，**无需 Rust、Node.js 或编译工具**。每个安装包包含家庭 Agent、信令服务器、TURN 服务器、已构建的 Web UI 和部署模板；按所在机器的用途启动对应服务即可。

Linux / macOS：

```sh
curl -fsSL https://github.com/sxhxliang/ai-remote/releases/latest/download/install.sh | sh
```

Windows PowerShell（支持 5.1 / 7，不需要管理员权限）：

```powershell
irm https://github.com/sxhxliang/ai-remote/releases/latest/download/install.ps1 | iex
```

| 系统 | 架构 | 最低要求 |
| --- | --- | --- |
| Linux | x64、ARM64 | glibc 2.35+，例如 Ubuntu 22.04+、Debian 12+；不支持 Alpine / musl |
| macOS | Intel x64、Apple Silicon ARM64 | macOS 11+ |
| Windows | x64、ARM64 | Windows 10/11 或 Server 2019+；ARM64 使用对应的 Windows 系统 |

安装脚本自动识别架构、校验安装包 SHA-256，并保留已有配置。重复执行可更新到最新稳定版，旧版文件保留在 `versions/` 中。校验失败会终止安装。

| 内容 | Linux / macOS | Windows |
| --- | --- | --- |
| 安装目录 | `~/.local/share/ai-remote` | `%LOCALAPPDATA%\Programs\ai-remote` |
| 配置目录 | 安装目录下的 `config/` | 安装目录下的 `config\` |
| 命令目录 | `~/.local/bin`，按安装提示加入 PATH | 安装目录下的 `bin\`，自动加入当前用户 PATH |
| 当前版本 | 安装目录下的 `current` 符号链接 | 安装目录下的 `current.json` |

三个核心命令是 `ai-remote-agent`、`ai-remote-signaling`、`ai-remote-turn`（均支持 `--version`），Linux 还提供一键部署向导 `ai-remote-deploy`。信令服务开箱即用（免配置自动生成 Token 与直连网址），也可使用一键部署向导快速配置 systemd 服务。

指定版本或目录：

```sh
curl -fsSL https://github.com/sxhxliang/ai-remote/releases/latest/download/install.sh | AI_REMOTE_VERSION=v0.2.1 sh
curl -fsSL https://github.com/sxhxliang/ai-remote/releases/latest/download/install.sh | AI_REMOTE_INSTALL_DIR="$HOME/apps/ai-remote" AI_REMOTE_BIN_DIR="$HOME/bin" sh
```

```powershell
& ([scriptblock]::Create((irm https://github.com/sxhxliang/ai-remote/releases/latest/download/install.ps1))) -Version v0.2.1 -InstallDir 'D:\Apps\ai-remote'
```

Windows 可加 `-NoPath` 禁用 PATH 修改。两个脚本均支持 `AI_REMOTE_REPO=owner/repo`，便于使用自己的 fork。也可在 Release 页面手动下载 ZIP / tar.gz 和对应 `.sha256` 文件；`SHA256SUMS` 同时覆盖安装包和安装脚本。

## Windows 本地 Mock 人工测试（源码）

先安装 **Node.js 22.12+**、**Rust stable（MSVC）** 和 **Visual Studio C++ Build Tools**。无需安装真实 Ollama 或下载模型。

本节的 Mock 和开发脚本需要源码，不包含在预编译安装包中：

```powershell
git clone https://github.com/sxhxliang/ai-remote.git
cd ai-remote
```

在项目根目录双击 **`start-local.cmd`**，也可以在 PowerShell 运行：

```powershell
.\start-local.cmd
```

脚本会安装锁定的前端依赖、编译三个 Rust 服务、在后台启动整套服务，并打开浏览器。首次编译需要一些时间。页面地址是 **http://127.0.0.1:5173**。

人工验证：

1. 页面显示“当前使用本地 Mock Ollama”，连接配置已自动填写。
2. 点击“连接”，等待右上角显示“已连接”，模型列表应包含 `qwen2.5:7b` 和 `mock:latest`。
3. 输入“你好”并发送，应逐段收到“这是本地 Mock Ollama。中文流式响应正常，不会调用真实模型。”
4. 再发一条消息，在生成过程中点击“停止生成”；然后再次发送，确认连接还能继续使用。
5. 如需验证中继，展开连接设置，勾选“仅使用中继”，重新连接。右上角应显示“TURN 中继”。将 TURN 地址只保留 `turn:127.0.0.1:3478?transport=tcp` 可人工测试 TCP 中继。

Windows 脚本默认使用以下地址，全部只绑定回环地址：

| 服务 | 地址 |
| --- | --- |
| Web UI | http://127.0.0.1:5173 |
| Mock Ollama | http://127.0.0.1:11435 |
| 信令 | ws://127.0.0.1:8080/ws |
| TURN UDP / TCP | 127.0.0.1:3478 |
| TURN TLS / HTTPS 开发入口 | 127.0.0.1:18443 |

Mock 使用 11435，避免占用真实 Ollama 的 11434。关闭启动窗口后服务继续运行。**双击 `stop-local.cmd` 可统一停止本项目启动的服务。** 再次运行启动脚本会先停止上一套测试服务，再构建并启动。不会关闭其他项目或真实 Ollama。

后续快速启动，以及不自动打开浏览器的用法：

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\start-local.ps1 -SkipInstall -SkipBuild
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\start-local.ps1 -NoBrowser
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\start-local.ps1 -MockPort 11436
```

日志在 `.local/`：`launcher.error.log`、`home-agent.log`、`signaling.log`、`turn.log`、`mock-ollama.log` 和 `frontend.log`。运行配置与随机测试凭据在 `.local/runtime.json`，无需手动抄写。每个房间同时允许一个 Agent 和一个浏览器；测试第二个页面前先断开旧页面。

## 目录与开发命令

- `frontend/`：React 聊天 UI、Fetch 兼容客户端和 NDJSON 解析。
- `home-agent/`：Rust WebRTC → 本地 HTTP 桥接。
- `signaling-server/`：Rust / axum 的鉴权、房间与 SDP / ICE 转发，以及生产前端静态文件服务。
- `turn-server/`：Rust TURN，支持 UDP、TCP、TLS；TLS 入口可同时代理 HTTPS / WSS。
- `scripts/`：本地启动、停止、Mock 和集成测试。
- `deploy/`：环境变量模板与 systemd 单元。各 Rust 服务有独立 Cargo.toml，没有根 Cargo workspace。
- `.github/workflows/build.yml`：六个平台的自动构建、测试与 Release 发布。

跨平台开发与自动验证：

```sh
npm --prefix frontend ci
npm --prefix frontend test
node scripts/dev.mjs
node scripts/stop-local.mjs
node scripts/local-test.mjs --keep
```

`dev.mjs` 默认将 Mock 放在 11434；可加 `--mock-port 11435`。`local-test.mjs` 使用 11434，并独立启动、测试和回收整套服务，运行前需要先停止已有测试实例。`--keep` 在测试成功后保留服务；`--no-build` 复用已有构建。

对每个 Rust crate 执行：

```sh
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

集成测试覆盖鉴权、重复角色、直连、重新连接、中文分片、HTTP 错误、取消、超时、路径白名单，以及 TURN UDP / TCP / TLS 和 WSS。TCP / TLS 的原生探针通过测试专用分帧适配器访问真实 TURN 监听器；浏览器的实际交互由上面的人工步骤验证。

## 自动构建与发布

推送 `main`、提交 Pull Request，或在 [Actions](https://github.com/sxhxliang/ai-remote/actions/workflows/build.yml) 手动运行，都会构建 Linux / macOS / Windows 的 x64 和 ARM64 六种版本。构建产物在运行页面的 Artifacts 中保留 14 天。

每次构建执行前端测试、Rust 格式 / Clippy / 单元测试、安装包校验和原生安装测试。安装测试包含首次安装、重复安装、升级保留配置、校验失败和异常归档拒绝；Windows 同时验证 PowerShell 5.1 与 7。Linux x64 还会启动本地 Mock，验证完整 WebRTC、TURN UDP / TCP / TLS 和 WSS 通信。

推送版本标签后，工作流等待六个平台全部成功，再发布含安装包、安装脚本和 SHA256SUMS 的 GitHub Release。例如首个版本：

```sh
git tag v0.1.0
git push origin v0.1.0
```

发布新版本前，统一三个 Rust 服务的 `Cargo.toml` 包版本并更新各自的 `Cargo.lock`；标签必须与包版本完全匹配。`v0.2.0-rc.1` 等标签发布为预览版，不替换最新稳定版。已公开的 Release 不会被工作流覆盖。

打包和版本检查也可单独执行：

```sh
node --test scripts/release.test.mjs
node scripts/release-version.mjs
```

## 公网部署

VPS 部署信令、TURN 和构建后的前端，家庭电脑部署 Agent 与真实 Ollama。公司电脑无需安装上述开发工具。

```text
公司浏览器 <-- WebRTC DataChannel --> 家庭 Agent --> 127.0.0.1:11434 Ollama
     |                 |                  |
     +------ WSS 信令服务器 --------------+
          TURN 在无法直连时中继
```

### VPS

1. 为 VPS 配置域名，例如 `chat.example.com`，准备该域名的有效 TLS 证书。
2. 使用预编译包安装到 systemd 模板约定的目录：

```sh
curl -fsSL https://github.com/sxhxliang/ai-remote/releases/latest/download/install.sh | sudo env AI_REMOTE_INSTALL_DIR=/opt/ollama-link AI_REMOTE_BIN_DIR=/usr/local/bin sh
```

3. 运行一键部署向导（自动创建低权限系统用户 `ollama-link`、生成安全 Token、配置并启动 systemd 服务）：

```sh
# 交互式菜单选择
sudo ai-remote-deploy

# 或直接非交互式一键启动信令服务：
sudo ai-remote-deploy signaling
```

部署成功后，终端将输出：
- 浏览器一键访问 URL（含 Token Hash）
- Web 控制中心与房间连接监控地址（`/setup#token=...`，需要 Token 才会显示接入信息）
- 家里 Agent 的一键接入终端指令

部署 TURN 后，脚本会把 `STUN_URL`、`TURN_URL`（UDP 与 TCP 两个地址）和 TURN 凭据写入 `/etc/ollama-link/signaling.env`。信令服务在浏览器和 Agent 通过 Token 鉴权后，随 `ready` 消息把这些 STUN/TURN 配置下发给两端，两端都不需要再手动填写。直接用 `http://<公网IP>:8080` 访问时，云安全组需放行 **8080/TCP、3478/UDP、3478/TCP、49160–49200/UDP**。

4. （可选）如需部署 TURN 中继服务或使用 TLS 证书，可在向导中选择 `deploy_turn`，或编辑 `/etc/ollama-link/turn.env`。复制 `/opt/ollama-link/deploy/ollama-link-turn.service` 到 `/etc/systemd/system/` 后执行 `sudo systemctl enable --now ollama-link-turn`。

TURN 的 TLS 监听器用 **同一 IP 的 443/TCP** 接收浏览器 HTTPS、WSS 和 TURN/TLS。HTTP / WSS 固定转发到 `HTTPS_UPSTREAM=127.0.0.1:8080`，TURN 数据进入中继。信令服务与网页不需要再占用公网 443。

VPS 防火墙与云安全组需放行 **443/TCP、3478/UDP、49160–49200/UDP**；如需明文 TCP TURN，再放行 3478/TCP。信令 8080 保持回环监听。证书更新后重启 TURN 服务以重新加载证书。也可以使用 coturn 替换 TURN；coturn 是独立的 C/C++ 项目，需要自行安排其 TLS 监听与网页 443 的端口分配。

### 家庭电脑

Ollama 使用默认的 `127.0.0.1:11434`。将 `deploy/home-agent.env.example` 复制为自己的配置，填入 VPS 的信令地址、房间号和相同的 Token。STUN/TURN 由信令服务自动下发，通常无需配置。

使用一键安装包时，`config/home-agent.env` 已创建。Windows 编辑配置并运行 Agent：

```powershell
$aiRemoteRoot = Join-Path $env:LOCALAPPDATA 'Programs\ai-remote'
$aiRemoteRelease = Get-Content "$aiRemoteRoot\current.json" -Raw | ConvertFrom-Json
$aiRemoteVersionDir = Join-Path $aiRemoteRoot ("versions\{0}-{1}" -f $aiRemoteRelease.version, $aiRemoteRelease.target)
notepad "$aiRemoteRoot\config\home-agent.env"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$aiRemoteVersionDir\scripts\run-home-agent.ps1" -EnvFile "$aiRemoteRoot\config\home-agent.env"
```

Linux / macOS 前台运行（先编辑配置）：

```sh
set -a
. "$HOME/.local/share/ai-remote/config/home-agent.env"
set +a
"$HOME/.local/bin/ai-remote-agent"
```

Windows 从源码构建和运行：

```powershell
cargo build --locked --release --manifest-path .\home-agent\Cargo.toml
Copy-Item .\deploy\home-agent.env.example .\home-agent.env
# 编辑 home-agent.env 后运行：
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\run-home-agent.ps1 -EnvFile .\home-agent.env
```

Windows 当前用户登录自启（源码目录）：

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\install-home-agent-task.ps1 -EnvFile .\home-agent.env
```

预编译包使用 `$aiRemoteVersionDir\scripts\install-home-agent-task.ps1`，传入安装目录的 `config\home-agent.env`。计划任务固定使用注册时的版本目录，升级后需重新注册任务。

Linux 使用 `deploy/ollama-link-home-agent.service`，程序路径为 `/opt/ollama-link/bin/home-agent`，配置路径为 `/etc/ollama-link/home-agent.env`，服务账户同为 `ollama-link`。加载后运行 `systemctl enable --now ollama-link-home-agent`。

当前 webrtc-rs 家庭 Agent 只使用信令下发配置中的 STUN 与 **UDP TURN** 地址，TCP/TLS 地址留给浏览器。如需覆盖，可在家庭配置中填写 `turn:chat.example.com:3478?transport=udp`。公司浏览器可使用 **`turns:chat.example.com:443?transport=tcp`**。浏览器到 VPS 使用 TLS/TCP、VPS 到家庭 Agent 使用 UDP，可以满足公司侧阻断 UDP 的场景。TURN/TLS 不是 HTTP 流量；仅允许 HTTP 代理或进行严格协议过滤的网络仍需实地验证。

### 公司浏览器

打开 `https://chat.example.com`，填写房间号和 Token，点击连接后选择模型聊天。TURN 地址及凭据由信令服务自动下发，也可以在连接设置中额外填写。页面支持流式回复、停止生成、清空历史、模型列表，以及连接中断后的自动重连。Token 不写入浏览器持久存储。

## 配置与接口

Agent 的 `ALLOWED_PATHS` 默认精确允许 `GET /api/tags`、`POST /api/chat` 和 `POST /api/generate`。路径穿越、前缀匹配、替换请求主机和 HTTP 重定向不会绕过白名单。`/api/version` 可显式加入并以 GET 访问。不要开放模型删除、下载等接口作为聊天所需权限。

信令服务的 `STUN_URL`、`TURN_URL`（可用逗号分隔多个地址）、`TURN_USER` 和 `TURN_PASS` 会随鉴权后的 `ready` 消息下发给浏览器和 Agent，并与两端的本地设置合并去重；两端都没有任何配置时才使用公共 STUN。`/api/setup` 只有携带 `Authorization: Bearer <Token>` 时才返回 Token 和接入指令，且从不返回 TURN 密码。

Agent 的 `ICE_SERVERS_JSON` 接受浏览器形式的 ICE 配置，`urls` 可为字符串或数组。Agent 也支持 `STUN_URL`、`TURN_URL`、`TURN_USER`、`TURN_PASS` 和 `FORCE_RELAY`。`REQUEST_TIMEOUT_SECS` 默认 600 秒。

客户端 `OllamaRemoteClient.fetch` 是绑定好的 Fetch 兼容方法，支持 `Request`、`Response`、`ReadableStream` 和 `AbortSignal`，可以注入官方 Ollama JS SDK：

```js
import { Ollama } from 'ollama/browser';
import { OllamaRemoteClient } from './webrtcClient.js';

const client = new OllamaRemoteClient({ signalingUrl, room, token, iceServers });
await client.connect();
const ollama = new Ollama({ host: 'http://ollama.local', fetch: client.fetch });
const stream = await ollama.chat({
  model: 'qwen2.5:7b',
  messages: [{ role: 'user', content: '你好' }],
  stream: true,
});
for await (const part of stream) console.log(part.message.content);
await client.disconnect();
```

数据通道按请求 ID 关联，响应分为 `response`、`chunk`、`done`、`error`。分片通过 base64 保留原始字节，按消费发送 ACK；大请求分片上传，单请求上限 8 MiB，最多 4 个并发请求。取消操作会关闭 Agent 到 Ollama 的上游请求。

Mock 仅用于本地测试，支持 `utf8-split`、`http-error`、`disconnect`、`burst`、`stall`、`stall-stream` 等 `x-mock-scenario` 请求头；`GET /__mock__/requests` 可检查最近请求与取消状态。
