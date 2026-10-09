"""Declarative items and link rules. Both compile to Rust when defined, so a
spider made of them crawls without running Python per page."""

from __future__ import annotations

import logging
from collections.abc import Callable
from typing import Any

from netweir._native import ItemSpec, Node

#: `into` values Rust converts itself; anything else runs in Python.
_BUILTIN = {str: "text", int: "int", float: "float", bool: "bool"}


class Field:
    """One field of an Item. Made by ``netweir.css`` or ``netweir.xpath``."""

    __slots__ = ("all", "default", "into", "kind", "query", "re", "strip")

    def __init__(self, kind, query, re, into, all, strip, default):
        self.kind = kind
        self.query = query
        self.re = re
        self.into = into
        self.all = all
        self.strip = strip
        self.default = default
        # Compiling now reports a bad query or pattern where it was written.
        ItemSpec("check", [self._spec("check")])

    def _spec(self, name: str) -> tuple:
        convert = _BUILTIN.get(self.into, "text")
        return (name, self.kind, self.query, self.re, self.all, self.strip, convert, self.default)

    def _python_into(self) -> Callable[[Any], Any] | None:
        return None if self.into is None or self.into in _BUILTIN else self.into


def css(
    query: str,
    *,
    re: str | None = None,
    into: Callable[[Any], Any] | None = None,
    all: bool = False,
    strip: bool = False,
    default: Any = None,
) -> Field:
    """A field found by a CSS query (with ``::text`` and ``::attr()``).

    The value is the first result, as ``get()`` gives it, or every result with
    ``all=True``. ``re`` keeps what the pattern matches (its first group if it
    has one), in Rust's regex syntax. ``strip`` trims whitespace. ``into`` of
    ``int``, ``float``, ``bool`` or ``str`` converts in Rust; a value that
    won't convert becomes None, with a warning. Any other callable is applied
    to the finished value in Python. Nothing found gives ``default``.
    """
    return Field("css", query, re, into, all, strip, default)


def xpath(
    query: str,
    *,
    re: str | None = None,
    into: Callable[[Any], Any] | None = None,
    all: bool = False,
    strip: bool = False,
    default: Any = None,
) -> Field:
    """A field found by an XPath 1.0 query; see ``css`` for the options."""
    return Field("xpath", query, re, into, all, strip, default)


class Item:
    """Subclass this and give it fields::

        class Book(netweir.Item):
            title = netweir.css("h1::text")
            price = netweir.css(".price_color::text", re=r"[\\d.]+", into=float)

    Use it in a rule (``netweir.Follow(..., extract=Book)``) or call
    ``Book.extract(page)`` in a callback. Items come out as dicts, with
    keys in the order the fields are written.
    """

    _spec: ItemSpec
    _python_into: dict[str, Callable[[Any], Any]]

    def __init_subclass__(cls, **kwargs):
        super().__init_subclass__(**kwargs)
        fields: dict[str, Field] = {}
        for klass in reversed(cls.__mro__):
            for name, value in vars(klass).items():
                if isinstance(value, Field):
                    fields[name] = value
        cls._spec = ItemSpec(cls.__name__, [f._spec(name) for name, f in fields.items()])
        cls._python_into = {
            name: into for name, f in fields.items() if (into := f._python_into()) is not None
        }

    @classmethod
    def extract(cls, node: Any) -> dict[str, Any]:
        """The item found in a Page or below a Node."""
        root = getattr(node, "root", node)
        if not isinstance(root, Node):
            raise TypeError(f"extract() takes a Page or a Node, not {type(node).__name__}")
        item, invalid = cls._spec.extract(root)
        _warn_invalid(invalid)
        return cls._finish(item)

    @classmethod
    def _finish(cls, item: dict[str, Any], warned: set[str] | None = None) -> dict[str, Any]:
        for name, into in cls._python_into.items():
            value = item[name]
            if value is None:
                continue
            try:
                item[name] = [into(v) for v in value] if isinstance(value, list) else into(value)
            except Exception as e:  # noqa: BLE001 - reported, value stored as None
                _warn_once(f"{cls.__name__}.{name}", f"{into!r} failed on {value!r}: {e}", warned)
                item[name] = None
        return item


#: Fields already warned about outside a crawl; a crawl keeps its own.
_warned: set[str] = set()


def _warn_once(field: str, detail: str, warned: set[str] | None = None) -> None:
    warned = _warned if warned is None else warned
    if field not in warned:
        warned.add(field)
        logging.getLogger("netweir").warning("%s: %s; stored None", field, detail)


def _warn_invalid(invalid: list[tuple[str, str]], warned: set[str] | None = None) -> None:
    for field, text in invalid:
        _warn_once(field, f"couldn't convert {text!r}", warned)


class Follow:
    """A link rule for a Spider's ``rules``.

    Takes every link matched by ``css`` (or ``xpath``): a matched element's
    ``href``, or the strings themselves if the query asks for them
    (``::attr(href)``, ``@href``). Links are resolved as a browser would.

    The pages they lead to get ``extract``'s item taken from them, in Rust,
    and are passed to ``callback`` if one is given. ``follow`` says whether
    the rules apply again on those pages; it defaults to True for a rule with
    neither ``extract`` nor ``callback``, and False otherwise, as in Scrapy.
    """

    __slots__ = ("callback", "extract", "follow", "kind", "priority", "query")

    def __init__(
        self,
        css: str | None = None,
        *,
        xpath: str | None = None,
        extract: type[Item] | None = None,
        callback: Callable[..., Any] | str | None = None,
        follow: bool | None = None,
        priority: int = 0,
    ):
        if (css is None) == (xpath is None):
            raise TypeError("Follow takes a css query or an xpath query, not both or neither")
        if extract is not None and not (isinstance(extract, type) and issubclass(extract, Item)):
            raise TypeError("extract= takes an Item subclass")
        self.kind, self.query = ("css", css) if css is not None else ("xpath", xpath)
        self.extract = extract
        self.callback = callback
        self.follow = (extract is None and callback is None) if follow is None else follow
        self.priority = priority
