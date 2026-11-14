#!/usr/bin/env bash
# compose 全栈冒烟（阶段 09 主缝验收；可复现命令，见 docs/operations.md）。
# 前置：镜像 chargepilot/app:latest 已构建；docker/.env 已按 .env.example 填好。
# 用法：scripts/ops/compose-smoke.sh [BASE_URL，默认 http://127.0.0.1:8080]
set -euo pipefail
BASE="${1:-http://127.0.0.1:8080}"
PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); echo "  ✓ $1"; }
bad()  { FAIL=$((FAIL+1)); echo "  ✗ $1"; }
check(){ local name="$1" cond="$2"; if eval "$cond"; then ok "$name"; else bad "$name"; fi }

echo "== 1. 健康、指标与静态资源 =="
check "/health/live"        "curl -fsS $BASE/health/live | grep -q ok"
check "/health/ready"       "curl -fsS $BASE/health/ready | grep -q ok"
check "/metrics 可达"       "curl -fsS $BASE/metrics | grep -q telemetry_flush_batch_last"
check "/admin/ 直出 HTML"   "curl -fsS $BASE/admin/ | grep -qi '<div id=\"root\">'"
check "/h5/ 直出 HTML"      "curl -fsS $BASE/h5/ | grep -qi html"

echo "== 2. 管理端主缝 =="
ADMIN_TOKEN=$(curl -fsS -X POST $BASE/api/v1/admin/auth/login \
  -H 'content-type: application/json' \
  -d "{\"username\":\"$ADMIN_BOOTSTRAP_USER\",\"password\":\"$ADMIN_BOOTSTRAP_PASSWORD\"}" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["data"]["access_token"])')
check "管理员登录" "[ -n "$ADMIN_TOKEN" ]"
AH="authorization: Bearer $ADMIN_TOKEN"
SUF=$RANDOM$RANDOM

STATION_ID=$(curl -fsS -X POST $BASE/api/v1/admin/stations -H "$AH" -H 'content-type: application/json' \
  -d "{\"name\":\"冒烟站$SUF\",\"longitude\":120.1,\"latitude\":30.2,\"status\":\"active\"}" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["data"]["id"])')
check "建站点 id=$STATION_ID" "[ -n "$STATION_ID" ]"

VENDOR_ID=$(curl -fsS -X POST $BASE/api/v1/admin/vendors -H "$AH" -H 'content-type: application/json' \
  -d "{\"vendor_code\":\"v$SUF\",\"vendor_name\":\"冒烟厂商\",\"adapter_class\":\"dc589\",\"protocol\":\"tcp\",\"status\":\"enabled\"}" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["data"]["id"])')
check "建厂商 id=$VENDOR_ID" "[ -n "$VENDOR_ID" ]"

SCHEME='{"name":"冒烟方案","remark":"","amount":{"algorithm":"server_energy","periods":[{"electric_cents":50,"service_cents":30,"end_minute":1440}]},"packages":[{"id":1,"name":"1元","mode":"amount","price_cents":100},{"id":2,"name":"2小时","mode":"duration","price_cents":200,"minutes":120}],"policy":{"free_minutes":0,"min_electric_cents":0,"max_minutes":600,"loss_rate_bp":0,"channel_bp":10000},"stop":{"stop_when_full":true},"card":{"package_id":2,"max_minutes":600},"display":{"duration":[],"energy":[]}}'
TEMPLATE_ID=$(curl -fsS -X POST $BASE/api/v1/admin/settings/charging-schemes -H "$AH" -H 'content-type: application/json' \
  -d "{\"scheme\":$SCHEME}" | python3 -c 'import json,sys; print(json.load(sys.stdin)["data"]["id"])')
check "建计费模板 id=$TEMPLATE_ID" "[ -n "$TEMPLATE_ID" ]"

APPLY=$(curl -fsS -X POST $BASE/api/v1/admin/settings/charging-schemes/apply -H "$AH" -H 'content-type: application/json' \
  -d "{\"request_id\":\"smoke-$SUF\",\"station_id\":$STATION_ID,\"template_id\":$TEMPLATE_ID,\"expected_version\":0}")
check "发布方案 version=1" "echo '$APPLY' | grep -q '\"version\":1'"

DEVICE_ID="DCSMOKE$SUF"
IMPORT=$(curl -fsS -X POST $BASE/api/v1/admin/device-imports -H "$AH" -H 'content-type: application/json' \
  -d "{\"import_id\":\"imp-smoke-$SUF\",\"devices\":[{\"device_id\":\"$DEVICE_ID\",\"vendor_id\":$VENDOR_ID,\"station_id\":$STATION_ID,\"port_count\":2}]}")
check "导入设备 $DEVICE_ID provisioned=1" "echo '$IMPORT' | grep -q '\"provisioned\":1'"

DIAG=$(curl -fsS $BASE/api/v1/admin/diagnostics -H "$AH" || true)
if [[ "$DIAG" == *charging-start-timeout* ]]; then ok "诊断接口（sweeper+队列）"; else bad "诊断接口（sweeper+队列）: $DIAG"; fi

echo "== 3. 用户端主缝（development 登录 → 下单 → 模拟支付） =="
U_TOKEN=$(curl -fsS -X POST $BASE/api/v1/user/auth/login -H 'content-type: application/json' \
  -d "{\"code\":\"dev:smoke-$SUF\"}" | python3 -c 'import json,sys; print(json.load(sys.stdin)["data"]["jwt"])')
check "用户 development 登录" "[ -n "$U_TOKEN" ]"

OFFERS=$(curl -fsS -X POST $BASE/api/v1/user/scan/offers -H 'content-type: application/json' \
  -d "{\"port_code\":\"$DEVICE_ID-1\"}")
check "扫码查套餐（含 stop_when_full）" "echo '$OFFERS' | grep -q 'stop_when_full'"
OFFER_ID=$(echo "$OFFERS" | python3 -c 'import json,sys; print(json.load(sys.stdin)["data"]["items"][0]["id"])')
check "取套餐 offer_id=$OFFER_ID" "[ -n "$OFFER_ID" ]"

REQUEST_ID=$(python3 -c 'import uuid; print(uuid.uuid4())')
START=$(curl -fsS -X POST $BASE/api/v1/user/scan/start -H "authorization: Bearer $U_TOKEN" -H 'content-type: application/json' \
  -d "{\"client_request_id\":\"$REQUEST_ID\",\"port_code\":\"$DEVICE_ID-1\",\"offer_id\":$OFFER_ID}")
ORDER_NO=$(echo "$START" | python3 -c 'import json,sys; print(json.load(sys.stdin)["data"]["merchant_order_no"])' 2>/dev/null || true)
check "下单 merchant_order_no=$ORDER_NO" "[ -n "${ORDER_NO:-}" ]"

CONFIRM=$(curl -fsS -X POST $BASE/api/v1/user/development/payments/confirm -H "authorization: Bearer $U_TOKEN" -H 'content-type: application/json' \
  -d "{\"merchant_order_no\":\"$ORDER_NO\"}")
check "模拟支付到账（出充电单）" "echo '$CONFIRM' | grep -q 'charge_order_id'"

echo "== 4. 充电启动命令下发（设备不在线 → 兜底扫描 30s 后失败退款） =="
sleep 5
COMPOSE_FILE="$(dirname "$0")/../../docker/compose.prod.yaml"
PENDING=$(docker compose -f "$COMPOSE_FILE" exec -T mysql \
  mysql -uroot -p"$MYSQL_ROOT_PASSWORD" chargepilot -N -e \
  "SELECT status FROM charge_command ORDER BY created_at DESC LIMIT 1" 2>/dev/null || echo unknown)
case "$PENDING" in
  sent|pending|rejected|failed) ok "启动命令已落库（状态: ${PENDING}；sent=等设备回执，rejected=设备离线投递失败，failed=超时已退款）" ;;
  *) bad "启动命令状态异常: $PENDING" ;;
esac

echo
echo "冒烟结果：通过 ${PASS}，失败 ${FAIL}"
[ "$FAIL" -eq 0 ]
