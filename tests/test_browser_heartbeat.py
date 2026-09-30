import asyncio
from unittest import IsolatedAsyncioTestCase

from sidevoice.browser_heartbeat import HEARTBEAT_MISSES, HEARTBEAT_SECONDS, heartbeat_settings, watch


class HeartbeatSettingsTest(IsolatedAsyncioTestCase):
    async def test_the_room_keeps_its_own_clock_unless_this_one_is_configured(self):
        self.assertEqual(heartbeat_settings({}), (HEARTBEAT_SECONDS, HEARTBEAT_MISSES))
        self.assertEqual(heartbeat_settings({'VOICE_BROWSER_HEARTBEAT_SECONDS': '4',
                                             'VOICE_BROWSER_HEARTBEAT_MISSES': '3'}), (4.0, 3.0))

    async def test_an_unreadable_setting_falls_back_to_the_default_not_to_no_keepalive(self):
        self.assertEqual(heartbeat_settings({'VOICE_BROWSER_HEARTBEAT_SECONDS': 'pronto'}),
                         (HEARTBEAT_SECONDS, HEARTBEAT_MISSES))
        self.assertEqual(heartbeat_settings({'VOICE_BROWSER_HEARTBEAT_SECONDS': '-1'}),
                         (HEARTBEAT_SECONDS, HEARTBEAT_MISSES))
        # Zero is the one way to say "ask nothing", and it is said on purpose.
        self.assertEqual(heartbeat_settings({'VOICE_BROWSER_HEARTBEAT_SECONDS': '0'})[0], 0.0)


class HeartbeatPolicyTest(IsolatedAsyncioTestCase):
    """What the clock decides, with no socket, no room and no seat in sight."""

    async def run_watch(self, silences, *, interval=0.01, misses=2):
        asked, dropped, readings = [], [], list(silences)

        def silent_for():
            return readings.pop(0) if readings else 0.0

        async def drop(silence):
            dropped.append(silence)

        task = asyncio.create_task(watch(silent_for, lambda: asked.append(True), drop,
                                         interval=interval, misses=misses))
        try:
            await asyncio.wait_for(task, interval * (len(silences) + 4))
        except asyncio.TimeoutError:
            task.cancel()
        return asked, dropped

    async def test_a_browser_that_answers_is_never_asked_and_never_dropped(self):
        asked, dropped = await self.run_watch([0.0, 0.001, 0.002])
        self.assertEqual((asked, dropped), ([], []), 'a socket that keeps talking is left alone')

    async def test_one_silent_interval_is_a_question_not_a_verdict(self):
        asked, dropped = await self.run_watch([0.01, 0.015, 0.0])
        self.assertEqual(len(asked), 2, 'each quiet tick asks again')
        self.assertEqual(dropped, [], 'the budget is what ends a call, not one unanswered ask')

    async def test_a_browser_that_spends_its_budget_is_dropped_once_and_the_watch_ends(self):
        asked, dropped = await self.run_watch([0.01, 0.02, 0.05])
        self.assertEqual(len(asked), 1)
        self.assertEqual(dropped, [0.02], 'the silence it was dropped for is the silence it had')

    async def test_the_budget_is_the_interval_times_the_misses_allowed(self):
        _, patient = await self.run_watch([0.025], misses=3)
        self.assertEqual(patient, [], 'three misses allowed is 0.03 s of budget')
        _, strict = await self.run_watch([0.025], misses=1)
        self.assertEqual(strict, [0.025], 'one miss allowed is 0.01 s of budget')
