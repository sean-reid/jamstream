# Streaming to Twitch and YouTube

The host can put a session on air to Twitch and YouTube Live, either one alone or both at once. A platform that refuses its key or drops its connection does not take the other one with it; one encode feeds both, so a failure in that encode stops both.

Only the host can start or stop a broadcast. Everyone in the session sees the ON AIR lamp in the middle of the status bar, because everyone in the session is in the broadcast.

![The Broadcast tab of Settings, both Twitch and YouTube Live reading live with zero dropped frames, and ON AIR lit in the status bar](../images/session_destinations_live_two.png)
*On air to both platforms.*

## What goes out

The broadcast mix, over a card showing everyone in the session. The host shapes that mix at the top of the **Broadcast** tab in Settings, and it is not the same as anyone's monitor mix: to hear exactly what the stream carries, switch on **audition stream mix** there.

## Getting a stream key

Both platforms give you a key that keeps working session after session.

- **Twitch**: Creator Dashboard, Settings, Stream, Primary Stream key.
- **YouTube Live**: YouTube Studio, Go Live, Stream, Stream key. A channel needs live streaming enabled once before this appears, which can take 24 hours on a new account. Do it before the band is waiting.

**Add key** under Destinations shows these steps and opens the right page.

## Going live

**Settings**, then the **Broadcast** tab: the stream mix is at the top and Destinations under it.

1. **Add key**, paste the key, **Save key**. The row reads `ready`.
2. Repeat for the second platform if you want both.
3. **Go live**.

![The Broadcast tab with the Twitch key being entered: the field is masked and reads 24 characters under it, with a keychain checkbox and Go live still disabled](../images/session_destinations_key.png)
*Adding a Twitch key. The field never shows the key back, so a paste is checked by the character count.*

ON AIR lights for everyone in the session. You can add or drop a platform while you are on air; **Remove** stops that one and leaves the others streaming.

Your key is treated as a credential. The field is masked and there is no reveal button, so a paste is checked by the character count under it rather than by reading it back. Leave **keep this key in this computer's keychain** on and the next session starts with **Use saved key** instead of a paste; **Forget key** deletes it again.

## While you are on air

Each row says what that platform is actually doing:

| Row reads | What it means |
|---|---|
| `no key` | nothing configured for this platform |
| `key saved` | a key is on this computer, one click from being used |
| `asking` | the server has not answered yet |
| `ready` | configured, and goes live when you press Go live |
| `connecting` | starting up |
| `live` | the platform is receiving the broadcast |
| `failed` | it stopped, with the reason on the next line |

**dropped** counts repeated pictures, not missing ones. The video runs at 30 frames a second and the frame count is what holds it in step with the audio, so a frame the machine could not draw in time is sent again as the last picture rather than skipped, and this is how many of those went out. It is one count for the whole broadcast, so both rows show the same number, and it colours as it climbs. A number that climbs means the session machine cannot draw and encode 30 frames a second. Removing a destination does not bring it down, because one encode feeds every platform.

## When a platform fails

![The Broadcast tab with Twitch live and YouTube Live failed, showing the reason and a red dropped frame count, and STREAM FAILED lit in the status bar](../images/session_destinations_failed.png)
*One platform's connection failed; the other kept streaming.*

A stream that dies quietly is worse than one that never started, so a failure shows in three places: the row goes red with the reason under it, the tab counts the failures, and the status bar lights STREAM FAILED beside ON AIR even with Settings closed.

A destination that stops is retried on its own, on a backoff that starts at 500 ms and doubles to 16 seconds: the row goes back to `connecting`, and to `live` once a push has held for three seconds. A key the platform rejects fails every time, and the reason says so: **Remove** that destination and add the key again. `connection refused` or `authentication failed` means the key is wrong, was reset, or belongs to another channel.

**Stop streaming** takes everything off air at once, with no confirmation step, because a host who needs the stream to stop needs it to stop now. Ending the session stops it too.

## What it costs

Broadcasting is the one part of a session that moves real traffic: about 1.2 GB per hour per platform, against about 0.54 GB per hour for four musicians playing. Two platforms for three hours is about 7 GB, which DigitalOcean's and AWS's included transfer still covers and which costs about $0.80 on GCP. Set **stream destinations** on the wizard's cost preview and the estimate counts it; [Understanding cost](cost.md) has the rest.

## Platforms that are not here

Twitch and YouTube Live are the two that hand out a lasting key, need no application, and take a normal widescreen picture.

- **Facebook Live** mints a key per broadcast, so a session cannot be set up in advance.
- **Instagram Live** has no public ingest for ordinary accounts.
- **TikTok Live** is gated behind follower thresholds and a per-region application.
- **Kick** would work the way Twitch does and is held back only to keep this release to two platforms.

Instagram and TikTok are also vertical, and JamStream sends widescreen.
