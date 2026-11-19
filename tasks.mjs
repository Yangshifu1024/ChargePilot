#!/usr/bin/env node
// ChargePilot 任务入口（沿用旧仓库 tasks.mjs 习惯，make 无依赖版本）
// 用法：node tasks.mjs <task>
import { spawn, spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { createInterface } from 'node:readline';
import net from 'node:net';
import { setTimeout as sleep } from 'node:timers/promises';

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

// 模拟器默认连 dev 库（与 db-up/test-integration 一致），便于 --provision 开箱可用。
function simulatorEnv() {
  return {
    DATABASE_URL: 'mysql://chargepilot:chargepilot@127.0.0.1:3307/chargepilot_dev',
    ...envOf(),
    ...process.env,
  };
}

// —— debug 任务共用工具 ——

const DEV_MYSQL = 'chargepilot-dev-mysql';
const DEV_REDIS = 'chargepilot-dev-redis';
const IS_WINDOWS = process.platform === 'win32';
// npm 在 Windows 上是 .cmd，其余平台直接调 npm。
const NPM = IS_WINDOWS ? 'npm.cmd' : 'npm';

const DEBUG_HELP = `debug：一键拉起开发环境（dev MySQL/Redis + 后端 + 两个前端）
用法：node tasks.mjs debug [选项]
  --no-admin    不启动运营后台 Vite（5173）
  --no-h5       不启动 H5 开发服务（5174）
  --no-infra    不管理 MySQL/Redis 容器（使用 DATABASE_URL / REDIS_URL 指向的外部实例）
  --fresh       启动前 DROP+CREATE 重建开发库 chargepilot_dev
  --clean       Ctrl-C 退出时一并移除 dev 容器（默认保留，便于下次快速启动）
  -h, --help    显示本帮助
缺省环境变量：DATABASE_URL/REDIS_URL 指向 dev 容器；JWT_SECRET 每次随机生成；
引导账号 admin / admin-dev-pass-1234（仅账号不存在时创建，已存在账号的密码不会被覆盖）；
PAYMENT_MODE=simulation、LOGIN_MODE=development。
backend/.env 中的变量优先于上述缺省值（process.env 优先级最高）。`;

function docker(args, opts = {}) {
  return spawnSync('docker', args, { encoding: 'utf8', ...opts });
}

/** 容器状态：running / stopped / missing。 */
function containerState(name) {
  const r = docker(['inspect', '-f', '{{.State.Running}}', name]);
  if (r.status !== 0) return 'missing';
  return r.stdout.trim() === 'true' ? 'running' : 'stopped';
}

/** 复用已存在容器，否则创建；已停止的容器用 start 保留数据卷。 */
function ensureContainer(name, runArgs, label) {
  const state = containerState(name);
  if (state === 'running') {
    console.log(`复用运行中的 ${label} 容器（${name}）`);
    return;
  }
  if (state === 'stopped') {
    console.log(`启动已存在的 ${label} 容器（${name}）`);
    if (docker(['start', name], { stdio: 'inherit' }).status !== 0) {
      throw new Error(`${label} 容器启动失败：${name}`);
    }
    return;
  }
  console.log(`创建 ${label} 容器（${name}）…`);
  if (docker(['run', '-d', '--name', name, ...runArgs], { stdio: 'inherit' }).status !== 0) {
    throw new Error(`${label} 容器创建失败：${name}`);
  }
}

async function waitUntil(check, label, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (check()) return;
    await sleep(500);
  }
  throw new Error(`等待${label}超时（${Math.round(timeoutMs / 1000)}s）`);
}

function isPortFree(port) {
  return new Promise((resolve) => {
    const server = net.createServer();
    server.unref();
    server.once('error', () => resolve(false));
    server.listen(port, '127.0.0.1', () => server.close(() => resolve(true)));
  });
}

async function requireFreePort(port, label) {
  if (await isPortFree(port)) return;
  throw new Error(`端口 ${port}（${label}）已被占用，请先停掉占用进程`);
}

/** 从监听地址取端口：0.0.0.0:8080 / 127.0.0.1:9100 → 8080 / 9100。 */
function portOf(addr, fallback) {
  const m = /:(\d+)$/.exec(String(addr));
  return m ? Number(m[1]) : fallback;
}

function localHttpBase(addr) {
  const [host, port] = String(addr).split(':');
  const local = !host || host === '0.0.0.0' || host === '::' ? '127.0.0.1' : host;
  return `http://${local}:${port || '8080'}`;
}

function ensureNodeModules(dir) {
  if (existsSync(`${dir}/node_modules`)) return;
  console.log(`安装 ${dir} 依赖（npm ci）…`);
  const r = spawnSync(NPM, ['ci', '--no-audit', '--no-fund'], { cwd: dir, stdio: 'inherit' });
  if (r.status !== 0) throw new Error(`${dir} 依赖安装失败`);
}

async function waitForHttp(url, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const res = await fetch(url, { signal: AbortSignal.timeout(2000) });
      if (res.ok) return true;
    } catch {
      // 后端尚未就绪（连接拒绝/超时），继续轮询。
    }
    await sleep(500);
  }
  return false;
}

// 子进程集合与停机状态：debug 任务退出时统一收口。
const children = [];
let shuttingDown = false;
let cleanupInfra = false;

const TASK_COLORS = { api: '\x1b[36m', admin: '\x1b[35m', h5: '\x1b[33m' };

function pipeWithPrefix(stream, name) {
  const color = TASK_COLORS[name] ?? '\x1b[90m';
  const rl = createInterface({ input: stream, crlfDelay: Infinity });
  rl.on('line', (line) => {
    process.stdout.write(`${color}[${name}]\x1b[0m ${line}\n`);
  });
}

function killTree(child, signal) {
  if (child.pid == null || child.exitCode !== null || child.signalCode !== null) return;
  try {
    // detached 子进程自成进程组，负 PID 一次收掉 cargo/npm 及其子进程。
    if (IS_WINDOWS) child.kill(signal);
    else process.kill(-child.pid, signal);
  } catch {
    try {
      child.kill(signal);
    } catch {
      // 已退出：忽略。
    }
  }
}

/** 停掉全部子进程（先 TERM，20 秒后 KILL），按需移除 dev 容器后退出。 */
async function shutdown(exitCode) {
  if (shuttingDown) return;
  shuttingDown = true;
  console.log('\n正在停止开发环境…');
  for (const { child } of children) killTree(child, 'SIGTERM');
  const deadline = Date.now() + 20000;
  const alive = () =>
    children.some(({ child }) => child.exitCode === null && child.signalCode === null);
  while (Date.now() < deadline && alive()) await sleep(100);
  for (const { child } of children) killTree(child, 'SIGKILL');
  if (cleanupInfra) {
    for (const name of [DEV_MYSQL, DEV_REDIS]) docker(['rm', '-f', name], { stdio: 'inherit' });
  }
  process.exit(exitCode);
}

function spawnManaged(name, command, args, opts) {
  const child = spawn(command, args, {
    cwd: opts.cwd,
    env: opts.env,
    stdio: ['ignore', 'pipe', 'pipe'],
    detached: !IS_WINDOWS,
  });
  children.push({ name, child });
  pipeWithPrefix(child.stdout, name);
  pipeWithPrefix(child.stderr, name);
  child.on('error', (err) => {
    console.error(`[${name}] 启动失败：${err.message}`);
    void shutdown(1);
  });
  child.on('exit', (code, signal) => {
    if (shuttingDown) return;
    const reason = signal ? `信号 ${signal}` : `退出码 ${code}`;
    console.error(`[${name}] 已退出（${reason}），停止其余进程`);
    void shutdown(code && code !== 0 ? code : 1);
  });
  return child;
}

function parseDebugArgs(args) {
  const opts = { admin: true, h5: true, infra: true, fresh: false, clean: false, help: false };
  for (const arg of args) {
    if (arg === '--no-admin') opts.admin = false;
    else if (arg === '--no-h5') opts.h5 = false;
    else if (arg === '--no-infra') opts.infra = false;
    else if (arg === '--fresh') opts.fresh = true;
    else if (arg === '--clean') opts.clean = true;
    else if (arg === '--help' || arg === '-h') opts.help = true;
    else throw new Error(`未知参数：${arg}（用 --help 查看可用选项）`);
  }
  return opts;
}

function printDebugBanner(env, opts) {
  const lines = [
    '',
    'ChargePilot 开发环境启动中（首次编译需要几分钟）',
    `  后端 API    ${localHttpBase(env.HTTP_ADDR)}  (/health/live, /metrics)`,
    `  设备接入    ${env.DEVICE_ADDR}  (DC589)`,
  ];
  if (opts.admin) lines.push('  运营后台    http://127.0.0.1:5173/admin/');
  if (opts.h5) lines.push('  H5 用户端   http://127.0.0.1:5174/');
  lines.push(`  数据库      ${env.DATABASE_URL}`);
  lines.push(`  Redis       ${env.REDIS_URL}`);
  if (env.ADMIN_BOOTSTRAP_USER) {
    lines.push(
      `  管理员      ${env.ADMIN_BOOTSTRAP_USER} / ${env.ADMIN_BOOTSTRAP_PASSWORD}（仅新建账号；已存在则沿用原密码）`,
    );
  }
  lines.push(
    `  Ctrl-C 停止${opts.infra && opts.clean ? '并移除 dev 容器' : '（dev 容器保留，用 db-down / redis-down 清理）'}`,
  );
  console.log(`${lines.join('\n')}\n`);
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
  // 独立模拟器（crates/simulator）：single 指定板号联调，batch 容量压测。
  // 默认连 dev 库，--provision 开箱可用；process.env / backend/.env 可覆盖。
  simulator() {
    run('cargo', ['run', '-p', 'chargepilot-simulator', '--', 'single', ...process.argv.slice(3)], {
      env: simulatorEnv(),
    });
  },
  'simulator-batch'() {
    run('cargo', ['run', '-p', 'chargepilot-simulator', '--', 'batch', ...process.argv.slice(3)], {
      env: simulatorEnv(),
    });
  },
  // 一键开发环境：复用/创建 dev 容器，拉起后端与两个前端，Ctrl-C 统一收口。
  async debug() {
    const opts = parseDebugArgs(process.argv.slice(3));
    if (opts.help) {
      console.log(DEBUG_HELP);
      return;
    }
    cleanupInfra = opts.infra && opts.clean;

    for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
      process.on(signal, () => {
        // 130/143 是 shell 对 Ctrl-C / SIGTERM 的常规退出码。
        void shutdown(signal === 'SIGINT' ? 130 : 143);
      });
    }

    // 环境优先级：process.env > backend/.env > dev 缺省值。
    const env = { ...envOf(), ...process.env };
    const defaults = {
      DATABASE_URL: 'mysql://chargepilot:chargepilot@127.0.0.1:3307/chargepilot_dev',
      REDIS_URL: 'redis://127.0.0.1:6380',
      JWT_SECRET: randomBytes(32).toString('hex'),
      PAYMENT_MODE: 'simulation',
      LOGIN_MODE: 'development',
      ADMIN_BOOTSTRAP_USER: 'admin',
      ADMIN_BOOTSTRAP_PASSWORD: 'admin-dev-pass-1234',
      HTTP_ADDR: '0.0.0.0:8080',
      DEVICE_ADDR: '0.0.0.0:9100',
      RUST_LOG: 'info',
    };
    for (const [key, value] of Object.entries(defaults)) {
      if (!env[key] || !String(env[key]).trim()) env[key] = value;
    }

    // 提前撞端口，避免启动一半才失败。
    await requireFreePort(portOf(env.HTTP_ADDR, 8080), '后端 HTTP');
    await requireFreePort(portOf(env.DEVICE_ADDR, 9100), '设备接入');
    if (opts.admin) await requireFreePort(5173, 'admin-web');
    if (opts.h5) await requireFreePort(5174, 'H5');

    if (opts.infra) {
      if (docker(['info']).status !== 0) {
        throw new Error('Docker 未运行或不可用（或改用 --no-infra）');
      }
      ensureContainer(
        DEV_MYSQL,
        [
          '-e', 'MYSQL_ROOT_PASSWORD=root',
          '-e', 'MYSQL_DATABASE=chargepilot_dev',
          '-e', 'MYSQL_USER=chargepilot',
          '-e', 'MYSQL_PASSWORD=chargepilot',
          '-p', '3307:3306',
          'gh-proxy.org/docker/mysql:8.4',
        ],
        'MySQL',
      );
      ensureContainer(
        DEV_REDIS,
        ['-p', '6380:6379', 'gh-proxy.org/docker/redis:8-alpine'],
        'Redis',
      );
      await waitUntil(
        () =>
          docker([
            'exec', DEV_MYSQL, 'mysqladmin', 'ping',
            '-h', '127.0.0.1', '-uroot', '-proot', '--silent',
          ]).status === 0,
        ' MySQL 就绪',
        120000,
      );
      await waitUntil(
        () => (docker(['exec', DEV_REDIS, 'redis-cli', 'ping']).stdout ?? '').includes('PONG'),
        ' Redis 就绪',
        30000,
      );
      if (opts.fresh) {
        console.log('重建开发库 chargepilot_dev…');
        const r = docker(
          [
            'exec', DEV_MYSQL, 'mysql', '-uroot', '-proot', '-e',
            'DROP DATABASE IF EXISTS chargepilot_dev; CREATE DATABASE chargepilot_dev;',
          ],
          { stdio: 'inherit' },
        );
        if (r.status !== 0) throw new Error('开发库重建失败');
      }
    } else {
      console.log('跳过容器管理（--no-infra）');
    }

    if (opts.admin) ensureNodeModules('frontend/admin-web');
    if (opts.h5) ensureNodeModules('frontend/miniprogram');

    printDebugBanner(env, opts);

    const apiBase = localHttpBase(env.HTTP_ADDR);
    spawnManaged('api', 'cargo', ['run', '-p', 'chargepilot-app', '--', 'serve'], {
      cwd: BACKEND,
      env,
    });
    if (opts.admin) {
      spawnManaged('admin', NPM, ['run', 'dev'], {
        cwd: 'frontend/admin-web',
        env: { ...env, DEV_API_PROXY_TARGET: env.DEV_API_PROXY_TARGET || apiBase },
      });
    }
    if (opts.h5) {
      spawnManaged('h5', NPM, ['run', 'dev:h5'], {
        cwd: 'frontend/miniprogram',
        env: {
          ...env,
          DEV_API_PROXY_TARGET: env.DEV_API_PROXY_TARGET || apiBase,
          // 与后端 LOGIN_MODE 对齐：dev 直登 / wechat 真实授权。
          TARO_APP_LOGIN_MODE: env.LOGIN_MODE === 'wechat' ? 'wechat' : 'development',
        },
      });
    }

    const health = `${localHttpBase(env.HTTP_ADDR)}/health/live`;
    if (await waitForHttp(health, 600000)) {
      console.log(`后端已就绪：${health}`);
      if (opts.admin) console.log('运营后台：http://127.0.0.1:5173/admin/');
      if (opts.h5) console.log('H5 用户端：http://127.0.0.1:5174/');
    } else {
      console.warn('等待后端健康检查超时（进程仍在运行，请查看 [api] 日志）');
    }
  },
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
    console.log('任务：build | test | fmt | fmt-check | check-sql | check | migrate | test-integration [fresh] | simulator | simulator-batch | debug [--no-admin] [--no-h5] [--no-infra] [--fresh] [--clean] | db-up | db-down | redis-up | redis-down');
    console.log('debug：一键拉起开发环境（后端 + 两个前端 + dev 容器），Ctrl-C 统一停止；详细选项用 node tasks.mjs debug --help');
  },
};

// 信号统一交给 shutdown（仅 debug 会注册处理器；其余任务是同步短命令）。
const entry = tasks[task] ?? tasks.help;
Promise.resolve()
  .then(() => entry())
  .catch((err) => {
    console.error(`[tasks] ${err instanceof Error ? err.message : err}`);
    if (children.length > 0) void shutdown(1);
    else process.exit(1);
  });
