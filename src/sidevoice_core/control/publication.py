"""Pure audio-publication policy over a reply and the current browser facts."""
from dataclasses import dataclass


@dataclass(frozen=True)
class PublicationClient:
    id: str
    session_exists: bool
    connected: bool
    thread: str | None
    switching: bool
    revision: int
    turn_revision: int
    speaking: bool


@dataclass(frozen=True)
class PublicationDecision:
    session_id: str
    revision: int
    reason: str | None = None
    wait_for_quiet: bool = False

    @property
    def can_speak(self):
        return self.reason is None or self.wait_for_quiet


def publication_decision(session_id, revision, thread, asker, audience, *, session_exists=None):
    """A replacement listener inherits the reply; a live asker keeps its own epoch.

    Denial precedence is part of the policy: an expired session wins over a closed
    call, which wins over focus, which wins over a changed turn and speaking.
    Only a newer input turn or an active speaker may defer otherwise valid audio.
    """
    if session_exists is None:
        session_exists = asker is not None and asker.session_exists
    if (asker is None or not asker.connected) and audience:
        asker = audience[-1]
        session_id, revision = asker.id, asker.revision
        session_exists = asker.session_exists
    rules = (
        (not session_exists, 'session_changed'),
        (asker is None or not asker.connected, 'call_ended'),
        (asker is not None and (asker.thread != thread or asker.switching), 'focus_changed'),
        (asker is not None and revision != asker.revision,
         'newer_turn' if asker is not None and asker.turn_revision > revision else 'focus_changed'),
        (asker is not None and asker.speaking, 'user_speaking'),
    )
    reason = next((reason for applies, reason in rules if applies), None)
    wait = reason in {'newer_turn', 'user_speaking'}
    return PublicationDecision(session_id, asker.revision if wait else revision, reason, wait)
