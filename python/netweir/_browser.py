"""Driving Chrome: ``netweir.browser()``."""

from __future__ import annotations

import os
from collections.abc import Generator, Sequence
from typing import Any

from netweir._native import Browser, launch


class browser:
    """Starts Chrome. Use it either way:

    .. code-block:: python

        async with netweir.browser() as b:
            page = await b.new_page()

        b = await netweir.browser()
        ...
        await b.close()

    Without ``executable``, it uses ``$NETWEIR_CHROME``, then the usual
    install paths for Chrome, Chrome for Testing and Chromium. ``timeout``
    (seconds) is the default limit for navigations and actions.
    """

    def __init__(
        self,
        executable: str | os.PathLike[str] | None = None,
        *,
        headless: bool = True,
        args: Sequence[str] = (),
        timeout: float = 30.0,
    ):
        self._options: dict[str, Any] = {
            "executable": None if executable is None else os.fspath(executable),
            "headless": headless,
            "args": list(args),
            "timeout": timeout,
        }
        self._browser: Browser | None = None

    def __await__(self) -> Generator[Any, None, Browser]:
        return launch(**self._options).__await__()

    async def __aenter__(self) -> Browser:
        self._browser = await self
        return self._browser

    async def __aexit__(self, *exc: object) -> None:
        if self._browser is not None:
            await self._browser.close()
