"""Fast web scraping with a Rust engine."""

from netweir import export, repair
from netweir._browser import browser
from netweir._crawl import Settings, Spider
from netweir._errors import Blocked, BrowserError, BrowserTimeout, FetchError
from netweir._fetch import Client, Page, get
from netweir._native import (
    Browser,
    BrowserContext,
    BrowserPage,
    BrowserResponse,
    Node,
    ParseTimeout,
    Selection,
    SelectorError,
    XPathError,
    parse,
)
from netweir._request import Request
from netweir._rules import Follow, Item, css, xpath

__all__ = [
    "Blocked",
    "Browser",
    "BrowserContext",
    "BrowserError",
    "BrowserPage",
    "BrowserResponse",
    "BrowserTimeout",
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
    "browser",
    "css",
    "export",
    "get",
    "parse",
    "repair",
    "xpath",
]
