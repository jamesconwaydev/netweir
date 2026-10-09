"""The docs stay true to the code: every example parses, and every setting
is documented."""

import ast
import dataclasses
import re
from pathlib import Path

import pytest

import netweir

ROOT = Path(__file__).parent.parent
PAGES = [ROOT / "README.md", *sorted((ROOT / "docs").glob("*.md"))]


@pytest.mark.parametrize("page", PAGES, ids=lambda p: p.name)
def test_every_python_example_parses(page):
    for block in re.findall(r"```python\n(.*?)```", page.read_text(encoding="utf-8"), re.S):
        ast.parse(block)


def test_every_setting_is_documented():
    reference = (ROOT / "docs" / "settings.md").read_text(encoding="utf-8")
    missing = [
        f.name for f in dataclasses.fields(netweir.Settings) if f"| `{f.name}` |" not in reference
    ]
    assert not missing, f"docs/settings.md doesn't describe {missing}"


def test_documented_defaults_are_the_real_ones():
    reference = (ROOT / "docs" / "settings.md").read_text(encoding="utf-8")
    for f in dataclasses.fields(netweir.Settings):
        row = re.search(rf"\| `{f.name}` \| `([^`]*)` \|", reference)
        assert row, f.name
        assert ast.literal_eval(row.group(1)) == f.default, f"{f.name}: {row.group(1)}"
