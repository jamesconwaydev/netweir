"""The ``netweir`` command.

netweir crawl spider.py -o items.jsonl -s concurrency=32
"""

from __future__ import annotations

import argparse
import dataclasses
import importlib.util
import inspect
import logging
import sys
from pathlib import Path


class _Usage(Exception):
    """A mistake on the command line: reported in one line, exit status 2."""


def _load_spider(path: str, name: str | None):
    from netweir import Spider

    if not Path(path).is_file():
        raise _Usage(f"no such file: {path}")
    # A name of its own, so a file called netweir.py can't shadow the
    # package, registered before running it, as dataclasses and pickle
    # look modules up by name.
    module_name = "netweir_spider_" + "".join(c if c.isalnum() else "_" for c in Path(path).stem)
    spec = importlib.util.spec_from_file_location(module_name, path)
    if spec is None or spec.loader is None:
        raise _Usage(f"can't load {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[module_name] = module
    spec.loader.exec_module(module)
    spiders = [
        obj
        for obj in vars(module).values()
        if inspect.isclass(obj)
        and issubclass(obj, Spider)
        and obj is not Spider
        and obj.__module__ == module.__name__
    ]
    if name:
        spiders = [s for s in spiders if s.__name__ == name or s.name == name]
    if len(spiders) != 1:
        found = ", ".join(s.__name__ for s in spiders) or "none"
        raise _Usage(
            f"{path}: expected one Spider{' named ' + name if name else ''}, found {found}"
        )
    return spiders[0]


def _parse_setting(text: str, fields: dict) -> tuple[str, object]:
    key, sep, raw = text.partition("=")
    if not sep or key not in fields:
        known = ", ".join(sorted(fields))
        raise ValueError(f"unknown setting {key!r}; settings are: {known}")
    kind = fields[key].type
    if kind in ("bool", bool):
        if raw.lower() not in ("true", "false", "1", "0", "yes", "no"):
            raise ValueError(f"{key} takes true or false, not {raw!r}")
        return key, raw.lower() in ("true", "1", "yes")
    if kind in ("int", int):
        return key, int(raw)
    if kind in ("float", float):
        return key, float(raw)
    return key, None if raw.lower() == "none" else raw


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="netweir")
    commands = parser.add_subparsers(dest="command", required=True)
    crawl = commands.add_parser("crawl", help="run a spider")
    crawl.add_argument("file", help="a Python file defining a Spider subclass")
    crawl.add_argument("-o", "--output", help="write items to a .jsonl, .csv or .parquet file")
    crawl.add_argument(
        "-s", "--set", action="append", default=[], metavar="KEY=VALUE", help="override a setting"
    )
    crawl.add_argument("--spider", help="which Spider, when the file has several")
    crawl.add_argument("-q", "--quiet", action="store_true", help="only warnings and errors")
    args = parser.parse_args(argv)

    logging.basicConfig(
        level=logging.WARNING if args.quiet else logging.INFO, format="%(levelname)s %(message)s"
    )
    try:
        spider, output = _prepare(args)
    except (_Usage, ValueError, OSError) as e:
        print(f"netweir: {e}", file=sys.stderr)
        return 2
    try:
        spider.run(output=output)
    except KeyboardInterrupt:
        print("netweir: interrupted", file=sys.stderr)
        return 130
    return 0


def _prepare(args):
    """The spider and output the command line asks for, checked before the
    crawl starts, so a typo costs one line rather than a traceback."""
    from netweir import Settings, export

    spider_cls = _load_spider(args.file, args.spider)
    fields = {f.name: f for f in dataclasses.fields(Settings)}
    overrides = dict(_parse_setting(s, fields) for s in args.set)
    settings = dataclasses.replace(spider_cls.settings, **overrides)
    settings._engine()  # the profile, proxy and limits are valid
    spider = spider_cls()
    spider.settings = settings
    output = export.to_path(args.output) if args.output else None
    return spider, output
