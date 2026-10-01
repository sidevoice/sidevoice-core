"""How often a provider's model may be checked from this node (#124 §6; review R08).

A check calls a paid provider twice with the node's key. A paired device has the node's full authority, so this is
not about who may ask but about how much: a retry loop, a person rechecking again and again, or a misbehaving
client must not turn into an unbounded bill or a provider that rate-limits the node's real calls. So:

- a check that passed is remembered for a while, for the same provider, model, options, language and key — the key
  by a digest of it, so a new key is checked afresh — and answered from memory, costing nothing;
- identical checks asked at the same time share one run;
- a run is admitted only within a budget per device and per provider (a sliding window); past it the check is
  refused with how long to wait.

Pure apart from the clock it is given; no web framework.
"""
import asyncio
import hashlib
import json
import time
from collections import deque

PER_DEVICE = (6, 60.0)      # runs a device may start in a window of this many seconds
PER_PROVIDER = (12, 60.0)   # runs every device together may start against one provider
REMEMBER_SECONDS = 600.0    # how long a passed check answers for itself


class Limited(Exception):
    """Over budget: `retry_after` seconds until a run would be admitted."""
    def __init__(self, retry_after, scope):
        super().__init__(f'too many checks for this {scope}')
        self.retry_after, self.scope = retry_after, scope


def check_key(task, place, model, options, language, credential):
    """What makes two checks the same one. The credential is a digest, never the key."""
    revision = hashlib.sha256((credential or '').encode('utf8')).hexdigest()[:16]
    return json.dumps([task, place, model, options, language, revision], sort_keys=True, default=str)


class CheckBudget:
    def __init__(self, *, clock=time.monotonic, per_device=PER_DEVICE, per_provider=PER_PROVIDER, remember=REMEMBER_SECONDS):
        self.clock, self.per_device, self.per_provider, self.remember = clock, per_device, per_provider, remember
        self.windows = {}       # ('device'|'provider', id) -> deque of start times
        self.passed = {}        # key -> (time, result)
        self.running = {}       # key -> future of the run in flight

    def _wait(self, kind, name, limit):
        count, seconds = limit
        window = self.windows.setdefault((kind, name), deque())
        now = self.clock()
        while window and now - window[0] >= seconds:
            window.popleft()
        return None if len(window) < count else max(1, int(seconds - (now - window[0]) + 0.999))

    async def run(self, key, *, device, provider, work):
        """The answer to check `key`: remembered, shared with an identical one in flight, or a new run of `work()`
        if the budget admits it (else Limited)."""
        remembered = self.passed.get(key)
        if remembered and self.clock() - remembered[0] < self.remember:
            return {**remembered[1], 'remembered': True}
        if key in self.running:
            return await asyncio.shield(self.running[key])
        for kind, name, limit in (('device', device, self.per_device), ('provider', provider, self.per_provider)):
            wait = self._wait(kind, name, limit)
            if wait is not None:
                raise Limited(wait, kind)
        now = self.clock()
        self.windows[('device', device)].append(now)
        self.windows[('provider', provider)].append(now)
        future = asyncio.get_running_loop().create_future()
        self.running[key] = future
        try:
            result = await work()
        except BaseException as error:
            future.set_exception(error)
            future.exception()  # retrieved: nobody else may be waiting
            raise
        finally:
            self.running.pop(key, None)
        if result.get('ok'):
            self.passed[key] = (self.clock(), result)
        future.set_result(result)
        return result
