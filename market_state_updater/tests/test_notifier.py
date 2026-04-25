from __future__ import annotations

from unittest.mock import patch

from market_state_updater.notifier import TelegramNotifier


def test_disabled_when_no_token() -> None:
    n = TelegramNotifier(None, "123")
    assert n.enabled is False
    assert n.send("hi") is False


def test_disabled_when_no_chat_id() -> None:
    n = TelegramNotifier("tok", None)
    assert n.enabled is False
    assert n.send("hi") is False


def test_disabled_when_empty_strings() -> None:
    n = TelegramNotifier("", "")
    assert n.enabled is False
    assert n.send("hi") is False


def test_enabled_calls_telegram_api() -> None:
    n = TelegramNotifier("tok", "123")
    assert n.enabled is True
    with patch("market_state_updater.notifier.requests.post") as mock_post:
        mock_post.return_value.ok = True
        ok = n.send("hello")
    assert ok is True
    args, kwargs = mock_post.call_args
    assert "https://api.telegram.org/bottok/sendMessage" in args[0]
    assert kwargs["data"]["chat_id"] == "123"
    assert kwargs["data"]["text"] == "hello"
    assert kwargs["data"]["parse_mode"] == "HTML"


def test_send_swallows_exceptions() -> None:
    """전송 실패해도 main loop 죽이지 않게 모든 예외 swallow."""
    n = TelegramNotifier("tok", "123")
    with patch(
        "market_state_updater.notifier.requests.post",
        side_effect=RuntimeError("network down"),
    ):
        assert n.send("hi") is False


def test_send_returns_false_on_non_ok_response() -> None:
    n = TelegramNotifier("tok", "123")
    with patch("market_state_updater.notifier.requests.post") as mock_post:
        mock_post.return_value.ok = False
        mock_post.return_value.status_code = 401
        mock_post.return_value.text = "unauthorized"
        assert n.send("hi") is False
