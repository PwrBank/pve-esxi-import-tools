# PVE ESXi Import Tools - v1.1.1 Installation Guide

## 📦 Final Release Binaries

| Binary | Size | PVE Version | Built On | MD5 Checksum |
|--------|------|-------------|----------|--------------|
| `esxi-folder-fuse-v1.1.1-final-pve8.4.14` | 3.2M | PVE 8.4.14 | 10.10.110.30 | `76796ab448cdd798b4a3b5adc47a489d` |
| `esxi-folder-fuse-v1.1.1-final-pve9.0.10` | 3.2M | PVE 9.0.10 | 10.10.5.69 | `b0ee2ee729c8b894da07ad353b0e078e` |

### SHA-256 Checksums

```
427703c14e47668bd3082c84a30a7b45b1f243861598f651684303244ad4218e  esxi-folder-fuse-v1.1.1-final-pve8.4.14
7846bc695288c7555a1c7db6d5714c8b3826e6b0488899fcf21048a34a155962  esxi-folder-fuse-v1.1.1-final-pve9.0.10
```

## ⚡ Performance Highlights

- **Speed**: 71-120 MB/s (SSH streaming mode)
- **Concurrent connections**: 16 (default, adjustable with `--ssh-connections`)
- **Improvement over HTTP**: 2.5-3x faster (vs 40 MB/s)
- **Binary size**: 3.2MB (stripped from 65-74MB)

## 📋 Prerequisites

### 1. SSH Key Authentication (Required for Default SSH Mode)

**CRITICAL**: SSH streaming mode (the default) requires SSH key authentication. The tool does **not** automatically fall back to HTTP mode if SSH keys are missing - you must explicitly use `--use-http` for password-based authentication.

SSH key authentication must be configured between PVE and ESXi:

```bash
# On PVE host (run as root):
ssh-keygen -t rsa -b 4096 -f /root/.ssh/id_rsa -N ''
cat /root/.ssh/id_rsa.pub
```

**On ESXi host** (via SSH or web console):

```bash
# Add PVE public key
echo "ssh-rsa AAAAB3Nza... root@pve" >> /etc/ssh/keys-root/authorized_keys

# Make persistent across reboots
/sbin/auto-backup.sh
```

**Test connection**:
```bash
# From PVE - should connect without password
ssh root@your-esxi-host "hostname"
```

**If you cannot use SSH keys**, you must use HTTP mode by adding `--use-http` flag when using the command line, or the tool will fail. For Proxmox GUI usage with HTTP mode, you would need to modify the source code (not recommended - SSH mode is 2.5-3x faster).

### 2. ESXi Configuration

1. **Enable SSH** on ESXi host (if not already enabled)
2. **Password file**: Create `/etc/pve/priv/storage/esxi-host.pw` with ESXi root password:
   ```bash
   # On PVE host (use Python to avoid shell escaping issues):
   python3 << 'EOF'
   with open('/etc/pve/priv/storage/esxi-host.pw', 'w') as f:
       f.write('YOUR_ESXI_PASSWORD')
   import os
   os.chmod('/etc/pve/priv/storage/esxi-host.pw', 0o600)
   EOF
   ```

### 3. Proxmox Storage Configuration

Add ESXi storage in `/etc/pve/storage.cfg`:

```
esxi: esxi-host
	server YOUR_ESXI_IP
	username root
	content import
	skip-cert-verification 1
```

## 🔧 Installation

### For PVE 8.x:

```bash
# Download the binary (adjust path as needed)
cd /root
wget https://github.com/PwrBank/pve-esxi-import-tools/raw/direct-send/builds/esxi-folder-fuse-v1.1.1-final-pve8.4.14

# Or copy from your builds directory
cp /path/to/esxi-folder-fuse-v1.1.1-final-pve8.4.14 /root/

# Verify checksum
echo "76796ab448cdd798b4a3b5adc47a489d  esxi-folder-fuse-v1.1.1-final-pve8.4.14" | md5sum -c

# Stop any running imports
pkill -9 -f esxi-folder-fuse || true

# Backup original binary
cp /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse \
   /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse.backup-$(date +%Y%m%d)

# Install
rm -f /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse
cp esxi-folder-fuse-v1.1.1-final-pve8.4.14 /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse
chmod +x /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse

echo "✅ Installation complete for PVE 8!"
```

### For PVE 9.x:

```bash
# Download the binary (adjust path as needed)
cd /root
wget https://github.com/PwrBank/pve-esxi-import-tools/raw/direct-send/builds/esxi-folder-fuse-v1.1.1-final-pve9.0.10

# Or copy from your builds directory
cp /path/to/esxi-folder-fuse-v1.1.1-final-pve9.0.10 /root/

# Verify checksum
echo "b0ee2ee729c8b894da07ad353b0e078e  esxi-folder-fuse-v1.1.1-final-pve9.0.10" | md5sum -c

# Stop any running imports
pkill -9 -f esxi-folder-fuse || true

# Backup original binary
cp /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse \
   /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse.backup-$(date +%Y%m%d)

# Install
rm -f /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse
cp esxi-folder-fuse-v1.1.1-final-pve9.0.10 /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse
chmod +x /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse

echo "✅ Installation complete for PVE 9!"
```

## ✅ Verification

```bash
# Check version
/usr/libexec/pve-esxi-import-tools/esxi-folder-fuse --version
# Expected: 1.1.1

# Check binary size
ls -lh /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse
# Expected: 3.2M

# Check SSH connections option
/usr/libexec/pve-esxi-import-tools/esxi-folder-fuse --help | grep ssh-connections
# Expected: --ssh-connections=COUNT     number of concurrent SSH connections (default: 16)

# Test SSH authentication
ssh root@your-esxi-host "echo SSH works"
# Should connect without password prompt
```

## 🎯 Usage

### Proxmox GUI (Recommended)

Simply use the standard ESXi import in Proxmox GUI:
1. Navigate to **Datacenter → Storage**
2. Select your **ESXi storage**
3. Click **Content → Import**
4. Select VM and import

**SSH streaming mode is used automatically!** You should see **71-120 MB/s** transfer speeds.

### Command Line

```bash
# SSH mode (default - 71-120 MB/s):
/usr/libexec/pve-esxi-import-tools/esxi-folder-fuse \
  --user root \
  YOUR_ESXI_IP \
  /path/to/manifest.json \
  /mnt/esxi

# Adjust connections for faster networks:
/usr/libexec/pve-esxi-import-tools/esxi-folder-fuse \
  --ssh-connections 24 \
  --user root \
  YOUR_ESXI_IP \
  /path/to/manifest.json \
  /mnt/esxi

# HTTP fallback mode (40 MB/s):
/usr/libexec/pve-esxi-import-tools/esxi-folder-fuse \
  --use-http \
  --user root \
  --password yourpass \
  YOUR_ESXI_IP \
  /path/to/manifest.json \
  /mnt/esxi
```

## 🔍 Troubleshooting

### "failed to login" Error

**Cause**: Password file issue or Proxmox validation failure

**Solution**:
```bash
# Ensure password file exists and has correct password
python3 << 'EOF'
with open('/etc/pve/priv/storage/esxi-host.pw', 'w') as f:
    f.write('YOUR_ACTUAL_ESXI_PASSWORD')
import os
os.chmod('/etc/pve/priv/storage/esxi-host.pw', 0o600)
EOF

# Test with listvms.py
/usr/libexec/pve-esxi-import-tools/listvms.py \
  --skip-cert-verification \
  YOUR_ESXI_IP \
  root \
  /etc/pve/priv/storage/esxi-host.pw
```

### "Device or resource busy" Error

**Cause**: VM is running on ESXi (VMDK file is locked)

**Solution**: **Power off the VM** on ESXi before importing

### Slow Performance

If not seeing expected speeds:

1. **Check network bandwidth**: `iperf3 -s` on ESXi, `iperf3 -c ESXI_IP` on PVE
2. **Increase SSH connections**: Try `--ssh-connections=24` or `32`
3. **Check ESXi load**: High CPU/IO can throttle transfers
4. **Verify SSH keys**: `ssh root@esxi-host "dd if=/vmfs/volumes/datastore/file of=/dev/null bs=1M count=100"`

### View Logs

```bash
# Check FUSE logs
journalctl -t esxi-folder-fuse --since "10 minutes ago"

# Should show: "Using SSH+dd streaming mode (default) with 16 concurrent connections"
```

## 🔄 Reverting to Original

```bash
# Restore from backup
mv /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse.backup-YYYYMMDD \
   /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse

# Or reinstall from Proxmox packages
apt-get install --reinstall pve-esxi-import-tools
```

## 📊 Build Information

- **Version**: 1.1.1
- **Branch**: direct-send
- **Built**: 2025-10-14
- **Compiler**: rustc 1.85 (Rust 2024 edition)
- **Optimizations**:
  - Debug symbols stripped (95% size reduction)
  - 1MB dd block size
  - 16 concurrent SSH connections (default)
  - Root privilege retention for SSH key access
  - IsDirectory error preservation for FUSE

## 📝 Changelog

### v1.1.1 (2025-10-14)
- ✅ Increased default SSH connections from 8 to 16
- ✅ Fixed IsDirectory error handling for FUSE directory traversal
- ✅ Retained root privileges in SSH mode for key access
- ✅ Fixed authentication fallback when password provided
- ✅ Performance: 71-120 MB/s sustained (was 71-110 MB/s with fluctuation)

### v1.1.0 (2025-10-13)
- ✅ Implemented SSH+dd streaming as primary method
- ✅ Optimized dd block size (1MB)
- ✅ Made SSH default mode
- ✅ Achieved 90.9 MB/s average (2.27x faster than HTTP)

## 📄 License

AGPL-3

## 👥 Support

For issues, please check the [main README](../README.md) or create an issue on GitHub.

---

**🚀 Enjoy 2.5-3x faster ESXi imports with SSH streaming!**
