#!/usr/bin/env python3
"""前后端 API 契约对账（回归门禁）。

对 frontend/admin-web/src 与 frontend/miniprogram/src 的全部 API 调用点与
backend/crates/http-api/src 注册的路由做归一化比对：前端调用而后端无同形
路由即缺口，退出码 1。背景：阶段验收只覆盖"已实现端点的行为"，本脚本
守住"前端要的都存在"（2026-11-14 人工验收暴露 23 端点缺口后补建；
同日又发现初版只扫 admin-web、用户端小程序 15 端点断链，扩为两套前端）。
"""
import re, glob, subprocess, sys, os

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
FE_DIRS = [
    os.path.join(ROOT, 'frontend', 'admin-web', 'src'),
    os.path.join(ROOT, 'frontend', 'miniprogram', 'src'),
]
BE_SRC = os.path.join(ROOT, 'backend', 'crates', 'http-api', 'src')

TAILS = ('(status|unlock|reset-password|mfa|retry|grants|stats|decide|propose|'
         'review|release|inherit|charging-scheme|process|timeline|refunds|'
         'recharges|ack|dispatch|history|resolve|download|reply|read|curve|'
         'snapshot|bind|unbind|feedback|ongoing)')

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
    for fe_src in FE_DIRS:
        for f in glob.glob(fe_src + '/**/*.tsx', recursive=True) + glob.glob(fe_src + '/**/*.ts', recursive=True) + glob.glob(fe_src + '/**/*.js', recursive=True):
            text = open(f).read()
            # admin-web 写完整 /api/v1/...；miniprogram 以 TARO_APP_API_BASE
            # （默认 /api/v1）拼接，源码里只出现 /user/...，统一补前缀。
            fe.update(re.findall(r"""['"`](/api/v1/[^'"`]*)""", text))
            fe.update('/api/v1' + m for m in re.findall(r"""['"`](/user/[^'"`]*)""", text))
    fe_n = sorted({norm_fe(e) for e in fe})

    out = subprocess.run(
        ['grep', '-rhoE', '"/api/v1/[^"]*"', BE_SRC], capture_output=True, text=True)
    be = set(re.findall(r'"/api/v1/[^"]*"', out.stdout))
    be_n = {norm_be(b) for b in be}

    missing = []
    for e in fe_n:
        if e in be_n:
            continue
        # 小程序以字符串拼接构造子路径（如 '/user/charge/' + orderNo），
        # 提取出裸前缀；后端存在该前缀下的动态子路由即视为已覆盖。
        if any(b.startswith(e + '/{') for b in be_n):
            continue
        missing.append(e)
    for e in missing:
        print('MISSING:', e)
    print(f'\n前端调用点 {len(fe_n)} 个，后端路由 {len(be_n)} 个，缺口 {len(missing)} 个')
    return 1 if missing else 0

if __name__ == '__main__':
    sys.exit(main())
