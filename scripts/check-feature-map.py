#!/usr/bin/env python3
"""Validate the hand-maintained static map docs/feature-map-static.md.

docs/feature-map.md is the derived, record-driven map shipped by PR #82
(GET /api/features); it holds no hand-written status and is not checked here.
The static FM-01..FM-19 capability audit moved to feature-map-static.md when
the two add/add histories merged.
"""

import re
import sys
from functools import lru_cache
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DOC = ROOT / "docs/feature-map-static.md"
FIELDS = ("能力", "入口", "代码", "实现状态", "测试证据", "部署状态", "已知缺口", "下一步")
IMPLEMENTATION = ("已实现", "部分实现", "未实现", "已知缺陷")
DEPLOYMENT = ("部署未验证", "部署已验证", "未部署", "不适用")
REQUIRED_IDS = {f"FM-{number:02d}" for number in range(1, 20)}
REQUIRED_TEXT = (
    "mission", "objective", "task", "PostgreSQL", "resident", "worktree", "tmux",
    "Claude Code", "gate", "integrator", "fleet", "alerts", "deploy", "数据工程",
    "后训练文档", "cancel-notice", "目标运行环境", "feature-map-maintainer", "派活先读", "完成回写",
)
REF = re.compile(r"`([A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)+):(\d+)(?:-(\d+))?`")


@lru_cache(maxsize=None)
def line_count(relative):
    path = (ROOT / relative).resolve()
    if ROOT not in path.parents or not path.is_file():
        return None
    return len(path.read_text(encoding="utf-8").splitlines())


def validate(text):
    errors = []
    for token in REQUIRED_TEXT:
        if token not in text:
            errors.append(f"missing required coverage: {token}")
    for private in ("/Users/", "/home/", "/root/", "/private/"):
        if private in text:
            errors.append(f"private machine path: {private}")

    parts = re.split(r"(?m)^### (FM-\d+) ", text)
    identifiers = parts[1::2]
    blocks = parts[2::2]
    entries = {}
    for missing in sorted(REQUIRED_IDS - set(identifiers)):
        errors.append(f"missing entry: {missing}")
    for unknown in sorted(set(identifiers) - REQUIRED_IDS):
        errors.append(f"unknown entry: {unknown}")
    for identifier, block in zip(identifiers, blocks):
        if identifier in entries:
            errors.append(f"duplicate entry: {identifier}")
        entries[identifier] = block
        for field in FIELDS:
            if f"- {field}：" not in block:
                errors.append(f"{identifier}: missing field {field}")
        if not re.search(r"^- 实现状态：`(?:" + "|".join(IMPLEMENTATION) + r")`", block, re.M):
            errors.append(f"{identifier}: invalid implementation status")
        if not re.search(r"^- 部署状态：`(?:" + "|".join(DEPLOYMENT) + r")`", block, re.M):
            errors.append(f"{identifier}: invalid deployment status")
        deployment = re.search(r"(?m)^- 部署状态：(.*)$", block)
        if deployment and "`部署已验证`" in deployment.group(1):
            for marker in ("环境：", "commit：", "命令：", "结果：", "日期："):
                if marker not in deployment.group(1):
                    errors.append(f"{identifier}: verified deployment missing {marker}")
        code = re.search(r"(?m)^- 代码：(.*)$", block)
        if code and not REF.search(code.group(1)):
            errors.append(f"{identifier}: no code file:line reference")
        tests = re.search(r"(?m)^- 测试证据：(.*)$", block)
        if tests and not REF.search(tests.group(1)) and "未找到" not in tests.group(1):
            errors.append(f"{identifier}: no test reference or explicit absence")
    for identifier in ("FM-18", "FM-19"):
        block = entries.get(identifier, "")
        if "未修复" not in block or "目标运行环境" not in block:
            errors.append(f"{identifier}: must distinguish open code defect from 目标运行环境 runtime")

    references = list(REF.finditer(text))
    for match in references:
        relative, first, last = match.group(1), int(match.group(2)), int(match.group(3) or match.group(2))
        count = line_count(relative)
        if count is None:
            errors.append(f"missing reference file: {relative}")
        elif first < 1 or last < first or last > count:
            errors.append(f"bad reference: {relative}:{first}-{last} (file has {count} lines)")
    return errors, len(references)


def self_test(text):
    errors, _ = validate(text)
    assert not errors, "valid document failed: " + "; ".join(errors)

    bad_file = validate(text.replace("### FM-01 ", "`missing/file.rs:1`\n\n### FM-01 ", 1))[0]
    assert any("missing/file.rs" in error for error in bad_file), "missing file was accepted"

    changed = REF.sub(lambda match: f"`{match.group(1)}:{match.group(2)}-999999`", text, count=1)
    bad_line = validate(changed)[0]
    assert any("999999" in error for error in bad_line), "bad line range was accepted"

    changed = text.replace("- 已知缺口：", "- 旧缺口：", 1)
    bad_field = validate(changed)[0]
    assert any("FM-01: missing field 已知缺口" in error for error in bad_field), "missing field was accepted"

    changed, count = re.subn(r"(?m)^- 实现状态：`(?:已实现|部分实现|未实现|已知缺陷)`", "- 实现状态：`已修复`", text, count=1)
    assert count == 1, "implementation status negative was not constructed"
    bad_status = validate(changed)[0]
    assert any("FM-01: invalid implementation status" in error for error in bad_status), "bad status was accepted"

    duplicate = validate(text.replace("### FM-02 ", "### FM-01 ", 1))[0]
    assert any("duplicate entry: FM-01" in error for error in duplicate), "duplicate ID was accepted"

    private = validate(text + "\n/root/private/checkout\n")[0]
    assert any("private machine path" in error for error in private), "private path was accepted"

    changed, count = re.subn(r"(?m)^- 部署状态：`部署未验证`", "- 部署状态：`部署已验证`", text, count=1)
    assert count == 1, "deployment negative was not constructed"
    missing_metadata = validate(changed)[0]
    assert any("verified deployment missing" in error for error in missing_metadata), "missing deployment metadata was accepted"
    print("feature-map self-test: 7 negative cases, OK")


def main():
    text = DOC.read_text(encoding="utf-8")
    if sys.argv[1:] == ["--self-test"]:
        self_test(text)
        return 0
    if sys.argv[1:]:
        print(f"usage: {Path(sys.argv[0]).name} [--self-test]", file=sys.stderr)
        return 2
    errors, references = validate(text)
    if errors:
        print("\n".join(f"feature-map: {error}" for error in errors), file=sys.stderr)
        return 1
    entries = len(re.findall(r"(?m)^### FM-\d+ ", text))
    print(f"feature-map: {entries} entries, {references} references, OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
