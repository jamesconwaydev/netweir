"""Fast web scraping with a Rust engine."""

from netweir import export
from netweir._crawl import Settings, Spider
from netweir._errors import Blocked, FetchError
from netweir._fetch import Client, Page, get
from netweir._native import Node, ParseTimeout, Selection, SelectorError, XPathError, parse
from netweir._request import Request
from netweir._rules import Follow, Item, css, xpath

__all__ = [
    "Blocked",
    "Client",
    "Follow",
    "Item",
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
    "css",
    "get",
    "parse",
    "xpath",
]
