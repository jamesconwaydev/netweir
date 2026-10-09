from __future__ import annotations

import dataclasses
from collections.abc import Callable
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from netweir._fetch import Headers


@dataclasses.dataclass
class Request:
    """A page to fetch in a crawl, and what to do with it.

    ``callback`` is a function, or the name of a spider method, called with
    the Page; it defaults to the spider's ``parse``. ``errback`` is called
    with the Request and the FetchError when no response arrives. ``meta``
    travels with the request and comes back as ``page.meta``.
    """

    url: str
    callback: Callable[..., Any] | str | None = None
    priority: int = 0
    headers: Headers = None
    meta: dict[str, Any] = dataclasses.field(default_factory=dict)
    dont_filter: bool = False
    errback: Callable[..., Any] | str | None = None
