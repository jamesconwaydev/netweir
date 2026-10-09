"""Exporters: pipeline stages that write every item to a file.

    class Books(netweir.Spider):
        pipelines = [drop_out_of_stock, netweir.export.jsonl("books.jsonl")]

Serialising happens in Rust. A crawl opens its exporters when it starts
and closes them when it ends; each run writes the file afresh, except a run
that resumes from a checkpoint, which adds to it. Used on its own, an
exporter opens its file at the first item; close it to finish the file.
"""

from __future__ import annotations

import logging
import os
from typing import Any

from netweir._native import CsvWriter, JsonlWriter, ParquetWriter

log = logging.getLogger("netweir")


class _Exporter:
    """Writes items to ``path``. The file is opened by ``start`` (a crawl
    calls it) or at the first item, so defining an exporter doesn't touch
    the file. Closing finishes it; the next start begins it afresh."""

    def __init__(self, path: str):
        directory = os.path.dirname(os.path.abspath(path))
        if not os.path.isdir(directory):
            raise ValueError(f"can't write {path}: {directory} is not a directory")
        self.path = path
        self._append = False
        self._writer: Any = None

    def start(self, append: bool = False) -> None:
        """Opens the file: emptied, or with ``append`` added to."""
        if self._writer is not None:
            self.close()
        self._append = append
        self._writer = self._open()

    def _open(self) -> Any:
        raise NotImplementedError

    def _write(self, item: Any) -> Any:
        if self._writer is None:
            self._writer = self._open()
        return self._writer.write(item)

    def __call__(self, item: Any) -> Any:
        self._write(item)
        return item

    def flush(self) -> bool:
        """Puts what's written so far on disk. False if this format can't
        (Parquet is only readable once closed)."""
        if self._writer is not None:
            self._writer.flush()
        return True

    def close(self) -> None:
        """Finishes the file; one that never got an item is written empty
        (a CSV with ``fields`` gets its header)."""
        writer, self._writer = self._writer or self._open(), None
        self._closed(writer.close())

    def _closed(self, report: Any) -> None:
        pass


class jsonl(_Exporter):  # noqa: N801 - reads as a function in a pipeline list
    """One JSON object per line."""

    def _open(self) -> JsonlWriter:
        return JsonlWriter(self.path, self._append)


class csv(_Exporter):  # noqa: N801
    """A CSV file. Columns are ``fields`` if given, otherwise the first
    item's keys; keys outside them are left out, with one warning."""

    def __init__(self, path: str, fields: list[str] | None = None):
        self.fields = fields
        super().__init__(path)

    def _open(self) -> CsvWriter:
        self._warned: set[str] = set()
        return CsvWriter(self.path, self.fields, self._append)

    def __call__(self, item: Any) -> Any:
        extra = set(self._write(item)) - self._warned
        if extra:
            self._warned |= extra
            log.warning(
                "%s: no column for %s; pass fields= to include them",
                self.path,
                ", ".join(sorted(extra)),
            )
        return item


class parquet(_Exporter):  # noqa: N801
    """A Parquet file. Columns and their types (bool, 64-bit int, double
    or text) come from the first 1,000 items: a key holding both ints and
    floats is a double column, any other mix is text, and lists and dicts
    are stored as JSON text.

    Later values that don't fit are stored as null, and later keys with no
    column are left out; closing logs a warning for each.
    """

    def _open(self) -> ParquetWriter:
        path = self.path
        if self._append and os.path.exists(path) and os.path.getsize(path) > 0:
            # A Parquet file can't be added to: a resumed run writes the
            # next part beside it (books.1.parquet, books.2.parquet ...),
            # which readers such as pyarrow take together as one dataset.
            stem = path[: -len(".parquet")] if path.endswith(".parquet") else path
            n = 1
            while os.path.exists(f"{stem}.{n}.parquet"):
                n += 1
            path = f"{stem}.{n}.parquet"
            log.info("%s exists: this run's items go to %s", self.path, path)
        return ParquetWriter(path)

    def flush(self) -> bool:
        return False

    def _closed(self, report: list[tuple[str, int, int]]) -> None:
        for key, missing, wrong in report:
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
