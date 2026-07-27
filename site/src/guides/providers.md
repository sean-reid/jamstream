# Provider setup

JamStream hosts on three cloud providers. Each needs a one-time setup: an account, a credential with the narrowest permissions that still work, and a check that the credential works. One page per provider walks it from zero. Hosting on your own computer needs none of this; see [Playing on the same network](local.md).

| Provider | Setup effort | Machine used | Credential |
|---|---|---|---|
| [DigitalOcean](providers/digitalocean.md) | one token, about 10 minutes | s-2vcpu-2gb droplet | `DIGITALOCEAN_TOKEN` |
| [AWS](providers/aws.md) | more involved: IAM user, policy, access key | t4g.medium instance | `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` |
| [GCP](providers/gcp.md) | project, API enablement, service account key | e2-medium instance | `GOOGLE_APPLICATION_CREDENTIALS` |

**If you do not already live in one of these clouds, use DigitalOcean.** Its setup is one token in one screen, and its included transfer means a session's audio traffic costs nothing extra. AWS and GCP work well and are documented honestly: they take longer to set up because their permission systems are built for companies.

## Least privilege, and why it matters here

Each page shows how to grant JamStream only what it uses: create, list, tag, and destroy one class of machine. Set up this way, the credential on your laptop can manage jam servers and nothing else. If it ever leaks, the blast radius is a few small VMs, not your storage, your DNS, or your bill. It costs five extra minutes once. Do not hand JamStream an account-wide admin credential to save those minutes.

## Verifying any provider

The same check works everywhere. It authenticates, lists anything JamStream-tagged, and changes nothing:

```console
$ jamstream sweep --dry-run --provider digitalocean
No jamstream-tagged instances found.
```

That line means the credential works. An authentication error here names the environment variable it was missing.

Credentials stay in environment variables on your machine. They are never written to the server VM's disk, never included in invites, and never sent anywhere except to the provider's own API.
