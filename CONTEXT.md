# status-notifications

A daemon that polls Atom/RSS status-page feeds (OpenAI, Claude, and DeepSeek by
default) and delivers a notification for each new incident or incident update.
macOS today; Linux is planned (`mac-notification-sys` behind a feature, launchd
replaced by systemd). The domain language is about the feeds it polls, the
entries it extracts, and the sinks it pushes notifications to.

## Language

**Feed**:
A single status-page Atom/RSS source polled on a fixed interval.
_Avoid_: source, subscription, stream

**Entry**:
One normalized incident (or incident update) extracted from a feed, identified
by its `(id, updated)` pair. A single incident yields a sequence of entries as
its `<updated>` timestamp bumps.
_Avoid_: item, post, event, alert

**Sink**:
A one-way destination a notification is delivered to — a macOS banner, a
Telegram bot message, a Zulip message. Nothing flows back from a sink.
_Avoid_: sync, notifier, channel, target, endpoint
