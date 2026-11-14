#!/usr/bin/env python3
"""前后端 API 契约对账（回归门禁）。

对 frontend/admin-web/src 的全部 API 调用点与 backend/crates/http-api/src
注册的路由做归一化比对：前端调用而后端无同形路由即缺口，退出码 1。
背景：阶段验收只覆盖"已实现端点的行为"，本脚本守住"前端要的都存在"
（2026-11-14 人工验收暴露 23 端点缺口后补建）。
"""
import re, glob, subprocess, sys, os

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
FE_SRC = os.path.join(ROOT, 'frontend', 'admin-web', 'src')
BE_SRC = os.path.join(ROOT, 'backend', 'crates', 'http-api', 'src')

TAILS = ('(status|unlock|reset-password|mfa|retry|grants|stats|decide|propose|'
         'review|release|inherit|charging-scheme|process|timeline|refunds|'
         'recharges|ack|dispatch|history|resolve|download|reply)')

def norm_fe(e: str) -> str:
    e = e.split('?')[0].rstrip('/')
    e = re.sub(r'\$\{[^}]*\}', '{id}', e)
    e = re.sub(r'/\{id\}(?:' + TAILS + r')$', r'{id}/\1', e)
    return e

def norm_be(b: str) -> str:
    b = re.sub(r'\{[^}]*\}', '{id}', b).strip('"').rstrip('/')
    return b

def main() -> int:
    fe = set()
    for f in glob.glob(FE_SRC + '/**/*.tsx', recursive=True) + glob.glob(FE_SRC + '/**/*.ts', recursive=True):
        fe.update(re.findall(r"""['"`](/api/v1/[^'"`]*)""", open(f).read()))
    fe_n = sorted({norm_fe(e) for e in fe})

    out = subprocess.run(
        ['grep', '-rhoE', '"/api/v1/[^"]*"', BE_SRC], capture_output=True, text=True)
    be = set(re.findall(r'"/api/v1/[^"]*"', out.stdout))
    be_n = {norm_be(b) for b in be}

    missing = [e for e in fe_n if e not in be_n]
    for e in missing:
        print('MISSING:', e)
    print(f'\n前端调用点 {len(fe_n)} 个，后端路由 {len(be_n)} 个，缺口 {len(missing)} 个')
    return 1 if missing else 0

if __name__ == '__main__':
    sys.exit(main())
