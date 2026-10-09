import re
from collections.abc import Callable, Iterator
from typing import Any, Literal, TypeVar, overload

from netweir._errors import FetchError

_D = TypeVar("_D")

class SelectorError(ValueError): ...
class XPathError(ValueError): ...
class ParseTimeout(TimeoutError): ...

Filter = (
    str
    | list[str]
    | tuple[str, ...]
    | set[str]
    | bool
    | re.Pattern[str]
    | Callable[..., object]
    | None
)

class Node:
    # parsel-style queries
    def css(self, query: str) -> Selection: ...
    def xpath(self, query: str, **variables: str | int | float | bool) -> Selection: ...
    # Beautiful Soup style search
    def find(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        recursive: bool = True,
        string: Filter = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> Node | None: ...
    def find_all(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        recursive: bool = True,
        string: Filter = None,
        limit: int | None = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> list[Node]: ...
    def find_parent(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> Node | None: ...
    def find_parents(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        limit: int | None = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> list[Node]: ...
    def find_next_sibling(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> Node | None: ...
    def find_next_siblings(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        limit: int | None = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> list[Node]: ...
    def find_previous_sibling(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> Node | None: ...
    def find_previous_siblings(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        limit: int | None = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> list[Node]: ...
    def find_next(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> Node | None: ...
    def find_all_next(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        limit: int | None = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> list[Node]: ...
    def find_previous(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> Node | None: ...
    def find_all_previous(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        limit: int | None = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> list[Node]: ...
    def select(self, css: str) -> list[Node]: ...
    def select_one(self, css: str) -> Node | None: ...
    # what the node is
    @property
    def kind(self) -> Literal["element", "text", "comment", "document", "doctype", "other"]: ...
    @property
    def tag(self) -> str | None: ...
    @property
    def name(self) -> str | None: ...
    @property
    def text(self) -> str: ...
    @property
    def html(self) -> str: ...
    @property
    def string(self) -> str | None: ...
    def get_text(self, separator: str = "", strip: bool = False) -> str: ...
    # attributes
    @property
    def attrs(self) -> dict[str, str]: ...
    def attr(self, name: str) -> str | None: ...
    @overload
    def get(self, name: str) -> str | None: ...
    @overload
    def get(self, name: str, default: object) -> object: ...
    def __getitem__(self, name: str) -> str: ...
    # navigation
    @property
    def parent(self) -> Node | None: ...
    @property
    def parents(self) -> list[Node]: ...
    @property
    def children(self) -> list[Node]: ...
    @property
    def contents(self) -> list[Node]: ...
    @property
    def descendants(self) -> list[Node]: ...
    @property
    def next_sibling(self) -> Node | None: ...
    @property
    def previous_sibling(self) -> Node | None: ...
    @property
    def next_siblings(self) -> list[Node]: ...
    @property
    def previous_siblings(self) -> list[Node]: ...
    @property
    def next_element(self) -> Node | None: ...
    @property
    def previous_element(self) -> Node | None: ...
    def __eq__(self, other: object) -> bool: ...
    def __hash__(self) -> int: ...

class Selection:
    @overload
    def get(self) -> str | None: ...
    @overload
    def get(self, default: _D) -> str | _D: ...
    def getall(self) -> list[str]: ...
    def extract(self) -> list[str]: ...
    @overload
    def extract_first(self) -> str | None: ...
    @overload
    def extract_first(self, default: _D) -> str | _D: ...
    def css(self, query: str) -> Selection: ...
    def xpath(self, query: str, **variables: str | int | float | bool) -> Selection: ...
    def re(self, pattern: str | re.Pattern[str], replace_entities: bool = True) -> list[str]: ...
    @overload
    def re_first(
        self, pattern: str | re.Pattern[str], *, replace_entities: bool = True
    ) -> str | None: ...
    @overload
    def re_first(
        self, pattern: str | re.Pattern[str], default: _D, replace_entities: bool = True
    ) -> str | _D: ...
    @property
    def attrib(self) -> dict[str, str]: ...
    def __len__(self) -> int: ...
    @overload
    def __getitem__(self, index: int) -> Node | str: ...
    @overload
    def __getitem__(self, index: slice) -> Selection: ...
    def __iter__(self) -> Iterator[Node | str]: ...

class Response:
    @property
    def url(self) -> str: ...
    @property
    def status(self) -> int: ...
    @property
    def version(self) -> str: ...
    @property
    def headers(self) -> list[tuple[str, str]]: ...
    @property
    def body(self) -> bytes: ...
    def text(self) -> str: ...
    def parse(self, timeout: float | None = None) -> Node: ...

class Fetcher:
    def __init__(
        self,
        profile: str = "chrome",
        proxy: str | None = None,
        timeout: float = 30.0,
        verify: bool = True,
    ) -> None: ...
    async def get(self, url: str, headers: list[tuple[str, str]] | None = None) -> Response: ...
    def get_blocking(self, url: str, headers: list[tuple[str, str]] | None = None) -> Response: ...

class Crawler:
    def __init__(
        self,
        profile: str = "chrome",
        proxy: str | None = None,
        timeout: float = 30.0,
        verify: bool = True,
        concurrency: int = 64,
        per_domain: int = 8,
        obey_robots: bool = True,
        robots_agent: str = "netweir",
        obey_tdmrep: bool = True,
        throttle: bool = True,
        start_delay: float = 1.0,
        min_delay: float = 0.0,
        max_delay: float = 60.0,
        target_concurrency: float = 1.0,
    ) -> None: ...
    def submit(
        self,
        id: int,
        url: str,
        priority: int = 0,
        headers: list[tuple[str, str]] | None = None,
        dont_filter: bool = False,
    ) -> Literal["queued", "duplicate", "invalid"]: ...
    async def next(
        self, max: int = 256
    ) -> list[
        tuple[Literal["fetched"], int, Response]
        | tuple[Literal["failed"], int, FetchError]
        | tuple[Literal["dropped"], int, Literal["robots", "tdm"]]
    ]: ...
    def stats(self) -> dict[str, int]: ...

class JsonlWriter:
    def __init__(self, path: str) -> None: ...
    def write(self, item: Any) -> None: ...
    def close(self) -> None: ...

class CsvWriter:
    def __init__(self, path: str, fields: list[str] | None = None) -> None: ...
    def write(self, item: Any) -> list[str]: ...
    def close(self) -> None: ...

class ParquetWriter:
    def __init__(self, path: str) -> None: ...
    def write(self, item: Any) -> None: ...
    def close(self) -> list[tuple[str, int, int]]: ...

def parse(html: str, timeout: float | None = None) -> Node: ...
