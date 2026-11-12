"""任务分类器单测：JSON 提取、结构校验、解析失败重试。不访问网络。
运行：python3 -m unittest discover -s tests
"""
import io
import json
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
import server  # noqa: E402

TC = server.TaskClassifier


def resp(content):
    return io.BytesIO(json.dumps({"choices": [{"message": {"content": content}}]}).encode())


class FakeOpen:
    """按顺序返回预设内容的 urlopen 替身。"""

    def __init__(self, contents):
        self.contents, self.calls = list(contents), 0

    def __call__(self, req, timeout=None):
        self.calls += 1
        body = resp(self.contents.pop(0))
        body.__enter__ = lambda s=body: s
        body.__exit__ = lambda *a: False
        return body


def clf():
    with mock.patch.dict(os.environ, {"DECK_LLM_URL": "", "DECK_LLM_KEY": ""}):
        c = TC()  # 不启动后台线程
    c.url, c.key, c.on = "http://llm.invalid/v1", "k", True
    return c


class ExtractJson(unittest.TestCase):
    def test_plain(self):
        self.assertEqual(TC.extract_json('{"items": []}'), {"items": []})

    def test_code_fence(self):
        self.assertEqual(TC.extract_json('结果如下：\n```json\n{"items": [{"i": 0, "kind": "ci"}]}\n```'),
                         {"items": [{"i": 0, "kind": "ci"}]})

    def test_text_around(self):
        self.assertEqual(TC.extract_json('好的。{"items": [{"i": 1, "kind": "docs"}]} 以上。'),
                         {"items": [{"i": 1, "kind": "docs"}]})

    def test_skip_broken_brace(self):
        self.assertEqual(TC.extract_json('{坏的 {"items": []}'), {"items": []})

    def test_errors(self):
        for t in ["", None, "   ", "没有 JSON", "[1, 2]", '{"items": [}']:
            with self.assertRaises(ValueError, msg=repr(t)):
                TC.extract_json(t)


class ParseItems(unittest.TestCase):
    def test_normalize(self):
        c, titles = clf(), ["a", "b", "c"]
        got = c.parse_items({"items": [{"i": 0, "kind": "CI"}, {"i": "1", "kind": "library"}, {"i": 9, "kind": "ops"},
                                       {"i": True, "kind": "ops"}, "x"]}, titles)
        self.assertEqual(got, {"a": "ci", "b": "other"})

    def test_bad_structure(self):
        c = clf()
        for o in [{}, {"items": "x"}, {"items": []}, {"items": [{"i": 5, "kind": "ci"}]}]:
            with self.assertRaises(ValueError, msg=repr(o)):
                c.parse_items(o, ["a"])


class AskRetry(unittest.TestCase):
    def test_retry_then_ok(self):
        c = clf()
        fake = FakeOpen(["", "不是 JSON", '```json\n{"items":[{"i":0,"kind":"research"}]}\n```'])
        with mock.patch("urllib.request.urlopen", fake):
            self.assertEqual(c._ask(["查资料"]), {"查资料": "research"})
        self.assertEqual(fake.calls, 3)

    def test_give_up(self):
        c = clf()
        fake = FakeOpen(["", "{坏", '{"items": []}'])
        with mock.patch("urllib.request.urlopen", fake), self.assertRaises(ValueError):
            c._ask(["x"])
        self.assertEqual(fake.calls, TC.TRIES)


class Annotate(unittest.TestCase):
    def test_own_title_only(self):
        c = clf()
        c.cache.update({"CI 修复": "ci", "写说明": "docs"})
        leads = [{"title": "CI 修复", "agents": [{"task": "写说明"}, {"task": "新标题"}]}]
        out = c.annotate(leads)
        self.assertEqual(out[0]["kind"], "ci")
        self.assertEqual([a["kind"] for a in out[0]["agents"]], ["docs", None])
        self.assertIn("新标题", c.pending)
        self.assertNotIn("kind", leads[0])  # 不改原数据

    def test_off(self):
        with mock.patch.dict(os.environ, {"DECK_LLM_URL": "", "DECK_LLM_KEY": ""}):
            c = TC()
        self.assertIsNone(c.kind("CI 修复"))
        self.assertFalse(c.pending)


if __name__ == "__main__":
    unittest.main()
