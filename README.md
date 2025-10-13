# PVE ESXi Import Tools - SSH Streaming Branch (direct-send)

## 🚀 Performance Breakthrough: 2.27x Faster with SSH Streaming

This branch implements **SSH+dd streaming** as the primary data transfer method, bypassing ESXi's HTTP API throttling and achieving **90.9 MB/s** transfer speeds (compared to 40 MB/s with HTTP).

## ⚡ Performance Comparison

| Method | Speed | Time for 30GB VM | Notes |
|--------|-------|------------------|-------|
| **SSH Streaming (this branch)** | **90.9 MB/s** | **5.5 minutes** | ✅ **Default** |
| HTTP API (original) | 40 MB/s | 12.5 minutes | Available with `--use-http` |
| Direct ssh+dd | 103 MB/s | 4.8 minutes | Theoretical maximum |

**SSH streaming achieves 88% of direct SSH efficiency while maintaining full FUSE compatibility!**

## 🔧 How It Works

### SSH+dd Streaming Architecture

Instead of using ESXi's HTTP API (which is rate-limited), this branch:

1. **Direct datastore access**: Uses SSH to run `dd` directly on ESXi's `/vmfs/volumes/` filesystem
2. **Large block transfers**: Reads data in 1MB blocks (instead of 1-byte blocks)
3. **Connection pooling**: Maintains 8 concurrent SSH connections for parallel reads
4. **FUSE integration**: Seamlessly integrates with Proxmox's FUSE-based import system

### Architecture Diagram

```
Traditional HTTP:
PVE → HTTP API → ESXi HTTP Server (throttled) → Datastore
                 ↑ Bottleneck: 40 MB/s

SSH Streaming (this branch):
PVE → SSH → dd command → Direct VMFS access → Datastore
            ↑ Fast: 90.9 MB/s
```

### Key Optimizations

1. **Direct filesystem access** - Bypasses ESXi's HTTP server entirely
2. **Optimized dd block size** - Uses 1MB blocks for maximum throughput
3. **Reduced connection overhead** - 8 connections instead of 16 (fewer SSH processes)
4. **Smart byte alignment** - Handles arbitrary byte offsets efficiently

## 📋 Prerequisites

### 1. SSH Key Authentication

SSH key authentication must be set up between your PVE host and ESXi host:

```bash
# On PVE host:
# Generate SSH key if you don't have one
ssh-keygen -t rsa -b 4096 -f /root/.ssh/id_rsa -N ''

# Display the public key
cat /root/.ssh/id_rsa.pub
```

Then on your **ESXi host** (via SSH or console):

```bash
# Add the PVE public key to authorized_keys
echo "ssh-rsa AAAAB3Nza... root@pve" >> /etc/ssh/keys-root/authorized_keys

# Make it persistent across reboots
/sbin/auto-backup.sh
```

**Test the connection:**
```bash
# From PVE host - should connect without password:
ssh root@your-esxi-host "hostname"
```

### 2. ESXi SSH Access

Ensure SSH is enabled on your ESXi host:
- ESXi Web UI → Host → Actions → Services → Enable Secure Shell (SSH)

## 🔨 Building from Source

### Prerequisites

Install build dependencies on your **PVE host**:

```bash
apt-get update
apt-get install -y \
  build-essential \
  cargo \
  rustc \
  libssl-dev \
  pkg-config \
  libfuse3-dev \
  git
```

### Clone and Build

```bash
# Clone the repository
cd /root
git clone https://github.com/PwrBank/pve-esxi-import-tools.git
cd pve-esxi-import-tools

# Checkout the direct-send branch
git checkout direct-send

# Build the release binary (takes ~5 minutes)
cargo build --release

# The binary will be at:
# target/release/esxi-folder-fuse (approximately 74MB)
```

**Note**: The build automatically fetches Proxmox-specific dependencies from Git:
- `proxmox-async` from https://git.proxmox.com/git/proxmox.git
- `proxmox-fuse` from https://git.proxmox.com/git/proxmox-fuse.git
- `proxmox-http` from https://git.proxmox.com/git/proxmox.git

### Install the Binary

```bash
# Stop any running imports
pkill -f esxi-folder-fuse

# Backup the original binary
cp /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse \
   /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse.backup-$(date +%Y%m%d)

# Install the new SSH-enabled binary
rm -f /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse
cp target/release/esxi-folder-fuse /usr/libexec/pve-esxi-import-tools/
chmod +x /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse

echo "✅ Installation complete!"
```

### Verify Installation

```bash
# Check version
/usr/libexec/pve-esxi-import-tools/esxi-folder-fuse --version

# Check binary size (should be ~74MB)
ls -lh /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse

# View help (should show --use-http option)
/usr/libexec/pve-esxi-import-tools/esxi-folder-fuse --help | grep use-http
```

Expected output:
```
1.0.1
-rwxr-xr-x 1 root root 74M Oct 13 12:03 /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse
  --use-http                  use HTTP API instead of SSH+dd streaming (SSH is default)
```

## 🎯 Usage

### Proxmox GUI (Recommended)

**No configuration needed!** The Proxmox GUI will automatically use SSH streaming mode at 90 MB/s.

Simply use the standard ESXi import:
1. Navigate to Datacenter → Storage → Add → ESXi
2. Enter your ESXi host details
3. Import VMs as normal - SSH streaming is used automatically

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

# Adjust SSH connection count (default: 8):
/usr/libexec/pve-esxi-import-tools/esxi-folder-fuse \
  --ssh-connections 16 \
  --user root \
  10.10.5.67 \
  /path/to/manifest.json \
  /mnt/esxi
```

### Performance Tuning Options

```bash
--ssh-connections=COUNT     # Number of concurrent SSH connections (default: 8)
--cache-page-size=BYTES     # Cache page size (default: 134217728 = 128MB)
--cache-page-count=COUNT    # Number of cache pages (default: 16)
```

## 🧪 Testing & Validation

### Quick Performance Test

Test the SSH streaming performance:

```bash
# Mount with SSH mode
/usr/libexec/pve-esxi-import-tools/esxi-folder-fuse \
  --user root \
  10.10.5.67 \
  /path/to/manifest.json \
  /tmp/test-mount &

sleep 5

# Test read speed (1GB test)
time dd if=/tmp/test-mount/datacenter/datastore/vm/disk-flat.vmdk \
        of=/dev/null bs=128M count=8

# Expected result: ~90 MB/s, ~12 seconds for 1GB
```

### Compare HTTP vs SSH

```bash
# Test HTTP mode
time /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse \
  --use-http \
  --user root --password yourpass \
  10.10.5.67 manifest.json /tmp/http-mount &

# Test SSH mode (default)
time /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse \
  --user root \
  10.10.5.67 manifest.json /tmp/ssh-mount &

# Run read tests on both mounts and compare speeds
```

## 📊 Real-World Results

### Test Environment
- **ESXi Version**: 8.0.2 (Build 23305546)
- **PVE Version**: 9.0.10
- **Network**: 1 Gbps
- **Storage**: TrueNAS iSCSI (network storage)

### Performance Metrics

| Test | HTTP Mode | SSH Mode | Improvement |
|------|-----------|----------|-------------|
| 1GB read | 25 seconds (40 MB/s) | 12 seconds (90.9 MB/s) | **2.27x faster** |
| 30GB VM import | 12.5 minutes | 5.5 minutes | **2.27x faster** |
| CPU usage | ~1% | ~1-2% | Minimal increase |
| Memory usage | 1.8 GB | 1.8 GB | Same |

### SSH Performance Details

- **Direct ssh+dd**: 103 MB/s (theoretical maximum)
- **SSH through FUSE**: 90.9 MB/s (88% efficiency)
- **Efficiency loss**: 12% due to FUSE layer overhead
- **SSH connections used**: Typically 1-2 concurrent (sequential reads)

## 🔍 Technical Details

### Implementation Files

1. **`src/ssh_client.rs`** - SSH+dd streaming client
   - Connection pooling with Semaphore
   - Optimized dd command generation (1MB blocks)
   - Byte-offset alignment handling

2. **`src/client.rs`** - Unified DatastoreClient enum
   - Wraps both HTTP and SSH backends
   - Transparent switching between modes

3. **`src/fs.rs`** - FUSE filesystem integration
   - Updated to use DatastoreClient abstraction
   - Maintains full FUSE compatibility

4. **`src/main.rs`** - Command-line argument parsing
   - SSH is default mode
   - --use-http flag for HTTP fallback

### Why SSH is Faster

1. **No HTTP overhead**: Eliminates HTTP protocol parsing and headers
2. **Direct filesystem access**: Reads directly from `/vmfs/volumes/`
3. **Bypasses ESXi throttling**: ESXi's HTTP server rate limits don't apply
4. **Larger block sizes**: 1MB blocks vs variable HTTP chunk sizes
5. **Lower latency**: Direct socket communication vs HTTP request/response cycles

### SSH Block Size Optimization

The critical optimization was changing dd block size:

```rust
// Before (broken): bs=1 (1 byte blocks)
// Result: 0.8 MB/s, 22 minutes for 1GB

// After (optimized): bs=1M (1MB blocks)
// Result: 90.9 MB/s, 12 seconds for 1GB

// Improvement: 113x faster!
```

## ⚙️ Troubleshooting

### SSH Key Authentication Not Working

```bash
# Test SSH connection manually
ssh -v root@your-esxi-host

# Ensure key is in the right location on ESXi
ssh root@your-esxi-host "cat /etc/ssh/keys-root/authorized_keys"

# Make sure auto-backup was run on ESXi
ssh root@your-esxi-host "/sbin/auto-backup.sh"
```

### Slow Performance

If you're not seeing 90 MB/s speeds:

1. **Check network bandwidth**: `iperf3` between PVE and ESXi
2. **Verify storage backend**: Local NVMe/SSD is faster than iSCSI
3. **Check SSH encryption overhead**: Try fewer connections
4. **Monitor ESXi load**: High CPU/IO can slow transfers

### Fallback to HTTP Mode

If SSH mode isn't working, use HTTP mode:

```bash
/usr/libexec/pve-esxi-import-tools/esxi-folder-fuse \
  --use-http \
  --user root \
  --password yourpassword \
  --skip-cert-verification \
  10.10.5.67 \
  manifest.json \
  /mnt/esxi
```

### Check Logs

```bash
# View FUSE mount logs
journalctl -t esxi-folder-fuse --since "10 minutes ago"

# Should show: "Using SSH+dd streaming mode"
```

## 🔄 Reverting to Original

To revert to the original Proxmox binary:

```bash
# Restore from backup
mv /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse.backup-YYYYMMDD \
   /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse

# Or reinstall from Proxmox packages
apt-get install --reinstall pve-esxi-import-tools
```

## 📝 Version History

### v1.0.1 - direct-send branch (2025-10-13)

- ✅ **Implemented SSH+dd streaming** as primary transfer method
- ✅ **Optimized dd block size** from 1 byte to 1MB (113x improvement)
- ✅ **Reduced SSH connections** from 16 to 8 for efficiency
- ✅ **Made SSH the default mode** for automatic use by Proxmox GUI
- ✅ **Achieved 90.9 MB/s** transfer speed (2.27x faster than HTTP)
- ✅ **Full backward compatibility** with `--use-http` fallback

### v1.0.1 - performance branch (2025-10-08)

- Increased HTTP concurrent connections from 4 to 16
- Increased cache page size from 32 MB to 128 MB
- Increased cache page count from 8 to 16
- Achieved ~40 MB/s with HTTP mode

## 🤝 Contributing

Contributions welcome! Areas for improvement:

- **SSH connection reuse**: Keep persistent SSH connections to reduce process spawn overhead
- **Cipher optimization**: Test different SSH ciphers (aes128-ctr vs aes256-ctr)
- **Parallel reads**: Optimize cache to trigger more concurrent SSH streams
- **Compression**: Test SSH compression for network-constrained environments

## 📄 License

AGPL-3

## 👥 Authors

- **Original**: Wolfgang Bumiller <w.bumiller@proxmox.com>
- **Original**: Proxmox Development Team <support@proxmox.com>
- **HTTP Optimizations**: Performance branch (2025-10-08)
- **SSH Streaming**: direct-send branch (2025-10-13)

---

**Note**: This is the `direct-send` branch with SSH streaming. For the HTTP-optimized version, see the `performance` branch.
