"""Regex extraction for Selection.re(), done by Python's own re module so
every flag and every bit of Python syntax works as the user expects."""

from __future__ import annotations

import html
import re

_ENTITY = re.compile(r"&(#\d+|#[xX][0-9a-fA-F]+|[A-Za-z][A-Za-z0-9]*);?")


def _unescape(m: re.Match[str]) -> str:
    # Kept as written, as parsel keeps them: decoding these would turn
    # escaped markup back into markup.
    if m.group(1).lower() in ("lt", "amp"):
        return m.group(0)
    return html.unescape(m.group(0))


def extract(
    pattern: str | re.Pattern[str], strings: list[str], replace_entities: bool = True
) -> list[str]:
    """Every match in every string: the whole match, or each group when the
    pattern has groups, or only the first match's group named ``extract``."""
    regex = re.compile(pattern) if isinstance(pattern, str) else pattern
    out: list[str] = []
    for text in strings:
        if "extract" in regex.groupindex:
            m = regex.search(text)
            found = [m.group("extract")] if m and m.group("extract") is not None else []
        else:
            found = []
            for match in regex.findall(text):
                if isinstance(match, tuple):
                    found.extend(match)
                else:
                    found.append(match)
        if replace_entities:
            found = [_ENTITY.sub(_unescape, s) for s in found]
        out.extend(found)
    return out
