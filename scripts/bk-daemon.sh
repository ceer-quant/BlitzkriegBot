#!/usr/bin/env bash
# BlitzkriegBot 0-接管全自动后台交付守护进程
# 用途: 在宿主 macOS 终端后台运行一次，之后所有 AI 开发的特性分支自动秒级交付入 main，无需用户点任何允许。
#
# 原理:
#   AI 在沙箱内静默开发、测试完毕后，将交付指令写入 .pipeline-queue
#   本守护进程在宿主环境检测到队列后，自动提取凭证、push、创建 Issue/PR、等 CI 并 Squash 合并入 main。
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

QUEUE_FILE="$REPO_ROOT/.pipeline-queue"
PIPELINE_SCRIPT="$REPO_ROOT/scripts/auto-pipeline.sh"

echo "========================================================"
echo "⚡ BlitzkriegBot 0-接管交付守护进程已启动"
echo "📂 监控仓库: $REPO_ROOT"
echo "📬 队列文件: $QUEUE_FILE"
echo "========================================================"

touch "$QUEUE_FILE"

while true; do
  if [ -s "$QUEUE_FILE" ]; then
    echo "🔔 检测到新的交付任务..."
    # 逐行读取任务: branch|title
    while IFS='|' read -r branch title || [ -n "$branch" ]; do
      [ -z "$branch" ] && continue
      echo "🚀 开始交付分支: $branch"
      echo "📝 任务标题: $title"
      
      if bash "$PIPELINE_SCRIPT" ship "$branch" "$title"; then
        echo "✅ 分支 $branch 交付成功并已合入 main！"
      else
        echo "❌ 分支 $branch 交付失败，请查看上方日志"
      fi
    done < "$QUEUE_FILE"
    
    # 清空已处理队列
    : > "$QUEUE_FILE"
    echo "💤 队列已清空，继续待命..."
  fi
  sleep 3
done
