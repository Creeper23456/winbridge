<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# shmprobe results: ivshmem Windows driver and cache modes

Measured 2026-11-15/16 with `experiments/shmprobe` (this directory) on the disposable build VM (never the LTSC VM). Everything below is a measurement from that run unless marked **[INFERENCE]**. One run per cell unless stated; the host was shared with other VMs and test jobs running at the same time, so absolute numbers (and especially maximum latencies) are noisy.

## Verdict

* **The driver is usable.** The package from the Looking Glass installer (`ivshmem.sys/.inf/.cat`, driver 100.10.100.10000 of 2022-08-03, Red Hat, Inc.) is WHQL-signed, installs with `pnputil` without Code 52 on a guest configured like the LTSC VM (Secure Boot not active, VBS off, test-signing off) **and** with Secure Boot ON, and maps 256 MiB per device.
* **Use `cacheMode = 1` (cached) for both devices and both directions.** It is coherent with the host in both directions (verified), and by a wide margin the fastest: ring end to end 6,628 MiB/s guest to host and 7,722 MiB/s host to guest (1 M mixed records each way, 3.76 GB per direction), spin ping-pong round trip p50 0.20 us. Write-combined has the same write speed but reads cost 81 MiB/s (scalar) to about 7,200 MiB/s (`MOVNTDQA`); the ring's `memcpy` read path gets 208 MiB/s from it. Non-cached is 10 to 330 MiB/s for everything.
* **Keep `publish_fence()` (`sfence`) in the ring producer.** With a cached mapping only non-temporal stores need it (147,815 index-before-data observations without it, none with it), scalar and `rep movsb` stores were never reordered; with write-combined every store kind can be reordered without it.
* **BLOCKER for the security model: the device is NOT restricted to SYSTEM/Administrators.** The driver's default DACL on the device object is `O:BAG:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1201bf;;;WD)(A;;0x1200a9;;;RC)`: **Everyone** has read/write. A standard user (member of `Users` only, no elevation) opened both devices and got `STATUS_SUCCESS` from `REQUEST_MMAP`, i.e. a user-mode pointer to the whole 256 MiB BAR. Because the driver allows only one mapping per device, an unprivileged process can also hold the mapping and lock the agent out, or **[INFERENCE]** write forged host messages into the ring. `docs/IVSHMEM-TRANSPORT.md` sections 7 and 8 ("default security SYSTEM and Administrators") are **wrong** and must be corrected. A working mitigation was measured (section 6): the elevated agent restricts the device DACL at runtime; it does not survive a reboot, and the registry `Security` value did not work.
* The remaining single-mapping behaviour is as the source says (section 5), including recovery after the owner dies without `RELEASE_MMAP`.

## 1. Environment

| | |
|---|---|
| Host | AMD Ryzen 9 5900X (12 cores / 24 threads), Linux 6.12.117 (Debian 13), QEMU 10.0.13 (Debian 1:10.0.13+ds-0+deb13u1), KVM, `/dev/shm` tmpfs |
| Build VM (copy) | q35, `-cpu host,hv_relaxed,hv_vapic,hv_spinlocks=0x1fff,hv_time`, 12 vCPUs, 12 GiB, OVMF_CODE_4M (identical to Debian `OVMF_CODE_4M.fd`, no Secure Boot code), virtio disks |
| Guest | Windows 11 IoT Enterprise LTSC ("Windows 11 Enterprise LTSC 2024" in the boot entry), version 10.0.26100, 64-bit; CPU seen by the guest: AMD Ryzen 9 5900X, features used: sse2, sse4.1, avx2, ermsb |
| ivshmem devices | two `ivshmem-plain` (PCI 1AF4:1110, subsystem 1AF4:1100, "PCI standard RAM Controller" before the driver) at PCI bus 0 device 10 and device 11, 256 MiB each, backing files `/dev/shm/wbprobe-a` and `/dev/shm/wbprobe-b` (mode 0600), `-object memory-backend-file,id=...,mem-path=...,size=256M,share=on -device ivshmem-plain,memdev=...` |
| Rust | rustc 1.100.0 in the VM and on the host; static CRT (`+crt-static`) is the repository default for the MSVC target |

QEMU command line: `run.sh` of the copy ends with `"$@"`, so the two `-object/-device` pairs are passed straight through; `vm start` hard-codes its own extra arguments and was therefore bypassed by reverting to the `clean` snapshot, starting `run.sh <extra args>` directly in the background, and using `./vm stop` for the rollback. **No existing file of the build VM copy was edited.** For the Secure Boot attempt (step 6) a new file `run-sb.sh` was derived from `run.sh` with `sed` (secure `OVMF_CODE_4M.secboot.fd`, `smm=on`, `-global driver=cfi.pflash01,property=secure,value=on`, a throw-away copy of Debian's `OVMF_VARS_4M.ms.fd`); it was deleted afterwards.

### Does the build VM match the LTSC guest?

Yes for everything that matters to the driver (output of `env.ps1` in the build VM):

```
Confirm-SecureBootUEFI failed: Variable is currently undefined: 0xC0000100 | GetFWVarFailed,...
SecureBoot\State: Cannot find path 'HKLM:\SYSTEM\CurrentControlSet\Control\SecureBoot\State' because it does not exist.
VirtualizationBasedSecurityStatus : 0     SecurityServicesRunning : {0}     SecurityServicesConfigured : {0}
bcdedit {current}: no testsigning / nointegritychecks entry (test-signing off); description "Windows 11 Enterprise LTSC 2024"
CodeIntegrity event 3085: "Code Integrity will disable WHQL driver enforcement for this boot session."
```

That is the same `0xC0000100` answer as the LTSC VM. The firmware files of the LTSC VM were not compared (that VM was not touched).

## 2. Signature check and install (Secure Boot not active, like the LTSC VM)

Hashes of the copied package (unmodified):

```
416C73F68F1BDAF10BDD86F2C29C62370EDBDD9D3AEB109CB618ECCFD050A45D  ivshmem.sys
40CC6242B8BF05600E5361B81EC4F5150B23BFDCBAB538B68E406F8AC57E979D  ivshmem.cat
E41554A07EEE128AEE07CEBDD2423DCFCDE45E3868E2AE90F7BA8CAF52AF8F1C  ivshmem.inf
```

`Get-AuthenticodeSignature` (identical for `ivshmem.cat` and `ivshmem.sys`):

```
Status        : Valid
StatusMessage : Signature verified.
SignatureType : Authenticode
Subject    : CN=Microsoft Windows Hardware Compatibility Publisher, O=Microsoft Corporation, L=Redmond, S=Washington, C=US
Issuer     : CN=Microsoft Windows Third Party Component CA 2014, O=Microsoft Corporation, L=Redmond, S=Washington, C=US
Thumbprint : 451B7F8A4C0E669189E9382A09E423C2B875AD42
NotBefore  : 6/7/2022 11:08:06 AM        NotAfter : 6/1/2023 11:08:06 AM
TimeStamper: CN=Microsoft Time-Stamp Service, OU=Thales TSS ESN:0A56-E329-4D4D, ...
```

The signing certificate expired on 2023-06-01; the signature is valid because it is timestamped. `certutil -dump ivshmem.cat` shows the chain Microsoft Windows Hardware Compatibility Publisher, Microsoft Windows Third Party Component CA 2014, Microsoft Root Certificate Authority 2010 and "Signature matches Public Key". `certutil -verify ivshmem.cat` fails with `0x8009310b CRYPT_E_ASN1_BADTAG` (it expects a certificate, not a catalog; not meaningful); `certutil -verifyctl` is for CTLs and was not applicable; `signtool` is not installed in the build VM. After installation `Get-AuthenticodeSignature C:\Windows\System32\drivers\ivshmem.sys` reports `Valid`, `SignatureType : Catalog`.

Install:

```
> pnputil /add-driver C:\work\drv\ivshmem.inf /install
Driver package added successfully.
Published Name:         oem4.inf
Driver package installed on device: PCI\VEN_1AF4&DEV_1110&SUBSYS_11001AF4&REV_01\3&11583659&0&50
Driver package installed on device: PCI\VEN_1AF4&DEV_1110&SUBSYS_11001AF4&REV_01\3&11583659&0&58
Total driver packages:  1     Added driver packages:  1       pnputil exit code = 0
```

`Get-PnpDevice` before the install: both devices `PCI standard RAM Controller`, Status OK, class System (the inbox `machine.inf` PCI RAM controller). After: both `IVSHMEM Device`, **Status OK, ConfigManagerErrorCode CM_PROB_NONE, `DEVPKEY_Device_ProblemCode` 0**, provider "Red Hat, Inc.", INF `oem4.inf`, service `IVSHMEM`, driver version 100.10.100.10000, signer "Microsoft Windows Hardware Compatibility Publisher". `setupapi.dev.log`: `Signer Score - WHQL (0D000005)`, `Configure Device - exit(0x00000000)`, no errors. The install was repeated in every later boot from the clean snapshot (seven boots in total) with the same result.

## 3. Secure Boot ON (step 6)

Second firmware setup: secure OVMF code + `OVMF_VARS_4M.ms.fd` (Microsoft keys enrolled) in the build VM copy; the guest booted normally from the same disk image (rolled back afterwards, user data untouched):

```
Confirm-SecureBootUEFI = True
Registry UEFISecureBootEnabled = 1
VirtualizationBasedSecurityStatus : 0   (VBS still off)
CodeIntegrity event 3084: "Code Integrity will enable WHQL driver enforcement for this boot session.  Settings 0x0. Exemption 1."
pnputil exit code = 0; both devices Status OK, ConfigManagerErrorCode CM_PROB_NONE; setupapi: Signer Score WHQL
shmprobe guest info: both devices size 256 MiB, REQUEST_MMAP(cached) OK; pattern test cached: host_bad_words=0 guest_bad_words=0
```

So the package also installs and runs with Secure Boot ON (and WHQL enforcement enabled); the Looking Glass advice to disable Secure Boot does not apply to this 2022 signed build. HVCI/memory integrity was **not tested** (VBS is off in both setups).

## 4. Bandwidth by cache mode

Tool: `shmprobe guest bench` (kernels in `src/kernels.rs`): sequential, one thread, 64-byte aligned, window of 128 MiB of the 256 MiB BAR walked in blocks of the stated size, median of 5 repetitions (3 when a repetition takes over one second), MiB/s (1 MiB = 2^20 B). `w_*` writes local memory into the BAR, `r_*` reads the BAR (the scalar/SSE2/AVX2/`MOVNTDQA` variants only fold the data into registers, `r_movsb`/`r_memcpy` copy to local memory). `w_memcpy`/`r_memcpy` are Rust's `copy_nonoverlapping`, which is what `winbridge-shm` uses; it links to the static MSVC CRT `memcpy`. Warm-up (first touch of the 128 MiB window, pages committed in `/dev/shm`): 145 MiB/s non-cached, 12,960 MiB/s cached, 21,900 MiB/s write-combined. Device a (full size set), device b (two runs agree within about 10 %, quick size set) and the host mapping of the same file for reference follow; raw rows have min/best as well (`ROW,dev,mode,op,block,median,best,min`).

### Device a (PCI device 10), all block sizes

**noncached** (MiB/s, median of reps)

| op | 4 KiB | 16 KiB | 64 KiB | 256 KiB | 1 MiB | 4 MiB | 16 MiB |
|---|---:|---:|---:|---:|---:|---:|---:|
| w_scalar64 | 78 | 79 | 78 | 78 | 78 | 79 | 78 |
| w_movsb | 10 | 10 | 10 | 10 | 10 | 10 | 10 |
| w_memcpy | 316 | 10 | 10 | 10 | 10 | 314 | 313 |
| w_sse2 | 158 | 157 | 159 | 159 | 159 | 159 | 156 |
| w_avx2 | 311 | 314 | 312 | 314 | 316 | 319 | 315 |
| w_nt_sse2 | 158 | 158 | 155 | 158 | 158 | 159 | 158 |
| w_nt_avx2 | 318 | 312 | 313 | 316 | 312 | 312 | 314 |
| r_scalar64 | 83 | 82 | 82 | 82 | 75 | 81 | 81 |
| r_sse2 | 144 | 151 | 152 | 157 | 160 | 163 | 163 |
| r_avx2 | 316 | 318 | 320 | 318 | 315 | 321 | 324 |
| r_movntdqa | 162 | 163 | 163 | 165 | 165 | 164 | 163 |
| r_movntdqa_avx2 | 322 | 328 | 329 | 328 | 324 | 329 | 331 |
| r_movsb | 10 | 10 | 10 | 10 | 10 | 10 | 10 |
| r_memcpy | 335 | 10 | 10 | 10 | 10 | 260 | 259 |

**cached** (MiB/s, median of reps)

| op | 4 KiB | 16 KiB | 64 KiB | 256 KiB | 1 MiB | 4 MiB | 16 MiB |
|---|---:|---:|---:|---:|---:|---:|---:|
| w_scalar64 | 16276 | 15134 | 14575 | 14818 | 14523 | 12927 | 13154 |
| w_movsb | 16114 | 15645 | 14887 | 14821 | 23630 | 20800 | 22597 |
| w_memcpy | 15473 | 15569 | 14749 | 14916 | 20855 | 23825 | 23553 |
| w_sse2 | 17055 | 16734 | 16398 | 16435 | 16387 | 15743 | 14582 |
| w_avx2 | 16912 | 15830 | 15157 | 15349 | 15094 | 14503 | 13302 |
| w_nt_sse2 | 15444 | 20971 | 22508 | 23585 | 23774 | 23865 | 23322 |
| w_nt_avx2 | 15616 | 21049 | 22501 | 23461 | 23774 | 23824 | 23846 |
| r_scalar64 | 25821 | 25824 | 25813 | 25772 | 26272 | 25796 | 25076 |
| r_sse2 | 28290 | 28591 | 28112 | 29052 | 29646 | 29462 | 28345 |
| r_avx2 | 29859 | 30481 | 30814 | 30528 | 31074 | 31052 | 30935 |
| r_movntdqa | 27566 | 27352 | 27071 | 27610 | 27548 | 29193 | 29624 |
| r_movntdqa_avx2 | 32610 | 31850 | 31080 | 32243 | 31796 | 31677 | 31290 |
| r_movsb | 30844 | 31001 | 30561 | 32216 | 20456 | 20043 | 19892 |
| r_memcpy | 32975 | 30638 | 30626 | 32001 | 20172 | 19904 | 20341 |

**writecombined** (MiB/s, median of reps)

| op | 4 KiB | 16 KiB | 64 KiB | 256 KiB | 1 MiB | 4 MiB | 16 MiB |
|---|---:|---:|---:|---:|---:|---:|---:|
| w_scalar64 | 23866 | 23934 | 23888 | 23935 | 23921 | 23951 | 23907 |
| w_movsb | 23854 | 23860 | 24006 | 23844 | 23748 | 23831 | 23814 |
| w_memcpy | 21352 | 23853 | 23572 | 23652 | 23188 | 23257 | 23407 |
| w_sse2 | 23546 | 23479 | 23880 | 23434 | 23912 | 23915 | 23915 |
| w_avx2 | 23754 | 23609 | 23802 | 23781 | 23939 | 23719 | 23248 |
| w_nt_sse2 | 15158 | 20694 | 22397 | 23292 | 23765 | 23753 | 23781 |
| w_nt_avx2 | 15139 | 20299 | 22391 | 23502 | 23788 | 23817 | 23806 |
| r_scalar64 | 81 | 81 | 81 | 81 | 81 | 81 | 81 |
| r_sse2 | 163 | 164 | 164 | 164 | 162 | 163 | 163 |
| r_avx2 | 328 | 328 | 328 | 327 | 328 | 326 | 325 |
| r_movntdqa | 7158 | 7249 | 7234 | 7200 | 7199 | 7173 | 7068 |
| r_movntdqa_avx2 | 15378 | 15389 | 14878 | 15615 | 15335 | 15206 | 14592 |
| r_movsb | 320 | 320 | 317 | 322 | 266 | 200 | 177 |
| r_memcpy | 327 | 320 | 321 | 316 | 294 | 256 | 256 |


### Device b (PCI device 11), quick block set

**noncached** (MiB/s, median of reps)

| op | 4 KiB | 64 KiB | 1 MiB | 16 MiB |
|---|---:|---:|---:|---:|
| w_scalar64 | 78 | 78 | 78 | 77 |
| w_movsb | 10 | 10 | 10 | 10 |
| w_memcpy | 313 | 10 | 10 | 314 |
| w_sse2 | 157 | 157 | 148 | 155 |
| w_avx2 | 312 | 314 | 314 | 318 |
| w_nt_sse2 | 157 | 156 | 159 | 156 |
| w_nt_avx2 | 315 | 313 | 315 | 319 |
| r_scalar64 | 84 | 84 | 84 | 84 |
| r_sse2 | 167 | 167 | 167 | 167 |
| r_avx2 | 334 | 335 | 335 | 335 |
| r_movntdqa | 168 | 167 | 168 | 166 |
| r_movntdqa_avx2 | 330 | 329 | 329 | 333 |
| r_movsb | 10 | 10 | 10 | 10 |
| r_memcpy | 335 | 10 | 10 | 260 |

**cached** (MiB/s, median of reps)

| op | 4 KiB | 64 KiB | 1 MiB | 16 MiB |
|---|---:|---:|---:|---:|
| w_scalar64 | 16436 | 15098 | 15186 | 13909 |
| w_movsb | 16497 | 15242 | 23823 | 23245 |
| w_memcpy | 15227 | 15618 | 23965 | 23935 |
| w_sse2 | 18579 | 17668 | 17448 | 16268 |
| w_avx2 | 18308 | 15751 | 15551 | 13917 |
| w_nt_sse2 | 15433 | 22513 | 23426 | 23800 |
| w_nt_avx2 | 15564 | 22572 | 23805 | 23807 |
| r_scalar64 | 24724 | 25450 | 25480 | 25627 |
| r_sse2 | 28873 | 29232 | 29201 | 29166 |
| r_avx2 | 31349 | 32103 | 32461 | 32495 |
| r_movntdqa | 28738 | 29332 | 29532 | 29136 |
| r_movntdqa_avx2 | 32574 | 32568 | 31603 | 31256 |
| r_movsb | 30391 | 30297 | 19623 | 18339 |
| r_memcpy | 31462 | 29291 | 21676 | 20040 |

**writecombined** (MiB/s, median of reps)

| op | 4 KiB | 64 KiB | 1 MiB | 16 MiB |
|---|---:|---:|---:|---:|
| w_scalar64 | 23690 | 23805 | 23502 | 23311 |
| w_movsb | 23958 | 23938 | 23389 | 23150 |
| w_memcpy | 21488 | 23838 | 23791 | 23852 |
| w_sse2 | 23948 | 24014 | 23912 | 23853 |
| w_avx2 | 23981 | 23934 | 23890 | 23821 |
| w_nt_sse2 | 15512 | 22566 | 23797 | 23812 |
| w_nt_avx2 | 15567 | 22579 | 23754 | 23886 |
| r_scalar64 | 83 | 82 | 82 | 82 |
| r_sse2 | 162 | 164 | 164 | 164 |
| r_avx2 | 327 | 324 | 324 | 324 |
| r_movntdqa | 7157 | 7220 | 7242 | 7225 |
| r_movntdqa_avx2 | 14621 | 14756 | 14750 | 14658 |
| r_movsb | 319 | 319 | 271 | 181 |
| r_memcpy | 324 | 317 | 302 | 253 |


### Host reference (Linux mmap of `/dev/shm/wbprobe-a`, same kernels)

**host** (MiB/s, median of reps)

| op | 4 KiB | 16 KiB | 64 KiB | 256 KiB | 1 MiB | 4 MiB | 16 MiB |
|---|---:|---:|---:|---:|---:|---:|---:|
| w_scalar64 | 16496 | 15175 | 14972 | 14736 | 14415 | 14147 | 12725 |
| w_movsb | 16293 | 15809 | 15112 | 14909 | 17896 | 23688 | 23569 |
| w_memcpy | 14148 | 15388 | 14874 | 15404 | 14900 | 14090 | 13233 |
| w_sse2 | 17681 | 17279 | 16549 | 16674 | 16346 | 16306 | 14000 |
| w_avx2 | 17486 | 16745 | 15229 | 15094 | 14873 | 14011 | 13362 |
| w_nt_sse2 | 15476 | 20950 | 22614 | 23726 | 23880 | 23828 | 23575 |
| w_nt_avx2 | 15325 | 20634 | 22728 | 23681 | 23874 | 23831 | 23385 |
| r_scalar64 | 24363 | 25408 | 25513 | 25429 | 25406 | 25500 | 25424 |
| r_sse2 | 29685 | 28478 | 29439 | 29695 | 29788 | 29035 | 29748 |
| r_avx2 | 32593 | 32041 | 32128 | 30276 | 31285 | 30573 | 32546 |
| r_movntdqa | 28183 | 28329 | 28250 | 28461 | 28267 | 28337 | 28005 |
| r_movntdqa_avx2 | 29992 | 31173 | 30900 | 31147 | 30824 | 31360 | 31283 |
| r_movsb | 30931 | 31316 | 30036 | 29659 | 22271 | 18460 | 19736 |
| r_memcpy | 22694 | 26414 | 27844 | 28638 | 26672 | 25630 | 19419 |


What it shows:

* **Cached** behaves exactly like ordinary RAM: 13,000 to 24,000 MiB/s writes and 20,000 to 33,000 MiB/s reads for every kernel; the best kernels are `rep movsb`/NT stores for writes at 1 MiB and above and `MOVNTDQA`/AVX2 loads for reads. The `memcpy` the ring uses is within a few percent of the best kernel in both directions.
* **Write-combined**: writes are as fast as cached (about 23,900 MiB/s) but reads are uncached: 81 MiB/s for 64-bit scalar loads, 163 for SSE2, 328 for AVX2, 177 to 320 for `rep movsb`/`memcpy`. Only **`MOVNTDQA` streaming loads** are fast (about 7,200 MiB/s 128-bit, 15,000 MiB/s 256-bit): the tens-of-MB/s reading in section 4 of the design doc is confirmed for ordinary loads and refuted for streaming loads. The ring does not use streaming loads, so its consumer reads at about 208 MiB/s (section 8).
* **Non-cached**: 10 MiB/s for `rep movsb` (byte-wise), 78 to 84 MiB/s for 64-bit scalar, 150 to 165 for 128-bit, about 320 for 256-bit; Rust's `memcpy` drops from about 315 to 10 MiB/s in the 16 KiB to 1 MiB range (it switches to `rep movsb` **[INFERENCE]**, since the rate equals `w_movsb`/`r_movsb`). Streaming loads and non-temporal stores give nothing over plain ones.
* Cached 16 MiB blocks are slightly slower than 4 KiB to 1 MiB for scalar/SSE2/AVX2 stores (L2 and L3 spill), with no consequence for the ring.

## 5. Single mapping and ownership (`shmprobe guest single`, SYSTEM)

Exact status codes via `NtDeviceIoControlFile` (the Win32 value of 0xC0000038 is `ERROR_DEVICE_ALREADY_ATTACHED` 548 **[INFERENCE]**, not recorded). Identical in two separate boots.

```
1. REQUEST_MMAP on handle H1: STATUS_SUCCESS   (peer=0 size=256 MiB vectors=0, user pointer e.g. 0x218eb820000)
2. second REQUEST_MMAP on the same handle H1:                          0xC0000038 STATUS_DEVICE_ALREADY_ATTACHED
2b. second REQUEST_MMAP, other cache mode, same handle:                0xC0000038 STATUS_DEVICE_ALREADY_ATTACHED
3. REQUEST_MMAP on a second handle H2 of the same process:             0xC0000038 STATUS_DEVICE_ALREADY_ATTACHED
3b. RELEASE_MMAP from the non-owner handle H2:                         0xC0000008 STATUS_INVALID_HANDLE
4. REQUEST_MMAP from a second process while H1 holds the mapping:      0xC0000038 STATUS_DEVICE_ALREADY_ATTACHED (RELEASE by it: 0xC0000008)
5. RELEASE_MMAP by the owner: STATUS_SUCCESS; again (not mapped):      0xC0000010 STATUS_INVALID_DEVICE_REQUEST
5c. REQUEST_MMAP on H2 after H1 released: STATUS_SUCCESS (H2 is the new owner and can release)
6. child process maps and exits WITHOUT RELEASE_MMAP, then the parent maps: STATUS_SUCCESS; data written through the first mapping is still there
7. child holds the mapping, the parent gets 0xC0000038; the child is killed with TerminateProcess, the parent then maps: STATUS_SUCCESS
8. handle closed without RELEASE_MMAP, then REQUEST_MMAP on another handle: STATUS_SUCCESS
9. 300 map/release cycles (100 per mode): 0 failures
10. cache mode 3 and 255: 0xC000000D STATUS_INVALID_PARAMETER;  11. 16-byte output buffer: 0xC0000206 STATUS_INVALID_BUFFER_SIZE
```

So the mapping is released automatically when the owning handle is closed (process exit or kill included); the shared memory content survives unmap/remap. `IVSHMEM_MMAP.vectors` is 0 for `ivshmem-plain`, as expected; doorbells and events were not tested (they need `ivshmem-doorbell` and `ivshmem-server`).

## 6. Who can open the device (`shmprobe guest open-test`)

A local standard user `wbstd` (member of `Users` only) and a local administrator `wbadm` were created in the VM and the probe was run as each through a scheduled task. A standard user cannot be scheduled without the "Log on as a batch job" right, which `Users` lack by default; it was granted to `Users` in the throw-away VM (`secedit`), the first attempt without it never ran (`LastTaskResult 267011`).

**Default security: a standard user can open and map both devices.**

```
device object security: O:BAG:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1201bf;;;WD)(A;;0x1200a9;;;RC)
=== as wbstd (Users, not elevated): open(GENERIC_READ|GENERIC_WRITE) OK; REQUEST_MMAP -> STATUS_SUCCESS; open(access 0) OK, REQUEST_MMAP -> STATUS_SUCCESS (FILE_ANY_ACCESS)
=== as wbadm (elevated), as SYSTEM: the same
```

Mitigations tried (guest rolled back after each boot):

| Method | Result |
|---|---|
| **Runtime**: elevated process opens the interface with `WRITE_DAC` and calls `SetKernelObjectSecurity(DACL, "D:P(A;;GA;;;SY)(A;;GA;;;BA)")` (`shmprobe guest harden`) | **Works**: DACL becomes `O:BAG:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)`; `wbstd` gets `Win32 error 5` (ACCESS_DENIED) on open for both access masks; SYSTEM and elevated `wbadm` still open and map. The default DACL does not grant `WRITE_DAC` to Everyone, so only SYSTEM/Administrators can do this. **Not persistent**: it applies to the live device object, so it has to be redone after every boot (and after any device restart), and there is a window between boot and the agent applying it. |
| Per-device `HKLM\SYSTEM\CurrentControlSet\Enum\PCI\...\Device Parameters\Security` as `REG_BINARY` (device a) and as `REG_SZ` SDDL (device b), then `pnputil /restart-device`, then a full guest reboot | **No effect**: after the reboot the DACL was unchanged (Everyone still has read/write) and `wbstd` could still map both devices. |
| Editing the INF (`HKR,,Security`) | Not tested: it would invalidate the catalog signature. |
| Class-key `Security` value | Not tested (would affect every System-class device). |

Consequence: the agent (or a small SYSTEM start-up task) must restrict the DACL before any untrusted code can run and check the resulting SDDL, treating failure as "shared memory unavailable". A malicious local process that wins the race after boot is not stopped by this; **[INFERENCE]** a SYSTEM boot-time task closes most of the window. The driver's single-mapping rule means whoever maps first owns the device, so the agent should hold the mapping for its whole lifetime.

## 7. Visibility ordering (`shmprobe guest order` / `host order`)

The guest repeatedly writes a 1 KiB block (every 64-bit word carries `round << 10 | word`), optionally executes a fence, then stores the round number in an index word on a different page. The host polls the index; whenever it changed it reads the block, and **a word with a round older than the index is a violation** (index seen before the data). Runs of 1.5 s per cell (the first session used 2 s and only `none`/`sfence`, with the same pattern of results); the count is violations / index changes the host observed.

| Cache mode | Store kind | no fence | `sfence` | `mfence` |
|---|---|---:|---:|---:|
| cached | scalar 64-bit | 0 / 9.0 M | 0 / 10.1 M | 0 / 10.2 M |
| cached | `rep movsb` | 0 / 8.3 M | 0 / 9.2 M | 0 / 9.3 M |
| cached | non-temporal SSE2 | **147,815** / 2.0 M | 0 / 2.5 M | 0 / 2.9 M |
| write-combined | scalar 64-bit | **1** / 30 k (6 in the 2 s run) | 0 / 2.4 M | 0 / 2.6 M |
| write-combined | `rep movsb` | **3** / 17.6 k (2 in the 2 s run) | 0 / 2.5 M | 0 / 2.7 M |
| write-combined | non-temporal SSE2 | 0 / 7.8 k (1 in the 2 s run) | 0 / 2.8 M | 0 / 3.1 M |
| non-cached | scalar 64-bit | 0 / 116 k | 0 / 116 k | 0 / 115 k |
| non-cached | `rep movsb` | **3** / 14.8 k | **6** / 14.8 k | **4** / 14.8 k |
| non-cached | non-temporal SSE2 | 0 / 225 k | 0 / 224 k | 0 / 226 k |

Repeat of the non-cached rows for 4 s each: `rep movsb` 13 / 14 / 15 violations (none, `sfence`, `mfence`) of about 39.5 k index changes, always with a lag of exactly 63 rounds; scalar and NT stores 0. No torn words (a word with the wrong position tag) and no wrong final state were ever seen (`final_check_bad=0`).

Conclusions:

* **Cached: only non-temporal stores need `sfence`; scalar and `rep movsb` stores (the 1 KiB case) were never reordered on this CPU** (26 to 29 M index changes per store kind, zero violations). The ring already executes `publish_fence()` (an `sfence`) before storing the head; keep it, because a large `memcpy` may use non-temporal stores or fast-string stores (the `memcpy` curve in section 4 jumps to the NT/`rep movsb` speed at 1 MiB) **[INFERENCE]**.
* **Write-combined: any store kind can be seen reordered without a fence; with `sfence` or `mfence` there were none.** Without a fence the host also sees the index only rarely (17 k to 30 k changes out of 15 M rounds): the WC buffer holds lines back.
* **Non-cached with `rep movsb`: an unexplained anomaly.** Rarely (about 0.03 % of index changes) the host saw block words that were up to 63 rounds (about 6 ms) older than the index, **even after `sfence` or `mfence`**. Scalar and NT stores never showed it. Not root-caused (suspects: fast-string stores to UC memory, or the memory-type mix of a UC guest mapping and the cacheable host mapping **[INFERENCE]**). It is another reason not to use non-cached; it does not affect the recommendation.
* This was measured on one CPU (Zen 3 host, Zen 3 guest view); other CPUs may order differently, but the fence rules above are the architectural minimum anyway.

Data correctness (`shmprobe guest pattern` / `host pattern`, 64 MiB span per mode, guest writes a position-dependent pattern, host verifies it, host overwrites the same span, guest reads it back): `guest->host host_bad_words=0` and `host->guest guest_bad_words=0` in **all three modes** (once; the ring tests of section 8 verify every record in all three modes as well), and once more in cached mode with Secure Boot ON. The host overwrote lines the guest had just cached and the guest saw the new data: the cached mapping is hardware-coherent here, which settles the open point in section 4 of the design doc (for QEMU RAM-backed ivshmem on KVM; **[INFERENCE]** not valid for other hypervisors).

## 8. End to end with the `winbridge-shm` ring (`shmprobe guest ring` / `host ring`)

`Mapping` is implemented for the driver's user pointer in the guest (a `Map` that calls `RELEASE_MMAP` on drop) and for the host `mmap`. The host calls `Region::create` (guest role `GuestAgent`), the guest `Region::attach`s the same memory. Four lanes per region: bulk guest to host (64 MiB data area), bulk host to guest (64 MiB), ping (guest to host, 64 KiB, control) and pong. 1,000,000 records per direction, lengths 16 B to 64 KiB (80 % up to 1 KiB, 15 % 1 to 16 KiB, 5 % 16 to 64 KiB; 3.76 GB in total per direction, mean 3.76 KB), every record carries its sequence number, a checksum and a tag, verified by the consumer; **all records verified in every run**. The producer/consumer pair spins; MiB/s is payload bytes over the producer's/consumer's wall time.

| Mode | Records | guest to host MiB/s | host to guest MiB/s | ping-pong spin round trip (min / p50 / p99 / p99.9 / max) |
|---|---:|---:|---:|---|
| cached, device a alone | 1 M | **6,628** | **7,722** | 0.10 / 0.20 / 0.30 / 0.40 / 75.2 us (199,900 pings) |
| cached, device a, first session | 100 k | 7,713 | 8,261 | 0.10 / 0.20 / 0.30 / 0.30 / 79.4 us |
| cached, both devices at once (two processes each side) | 1 M each | a: 6,273 / b: 3,380 | a: 6,403 / b: 5,983 | 0.10 / 0.20 / 0.30 / 0.3 to 0.5 / 73.3 and 79.4 us |
| write-combined | 1 M | 4,942 | **208** | 0.50 / 1.00 / 1.40 / 1.80 / 91.8 us |
| non-cached | 100 k | 11.8 | 12.4 | 0.60 / 0.80 / 1.10 / 1.40 / 96.1 us |

* The maximum is the tail from the guest scheduler/hypervisor (a vCPU being descheduled), not ring behaviour: p99.9 is below 2 us in all modes. On a shared host a 100 us stall of a vCPU is enough to produce these maxima.
* Write-combined host to guest is slow because the ring's `memcpy` reads the BAR with ordinary loads. A consumer using `MOVNTDQA` would reach several thousand MiB/s (section 4) but needs a read hook in the crate; even then cached has the same write speed and faster reads, so there is no case for write-combined. **There is no per-direction trade-off to make: one mode per mapping, and cached wins in both directions.**
* **CPU use**: spin polling keeps one core at 100 % on each side (process `cpu%` 95 to 108 in the 0.5 to 1 s bulk phases). `cpu%` comes from `GetProcessTimes`, which has scheduler-tick resolution, so phases of only 10 to 20 ms (the cached spin ping-pong) report 69 to 138 %: do not read anything into those. With the host consumer sleeping 1 ms whenever the lane is empty (no kick) the host uses 0 to 1 % of a core and the round-trip p50 is 1.056 ms in every mode (p99 1.06 to 1.10 ms; the sleep granularity dominates), while the guest client, which keeps spinning, still shows about 100 %. The virtio-serial kick that should shorten that wait was not part of this test.
* Two devices busy at the same time (4 spinning threads in the guest, 4 on the host, 12 vCPUs) cost device a 5 % (guest to host) and 17 % (host to guest) and device b 49 % and 23 % relative to device a alone; this is a CPU-contention effect of the shared test host, not a device limit.

## 9. What is settled and what is not (design doc items)

| Item (doc section) | Status |
|---|---|
| 8.1 signature/install without Code 52, same OVMF/Secure-Boot-off setup | **Settled**: Valid WHQL signature, Status OK, ProblemCode 0, Secure Boot not active |
| 8.1 repeat with Secure Boot ON | **Settled**: installs and runs, `Confirm-SecureBootUEFI = True` |
| 4/8 cache modes, real read and write speeds | **Settled** (section 4); recommendation: cached for both devices |
| 4 cached mapping hardware-coherent with host | **Settled** for this QEMU/KVM/AMD setup (pattern + ring tests) |
| 4 `sfence` needed only for write-combined | **Refined**: needed for write-combined (any stores) and for non-temporal stores in cached mode; harmless otherwise |
| 4 scalar reads tens of MB/s on WC/UC | **Confirmed** (81 MiB/s WC, 83 UC); streaming loads in WC reach 7,200 to 15,000 MiB/s |
| 8 single mapping, `STATUS_DEVICE_ALREADY_ATTACHED`, release on handle close | **Settled** (section 5) |
| 7/8 default device security is SYSTEM and Administrators | **Refuted**: Everyone has read/write; runtime DACL fix works (section 6) |
| 2 PCI 1AF4:1110, size | Settled for the ID and the 256 MiB size (the BAR index itself was not queried) |
| 5 doorbell/interrupt vectors | `vectors = 0` for `ivshmem-plain`; doorbell not tested |
| 8.2 HVCI/memory integrity | Not tested (VBS off) |
| 8.5 EDR, Windows Update replacing the driver | Not testable here |

## 10. Reproduce

```
cd experiments/shmprobe && cargo build --release        # Linux host part; also builds with cargo build --release in the VM (MSVC)
shmprobe host pattern|order|ring --file /dev/shm/wbprobe-a [options]      # Linux
shmprobe guest info|identify|bench|pattern|order|ring|single|open-test|harden [--dev N] [--modes 0,1,2] [--quick] ...   # Windows, elevated
```

Guest and host processes of `pattern`, `order` and `ring` are started separately (host first); the first page of each device is used as a mailbox by `pattern`/`order` and as the ring header by `ring`, so run one test at a time per device. Without `--file`, the `guest` role on Linux refuses to run; with `--file` it maps a plain file, which is how the whole tool was validated on one machine before it went into the VM.

Run from a clean guest snapshot each time: `identify` writes a tag at offset 0x800 of each device so the guest device index (PCI location) can be matched to the host backing file; in this run device index 0 was `wbprobe-a` (PCI device 10) and index 1 was `wbprobe-b`.
