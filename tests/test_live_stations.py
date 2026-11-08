"""Offline tests for the opt-in measurement tool, not for a built-in catalogue."""
import importlib.util
from pathlib import Path
import socket
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("live_probe", Path(__file__).with_name("live_stations.py"))
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


class ConfigurationTests(unittest.TestCase):
    def test_only_top_level_network_and_autostart_are_overridden(self):
        text = 'listen: "0.0.0.0:6600"\nautostart: true\nstations:\n - name: "listen: leave this alone"\n   url: "http://127.0.0.1/test"\n'
        isolated = probe.isolated_config(text, 45678)
        self.assertIn('listen: "127.0.0.1:45678"', isolated)
        self.assertIn('autostart: false', isolated)
        self.assertIn('name: "listen: leave this alone"', isolated)

    def test_indentless_station_sequence_is_preserved(self):
        text = 'stations:\n- name: Synthetic\n  url: http://127.0.0.1/test\n'
        isolated = probe.isolated_config(text, 45678)
        self.assertIn(text, isolated)
        self.assertIn('autostart: false', isolated)

    def test_ambiguous_config_is_rejected_not_guessed(self):
        for text in ('listen: x\nlisten: y\n', '{listen: x}\n', 'listen: |\n  multiline\n'):
            with self.subTest(text=text), self.assertRaises(probe.ProbeError):
                probe.isolated_config(text, 45678)


class SessionTests(unittest.TestCase):
    def test_timeout_cleanup_and_next_attempt_use_fresh_sessions(self):
        sessions = []

        class Fake:
            def __init__(self, *_):
                self.sock = mock.Mock()
                self.commands = []
                sessions.append(self)

            def status(self, *_):
                return {'radiod_state': 'stop'}

            def command(self, text, *_):
                if self is sessions[0]:
                    raise AssertionError('command sent on desynchronized session')
                self.commands.append(text)
                return []

        with mock.patch.object(probe, 'MPD', Fake), mock.patch.object(
            probe, 'probe', side_effect=[socket.timeout('delayed unread response'), .25]
        ):
            with self.assertRaises(socket.timeout):
                probe.checked_probe(45678, {'Id': '1'}, 5)
            self.assertEqual(probe.checked_probe(45678, {'Id': '2'}, 5), .25)
        self.assertEqual(len(sessions), 3)
        sessions[0].sock.close.assert_called()
        self.assertEqual(sessions[0].commands, [])
        self.assertEqual(sessions[1].commands, ['stop'])
        sessions[1].sock.close.assert_called()
        sessions[2].sock.close.assert_called()


if __name__ == '__main__':
    unittest.main()
