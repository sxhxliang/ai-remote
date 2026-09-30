#!/usr/bin/env bash
# ==============================================================================
# AI Remote VPS 生产环境一键管理脚本 (vps.sh)
# 支持：一键安装、平滑升级、彻底卸载、配置管理 (含多 Token)、服务运维监控
# ==============================================================================
set -euo pipefail

REPO="sxhxliang/ai-remote"
INSTALL_DIR="/opt/ollama-link"
CONFIG_DIR="/etc/ollama-link"
SERVICE_USER="ollama-link"
BIN_DIR="${INSTALL_DIR}/bin"
FRONTEND_DIR="${INSTALL_DIR}/frontend"

SIG_SERVICE="ollama-link-signaling"
TURN_SERVICE="ollama-link-turn"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

log_info() { printf "${GREEN}[INFO]${NC} %s\n" "$*"; }
log_warn() { printf "${YELLOW}[WARN]${NC} %s\n" "$*"; }
log_err()  { printf "${RED}[ERROR]${NC} %s\n" "$*" >&2; }

check_root() {
  if [ "$(id -u)" -ne 0 ]; then
    log_err "本脚本需要 root 权限，请使用 sudo 执行："
    printf "  sudo bash %s %s\n" "$0" "${*:-}"
    exit 1
  fi
}

check_systemd() {
  if ! command -v systemctl >/dev/null 2>&1 || ! [ -d /run/systemd/system ]; then
    log_err "当前系统未检测到正在运行的 systemd，无法作为系统服务管理。"
    exit 1
  fi
}

check_arch() {
  local machine
  machine=$(uname -m)
  case "$machine" in
    x86_64|amd64) ARCH="x86_64" ;;
    aarch64|arm64) ARCH="aarch64" ;;
    *) log_err "不支持的 CPU 架构: $machine"; exit 1 ;;
  esac

  if ! command -v getconf >/dev/null 2>&1; then
    log_err "缺少 getconf 工具，无法检测 glibc 版本。"
    exit 1
  fi
  local libc libc_version libc_major libc_minor
  libc=$(getconf GNU_LIBC_VERSION 2>/dev/null || true)
  if [ -z "$libc" ]; then
    log_err "非 glibc 系统（如 Alpine/musl）不受支持。请使用 Ubuntu 22.04+ 或 Debian 12+。"
    exit 1
  fi
  libc_version=${libc##* }
  libc_major=${libc_version%%.*}
  libc_minor=${libc_version#*.}
  libc_minor=${libc_minor%%.*}
  if [ "$libc_major" -lt 2 ] || { [ "$libc_major" -eq 2 ] && [ "$libc_minor" -lt 35 ]; }; then
    log_err "glibc 版本低于 2.35 (当前: $libc_version)。推荐 Ubuntu 22.04+ 或 Debian 12+。"
    exit 1
  fi
}

ensure_dependencies() {
  local needed=()
  for cmd in curl tar openssl; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
      needed+=("$cmd")
    fi
  done
  if [ ${#needed[@]} -gt 0 ]; then
    log_info "正在安装必要系统依赖: ${needed[*]} ..."
    if command -v apt-get >/dev/null 2>&1; then
      apt-get update -qq && apt-get install -y -qq "${needed[@]}"
    elif command -v dnf >/dev/null 2>&1; then
      dnf install -y -q "${needed[@]}"
    elif command -v yum >/dev/null 2>&1; then
      yum install -y -q "${needed[@]}"
    else
      log_warn "未能自动安装依赖，请手动确保安装了: ${needed[*]}"
    fi
  fi
}

get_public_ip() {
  local ip=""
  for url in "https://api.ipify.org" "https://icanhazip.com" "https://ifconfig.me/ip"; do
    ip=$(curl -s --max-time 3 "$url" 2>/dev/null | tr -d ' \r\n') || true
    if [[ "$ip" =~ ^[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}$ ]]; then
      echo "$ip"
      return 0
    fi
  done
  echo "127.0.0.1"
}

generate_token() {
  if command -v openssl >/dev/null 2>&1; then
    openssl rand -hex 16
  else
    head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n'
  fi
}

ensure_user_and_dirs() {
  if ! id -u "$SERVICE_USER" >/dev/null 2>&1; then
    useradd -r -s /usr/sbin/nologin -d "$INSTALL_DIR" -M "$SERVICE_USER" 2>/dev/null || \
    useradd -r -s /sbin/nologin -d "$INSTALL_DIR" -M "$SERVICE_USER" 2>/dev/null || true
  fi
  mkdir -p "$BIN_DIR" "$FRONTEND_DIR" "$CONFIG_DIR"
  chown -R "$SERVICE_USER":"$SERVICE_USER" "$INSTALL_DIR" 2>/dev/null || true
  chmod 750 "$INSTALL_DIR" 2>/dev/null || true
  chmod 755 "$CONFIG_DIR" 2>/dev/null || true
}

get_latest_release_version() {
  local tag
  tag=$(curl -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/${REPO}/releases/latest" 2>/dev/null | awk -F'/' '{print $NF}') || true
  if [ -z "$tag" ]; then
    tag=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null | grep '"tag_name":' | head -n1 | cut -d'"' -f4) || true
  fi
  echo "${tag:-}"
}

download_and_extract_release() {
  local version="$1"
  check_arch
  local target="${ARCH}-unknown-linux-gnu"
  local tarball="ai-remote-${version}-${target}.tar.gz"
  local url="https://github.com/${REPO}/releases/download/${version}/${tarball}"

  local tmp_dir
  tmp_dir=$(mktemp -d)

  log_info "正在下载 AI Remote 发布包 (${version}, ${ARCH}) ..."
  if ! curl -fSL --progress-bar "$url" -o "${tmp_dir}/${tarball}"; then
    rm -rf "$tmp_dir"
    log_err "下载发布包失败: $url"
    exit 1
  fi

  log_info "正在解压并更新程序文件到 ${INSTALL_DIR} ..."
  tar -xzf "${tmp_dir}/${tarball}" -C "$tmp_dir"

  local src_dir
  src_dir=$(find "$tmp_dir" -maxdepth 2 -type f \( -name "ai-remote-signaling" -o -name "signaling-server" \) -exec dirname {} + | head -n1)

  if [ -z "$src_dir" ]; then
    rm -rf "$tmp_dir"
    log_err "在发布包中未能找到可执行程序。"
    exit 1
  fi

  mkdir -p "$BIN_DIR" "$FRONTEND_DIR"

  for bin in signaling-server turn-server home-agent ai-remote-signaling ai-remote-turn ai-remote-agent; do
    if [ -f "${src_dir}/${bin}" ]; then
      cp -f "${src_dir}/${bin}" "${BIN_DIR}/"
      chmod 755 "${BIN_DIR}/${bin}"
    fi
  done

  [ -f "${BIN_DIR}/ai-remote-signaling" ] && ln -sf "ai-remote-signaling" "${BIN_DIR}/signaling-server" 2>/dev/null || true
  [ -f "${BIN_DIR}/ai-remote-turn" ] && ln -sf "ai-remote-turn" "${BIN_DIR}/turn-server" 2>/dev/null || true
  [ -f "${BIN_DIR}/ai-remote-agent" ] && ln -sf "ai-remote-agent" "${BIN_DIR}/home-agent" 2>/dev/null || true

  local fe_src=""
  for cand in "${src_dir}/frontend" "${src_dir}/web" "${src_dir}/dist"; do
    if [ -d "$cand" ]; then fe_src="$cand"; break; fi
  done
  if [ -n "$fe_src" ]; then
    rm -rf "${FRONTEND_DIR:?}"/*
    cp -r "$fe_src"/* "$FRONTEND_DIR/"
  fi

  for cmd in ai-remote-signaling ai-remote-turn ai-remote-agent; do
    if [ -f "${BIN_DIR}/${cmd}" ]; then
      ln -sf "${BIN_DIR}/${cmd}" "/usr/local/bin/${cmd}" 2>/dev/null || true
    fi
  done
  ln -sf "${BIN_DIR}/signaling-server" "/usr/local/bin/signaling-server" 2>/dev/null || true
  ln -sf "${BIN_DIR}/turn-server" "/usr/local/bin/turn-server" 2>/dev/null || true

  chown -R "$SERVICE_USER":"$SERVICE_USER" "$INSTALL_DIR" 2>/dev/null || true
  echo "$version" > "${INSTALL_DIR}/version.txt"
  rm -rf "$tmp_dir"
  log_info "程序文件安装/更新完成 (版本: ${version})。"
}

install_systemd_services() {
  printf "[Unit]\nDescription=AI Remote Signaling and Web Frontend\nWants=network-online.target\nAfter=network-online.target\nStartLimitIntervalSec=0\n\n[Service]\nType=simple\nUser=%s\nGroup=%s\nWorkingDirectory=%s\nEnvironmentFile=%s/signaling.env\nExecStart=%s/signaling-server\nRestart=on-failure\nRestartSec=3\nTimeoutStopSec=15\nNoNewPrivileges=true\nPrivateTmp=true\nProtectSystem=strict\nProtectHome=true\nReadWritePaths=%s\nLimitNOFILE=8192\n\n[Install]\nWantedBy=multi-user.target\n" \
    "$SERVICE_USER" "$SERVICE_USER" "$INSTALL_DIR" "$CONFIG_DIR" "$BIN_DIR" "$INSTALL_DIR" > "/etc/systemd/system/${SIG_SERVICE}.service"

  printf "[Unit]\nDescription=AI Remote TURN Relay Server\nWants=network-online.target\nAfter=network-online.target %s.service\nStartLimitIntervalSec=0\n\n[Service]\nType=simple\nUser=%s\nGroup=%s\nWorkingDirectory=%s\nEnvironmentFile=%s/turn.env\nExecStart=%s/turn-server\nRestart=on-failure\nRestartSec=3\nTimeoutStopSec=15\nAmbientCapabilities=CAP_NET_BIND_SERVICE\nCapabilityBoundingSet=CAP_NET_BIND_SERVICE\nNoNewPrivileges=true\nPrivateTmp=true\nProtectSystem=strict\nProtectHome=true\nLimitNOFILE=8192\n\n[Install]\nWantedBy=multi-user.target\n" \
    "$SIG_SERVICE" "$SERVICE_USER" "$SERVICE_USER" "$INSTALL_DIR" "$CONFIG_DIR" "$BIN_DIR" > "/etc/systemd/system/${TURN_SERVICE}.service"

  systemctl daemon-reload
}

init_signaling_config() {
  local env_file="${CONFIG_DIR}/signaling.env"
  if [ -f "$env_file" ]; then
    log_info "检测到已有信令配置文件: ${env_file}，保留现有配置。"
    return 0
  fi

  local pub_ip
  pub_ip=$(get_public_ip)
  local token
  token=$(generate_token)

  printf "# 信令服务器监听地址与端口\nSIGNALING_BIND=0.0.0.0:8080\n\n# 全局默认 Token\nSIGNALING_TOKEN=%s\n\n# 静态前端资源目录\nFRONTEND_DIR=%s\n\n# 房间专属 Token 字典\nROOM_TOKENS_JSON=\n\n# STUN 与 TURN 配置\nSTUN_URL=stun:%s:3478\nTURN_URL=turn:%s:3478?transport=udp,turn:%s:3478?transport=tcp\nTURN_USER=ollama-link\nTURN_PASS=\n" \
    "$token" "$FRONTEND_DIR" "$pub_ip" "$pub_ip" "$pub_ip" > "$env_file"

  chmod 640 "$env_file"
  chown root:"$SERVICE_USER" "$env_file" 2>/dev/null || true
  log_info "已初始化信令配置文件: ${env_file}"
}

init_turn_config() {
  local env_file="${CONFIG_DIR}/turn.env"
  if [ -f "$env_file" ]; then
    log_info "检测到已有 TURN 配置文件: ${env_file}，保留现有配置。"
    return 0
  fi

  local pub_ip
  pub_ip=$(get_public_ip)
  local turn_pass
  turn_pass=$(generate_token)

  printf "TURN_BIND=0.0.0.0:3478\nPUBLIC_IP=%s\nTURN_REALM=chat.example.com\nTURN_USER=ollama-link\nTURN_PASS=%s\nMIN_PORT=49160\nMAX_PORT=49200\n" \
    "$pub_ip" "$turn_pass" > "$env_file"

  chmod 640 "$env_file"
  chown root:"$SERVICE_USER" "$env_file" 2>/dev/null || true

  local sig_env="${CONFIG_DIR}/signaling.env"
  if [ -f "$sig_env" ]; then
    sed -i "s/^TURN_PASS=.*/TURN_PASS=${turn_pass}/" "$sig_env" 2>/dev/null || true
  fi
  log_info "已初始化 TURN 配置文件并同步凭据到信令服务。"
}

do_install() {
  local mode="${1:-all}"
  log_info "开始在 VPS 部署 AI Remote (模式: ${mode}) ..."
  check_root
  check_systemd
  check_arch
  ensure_dependencies
  ensure_user_and_dirs

  local version
  version=$(get_latest_release_version)
  if [ -z "$version" ]; then
    log_warn "未能在线获取最新 Release 标签，回退使用 v0.2.3..."
    version="v0.2.3"
  fi

  download_and_extract_release "$version"
  install_systemd_services

  if [ "$mode" = "all" ] || [ "$mode" = "signaling" ]; then
    init_signaling_config
    systemctl enable "${SIG_SERVICE}" >/dev/null 2>&1 || true
    systemctl restart "${SIG_SERVICE}" || true
  fi

  if [ "$mode" = "all" ] || [ "$mode" = "turn" ]; then
    init_turn_config
    systemctl enable "${TURN_SERVICE}" >/dev/null 2>&1 || true
    systemctl restart "${TURN_SERVICE}" || true
  fi

  log_info "部署完成！"
  show_status
}

do_upgrade() {
  log_info "正在检查并平滑升级 AI Remote ..."
  check_root
  check_systemd
  check_arch
  ensure_dependencies

  local cur_version="未知"
  [ -f "${INSTALL_DIR}/version.txt" ] && cur_version=$(cat "${INSTALL_DIR}/version.txt")

  local latest_version
  latest_version=$(get_latest_release_version)
  if [ -z "$latest_version" ]; then
    log_err "获取最新版本信息失败，请检查网络。"
    exit 1
  fi

  log_info "当前运行版本: ${cur_version}，最新可用版本: ${latest_version}"
  if [ "$cur_version" = "$latest_version" ]; then
    printf "当前已是最新版 (%s)，是否仍要强制重新更新？[y/N]: " "$latest_version"
    read -r ans
    if [[ ! "$ans" =~ ^[Yy]$ ]]; then
      log_info "已取消更新。"
      return 0
    fi
  fi

  local backup_dir="/tmp/ai-remote-backup-$(date +%s)"
  mkdir -p "$backup_dir"
  cp -r "$CONFIG_DIR" "$backup_dir/" 2>/dev/null || true
  log_info "配置已安全备份至: $backup_dir"

  download_and_extract_release "$latest_version"

  log_info "正在重载 systemd 并重启各运行服务 ..."
  systemctl daemon-reload
  if systemctl is-active --quiet "$SIG_SERVICE" 2>/dev/null; then
    systemctl restart "$SIG_SERVICE"
    log_info "信令服务已重启生效。"
  fi
  if systemctl is-active --quiet "$TURN_SERVICE" 2>/dev/null; then
    systemctl restart "$TURN_SERVICE"
    log_info "TURN 服务已重启生效。"
  fi

  log_info "恭喜，平滑升级完成！当前版本: ${latest_version}"
}

do_uninstall() {
  check_root
  check_systemd
  printf "${RED}${BOLD}警告：确定要卸载 AI Remote 吗？${NC}\n"
  printf "此操作将停止服务并移除 systemd 单元文件。\n"
  printf "是否同时删除配置文件与数据目录 (%s, %s)? [y/N]: " "$CONFIG_DIR" "$INSTALL_DIR"
  read -r purge_ans

  log_info "正在停止并禁用系统服务 ..."
  systemctl stop "$SIG_SERVICE" "$TURN_SERVICE" 2>/dev/null || true
  systemctl disable "$SIG_SERVICE" "$TURN_SERVICE" 2>/dev/null || true

  rm -f "/etc/systemd/system/${SIG_SERVICE}.service" "/etc/systemd/system/${TURN_SERVICE}.service"
  rm -f "/usr/local/bin/ai-remote-signaling" "/usr/local/bin/ai-remote-turn" "/usr/local/bin/ai-remote-agent" \
        "/usr/local/bin/signaling-server" "/usr/local/bin/turn-server"
  systemctl daemon-reload

  rm -rf "$INSTALL_DIR"

  if [[ "$purge_ans" =~ ^[Yy]$ ]]; then
    rm -rf "$CONFIG_DIR"
    log_info "已完全清除所有程序文件及配置目录。"
  else
    log_info "程序文件已移除，配置文件保留在: ${CONFIG_DIR}"
  fi

  log_info "AI Remote 服务已成功卸载。"
}

show_status() {
  printf "\n${CYAN}================================================================================${NC}\n"
  printf "                   ${BOLD}AI Remote VPS 服务运行状态${NC}\n"
  printf "${CYAN}================================================================================${NC}\n"

  local sig_status turn_status
  sig_status=$(systemctl is-active "$SIG_SERVICE" 2>/dev/null || echo "not-installed")
  turn_status=$(systemctl is-active "$TURN_SERVICE" 2>/dev/null || echo "not-installed")

  if [ "$sig_status" = "active" ]; then
    printf "  信令服务器 (Signaling): ${GREEN}● 运行中 (active)${NC}\n"
  else
    printf "  信令服务器 (Signaling): ${RED}○ 未运行 (${sig_status})${NC}\n"
  fi

  if [ "$turn_status" = "active" ]; then
    printf "  TURN 中继服务 (TURN):   ${GREEN}● 运行中 (active)${NC}\n"
  else
    printf "  TURN 中继服务 (TURN):   ${YELLOW}○ 未运行 (${turn_status})${NC}\n"
  fi

  local version="未知"
  [ -f "${INSTALL_DIR}/version.txt" ] && version=$(cat "${INSTALL_DIR}/version.txt")
  printf "  程序版本:               %s\n" "$version"

  local sig_env="${CONFIG_DIR}/signaling.env"
  if [ -f "$sig_env" ]; then
    local bind token room_tokens
    bind=$(grep -E "^SIGNALING_BIND=" "$sig_env" | cut -d= -f2- || echo "0.0.0.0:8080")
    token=$(grep -E "^SIGNALING_TOKEN=" "$sig_env" | cut -d= -f2- || echo "")
    room_tokens=$(grep -E "^ROOM_TOKENS_JSON=" "$sig_env" | cut -d= -f2- || echo "")

    local pub_ip
    pub_ip=$(get_public_ip)
    local port="${bind##*:}"

    printf "\n  Web 访问链接:        ${BOLD}http://%s:%s${NC}\n" "$pub_ip" "$port"
    if [ -n "$room_tokens" ] && [ "$room_tokens" != "{}" ]; then
      printf "  鉴权模式:            ${CYAN}多 Token 房间隔离模式 (ROOM_TOKENS_JSON 已启用)${NC}\n"
      printf "  房间列表:            %s\n" "$room_tokens"
    else
      printf "  全局默认 Token:      ${BOLD}%s${NC}\n" "$token"
      printf "  浏览器直连地址:      ${GREEN}http://%s:%s/#/?token=%s${NC}\n" "$pub_ip" "$port" "$token"
    fi
  fi

  printf "${CYAN}--------------------------------------------------------------------------------${NC}\n"
  printf "  云服务器安全组/防火墙建议放行端口：\n"
  printf "    - TCP 8080:  Web 前端与 WebSocket 信令端口 (若使用域名反代则开放 80/443)\n"
  printf "    - UDP 3478:  STUN/TURN UDP 中继通道\n"
  printf "    - TCP 3478:  STUN/TURN TCP 中继通道\n"
  printf "    - UDP 49160-49200: TURN 数据转发中继动态端口段\n"
  printf "${CYAN}================================================================================${NC}\n\n"
}

manage_room_tokens() {
  local sig_env="${CONFIG_DIR}/signaling.env"
  if [ ! -f "$sig_env" ]; then
    log_err "信令配置文件不存在: $sig_env"
    return 1
  fi

  while true; do
    local current_json_raw
    current_json_raw=$(grep -E "^ROOM_TOKENS_JSON=" "$sig_env" | cut -d= -f2- || true)

    printf "\n${CYAN}================================================================================${NC}\n"
    printf "                    ${BOLD}多 Token 房间密钥管理 (ROOM_TOKENS_JSON)${NC}\n"
    printf "${CYAN}================================================================================${NC}\n"
    printf "当前 ROOM_TOKENS_JSON:\n  %s\n\n" "${current_json_raw:-<未设置 (使用全局 Token)>}"
    printf "  1) 添加或修改房间 Token\n"
    printf "  2) 删除指定房间 Token\n"
    printf "  3) 直接输入/粘贴完整 JSON 字符串\n"
    printf "  4) 清空多 Token (恢复使用全局默认 Token)\n"
    printf "  5) 保存并重启信令服务生效\n"
    printf "  0) 返回上一层菜单\n"
    printf "${CYAN}--------------------------------------------------------------------------------${NC}\n"
    printf "请选择 [0-5]: "
    read -r opt
    case "$opt" in
      1)
        printf "请输入房间 ID (仅限字母数字下划线中划线，如 living-room): "
        read -r r_id
        if [[ ! "$r_id" =~ ^[a-zA-Z0-9_-]{1,64}$ ]]; then
          log_err "房间 ID 格式不合规！"
          continue
        fi
        printf "请输入对应 Token (至少 16 字符，直接回车自动生成 32 位安全随机串): "
        read -r r_tk
        [ -z "$r_tk" ] && r_tk=$(generate_token)
        if [ ${#r_tk} -lt 16 ]; then
          log_err "Token 长度必须 >= 16 字符！"
          continue
        fi

        python3 -c "
import json
raw = '''${current_json_raw}'''.strip()
if raw.startswith("'") and raw.endswith("'"): raw = raw[1:-1]
data = json.loads(raw) if raw else {}
data['$r_id'] = '$r_tk'
with open('${sig_env}.tmp', 'w') as f:
    f.write(json.dumps(data))
" 2>/dev/null || true

        if [ -f "${sig_env}.tmp" ]; then
          local new_data
          new_data=$(cat "${sig_env}.tmp")
          rm -f "${sig_env}.tmp"
          grep -v "^ROOM_TOKENS_JSON=" "$sig_env" > "${sig_env}.tmp" || true
          echo "ROOM_TOKENS_JSON='${new_data}'" >> "${sig_env}.tmp"
          mv "${sig_env}.tmp" "$sig_env"
          log_info "已成功添加/更新房间 [$r_id]！"
        else
          log_err "更新 JSON 失败，请检查环境中的 python3。"
        fi
        ;;
      2)
        printf "请输入要删除的房间 ID: "
        read -r r_id
        python3 -c "
import json
raw = '''${current_json_raw}'''.strip()
if raw.startswith("'") and raw.endswith("'"): raw = raw[1:-1]
data = json.loads(raw) if raw else {}
data.pop('$r_id', None)
with open('${sig_env}.tmp', 'w') as f:
    f.write(json.dumps(data))
" 2>/dev/null || true
        if [ -f "${sig_env}.tmp" ]; then
          local new_data
          new_data=$(cat "${sig_env}.tmp")
          rm -f "${sig_env}.tmp"
          grep -v "^ROOM_TOKENS_JSON=" "$sig_env" > "${sig_env}.tmp" || true
          echo "ROOM_TOKENS_JSON='${new_data}'" >> "${sig_env}.tmp"
          mv "${sig_env}.tmp" "$sig_env"
          log_info "已删除房间 [$r_id]。"
        fi
        ;;
      3)
        printf "请输入完整的 JSON (例如 {\"living-room\":\"token_16_characters_min\"}):\n> "
        read -r raw_input
        if python3 -c "import json; data=json.loads('$raw_input'); assert all(len(v)>=16 for v in data.values())" 2>/dev/null; then
          grep -v "^ROOM_TOKENS_JSON=" "$sig_env" > "${sig_env}.tmp" || true
          echo "ROOM_TOKENS_JSON='${raw_input}'" >> "${sig_env}.tmp"
          mv "${sig_env}.tmp" "$sig_env"
          log_info "ROOM_TOKENS_JSON 已直接更新。"
        else
          log_err "输入的 JSON 格式非法或包含少于 16 字符的 Token，未保存。"
        fi
        ;;
      4)
        grep -v "^ROOM_TOKENS_JSON=" "$sig_env" > "${sig_env}.tmp" || true
        echo "ROOM_TOKENS_JSON=" >> "${sig_env}.tmp"
        mv "${sig_env}.tmp" "$sig_env"
        log_info "已清空 ROOM_TOKENS_JSON，服务恢复使用全局默认 Token。"
        ;;
      5)
        systemctl restart "$SIG_SERVICE" 2>/dev/null || true
        log_info "信令服务已重启生效！"
        show_status
        return 0
        ;;
      0)
        return 0
        ;;
      *)
        log_err "无效选项"
        ;;
    esac
  done
}

pause() {
  printf "\n按 Enter 键继续..."
  read -r _
}

interactive_menu() {
  while true; do
    clear 2>/dev/null || true
    printf "${CYAN}================================================================================${NC}\n"
    printf "            ${BOLD}AI Remote VPS 生产环境一键管理工具 (vps.sh)${NC}\n"
    printf "${CYAN}================================================================================${NC}\n"
    printf "  ${BOLD}1)${NC} 一键安装部署完整服务 (信令服务 + TURN 中继 + 前端) ${GREEN}[推荐]${NC}\n"
    printf "  ${BOLD}2)${NC} 仅安装信令服务器与 Web 前端\n"
    printf "  ${BOLD}3)${NC} 仅安装 TURN 中继服务器\n"
    printf "  ${BOLD}4)${NC} 一键平滑升级 (更新二进制与前端静态文件，保留配置) ${YELLOW}[更新]${NC}\n"
    printf "  ${BOLD}5)${NC} 查看服务运行状态与访问直链\n"
    printf "  ${BOLD}6)${NC} 多 Token 房间密钥配置管理 (ROOM_TOKENS_JSON)\n"
    printf "  ${BOLD}7)${NC} 修改监听端口与全局默认 Token\n"
    printf "  ${BOLD}8)${NC} 服务控制 (启动 / 停止 / 重启 / 查看实时日志)\n"
    printf "  ${BOLD}9)${NC} 彻底卸载服务与清理程序文件 ${RED}[卸载]${NC}\n"
    printf "  ${BOLD}0)${NC} 退出\n"
    printf "${CYAN}--------------------------------------------------------------------------------${NC}\n"
    printf "请输入选项 [0-9]: "
    read -r choice
    case "$choice" in
      1) do_install "all"; pause ;;
      2) do_install "signaling"; pause ;;
      3) do_install "turn"; pause ;;
      4) do_upgrade; pause ;;
      5) show_status; pause ;;
      6) manage_room_tokens ;;
      7)
        local sig_env="${CONFIG_DIR}/signaling.env"
        if [ ! -f "$sig_env" ]; then
          log_err "未找到信令配置文件: $sig_env，请先安装服务。"
          pause
          continue
        fi
        printf "当前配置: %s\n" "$(grep -E "^SIGNALING_BIND=" "$sig_env" || true)"
        printf "请输入新的监听端口 (如 8080，直接回车不修改): "
        read -r n_port
        if [ -n "$n_port" ]; then
          sed -i "s/^SIGNALING_BIND=.*/SIGNALING_BIND=0.0.0.0:${n_port}/" "$sig_env"
        fi
        printf "请输入新的全局 Token (至少16字符，输入 auto 自动生成，直接回车不修改): "
        read -r n_token
        if [ "$n_token" = "auto" ]; then n_token=$(generate_token); fi
        if [ -n "$n_token" ] && [ ${#n_token} -ge 16 ]; then
          sed -i "s/^SIGNALING_TOKEN=.*/SIGNALING_TOKEN=${n_token}/" "$sig_env"
        fi
        systemctl restart "$SIG_SERVICE" 2>/dev/null || true
        log_info "配置已更新并重启服务！"
        pause
        ;;
      8)
        printf "请选择操作: 1) 重启服务  2) 停止服务  3) 启动服务  4) 查看信令日志  5) 查看TURN日志\n> "
        read -r s_opt
        case "$s_opt" in
          1) systemctl restart "$SIG_SERVICE" "$TURN_SERVICE" 2>/dev/null || true; log_info "已重启";;
          2) systemctl stop "$SIG_SERVICE" "$TURN_SERVICE" 2>/dev/null || true; log_info "已停止";;
          3) systemctl start "$SIG_SERVICE" "$TURN_SERVICE" 2>/dev/null || true; log_info "已启动";;
          4) journalctl -u "$SIG_SERVICE" -n 50 -f; break ;;
          5) journalctl -u "$TURN_SERVICE" -n 50 -f; break ;;
        esac
        pause
        ;;
      9) do_uninstall; pause ;;
      0) exit 0 ;;
      *) log_err "无效输入"; pause ;;
    esac
  done
}

main() {
  case "${1:-}" in
    install)
      shift || true
      do_install "${1:-all}"
      ;;
    upgrade|update)
      do_upgrade
      ;;
    uninstall|remove)
      do_uninstall
      ;;
    status)
      show_status
      ;;
    start)
      check_root
      systemctl start "$SIG_SERVICE" "$TURN_SERVICE" 2>/dev/null || true
      show_status
      ;;
    stop)
      check_root
      systemctl stop "$SIG_SERVICE" "$TURN_SERVICE" 2>/dev/null || true
      show_status
      ;;
    restart)
      check_root
      systemctl restart "$SIG_SERVICE" "$TURN_SERVICE" 2>/dev/null || true
      show_status
      ;;
    logs)
      shift || true
      local target="${1:-$SIG_SERVICE}"
      [ "$target" = "turn" ] && target="$TURN_SERVICE"
      [ "$target" = "signaling" ] && target="$SIG_SERVICE"
      journalctl -u "$target" -n 50 -f
      ;;
    config)
      manage_room_tokens
      ;;
    help|--help|-h)
      printf "AI Remote VPS 一键管理脚本使用方法:\n"
      printf "  交互菜单: sudo bash %s\n" "$0"
      printf "  命令行模式:\n"
      printf "    sudo bash %s install [all|signaling|turn]  # 一键安装\n" "$0"
      printf "    sudo bash %s upgrade                       # 平滑升级\n" "$0"
      printf "    sudo bash %s uninstall                     # 彻底卸载\n" "$0"
      printf "    sudo bash %s status                        # 查看服务状态\n" "$0"
      printf "    sudo bash %s restart                       # 重启服务\n" "$0"
      printf "    sudo bash %s logs [signaling|turn]         # 查看日志\n" "$0"
      printf "    sudo bash %s config                        # 多 Token 房间配置管理\n" "$0"
      ;;
    *)
      if [ -t 0 ]; then
        interactive_menu
      else
        show_status
      fi
      ;;
  esac
}

main "$@"
