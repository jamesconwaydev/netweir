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


def _load_spider(path: str, name: str | None):
    from netweir import Spider

    spec = importlib.util.spec_from_file_location(Path(path).stem, path)
    if spec is None or spec.loader is None:
        raise SystemExit(f"can't load {path}")
    module = importlib.util.module_from_spec(spec)
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
        raise SystemExit(
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
    from netweir import Settings

    parser = argparse.ArgumentParser(prog="netweir")
    commands = parser.add_subparsers(dest="command", required=True)
    crawl = commands.add_parser("crawl", help="run a spider")
    crawl.add_argument("file", help="a Python file defining a Spider subclass")
    crawl.add_argument("-o", "--output", help="write items to a .jsonl or .csv file")
    crawl.add_argument(
        "-s", "--set", action="append", default=[], metavar="KEY=VALUE", help="override a setting"
    )
    crawl.add_argument("--spider", help="which Spider, when the file has several")
    crawl.add_argument("-q", "--quiet", action="store_true", help="only warnings and errors")
    args = parser.parse_args(argv)

    logging.basicConfig(
        level=logging.WARNING if args.quiet else logging.INFO, format="%(levelname)s %(message)s"
    )
    spider_cls = _load_spider(args.file, args.spider)
    fields = {f.name: f for f in dataclasses.fields(Settings)}
    try:
        overrides = dict(_parse_setting(s, fields) for s in args.set)
        settings = dataclasses.replace(spider_cls.settings, **overrides)
    except ValueError as e:
        print(f"netweir: {e}", file=sys.stderr)
        return 2
    spider = spider_cls()
    spider.settings = settings
    spider.run(output=args.output)
    return 0
