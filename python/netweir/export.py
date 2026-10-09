"""Exporters: pipeline stages that write every item to a file.

    class Books(netweir.Spider):
        pipelines = [drop_out_of_stock, netweir.export.jsonl("books.jsonl")]

Serialising happens in Rust. Close an exporter (crawls do it for you) to
flush the file.
"""

from __future__ import annotations

import logging
from typing import Any

from netweir._native import CsvWriter, JsonlWriter, ParquetWriter

log = logging.getLogger("netweir")


class jsonl:  # noqa: N801 - reads as a function in a pipeline list
    """One JSON object per line."""

    def __init__(self, path: str):
        self.path = path
        self._writer = JsonlWriter(path)

    def __call__(self, item: Any) -> Any:
        self._writer.write(item)
        return item

    def close(self) -> None:
        self._writer.close()


class csv:  # noqa: N801
    """A CSV file. Columns are ``fields`` if given, otherwise the first
    item's keys; keys outside them are left out, with one warning."""

    def __init__(self, path: str, fields: list[str] | None = None):
        self.path = path
        self._writer = CsvWriter(path, fields)
        self._warned: set[str] = set()

    def __call__(self, item: Any) -> Any:
        extra = set(self._writer.write(item)) - self._warned
        if extra:
            self._warned |= extra
            log.warning(
                "%s: no column for %s; pass fields= to include them",
                self.path,
                ", ".join(sorted(extra)),
            )
        return item

    def close(self) -> None:
        self._writer.close()


class parquet:  # noqa: N801
    """A Parquet file. Columns and their types (bool, 64-bit int, double
    or text) come from the first 1,000 items: a key holding both ints and
    floats is a double column, any other mix is text, and lists and dicts
    are stored as JSON text.

    Later values that don't fit are stored as null, and later keys with no
    column are left out; closing logs a warning for each.
    """

    def __init__(self, path: str):
        self.path = path
        self._writer = ParquetWriter(path)

    def __call__(self, item: Any) -> Any:
        self._writer.write(item)
        return item

    def close(self) -> None:
        for key, missing, wrong in self._writer.close():
            if missing:
                log.warning("%s: no column for %r, left out of %d items", self.path, key, missing)
            if wrong:
                log.warning(
                    "%s: %d values of %r didn't fit the column type and were stored as null",
                    self.path,
                    wrong,
                    key,
                )


def to_path(path: str):
    """The exporter for a file name's extension: .jsonl/.jl, .csv or
    .parquet."""
    lower = path.lower()
    if lower.endswith((".jsonl", ".jl")):
        return jsonl(path)
    if lower.endswith(".csv"):
        return csv(path)
    if lower.endswith(".parquet"):
        return parquet(path)
    raise ValueError(f"can't tell the format of {path!r}: use .jsonl, .csv or .parquet")
