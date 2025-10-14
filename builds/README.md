# ESXi Folder FUSE - SSH Streaming Builds

## Version 1.1.1 - direct-send Branch (Latest)

These optimized binaries implement **SSH+dd streaming** for 2.27x faster ESXi VM imports (90.9 MB/s vs 40 MB/s HTTP).

## Available Binaries

| Binary | Size | PVE Version | Built On | MD5 Checksum | Deployed |
|--------|------|-------------|----------|--------------|----------|
| `esxi-folder-fuse-v1.1.1-pve8.4.14` | 3.2M | PVE 8.4.14 | 10.10.110.30 | `a6979786522e4d51fc8ff5def6e26f95` | ✅ Yes |
| `esxi-folder-fuse-v1.1.1-pve9.0.10` | 3.2M | PVE 9.0.10 | 10.10.5.69 | `3e00b8be5602f405378cb1dc816215b7` | ✅ Yes |

### SHA-256 Checksums

```
371ec01ee9270e69a8806a0a6fc0b4ffbf9d5ff1d47d3416228eacb089686719  esxi-folder-fuse-v1.1.1-pve8.4.14
8bed99a28d86f21de8eac6b46a65a6b562302189dc95e4390e6659fe04489b84  esxi-folder-fuse-v1.1.1-pve9.0.10
```

## Installation Instructions

### Prerequisites

1. **SSH Key Authentication** must be set up between PVE and ESXi:

```bash
# On PVE host:
ssh-keygen -t rsa -b 4096 -f /root/.ssh/id_rsa -N ''
cat /root/.ssh/id_rsa.pub
```

2. **Add key to ESXi** (via SSH or console):

```bash
# On ESXi:
echo "ssh-rsa AAAAB3Nza... root@pve" >> /etc/ssh/keys-root/authorized_keys
/sbin/auto-backup.sh  # Make persistent across reboots
```

3. **Test SSH connection**:

```bash
# From PVE - should connect without password:
ssh root@your-esxi-host "hostname"
```

### Installation

**Choose the correct binary for your PVE version:**

#### For PVE 8 (pve-manager/8.x):

```bash
# Stop any running imports
pkill -f esxi-folder-fuse

# Backup original binary
cp /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse \
   /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse.backup-$(date +%Y%m%d)

# Install PVE 8 binary
rm -f /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse
cp builds/esxi-folder-fuse-v1.1.1-pve8.4.14 /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse
chmod +x /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse
```

#### For PVE 9 (pve-manager/9.x):

```bash
# Stop any running imports
pkill -f esxi-folder-fuse

# Backup original binary
cp /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse \
   /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse.backup-$(date +%Y%m%d)

# Install PVE 9 binary
rm -f /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse
cp builds/esxi-folder-fuse-v1.1.1-pve9.0.10 /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse
chmod +x /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse
```

### Verification

```bash
# Check version (should show 1.0.1)
/usr/libexec/pve-esxi-import-tools/esxi-folder-fuse --version

# Verify binary size (should be ~3.2M)
ls -lh /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse

# Check for SSH mode help text
/usr/libexec/pve-esxi-import-tools/esxi-folder-fuse --help | grep use-http
```

Expected output:
```
1.1.1
-rwxr-xr-x 1 root root 3.2M Oct 13 12:24 /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse
  --use-http                  use HTTP API instead of SSH+dd streaming (SSH is default)
```

## Usage

### Proxmox GUI (Automatic)

Simply use the standard ESXi import in the Proxmox GUI:
1. Datacenter → Storage → Add → ESXi
2. Enter ESXi host details
3. Import VMs - **SSH streaming is used automatically at 90 MB/s!**

No configuration needed - SSH mode is the default.

### Command Line

```bash
# Default mode (SSH streaming - 90 MB/s):
/usr/libexec/pve-esxi-import-tools/esxi-folder-fuse \
  --user root \
  10.10.5.67 \
  /path/to/manifest.json \
  /mnt/esxi

# Fallback to HTTP if needed (40 MB/s):
/usr/libexec/pve-esxi-import-tools/esxi-folder-fuse \
  --use-http \
  --user root \
  --password yourpassword \
  10.10.5.67 \
  /path/to/manifest.json \
  /mnt/esxi
```

## Performance

| Method | Speed | Time for 30GB VM | Notes |
|--------|-------|------------------|-------|
| **SSH Streaming (this build)** | **90.9 MB/s** | **5.5 minutes** | ✅ **Default** |
| HTTP API (original) | 40 MB/s | 12.5 minutes | Available with `--use-http` |
| Direct ssh+dd | 103 MB/s | 4.8 minutes | Theoretical maximum |

**SSH streaming achieves 88% of direct SSH efficiency while maintaining full FUSE compatibility!**

## Build Information

- **Version**: 1.1.1
- **Built**: 2025-10-13
- **Branch**: direct-send
- **Deployed To**:
  - PVE 8 (10.10.110.30 - pve-manager/8.4.14)
  - PVE 9 (10.10.5.69 - pve-manager/9.0.10)
- **Optimizations**:
  - Stripped debug symbols (95-96% size reduction)
  - 1MB dd block size for optimal throughput
  - 8 concurrent SSH connections (default)
  - Direct `/vmfs/volumes/` filesystem access

## Reverting to Original

```bash
# Restore from backup
mv /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse.backup-YYYYMMDD \
   /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse

# Or reinstall from Proxmox packages
apt-get install --reinstall pve-esxi-import-tools
```

## Troubleshooting

### SSH Connection Issues

```bash
# Test manual SSH connection
ssh -v root@your-esxi-host

# Verify key is on ESXi
ssh root@your-esxi-host "cat /etc/ssh/keys-root/authorized_keys"

# Ensure auto-backup was run
ssh root@your-esxi-host "/sbin/auto-backup.sh"
```

### View Logs

```bash
# Check FUSE mount logs
journalctl -t esxi-folder-fuse --since "10 minutes ago"

# Should show: "Using SSH+dd streaming mode"
```

### Slow Performance

If not seeing 90 MB/s:
1. Check network bandwidth with `iperf3`
2. Verify storage backend performance
3. Monitor ESXi load (CPU/IO)
4. Try fewer SSH connections: `--ssh-connections 4`

## License

AGPL-3

## Authors

- **Original**: Wolfgang Bumiller <w.bumiller@proxmox.com>
- **Original**: Proxmox Development Team <support@proxmox.com>
- **SSH Streaming**: direct-send branch (2025-10-13)
