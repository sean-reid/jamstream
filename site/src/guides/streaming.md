# Streaming to Twitch and YouTube

The host can put a session on air to Twitch and YouTube Live, either one alone or both at once. One platform dropping out never interrupts the other.

Only the host can start or stop a broadcast. Everyone in the session sees the on air lamp, because everyone in the session is in the broadcast.

![Destinations sheet over the session screen, both Twitch and YouTube Live reading live with zero dropped frames](../images/session_destinations_live_two.png)
*On air to both platforms.*

## What goes out

The broadcast mix, over a card showing everyone in the session. The host shapes that mix in the **Stream mix** sheet, and it is not the same as anyone's monitor mix: to hear exactly what the stream carries, switch on **audition stream mix** there.

## Getting a stream key

Both platforms give you a key that keeps working session after session.

- **Twitch**: Creator Dashboard, Settings, Stream, Primary Stream key.
- **YouTube Live**: YouTube Studio, Go Live, Stream, Stream key. A channel needs live streaming enabled once before this appears, which can take 24 hours on a new account. Do it before the band is waiting.

**Add key** in the Destinations sheet shows these steps and opens the right page.

## Going live

**Destinations** in the session's status bar opens the sheet.

1. **Add key**, paste the key, **Save key**. The row reads `ready`.
2. Repeat for the second platform if you want both.
3. **Go live**.

![Destinations sheet with the Twitch key being entered: the field is masked and reads 24 characters beside it, with a keychain checkbox and Go live still disabled](../images/session_destinations_key.png)
*Adding a Twitch key. The field never shows the key back, so a paste is checked by the character count.*

The lamp turns amber for everyone in the session. You can add or drop a platform while you are on air; **Remove** stops that one and leaves the others streaming.

Your key is treated as a credential. The field is masked and there is no reveal button, so a paste is checked by the character count beside it rather than by reading it back. Leave **keep this key in this computer's keychain** on and the next session starts with **Use saved key** instead of a paste; **Forget key** deletes it again.

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

**dropped** counts frames the broadcast had to skip. It should stay at 0. Amber and red mean the session machine is not keeping up, and dropping one destination is the fix.

## When a platform fails

![Destinations sheet with Twitch live and YouTube Live failed, showing the reason and a red dropped frame count](../images/session_destinations_failed.png)
*One platform died, the other kept streaming.*

A stream that dies quietly is worse than one that never started, so a failure shows in three places: the row goes red with the reason under it, the sheet counts the failures, and the status bar says `1 failed` beside the latency readout even with the sheet closed.

A platform that hiccups is retried and comes back on its own: the row returns to `connecting`, then `live`. A key the platform rejects fails every time, and the reason says so: **Remove** that destination and add the key again. `connection refused` or `authentication failed` means the key is wrong, was reset, or belongs to another channel.

**Stop streaming** takes everything off air at once, with no confirmation step, because a host who needs the stream to stop needs it to stop now. Ending the session stops it too.

## What it costs

Broadcasting is the one part of a session that moves real traffic: about 1.2 GB per hour per platform, against roughly 0.4 GB per hour for four musicians playing. Two platforms for three hours is about 7 GB, which DigitalOcean's and AWS's included transfer still covers and which costs about $0.80 on GCP. Set **stream destinations** on the wizard's cost preview and the estimate counts it; [Understanding cost](cost.md) has the rest.

## Platforms that are not here

Twitch and YouTube Live are the two that hand out a lasting key, need no application, and take a normal widescreen picture.

- **Facebook Live** mints a key per broadcast, so a session cannot be set up in advance.
- **Instagram Live** has no public ingest for ordinary accounts.
- **TikTok Live** is gated behind follower thresholds and a per-region application.
- **Kick** would work the way Twitch does and is held back only to keep this release to two platforms.

Instagram and TikTok are also vertical, and JamStream sends widescreen.
