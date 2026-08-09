# Security

## Reporting a vulnerability

Report privately through GitHub: open the
[Security tab](https://github.com/sean-reid/jamstream/security/advisories/new)
and use Report a vulnerability. That reaches me without the details being
public. Please do not open a normal issue for anything exploitable.

Tell me what you did, what happened, and which version or commit you were on.
A proof of concept helps more than a description.

This is a personal project, so I cannot promise a response time. I will
acknowledge a report and say what I intend to do about it, and I will credit
you in the advisory unless you would rather I did not.

## What is supported

JamStream is in beta. Fixes go onto `main` and into the next release; there are
no patches for older tags. The latest release is the supported one.

## Where the interesting surface is

If you are looking for somewhere to start:

- `crates/session` handles unauthenticated UDP from the internet. The session
  server parses packets from anyone who can reach the port, before any invite
  has been checked, and it runs as an unprivileged user for that reason.
- Invites are bearer tokens: one per person per session, revocable by the host.
  Holding one is enough to join, so anything that leaks or forges one matters.
- `crates/cloud` talks to AWS, GCP, and DigitalOcean with the operator's own
  credentials, and signs its own requests.
- Stream keys and cloud credentials live in the operating system keychain and
  are never displayed back. Anything that puts one on screen, in a log, or in
  a crash report is a bug worth reporting.

## What is out of scope

Denial of service against a session server by flooding the port. A session
server is a machine the host launched for one rehearsal and can delete; the
cost of that is a session ending early, and there is no shared service behind
it to protect.
