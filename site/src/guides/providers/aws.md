# AWS

AWS works well and is priced fine; the setup is more involved than DigitalOcean's because IAM is built for companies. Budget 30 minutes the first time. If you are not already an AWS user, [DigitalOcean](digitalocean.md) is less work.

JamStream launches a `t4g.medium` instance (arm64, Debian 12): about $0.034 per hour in us-east-1 as of July 2026 ([on-demand pricing](https://aws.amazon.com/ec2/pricing/on-demand/)). AWS includes 100 GB per month of free data transfer out across your whole account, which comfortably covers session audio.

## 1. Create the account

1. Sign up at [aws.amazon.com](https://aws.amazon.com). You need a credit or debit card (AWS makes a temporary $1 authorization to verify it) and a phone number for an SMS or voice verification step ([registration FAQ](https://aws.amazon.com/free/registration-faqs/)).
2. As of July 2026, new accounts choose a free or paid plan and receive $100 in credits, with up to $100 more for completing onboarding activities; the free plan ends after 6 months or when credits run out ([AWS Free Tier](https://aws.amazon.com/free/)). Terms change; check the current ones.

## 2. Create an IAM user with a minimal policy

Do not use your root account's credentials. Create a user that can run the session VM, tear it down, and manage the firewall that lets your band reach it, and that cannot do anything else.

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
      "Sid": "JamstreamCreateSessionFirewall",
      "Effect": "Allow",
      "Action": "ec2:CreateSecurityGroup",
      "Resource": [
        "arn:aws:ec2:*:*:security-group/*",
        "arn:aws:ec2:*:*:vpc/*"
      ]
    },
    {
      "Sid": "JamstreamOwnSessionFirewalls",
      "Effect": "Allow",
      "Action": [
        "ec2:AuthorizeSecurityGroupIngress",
        "ec2:DeleteSecurityGroup"
      ],
      "Resource": "arn:aws:ec2:*:*:security-group/*",
      "Condition": {
        "StringLike": { "aws:ResourceTag/jamstream-session": "*" }
      }
    },
    {
      "Sid": "JamstreamFindSessionFirewalls",
      "Effect": "Allow",
      "Action": "ec2:DescribeSecurityGroups",
      "Resource": "*"
    },
    {
      "Sid": "JamstreamTagAtCreation",
      "Effect": "Allow",
      "Action": "ec2:CreateTags",
      "Resource": "*",
      "Condition": {
        "StringEquals": {
          "ec2:CreateAction": ["RunInstances", "CreateSecurityGroup"]
        }
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

What each block is for. Action names are from the [EC2](https://docs.aws.amazon.com/service-authorization/latest/reference/list_ec2.html) and [SSM](https://docs.aws.amazon.com/service-authorization/latest/reference/list_ssm.html) authorization references; the policy above is the list, so read it rather than a summary here.

- **Instances.** Starting the session VM, stopping it, and reading its address once AWS assigns one. The sweeper uses the same read to find VMs a crashed client left behind.
- **Creating the firewall.** Each session gets its own security group, created before the instance so the VM is never briefly up on your VPC's defaults. This is the loose block: a group does not exist before it is created, so it carries no tag to condition on, and the call is authorized against the VPC the group lands in as well as against the group. Resource types are as far as it narrows here, so **a key with this policy can create a security group in any VPC in the account.** It cannot open a port on one, or delete one, unless JamStream tagged it; that is the next block.
- **Opening the port and cleaning up.** Scoped to groups tagged `jamstream-session`, which JamStream sets at creation. One UDP port is opened, the session port, to `0.0.0.0/0` and `::/0`. Deletion happens when the session ends, and on the next sweep for anything a crash left behind. Groups belonging to the rest of your account carry no such tag and are out of reach.
- **Finding the firewalls.** EC2's `Describe` actions take no resource and no tag condition, so this one is read-only across every security group in the region. JamStream uses it to find a group a half-finished launch left behind, and to list the groups the sweeper should delete.
- **Tagging at creation.** Condition-scoped to the two calls that create something, so the user can tag an instance or a security group as it is made and can never retag anything else ([tagging at creation](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/supported-iam-actions-tagging.html)). The tag is what makes the sweeper and the block above work.
- **The Debian image.** The SSM resource has an empty account field because `/aws/service/debian/...` is a public parameter published by AWS; that ARN form is per [the SSM IAM docs](https://docs.aws.amazon.com/systems-manager/latest/userguide/security_iam_service-with-iam.html). JamStream reads it to find the current Debian 12 arm64 image per region.

3. Name the policy `jamstream-host`, create it, attach it to the user, and finish creating the user.

A key with this policy can run and terminate EC2 instances, manage its own session firewalls, read security groups and instances, and read one public parameter. It cannot reach S3, billing, or IAM.

If a launch fails with `UnauthorizedOperation`, the app names the action your policy is missing. Compare it against the JSON above: an older `jamstream-host` policy predating the per-session firewall will be missing the security group blocks and the `CreateSecurityGroup` entry in the tagging condition.

## 3. Create an access key

1. Open the `jamstream` user, go to the **Security credentials** tab, and under Access keys click **Create access key** ([access key docs](https://docs.aws.amazon.com/IAM/latest/UserGuide/id_credentials_access-keys.html)).
2. When asked for a use case, choose **Command Line Interface (CLI)** and confirm.
3. On the final page, copy both values or download the CSV; the secret is shown once.

## 4. Connect the app

In the host wizard, select **aws**; while no credentials are saved the row reads `setup needed` and the Connect AWS pane opens, with **Open the IAM console** landing on the users page. Paste both values, the access key id and the secret access key, and click **Check credentials**. The app authenticates against the API with the pasted keys, and only a passing check saves them: the pane says "Works. Saved to your keychain." and the row flips to `ready`. A failure is shown verbatim, and nothing is stored.

The keys live in your system keychain from then on. You are ready to host; continue with the [quickstart](../../quickstart.md#host-on-the-internet-with-digitalocean), picking aws in the wizard instead.

## 5. Optional: a bucket and a second key, for recording

[Recording a cloud session](../recording.md) writes takes to an S3 bucket in your own account, and the key that writes them must not be the key from step 3. Launching a recorded session writes this key into the session machine's user data, so it has to be a key whose worst case is junk in one bucket prefix: the key from step 3 can start and destroy EC2 instances. Leave recording off and everything above is all there is.

1. In the S3 console, **Create bucket**, in the same region you host in, with the default settings. Give recordings a bucket that holds nothing else: the lifecycle permission below is bucket-wide.
2. Create a second IAM user, `jamstream-recording`, the same way as step 2, with only this policy. Name your bucket in both places:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "JamstreamWriteRecordings",
      "Effect": "Allow",
      "Action": [
        "s3:PutObject",
        "s3:GetObject",
        "s3:DeleteObject",
        "s3:AbortMultipartUpload"
      ],
      "Resource": "arn:aws:s3:::YOUR-BUCKET/jamstream/recordings/*"
    },
    {
      "Sid": "JamstreamRetentionRule",
      "Effect": "Allow",
      "Action": [
        "s3:GetLifecycleConfiguration",
        "s3:PutLifecycleConfiguration"
      ],
      "Resource": "arn:aws:s3:::YOUR-BUCKET"
    },
    {
      "Sid": "JamstreamFindTakes",
      "Effect": "Allow",
      "Action": "s3:ListBucket",
      "Resource": "arn:aws:s3:::YOUR-BUCKET",
      "Condition": {
        "StringLike": {
          "s3:prefix": "jamstream/recordings/*"
        }
      }
    }
  ]
}
```

3. Give that user its own access key, exactly as in step 3.

The key can read, write and delete under one prefix of one bucket, list what is there, and read and set that bucket's expiry rules. It cannot see anything outside `jamstream/recordings/`, and it cannot touch EC2.

Why each one is there. `PutObject` uploads the take. `GetObject` is how the app and `jamstream recordings` download it again, which is the whole point of a bucket you own. `ListBucket` is how either of them finds a take in the first place, and its condition on the prefix is what keeps the rest of the bucket invisible. `DeleteObject` is because arming a session writes one small probe object and removes it, which is how a bucket that refuses the key fails while you are configuring rather than mid-song. The two lifecycle actions are bucket-wide, which is the other reason recordings want a bucket of their own, and both are needed because setting a rule replaces the bucket's whole list, so the rules already there are read and written back with the new one. Grant only the `Put` half and arming a session says retention could not be applied, and nothing will delete the takes for you.

One thing to know rather than worry about. This same key is written into each session machine so it can upload, so anything that key can do, a compromised session machine could do to that prefix. It could already delete your takes before it could read them, which is the worse of the two, so `GetObject` widens that less than it looks. If you would rather the machine could only write, make a second key with `PutObject` and `AbortMultipartUpload` alone for recording and keep this one for the app; nothing in JamStream requires them to be the same key. The two lifecycle actions are bucket-wide, which is the other reason recordings want a bucket of their own. Both are needed: setting a rule replaces the bucket's whole list, so the rules already there are read and written back with the new one. Grant only the `Put` half and arming a session says retention could not be applied, and nothing will delete the takes for you.

Paste both values into **Settings**, then **Recording**, in the app, and click Check. The app keeps this key in a keychain slot of its own, so the two AWS keys never stand in for each other.

From the terminal the pair goes in `JAMSTREAM_RECORDING_ACCESS_KEY_ID` and `JAMSTREAM_RECORDING_SECRET_ACCESS_KEY`. `AWS_ACCESS_KEY_ID` is deliberately not read for recording, for the reason at the top of this section; if only that pair is set, the launch says so rather than handing the machine your launch key. [`jamstream recordings`](../../cli/recordings.md#the-storage-key) covers every provider.

## For the CLI and automation

The CLI reads the keys from the environment instead:

```console
$ export AWS_ACCESS_KEY_ID=AKIA...
$ export AWS_SECRET_ACCESS_KEY=your_secret_here
$ jamstream sweep --dry-run --provider aws
No jamstream-tagged instances found.
```

That output means the key authenticates and can list instances. JamStream reads exactly these two variables; it does not read `~/.aws/config` profiles in the current build. The app reads the same variables as a silent fallback, so a machine set up this way is `ready` in the wizard with nothing pasted.
