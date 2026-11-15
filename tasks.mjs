#!/usr/bin/env node
// ChargePilot 任务入口（沿用旧仓库 tasks.mjs 习惯，make 无依赖版本）
// 用法：node tasks.mjs <task>
import { spawnSync } from 'node:child_process';
import { existsSync, readFileSync, readdirSync } from 'node:fs';

const BACKEND = 'backend';
const task = process.argv[2] ?? 'help';

function run(cmd, args, opts = {}) {
  const r = spawnSync(cmd, args, { stdio: 'inherit', cwd: opts.cwd ?? BACKEND, env: { ...process.env, ...opts.env } });
  if (r.status !== 0) process.exit(r.status ?? 1);
}

function envOf() {
  // 从 backend/.env 读取 DATABASE_URL（存在时），不覆盖已有环境变量
  const env = {};
  if (existsSync(`${BACKEND}/.env`)) {
    for (const line of readFileSync(`${BACKEND}/.env`, 'utf8').split('\n')) {
      const m = line.match(/^([A-Z_][A-Z0-9_]*)=(.*)$/);
      if (m && !process.env[m[1]]) env[m[1]] = m[2];
    }
  }
  return env;
}

// 统计目录内全部 .rs 文件中 pattern 出现次数（逐行计数，一行多处分别计）。
function countPattern(dir, pattern) {
  let total = 0;
  const walk = (d) => {
    for (const e of readdirSync(d, { withFileTypes: true })) {
      const p = `${d}/${e.name}`;
      if (e.isDirectory()) walk(p);
      else if (e.name.endsWith('.rs')) {
        for (const line of readFileSync(p, 'utf8').split('\n')) {
          let idx = line.indexOf(pattern);
          while (idx !== -1) {
            total += 1;
            idx = line.indexOf(pattern, idx + pattern.length);
          }
        }
      }
    }
  };
  walk(dir);
  return total;
}

const tasks = {
  build() { run('cargo', ['build', '--workspace']); },
  test() { run('cargo', ['test', '--workspace']); },
  fmt() { run('cargo', ['fmt', '--all']); },
  'fmt-check'() { run('cargo', ['fmt', '--all', '--', '--check']); },
  // 静态门禁（二期票 03）：workspace.lints 让 rustc 警告即错误，clippy 失败级
  // 三组（correctness/suspicious/perf）+ unwrap/expect/wildcard 禁令在此把守。
  clippy() { run('cargo', ['clippy', '--workspace', '--all-targets']); },
  // 前后端契约对账（纯静态：扫前端调用点与后端路由注册，免服务）。
  'api-contract'() { run('python3', ['scripts/ops/check-api-contract.py'], { cwd: '.' }); },
  // SQL 层审计（蓝图批次 4 收尾）：域 crate 的 service/ 只做业务编排与事务
  // 组装，SQL 只允许出现在 repo/。逐域统计 sqlx::query* 出现次数，非零即失败
  // （子串口径覆盖 query/query_as/query_scalar/query_builder 全部形态）。
  'check-sql'() {
    const modulesDir = `${BACKEND}/crates/modules`;
    const offenders = [];
    let domains = 0;
    for (const entry of readdirSync(modulesDir, { withFileTypes: true })) {
      if (!entry.isDirectory()) continue;
      const serviceDir = `${modulesDir}/${entry.name}/src/service`;
      if (!existsSync(serviceDir)) continue;
      domains += 1;
      const count = countPattern(serviceDir, 'sqlx::query');
      if (count > 0) offenders.push(`${entry.name}: ${count} 处`);
    }
    if (offenders.length > 0) {
      console.error(
        `SQL 层违规（service/ 出现 sqlx::query，SQL 应移入 repo/）：\n  ${offenders.join('\n  ')}`,
      );
      process.exit(1);
    }
    console.log(`SQL 层审计：${domains} 域 service/ 均无 sqlx::query，SQL 全部收敛 repo/`);
    // 网关层业务查询禁令（SQL 归位收尾）：device_events.rs 的回执定位/确认
    // 已下沉所属域 repo（charging/device），出现 sqlx::query 即越界。
    // sink.rs / telemetry.rs 是蓝图第五章豁免的运行时存储，不在禁令内。
    const gatewayEvents = `${BACKEND}/crates/gateway-runtime/src/device_events.rs`;
    if (existsSync(gatewayEvents)) {
      const count = readFileSync(gatewayEvents, 'utf8')
        .split('\n')
        .filter((line) => line.includes('sqlx::query')).length;
      if (count > 0) {
        console.error(
          `SQL 层违规（gateway-runtime/device_events.rs 出现 sqlx::query，业务查询应下沉所属域 repo）：${count} 处`,
        );
        process.exit(1);
      }
    }
    console.log('网关层审计：device_events.rs 无 sqlx::query，业务查询保持域内 repo');
  },
  check() {
    tasks['fmt-check']();
    tasks['check-sql']();
    tasks.clippy();
    tasks.test();
    tasks['api-contract']();
  },
  migrate() { run('cargo', ['run', '-p', 'chargepilot-app', '--', 'migrate'], { env: envOf() }); },
  // 真库集成测试（票 01，.scratch/engineering-refactor/issues/01）：
  // 真库门控测试静态标记 #[ignore]，本任务注入连接变量并以 --include-ignored 全量执行。
  // 用法：node tasks.mjs test-integration [fresh]——fresh 先重建开发库（DROP+CREATE）再跑，
  // 清理历史残留数据对频次窗口类断言的干扰。需 db-up / redis-up 容器在运行。
  'test-integration'() {
    const TEST_MYSQL_URL = 'mysql://chargepilot:chargepilot@127.0.0.1:3307/chargepilot_dev';
    const TEST_REDIS_URL = 'redis://127.0.0.1:6380';
    if (process.argv[3] === 'fresh') {
      spawnSync('docker', [
        'exec', 'chargepilot-dev-mysql', 'mysql', '-uroot', '-proot', '-e',
        'DROP DATABASE IF EXISTS chargepilot_dev; CREATE DATABASE chargepilot_dev;',
      ], { stdio: 'inherit' });
      console.log('开发库已重建，重新迁移…');
    }
    // 集成测试假设 schema 就绪；fresh 后必须重新迁移（平时迁移幂等，代价低）。
    run('cargo', ['run', '-p', 'chargepilot-app', '--', 'migrate'], {
      env: { ...envOf(), DATABASE_URL: TEST_MYSQL_URL },
    });
    // 前置清场（票 23，承接票 16/20 登记的移交）：历史轮残留会挤占领取窗口
    // （孤儿退款记录占 refund 领取 LIMIT、旧事件占 domain_event claim 窗口、
    // 到期导出任务被清理循环一并回收挤占计数断言、限流键污染频次断言）。
    // 只清测试残留，不触业务终态数据。
    spawnSync('docker', [
      'exec', 'chargepilot-dev-mysql', 'mysql', '-uchargepilot', '-pchargepilot', 'chargepilot_dev', '-e',
      "DELETE rr FROM refund_record rr LEFT JOIN payment_order po ON po.id = rr.payment_order_id"
      + " WHERE po.id IS NULL AND rr.status IN ('pending','processing');"
      + " DELETE FROM domain_event WHERE status IN ('pending','failed');"
      + " DELETE FROM export_task WHERE expires_at < UTC_TIMESTAMP(3) OR status IN ('failed','expired');",
    ], { stdio: 'inherit' });
    spawnSync('docker', [
      'exec', 'chargepilot-dev-redis', 'redis-cli', 'DEL', 'admin:login-rate:10.0.0.1',
    ], { stdio: 'inherit' });
    run('cargo', ['test', '--workspace', '--', '--include-ignored'], {
      env: { ...envOf(), TEST_MYSQL_URL, TEST_REDIS_URL },
    });
  },
  // 开发数据库：临时 MySQL 容器（gh-proxy 镜像前缀，见阶段 09 spec）
  'db-up'() {
    run('docker', [
      'run', '-d', '--name', 'chargepilot-dev-mysql',
      '-e', 'MYSQL_ROOT_PASSWORD=root', '-e', 'MYSQL_DATABASE=chargepilot_dev',
      '-e', 'MYSQL_USER=chargepilot', '-e', 'MYSQL_PASSWORD=chargepilot',
      '-p', '3307:3306', 'gh-proxy.org/docker/mysql:8.4',
    ], { cwd: '.' });
    console.log('MySQL 已启动：127.0.0.1:3307（chargepilot_dev / chargepilot）');
  },
  'db-down'() { spawnSync('docker', ['rm', '-f', 'chargepilot-dev-mysql'], { stdio: 'inherit' }); },
  // 开发 Redis（会话/MFA/频次，gh-proxy 镜像前缀；宿主 6380 避开默认实例）
  'redis-up'() {
    run('docker', ['run', '-d', '--name', 'chargepilot-dev-redis', '-p', '6380:6379', 'gh-proxy.org/docker/redis:8-alpine'], { cwd: '.' });
    console.log('Redis 已启动：127.0.0.1:6380');
  },
  'redis-down'() { spawnSync('docker', ['rm', '-f', 'chargepilot-dev-redis'], { stdio: 'inherit' }); },
  help() {
    console.log('任务：build | test | fmt | fmt-check | check-sql | check | migrate | test-integration [fresh] | db-up | db-down | redis-up | redis-down');
  },
};

(tasks[task] ?? tasks.help)();
