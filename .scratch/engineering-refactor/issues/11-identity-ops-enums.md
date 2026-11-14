# 11: identity 与 ops 域枚举化（S3f）

**What to build:** 身份与运营两域词表转正：账号状态（两套取值集合：C 端 active/frozen 与后台 active/disabled/locked）、公告状态（草稿/已发布/已过期）、订单评价类别等。注意两套账号状态是不同表的独立词表，不合并。

**Blocked by:** 05

Status: ready-for-agent

- [ ] 两套账号状态分别枚举化（不合并），公告状态、评价类别转正
- [ ] 字面量对照测试锁定；常量串容器删除
- [ ] 日常 check + 全量集成轮跑绿；字面量清单快照只减不增
