// 魔法学院召唤逻辑单测：从 index.html 抽出 <summon> 段落单独执行，不需要浏览器。
// 运行：node --test tests/*.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const html = readFileSync(new URL("../index.html", import.meta.url), "utf8");
const m = html.match(/\/\/ <summon>[^\n]*\n([\s\S]*?)\/\/ <\/summon>/);
assert.ok(m, "index.html 里找不到 <summon> 段落");
const { summonTarget, summonStage, fireAt, ringOrder, orbStatus } = new Function(m[1] + "\nreturn {summonTarget, summonStage, fireAt, ringOrder, orbStatus};")();

const near = (a, b, msg) => assert.ok(Math.abs(a - b) < 1e-9, `${msg || ""} ${a} ≠ ${b}`);

test("没人施法 = 0，不管总人数是多少", () => {
  for (const total of [0, 1, 3, 5, 12]) assert.equal(summonTarget(0, total), 0, `total=${total}`);
  assert.equal(summonTarget(-1, 3), 0);
  assert.equal(summonTarget(NaN, 3), 0);
});

test("一个人施法龙就探头：恰好 .45，和总人数无关", () => {
  for (const total of [1, 2, 3, 5, 6, 30]) near(summonTarget(1, total), .45, `total=${total}`);
});

test("施法的人越多越深：单调不减、不超过 1、凑够 5 人（或全员）封顶", () => {
  for (let total = 1; total <= 12; total++) {
    let prev = 0;
    for (let w = 0; w <= total; w++) {
      const v = summonTarget(w, total);
      assert.ok(v >= prev, `total=${total} w=${w}: ${v} < ${prev}`);
      assert.ok(v <= 1, `total=${total} w=${w}: ${v} > 1`);
      prev = v;
    }
    // 只有一个人时没有"更多的人"可比，就停在探头那一档
    near(summonTarget(Math.min(total, 5), total), total === 1 ? .45 : 1, `total=${total} 封顶`);
  }
  // 人很多时按 5 人算跨度，不会因为总人数大了而变慢
  near(summonTarget(3, 30), summonTarget(3, 5));
  assert.equal(summonTarget(99, 30), 1);
});

test("总人数不足 2 时跨度取 1，不会除零", () => {
  assert.ok(Number.isFinite(summonTarget(1, 1)));
  assert.ok(Number.isFinite(summonTarget(1, 0)));
  near(summonTarget(2, 2), 1);
});

test("阶段边界：.05 / .3 / .8 各属于更高的一档", () => {
  assert.equal(summonStage(0), 0);
  assert.equal(summonStage(.0499), 0);
  assert.equal(summonStage(.05), 1);
  assert.equal(summonStage(.2999), 1);
  assert.equal(summonStage(.3), 2);
  assert.equal(summonStage(.7999), 2);
  assert.equal(summonStage(.8), 3);
  assert.equal(summonStage(1), 3);
});

test("一个人施法 = 阶段 2（龙头探入但不喷火），凑够人手才到阶段 3", () => {
  assert.equal(summonStage(summonTarget(1, 5)), 2);
  assert.equal(summonStage(summonTarget(3, 5)), 2);
  assert.equal(summonStage(summonTarget(4, 5)), 3);
  assert.equal(summonStage(summonTarget(5, 5)), 3);
});

test("吐火：进度不到 .8 恒为 0", () => {
  for (const level of [0, .1, .45, .79, .7999]) for (let t = -25; t <= 45; t += .25) assert.equal(fireAt(level, t), 0, `level=${level} t=${t}`);
});

test("吐火：每 10 秒喷 3 秒，起止各 .4 秒渐变，其余时间为 0", () => {
  assert.equal(fireAt(1, 0), 0);
  near(fireAt(1, .2), .5);
  near(fireAt(1, .4), 1);
  near(fireAt(1, 1.5), 1);
  near(fireAt(1, 2.6), 1);
  near(fireAt(1, 2.8), .5);
  assert.equal(fireAt(1, 3), 0);
  for (const t of [3.01, 4, 5, 9, 9.99]) assert.equal(fireAt(1, t), 0, `t=${t}`);
});

test("吐火：包络在 0..1 之间，按 10 秒循环，负时间也一样", () => {
  for (let t = -30; t <= 60; t += .05) {
    const v = fireAt(.9, t);
    assert.ok(v >= 0 && v <= 1, `t=${t} v=${v}`);
  }
  near(fireAt(1, 10.2), fireAt(1, .2));
  near(fireAt(1, 32.8), fireAt(1, 2.8));
  near(fireAt(1, -9.8), fireAt(1, .2));
  assert.equal(fireAt(.8, 1), 1);
});

test("围阵落位：m 个站位里除 leader 外各用一次，不越出本扇区", () => {
  for (let m = 1; m <= 30; m++) {
    const o = ringOrder(m);
    assert.equal(o.length, m - 1, `m=${m}`);
    assert.equal(new Set(o).size, m - 1, `m=${m} 有重复`);
    for (const j of o) assert.ok(j !== 0 && j >= -Math.floor((m - 1) / 2) && j <= Math.floor(m / 2), `m=${m} j=${j}`);
  }
});

test("围阵落位：人少时散开围住法阵——第一个站 leader 正对面，前三个把整圈大致四等分", () => {
  const o = ringOrder(24);
  assert.equal(o[0], 12);
  assert.deepEqual([...o.slice(1, 3)].sort((a, b) => a - b), [-6, 6]);
  // 任意前缀里相邻两人的间隔都不小于"均分间隔"的一半左右：不会挤在 leader 身边
  for (let n = 1; n <= 23; n++) {
    const pos = [0, ...o.slice(0, n)].map(j => (j + 24) % 24).sort((a, b) => a - b);
    const gaps = pos.map((p, i) => (pos[(i + 1) % pos.length] - p + 24) % 24 || 24);
    assert.ok(Math.min(...gaps) >= Math.floor(24 / (n + 1) / 2), `n=${n} 最小间隔 ${Math.min(...gaps)}`);
  }
});

test("光球状态：有 leader 在进行中就是 working；否则 失败 > 已完成 > 待命 > 已中止；没人 = done", () => {
  const P = (status, boss = true) => ({ status, boss });
  assert.equal(orbStatus([]), "done");
  assert.equal(orbStatus([P("working"), P("failed")]), "working");
  assert.equal(orbStatus([P("finished"), P("failed")]), "failed");
  assert.equal(orbStatus([P("idle"), P("finished")]), "finished");
  assert.equal(orbStatus([P("stopped"), P("idle")]), "idle");
  assert.equal(orbStatus([P("stopped"), P("done")]), "stopped");
  assert.equal(orbStatus([P("done")]), "done");
});

test("光球状态：只看 leader；子 agent 失败不把已完成的会话染红，leader 都不在场时才看其他人", () => {
  const P = (status, boss) => ({ status, boss });
  assert.equal(orbStatus([P("finished", true), P("failed", false)]), "finished");
  assert.equal(orbStatus([P("idle", true), P("working", false)]), "idle");
  assert.equal(orbStatus([P("working", false), P("finished", false)]), "working");
});
