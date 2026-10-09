"""Fast web scraping with a Rust engine."""

from netweir._native import Node, ParseTimeout, Selection, SelectorError, parse

__all__ = ["Node", "ParseTimeout", "Selection", "SelectorError", "parse"]
