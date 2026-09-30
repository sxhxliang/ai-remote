# AI Remote 生产部署与多 Agent 运维指南

本文档介绍如何为 **AI Remote** 配置多 Token 信令服务器、在多平台（Linux、macOS、Windows、Docker）部署 Home-Agent 实例、使用独立路径访问各 Agent，以及链接分享的安全风险控制与常见故障排查。

---

## 目录

1. [多 Token 信令服务配置](#1-多-token-信令服务配置)
   - [环境变量设置说明](#环境变量设置说明)
   - [ROOM_TOKENS_JSON 规范与格式](#room_tokens_json-规范与格式)
   - [Linux Systemd 部署示例](#linux-systemd-部署示例)
   - [Docker 部署信令服务](#docker-部署信令服务)
   - [Nginx 反向代理与 SSL (WSS)](#nginx-反向代理与-ssl-wss)
2. [各平台 Home-Agent 启动命令](#2-各平台-home-agent-启动命令)
   - [CLI 参数与环境变量映射](#cli-参数与环境变量映射)
   - [Linux / macOS 启动方法](#linux--macos-启动方法)
   - [Windows 启动方法 (PowerShell / CMD)](#windows-启动方法-powershell--cmd)
   - [Docker / Docker Compose 启动 Agent](#docker--docker-compose-启动-agent)
   - [单机多 Agent 或局域网多设备配置](#单机多-agent-或局域网多设备配置)
3. [环境变量完整参考表](#3-环境变量完整参考表)
   - [信令服务器 (Signaling Server)](#信令服务器-signaling-server)
   - [家庭代理 (Home-Agent)](#家庭代理-home-agent)
4. [独立路径与链接分享风险说明](#4-独立路径与链接分享风险说明)
   - [URL Hash 机制与防护](#url-hash-机制与防护)
   - [潜在安全风险](#潜在安全风险)
   - [安全最佳实践](#安全最佳实践)
5. [常见故障排查指南 (Troubleshooting)](#5-常见故障排查指南-troubleshooting)
   - [403 Forbidden / Token 不匹配](#403-forbidden--token-不匹配)
   - [WebRTC 卡在 Connecting / ICE 穿透失败](#webrtc-卡在-connecting--ice-穿透失败)
   - [Ollama 连接失败或接口 404/403](#ollama-连接失败或接口-404403)
   - [WebSocket 断连或频繁重连](#websocket-断连或频繁重连)

---

## 1. 多 Token 信令服务配置

信令服务负责在浏览器与各 Home-Agent 之间转发 WebRTC 握手信令（SDP Offer/Answer 和 ICE Candidates）。

### 环境变量设置说明

信令服务器支持两种 Token 鉴权模式：
1. **单 Token 模式（默认 / 共享模式）**：
   - 由 `SIGNALING_TOKEN` 指定。所有进入房间的 Client 与 Agent 均需匹配此全局 Token。
   - 若未配置且未配置 `ROOM_TOKENS_JSON`，服务会自动生成一个 32 位的临时 Token。
2. **多 Token 模式（推荐 / 隔离模式）**：
   - 由 `ROOM_TOKENS_JSON` 指定，接收一个 JSON Object 字符串，格式为 `{"<room_id>": "<token>"}`。
   - **优先级高于 `SIGNALING_TOKEN`**：当 `ROOM_TOKENS_JSON` 非空时，每个房间只能使用其专属的 Token 连接，防止不同房间之间的未经授权访问与混淆。

### ROOM_TOKENS_JSON 规范与格式

`ROOM_TOKENS_JSON` 必须符合以下要求：
- **有效 JSON 字符串**：外部在 Shell 中建议用单引号包裹 `'{"...":"..."}'`。
- **Room ID 命名规则**：长度 1~64 字符，仅允许英文字母、数字、下划线 `_` 和中划线 `-`（即 `^[a-zA-Z0-9_-]{1,64}$`）。
- **Token 长度要求**：每个 Token 字符串的长度必须 **>= 16 字符**。建议使用安全随机生成的 Hex 或 Base64 字符串（例如 `openssl rand -hex 16`）。

#### 示例配置
```json
{
  "living-room": "9f82d3e1b4a5c6d7e8f90123456789ab",
  "study-mac": "a1b2c3d4e5f6789012345678abcdef01",
  "gpu-server": "fe80112233445566778899aabbccddeeff"
}
```

### Linux Systemd 部署示例

在 `/etc/ollama-link/signaling.env` 中配置：

```bash
# 绑定地址与端口
SIGNALING_BIND=127.0.0.1:8080
FRONTEND_DIR=/opt/ollama-link/frontend

# 配置各房间专属 Token
ROOM_TOKENS_JSON='{"living-room":"9f82d3e1b4a5c6d7e8f90123456789ab","study-mac":"a1b2c3d4e5f6789012345678abcdef01","gpu-server":"fe80112233445566778899aabbccddeeff"}'

# STUN / TURN 服务器配置（会随 ready 消息下发给浏览器和 Agent）
STUN_URL=stun:chat.example.com:3478
TURN_URL=turn:chat.example.com:3478?transport=udp,turn:chat.example.com:3478?transport=tcp
TURN_USER=ollama-link
TURN_PASS=your-turn-password-here
```

重新加载服务并启动：
```bash
sudo systemctl daemon-reload
sudo systemctl restart ollama-link-signaling
sudo journalctl -u ollama-link-signaling -f
```

### Docker 部署信令服务

通过 Docker 快速启动信令服务：

```bash
docker run -d \
  --name ai-remote-signaling \
  --restart unless-stopped \
  -p 8080:8080 \
  -e SIGNALING_BIND=0.0.0.0:8080 \
  -e ROOM_TOKENS_JSON='{"living-room":"9f82d3e1b4a5c6d7e8f90123456789ab","study-mac":"a1b2c3d4e5f6789012345678abcdef01"}' \
  -e STUN_URL="stun:chat.example.com:3478" \
  -e TURN_URL="turn:chat.example.com:3478?transport=udp,turn:chat.example.com:3478?transport=tcp" \
  -e TURN_USER="ollama-link" \
  -e TURN_PASS="your-turn-password" \
  ghcr.io/sxhxliang/ai-remote-signaling:latest
```

### Nginx 反向代理与 SSL (WSS)

WebRTC 信令通信及麦克风等现代浏览器特性强制要求 HTTPS / WSS 协议。以下是生产环境推荐的 Nginx 反代配置：

```nginx
server {
    listen 443 ssl http2;
    server_name chat.example.com;

    ssl_certificate /etc/letsencrypt/live/chat.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/chat.example.com/privkey.pem;

    # 静态前端资源直接由信令服务器代理或由 Nginx 处理
    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }

    # WebSocket 信令通道支持
    location /ws {
        proxy_pass http://127.0.0.1:8080/ws;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_read_timeout 86400s;
        proxy_send_timeout 86400s;
    }
}
```

---

## 2. 各平台 Home-Agent 启动命令

Home-Agent 运行在拥有 Ollama 或本地 LLM 服务的设备上。它通过长连接连入信令服务器的房间，并等待浏览器发起 WebRTC 会话。

### CLI 参数与环境变量映射

`ai-remote-agent` 命令行工具同时支持命令行参数与环境变量（命令行参数优先级高于环境变量）：

| 功能 | 命令行参数 | 环境变量 | 说明 |
| :--- | :--- | :--- | :--- |
| **信令地址** | `-u`, `--url` 或第1个位置参数 | `SIGNALING_URL` | 信令 WebSocket 地址，如 `wss://chat.example.com/ws` |
| **访问 Token** | `-t`, `--token` 或第2个位置参数 | `SIGNALING_TOKEN` | 必须与信令端该房间设置的 Token 一致，至少 16 字符 |
| **房间 ID** | `-r`, `--room` 或第3个位置参数 | `ROOM_ID` | 房间标识，如 `living-room`，默认 `default` |
| **Ollama 地址** | 环境变量 | `OLLAMA_BASE` | 默认 `http://127.0.0.1:11434` |
| **OpenAI 兼容地址**| 环境变量 | `OPENAI_BASE` | 默认回退至 `OLLAMA_BASE` |
| **OpenAI API Key** | 环境变量 | `OPENAI_API_KEY` | 可选，用于上游第三方 API 鉴权 |
| **接口白名单** | 环境变量 | `ALLOWED_PATHS` | 默认白名单安全路径，逗号分隔 |

---

### Linux / macOS 启动方法

#### 1. 命令行直接启动（临时运行或测试）

```bash
# 使用位置参数启动
ai-remote-agent wss://chat.example.com/ws 9f82d3e1b4a5c6d7e8f90123456789ab living-room

# 或使用显式命名参数启动
ai-remote-agent \
  --url wss://chat.example.com/ws \
  --token 9f82d3e1b4a5c6d7e8f90123456789ab \
  --room living-room
```

#### 2. 通过环境变量运行

```bash
export SIGNALING_URL="wss://chat.example.com/ws"
export SIGNALING_TOKEN="9f82d3e1b4a5c6d7e8f90123456789ab"
export ROOM_ID="living-room"
export OLLAMA_BASE="http://127.0.0.1:11434"

ai-remote-agent
```

#### 3. Linux systemd 多实例模板化管理

通过 systemd 实例模板 `@.service`，可以在同一台服务器轻松运行多个不同房间的 Agent。

创建模板文件 `/etc/systemd/system/ai-remote-agent@.service`：
```ini
[Unit]
Description=AI Remote Home Agent (%i)
After=network.target

[Service]
Type=simple
User=ollama-link
EnvironmentFile=/etc/ollama-link/agent-%i.env
ExecStart=/usr/local/bin/ai-remote-agent
Restart=always
RestartSec=5s

[Install]
WantedBy=multi-user.target
```

为不同房间创建独立的 env 文件，如 `/etc/ollama-link/agent-living-room.env`：
```bash
SIGNALING_URL=wss://chat.example.com/ws
ROOM_ID=living-room
SIGNALING_TOKEN=9f82d3e1b4a5c6d7e8f90123456789ab
OLLAMA_BASE=http://127.0.0.1:11434
```

启动并设置开机自启：
```bash
sudo systemctl enable --now ai-remote-agent@living-room
sudo systemctl status ai-remote-agent@living-room
```

---

### Windows 启动方法 (PowerShell / CMD)

#### 1. PowerShell 命令行启动

```powershell
# 临时命令启动
ai-remote-agent -u "wss://chat.example.com/ws" -t "9f82d3e1b4a5c6d7e8f90123456789ab" -r "living-room"
```

#### 2. PowerShell 脚本指定环境变量

```powershell
$env:SIGNALING_URL = "wss://chat.example.com/ws"
$env:SIGNALING_TOKEN = "9f82d3e1b4a5c6d7e8f90123456789ab"
$env:ROOM_ID = "living-room"
$env:OLLAMA_BASE = "http://127.0.0.1:11434"

ai-remote-agent
```

#### 3. CMD 批处理脚本 (`start-agent.bat`)

```cmd
@echo off
set SIGNALING_URL=wss://chat.example.com/ws
set SIGNALING_TOKEN=9f82d3e1b4a5c6d7e8f90123456789ab
set ROOM_ID=living-room
set OLLAMA_BASE=http://127.0.0.1:11434

ai-remote-agent
pause
```

#### 4. Windows 任务计划程序开机自启

可使用仓库提供的脚本自动安装登录自启任务：
```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\install-home-agent-task.ps1 -EnvFile .\home-agent.env
```

---

### Docker / Docker Compose 启动 Agent

若希望在容器化环境中运行 Home-Agent（例如 NAS 或 Linux 服务器）：

#### Docker 单命令启动
```bash
docker run -d \
  --name ai-agent-living-room \
  --restart unless-stopped \
  --add-host=host.docker.internal:host-gateway \
  -e SIGNALING_URL="wss://chat.example.com/ws" \
  -e SIGNALING_TOKEN="9f82d3e1b4a5c6d7e8f90123456789ab" \
  -e ROOM_ID="living-room" \
  -e OLLAMA_BASE="http://host.docker.internal:11434" \
  ghcr.io/sxhxliang/ai-remote-agent:latest
```

> **注意**：容器内访问宿主机上运行的 Ollama 时，请使用 `http://host.docker.internal:11434` 并加上 `--add-host=host.docker.internal:host-gateway`，而不要使用 `127.0.0.1`。

#### Docker Compose 多 Agent 编排配置 (`docker-compose.yml`)

```yaml
version: '3.8'

services:
  agent-living-room:
    image: ghcr.io/sxhxliang/ai-remote-agent:latest
    restart: unless-stopped
    extra_hosts:
      - "host.docker.internal:host-gateway"
    environment:
      - SIGNALING_URL=wss://chat.example.com/ws
      - ROOM_ID=living-room
      - SIGNALING_TOKEN=9f82d3e1b4a5c6d7e8f90123456789ab
      - OLLAMA_BASE=http://host.docker.internal:11434

  agent-gpu-server:
    image: ghcr.io/sxhxliang/ai-remote-agent:latest
    restart: unless-stopped
    environment:
      - SIGNALING_URL=wss://chat.example.com/ws
      - ROOM_ID=gpu-server
      - SIGNALING_TOKEN=fe80112233445566778899aabbccddeeff
      - OLLAMA_BASE=http://192.168.1.100:11434
```

---

### 单机多 Agent 或局域网多设备配置

- **同一台主机挂接不同后端**：例如一个 Agent 转发给本地 Ollama (`http://127.0.0.1:11434`)，另一个 Agent 转发给 vLLM / LocalAI (`http://127.0.0.1:8000`)。
  - 为每个 Agent 分配不同的 `ROOM_ID` 与 `SIGNALING_TOKEN`。
  - 每个进程各自作为独立 peer 接入信令，完全隔离。
- **多台不同主机**：书房电脑（M系列 Mac）、客厅工作站（RTX 4090）各自运行自己的 `ai-remote-agent`，接入同一个信令服务器对应的不同房间号。前端可以在多个 Agent 卡片之间一键自由切换。

---

## 3. 环境变量完整参考表

### 信令服务器 (Signaling Server)

| 环境变量 | 默认值 | 说明 |
| :--- | :--- | :--- |
| `SIGNALING_BIND` | `0.0.0.0:8080` | 信令服务器监听的主机与端口 |
| `SIGNALING_TOKEN` | 自动生成 (32位 UUID) | 全局默认访问 Token，单 Token 模式下使用 |
| `ROOM_TOKENS_JSON` | `{}` | 多房间 Token JSON 字典。设置后**覆盖全局 Token**，仅允许列表内的房间按其专有 Token 认证接入 |
| `FRONTEND_DIR` | 未设置 | 前端静态页面文件目录，信令服务可内嵌直接托管 Web UI |
| `PUBLIC_HOST` / `PUBLIC_IP` | 空 | 公网访问域名或 IP（用于自动生成直链提示） |
| `STUN_URL` | 空 | 下发给客户端与 Agent 的 STUN 地址，逗号分隔 |
| `TURN_URL` | 空 | 下发给客户端与 Agent 的 TURN 地址（支持 udp/tcp），逗号分隔 |
| `TURN_USER` | 空 | TURN 鉴权用户名 |
| `TURN_PASS` | 空 | TURN 鉴权密码 |

### 家庭代理 (Home-Agent)

| 环境变量 | 默认值 | 说明 |
| :--- | :--- | :--- |
| `SIGNALING_URL` | **必需** | 信令服务器的 WebSocket 地址（建议 `wss://...`） |
| `SIGNALING_TOKEN` | **必需** | 该 Agent 对应房间的接入 Token，长度需 >= 16 字符 |
| `ROOM_ID` | `default` | 房间名称，由字母、数字、中划线、下划线组成（1~64 字符） |
| `OLLAMA_BASE` | `http://127.0.0.1:11434` | 本地 Ollama 监听地址（必须仅为 Origin，无额外 Path） |
| `OPENAI_BASE` | 等同于 `OLLAMA_BASE` | OpenAI 兼容服务地址 |
| `OPENAI_API_KEY` | 空 | 可选，向上游发送请求时携带的 `Authorization: Bearer <KEY>` |
| `ALLOWED_PATHS` | `/api/generate,/api/chat,/api/tags,/v1/models,/v1/chat/completions` | 安全路径白名单，防止路径穿越和任意内网调用 |
| `REQUEST_TIMEOUT_SECS`| `600` | 单次 LLM 请求超时时长（秒） |
| `FORCE_RELAY` | `false` | 强制走 TURN 中继，禁止 P2P 直连（调试 NAT 穿透时适用） |
| `ICE_SERVERS_JSON` | `[]` | 本地自定 ICE 服务器配置 JSON |

---

## 4. 独立路径与链接分享风险说明

前端提供了为每个 Agent 生成独立路径的功能（例如 `https://chat.example.com/#/agent/living-room?token=9f82d3e1b4a5...`）。

### URL Hash 机制与防护

- **Hash 保护设计**：独立路径采用 URL Hash (`#/...`) 而非标准 Query Path (`?...`)。
- **不发送到服务端**：根据 RFC 标准，浏览器不会将 `#` 后面的任何内容（包括参数）包含在发往 Web 服务器的 HTTP 请求报文中。
- **避免日志泄漏**：信令服务器的 HTTP 日志、Nginx 的 `access.log`、中间 CDN 或安全代理服务器**均无法记录到 URL 中的 Token**。

### 潜在安全风险

尽管有 Hash 机制防护，在分享和使用该链接时仍需警惕以下风险：

1. **凭证等同于完整访问权**：
   - 任何获取到该直链的人，都可以直接在浏览器打开并与该 Agent 建立 WebRTC 连接，自由调用家庭电脑上的 Ollama 模型及算力。
2. **浏览器痕迹与屏幕分享泄露**：
   - 带有 Token 的完整 URL 会保存在本地浏览器的历史记录（History）、收藏夹（Bookmarks）以及地址栏自动补全中。
   - 在远程会议或投屏时，地址栏中的 URL 极易被参会人员截获。
3. **聊天工具或剪贴板监听**：
   - 如果通过微信、钉钉、Slack、飞书或第三方不加密通信工具发送带有 Token 的完整链接，可能被聊天工具的链接抓取程序（Link Preview Bot）或第三方安全审计系统记录。
4. **明文 HTTP 监听风险**：
   - 如果信令服务未使用 HTTPS / WSS，在局域网内可能被 Wi-Fi 嗅探者截获 WebSocket 协商报文中的 Token。

### 安全最佳实践

- [x] **强制启用 HTTPS / TLS**：通过反代开启严格的 HTTPS / WSS，禁止纯 HTTP 访问。
- [x] **避免在公共平台传播**：严禁将含有 Token 的直连链接发布到 GitHub Issues、论坛、社交媒体或公开群聊中。
- [x] **定期轮换 Token**：定期在信令服务端的 `ROOM_TOKENS_JSON` 中修改对应房间的 Token 并重启信令服务；旧 Token 会立即失效。
- [x] **收紧 `ALLOWED_PATHS` 白名单**：确保 Agent 的白名单仅开放聊天与模型获取端点，不要随意加入未经验证的管理接口。
- [x] **使用复杂随机 Token**：建议使用 32 位及以上的高熵密码学随机串（如 `openssl rand -hex 16`），杜绝弱口令。

---

## 5. 常见故障排查指南 (Troubleshooting)

### 403 Forbidden / Token 不匹配

**现象**：Agent 启动后立即退出，报错 `Invalid Token` 或前端提示 `Connection closed (403)`。

**排查步骤**：
1. **检查 `ROOM_TOKENS_JSON` 语法**：
   - 确保 JSON 键值对正确，单双引号没有转义错误。
   - 检查 JSON 中对应 `ROOM_ID` 的 Token 是否与 Agent 启动传入的完全一致。
2. **Token 长度**：
   - 信令服务端与 Agent 端均强制校验 Token 长度必须 **>= 16 字符**。不足 16 字符会直接报错拒绝启动。
3. **Room ID 字符合法性**：
   - 必须全为 ASCII 字母、数字、`-` 或 `_`，不能包含空格、中文或特殊符号。

---

### WebRTC 卡在 Connecting / ICE 穿透失败

**现象**：前端与信令服务连接成功，但 WebRTC 状态一直停留在 `Connecting`，最终变为 `Failed` 或 `Disconnected`。

**排查步骤**：
1. **排查 UDP 阻断**：
   - WebRTC 默认优先尝试 UDP 打洞与传输。国内某些公司企业内网或移动 4G/5G 热点会直接丢弃外部 UDP 数据包。
   - **解决方案**：在信令服务上配置基于 TCP 的 TURN 服务器（例如 coturn），并在前端/信令下发配置中启用 `turns:domain:443?transport=tcp` 或 `turn:domain:3478?transport=tcp`。
2. **Agent 端限制**：
   - Home-Agent 核心库仅支持 UDP 类型的 STUN 和 TURN，不直接支持 TCP TURN。因此 Agent 设备所在的家庭网络必须允许出站 UDP。
3. **测试强制中继模式**：
   - 在 Agent 端设置 `FORCE_RELAY=true`，若此时能正常连接，说明公网 P2P 直接打洞受阻（对称 NAT），需要依赖稳定 TURN 服务器中继。

---

### Ollama 连接失败或接口 404/403

**现象**：WebRTC 数据通道已连通，但发送消息提示 `Request failed: Connection refused (os error 111)` 或 `404 Not Found`。

**排查步骤**：
1. **测试 Agent 本地到 Ollama 连通性**：
   在运行 Agent 的设备上执行：
   ```bash
   curl -i http://127.0.0.1:11434/api/tags
   ```
   如果连接拒绝，检查 Ollama 是否启动，或是否监听了自定义端口/地址。
2. **Docker 环境中的本地回环地址**：
   如果在 Docker 容器内运行 Agent，`127.0.0.1` 会指向容器自身而非宿主机。请将 `OLLAMA_BASE` 设为 `http://host.docker.internal:11434`。
3. **白名单拦截**：
   如果调用了自定模型接口返回 `path not allowed`，检查 `ALLOWED_PATHS` 是否包含了该请求路径。

---

### WebSocket 断连或频繁重连

**现象**：前端显示 `Disconnected`，间隔数秒后反复 `Connecting`。

**排查步骤**：
1. **Nginx 超时配置**：
   默认情况下 Nginx 对空闲代理连接可能在 60 秒后关闭。请确保在 Nginx 的 `/ws` location 中设置了长超时：
   ```nginx
   proxy_read_timeout 86400s;
   proxy_send_timeout 86400s;
   ```
2. **Upgrade 头缺失**：
   确保 Nginx 配置了正确的 WebSocket 握手头：
   ```nginx
   proxy_set_header Upgrade $http_upgrade;
   proxy_set_header Connection "upgrade";
   ```
