# Testing on real machines (Ubuntu Server VMs)

The localhost smoke test (`bin/dev-smoke-test.sh`) and the automated
`cargo test` integration test both prove the logic works, but only this
proves it works across real, separate hosts: real TCP, real TLS between
different machines, real `sysinfo` readings on actual Ubuntu — none of
which a single-machine test can fully stand in for.

## Before you start: building the binary

This Mac has no cross-compilation toolchain for Linux set up (confirmed by
trying: `ring`, the crypto backend, needs a C cross-compiler targeting
Linux that isn't installed here, and getting one right — matching your
VMs' exact glibc version — is its own rabbit hole). The reliable path is to
**build natively on one of your Ubuntu VMs** and copy the resulting single
binary to the others; that sidesteps cross-compilation version-matching
entirely since it's compiled in an environment identical to where it runs.
Once this workflow is proven, cross-compiling from the Mac is a reasonable
later optimization — not necessary to get started.

Pick one VM as your **build machine** (it can also be your head node —
build machines don't need to be dedicated).

## 1. Get the code onto the build VM

From this Mac:

```bash
rsync -avz --exclude target --exclude data --exclude log \
  /Users/paul/myFiles/PROJECTS/PRIO01_Minimal_Distibuted_System/ \
  <user>@<build-vm-ip>:~/clusterd/
```

(Swap in your VM's actual username/IP. If you'd rather push this repo to a
git remote first and `git clone` on the VM instead, that works too — same
result.)

## 2. Install Rust and a C toolchain on the build VM

```bash
ssh <user>@<build-vm-ip>
sudo apt update && sudo apt install -y build-essential pkg-config
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
```

## 3. Build the release binary

```bash
cd ~/clusterd
cargo build --release -p clusterd
ls -la target/release/clusterd   # this is the one file everything else needs
```

## 4. Copy the binary to every other VM

From the build VM (or from the Mac, if it can reach the other VMs):

```bash
for host in <worker-vm-1-ip> <worker-vm-2-ip>; do
  scp target/release/clusterd <user>@$host:~/clusterd
done
```

Each VM now needs just that one file — no Rust toolchain required on
workers, matching the "single static binary" deployment goal.

## 5. Set up the head node

On whichever VM you want as head (commonly the build VM itself):

```bash
./clusterd --data-dir ~/clusterd-data init \
  --node-name head-1 \
  --role head \
  --listen 0.0.0.0:7700
```

This generates the cluster CA and the head's own certificate, and prints
next steps. Note the head VM's IP — workers need it.

If `ufw` is active on this VM, allow the port: `sudo ufw allow 7700/tcp`.

## 6. Issue a certificate for each worker

Still on the head:

```bash
./clusterd --data-dir ~/clusterd-data cert issue --name worker-1 --out ~/worker-1-certs
./clusterd --data-dir ~/clusterd-data cert issue --name worker-2 --out ~/worker-2-certs
```

Copy each output directory to its worker:

```bash
scp ~/worker-1-certs/* <user>@<worker-vm-1-ip>:~/worker-1-certs/
scp ~/worker-2-certs/* <user>@<worker-vm-2-ip>:~/worker-2-certs/
```

## 7. Set up each worker

On each worker VM:

```bash
./clusterd --data-dir ~/clusterd-data init \
  --node-name worker-1 \
  --role worker \
  --head <head-vm-ip>:7700

cp ~/worker-1-certs/* ~/clusterd-data/
```

(Use the matching `worker-N` name/cert directory on each machine.)

## 8. Start the agents

Phase 3 will install these as real systemd services; for now, run them
under `tmux`/`screen` (or just leave an SSH session open) so they keep
running after you disconnect:

```bash
# on each VM (head and every worker):
RUST_LOG=info ./clusterd --data-dir ~/clusterd-data agent run
```

## 9. Check the pool from the head

In a separate session on the head VM:

```bash
./clusterd --data-dir ~/clusterd-data status
```

Expect every node listed as `up`, with live CPU/RAM/disk figures.

## 10. Confirm down-detection works for real

Stop the agent on one worker (Ctrl-C, or `kill` its process), wait about 15
seconds (the default `heartbeat_timeout_secs`), then run `status` again on
the head — that worker should now show `DOWN` while the others stay
current.

## If something doesn't connect

- **`status` says "connecting to control socket... is clusterd agent run
  running on this host?"** — you're running `status` on a machine where the
  agent isn't up yet, or `--data-dir` doesn't match what the agent was
  started with.
- **Worker logs show a TLS/handshake error** — double check `ca.crt`,
  `node.crt`, and `node.key` in the worker's data directory are the ones
  `cert issue` produced for *that* worker's name, not a different one or a
  stale copy.
- **Worker never appears in `status`** — check `ufw`/security-group rules
  on the head VM allow inbound TCP on the port you passed to `--listen`,
  and that the worker's `--head` address is actually reachable
  (`nc -zv <head-vm-ip> 7700` from the worker is a quick check).
