#!/bin/sh
# Install a published release without Rust or Node.js. Run with: curl -fsSL URL | sh
set -eu

fail() { printf 'ai-remote: %s\n' "$*" >&2; exit 1; }

download() {
  curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
    --connect-timeout 15 --retry 3 --output "$2" "$1"
}

checksum() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{print $1}'
  else fail 'Install sha256sum or shasum before continuing.'
  fi
}

cleanup() {
  if [ -n "${download_dir:-}" ]; then
    case "$download_dir" in "$temporary_root"/ai-remote-install.*) rm -rf -- "$download_dir" ;; esac
  fi
  if [ -n "${stage_dir:-}" ]; then
    case "$stage_dir" in "$install_root"/.install.*) rm -rf -- "$stage_dir" ;; esac
  fi
}

main() {
  command -v curl >/dev/null 2>&1 || fail 'curl is required.'
  command -v tar >/dev/null 2>&1 || fail 'tar is required.'
  repo=${AI_REMOTE_REPO:-sxhxliang/ai-remote}
  printf '%s\n' "$repo" | grep -Eq '^[A-Za-z0-9][A-Za-z0-9_.-]*/[A-Za-z0-9][A-Za-z0-9_.-]*$' || fail 'Invalid AI_REMOTE_REPO; expected owner/repo.'

  machine=$(uname -m)
  operating_system=$(uname -s)
  # Detect Apple Silicon even when the invoking shell runs under Rosetta.
  if [ "$operating_system" = Darwin ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = 1 ]; then machine=arm64; fi
  case "$machine" in x86_64|amd64) architecture=x86_64 ;; aarch64|arm64) architecture=aarch64 ;; *) fail "Unsupported architecture: $machine" ;; esac
  case "$operating_system" in
    Linux)
      command -v getconf >/dev/null 2>&1 || fail 'The Linux release requires glibc 2.35 or later.'
      libc=$(getconf GNU_LIBC_VERSION 2>/dev/null) || fail 'musl/Alpine is not supported by this glibc release.'
      libc_version=${libc##* }
      libc_major=${libc_version%%.*}
      libc_minor=${libc_version#*.}; libc_minor=${libc_minor%%.*}
      if [ "$libc_major" -lt 2 ] || { [ "$libc_major" -eq 2 ] && [ "$libc_minor" -lt 35 ]; }; then
        fail 'The Linux release requires glibc 2.35+ (for example Ubuntu 22.04+ or Debian 12+).'
      fi
      target=$architecture-unknown-linux-gnu ;;
    Darwin) target=$architecture-apple-darwin ;;
    *) fail "Unsupported OS: $operating_system. Use install.ps1 on Windows." ;;
  esac

  version=${AI_REMOTE_VERSION:-latest}
  if [ "$version" = latest ]; then
    release_url=$(curl --fail --silent --show-error --location --head --proto '=https' --tlsv1.2 \
      --connect-timeout 15 --retry 3 --output /dev/null --write-out '%{url_effective}' "https://github.com/$repo/releases/latest") \
      || fail 'No published release was found. Publish a v* tag before installing.'
    version=${release_url##*/}
  fi
  printf '%s\n' "$version" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$' || fail 'Invalid AI_REMOTE_VERSION; use a tag such as v0.2.1.'
  asset=ai-remote-$target.tar.gz
  base_url=https://github.com/$repo/releases/download/$version
  temporary_root=$(cd "${TMPDIR:-/tmp}" && pwd -P)
  download_dir=$(mktemp -d "$temporary_root/ai-remote-install.XXXXXX")
  stage_dir=
  trap cleanup 0
  trap 'exit 130' 2
  trap 'exit 143' 15
  printf 'Downloading AI Remote %s (%s)...\n' "$version" "$target"
  download "$base_url/$asset" "$download_dir/$asset" || fail 'Release download failed.'
  download "$base_url/$asset.sha256" "$download_dir/checksum" || fail 'Checksum download failed.'
  expected_hash= expected_name=
  read -r expected_hash expected_name < "$download_dir/checksum" || true
  [ "${#expected_hash}" -eq 64 ] && [ "$expected_name" = "$asset" ] || fail 'Invalid checksum file.'
  case "$expected_hash" in *[!a-f0-9]*) fail 'Invalid SHA-256 digest.' ;; esac
  [ "$(checksum "$download_dir/$asset")" = "$expected_hash" ] || fail 'SHA-256 verification failed; nothing was installed.'

  tar -tzf "$download_dir/$asset" > "$download_dir/entries" || fail 'Invalid release archive.'
  while IFS= read -r entry; do
    case "$entry" in ai-remote|ai-remote/*) ;; *) fail 'Archive contains an unexpected path.' ;; esac
    case "/$entry/" in */../*) fail 'Archive contains a parent-directory path.' ;; esac
  done < "$download_dir/entries"
  tar -tvzf "$download_dir/$asset" > "$download_dir/types"
  awk '{ kind = substr($1, 1, 1); if (kind != "-" && kind != "d") exit 1 }' "$download_dir/types" || fail 'Archive links and special files are not allowed.'

  install_root=${AI_REMOTE_INSTALL_DIR:-${HOME:?HOME is required}/.local/share/ai-remote}
  bin_dir=${AI_REMOTE_BIN_DIR:-${HOME:?HOME is required}/.local/bin}
  case "$install_root" in /*) ;; *) fail 'AI_REMOTE_INSTALL_DIR must be an absolute path.' ;; esac
  case "$bin_dir" in /*) ;; *) fail 'AI_REMOTE_BIN_DIR must be an absolute path.' ;; esac
  mkdir -p "$install_root" "$bin_dir"
  install_root=$(cd "$install_root" && pwd -P)
  bin_dir=$(cd "$bin_dir" && pwd -P)
  [ "$install_root" != / ] || fail 'Refusing to install into the filesystem root.'
  for link in current bin frontend deploy scripts; do
    if [ -e "$install_root/$link" ] && [ ! -L "$install_root/$link" ]; then fail "Refusing to replace $install_root/$link; choose an application-specific install directory."; fi
  done
  for name in agent signaling turn; do
    if [ -e "$bin_dir/ai-remote-$name" ] && [ ! -L "$bin_dir/ai-remote-$name" ]; then fail "Refusing to replace $bin_dir/ai-remote-$name."; fi
  done
  mkdir -p "$install_root/versions"
  stage_dir=$(mktemp -d "$install_root/.install.XXXXXX")
  tar -xzf "$download_dir/$asset" --no-same-owner -C "$stage_dir"
  payload=$stage_dir/ai-remote
  [ "$(cat "$payload/VERSION")" = "$version" ] || fail 'Archive version does not match the requested release.'
  grep -F "\"target\": \"$target\"" "$payload/manifest.json" >/dev/null || fail 'Archive target does not match this machine.'
  for service in home-agent signaling-server turn-server; do
    [ -f "$payload/bin/$service" ] || fail "Missing $service in the archive."
    chmod 755 "$payload/bin/$service"
  done
  [ -f "$payload/frontend/index.html" ] || fail 'Frontend is missing from the archive.'
  for name in home-agent signaling turn; do
    [ -f "$payload/deploy/$name.env.example" ] || fail "Missing $name configuration template."
  done
  version_dir=$install_root/versions/$version-$target
  if [ -e "$version_dir" ]; then
    [ -f "$version_dir/.archive-sha256" ] && [ "$(cat "$version_dir/.archive-sha256")" = "$expected_hash" ] || fail 'This version already exists with different contents. Use a new release tag or install directory.'
  else
    printf '%s\n' "$expected_hash" > "$payload/.archive-sha256"
    mv "$payload" "$version_dir"
  fi
  if [ ! -d "$install_root/config" ]; then mkdir -m 700 "$install_root/config"; fi
  for name in home-agent signaling turn; do
    if [ ! -e "$install_root/config/$name.env" ]; then
      cp "$version_dir/deploy/$name.env.example" "$install_root/config/$name.env"
      chmod 600 "$install_root/config/$name.env"
    fi
  done
  ln -sfn "$version_dir" "$install_root/current"
  for link in bin frontend deploy scripts; do ln -sfn "current/$link" "$install_root/$link"; done
  ln -sfn "$install_root/bin/home-agent" "$bin_dir/ai-remote-agent"
  ln -sfn "$install_root/bin/signaling-server" "$bin_dir/ai-remote-signaling"
  ln -sfn "$install_root/bin/turn-server" "$bin_dir/ai-remote-turn"
  if [ -f "$install_root/deploy/deploy.sh" ]; then
    chmod 755 "$install_root/deploy/deploy.sh" 2>/dev/null || true
    ln -sfn "$install_root/deploy/deploy.sh" "$bin_dir/ai-remote-deploy"
  fi
  printf 'Installed AI Remote %s in %s\n' "$version" "$install_root"
  printf 'Configuration (existing files preserved): %s/config\n' "$install_root"
  printf 'Check the install: "%s/ai-remote-agent" --version\n' "$bin_dir"
  printf 'Add to PATH if needed: export PATH="%s:$PATH"\n' "$bin_dir"

  printf '\n'
  printf '================================================================================\n'
  printf '  🚀 AI Remote %s 安装完成！[免配置模式 / Zero-Config]\n' "$version"
  printf '================================================================================\n'
  printf '  🌐 [云端 VPS] 启动信令服务 (自带 Web 前端与配置面板):\n'
  printf '     "%s/ai-remote-signaling"\n' "$bin_dir"
  printf '\n'
  printf '  🏠 [家里电脑] 启动 Agent (直连本机 Ollama 11434):\n'
  printf '     "%s/ai-remote-agent" <信令WS地址> <访问Token>\n' "$bin_dir"
  printf '     例如: ai-remote-agent ws://your-vps-ip:8080/ws your-token\n'
  printf '\n'
  printf '  ⚙️  [Web 管理中心] 实时监控与连接测试:\n'
  printf '     http://<VPS-IP>:8080/setup\n'
  printf '\n'
  printf '  🛠️  [生产部署] 一键注册为 systemd 后台服务 (守护进程):\n'
  printf '     sudo bash "%s/deploy/deploy.sh"\n' "$install_root"
  printf '     或直接执行: sudo ai-remote-deploy\n'
  printf '================================================================================\n\n'

  if [ -n "${AI_REMOTE_DEPLOY:-}" ] && [ -f "$install_root/deploy/deploy.sh" ]; then
    bash "$install_root/deploy/deploy.sh" "$AI_REMOTE_DEPLOY"
  fi
}

main "$@"
