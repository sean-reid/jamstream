# DigitalOcean

The recommended provider: one token in one screen, and the droplet's included transfer covers a session's audio traffic. Total setup is about 10 minutes, most of it account signup.

JamStream launches an `s-2vcpu-2gb` Basic droplet: $0.02679 per hour as of July 2026, billed per second with a 60 second minimum, with 3,000 GiB of transfer included ([pricing](https://www.digitalocean.com/pricing/droplets)). Powered-off droplets still bill on DigitalOcean, which is why JamStream only ever destroys them outright.

## 1. Create the account

1. Sign up at [digitalocean.com](https://www.digitalocean.com).
2. Add a payment method before creating anything; DigitalOcean requires one to verify the account. Cards, PayPal, Google Pay, and several others are accepted ([payment methods](https://docs.digitalocean.com/platform/billing/manage-payment-methods/)). Card signups may show a temporary preauthorization hold; PayPal makes a $5 authorization charge.
3. As of July 2026, new accounts get a $5 signup credit that expires 90 days after signup ([signup credit](https://docs.digitalocean.com/platform/billing/signup-credit/)). Promotions change; check the current terms. $5 covers roughly 60 session hours, so the credit alone funds a lot of rehearsal.

## 2. Create an API token

Follow [DigitalOcean's token guide](https://docs.digitalocean.com/reference/api/create-personal-access-token/); the short form:

1. Log in to the control panel at cloud.digitalocean.com.
2. In the main menu, open **Account**, then **API**. You land on the Applications & API page, Tokens tab.
3. Under Personal access tokens, click **Generate New Token**.
4. Name it something you will recognize later, like `jamstream`.
5. Pick an expiration. Shorter is safer; when it expires you generate a new one, which takes a minute.
6. For scopes, choose **Custom Scopes** and grant exactly these:

| Scope | Why JamStream needs it |
|---|---|
| `droplet:create` | launch the session server |
| `droplet:read` | find it and read its address |
| `droplet:delete` | destroy it when the session ends |
| `tag:create`, `tag:read`, `tag:delete` | every JamStream droplet is tagged, and the sweeper finds strays by tag |
| `regions:read`, `sizes:read`, `actions:read`, `image:read`, `ssh_key:read` | required companions of `droplet:create` in DigitalOcean's scope system, and `sizes:read` is how live pricing is fetched |

The scope names are from [DigitalOcean's scope reference](https://docs.digitalocean.com/reference/api/scopes/); `droplet:create` lists the read scopes as requirements, so the token cannot be created without them.

7. Click generate and copy the token immediately; it is shown once.

Scoped this way, the token can manage droplets and tags, and nothing else in your account: no storage, no DNS, no billing.

## 3. Put the token in your environment

```console
$ export DIGITALOCEAN_TOKEN=dop_v1_your_token_here
```

Add that line to your shell profile if you host regularly, or keep the token in a password manager and export it per session.

## 4. Verify

```console
$ jamstream sweep --dry-run --provider digitalocean
No jamstream-tagged instances found.
```

That output means the token authenticates and can list droplets. You are ready to host; continue with the [quickstart](../../quickstart.md#4-host).
