"""Publication is a policy over facts; no sockets or event sequence required."""
import unittest
from dataclasses import replace
from sidevoice.publication import PublicationClient, publication_decision


class PublicationRules(unittest.TestCase):
    client = PublicationClient('s', True, True, 'a', False, 3, 3, False)

    def decision(self, client=None, revision=3, audience=()):
        return publication_decision('s', revision, 'a', client or self.client, audience)

    def test_current_quiet_listener_can_speak(self):
        d = self.decision()
        self.assertTrue(d.can_speak)
        self.assertFalse(d.wait_for_quiet)
        self.assertEqual((d.session_id, d.revision, d.reason), ('s', 3, None))

    def test_missing_session_precedes_other_denials(self):
        d = self.decision(replace(self.client, session_exists=False, connected=False, switching=True))
        self.assertEqual(d.reason, 'session_changed')
        self.assertFalse(d.can_speak)

    def test_closed_call_is_not_saved_for_later_audio(self):
        d = self.decision(replace(self.client, connected=False))
        self.assertEqual(d.reason, 'call_ended')
        self.assertFalse(d.can_speak)

    def test_wrong_focus_or_switching_cannot_speak(self):
        for client in (replace(self.client, thread='b'), replace(self.client, switching=True)):
            with self.subTest(client=client):
                d = self.decision(client)
                self.assertEqual(d.reason, 'focus_changed')
                self.assertFalse(d.can_speak)

    def test_newer_input_can_wait_at_the_current_audio_revision(self):
        d = self.decision(revision=2)
        self.assertEqual((d.reason, d.revision), ('newer_turn', 3))
        self.assertTrue(d.can_speak)
        self.assertTrue(d.wait_for_quiet)

    def test_changed_audio_epoch_without_new_input_is_a_focus_change(self):
        d = self.decision(replace(self.client, turn_revision=1), revision=2)
        self.assertEqual(d.reason, 'focus_changed')
        self.assertFalse(d.can_speak)

    def test_user_speaking_can_wait_without_interrupting_capture(self):
        d = self.decision(replace(self.client, speaking=True))
        self.assertEqual(d.reason, 'user_speaking')
        self.assertTrue(d.wait_for_quiet)

    def test_replacement_listener_inherits_a_disconnected_askers_reply(self):
        successor = replace(self.client, id='new', revision=8, turn_revision=8)
        for asker in (None, replace(self.client, connected=False)):
            with self.subTest(asker=asker):
                d = publication_decision('s', 3, 'a', asker, (successor,))
                self.assertEqual((d.session_id, d.revision), ('new', 8))
                self.assertTrue(d.can_speak)

    def test_live_asker_is_not_replaced_by_another_browser(self):
        d = self.decision(replace(self.client, thread='b'), audience=(replace(self.client, id='new'),))
        self.assertEqual(d.session_id, 's')
        self.assertFalse(d.can_speak)

    def test_no_asker_and_no_audience_means_text_only(self):
        d = publication_decision('s', 3, 'a', None, ())
        self.assertEqual(d.reason, 'session_changed')
        self.assertFalse(d.can_speak)

    def test_session_without_connected_client_is_call_ended(self):
        d = publication_decision('s', 3, 'a', None, (), session_exists=True)
        self.assertEqual(d.reason, 'call_ended')
        self.assertFalse(d.can_speak)
