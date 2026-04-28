# AWS Nitro Kernel Configuration Guide

> **Applies to:** Daimonos Buildroot distro running on AWS EC2 Nitro instances (t3, m5, c5, r5, etc.)
>
> **Kernel version:** 6.12.27 (Buildroot custom config)
>
> **Config file:** `distro/br2-external/board/daimonos/linux.config`

## Overview

Daimonos uses a heavily stripped custom kernel — no modules, no GPU, no USB, no wireless. Every enabled config option is intentional. This document explains the options required for AWS Nitro instances, the failure modes when they're missing, and how to verify the config before deploying.

These requirements were discovered through multiple failed deployment attempts where the instance appeared to boot (EC2 status checks passed) but was unreachable or non-functional. AWS provides almost no diagnostic output for kernel-level boot failures — the only tool is `get-console-screenshot`, which captures the VGA framebuffer.

---

## Critical Kernel Configs

### 1. NVMe Storage — `CONFIG_BLK_DEV_NVME=y`

**Why:** Nitro instances expose EBS volumes as NVMe devices (`/dev/nvme0n1`). Without this driver, the kernel cannot see the root filesystem.

**Failure mode:** Kernel panic: `VFS: Unable to mount root fs on unknown-block(0,0)`. The instance gets stuck at decompression with no serial output.

**Verification:**

```bash
grep CONFIG_BLK_DEV_NVME linux.config
# Expected: CONFIG_BLK_DEV_NVME=y
```

**Notes:**
- QEMU uses virtio block devices (`/dev/vda`), so NVMe issues only appear on real AWS hardware
- The dual-image build (`disk-qemu.img` / `disk-aws.img`) uses different `root=` paths for this reason

---

### 2. ENA Network Driver — `CONFIG_ENA_ETHERNET=y`

**Why:** Nitro instances use the Elastic Network Adapter (ENA) for networking. Without it, the instance has no network connectivity — SSH is unreachable and DHCP never completes.

**Failure mode:** Instance boots, kernel runs, but no network interface appears. `udhcpc` times out. SSH never becomes available.

**Verification:**

```bash
grep CONFIG_ENA_ETHERNET linux.config
# Expected: CONFIG_ENA_ETHERNET=y
```

**Notes:**
- QEMU uses virtio-net (`CONFIG_VIRTIO_NET=y`), which is also required for local testing
- Both drivers must be enabled since we produce images for both platforms

---

### 3. PCI MSI/MSI-X Interrupts — `CONFIG_PCI_MSI=y`

**Why:** The NVMe controller on Nitro requires MSI (Message Signaled Interrupts) or MSI-X. Without this, NVMe commands time out even if the driver is loaded.

**Failure mode:** Kernel loads the NVMe driver but I/O operations hang. The instance appears stuck after decompression — no console output, no boot progress. EC2 status checks may still pass (network-level ARP responses from the Nitro hypervisor).

**Verification:**

```bash
grep CONFIG_PCI_MSI linux.config
# Expected: CONFIG_PCI_MSI=y
```

**Notes:**
- This is the most insidious failure — the NVMe driver loads but silently fails
- Must be combined with `CONFIG_PCI=y` and `CONFIG_PCIEPORTBUS=y`

---

### 4. ACPI Hardware Discovery — `CONFIG_ACPI=y`

**Why:** Nitro uses ACPI tables to describe hardware (CPU, memory, PCI topology, NVMe controllers). Without ACPI, the kernel cannot enumerate devices.

**Failure mode:** PCI devices are not discovered. NVMe and ENA drivers never probe. Instance is effectively dead.

**Required ACPI sub-options:**

```
CONFIG_ACPI=y
CONFIG_ACPI_BUTTON=y
CONFIG_ACPI_FAN=y
CONFIG_ACPI_PROCESSOR=y
CONFIG_ACPI_THERMAL=y
```

**Verification:**

```bash
grep CONFIG_ACPI linux.config
# Should show CONFIG_ACPI=y and sub-options
```

---

### 5. VGA Console — `CONFIG_VGA_CONSOLE=y`

**Why:** Enables the EC2 console screenshot feature (`aws ec2 get-console-screenshot`), which captures the VGA framebuffer. This is the primary debugging tool for boot failures on Nitro.

**Failure mode:** `get-console-screenshot` returns a blank/black image. You lose the only visual diagnostic for kernel boot issues.

**Verification:**

```bash
grep CONFIG_VGA_CONSOLE linux.config
# Expected: CONFIG_VGA_CONSOLE=y
```

**Notes:**
- Also requires `CONFIG_DUMMY_CONSOLE=y`
- The serial console (`console=ttyS0,115200n8`) is configured in `grub.cfg` but EC2's `get-console-output` API may not capture it on all instance types

---

### 6. Serial Console — `CONFIG_SERIAL_8250=y` + `CONFIG_SERIAL_8250_CONSOLE=y`

**Why:** Serial output is the primary console for BusyBox init and daimonos boot messages. Required for `console=ttyS0,115200n8` in the kernel command line.

**Failure mode:** No serial output. EC2 Serial Console feature doesn't work. Boot messages are lost.

**Kernel command line (in `grub.cfg`):**

```
console=ttyS0,115200n8 console=tty0
```

The dual `console=` entries send output to both serial (for EC2 Serial Console) and VGA (for screenshots).

---

### 7. Hypervisor Guest Support

**Why:** Nitro is based on KVM. These options enable paravirtualization optimizations.

```
CONFIG_HYPERVISOR_GUEST=y
CONFIG_PARAVIRT=y
CONFIG_KVM_GUEST=y
```

**Failure mode:** Not a hard failure — the kernel will boot without these. But performance is worse because the guest doesn't use paravirt clocks, hypercalls, or optimized I/O paths.

---

### 8. Xen Support (for older EC2 instance types)

**Why:** Older EC2 instance types (t2, m4, c4) use the Xen hypervisor instead of Nitro/KVM. If you need to support these:

```
CONFIG_XEN=y
CONFIG_XEN_PV=y
CONFIG_XEN_BLKDEV_FRONTEND=y
CONFIG_XEN_NETDEV_FRONTEND=y
```

**Notes:**
- Not needed if you only target t3+ / m5+ / c5+ (Nitro) instances
- Kept in the current config for broad EC2 compatibility

---

### 9. Clocksource — `CONFIG_X86_TSC=y`

**Why:** Nitro instances use TSC (Time Stamp Counter) as the primary clocksource. Without it, timekeeping may fall back to slower alternatives.

**Failure mode:** Not a hard failure, but timekeeping may be inaccurate and timer-dependent operations (timeouts, cron) may misbehave.

---

### 10. SysRq — `CONFIG_MAGIC_SYSRQ=y`

**Why:** Enables `echo b > /proc/sysrq-trigger` for emergency reboots. Used by the first-boot rootfs resize script (`S20resize`) which needs to reboot after expanding the partition table.

**Failure mode:** The resize script can't reboot after phase 1, so the partition expands but the filesystem never grows. The instance ends up with a 256 MB rootfs on a 10 GB disk.

---

## Virtio Drivers (for QEMU testing)

These are required for local QEMU testing but are not used on AWS Nitro:

```
CONFIG_VIRTIO_PCI=y
CONFIG_VIRTIO_BLK=y        # /dev/vda block devices
CONFIG_VIRTIO_NET=y        # virtio network
CONFIG_VIRTIO_BALLOON=y    # memory ballooning
CONFIG_VIRTIO_MMIO=y       # MMIO transport
CONFIG_VIRTIO_CONSOLE=y    # virtio serial
CONFIG_HW_RANDOM_VIRTIO=y  # RNG
CONFIG_SCSI_VIRTIO=y       # SCSI over virtio
```

---

## Explicitly Disabled Options

These are intentionally disabled to minimize the kernel:

| Config | Reason |
|--------|--------|
| `CONFIG_MODULES` | Everything built-in — no module loading complexity |
| `CONFIG_DRM` | No GPU, no display server |
| `CONFIG_SOUND` | No audio |
| `CONFIG_USB` | No USB devices on EC2 |
| `CONFIG_INPUT_EVDEV` | No input devices beyond serial keyboard |
| `CONFIG_INPUT_MOUSE` | No mouse |
| `CONFIG_WIRELESS` | No WiFi |
| `CONFIG_WLAN` | No WiFi drivers |
| `CONFIG_NE2K_PCI` | Legacy NE2000 NIC, not needed |
| `CONFIG_8139CP` | RTL8139 NIC, not needed |

---

## Networking Stack

Required for SSH and DHCP:

```
CONFIG_NET=y
CONFIG_PACKET=y        # raw sockets, needed by udhcpc (DHCP)
CONFIG_UNIX=y          # Unix domain sockets
CONFIG_INET=y          # IPv4
CONFIG_IP_MULTICAST=y  # mDNS / multicast
CONFIG_IPV6=y          # IPv6
CONFIG_NETDEVICES=y    # network device support
```

---

## Filesystem

```
CONFIG_EXT4_FS=y         # Root filesystem
CONFIG_TMPFS=y           # /tmp, /run
CONFIG_TMPFS_POSIX_ACL=y # POSIX ACLs on tmpfs
CONFIG_PROC_FS=y         # /proc (required by init scripts)
CONFIG_SYSFS=y           # /sys (required by device management)
CONFIG_DEVTMPFS=y        # auto-populated /dev
CONFIG_DEVTMPFS_MOUNT=y  # kernel mounts devtmpfs before init
```

---

## Device Management

```
CONFIG_DEVTMPFS=y        # /dev auto-populated by kernel
CONFIG_DEVTMPFS_MOUNT=y  # mounted automatically before init
CONFIG_SCSI=y            # SCSI subsystem (NVMe uses SCSI layer)
CONFIG_BLK_DEV_SD=y      # SCSI disk support
CONFIG_ATA=y             # ATA support
CONFIG_ATA_PIIX=y        # Intel PIIX ATA controller
```

---

## AWS Import Considerations

### `import-image` vs `import-snapshot`

AWS's `import-image` API validates the kernel version against a whitelist of known distribution kernels. Custom kernel 6.12.27 is rejected with:

```
ClientError: Unsupported kernel version 6.12.27
```

**Workaround:** Use `import-snapshot` to import the raw disk as an EBS snapshot, then `register-image` to create an AMI manually. This bypasses the kernel version check.

```bash
# Import as snapshot (no kernel check)
aws ec2 import-snapshot --disk-container '{
  "Format": "raw",
  "UserBucket": {"S3Bucket": "bucket", "S3Key": "image.raw"}
}'

# Register AMI from snapshot
aws ec2 register-image \
  --name "daimonos-$(date +%Y%m%d)" \
  --root-device-name /dev/sda1 \
  --block-device-mappings '[{
    "DeviceName": "/dev/sda1",
    "Ebs": {"SnapshotId": "snap-xxx", "VolumeSize": 10, "VolumeType": "gp3"}
  }]' \
  --virtualization-type hvm \
  --boot-mode legacy-bios \
  --ena-support
```

### Boot Mode

The AMI must be registered with `--boot-mode legacy-bios` because the image uses GRUB2 with BIOS/MBR boot (not UEFI). Nitro instances support both BIOS and UEFI, but BIOS must be explicitly specified.

### Root Device Naming

- **AMI registration:** `--root-device-name /dev/sda1` (the AWS block device mapping name)
- **Inside the instance:** The actual device is `/dev/nvme0n1p1` (NVMe namespace 0, partition 1)
- **In grub.cfg:** `root=/dev/nvme0n1p1`
- **In /proc/mounts:** Shows `/dev/root` (BusyBox convention), not the actual device path

---

## Verifying the Config

### Before building

Check that all critical options are present:

```bash
cd distro/br2-external/board/daimonos

for opt in BLK_DEV_NVME ENA_ETHERNET PCI_MSI ACPI VGA_CONSOLE \
           SERIAL_8250 SERIAL_8250_CONSOLE DEVTMPFS DEVTMPFS_MOUNT \
           EXT4_FS MAGIC_SYSRQ; do
  if grep -q "CONFIG_${opt}=y" linux.config; then
    echo "OK: CONFIG_${opt}"
  else
    echo "MISSING: CONFIG_${opt}"
  fi
done
```

### After deploying

From a running instance (via MCP `exec` tool):

```bash
# Check NVMe
ls /dev/nvme*

# Check ENA
ip link show

# Check filesystem expanded
df -h /

# Check swap
free -h

# Check kernel version
uname -r
```

### Debugging a non-booting instance

1. **Console screenshot:** `aws ec2 get-console-screenshot --instance-id i-xxx`
2. **Console output:** `aws ec2 get-console-output --instance-id i-xxx` (may be empty on Nitro)
3. **Serial console:** `aws ec2-instance-connect send-serial-console-ssh-public-key ...` then SSH to `serial-console.ec2-instance-connect.<region>.aws`
4. **Status checks:** `aws ec2 describe-instance-status --instance-ids i-xxx`
   - System=ok + Instance=ok but no SSH → sshd failed (check console screenshot for errors)
   - System=ok + Instance=impaired → kernel or init issues
   - System=impaired → hardware/hypervisor issue, re-launch

---

## Change Log

| Date | Change | Issue |
|------|--------|-------|
| 2026-04-27 | Initial kernel config for Buildroot distro | CLA-208 |
| 2026-04-27 | Added `CONFIG_MAGIC_SYSRQ=y` for resize reboot | CLA-216 |
| 2026-04-27 | Documented `import-snapshot` workaround for kernel version rejection | CLA-215 |
| 2026-04-27 | Full documentation created | CLA-214 |
