#!/usr/bin/env node
// ChargePilot 任务入口（沿用旧仓库 tasks.mjs 习惯，make 无依赖版本）
// 用法：node tasks.mjs <task>
import { spawnSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';

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

const tasks = {
  build() { run('cargo', ['build', '--workspace']); },
  test() { run('cargo', ['test', '--workspace']); },
  fmt() { run('cargo', ['fmt', '--all']); },
  'fmt-check'() { run('cargo', ['fmt', '--all', '--', '--check']); },
  check() {
    tasks['fmt-check']();
    tasks.test();
  },
  migrate() { run('cargo', ['run', '-p', 'chargepilot-app', '--', 'migrate'], { env: envOf() }); },
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
    console.log('任务：build | test | fmt | fmt-check | check | migrate | db-up | db-down | redis-up | redis-down');
  },
};

(tasks[task] ?? tasks.help)();
