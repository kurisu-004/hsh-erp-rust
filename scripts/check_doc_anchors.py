#!/usr/bin/env python3
"""docs/api 内部锚点自检：解析相对 markdown 链接的 `#fragment`，与目标文件标题 slug 比对。

slug 规则按本仓既有锚点的实际写法标定（标定样本）：
  `### GET /api/v2/parts/{part_id}/soft-delete` → `#get-apiv2partspart_idsoft-delete`
  `## 状态机（can_transition_to 白名单）`        → `#状态机can_transition_to-白名单`
  `### 错误码语义变更（20109 / 20101）`          → `#错误码语义变更20109--20101`
即：NFKC 归一 → 去 code fence 反引号 / 图片 / 链接语法 → 转小写并去首尾空白
→ **丢弃所有非「字母数字 / `-` / `_` / 空格」字符**（含 `/` `.` `:` `（）` `—` `，`
以及 CJK 标点）→ 空格逐个转 `-`（不合并，故 `20109 / 20101` 产出 `--`）
→ 重名标题追加 `-1` / `-2`。

用法：python3 scripts/check_doc_anchors.py [docs/api]
退出码：0 = 全部可解析；1 = 存在未解析锚点（打印明细）。
"""
import re
import sys
import unicodedata
from pathlib import Path

LINK_RE = re.compile(r"\[[^\]]*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
FENCE_RE = re.compile(r"^\s*```")
HEADING_RE = re.compile(r"^(#{1,6})\s+(.*?)\s*$")


def slugify(text: str) -> str:
    text = re.sub(r"!\[[^\]]*\]\([^)]*\)", "", text)
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)
    text = re.sub(r"<[^>]*>", "", text)
    text = text.replace("`", "")
    text = unicodedata.normalize("NFKC", text).strip().lower()
    kept = [ch for ch in text if ch.isalnum() or ch in "-_ "]
    return "".join("-" if ch == " " else ch for ch in kept)


def headings(path: Path) -> set[str]:
    slugs: set[str] = set()
    seen: dict[str, int] = {}
    in_fence = False
    for line in path.read_text(encoding="utf-8").splitlines():
        if FENCE_RE.match(line):
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        m = HEADING_RE.match(line)
        if not m:
            continue
        base = slugify(m.group(2))
        n = seen.get(base, 0)
        seen[base] = n + 1
        slugs.add(base if n == 0 else f"{base}-{n}")
    return slugs


def main() -> int:
    root = Path(sys.argv[1] if len(sys.argv) > 1 else "docs/api").resolve()
    files = sorted(root.rglob("*.md"))
    cache = {p: headings(p) for p in files}
    bad: list[tuple[Path, int, str, str]] = []
    total = 0
    for src in files:
        in_fence = False
        for lineno, line in enumerate(src.read_text(encoding="utf-8").splitlines(), 1):
            if FENCE_RE.match(line):
                in_fence = not in_fence
                continue
            if in_fence:
                continue
            for target in LINK_RE.findall(line):
                if target.startswith(("http://", "https://", "mailto:", "#!")):
                    continue
                path_part, _, frag = target.partition("#")
                if not frag:
                    continue
                total += 1
                from urllib.parse import unquote

                frag = unquote(frag).lower()
                dst = src if not path_part else (src.parent / path_part).resolve()
                if not dst.exists():
                    bad.append((src, lineno, target, "目标文件不存在"))
                elif frag not in cache.get(dst, set()):
                    bad.append((src, lineno, target, "锚点不存在"))
    print(f"扫描 {len(files)} 个文件 / 内部锚点链接 {total} 条")
    print(f"未解析锚点：{len(bad)}")
    for src, lineno, target, why in bad:
        print(f"  {src.relative_to(root.parent)}:{lineno}  [{why}]  -> {target}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
