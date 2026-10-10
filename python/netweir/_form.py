"""Reading a form off a page as the browser would submit it."""

from __future__ import annotations

import dataclasses
import secrets
import string
from collections.abc import Iterable, Mapping
from typing import TYPE_CHECKING, Any
from urllib.parse import urlencode, urljoin, urlsplit, urlunsplit

if TYPE_CHECKING:
    from netweir._native import Node
    from netweir._request import Request

URLENCODED = "application/x-www-form-urlencoded"
MULTIPART = "multipart/form-data"
# Never submitted by themselves: buttons only when they're the one pressed,
# files not yet, images as coordinates nobody asked for.
_SKIPPED = {"submit", "button", "reset", "image", "file"}


@dataclasses.dataclass
class Form:
    """A form ready to submit: where to, how, the fields in the order the
    browser would send them, and the page it's on.

    Submit it with ``netweir.submit(form)`` or ``client.submit(form)``, or
    in a crawl yield ``form.request(callback=...)``.
    """

    method: str
    action: str
    fields: list[tuple[str, str]]
    enctype: str = URLENCODED
    referer: str | None = None

    def encoded(self) -> tuple[str, bytes | None, str | None]:
        """The URL, body and Content-Type the browser would send: a GET
        form's fields go in the query string, replacing any there."""
        if self.method == "GET":
            parts = urlsplit(self.action)
            query = urlencode(self.fields)
            return urlunsplit(parts._replace(query=query, fragment="")), None, None
        if self.enctype == MULTIPART:
            return self.action, *_multipart(self.fields)
        return self.action, urlencode(self.fields).encode(), URLENCODED

    def request(self, callback: Any = None, **kwargs: Any) -> Request:
        """A crawl Request that submits this form."""
        from netweir._request import Request

        url, body, content_type = self.encoded()
        if body is None:
            return Request(url, callback=callback, referer=self.referer, **kwargs)
        return Request(
            url,
            callback=callback,
            method=self.method,
            body=body,
            content_type=content_type,
            kind="form",
            referer=self.referer,
            **kwargs,
        )


def read_form(
    root: Node,
    page_url: str,
    query: str | None = None,
    *,
    data: Mapping[str, Any] | Iterable[tuple[str, Any]] | None = None,
    click: str | bool | None = None,
    formid: str | None = None,
    formname: str | None = None,
    formnumber: int = 0,
) -> Form:
    """The form on the page chosen by ``query`` (a CSS selector, or an
    XPath when it starts with ``/`` or ``(``), ``formid``, ``formname``, or
    else its position, ``formnumber``."""
    form = _pick(root, query, formid, formname, formnumber)
    method = (form.attr("method") or "get").strip().upper()
    if method not in ("GET", "POST"):
        method = "GET"
    action = urljoin(page_url, (form.attr("action") or "").strip() or page_url)
    enctype = (form.attr("enctype") or "").strip().lower()
    if enctype not in (URLENCODED, MULTIPART):
        enctype = URLENCODED
    fields = _successful(form)
    button = _button(form, click)
    if button is not None:
        fields.append(button)
    if data:
        fields = _merged(fields, data)
    return Form(method, action, fields, enctype, page_url)


def _pick(root: Node, query: str | None, formid: str | None, formname: str | None, n: int) -> Node:
    if query is not None:
        is_xpath = query.lstrip().startswith(("/", "("))
        found = [
            x for x in (root.xpath(query) if is_xpath else root.css(query)) if hasattr(x, "tag")
        ]
        found = [x if x.tag == "form" else x.find_parent("form") for x in found]
        found = [x for x in found if x is not None]
        what = f"matching {query!r}"
    elif formid is not None:
        found = [f for f in root.select("form") if f.attr("id") == formid]
        what = f"with id {formid!r}"
    elif formname is not None:
        found = [f for f in root.select("form") if f.attr("name") == formname]
        what = f"named {formname!r}"
    else:
        forms = root.select("form")
        found = forms[n : n + 1] if 0 <= n < len(forms) else []
        what = f"number {n} (the page has {len(forms)})"
    if not found:
        raise ValueError(f"no form {what} on the page")
    return found[0]


def _successful(form: Node) -> list[tuple[str, str]]:
    """The controls the HTML standard says a submission includes, in
    document order."""
    fields: list[tuple[str, str]] = []
    for control in form.select("input, select, textarea"):
        name = control.attr("name")
        if not name or control.attr("disabled") is not None:
            continue
        if control.tag == "select":
            options = [o for o in control.select("option") if o.attr("disabled") is None]
            chosen = [o for o in options if o.attr("selected") is not None]
            if not chosen and control.attr("multiple") is None and options:
                chosen = options[:1]
            fields.extend((name, _option_value(o)) for o in chosen)
        elif control.tag == "textarea":
            fields.append((name, control.text))
        else:
            kind = (control.attr("type") or "").strip().lower()
            if kind in _SKIPPED:
                continue
            if kind in ("checkbox", "radio"):
                if control.attr("checked") is not None:
                    fields.append((name, control.attr("value") or "on"))
            else:
                # Text, hidden and the like; an unknown type is text too.
                fields.append((name, control.attr("value") or ""))
    return fields


def _option_value(option: Node) -> str:
    value = option.attr("value")
    return value if value is not None else " ".join(option.text.split())


def _button(form: Node, click: str | bool | None) -> tuple[str, str] | None:
    """The submit button pressed: the one named ``click``, none for
    ``click=False``, or else the first."""
    if click is False:
        return None
    buttons = [
        b
        for b in form.select("input, button")
        if b.attr("disabled") is None
        and (
            (b.tag == "input" and (b.attr("type") or "").strip().lower() == "submit")
            or (b.tag == "button" and (b.attr("type") or "submit").strip().lower() == "submit")
        )
    ]
    if isinstance(click, str):
        named = [b for b in buttons if b.attr("name") == click]
        if not named:
            raise ValueError(f"no submit button named {click!r} in the form")
        return click, named[0].attr("value") or ""
    # The first submit button is the one pressed; without a name it adds
    # nothing.
    if buttons and buttons[0].attr("name"):
        return buttons[0].attr("name"), buttons[0].attr("value") or ""
    return None


def _merged(
    fields: list[tuple[str, str]], data: Mapping[str, Any] | Iterable[tuple[str, Any]]
) -> list[tuple[str, str]]:
    """``fields`` with ``data`` replacing every field of the same name, in
    the place of the first, and adding the rest at the end. A list value
    gives the name more than once."""
    pairs = data.items() if isinstance(data, Mapping) else data
    given: dict[str, list[str]] = {}
    for name, value in pairs:
        values = value if isinstance(value, list | tuple) else [value]
        given.setdefault(name, []).extend(str(v) for v in values)
    out: list[tuple[str, str]] = []
    placed: set[str] = set()
    for name, value in fields:
        if name not in given:
            out.append((name, value))
        elif name not in placed:
            out.extend((name, v) for v in given[name])
            placed.add(name)
    out.extend((name, v) for name, values in given.items() if name not in placed for v in values)
    return out


def _multipart(fields: list[tuple[str, str]]) -> tuple[bytes, str]:
    """The body and Content-Type of a multipart/form-data submission of
    text fields, with a boundary shaped as Chrome's."""
    alphabet = string.ascii_letters + string.digits
    boundary = "----WebKitFormBoundary" + "".join(secrets.choice(alphabet) for _ in range(16))

    def escape(name: str) -> str:
        return name.replace('"', "%22").replace("\r", "%0D").replace("\n", "%0A")

    out = bytearray()
    for name, value in fields:
        out += f'--{boundary}\r\nContent-Disposition: form-data; name="{escape(name)}"\r\n\r\n'.encode()
        out += value.encode() + b"\r\n"
    out += f"--{boundary}--\r\n".encode()
    return bytes(out), f"{MULTIPART}; boundary={boundary}"
