"""Fast web scraping with a Rust engine."""

from netweir._errors import FetchError
from netweir._fetch import Client, Page, get
from netweir._native import Node, ParseTimeout, Selection, SelectorError, parse

__all__ = [
    "Client",
    "FetchError",
    "Node",
    "Page",
    "ParseTimeout",
    "Selection",
    "SelectorError",
    "get",
    "parse",
]
