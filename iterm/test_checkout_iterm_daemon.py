import unittest

from checkout_iterm_daemon import find_session, open_session, snapshot_sessions


class FakeApp:
    def __init__(self, sessions, terminal_windows=None):
        self.sessions = sessions
        self.lookups = []
        self.terminal_windows = terminal_windows or []

    def get_session_by_id(self, session_id):
        self.lookups.append(session_id)
        return self.sessions.get(session_id)


class FindSessionTest(unittest.TestCase):
    def test_finds_live_secondary_id_when_primary_is_stale(self):
        live = object()
        app = FakeApp({"live": live})

        self.assertIs(find_session(app, ["stale", "", "live"]), live)
        self.assertEqual(app.lookups, ["stale", "live"])

    def test_recovers_session_by_name_from_cached_app_model(self):
        live = type("Session", (), {"name": "work-dash"})()
        tab = type("Tab", (), {"sessions": [live]})()
        window = type("Window", (), {"tabs": [tab]})()
        app = FakeApp({}, [window])

        self.assertIs(find_session(app, ["stale"], "work-dash"), live)

    def test_reuses_new_session_alias_before_title_updates(self):
        live = object()
        aliases = {"work-dash": "new-session"}
        app = FakeApp({"new-session": live})

        self.assertIs(
            find_session(app, ["stale"], "work-dash", aliases=aliases), live
        )


class FakeSession:
    def __init__(self):
        self.name = None
        self.sent_text = None
        self.activated = False

    async def async_set_name(self, name):
        self.name = name

    async def async_send_text(self, text):
        self.sent_text = text

    async def async_activate(self, select_tab, order_window_front):
        self.activated = select_tab and order_window_front


class OpenSessionTest(unittest.IsolatedAsyncioTestCase):
    async def test_creates_tab_and_sends_launch_command(self):
        session = FakeSession()
        tab = type("Tab", (), {"current_session": session})()

        class Window:
            async def async_create_tab(self):
                return tab

        class App:
            current_terminal_window = Window()
            activated = False

            async def async_activate(self, raise_all_windows):
                self.activated = not raise_all_windows

        app = App()
        result = await open_session(None, app, "work-dash", "checkout workspace")

        self.assertIs(result, session)
        self.assertEqual(session.name, "work-dash")
        self.assertEqual(session.sent_text, "checkout workspace\n")
        self.assertTrue(session.activated)
        self.assertTrue(app.activated)

    async def test_background_tab_does_not_activate_iterm_or_the_new_session(self):
        session = FakeSession()
        tab = type("Tab", (), {"current_session": session})()

        class Window:
            async def async_create_tab(self):
                return tab

        class App:
            current_terminal_window = Window()
            activated = False

            async def async_activate(self, raise_all_windows):
                self.activated = True

        app = App()
        result = await open_session(
            None, app, "workflow-item", "node runner.js", focus=False
        )

        self.assertIs(result, session)
        self.assertFalse(session.activated)
        self.assertFalse(app.activated)


class SnapshotSessionsTest(unittest.IsolatedAsyncioTestCase):
    async def test_returns_all_sessions_and_the_current_session(self):
        first = type("Session", (), {"session_id": "first", "name": "one"})()
        current = type("Session", (), {"session_id": "current", "name": "two"})()
        tab = type("Tab", (), {"sessions": [first, current], "current_session": current})()
        window = type("Window", (), {"tabs": [tab], "current_tab": tab})()
        app = type("App", (), {"terminal_windows": [window], "current_terminal_window": window})()

        sessions, focused = await snapshot_sessions(app)

        self.assertEqual(sessions, [
            {"sessionId": "first", "name": "one"},
            {"sessionId": "current", "name": "two"},
        ])
        self.assertEqual(focused, "current")


if __name__ == "__main__":
    unittest.main()
