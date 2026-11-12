// 小人路线单测：从 index.html 抽出 <routes> 段落，用一间简化的办公室验证寻路规则，不需要浏览器。
// 运行：node --test tests/*.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const html = readFileSync(new URL("../index.html", import.meta.url), "utf8");
const m = html.match(/\/\/ <routes>[^\n]*\n([\s\S]*?)\/\/ <\/routes>/);
assert.ok(m, "index.html 里找不到 <routes> 段落");
const R = new Function(m[1] + "\nreturn {routeOf, posAt, reroute, remapS, shiftSpot, legIn};")();

// 简化办公室：主通道中线 y = 10，枢纽在 (6, 10)；电梯在左边核心筒里。
// 上排工位 desk 在 x = 12 的岛旁通道进出；休息区沙发 sofa 在主通道下方 x = 30 处；机房 srv 从枢纽沿核心筒竖着走。
const HUB = [6, 10];
const mk = (geo) => {
  const my = geo.mainY, sh = geo.BD1 - 9, dD = geo.D - 20;
  return {
    geo: [geo],
    entry: [[[0.2, 5], [3.4, 5], [3.4, 5.1], [6, 5.1], [6, my]]],
    desk: { id: "L|a1", oi: 0, p: [11, 6], approach: [[6, my], [12, my], [12, 6], [11, 6]] },
    desk2: { id: "L|a2", oi: 0, p: [17, 16 + dD], approach: [[6, my], [16, my], [16, 16 + dD], [17, 16 + dD]] },
    sofa: { id: "0:lounge:0", kind: "lounge", oi: 0, p: [28, 12.35 + sh], approach: [[6, my], [30.3, my], [30.3, 12.83 + sh], [28, 12.83 + sh], [28, 12.35 + sh]] },
    srv: { id: "0:srv:0", kind: "srv", oi: 0, p: [4, 13], approach: [[6, my], [6, 13], [4, 13]] },
  };
};
const W = mk({ BD1: 9, D: 20, W: 40, mainY: 10 });
const close = (a, b, msg) => assert.ok(Math.abs(a - b) < 1e-6, `${msg}: ${a} ≠ ${b}`);
const axisAligned = (path) => path.every((q, i) => !i || Math.abs(q[0] - path[i - 1][0]) < 1e-9 || Math.abs(q[1] - path[i - 1][1]) < 1e-9);
const spots = [null, W.desk, W.desk2, W.sofa, W.srv];

test("任意两点之间的路线都是沿通道的横平竖直折线，首尾就是起点和终点", () => {
  for (const a of spots) for (const b of spots){
    if (!a && !b) continue;
    const { path } = R.routeOf(W, 0, a, b);
    assert.ok(axisAligned(path), `${a?.id} → ${b?.id} 出现斜线`);
    assert.deepEqual(path[0], a ? a.p : W.entry[0][0]);
    assert.deepEqual(path[path.length - 1], b ? b.p : W.entry[0][0]);
  }
});

test("工位 → 休息区沿主通道直走，不先绕回电梯口", () => {
  const { path } = R.routeOf(W, 0, W.desk, W.sofa);
  assert.ok(Math.min(...path.map((q) => q[0])) >= 11, "路线跑到了工位左边（往电梯方向）");
  assert.ok(!path.some((q) => q[0] === HUB[0] && q[1] === HUB[1]), "路线经过了电梯口的枢纽");
});

test("回电梯、去机房仍经过枢纽", () => {
  for (const [a, b] of [[W.sofa, null], [W.desk, W.srv]]){
    const { path } = R.routeOf(W, 0, a, b);
    assert.ok(path.some((q) => q[0] === HUB[0] && q[1] === HUB[1]));
  }
});

test("走到一半换目标：位置连续，只沿原路或主通道改道", () => {
  for (const from of spots) for (const to of spots.filter(Boolean)) for (const dest of spots){
    if (from === to || dest === to) continue;
    const r = R.routeOf(W, 0, from, to), L = r.La + r.Lc + r.Lb;
    for (let k = 0; k <= 40; k++){
      const s = (L * k) / 40, cur = R.posAt(r.path, s);
      const n = R.reroute(W, 0, from, to, s, dest), nr = R.routeOf(W, 0, n.from, dest), now = R.posAt(nr.path, n.s);
      const tag = `${from?.id} → ${to.id} 在 ${s.toFixed(2)} 处改去 ${dest?.id}`;
      close(now.x, cur.x, tag + " x"); close(now.y, cur.y, tag + " y");
      assert.ok(axisAligned(nr.path), tag + " 出现斜线");
    }
  }
});

test("已在到达支路上改道：原路退回主通道", () => {
  const r = R.routeOf(W, 0, W.desk, W.sofa), s = r.La + r.Lc + 1;    // 刚拐进休息区
  const n = R.reroute(W, 0, W.desk, W.sofa, s, null);
  assert.equal(n.from, W.sofa);
  const p = R.routeOf(W, 0, n.from, null).path;
  assert.deepEqual(p.slice(0, 4), [[28, 12.35], [28, 12.83], [30.3, 12.83], [30.3, 10]]);
});

test("场景重建后：行走中的人按同一段同样比例落到新路线上，仍在通道里", () => {
  const W2 = mk({ BD1: 10.2, D: 21.2, W: 40, mainY: 11.2 });
  const sofa2 = W2.sofa, o = R.routeOf(W, 0, W.desk, W.sofa), n = R.routeOf(W2, 0, W2.desk, sofa2);
  // 走在主通道上：重建后应在新的主通道中线上
  const s = o.La + o.Lc / 2, q = R.posAt(n.path, R.remapS(o, n, s));
  close(q.y, 11.2, "主通道 y");
  // 已进休息区：仍在新休息区的支路上
  const s2 = o.La + o.Lc + 0.5, q2 = R.posAt(n.path, R.remapS(o, n, s2));
  close(q2.x, 30.3, "休息区竖道 x");
  assert.ok(q2.y > 11.2 && q2.y < 12.83 + 1.2);
});

test("新场景里已收掉的工位按版面平移换算", () => {
  const a = W.geo[0], b = { BD1: 10.2, D: 21.2, W: 40, mainY: 11.2 };
  const top = R.shiftSpot(W.desk, a, b), bot = R.shiftSpot(W.desk2, a, b), so = R.shiftSpot(W.sofa, a, b);
  assert.deepEqual(top.p, W.desk.p);                     // 上排不动
  assert.deepEqual(top.approach[1], [12, 11.2]);         // 路口跟着主通道
  close(bot.p[1], W.desk2.p[1] + 1.2, "下排贴前窗");
  close(so.p[1], W.sofa.p[1] + 1.2, "休息区跟着主通道");
});
