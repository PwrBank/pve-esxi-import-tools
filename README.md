# PVE ESXi Import Tools - Performance Optimizations

## ⚠️ WARNING ⚠️
This should not be used in production, just for testing purposes

This has been ran successfully on a low bandwidth migration, but nothing more.

## Overview

This document describes the performance optimizations made to the ESXi import tool to address the 50% slower import speeds compared to direct `qm import`.

## Changes Made

### Version 1.1.0 (2025-10-10)

#### 1. HTTP Client Connection Pool
**Files**: `src/esxi.rs`

**Major Change**: Implemented a pool of 8 independent HTTP clients, each maintaining its own TCP connection to ESXi.

**Implementation**:
```rust
/// Pool of multiple HTTP clients to enable multiple concurrent TCP connections
pub struct EsxiClientPool {
    clients: Vec<Arc<EsxiClient>>,
    counter: AtomicUsize,
}
```

**Key Features**:
- **8 concurrent TCP connections** to ESXi (up from 1 with HTTP/2 multiplexing)
- **Round-robin distribution** of requests across clients
- **Logging** to track pool creation and usage patterns
- Each client maintains its own SSL connector and connection state

**Impact**:
- **730-773 Mbps** (91-97 MB/s) throughput on standard MTU 1500 networks
- **64% improvement** over single-connection HTTP/2 (445 Mbps)
- Successfully bypasses ESXi's per-connection rate limiting

#### 2. Increased Concurrent Request Limit
**File**: `src/esxi.rs:414`

**Change**:
```rust
// Before:
requests: tokio::sync::Semaphore::new(4),

// After:
requests: tokio::sync::Semaphore::new(16),
```

**Impact**: Increased from 4 to 16 concurrent HTTP requests per client, allowing 16 parallel requests distributed across 8 TCP connections.

#### 3. Increased Default Cache Page Size
**File**: `src/main.rs:26`

**Change**:
```rust
// Before:
static mut FILE_CACHE_PAGE_SIZE: u64 = 32 << 20;  // 32 MB

// After:
static mut FILE_CACHE_PAGE_SIZE: u64 = 128 << 20;  // 128 MB
```

**Impact**: Each HTTP request now fetches 128 MB chunks instead of 32 MB, reducing the number of round trips required and improving throughput.

#### 4. Increased Default Cache Page Count
**File**: `src/main.rs:27`

**Change**:
```rust
// Before:
static mut FILE_CACHE_PAGE_COUNT: usize = 8;

// After:
static mut FILE_CACHE_PAGE_COUNT: usize = 16;
```

**Impact**: Doubled the cache capacity from 256 MB (8 × 32 MB) to 2 GB (16 × 128 MB), allowing for better readahead caching and reduced repeated requests.

#### 5. Optimized Release Build Profile
**File**: `Cargo.toml`

**Addition**:
```toml
[profile.release]
strip = true
lto = true
codegen-units = 1
```

**Impact**:
- **Binary size reduced** from 82 MB to 3.6 MB (stripped debug symbols)
- **LTO (Link Time Optimization)** enabled for better performance
- **Single codegen unit** for maximum optimization

## Performance Analysis

### Version 1.1.0 Performance (with Client Pool)
- **TCP connections**: 7-8 concurrent connections to ESXi
- **Throughput**: 730-773 Mbps (91-97 MB/s) on MTU 1500
- **Peak rate**: 852 Mbps (106 MB/s)
- **Cache**: 2 GB total (128 MB × 16 pages)
- **Memory usage**: ~2.4 GB during active transfer

### Comparison with Previous Versions
- **Original (v1.0.0)**: 445 Mbps single connection
- **v1.1.0**: 730-773 Mbps with 7-8 connections
- **Improvement**: **64% faster** than original

### MTU Impact Analysis
Testing revealed significant performance degradation with jumbo frames (MTU 9000):
- **MTU 1500**: 730-773 Mbps (optimal)
- **MTU 9000**: 106-163 Mbps (3-4x slower)
- **Cause**: ESXi's small TCP window (65-71 KB) combined with larger packet sizes leads to increased out-of-order packets and retransmissions

**Recommendation**: Use **MTU 1500** for best performance with this tool.

### Bottleneck Identification
1. **ESXi per-connection rate limiting** - ESXi throttles individual TCP connections (~50-60 MB/s per connection)
2. **TCP window size** - ESXi advertises small receive windows (65-71 KB)
3. **Solution**: Multiple independent TCP connections bypass the per-connection limit

## Installation

### Pre-built Binaries

Download the appropriate binary for your Proxmox VE version:

- **PVE 9.0.10 / Debian 13 (Trixie)**: `esxi-folder-fuse-v1.1.0-pve9.0.10` (6.4M)
- **PVE 8.4.14 / Debian 12 (Bookworm)**: `esxi-folder-fuse-v1.1.0-pve8.4.14` (3.6M)

### Installation Steps

```bash
# Stop any running import processes
pkill -f esxi-folder-fuse

# Backup the original binary
cp /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse \
   /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse.backup-$(date +%Y%m%d)

# Install the new binary (replace with appropriate version)
cp esxi-folder-fuse-v1.1.0-pve8.4.14 /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse
chmod +x /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse
```

### Building from Source

#### Prerequisites
The following packages are required to build the optimized binary:

**For PVE 9.0.10 / Debian 13**:
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

**For PVE 8.4.14 / Debian 12**:
```bash
# Install rustup for newer Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# Install dependencies
apt-get update
apt-get install -y \
  build-essential \
  libssl-dev \
  pkg-config \
  libfuse3-dev \
  git
```

#### Build Instructions

1. **Clone or copy the source code**:
```bash
cd /root
git clone <repository-url> pve-esxi-import-tools
# or copy the modified source files
```

2. **Build the project**:
```bash
cd /root/pve-esxi-import-tools
cargo build --release
```

The build process will:
- Download dependencies from crates.io and Proxmox Git repositories
- Compile with full optimizations (LTO, stripped symbols)
- Take approximately 1-5 minutes depending on CPU

3. **Install the binary**:
```bash
# Stop running processes
pkill -f esxi-folder-fuse

# Install
cp target/release/esxi-folder-fuse /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse
```

### Verifying Installation
```bash
# Check binary size and type
ls -lh /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse
file /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse

# Expected: 3.6M-6.4M, stripped
```

### Monitoring Connection Pool

Check the logs during an import to verify the pool is working:

```bash
# View pool creation logs
journalctl --since '5 minutes ago' --no-pager | grep -i 'HTTP client pool'

# Output should show:
# Creating HTTP client pool with 8 clients for https://...
# HTTP client pool initialized with 8 clients

# Monitor active TCP connections
watch -n 1 'ss -tn | grep <esxi-ip>:443 | wc -l'
# Should show 6-8 concurrent connections during active transfer
```

## Usage

The optimized binary is a drop-in replacement. No configuration changes are required. The Proxmox import interface will automatically use the new binary.

### Command-line Options (Optional)
You can further tune performance with command-line options:

```bash
--cache-page-size=BYTES     # Default: 134217728 (128 MB)
--cache-page-count=COUNT    # Default: 16
```

## Testing Results

### Network Throughput (v1.1.0)
- **Observed**: 730-773 Mbps (91-97 MB/s) during import
- **Peak**: 852 Mbps (106 MB/s)
- **TCP connections**: 7-8 concurrent
- **CPU Usage**: Very low (~1-4% per core)
- **Memory Usage**: ~2.4 GB (up from 379 MB with old binary)

### Log Output Example
```
Oct 10 09:13:49 der-pve3 esxi-folder-fuse[1505126]: Creating HTTP client pool with 8 clients for https://10.10.254.67
Oct 10 09:13:49 der-pve3 esxi-folder-fuse[1505126]: HTTP client pool initialized with 8 clients
Oct 10 09:13:50 der-pve3 esxi-folder-fuse[1505126]: Client pool usage: 100 total requests distributed across 8 clients
```

## Recommendations

1. **Network MTU**: Use **MTU 1500** (standard) for best performance. Jumbo frames (MTU 9000) reduce performance by 3-4x with this tool.

2. **Network speed**: Ensure 10 Gigabit network connectivity between PVE and ESXi for maximum throughput.

3. **Storage**: While this tool significantly improves transfer speeds, local storage on ESXi will always be faster than iSCSI/NFS storage.

4. **Monitoring**: Monitor TCP connections during import to verify multiple connections are established:
   ```bash
   ss -tn | grep <esxi-ip>:443
   ```

## Troubleshooting

### Only 1 connection established
- Check logs to verify pool was created: `journalctl --since '5 minutes ago' | grep pool`
- Verify you're running the v1.1.0 binary: `ls -lh /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse`

### Slow transfer speeds with MTU 9000
- ESXi's TCP implementation doesn't handle jumbo frames well with small TCP windows
- Solution: Change interface MTU to 1500 on the PVE import interface

### Out of memory errors
- The 2 GB cache requires sufficient RAM
- Reduce cache size if needed with command-line options

## Version History

- **v1.1.0** (2025-10-10)
  - **Major**: Implemented HTTP client connection pool (8 clients)
  - Added logging for pool creation and usage tracking
  - Optimized build profile (stripped, LTO enabled)
  - Performance improvement: 64% faster than v1.0.1
  - Binary size: 3.6M (PVE 8.4) / 6.4M (PVE 9.0)

- **v1.0.1** (2025-10-08)
  - Increased concurrent connections from 4 to 16
  - Increased cache page size from 32 MB to 128 MB
  - Increased cache page count from 8 to 16
  - Total cache increased from 256 MB to 2 GB

## Files Modified

1. `src/esxi.rs` - Added `EsxiClientPool` for multiple TCP connections
2. `src/fs.rs` - Updated to use `EsxiClientPool` instead of `EsxiClient`
3. `src/main.rs` - Cache configuration changes, pool initialization
4. `Cargo.toml` - Version bump to 1.1.0, optimized release profile
5. `.cargo/config.toml` - Build configuration adjustments

## Technical Details

### HTTP Client Pool Architecture

The connection pool creates 8 independent `EsxiClient` instances, each with:
- Its own SSL connector (forcing separate TCP connections)
- Its own connection state and session cookies
- Round-robin request distribution via atomic counter

This bypasses HTTP/2's connection multiplexing limitation where all requests use a single TCP connection, which ESXi rate-limits.

### Why Multiple Connections Work

ESXi applies rate limiting per TCP connection (~50-60 MB/s per connection). By using 8 connections:
- Total throughput: 8 × 50 MB/s = ~400 MB/s theoretical maximum
- Observed: ~95 MB/s actual (limited by other factors)
- 64% improvement over single connection

## License

AGPL-3

## Authors

- Original: Wolfgang Bumiller <w.bumiller@proxmox.com>
- Original: Proxmox Development Team <support@proxmox.com>
- v1.0.1 Performance Optimizations: 2025-10-08
- v1.1.0 Connection Pool Implementation: 2025-10-10
