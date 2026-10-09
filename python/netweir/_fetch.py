from __future__ import annotations

import asyncio
from collections.abc import Iterable, Mapping
from urllib.parse import urljoin

from netweir._errors import Blocked
from netweir._native import Fetcher, Node, Response, Selection
from netweir._request import Request

Headers = Mapping[str, str] | Iterable[tuple[str, str]] | None


def _pairs(headers: Headers) -> list[tuple[str, str]]:
    if headers is None:
        return []
    if isinstance(headers, Mapping):
        return list(headers.items())
    return list(headers)


class Page:
    """A fetched page: the response, and the document parsed from it."""

    __slots__ = ("_base", "_outcome", "_response", "_root", "request")

    def __init__(
        self, response: Response, request: Request | None = None, root: Node | None = None
    ):
        self._response = response
        #: Parsed on first use, unless the crawl engine already parsed it.
        self._root: Node | None = root
        self._base: str | None = None
        self._outcome: tuple | None = None
        #: In a crawl, the Request this page answers.
        self.request = request

    @property
    def url(self) -> str:
        """The final URL, after redirects."""
        return self._response.url

    @property
    def status(self) -> int:
        return self._response.status

    def _classified(self) -> tuple:
        if self._outcome is None:
            self._outcome = self._response.classify()
        return self._outcome

    @property
    def outcome(self) -> str:
        """What the response means: ``"ok"``, ``"blocked"`` (by bot
        protection), ``"throttled"`` (429, or 503 with Retry-After),
        ``"payment_required"`` (402) or ``"http_error"``."""
        return self._classified()[0]

    @property
    def blocked(self) -> str | None:
        """The bot-protection vendor that blocked this request, or None."""
        return self._classified()[1]

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

    @property
    def depth(self) -> int:
        """Links followed from a start page to get here; 0 outside a crawl."""
        return self.request.depth if self.request is not None else 0

    @property
    def meta(self) -> dict:
        """The meta of the Request this page answers; empty outside a crawl."""
        return self.request.meta if self.request is not None else {}

    def urljoin(self, url: str) -> str:
        """``url`` made absolute, against the page's ``<base href>`` if it
        has one and its URL otherwise, as a browser resolves links."""
        if self._base is None:
            href = self.root.css("base[href]::attr(href)").get()
            self._base = urljoin(self.url, href) if href else self.url
        return urljoin(self._base, str(url))

    def follow(self, url: str, callback=None, **kwargs) -> Request:
        """A Request for ``url``, resolved as ``urljoin`` does."""
        return Request(self.urljoin(url), callback=callback, **kwargs)

    def follow_all(self, urls, callback=None, **kwargs) -> list[Request]:
        """``follow`` for every URL in ``urls`` (a list or a Selection)."""
        return [self.follow(u, callback, **kwargs) for u in urls]

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

    async def get(self, url: str, headers: Headers = None, raise_on_block: bool = True) -> Page:
        """Fetches one page. A block page raises Blocked unless
        ``raise_on_block=False``."""
        return _checked(Page(await self._fetcher.get(url, _pairs(headers))), raise_on_block)

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
    raise_on_block: bool = True,
) -> Page:
    """Fetches one page and waits for it. For many pages, use Client.

    A page from bot protection (a Cloudflare challenge, a DataDome captcha
    and the like) raises Blocked; pass ``raise_on_block=False`` to get it
    as a page, with ``page.blocked`` naming the vendor."""
    page = Page(Fetcher(profile, proxy, timeout).get_blocking(url, _pairs(headers)))
    return _checked(page, raise_on_block)


def _checked(page: Page, raise_on_block: bool) -> Page:
    if raise_on_block and page.blocked is not None:
        raise Blocked(page.blocked, page._classified()[2], page)
    return page
