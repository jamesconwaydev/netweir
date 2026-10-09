class FetchError(Exception):
    """A request failed before a response arrived.

    ``kind`` says why: ``"invalid"`` (bad URL, header or proxy),
    ``"timeout"``, ``"connect"``, ``"tls"``, ``"too_many_redirects"``,
    ``"body"`` (the connection broke mid-response) or ``"other"``.
    """

    def __init__(self, message: str, kind: str = "other"):
        super().__init__(message)
        self.kind = kind
