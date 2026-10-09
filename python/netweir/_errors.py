class FetchError(Exception):
    """A request failed before a response arrived.

    ``kind`` says why: ``"invalid"`` (bad URL, header or proxy),
    ``"timeout"``, ``"connect"``, ``"tls"``, ``"too_many_redirects"``,
    ``"body"`` (the connection broke mid-response) or ``"other"``.
    """

    def __init__(self, message: str, kind: str = "other"):
        super().__init__(message)
        self.kind = kind


class Blocked(Exception):
    """Bot protection turned the request away.

    ``vendor`` names it (``"cloudflare"``, ``"datadome"``, ``"akamai"``,
    ``"human"``, ``"kasada"``, ``"imperva"``, ``"aws-waf"``), ``kind`` says
    how (``"challenge"``, ``"captcha"``, ``"block"`` or ``"rate_limit"``)
    and ``page`` is the response that said so.
    """

    def __init__(self, vendor: str, kind: str, page):
        super().__init__(f"{page.url}: blocked by {vendor} ({kind})")
        self.vendor = vendor
        self.kind = kind
        self.page = page
