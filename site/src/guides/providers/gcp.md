# GCP

Google Cloud works well; the setup has the most moving parts of the three providers: a project, an API to enable, a service account, and a key file. Budget 30 minutes. If you are starting from nothing, [DigitalOcean](digitalocean.md) is less work, and GCP is the only one of the three that bills the session's audio traffic on top of the machine.

JamStream launches an `e2-medium` instance: about $0.034 per hour in us-central1 as of July 2026 ([VM pricing](https://cloud.google.com/compute/vm-instance-pricing)). Internet egress on the default premium tier costs $0.12 per GiB after the first free GiB each month ([network pricing](https://cloud.google.com/vpc/network-pricing)), which adds roughly $0.19 to a three hour four musician session.

## 1. Create the account

1. Sign in with a Google account at [console.cloud.google.com](https://console.cloud.google.com) and set up a billing account; a card or other valid payment method is required, with a small temporary authorization hold.
2. As of July 2026, new customers get a $300 credit valid for 90 days ([free trial terms](https://docs.cloud.google.com/free/docs/free-cloud-features)). Terms change; check the current ones.

## 2. Create a project and enable Compute Engine

1. Create a project at [console.cloud.google.com/projectcreate](https://console.cloud.google.com/projectcreate); name it `jamstream` and note the generated project id, which may have a suffix ([creating projects](https://docs.cloud.google.com/resource-manager/docs/creating-managing-projects)).
2. Enable the Compute Engine API for the project: open **APIs & Services**, then **API Library**, search for "Compute Engine API", and click **Enable**. With the gcloud tool installed it is one command:

```console
$ gcloud services enable compute.googleapis.com
```

## 3. Create a service account with one role

1. In the console, open **IAM & Admin**, then **Service Accounts**, and click **Create service account** ([docs](https://docs.cloud.google.com/iam/docs/service-accounts-create)). Name it `jamstream`.
2. Grant it exactly one role: **Compute Instance Admin (v1)**, `roles/compute.instanceAdmin.v1`. It covers creating, listing, labeling, and deleting instances ([Compute IAM roles](https://docs.cloud.google.com/compute/docs/access/iam)).
3. Click Done. You do not need `roles/iam.serviceAccountUser`: that role is only required to create VMs that run as a service account, and JamStream's session VMs run with no service account attached at all, so the VM itself holds no Google credentials.

Scoped this way, the key can manage Compute Engine instances in this one project and nothing else: no storage, no other projects, no IAM changes.

## 4. Create a JSON key

1. Open the `jamstream` service account, go to the **Keys** tab, click **Add key**, then **Create new key**, choose **JSON**, and click **Create** ([key docs](https://docs.cloud.google.com/iam/docs/keys-create-delete)). The key file downloads once; store it like a password.
2. If the create button is blocked with an organization policy error: organizations created since May 2024 disable service account key creation by default ([secure by default](https://docs.cloud.google.com/resource-manager/docs/secure-by-default-organizations)). Personal accounts without an organization are unaffected. An organization admin can lift `iam.disableServiceAccountKeyCreation` for the project; otherwise use a short-lived token instead, below.

## 5. Put the key in your environment

```console
$ export GOOGLE_APPLICATION_CREDENTIALS=$HOME/keys/jamstream-gcp.json
```

The project id is read from the key file. Set `GOOGLE_CLOUD_PROJECT` only if you need to override it.

No key file, or key creation blocked? JamStream also accepts a pre-minted token, which expires after about an hour:

```console
$ export GOOGLE_CLOUD_PROJECT=jamstream-123456
$ export GCP_ACCESS_TOKEN=$(gcloud auth print-access-token)
```

## 6. Verify

```console
$ jamstream sweep --dry-run --provider gcp
No jamstream-tagged instances found.
```

That output means the credential authenticates and can list instances. Continue with the [quickstart](../../quickstart.md#4-host), swapping `--provider digitalocean` for `--provider gcp`.
