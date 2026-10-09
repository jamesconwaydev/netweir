from __future__ import annotations

import asyncio
from collections.abc import Iterable, Mapping

from netweir._native import Fetcher, Node, Response, Selection

Headers = Mapping[str, str] | Iterable[tuple[str, str]] | None


def _pairs(headers: Headers) -> list[tuple[str, str]]:
    if headers is None:
        return []
    if isinstance(headers, Mapping):
        return list(headers.items())
    return list(headers)


class Page:
    """A fetched page: the response, and the document parsed from it."""

    __slots__ = ("_response", "_root")

    def __init__(self, response: Response):
        self._response = response
        self._root: Node | None = None

    @property
    def url(self) -> str:
        """The final URL, after redirects."""
        return self._response.url

    @property
    def status(self) -> int:
        return self._response.status

    @property
    def version(self) -> str:
        return self._response.version

    @property
    def raw_headers(self) -> list[tuple[str, str]]:
        """Every header as (name, value), in the order received, repeats
        included. Names are lowercase."""
        return self._response.headers

    @property
    def headers(self) -> dict[str, str]:
        """Lowercase names. A header sent more than once has its values
        joined with ", ", except set-cookie, which keeps the last; use
        raw_headers for all of them."""
        out: dict[str, str] = {}
        for name, value in self._response.headers:
            if name in out and name != "set-cookie":
                out[name] = f"{out[name]}, {value}"
            else:
                out[name] = value
        return out

    @property
    def body(self) -> bytes:
        return self._response.body

    @property
    def text(self) -> str:
        """The body decoded the way a browser would choose the encoding."""
        return self._response.text()

    @property
    def root(self) -> Node:
        """The parsed document. Parsed on first use."""
        if self._root is None:
            self._root = self._response.parse()
        return self._root

    def css(self, query: str) -> Selection:
        return self.root.css(query)

    def xpath(self, query: str, **variables: str | int | float | bool) -> Selection:
        return self.root.xpath(query, **variables)

    def __getattr__(self, name: str):
        # Everything else a Node offers (find_all, select, get_text ...)
        # works on the page's document.
        if name.startswith("_"):
            raise AttributeError(name)
        return getattr(self.root, name)

    def __repr__(self) -> str:
        return f"<Page {self.status} {self.url}>"


class Client:
    """Fetches pages as one browser, reusing connections and cookies.

    ``profile`` names the browser to look like (``"chrome"`` is the newest
    Chrome). ``timeout`` covers a whole request, in seconds.
    """

    def __init__(self, profile: str = "chrome", proxy: str | None = None, timeout: float = 30.0):
        self._fetcher = Fetcher(profile, proxy, timeout)

    async def get(self, url: str, headers: Headers = None) -> Page:
        return Page(await self._fetcher.get(url, _pairs(headers)))

    async def get_many(
        self, urls: Iterable[str], headers: Headers = None, return_exceptions: bool = False
    ) -> list[Page | BaseException]:
        """Fetches every URL concurrently and returns pages in the same
        order. With ``return_exceptions=True`` a failed URL gives its
        FetchError in place of a page instead of raising."""
        pairs = _pairs(headers)
        return await asyncio.gather(
            *(self.get(url, pairs) for url in urls), return_exceptions=return_exceptions
        )

    async def __aenter__(self) -> Client:
        return self

    async def __aexit__(self, *exc: object) -> None:
        pass


def get(
    url: str,
    profile: str = "chrome",
    headers: Headers = None,
    proxy: str | None = None,
    timeout: float = 30.0,
) -> Page:
    """Fetches one page and waits for it. For many pages, use Client."""
    return Page(Fetcher(profile, proxy, timeout).get_blocking(url, _pairs(headers)))
