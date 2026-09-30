"""The browser link's keepalive: the room asks, the browser answers, and no seat is held for ever.

Behind a tunnel or a proxy, closing a tab does not reach the room as a socket close. The TCP
connection stays up, the pipeline never ends, the ordinary disconnect never runs and the seat stays
taken (2026-09-22: eight seats held by browsers that no longer existed, over one afternoon of one
person reloading one tab, and a phone that could not get in until the room was restarted).

The vocabulary is the connector link's, deliberately: an interval the room asks on and a budget of
missed beats past which the peer is gone — `ping_interval` / `ping_timeout` and `HEARTBEAT_MISSES`
in `connector_socketio.py`. The numbers are this link's own, because the two links are not one
thing and neither budget should move because the other did.

**The room asks and the browser answers**, rather than the browser telling on a clock of its own: a
background tab's timers are throttled or stopped, so a page that is perfectly alive would look dead
on its own beat, while a frame that arrives still wakes its socket handler. An answer also proves
the page is running, which a TCP connection a proxy is holding open does not.

Anything the browser sends counts as an answer, microphone PCM included, so the question is only
asked of a socket that has actually gone quiet — which, since the microphone is never paused, means
a browser that is muted or gone.
"""
import asyncio

# How often the room asks a quiet browser to say something, and how many asks it may miss before its
# seat goes back to the room. Fifteen seconds is what the connector link already asks on, so a
# machine and a browser go quiet on the same clock; two misses because one lost ask is a hiccup (a
# phone changing network), and two is a peer that is not there. A seat is therefore free between 30
# and 45 s after the browser stopped answering, which is nothing next to somebody walking back to a
# room that is full, and long next to any blip.
HEARTBEAT_SECONDS = 15.0
HEARTBEAT_MISSES = 2


def heartbeat_settings(config):
    """How often to ask and how many misses to allow, as this room was configured.

    `VOICE_BROWSER_HEARTBEAT_SECONDS` at zero turns the keepalive off entirely, which is the escape
    hatch for a room whose browsers are on the same machine and can be trusted to close their own
    sockets. Anything unreadable falls back to the defaults rather than to no keepalive at all.
    """
    def number(name, fallback, floor):
        try:
            value = float(config.get(name, fallback))
        except (TypeError, ValueError):
            return fallback
        return fallback if value < 0 else max(value, floor) if value else 0.0

    interval = number('VOICE_BROWSER_HEARTBEAT_SECONDS', HEARTBEAT_SECONDS, 0.01)
    # A budget of no misses would drop a browser the moment it drew breath, so zero here means the
    # default: "ask nothing at all" is said with the interval, which is the setting that means it.
    misses = number('VOICE_BROWSER_HEARTBEAT_MISSES', HEARTBEAT_MISSES, 1.0) or HEARTBEAT_MISSES
    return interval, misses


async def watch(silent_for, ask, drop, *, interval=HEARTBEAT_SECONDS, misses=HEARTBEAT_MISSES):
    """Ask a browser that has gone quiet to say something, and drop the one that never does.

    `silent_for()` says how long nothing at all has arrived on this socket, `ask()` sends the
    question and `drop(silence)` ends the call. Nothing here knows what a socket, a room or a seat
    is: a browser that stopped answering must leave by the same door an ordinary disconnect uses,
    and that door belongs to the call, not to this clock.
    """
    budget = interval * misses
    while True:
        await asyncio.sleep(interval)
        silence = silent_for()
        if silence >= budget:
            await drop(silence)
            return
        if silence >= interval:
            ask()
