// 跑马灯位移单测：从 index.html / classic.html 抽出 <ticker> 段落，验证换内容后可见条目留在原地，不需要浏览器。
// 运行：node --test tests/*.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const load = (file) => {
  const html = readFileSync(new URL("../" + file, import.meta.url), "utf8");
  const m = html.match(/\/\/ <ticker>[^\n]*\n([\s\S]*?)\/\/ <\/ticker>/);
  assert.ok(m, file + " 里找不到 <ticker> 段落");
  return new Function(m[1] + "\nreturn tkOffset;")();
};

// 模拟一条跑马灯：items = [[id, 宽度]...]，重复两份，条目间距 GAP；位移 off 时各条在屏幕上的 left。
const GAP = 26;
const lay = (items) => {
  const one = []; let x = 0;
  for (const [id, w] of items) { one.push([id, x, w]); x += w + GAP; }
  return { period: x, all: [...one, ...one.map(([id, l, w]) => [id, l + x, w])] };
};
const visible = (m, off, track) => m.all.map(([id, l, w]) => [id, l - off, w]).filter(([, l, w]) => l + w > 0 && l < track);

for (const file of ["index.html", "classic.html"]) {
  const tkOffset = load(file);
  // 按页面里的做法换内容：旧位移下量出新内容的 left，再算新位移
  const swap = (oldItems, newItems, off, track = 900) => {
    const a = lay(oldItems), b = lay(newItems);
    const seen = visible(a, off, track).map(([id, l]) => [id, l]);
    const next = tkOffset(off, b.period, seen, (id) => { const k = b.all.find((x) => x[0] === id); return k ? k[1] - off : null; });
    return { seen, next, after: visible(b, next, track), period: b.period };
  };
  const A = [["a", 200], ["b", 260], ["c", 180], ["d", 240], ["e", 220], ["f", 210]];

  test(`${file}: 头部插入新事件、尾部挤掉最旧的，可见条目原地不动`, () => {
    const r = swap(A, [["n1", 300], ["n2", 150], ...A.slice(0, 5)], 250);
    assert.ok(r.next >= 0 && r.next < r.period);
    for (const [id, left] of r.seen) {
      const hit = r.after.find((x) => x[0] === id && Math.abs(x[1] - left) < 1e-6);
      assert.ok(hit, `${id} 应留在 left=${left}，实际 ${JSON.stringify(r.after)}`);
    }
  });

  test(`${file}: 最左边那条被挤掉时，用下一条还在的做锚点`, () => {
    // 位移滚到第二份的尾部：最左可见的是 f，新内容里没有 f
    const a = lay(A), off = a.period - 230;
    const r = swap(A, [["n1", 300], ...A.slice(0, 5)], off);
    assert.equal(r.seen[0][0], "f");
    const [id, left] = r.seen[1];
    assert.ok(r.after.some((x) => x[0] === id && Math.abs(x[1] - left) < 1e-6), `${id} 应留在原地`);
  });

  test(`${file}: 内容没变时位移不变；全部换新时沿用原位移并收进一圈以内`, () => {
    assert.equal(swap(A, A, 333).next, 333);
    const r = swap(A, [["x", 100], ["y", 100]], 1000);
    assert.equal(r.next, 1000 % r.period);
  });

  test(`${file}: 没有事件（一圈长度为 0）时位移归零`, () => {
    assert.equal(tkOffset(123, 0, [["a", 5]], () => 9), 0);
  });
}
