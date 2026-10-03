"""What the room says when it will not do something: a status and a sentence, and no web framework.

The status keeps the HTTP meaning the web surface answers with (409 a stale or foreign request,
429 a full queue, 404/410 a reply the room no longer has, 502 a provider that failed), so the
mapping is one handler in `sidevoice_core.server` and never a second vocabulary.
"""


class Refusal(Exception):
    def __init__(self, status_code, detail):
        super().__init__(detail)
        self.status_code, self.detail = status_code, detail
        self.text_saved = False
