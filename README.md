# PVE ESXi Import Tools - Performance Optimizations

## ⚠️ WARNING ⚠️
This should not be used in production, just for testing purposes

This has been ran succesfully on a low bandwidth migration, but nothing more.

## Overview

This document describes the performance optimizations made to the ESXi import tool to address the 50% slower import speeds compared to direct `qm import`.

## Changes Made

### 1. Increased Concurrent Connection Limit
**File**: `src/esxi.rs:414`

**Change**:
```rust
// Before:
requests: tokio::sync::Semaphore::new(4),

// After:
requests: tokio::sync::Semaphore::new(16),
```

**Impact**: Increased from 4 to 16 concurrent HTTP requests, allowing for 4x more parallel data fetching from ESXi, theoretically.

### 2. Increased Default Cache Page Size
**File**: `src/main.rs:26`

**Change**:
```rust
// Before:
static mut FILE_CACHE_PAGE_SIZE: u64 = 32 << 20;  // 32 MB

// After:
static mut FILE_CACHE_PAGE_SIZE: u64 = 128 << 20;  // 128 MB
```

**Impact**: Each HTTP request now fetches 128 MB chunks instead of 32 MB, reducing the number of round trips required and improving throughput.

### 3. Increased Default Cache Page Count
**File**: `src/main.rs:27`

**Change**:
```rust
// Before:
static mut FILE_CACHE_PAGE_COUNT: usize = 8;

// After:
static mut FILE_CACHE_PAGE_COUNT: usize = 16;
```

**Impact**: Doubled the cache capacity from 256 MB (8 × 32 MB) to 2 GB (16 × 128 MB), allowing for better readahead caching and reduced repeated requests.

## Performance Analysis

### Original Performance
- **Import time**: ~13 minutes
- **Throughput**: ~40 MB/s
- **Concurrent connections**: 4
- **Cache page size**: 32 MB
- **Total cache**: 256 MB

### Expected Performance with Optimizations
- **Concurrent connections**: 16 (4x increase)
- **Cache page size**: 128 MB (4x increase)
- **Total cache**: 2 GB (8x increase)
- **Expected throughput improvement**: 2-4x depending on bottleneck

### Actual Bottleneck Identified
Through testing and analysis, the primary bottleneck was found to be:

1. **ESXi HTTP API rate limiting** - ESXi throttles individual HTTP connections
2. **iSCSI storage latency** - When VMs are stored on network storage (TrueNAS iSCSI), the import involves a double network hop:
   - PVE → ESXi → TrueNAS (iSCSI) → ESXi → PVE
3. **Request processing time** - Each 128 MB chunk takes ~1.2 seconds to process through ESXi's HTTP API

### Comparison: Direct vs ESXi Import
- **Direct `qm import`**: 8 minutes (no network overhead, direct disk access)
- **ESXi import tool**: 13 minutes (double network hop through iSCSI)
- **Difference**: 62% longer due to iSCSI storage latency, not tool limitations

## Installation

### Building from Source

#### Prerequisites
The following packages are required to build the optimized binary:

```bash
# On the PVE node (Proxmox VE)
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

#### Build Instructions

Since the project depends on Proxmox-specific Rust crates that are not available on crates.io, you need to build it using Git dependencies:

1. **Update Cargo.toml to use Git dependencies** (already done in this version):
```toml
proxmox-async = { git = "https://git.proxmox.com/git/proxmox.git", package = "proxmox-async" }
proxmox-fuse = { git = "https://git.proxmox.com/git/proxmox-fuse.git" }
proxmox-http = { git = "https://git.proxmox.com/git/proxmox.git", package = "proxmox-http", features = [ "body", "client" ] }
```

2. **Build the project**:
```bash
cd /root/pve-esxi-import-tools
cargo build --release
```

The build process will:
- Download all dependencies from crates.io and Proxmox Git repositories
- Compile the Rust code with optimizations enabled
- Take approximately 10-15 minutes depending on CPU

3. **Install the optimized binary**:
```bash
# Stop any running import processes
pkill -f esxi-folder-fuse

# Backup the original binary
cp /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse \
   /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse.backup-$(date +%Y%m%d)

# Install the new binary
cp target/release/esxi-folder-fuse /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse

# Also copy the Python script (if updated)
cp listvms.py /usr/bin/esxi-listvms
chmod +x /usr/bin/esxi-listvms
```

### Verifying Installation
```bash
# Check version
/usr/libexec/pve-esxi-import-tools/esxi-folder-fuse --version

# Verify binary size (optimized version is ~6.4MB)
ls -lh /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse
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

### Network Throughput
- **Observed**: 288-319 Mbps (~36-40 MB/s) during import
- **CPU Usage**: Very low (~1% system, 98%+ idle)
- **I/O Wait**: 0% (no disk bottleneck on PVE side)
- **Memory Usage**: ~1.8 GB (up from 379 MB with old binary)

### ESXi Logs Analysis
ESXi logs show successful request processing:
- Each 128 MB chunk request takes ~1.2 seconds
- Theoretical throughput with optimization: 128 MB / 1.2s = ~107 MB/s
- Actual throughput limited by iSCSI storage latency: ~40 MB/s

## Recommendations

1. **For fastest imports**: Store VMs on ESXi's local NVMe/SSD storage rather than iSCSI
2. **Network optimization**: Ensure 10 Gigabit network connectivity between PVE and ESXi
3. **ESXi tuning**: Consider ESXi HTTP service tuning if available
4. **Alternative**: For VMs on slow storage, consider using direct disk access methods when possible

## Version History

- **v1.0.1** (2025-10-08)
  - Increased concurrent connections from 4 to 16
  - Increased cache page size from 32 MB to 128 MB
  - Increased cache page count from 8 to 16
  - Total cache increased from 256 MB to 2 GB

## Files Modified

1. `src/esxi.rs` - Connection limit changes
2. `src/main.rs` - Cache configuration changes
3. `.cargo/config.toml` - Build configuration adjustments

## Backup

The original binary has been backed up to:
```
/usr/libexec/pve-esxi-import-tools/esxi-folder-fuse.backup-20251008
```

To revert to the original:
```bash
mv /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse.backup-20251008 \
   /usr/libexec/pve-esxi-import-tools/esxi-folder-fuse
```

## Contributing

When making further optimizations, consider:
- ESXi's rate limiting behavior
- Network latency and bandwidth constraints
- Storage backend performance characteristics
- HTTP/2 multiplexing efficiency

## License

AGPL-3

## Authors

- Original: Wolfgang Bumiller <w.bumiller@proxmox.com>
- Original: Proxmox Development Team <support@proxmox.com>
- Performance Optimizations: 2025-10-08
