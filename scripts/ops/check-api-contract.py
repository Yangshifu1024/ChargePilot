#!/usr/bin/env python3
"""前后端 API 契约对账（回归门禁）。

对 frontend/admin-web/src 与 frontend/miniprogram/src 的全部 API 调用点与
backend/crates/http-api/src 注册的路由做两段归一化比对，任一缺口退出码 1：

1. 路由路径：前端调用而后端无同形路由即缺口。2026-11-14 人工验收暴露
   23 端点缺口后补建；同日发现初版只扫 admin-web、用户端小程序 15 端点
   断链，扩为两套前端。
2. 请求字段名：后端入参 DTO 的必填字段必须出现在前端发送的字段里。
   2026-11-15 真机验收暴露 3 处字段名错位（port_id/port_code、provider
   /mode）后补建——集成测试自己拼请求体、与服务端共享同一套（错误）字段
   名，抓不住这类回归，只有真实前端能让两者相撞。axum/serde 默认忽略未
   知字段，前端多发无害；body 是变量、URL 拼接无法归一、DTO 含 flatten
   或找不到定义时跳过该调用点/路由，宁漏不误。
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

# 后端路由注册：单行与多行两种形态都由 \s* 跨行覆盖。
ROUTE_RE = re.compile(
    r'\.route\(\s*"(/api/v1/[^"]+)"\s*,\s*'
    r'axum::routing::(get|post|put|delete)\(\s*(\w+)')

def norm_fe(e: str) -> str:
    e = e.split('?')[0].rstrip('/')
    e = re.sub(r'\$\{[^}]*\}', '{id}', e)
    e = re.sub(r'/\{id\}(?:' + TAILS + r')$', r'{id}/\1', e)
    return e

def norm_be(b: str) -> str:
    b = re.sub(r'\{[^}]*\}', '{id}', b).strip('"').rstrip('/')
    return b

# ---------- 第二段：请求字段名对账 ----------

# admin-web：apiPost/apiPut/apiDelete('直串' 或 `模板串`，body)。
ADMIN_CALL_RE = re.compile(r"""api(?:Post|Put|Delete)\(\s*(['"`])(/api/v1/[^'"`]*)\1""")
# miniprogram：app.request / this.rawRequest / 裸 request('POST'|'PUT', '/user/...', body)。
# 大小写不敏感以同时命中 request 与 rawRequest；GET 无 body 不参与对账。
MP_CALL_RE = re.compile(r"""request\(\s*'(POST|PUT)'\s*,\s*'(/user/[^']*)'\s*,\s*""", re.I)

def balanced(text: str, i: int, open_ch='{', close_ch='}') -> int:
    """i 指向 open_ch，返回配对 close_ch 之后的下标；跳过字符串与模板串。"""
    depth = 0
    n = len(text)
    while i < n:
        c = text[i]
        if c in '"\'`':
            q = c
            i += 1
            while i < n and text[i] != q:
                i += 2 if text[i] == '\\' else 1
        elif c == open_ch:
            depth += 1
        elif c == close_ch:
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    return None

def literal_after(text: str, pos: int) -> str:
    """取调用点第一实参：对象字面量返回其文本；无参/变量/表达式返回 None。

    逗号可有可无：MP_CALL_RE 尾部已消费 'URL' 后的逗号，直接落在 '{' 上；
    ADMIN_CALL_RE 停在 URL 闭引号，还要先跳过逗号。
    """
    n = len(text)
    i = pos
    while i < n and text[i].isspace():
        i += 1
    if i < n and text[i] == ',':
        i += 1
        while i < n and text[i].isspace():
            i += 1
    if i < n and text[i] == '{':
        j = balanced(text, i)
        if j is not None:
            return text[i:j]
    return None

def body_fields(obj: str) -> set:
    """对象字面量里的字段名（超集方向：多收无害，少收才误报）。"""
    # 先删字符串与模板串内容（注释里的引号残留由随后注释删除兜底）。
    obj = re.sub(r'`[^`]*`|\'(?:[^\'\\]|\\.)*\'|"(?:[^"\\]|\\.)*"', ' ', obj)
    obj = re.sub(r'//[^\n]*', ' ', obj)
    fields = set(re.findall(r'([A-Za-z_$][\w$]*)\s*:', obj))
    # 简写属性 {code} 等价 code: code（小程序登录调用即此形态）。
    fields.update(re.findall(r'[{,]\s*([A-Za-z_$][\w$]*)\s*[},]', obj))
    return fields

def fe_field_calls(path: str, text: str) -> list:
    """可解析的带 body 调用点：(归一路由, 字段集, 文件:行)。"""
    calls = []
    rel = os.path.relpath(path, ROOT)
    for m in ADMIN_CALL_RE.finditer(text):
        if text[m.end():].lstrip().startswith('+'):
            continue  # URL 字符串拼接，归一不出整条路径
        obj = literal_after(text, m.end())
        if obj is None:
            continue  # body 是变量/无参，提取不了字段名
        line = text.count('\n', 0, m.start()) + 1
        calls.append((norm_fe(m.group(2)), body_fields(obj), f'{rel}:{line}'))
    for m in MP_CALL_RE.finditer(text):
        obj = literal_after(text, m.end())
        if obj is None:
            continue
        line = text.count('\n', 0, m.start()) + 1
        calls.append((norm_fe('/api/v1' + m.group(2)), body_fields(obj), f'{rel}:{line}'))
    return calls

def handler_dto(text: str, handler: str) -> str:
    """handler 的请求 DTO 名；解析不出（无 body / 动态 Value 裸取）返回 None。

    三种关联形态：①签名 typed 提取器 AxumJson<Dto>/axum::Json<Dto>；
    ②签名拿 Value/Bytes、体里 decode_strict::<Dto>/from_slice::<Dto>/
    from_value::<Dto>（settlement billing 域的严格解码）；③类型注解
    let x: Dto = from_value(...)（仅 1 处且 DTO 在本 crate 外，靠
    struct_body 找不到定义自然跳过）。domain/modules 类型同理跳过。
    """
    m = re.search(r'(?:async\s+)?fn\s+' + re.escape(handler) + r'\b', text)
    if not m:
        return None
    i = text.index('(', m.end())
    j = balanced(text, i, '(', ')')
    if j is None:
        return None
    dm = re.search(r'(?:AxumJson|\baxum::Json|\bJson)<(\w+)>', text[i:j])
    if dm and dm.group(1) != 'Value':
        return dm.group(1)
    # handler 体：签名闭括号到下一个 fn 定义（无则 +4000 字符兜底）。
    nxt = re.search(r'\n(?:async\s+)?fn\s', text[j:])
    end = j + nxt.start() if nxt else j + 4000
    dm = re.search(r'(?:decode_strict|from_slice|from_value)::<(\w+)>', text[j:end])
    return dm.group(1) if dm else None

def struct_body(text: str, name: str) -> str:
    """struct 定义的 braces 内容；找不到定义（如 Value/别名）返回 None。"""
    m = re.search(r'struct\s+' + re.escape(name) + r'\b[^{;]*\{', text)
    if not m:
        return None
    j = balanced(text, m.end() - 1)
    return text[m.end():j - 1] if j is not None else None

def parse_struct_fields(body: str) -> dict:
    """DTO 字段：wire 名 → 是否必填。含 flatten（字段集不可判定）返回 None。"""
    body = re.sub(r'//[^\n]*', '', body)
    body = re.sub(r'/\*.*?\*/', '', body, flags=re.S)
    fields: dict = {}
    for m in re.finditer(r'((?:#\[[^\]]*\]\s*)+)?([a-z_]\w*)\s*:\s*([^,{}\n]+)', body):
        attrs, name, ty = (m.group(1) or ''), m.group(2), m.group(3)
        if 'flatten' in attrs:
            return None
        wire = name
        rm = re.search(r'rename\s*=\s*"([^"]+)"', attrs)
        if rm:
            wire = rm.group(1)
        optional = bool(re.match(r'\s*Option<', ty)) or bool(re.search(r'\bdefault\b', attrs))
        fields[wire] = not optional
    return fields

def be_request_dtos() -> dict:
    """归一路由 → [DTO 名, 必填 wire 字段集]。无 body DTO 的路由不入表。"""
    files = {}
    for f in glob.glob(BE_SRC + '/**/*.rs', recursive=True):
        files[f] = open(f).read()
    all_text = '\n'.join(files.values())
    dto_map: dict = {}
    for text in files.values():
        for m in ROUTE_RE.finditer(text):
            url, handler = m.group(1), m.group(3)
            dto = handler_dto(text, handler) or next(
                (handler_dto(t, handler) for t in files.values() if t is not text), None)
            if not dto:
                continue
            body = struct_body(all_text, dto)
            if body is None:
                continue
            fields = parse_struct_fields(body)
            if fields is None:
                continue
            required = {k for k, req in fields.items() if req}
            if not required:
                continue
            entry = dto_map.setdefault(norm_be(url), [dto, set()])
            entry[1] |= required
    return dto_map

def check_fields(calls: list, dto_map: dict):
    """逐调用点核对：后端必填字段 ⊆ 前端发送字段。"""
    missing = []
    checked = 0
    for url, fields, loc in calls:
        hit = dto_map.get(url)
        if not hit:
            continue  # URL 归一不进必填字段表：路由对账管，字段段跳过
        checked += 1
        lack = sorted(hit[1] - fields)
        if lack:
            missing.append((url, hit[0], lack, loc))
    return missing, checked

def main() -> int:
    fe = set()
    calls = []
    for fe_src in FE_DIRS:
        for f in glob.glob(fe_src + '/**/*.tsx', recursive=True) + glob.glob(fe_src + '/**/*.ts', recursive=True) + glob.glob(fe_src + '/**/*.js', recursive=True):
            text = open(f).read()
            # admin-web 写完整 /api/v1/...；miniprogram 以 TARO_APP_API_BASE
            # （默认 /api/v1）拼接，源码里只出现 /user/...，统一补前缀。
            fe.update(re.findall(r"""['"`](/api/v1/[^'"`]*)""", text))
            fe.update('/api/v1' + m for m in re.findall(r"""['"`](/user/[^'"`]*)""", text))
            calls += fe_field_calls(f, text)
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

    dto_map = be_request_dtos()
    field_missing, checked = check_fields(calls, dto_map)
    for url, dto, lack, loc in field_missing:
        print(f'FIELD_MISSING: {url} 后端 {dto} 必填 {lack}，前端调用点 {loc} 未发送')

    print(f'\n前端调用点 {len(fe_n)} 个，后端路由 {len(be_n)} 个，缺口 {len(missing)} 个')
    print(f'请求字段对账：带必填字段的路由 {len(dto_map)} 个，核对前端调用点 {checked} 个，缺口 {len(field_missing)} 个')
    return 1 if (missing or field_missing) else 0

if __name__ == '__main__':
    sys.exit(main())
