// 模型显示名单测：从各页面抽出 <model> 段落，验证代理回报的各种 id 归一到统一显示名。
// 运行：node --test tests/*.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const load = (file) => {
  const html = readFileSync(new URL("../" + file, import.meta.url), "utf8");
  const m = html.match(/\/\/ <model>[^\n]*\n([\s\S]*?)\/\/ <\/model>/);
  assert.ok(m, file + " 里找不到 <model> 段落");
  return new Function(m[1] + "\nreturn { modelName, modelFull };")();
};

for (const file of ["index.html", "office.html", "classic.html"]) {
  const { modelName, modelFull } = load(file);
  test(file + "：Qwen 各种写法归一为 Qwen Max", () => {
    for (const id of ["qwen-max", "qwen4-max", "qwen-max[1m]", "Qwen4-Max", "qwen4.0-max-preview"]) assert.equal(modelName(id), "Qwen Max", id);
    assert.equal(modelName("qwen3.8-flash"), "Qwen Flash");
  });
  test(file + "：DeepSeek 各种写法归一为 DeepSeek", () => {
    for (const id of ["deepseek", "deepseek[1m]", "deepseek-v4.1-flash-1017", "DeepSeek-Flash"]) assert.equal(modelName(id), "DeepSeek", id);
  });
  test(file + "：Claude 保持原有缩写，空值为空串", () => {
    assert.equal(modelName("claude-opus-5-5"), "opus-5.5");
    assert.equal(modelName(null), "");
  });
  test(file + "：详情显示名后附原始 id", () => {
    assert.equal(modelFull("qwen4-max"), "Qwen Max（qwen4-max）");
    assert.equal(modelFull("gpt-sol"), "gpt-sol");
  });
}
