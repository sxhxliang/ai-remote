#!/usr/bin/env bash
# ==============================================================================
# AI Remote 一键生产环境部署脚本 (Linux systemd)
# 支持一键部署：信令服务器 (含前端与Web配置中心)、家庭 Agent、TURN 中继服务
# ==============================================================================
set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m' # No Color

log_info() { printf "${GREEN}[INFO]${NC} %s\n" "$*"; }
log_warn() { printf "${YELLOW}[WARN]${NC} %s\n" "$*"; }
log_err()  { printf "${RED}[ERROR]${NC} %s\n" "$*" >&2; }

# 检查 root 权限
check_root() {
  if [ "$(id -u)" -ne 0 ]; then
    log_err "此部署脚本需要 root 权限以配置 systemd 服务。请使用 sudo 执行："
    printf "  sudo bash %s %s\n" "$0" "${*:-}"
    exit 1
  fi
}

# 检查 systemd
check_systemd() {
  if ! command -v systemctl >/dev/null 2>&1 || ! [ -d /run/systemd/system ]; then
    log_err "当前系统未检测到正在运行的 systemd，无法自动注册服务。"
    exit 1
  fi
}

# 自动生成随机安全 Hex Token (32 字符)
generate_token() {
  if command -v openssl >/dev/null 2>&1; then
    openssl rand -hex 16
  elif [ -r /dev/urandom ]; then
    head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n'
  else
    date +%s%N | sha256sum | head -c 32
  fi
}

# 读取已有配置中的值：重复部署时沿用旧密钥，避免已接入的 Agent 和浏览器失效
read_env_value() {
  local file="$1" key="$2"
  [ -f "$file" ] || return 0
  { grep -E "^${key}=" "$file" || true; } | tail -n1 | cut -d= -f2-
}

# enable --now 不会重启已在运行的服务，改写的配置不会生效，因此显式 restart
start_service() {
  systemctl daemon-reload
  systemctl enable "$1" >/dev/null 2>&1
  systemctl restart "$1"
}

# 把 TURN 配置写入信令配置；信令服务在鉴权后把它下发给浏览器与家里 Agent
sync_turn_to_signaling() {
  local pub_ip="$1" turn_user="$2" turn_pass="$3"
  local sig_env="/etc/ollama-link/signaling.env"
  [ -f "$sig_env" ] && [ -n "$pub_ip" ] && [ -n "$turn_pass" ] || return 0
  grep -v -E '^(STUN_URL|TURN_URL|TURN_USER|TURN_PASS)=' "$sig_env" > "${sig_env}.tmp" || true
  cat >> "${sig_env}.tmp" <<EOF
STUN_URL=stun:${pub_ip}:3478
TURN_URL=turn:${pub_ip}:3478?transport=udp,turn:${pub_ip}:3478?transport=tcp
TURN_USER=${turn_user:-ollama-link}
TURN_PASS=${turn_pass}
EOF
  mv "${sig_env}.tmp" "$sig_env"
  chmod 640 "$sig_env"
  chown root:ollama-link "$sig_env" 2>/dev/null || true
}

# 获取本机公网 IPv4
get_public_ip() {
  local ip=""
  for url in "https://api.ipify.org" "https://ifconfig.me" "https://icanhazip.com"; do
    ip=$(curl -4 -s --connect-timeout 3 "$url" 2>/dev/null | tr -d ' \r\n' || true)
    if [[ "$ip" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
      echo "$ip"
      return 0
    fi
  done
  # 本地 IP 回退
  ip=$(ip -4 route get 1.1.1.1 2>/dev/null | awk '{print $7}' | head -n1 || true)
  echo "${ip:-127.0.0.1}"
}

# 查找安装目录与二进制文件
find_install_paths() {
  SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
  PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." 2>/dev/null && pwd -P || echo "")"

  INSTALL_ROOT="${AI_REMOTE_INSTALL_DIR:-/opt/ollama-link}"
  BIN_DIR="${INSTALL_ROOT}/bin"

  # 如果目标目录缺少程序，尝试从当前脚本所在目录结构或安装脚本初始化
  if [ ! -f "$BIN_DIR/signaling-server" ] && [ -n "$PROJECT_ROOT" ]; then
    if [ -f "$PROJECT_ROOT/bin/signaling-server" ]; then
      BIN_DIR="$PROJECT_ROOT/bin"
      INSTALL_ROOT="$PROJECT_ROOT"
    elif [ -f "$PROJECT_ROOT/signaling-server/target/release/signaling-server" ]; then
      mkdir -p "$INSTALL_ROOT/bin"
      cp "$PROJECT_ROOT/signaling-server/target/release/signaling-server" "$INSTALL_ROOT/bin/"
      [ -f "$PROJECT_ROOT/home-agent/target/release/home-agent" ] && cp "$PROJECT_ROOT/home-agent/target/release/home-agent" "$INSTALL_ROOT/bin/"
      [ -f "$PROJECT_ROOT/turn-server/target/release/turn-server" ] && cp "$PROJECT_ROOT/turn-server/target/release/turn-server" "$INSTALL_ROOT/bin/"
      BIN_DIR="$INSTALL_ROOT/bin"
    fi
  fi
}

# 确保专用低权限服务账户存在
ensure_service_user() {
  if ! id -u ollama-link >/dev/null 2>&1; then
    log_info "创建系统专用服务账户: ollama-link"
    useradd --system --no-create-home --shell /usr/sbin/nologin ollama-link 2>/dev/null || \
      adduser -S -D -H -s /usr/sbin/nologin ollama-link 2>/dev/null || true
  fi
}

# 确保配置目录与权限
ensure_config_dir() {
  mkdir -p /etc/ollama-link
  chmod 750 /etc/ollama-link
  chown root:ollama-link /etc/ollama-link 2>/dev/null || true
}

# 部署信令服务器
deploy_signaling() {
  log_info "开始配置并部署信令服务器 (Signaling Server)..."
  check_root
  check_systemd
  find_install_paths
  ensure_service_user
  ensure_config_dir

  local pub_ip
  pub_ip=$(get_public_ip)
  local env_file="/etc/ollama-link/signaling.env"
  local token="${SIGNALING_TOKEN:-$(read_env_value "$env_file" SIGNALING_TOKEN)}"
  if [ -z "$token" ]; then
    token=$(generate_token)
  fi

  local bind="${SIGNALING_BIND:-0.0.0.0:8080}"
  local service_file="/etc/systemd/system/ollama-link-signaling.service"

  # 写入配置文件
  cat > "$env_file" <<EOF
# AI Remote 信令服务环境配置 (由 deploy.sh 自动生成)
SIGNALING_BIND=${bind}
SIGNALING_TOKEN=${token}
ROOM_ID=default
PUBLIC_HOST=${pub_ip}:8080
EOF
  chmod 640 "$env_file"
  chown root:ollama-link "$env_file" 2>/dev/null || true

  # 写入并配置 systemd unit
  cat > "$service_file" <<EOF
[Unit]
Description=AI Remote Signaling and Frontend Service
Wants=network-online.target
After=network-online.target
StartLimitIntervalSec=0

[Service]
Type=simple
User=ollama-link
Group=ollama-link
WorkingDirectory=${INSTALL_ROOT}
EnvironmentFile=${env_file}
ExecStart=${BIN_DIR}/signaling-server
Restart=on-failure
RestartSec=3
TimeoutStopSec=15
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
LimitNOFILE=8192

[Install]
WantedBy=multi-user.target
EOF

  # 重写信令配置后，若 TURN 已部署则重新同步中继设置
  local turn_env="/etc/ollama-link/turn.env"
  if [ -f "$turn_env" ]; then
    sync_turn_to_signaling "$(read_env_value "$turn_env" PUBLIC_IP)" "$(read_env_value "$turn_env" TURN_USER)" "$(read_env_value "$turn_env" TURN_PASS)"
  fi
  start_service ollama-link-signaling

  sleep 1
  if systemctl is-active --quiet ollama-link-signaling; then
    local web_url="http://${pub_ip}:8080/#room=default&token=${token}"
    local setup_url="http://${pub_ip}:8080/setup#token=${token}"
    local ws_url="ws://${pub_ip}:8080/ws"

    printf "\n"
    printf "${GREEN}================================================================================${NC}\n"
    printf "  ${BOLD}🚀 AI Remote 信令服务部署成功并已启动运行！${NC}\n"
    printf "${GREEN}================================================================================${NC}\n"
    printf "  📡 服务状态:       ${GREEN}active (running)${NC}\n"
    printf "  🔌 WebSocket 接口: ${CYAN}%s${NC}\n" "$ws_url"
    printf "  🔑 默认房间:       ${BOLD}default${NC}\n"
    printf "  🛡️  访问 Token:     ${YELLOW}%s${NC}\n" "$token"
    printf '%s\n' "--------------------------------------------------------------------------------"
    printf "  🌐 ${BOLD}[公司电脑] 浏览器打开一键直达 (配置自动填充):${NC}\n"
    printf "     ${CYAN}%s${NC}\n\n" "$web_url"
    printf "  ⚙️  ${BOLD}[Web 控制中心] 实时监控与密钥管理:${NC}\n"
    printf "     ${CYAN}%s${NC}\n\n" "$setup_url"
    printf "  🏠 ${BOLD}[家里电脑] Agent 一键接入指令 (复制到家里电脑终端运行):${NC}\n"
    printf "     ${BOLD}ai-remote-agent %s %s${NC}\n" "$ws_url" "$token"
    printf "  🧱 ${BOLD}云服务器安全组需放行:${NC} 8080/TCP；部署 TURN 后还需 3478/UDP、3478/TCP、49160-49200/UDP\n"
    printf "${GREEN}================================================================================${NC}\n\n"
  else
    log_err "信令服务启动失败，查看日志排查问题："
    printf "  journalctl -u ollama-link-signaling -n 30 --no-pager\n"
    exit 1
  fi
}

# 部署家庭电脑 Agent 服务
deploy_agent() {
  local ws_url="${1:-${SIGNALING_URL:-}}"
  local token="${2:-${SIGNALING_TOKEN:-}}"
  local room="${ROOM_ID:-default}"

  log_info "开始配置并部署家里 Agent (Home Agent)..."
  check_root
  check_systemd
  find_install_paths
  ensure_service_user
  ensure_config_dir

  if [ -z "$ws_url" ]; then
    printf "${BOLD}请输入信令服务器 WebSocket 地址 (例如 ws://1.2.3.4:8080/ws): ${NC}"
    read -r ws_url
  fi
  if [ -z "$token" ]; then
    printf "${BOLD}请输入访问 Token: ${NC}"
    read -r token
  fi

  if [ -z "$ws_url" ] || [ -z "$token" ]; then
    log_err "信令地址和 Token 不能为空！"
    exit 1
  fi

  local env_file="/etc/ollama-link/home-agent.env"
  local service_file="/etc/systemd/system/ollama-link-home-agent.service"

  cat > "$env_file" <<EOF
# AI Remote 家庭 Agent 环境配置
SIGNALING_URL=${ws_url}
ROOM_ID=${room}
SIGNALING_TOKEN=${token}
OLLAMA_BASE=http://127.0.0.1:11434
ALLOWED_PATHS=/api/chat,/api/generate,/api/tags,/v1/models,/v1/chat/completions
REQUEST_TIMEOUT_SECS=600
FORCE_RELAY=false
EOF
  chmod 640 "$env_file"
  chown root:ollama-link "$env_file" 2>/dev/null || true

  cat > "$service_file" <<EOF
[Unit]
Description=AI Remote Home WebRTC Agent Service
Wants=network-online.target
After=network-online.target
StartLimitIntervalSec=0

[Service]
Type=simple
User=ollama-link
Group=ollama-link
WorkingDirectory=${INSTALL_ROOT}
EnvironmentFile=${env_file}
ExecStart=${BIN_DIR}/home-agent
Restart=on-failure
RestartSec=3
TimeoutStopSec=15
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
# AF_NETLINK: ICE 通过 getifaddrs 枚举本机网卡，缺少它会没有本机候选地址
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK
LimitNOFILE=8192

[Install]
WantedBy=multi-user.target
EOF

  start_service ollama-link-home-agent

  sleep 1
  if systemctl is-active --quiet ollama-link-home-agent; then
    printf "\n"
    printf "${GREEN}================================================================================${NC}\n"
    printf "  ${BOLD}🏠 AI Remote 家庭 Agent 已成功启动并常驻后台！${NC}\n"
    printf "${GREEN}================================================================================${NC}\n"
    printf "  📡 服务状态:       ${GREEN}active (running)${NC}\n"
    printf "  🔌 连接信令:       %s\n" "$ws_url"
    printf "  🔑 房间号:         %s\n" "$room"
    printf "  🦙 本地 Ollama:    http://127.0.0.1:11434\n"
    printf "  🧭 STUN/TURN:      由信令服务在连接时自动下发，无需在此配置\n"
    printf "  📋 查看实时日志:   journalctl -u ollama-link-home-agent -f\n"
    printf "${GREEN}================================================================================${NC}\n\n"
  else
    log_err "Agent 服务启动失败，查看日志排查："
    printf "  journalctl -u ollama-link-home-agent -n 30 --no-pager\n"
    exit 1
  fi
}

# 部署 TURN 服务
deploy_turn() {
  log_info "开始配置并部署 TURN 中继服务器 (TURN Server)..."
  check_root
  check_systemd
  find_install_paths
  ensure_service_user
  ensure_config_dir

  local pub_ip
  pub_ip=$(get_public_ip)
  local env_file="/etc/ollama-link/turn.env"
  local turn_pass="${TURN_PASS:-$(read_env_value "$env_file" TURN_PASS)}"
  if [ -z "$turn_pass" ]; then
    turn_pass=$(generate_token)
  fi
  local service_file="/etc/systemd/system/ollama-link-turn.service"

  cat > "$env_file" <<EOF
# AI Remote TURN 服务器环境配置
PUBLIC_IP=${pub_ip}
TURN_USER=ollama-link
TURN_PASS=${turn_pass}
REALM=webrtc-ollama
TURN_UDP_BIND=0.0.0.0:3478
TURN_TCP_BIND=0.0.0.0:3478
RELAY_BIND=0.0.0.0
RELAY_MIN_PORT=49160
RELAY_MAX_PORT=49200
TURN_IDLE_TIMEOUT_SECS=600
EOF
  chmod 640 "$env_file"
  chown root:ollama-link "$env_file" 2>/dev/null || true

  cat > "$service_file" <<EOF
[Unit]
Description=AI Remote TURN Relay Server
Wants=network-online.target
After=network-online.target
StartLimitIntervalSec=0

[Service]
Type=simple
User=ollama-link
Group=ollama-link
WorkingDirectory=${INSTALL_ROOT}
EnvironmentFile=${env_file}
ExecStart=${BIN_DIR}/turn-server
Restart=on-failure
RestartSec=3
TimeoutStopSec=15
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
LimitNOFILE=8192

[Install]
WantedBy=multi-user.target
EOF

  start_service ollama-link-turn

  sleep 1
  if systemctl is-active --quiet ollama-link-turn; then
    # 若信令服务已存在，同步 TURN 配置并重启，使浏览器与 Agent 连接时获取新配置
    if [ -f /etc/ollama-link/signaling.env ]; then
      sync_turn_to_signaling "$pub_ip" ollama-link "$turn_pass"
      systemctl restart ollama-link-signaling 2>/dev/null || true
    fi

    printf "\n"
    printf "${GREEN}================================================================================${NC}\n"
    printf "  ${BOLD}🔄 AI Remote TURN 中继服务已启动！${NC}\n"
    printf "${GREEN}================================================================================${NC}\n"
    printf "  📡 服务状态:       ${GREEN}active (running)${NC}\n"
    printf "  🌐 公网 IP:        %s\n" "$pub_ip"
    printf "  🔌 STUN/TURN 端口: 3478 (UDP/TCP)\n"
    printf "  👤 TURN 用户名:    ollama-link\n"
    printf "  🔑 TURN 密码:      %s\n" "$turn_pass"
    printf "  💡 已将中继配置同步给信令服务，浏览器与家里 Agent 连接时会自动获取。\n"
    printf "  🧱 ${BOLD}云服务器安全组需放行:${NC} 3478/UDP、3478/TCP、49160-49200/UDP\n"
    printf "${GREEN}================================================================================${NC}\n\n"
  else
    log_err "TURN 服务启动失败，查看日志："
    printf "  journalctl -u ollama-link-turn -n 30 --no-pager\n"
    exit 1
  fi
}

# 部署全部 (VPS 信令 + TURN)
deploy_all() {
  deploy_signaling
  deploy_turn
}

# 交互式菜单
interactive_menu() {
  clear 2>/dev/null || true
  printf "${CYAN}================================================================================${NC}\n"
  printf "  ${BOLD}🚀 AI Remote 一键生产环境部署向导 (systemd)${NC}\n"
  printf "${CYAN}================================================================================${NC}\n"
  printf "  请选择要部署的角色或服务：\n\n"
  printf "  ${BOLD}1)${NC} 部署信令服务器 (Signaling Server + Web 聊天界面 + 控制中心) ${GREEN}[公网 VPS 推荐]${NC}\n"
  printf "  ${BOLD}2)${NC} 部署家里 Agent (Home Agent 后台守护服务) ${YELLOW}[家里电脑]${NC}\n"
  printf "  ${BOLD}3)${NC} 部署 TURN 中继服务器 (TURN Server)\n"
  printf "  ${BOLD}4)${NC} 完整部署信令与 TURN 服务 (Signaling + TURN) ${GREEN}[VPS 全套]${NC}\n"
  printf "  ${BOLD}5)${NC} 退出\n"
  printf "${CYAN}--------------------------------------------------------------------------------${NC}\n"
  printf "请输入选项 [1-5]: "
  read -r choice
  case "$choice" in
    1) deploy_signaling ;;
    2) deploy_agent ;;
    3) deploy_turn ;;
    4) deploy_all ;;
    5) exit 0 ;;
    *) log_err "无效选项"; exit 1 ;;
  esac
}

# 主入口分发
main() {
  case "${1:-}" in
    signaling|server)
      deploy_signaling
      ;;
    agent|home)
      shift || true
      deploy_agent "${1:-}" "${2:-}"
      ;;
    turn)
      deploy_turn
      ;;
    all)
      deploy_all
      ;;
    help|--help|-h)
      printf "用法: sudo bash %s [signaling|agent|turn|all]\n" "$0"
      printf "  signaling : 部署信令服务器与 Web 前端\n"
      printf "  agent     : 部署家里 Agent 守护进程 (可选参数: [SIGNALING_URL] [TOKEN])\n"
      printf "  turn      : 部署 TURN 中继服务\n"
      printf "  all       : 同时部署信令服务与 TURN 服务\n"
      ;;
    *)
      if [ -t 0 ]; then
        interactive_menu
      else
        deploy_signaling
      fi
      ;;
  esac
}

main "$@"
