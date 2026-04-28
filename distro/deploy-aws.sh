#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROFILE="${AWS_PROFILE:-experimental-admin}"
REGION="${AWS_REGION:-us-east-1}"
BUCKET="${DAIMONOS_S3_BUCKET:-daimonos-images}"
AMI_NAME="daimonos-$(date +%Y%m%d-%H%M%S)"
KEY_FILE="${SSH_KEY_FILE:-$HOME/.ssh/id_ed25519.pub}"
INSTANCE_TYPE="${INSTANCE_TYPE:-t3.micro}"

# Prefer Buildroot AWS image, fall back to legacy
BR_IMAGE="$SCRIPT_DIR/buildroot/output/images/disk-aws.img"
ALPINE_IMAGE="$SCRIPT_DIR/build/daimonos.raw"

if [ -f "$BR_IMAGE" ]; then
    IMAGE="$BR_IMAGE"
elif [ -f "$ALPINE_IMAGE" ]; then
    IMAGE="$ALPINE_IMAGE"
    echo "WARNING: Using legacy Alpine image. Run build-buildroot.sh for the current image."
else
    echo "ERROR: No image found."
    echo "  Buildroot: $BR_IMAGE  (run ./build-buildroot.sh)"
    echo "  Alpine:    $ALPINE_IMAGE  (run ./build.sh)"
    exit 1
fi

echo "=== daimonos AWS deployment ==="
echo "Profile:  $PROFILE"
echo "Region:   $REGION"
echo "AMI name: $AMI_NAME"
echo "Image:    $IMAGE ($(du -h "$IMAGE" | cut -f1))"
echo ""

# ── Inject SSH key into image before upload ──
if [ -f "$KEY_FILE" ]; then
    echo "Injecting SSH public key from $KEY_FILE..."
    DEPLOY_IMAGE="$SCRIPT_DIR/build/daimonos-deploy.raw"
    cp "$IMAGE" "$DEPLOY_IMAGE"

    LOOP_DEV=$(sudo losetup --find --show --partscan "$DEPLOY_IMAGE")
    PART="${LOOP_DEV}p1"
    if [ ! -b "$PART" ]; then
        PART="$LOOP_DEV"
    fi

    MNT=$(mktemp -d)
    sudo mount "$PART" "$MNT"
    sudo mkdir -p "$MNT/home/agent/.ssh"
    sudo cp "$KEY_FILE" "$MNT/home/agent/.ssh/authorized_keys"
    sudo chmod 600 "$MNT/home/agent/.ssh/authorized_keys"
    sudo chown 1000:1000 "$MNT/home/agent/.ssh/authorized_keys"
    sudo umount "$MNT"
    sudo losetup -d "$LOOP_DEV"
    rmdir "$MNT"

    IMAGE="$DEPLOY_IMAGE"
    echo "SSH key injected."
else
    echo "WARNING: No SSH key found at $KEY_FILE"
    echo "You'll need to inject a key manually before connecting."
fi

# ── Create S3 bucket if needed ──
if ! aws s3api head-bucket --bucket "$BUCKET" --profile "$PROFILE" --region "$REGION" 2>/dev/null; then
    echo "Creating S3 bucket: $BUCKET..."
    aws s3api create-bucket \
        --bucket "$BUCKET" \
        --profile "$PROFILE" \
        --region "$REGION" 2>/dev/null || true
fi

# ── Upload image to S3 ──
S3_KEY="images/${AMI_NAME}.raw"
echo "Uploading to s3://$BUCKET/$S3_KEY..."
aws s3 cp "$IMAGE" "s3://$BUCKET/$S3_KEY" \
    --profile "$PROFILE" \
    --region "$REGION"

# ── Create vmimport service role if needed ──
ROLE_EXISTS=$(aws iam get-role --role-name vmimport --profile "$PROFILE" 2>/dev/null && echo "yes" || echo "no")
if [ "$ROLE_EXISTS" = "no" ]; then
    echo "Creating vmimport IAM role..."
    aws iam create-role \
        --role-name vmimport \
        --assume-role-policy-document '{
            "Version": "2012-10-17",
            "Statement": [{
                "Effect": "Allow",
                "Principal": {"Service": "vmie.amazonaws.com"},
                "Action": "sts:AssumeRole",
                "Condition": {
                    "StringEquals": {"sts:ExternalId": "vmimport"}
                }
            }]
        }' \
        --profile "$PROFILE"

    aws iam put-role-policy \
        --role-name vmimport \
        --policy-name vmimport-s3 \
        --policy-document "{
            \"Version\": \"2012-10-17\",
            \"Statement\": [{
                \"Effect\": \"Allow\",
                \"Action\": [\"s3:GetBucketLocation\", \"s3:GetObject\", \"s3:ListBucket\"],
                \"Resource\": [\"arn:aws:s3:::${BUCKET}\", \"arn:aws:s3:::${BUCKET}/*\"]
            }, {
                \"Effect\": \"Allow\",
                \"Action\": [\"ec2:ModifySnapshotAttribute\", \"ec2:CopySnapshot\", \"ec2:RegisterImage\", \"ec2:Describe*\"],
                \"Resource\": \"*\"
            }]
        }" \
        --profile "$PROFILE"

    echo "Waiting for IAM role propagation..."
    sleep 10
fi

# ── Import as EBS snapshot (bypasses kernel version check) ──
echo "Importing disk as EBS snapshot..."
IMPORT_TASK=$(aws ec2 import-snapshot \
    --description "$AMI_NAME" \
    --disk-container "{
        \"Description\": \"daimonos root\",
        \"Format\": \"raw\",
        \"UserBucket\": {
            \"S3Bucket\": \"$BUCKET\",
            \"S3Key\": \"$S3_KEY\"
        }
    }" \
    --profile "$PROFILE" \
    --region "$REGION" \
    --output json)

TASK_ID=$(echo "$IMPORT_TASK" | python3 -c "import sys,json; print(json.load(sys.stdin)['ImportTaskId'])")
echo "Import task: $TASK_ID"

echo "Waiting for snapshot import (this can take 10-30 minutes)..."
while true; do
    STATUS=$(aws ec2 describe-import-snapshot-tasks \
        --import-task-ids "$TASK_ID" \
        --profile "$PROFILE" \
        --region "$REGION" \
        --output json)

    STATE=$(echo "$STATUS" | python3 -c "import sys,json; t=json.load(sys.stdin)['ImportSnapshotTasks'][0]['SnapshotTaskDetail']; print(t['Status'])")
    PROGRESS=$(echo "$STATUS" | python3 -c "import sys,json; t=json.load(sys.stdin)['ImportSnapshotTasks'][0]['SnapshotTaskDetail']; print(t.get('Progress', '?'))")

    echo "  Status: $STATE  Progress: $PROGRESS%"

    if [ "$STATE" = "completed" ]; then
        SNAPSHOT_ID=$(echo "$STATUS" | python3 -c "import sys,json; t=json.load(sys.stdin)['ImportSnapshotTasks'][0]['SnapshotTaskDetail']; print(t['SnapshotId'])")
        break
    elif [ "$STATE" = "deleted" ] || [ "$STATE" = "deleting" ] || [ "$STATE" = "error" ]; then
        MSG=$(echo "$STATUS" | python3 -c "import sys,json; t=json.load(sys.stdin)['ImportSnapshotTasks'][0]['SnapshotTaskDetail']; print(t.get('StatusMessage', 'unknown'))")
        echo "ERROR: Snapshot import failed: $MSG"
        exit 1
    fi

    sleep 30
done

echo "Snapshot: $SNAPSHOT_ID"

# ── Register AMI from snapshot ──
echo "Registering AMI..."
AMI_ID=$(aws ec2 register-image \
    --name "$AMI_NAME" \
    --description "daimonos agent OS" \
    --architecture x86_64 \
    --root-device-name /dev/sda1 \
    --block-device-mappings "[{
        \"DeviceName\": \"/dev/sda1\",
        \"Ebs\": {
            \"SnapshotId\": \"$SNAPSHOT_ID\",
            \"VolumeSize\": 10,
            \"VolumeType\": \"gp3\",
            \"DeleteOnTermination\": true
        }
    }]" \
    --virtualization-type hvm \
    --boot-mode legacy-bios \
    --ena-support \
    --profile "$PROFILE" \
    --region "$REGION" \
    --output text \
    --query 'ImageId')

echo ""
echo "=== Import complete ==="
echo "AMI ID: $AMI_ID"

# ── Tag the AMI ──
aws ec2 create-tags \
    --resources "$AMI_ID" \
    --tags "Key=Name,Value=$AMI_NAME" "Key=Project,Value=daimonos" \
    --profile "$PROFILE" \
    --region "$REGION"

echo ""
echo "To launch an instance:"
echo "  aws ec2 run-instances \\"
echo "    --image-id $AMI_ID \\"
echo "    --instance-type $INSTANCE_TYPE \\"
echo "    --key-name YOUR_KEY_NAME \\"
echo "    --security-groups daimonos-sg \\"
echo "    --profile $PROFILE \\"
echo "    --region $REGION"
echo ""
echo "  --key-name is required: the instance fetches this key pair's public key"
echo "  from IMDSv2 at boot and merges it with any static keys in the image."
echo ""
echo "To connect:"
echo "  ssh -i ~/.ssh/YOUR_KEY agent@INSTANCE_IP"
