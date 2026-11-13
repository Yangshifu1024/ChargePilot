// 非法迁移编译期拒绝的自动化证明（阶段 00 验收项）。
// trybuild 编译 tests/compile_fail/*.rs，预期失败并比对错误信息。
#[test]
fn illegal_transitions_fail_to_compile() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/*.rs");
}
