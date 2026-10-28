#!/usr/bin/env bash
# BlitzkriegBot 0-接管全自动流水线脚本 (macOS)
# 规范依据: dev-docs/AI_WORKFLOW.md, dev-docs/GITHUB_GOVERNANCE.md, dev-docs/DEVELOPMENT.md
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# 1. 凭证提取
CRED_FILE="${HOME}/.config/blitzkrieg-bot/git-credentials"
if [ ! -f "$CRED_FILE" ]; then
  CRED_FILE="/Users/fancer/.config/blitzkrieg-bot/git-credentials"
fi

if [ ! -f "$CRED_FILE" ]; then
  echo "错误: 未找到凭证文件 $CRED_FILE" >&2
  exit 1
fi

TOKEN="$(sed -E 's|https://[^:]+:([^@]+)@.*|\1|' "$CRED_FILE")"
if [ -z "$TOKEN" ]; then
  echo "错误: 无法从 $CRED_FILE 提取 GitHub Token" >&2
  exit 1
fi

export GH_TOKEN="$TOKEN"
GIT_AUTHOR_NAME="ceer_quant"
GIT_AUTHOR_EMAIL="ceer_quant@users.noreply.github.com"
GIT_CMD=(git -c "user.name=$GIT_AUTHOR_NAME" -c "user.email=$GIT_AUTHOR_EMAIL" -c "credential.helper=store --file=$CRED_FILE")
REPO="ceer-quant/BlitzkriegBot"
API_AUTH=(-H "Authorization: Bearer $TOKEN" -H "Accept: application/vnd.github+json")

# 2. 本地门禁检查
run_local_gates() {
  echo "== [1/4] 运行本地安全门禁: secret-scan =="
  bash scripts/secret-scan.sh

  echo "== [2/4] 运行 WebUI 全部门禁: npm run check:all (31 套回归测试) =="
  (
    cd ui/webapp/webui
    npm run check:all
  )
  echo "== 本地门禁全部通过 (PASS) =="
}

# 3. 创建 GitHub Issue
create_issue() {
  local title="$1"
  local body="$2"
  local payload
  payload=$(python3 -c "import json, sys; print(json.dumps({'title': sys.argv[1], 'body': sys.argv[2], 'labels': ['type/feature', 'module/panel', 'priority/P2']}))" "$title" "$body")
  
  local res
  res=$(curl -s -X POST "${API_AUTH[@]}" -d "$payload" "https://api.github.com/repos/$REPO/issues")
  local issue_num
  issue_num=$(echo "$res" | python3 -c "import json, sys; print(json.load(sys.stdin).get('number', ''))" 2>/dev/null || echo "")
  echo "$issue_num"
}

# 4. 创建 Pull Request
create_pr() {
  local branch="$1"
  local title="$2"
  local body="$3"
  local payload
  payload=$(python3 -c "import json, sys; print(json.dumps({'head': sys.argv[1], 'base': 'main', 'title': sys.argv[2], 'body': sys.argv[3]}))" "$branch" "$title" "$body")

  local res
  res=$(curl -s -X POST "${API_AUTH[@]}" -d "$payload" "https://api.github.com/repos/$REPO/pulls")
  local pr_num
  pr_num=$(echo "$res" | python3 -c "import json, sys; doc = json.load(sys.stdin); print(doc.get('number', ''))" 2>/dev/null || echo "")

  if [ -z "$pr_num" ]; then
    echo "⚠️ PR 创建响应: $res" >&2
    local existing
    existing=$(curl -s "${API_AUTH[@]}" "https://api.github.com/repos/$REPO/pulls?head=ceer-quant:$branch&state=open")
    pr_num=$(echo "$existing" | python3 -c "import json, sys; pulls = json.load(sys.stdin); print(pulls[0]['number'] if isinstance(pulls, list) and len(pulls) > 0 else '')" 2>/dev/null || echo "")
  fi
  echo "$pr_num"
}

# 5. 等待 CI 并合并
wait_and_merge() {
  local pr_num="$1"
  if [ -z "$pr_num" ]; then
    echo "❌ 错误: 未能获取有效的 PR 编号，终止合并" >&2
    exit 1
  fi
  echo "== 正在监控 PR #$pr_num 的 CI 门禁状态 =="

  while true; do
    local pr_json
    pr_json=$(curl -s "${API_AUTH[@]}" "https://api.github.com/repos/$REPO/pulls/$pr_num")
    local head_sha
    head_sha=$(echo "$pr_json" | python3 -c "import json, sys; print(json.load(sys.stdin)['head']['sha'])" 2>/dev/null || echo "")

    if [ -z "$head_sha" ]; then
      echo "⚠️ 获取 head sha 失败，稍后重试: $pr_json" >&2
      sleep 10
      continue
    fi

    local checks_json
    checks_json=$(curl -s "${API_AUTH[@]}" "https://api.github.com/repos/$REPO/commits/$head_sha/check-runs")
    
    local verdict
    verdict=$(echo "$checks_json" | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
    runs = data.get('check_runs', [])
    if not runs:
        print('pending')
        sys.exit(0)
    conclusions = [r.get('conclusion') for r in runs]
    statuses = [r.get('status') for r in runs]
    if any(s in ('in_progress', 'queued') for s in statuses):
        print('running')
    elif any(c in ('failure', 'timed_out', 'cancelled') for c in conclusions):
        print('failed')
    elif all(c in ('success', 'neutral', 'skipped') for c in conclusions):
        print('passed')
    else:
        print('unknown')
except Exception as e:
    print('error')
")
    if [ "$verdict" = "passed" ]; then
      echo "✅ CI 检查已全部通过！"
      break
    elif [ "$verdict" = "failed" ]; then
      echo "❌ CI 检查失败，请核对日志！" >&2
      exit 1
    else
      echo "⏳ CI 运行中... 等待 30 秒 (当前状态: $verdict)"
      sleep 30
    fi
  done

  echo "== 执行 Squash 合并 =="
  local pr_title
  pr_title=$(echo "$pr_json" | python3 -c "import json, sys; print(json.load(sys.stdin)['title'])")
  local merge_payload
  merge_payload=$(python3 -c "import json, sys; print(json.dumps({'commit_title': sys.argv[1], 'merge_method': 'squash'}))" "$pr_title")

  local merge_res
  merge_res=$(curl -s -X PUT "${API_AUTH[@]}" -d "$merge_payload" "https://api.github.com/repos/$REPO/pulls/$pr_num/merge")
  local merged
  merged=$(echo "$merge_res" | python3 -c "import json, sys; print(json.load(sys.stdin).get('merged', False))")

  if [ "$merged" = "True" ]; then
    echo "🎉 PR #$pr_num 已成功 Squash 合并！"
    local branch
    branch=$(echo "$pr_json" | python3 -c "import json, sys; print(json.load(sys.stdin)['head']['ref'])")
    
    echo "== 删除远端分支 $branch =="
    curl -s -X DELETE "${API_AUTH[@]}" "https://api.github.com/repos/$REPO/git/refs/heads/$branch" >/dev/null || true
    
    echo "== 同步本地 main 分支 =="
    "${GIT_CMD[@]}" fetch ceer
    "${GIT_CMD[@]}" checkout main
    "${GIT_CMD[@]}" pull ceer main
    echo "== 完成！工作区已是最新的 main 分支 =="
  else
    echo "❌ 合并失败: $merge_res" >&2
    exit 1
  fi
}

COMMAND="${1:-help}"
case "$COMMAND" in
  check)
    run_local_gates
    ;;
  ship)
    BRANCH="${2:-}"
    TITLE="${3:-}"
    if [ -z "$BRANCH" ] || [ -z "$TITLE" ]; then
      echo "用法: $0 ship <branch_name> <pr_title>" >&2
      exit 1
    fi
    run_local_gates

    echo "== 推送分支 $BRANCH 到 ceer =="
    "${GIT_CMD[@]}" push ceer "$BRANCH" 2>&1 | sed 's|https://[^@]*@|redacted:|'

    echo "== 创建 GitHub Issue =="
    ISSUE_NUM=$(create_issue "$TITLE" "自动化流水线任务：$TITLE")
    echo "已创建 Issue #$ISSUE_NUM"

    echo "== 创建 Pull Request =="
    PR_BODY="### 变更说明
$TITLE

Closes #$ISSUE_NUM

### 门禁验证
- 本地 secret-scan 通过
- WebUI check:all 31 套前端门禁全部 PASS"
    PR_NUM=$(create_pr "$BRANCH" "$TITLE" "$PR_BODY")
    echo "已创建 PR #$PR_NUM"

    wait_and_merge "$PR_NUM"
    ;;
  merge)
    PR_NUM="${2:-}"
    if [ -z "$PR_NUM" ]; then
      echo "用法: $0 merge <pr_number>" >&2
      exit 1
    fi
    wait_and_merge "$PR_NUM"
    ;;
  *)
    echo "BlitzkriegBot 0-接管流水线命令帮助:"
    echo "  $0 check                  : 运行本地安全与 WebUI 31 套门禁"
    echo "  $0 ship <branch> <title>  : 门禁 -> 推送 -> 开Issue/PR -> 等CI -> 自动合并"
    echo "  $0 merge <pr_number>      : 等待指定 PR 的 CI 绿灯并自动执行 Squash 合并"
    ;;
esac
