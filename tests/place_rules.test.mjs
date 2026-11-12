// 去处规则单测：从 index.html 抽出 <place-rules> 段落单独执行，不需要浏览器。
// 运行：node --test tests/
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const html = readFileSync(new URL("../index.html", import.meta.url), "utf8");
const m = html.match(/\/\/ <place-rules>[^\n]*\n([\s\S]*?)\/\/ <\/place-rules>/);
assert.ok(m, "index.html 里找不到 <place-rules> 段落");
const { placeKind, isCiTask } = new Function(m[1] + "\nreturn {placeKind, isCiTask};")();

const sub = (o) => ({ boss: false, status: "working", type: "implementer", task: "", kind: null, ...o });
const boss = (o) => ({ boss: true, status: "working", type: "lead", title: "", kind: null, ...o });

test("有分类结果时：子 agent 按类型去对应场景", () => {
  for (const k of ["code", "docs", "other"]) assert.equal(placeKind(sub({ kind: k })), null, k);
  for (const k of ["research", "review", "plan"]) assert.equal(placeKind(sub({ kind: k })), "meet", k);
  for (const k of ["ci", "ops"]) assert.equal(placeKind(sub({ kind: k })), "srv", k);
});

test("分类结果优先于类型名", () => {
  assert.equal(placeKind(sub({ type: "advisor", kind: "code" })), null);
  assert.equal(placeKind(sub({ type: "helper", kind: "research" })), "meet");
  assert.equal(placeKind(sub({ type: "implementer", kind: "ops" })), "srv");
});

test("主管只去机房，其余守在课长席", () => {
  assert.equal(placeKind(boss({ kind: "ci" })), "srv");
  assert.equal(placeKind(boss({ kind: "ops" })), "srv");
  for (const k of ["research", "review", "plan", "code"]) assert.equal(placeKind(boss({ kind: k })), null, k);
});

test("不在工作中时不按类型走", () => {
  assert.equal(placeKind(sub({ status: "finished", kind: "ops" })), "lounge");
  assert.equal(placeKind(sub({ status: "failed", kind: "ops" })), null);
  assert.equal(placeKind(boss({ status: "idle", kind: "ci" })), null);
});

test("没有分类结果时退回本地规则：只看自己的标题", () => {
  assert.equal(placeKind(sub({ task: "改写 CI 缓存分层" })), "srv");
  assert.equal(placeKind(boss({ title: "CI 镜像缓存升级" })), "srv");
  // 子 agent 不看所属会话的标题
  assert.equal(placeKind(sub({ task: "调整缓存键", title: "CI 镜像缓存升级" })), null);
  assert.equal(placeKind(sub({ type: "advisor", task: "评审锁粒度" })), "meet");
  assert.equal(placeKind(sub({ type: "helper", task: "批量替换链接" })), "srv");
  assert.equal(placeKind(sub({ task: "补齐单测" })), null);
});

test("本地规则里的 CI 只按独立的词识别", () => {
  for (const t of ["CI", "ci: 缓存分层", "修复 CI", "跑CI", "CI/CD 流水线", "GitHub CI 失败", "(ci)"]) assert.ok(isCiTask(sub({ task: t })), t);
  for (const t of ["", "decision log", "CICD", "Circle 配置", "specific", "acid"]) assert.ok(!isCiTask(sub({ task: t })), t);
});
