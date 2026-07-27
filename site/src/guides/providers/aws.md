# AWS

AWS works well and is priced fine; the setup is more involved than DigitalOcean's because IAM is built for companies. Budget 30 minutes the first time. If you are not already an AWS user, [DigitalOcean](digitalocean.md) is less work.

JamStream launches a `t4g.medium` instance (arm64, Debian 12): about $0.034 per hour in us-east-1 as of July 2026 ([on-demand pricing](https://aws.amazon.com/ec2/pricing/on-demand/)). AWS includes 100 GB per month of free data transfer out across your whole account, which comfortably covers session audio.

## 1. Create the account

1. Sign up at [aws.amazon.com](https://aws.amazon.com). You need a credit or debit card (AWS makes a temporary $1 authorization to verify it) and a phone number for an SMS or voice verification step ([registration FAQ](https://aws.amazon.com/free/registration-faqs/)).
2. As of July 2026, new accounts choose a free or paid plan and receive $100 in credits, with up to $100 more for completing onboarding activities; the free plan ends after 6 months or when credits run out ([AWS Free Tier](https://aws.amazon.com/free/)). Terms change; check the current ones.

## 2. Create an IAM user with a minimal policy

Do not use your root account's credentials. Create a user that can do exactly what JamStream does: run, list, tag, and terminate instances, plus read the public parameter that names the current Debian image.

1. Open the IAM console. In the navigation pane choose **Users**, then **Create user** ([IAM user guide](https://docs.aws.amazon.com/IAM/latest/UserGuide/id_users_create.html)). Name it `jamstream`. It needs no console access.
2. For permission options choose **Attach policies directly**, then **Create policy**, switch to the JSON editor, and paste:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "JamstreamInstances",
      "Effect": "Allow",
      "Action": [
        "ec2:RunInstances",
        "ec2:TerminateInstances",
        "ec2:DescribeInstances"
      ],
      "Resource": "*"
    },
    {
      "Sid": "JamstreamTagAtLaunch",
      "Effect": "Allow",
      "Action": "ec2:CreateTags",
      "Resource": "*",
      "Condition": {
        "StringEquals": { "ec2:CreateAction": "RunInstances" }
      }
    },
    {
      "Sid": "JamstreamDebianAmiLookup",
      "Effect": "Allow",
      "Action": "ssm:GetParameter",
      "Resource": "arn:aws:ssm:*::parameter/aws/service/debian/*"
    }
  ]
}
```

What each block is for:

- The five actions are the complete list of AWS calls JamStream makes. Action names are from the [EC2](https://docs.aws.amazon.com/service-authorization/latest/reference/list_ec2.html) and [SSM](https://docs.aws.amazon.com/service-authorization/latest/reference/list_ssm.html) authorization references.
- The `ec2:CreateTags` block is condition-scoped so the user can tag instances only while launching them, which is the only time JamStream tags anything ([tagging at launch](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/supported-iam-actions-tagging.html)).
- The SSM resource has an empty account field because `/aws/service/debian/...` is a public parameter published by AWS; that ARN form is per [the SSM IAM docs](https://docs.aws.amazon.com/systems-manager/latest/userguide/security_iam_service-with-iam.html). JamStream reads it to find the current Debian 12 arm64 image per region.

3. Name the policy `jamstream-host`, create it, attach it to the user, and finish creating the user.

A key with this policy can manage EC2 instances and read one public parameter, and nothing else: no S3, no billing, no IAM changes.

## 3. Create an access key

1. Open the `jamstream` user, go to the **Security credentials** tab, and under Access keys click **Create access key** ([access key docs](https://docs.aws.amazon.com/IAM/latest/UserGuide/id_credentials_access-keys.html)).
2. When asked for a use case, choose **Command Line Interface (CLI)** and confirm.
3. On the final page, copy both values or download the CSV; the secret is shown once.

## 4. Put the keys in your environment

```console
$ export AWS_ACCESS_KEY_ID=AKIA...
$ export AWS_SECRET_ACCESS_KEY=your_secret_here
```

JamStream reads exactly these two variables; it does not read `~/.aws/config` profiles in the current build.

## 5. Verify

```console
$ jamstream sweep --dry-run --provider aws
No jamstream-tagged instances found.
```

That output means the key authenticates and can list instances. Continue with the [quickstart](../../quickstart.md#host), swapping `--provider digitalocean` for `--provider aws`.
