#!/usr/bin/env bash
set -e

DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"

echo "================================================================================"
echo "  🚀 AI Remote 本地开发与体验启动器"
echo "================================================================================"

if ! command -v node >/dev/null 2>&1; then
  echo "错误: 未找到 Node.js，请先安装 Node.js 22.12 或更高版本。"
  exit 1
fi

node "$DIR/scripts/dev.mjs" "$@"
