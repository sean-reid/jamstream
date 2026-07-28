# Provider setup

JamStream hosts on three cloud providers. Each needs a one-time setup: an account, a credential with the narrowest permissions that still work, and a check that the credential works. The last two steps happen inside the app: selecting a provider that reads `setup needed` in the host wizard opens an inline pane that takes the credential, checks it with a real API call, and saves it to your system keychain, so the next session starts at `ready`. One page per provider walks the account and credential creation from zero. Hosting on your own computer needs none of this; see [Playing on the same network](local.md).

| Provider | Setup effort | Machine used | Credential |
|---|---|---|---|
| [DigitalOcean](providers/digitalocean.md) | one token, about 10 minutes | s-2vcpu-2gb droplet | API token |
| [AWS](providers/aws.md) | more involved: IAM user, policy, access key | t4g.medium instance | access key id and secret |
| [GCP](providers/gcp.md) | project, API enablement, service account key | e2-medium instance | service account JSON key |

**If you do not already live in one of these clouds, use DigitalOcean.** Its setup is one token in one screen, and its included transfer means a session's audio traffic costs nothing extra. AWS and GCP work well and are documented honestly: they take longer to set up because their permission systems are built for companies.

## Least privilege, and why it matters here

Each page shows how to grant JamStream only what it uses: create, list, tag, and destroy one class of machine. Set up this way, the credential on your laptop can manage jam servers and nothing else. If it ever leaks, the blast radius is a few small VMs, not your storage, your DNS, or your bill.

One credential per provider covers everything except one feature. [Recording a cloud session](recording.md) writes takes to a bucket, which needs a second key, scoped to that one bucket and nothing more; the last section of each provider page creates it. Recording is off unless you turn it on, so the plain path stays one credential.

## Verifying any provider

The app's **Check credentials** button makes a real authenticated call before saving anything: it fetches a price and lists anything JamStream-tagged, changing nothing. A failure is shown verbatim in the pane. The same check works from the terminal:

```console
$ jamstream sweep --dry-run --provider digitalocean
No jamstream-tagged instances found.
```

That line means the credential works. An authentication error here names the environment variable it was missing.

## For the CLI and automation: environment variables

The CLI reads credentials from the environment: `DIGITALOCEAN_TOKEN`, `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`, or `GOOGLE_APPLICATION_CREDENTIALS`, as each provider page shows. The app reads the same variables as a silent fallback, so a machine configured for the CLI works in the app with no extra setup; a value saved from the app's pane takes precedence.

Credentials are never included in an invite. Scope a DigitalOcean token to droplets and nothing else: a session droplet is given that token so it can delete itself when the session ends.
