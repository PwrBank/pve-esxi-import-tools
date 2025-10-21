#### Machines
Both the ESXi and Proxmox have the exact same hardware

**CPU:** 2x AMD EPYC 7252 

**RAM:** 128GB

**Network:** Intel E810-XXV 25GbE

**Storage:** Samsung MZPLL1T6HEHP

VM on the ESXi server is hitting the limits of what the SSD can do

<img width="480" height="345" alt="Pasted image 20251021083245" src="https://github.com/user-attachments/assets/2f3619c7-f58b-4c7e-8a84-65eeff302bb0" />

#### Baselines
**Verify jumbo-packets are working**
```
root@pve:/tmp# ping -M do -s 8928 10.20.30.40
PING 10.20.30.40 (10.20.30.40) 8928(8956) bytes of data.
8936 bytes from 10.20.30.40: icmp_seq=1 ttl=64 time=0.137 ms
8936 bytes from 10.20.30.40: icmp_seq=2 ttl=64 time=0.116 ms
8936 bytes from 10.20.30.40: icmp_seq=3 ttl=64 time=0.103 ms
```

**iperf3 test:**
```
[ ID] Interval           Transfer     Bitrate
[  5]   0.00-10.00  sec  3.32 GBytes  2.85 Gbits/sec                  receiver
[  8]   0.00-10.00  sec  3.23 GBytes  2.77 Gbits/sec                  receiver
[ 10]   0.00-10.00  sec  3.27 GBytes  2.81 Gbits/sec                  receiver
[ 12]   0.00-10.00  sec  3.27 GBytes  2.81 Gbits/sec                  receiver
[ 14]   0.00-10.00  sec  3.35 GBytes  2.88 Gbits/sec                  receiver
[ 16]   0.00-10.00  sec  3.26 GBytes  2.80 Gbits/sec                  receiver
[ 18]   0.00-10.00  sec  3.34 GBytes  2.87 Gbits/sec                  receiver
[ 20]   0.00-10.00  sec  3.38 GBytes  2.90 Gbits/sec                  receiver
[SUM]   0.00-10.00  sec  26.4 GBytes  22.7 Gbits/sec                  receiver
```

**iperf3 reading disk on VMware and sending to Proxmox**
Single Thread
```bash
/usr/lib/vmware/vsan/bin/iperf3.copy -c 10.20.30.41 -p 9001 -F /vmfs/volumes/nvme-storage/windows-test/windows-test-flat.vmdk
```

```
 ID] Interval           Transfer     Bitrate
[  5]   0.00-30.01  sec  13.2 GBytes  3.79 Gbits/sec                  receiver
```

4 Threads
```bash
/usr/lib/vmware/vsan/bin/iperf3.copy -c 10.20.30.41 -p 9001 -F /vmfs/volumes/nvme-storage/windows-test/windows-test-flat.vmdk -t 30 -P 4
```

```
[SUM]   0.00-30.00  sec  26.5 GBytes  7.60 Gbits/sec                  receiver
```

8 Threads
```bash
/usr/lib/vmware/vsan/bin/iperf3.copy -c 10.20.30.41 -p 9001 -F /vmfs/volumes/nvme-storage/windows-test/windows-test-flat.vmdk -t 30 -P 8
```

```
[SUM]   0.00-30.01  sec  32.3 GBytes  9.24 Gbits/sec                  receiver
```

16 Threads
```bash
/usr/lib/vmware/vsan/bin/iperf3.copy -c 10.20.30.41 -p 9001 -F /vmfs/volumes/nvme-storage/windows-test/windows-test-flat.vmdk -t 30 -P 16
```

```
[SUM]   0.00-30.01  sec  37.1 GBytes  10.6 Gbits/sec                  receiver
```

Reading from RAM on ESXi and writing to disk on Proxmox
```
[SUM]   0.00-30.00  sec  58.9 GBytes  16.9 Gbits/sec                  receiver
```

**Built in ESXi import tool**
736 seconds at ~140MiB/s



From here on out the first command is on ESXi and the second is on Proxmox
#### Test 0
Baseline dd+netcat to null
```bash
cat /dev/zero | nc -v -v -n 10.20.30.41 9001
```

```bash
nc -l -p 9001 > /dev/null
```

Watching on the Proxmox incoming port there is about 370-405MiB/s

#### Test 1
Straight DD using blocksize of 4M over netcat
```bash
dd if=/vmfs/volumes/nvme-storage/windows-test/windows-test-flat.vmdk bs=4M | nc 10.20.30.41 9001
```

```bash
nc -l -p 9001 | dd of=/nvme-storage/test.vmdk bs=4M
```

```
96636764160 bytes (97 GB, 90 GiB) copied, 293.057 s, 330 MB/s
```

DD with blocksize 4M -> pigz with default compression type -> netcat
```bash
dd if=/vmfs/volumes/nvme-storage/windows-test/windows-test-flat.vmdk bs=4M | pigz -c | nc 10.20.30.41 9001
```

```bash
nc -l -p 9001 | pigz -d | dd of=/nvme-storage/test.vmdk bs=4M
```

```
96636764160 bytes (97 GB, 90 GiB) copied, 361.495 s, 267 MB/s
```

DD with blocksize 4M -> pigz with byte size as 4M -> netcat
```bash
dd if=/vmfs/volumes/nvme-storage/windows-test/windows-test-flat.vmdk bs=4M | pigz -c -b 4096 | nc 10.20.30.41 9001
```

```bash
nc -l -p 9001 | pigz -d | dd of=/nvme-storage/test.vmdk bs=4M
```

DD with blocksize 4M -> pigz with byte size as 4M and lowest compression level -> netcat
```bash
dd if=/vmfs/volumes/nvme-storage/windows-test/windows-test-flat.vmdk bs=4M | pigz -1 -c -b 4096 | nc 10.20.30.41 9001
```

```bash
nc -l -p 9001 | pigz -d | dd of=/nvme-storage/test.vmdk bs=4M
```

```
96636764160 bytes (97 GB, 90 GiB) copied, 187.611 s, 515 MB/s
```

DD with blocksize 4M -> pigz with byte size as 4M and lowest compression level, plus 32 cores dedicated to decompressing -> netcat
```bash
dd if=/vmfs/volumes/nvme-storage/windows-test/windows-test-flat.vmdk bs=4M | pigz -1 -c -b 4096 | nc 10.20.30.41 9001
```

```bash
nc -l -p 9001 | pigz -d -p 32 | dd of=/nvme-storage/test.vmdk bs=4M
```

```
96636764160 bytes (97 GB, 90 GiB) copied, 193.444 s, 500 MB/s
```

Note: Even when defining how many processes to use on decompression, it only seems to actually use 4. This is also noted in the pigz documentation. 

#### Test 2
Straight DD using blocksize of 8M over netcat
```bash
dd if=/vmfs/volumes/nvme-storage/windows-test/windows-test-flat.vmdk bs=8M | nc 10.20.30.41 9001
```

```bash
nc -l -p 9001 | dd of=/nvme-storage/test.vmdk bs=8M
```

```
96636764160 bytes (97 GB, 90 GiB) copied, 318.567 s, 303 MB/s
```

DD with blocksize 8M -> pigz with default compression type -> netcat
```
96636764160 bytes (97 GB, 90 GiB) copied, 295.676 s, 327 MB/s
```

DD with blocksize 8M -> pigz with byte size as 4M -> netcat
```bash
dd if=/vmfs/volumes/nvme-storage/windows-test/windows-test-flat.vmdk bs=8M | pigz -c -b 8192 | nc 10.20.30.41 9001
```

```
96636764160 bytes (97 GB, 90 GiB) copied, 265.847 s, 364 MB/s
```

DD with blocksize 8M -> pigz with byte size as 4M and lowest compression level -> netcat
```bash
dd if=/vmfs/volumes/nvme-storage/windows-test/windows-test-flat.vmdk bs=8M | pigz -1 -c -b 4096 | nc 10.20.30.41 9001
```

```bash
nc -l -p 9001 | pigz -d | dd of=/nvme-storage/test.vmdk bs=8M
```

```
96636764160 bytes (97 GB, 90 GiB) copied, 204.974 s, 471 MB/s
```
#### Test 3
Straight DD using blocksize of 16M over netcat
```bash
dd if=/vmfs/volumes/nvme-storage/windows-test/windows-test-flat.vmdk bs=16M | nc 10.20.30.41 9001
```

```bash
nc -l -p 9001 | dd of=/nvme-storage/test.vmdk bs=16M
```

```
96636764160 bytes (97 GB, 90 GiB) copied, 332.333 s, 291 MB/s
```

DD with blocksize 16M -> pigz with byte size as 4M and lowest compression level -> netcat
```bash
dd if=/vmfs/volumes/nvme-storage/windows-test/windows-test-flat.vmdk bs=16M | pigz -1 -c -b 4096 | nc 10.20.30.41 9001
```

```bash
nc -l -p 9001 | pigz -d -p 32 | dd of=/nvme-storage/test.vmdk bs=16M
```

```
96636764160 bytes (97 GB, 90 GiB) copied, 192.143 s, 503 MB/s
```
#### Test 4
Straight DD using blocksize of 32M over netcat
```bash
dd if=/vmfs/volumes/nvme-storage/windows-test/windows-test-flat.vmdk bs=32M | nc 10.20.30.41 9001
```

```bash
nc -l -p 9001 | dd of=/nvme-storage/test.vmdk bs=32M
```

```
96636764160 bytes (97 GB, 90 GiB) copied, 317.241 s, 305 MB/s
```

DD with blocksize 32M -> pigz with byte size as 4M and lowest compression level -> netcat
```bash
dd if=/vmfs/volumes/nvme-storage/windows-test/windows-test-flat.vmdk bs=32M | pigz -1 -c -b 4096 | nc 10.20.30.41 9001
```

```bash
nc -l -p 9001 | pigz -d -p 32 | dd of=/nvme-storage/test.vmdk bs=32M
```

```
96636764160 bytes (97 GB, 90 GiB) copied, 196.749 s, 491 MB/s
```

#### Extra Tests for the fun of it
Using tar to stream the vmdk to pigz with low compression and then piped to netcat
```bash
tar -cf - /vmfs/volumes/nvme-storage/windows-test/windows-test-flat.vmdk | pigz -1 -c -b 4096 | nc 10.20.30.41 9001
```

```bash
nc -l -p 9001 | pigz -d -p 32 | tar -xvf -
```

`210.0092 s, 438MiB/s`


Multiple netcat streams at once, this is the same file on two different ports, but in theory you could read certain chunks of a file up and then create multiple dd+netcat streams to parallelize the whole process. 

Note: Had to define the core count for the compress process, as by default it uses all of them. Split the 32 cores in half.

ESXi:
```bash
dd if=/vmfs/volumes/nvme-storage/windows-test/windows-test-flat.vmdk bs=4M | pigz -1 -p 16 -c -b 4096 | nc 10.20.30.41 9001
```

```bash
dd if=/vmfs/volumes/nvme-storage/windows-test/windows-test-flat.vmdk bs=4M | pigz -1 -p 16 -c -b 4096 | nc 10.20.30.41 9002
```

PVE:
```bash
nc -l -p 9001 | pigz -d -p 32 | dd of=/nvme-storage/test.vmdk bs=4M
```

```bash
nc -l -p 9002 | pigz -d -p 32 | dd of=/nvme-storage/test2.vmdk bs=4M
```

```
96636764160 bytes (97 GB, 90 GiB) copied, 207.897 s, 465 MB/s
```

```
96636764160 bytes (97 GB, 90 GiB) copied, 229.128 s, 422 MB/s
```

