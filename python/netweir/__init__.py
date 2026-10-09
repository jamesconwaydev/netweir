"""Fast web scraping with a Rust engine."""

from netweir import export
from netweir._crawl import Settings, Spider
from netweir._errors import FetchError
from netweir._fetch import Client, Page, get
from netweir._native import Node, ParseTimeout, Selection, SelectorError, XPathError, parse
from netweir._request import Request

__all__ = [
    "Client",
    "Request",
    "Settings",
    "Spider",
    "FetchError",
    "Node",
    "Page",
    "ParseTimeout",
    "Selection",
    "SelectorError",
    "XPathError",
    "export",
    "get",
    "parse",
]
