// 模型名显示单测：从各页面抽出 <model> 段落，验证只改写法、不换名字。
// 运行：node --test tests/*.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const load = (file) => {
  const html = readFileSync(new URL("../" + file, import.meta.url), "utf8");
  const m = html.match(/\/\/ <model>[^\n]*\n([\s\S]*?)\/\/ <\/model>/);
  assert.ok(m, file + " 里找不到 <model> 段落");
  return new Function(m[1] + "\nreturn shortModel;")();
};

const CASES = [
  ["claude-opus-5-5", "Opus 5.5"],
  ["claude-fable-5-2", "Fable 5.2"],
  ["claude-sonnet-5-5[1m]", "Sonnet 5.5 · 1M"],
  ["claude-haiku-4-5-20251001", "Haiku 4.5"],
  ["claude-opus-4", "Opus 4"],
  ["claude-3-5-sonnet-20241022", "Sonnet 3.5"],
  ["qwen-max[1m]", "qwen-max · 1M"],
  ["deepseek[1m]", "deepseek · 1M"],
  ["qwen4-max", "qwen4-max"],
  ["gpt-sol", "gpt-sol"],
  [null, ""],
];

for (const file of ["index.html", "office.html", "classic.html"]) {
  const shortModel = load(file);
  test(file + "：模型名只改写法不换名字", () => {
    for (const [id, want] of CASES) assert.equal(shortModel(id), want, String(id));
  });
}
