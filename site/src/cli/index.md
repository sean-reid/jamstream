# CLI reference

The `jamstream` CLI carries complete parity with the desktop app's hosting flow, built for automation, scripting, and headless use: it hosts, monitors, and ends the same sessions unattended, joins them without a display, and reads and writes the same session state files, so either tool can watch or end what the other started. People playing music want the app and the [quickstart](../quickstart.md); the pages here document every command and flag.

- [jamstream host](host.md): provision a session server and mint invites.
- [jamstream status](status.md): list known sessions with elapsed time and accrued cost.
- [jamstream end](end.md): destroy a session's server and mark the session ended.
- [jamstream sweep](sweep.md): find and destroy orphaned jamstream instances.
- [jamstream join](join.md): join a session as a headless client.
