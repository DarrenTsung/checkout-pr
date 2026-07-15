import unittest

from checkout_iterm_daemon import find_session


class FakeApp:
    def __init__(self, sessions):
        self.sessions = sessions
        self.lookups = []

    def get_session_by_id(self, session_id):
        self.lookups.append(session_id)
        return self.sessions.get(session_id)


class FindSessionTest(unittest.TestCase):
    def test_finds_live_secondary_id_when_primary_is_stale(self):
        live = object()
        app = FakeApp({"live": live})

        self.assertIs(find_session(app, ["stale", "", "live"]), live)
        self.assertEqual(app.lookups, ["stale", "live"])


if __name__ == "__main__":
    unittest.main()
